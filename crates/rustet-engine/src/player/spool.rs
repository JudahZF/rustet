//! A deck source for a stream that does not go through the disk cache: a seek (the stream starts
//! at the target, `Manager::stream_from`), a track whose cache entry another task is writing
//! without readers (a pre-cache), or a music video (its video and audio joined by [`mvmux`]). The stream is written to a temp file that libvlc reads while
//! it grows; the file goes when libvlc drops the source.
//!
//! Seeking within the cache file of a running download blocks libvlc until the file is
//! complete (its MP4 demuxer has no fragment index to jump with), so the deck reloads from a
//! spool instead (spec §4.6).

use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, ready};

use rustet_core::pipeline::{Sink, StreamContext, StreamKind};
use rustet_playback::StreamClass;
use rustet_vlc::{AbortHandle, Source};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::create_private_dir;
use super::mvmux::{self, MuxIndex, MuxPlan};
use crate::state::Engine;

/// Buffer of each pipe from a music-video stream to the mux.
const MUX_PIPE: usize = 256 << 10;

/// How far the writer is.
#[derive(Debug, Clone, Default)]
struct Progress {
    /// Bytes readable from the file.
    written: u64,
    /// The stream ended: cleanly, or with this error.
    end: Option<Result<(), String>>,
}

/// Removes the file and stops the writer once the reader is gone.
struct Guard {
    path: PathBuf,
    cancel: CancellationToken,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A spool file that readers can open: kept, and its writer running, while any reader or this
/// value lives.
#[derive(Clone)]
struct SpoolFile {
    guard: Arc<Guard>,
    progress: watch::Receiver<Progress>,
    runtime: Handle,
}

impl SpoolFile {
    /// A reader that sees `head`, then the file from `base` on. Opens the file: blocking.
    fn source(&self, head: Arc<[u8]>, base: u64) -> io::Result<SpoolSource> {
        Ok(SpoolSource {
            file: std::fs::File::open(&self.guard.path)?,
            pos: 0,
            head,
            base,
            progress: self.progress.clone(),
            runtime: self.runtime.clone(),
            abort: CancellationToken::new(),
            _guard: self.guard.clone(),
        })
    }
}

/// A new spool file: the writer's end, the file to open readers on, and the token that stops
/// the writer (it fires when the last reader goes, or the engine stops).
async fn create(
    engine: &Arc<Engine>,
) -> io::Result<(SpoolWriter, SpoolSource, SpoolFile, CancellationToken)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = engine.spool_dir.clone();
    let path = dir.join(format!(
        "{}-{}.spool",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let (write, read) = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            create_private_dir(&dir)?;
            let write = std::fs::File::create(&path)?;
            let read = std::fs::File::open(&path)?;
            io::Result::Ok((write, read))
        })
        .await
        .map_err(io::Error::other)??
    };
    let (tx, rx) = watch::channel(Progress::default());
    let cancel = engine.shutdown.child_token();
    let sink = SpoolWriter {
        file: tokio::fs::File::from_std(write),
        unflushed: 0,
        progress: tx,
    };
    let spool = SpoolFile {
        guard: Arc::new(Guard {
            path,
            cancel: cancel.clone(),
        }),
        progress: rx,
        runtime: Handle::current(),
    };
    let source = SpoolSource {
        file: read,
        pos: 0,
        head: Arc::from([]),
        base: 0,
        progress: spool.progress.clone(),
        runtime: spool.runtime.clone(),
        abort: CancellationToken::new(),
        _guard: spool.guard.clone(),
    };
    Ok((sink, source, spool, cancel))
}

/// Starts the session's audio from `from_ms` (0: the whole stream) into a new spool file.
/// Returns libvlc's reader on it.
pub(crate) async fn start(
    engine: &Arc<Engine>,
    session: &str,
    from_ms: u64,
) -> io::Result<SpoolSource> {
    let playback = engine
        .playback
        .as_ref()
        .ok_or_else(|| io::Error::other("playback not available"))?;
    let (mut sink, source, _, cancel) = create(engine).await?;
    let manager = playback.manager.clone();
    let id = session.to_owned();
    tokio::spawn(async move {
        let mut ctx = StreamContext::new(cancel);
        let result = if from_ms > 0 {
            let from = from_ms as f64 / 1000.0;
            ctx.seek_actual_start = manager.seek_start(&id, StreamKind::Audio, from);
            // The exact target lets the stages drop the fragments before it.
            ctx.seek_target = Some(from);
            manager
                .stream_from(
                    &ctx,
                    StreamClass::Foreground,
                    &id,
                    StreamKind::Audio,
                    from,
                    &mut sink,
                )
                .await
                .map(drop)
        } else {
            manager
                .stream(
                    &ctx,
                    StreamClass::Foreground,
                    &id,
                    StreamKind::Audio,
                    &mut sink,
                )
                .await
        };
        debug!(session = id, from_ms, ?result, "[spool] stream ended");
        sink.finish(result.map_err(|e| e.to_string())).await;
    });
    Ok(source)
}

/// A music video's spool: the video's visible size once known, and views on what is in it.
#[derive(Clone)]
pub(crate) struct MusicVideo {
    pub video_size: watch::Receiver<Option<(u32, u32)>>,
    index: Arc<Mutex<MuxIndex>>,
    file: SpoolFile,
}

impl MusicVideo {
    /// A reader that plays from `at_ms` out of what is already spooled: the stream's head, then
    /// the spool from the fragments holding `at_ms`, following the file as it grows. `None` when
    /// `at_ms` is not written yet. Opens the file: blocking.
    pub(crate) fn view(&self, at_ms: u64) -> Option<io::Result<SpoolSource>> {
        let (head, base) = {
            let index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
            (
                Arc::<[u8]>::from(index.head.as_slice()),
                index.offset_for(at_ms)?,
            )
        };
        Some(self.file.source(head, base))
    }
}

/// Starts a music-video session's video and audio, joined into one fMP4 ([`mvmux`]), from the
/// segment holding `from_ms` (0: the whole video) into a new spool file. Returns libvlc's reader
/// and the spool.
pub(crate) async fn start_music_video(
    engine: &Arc<Engine>,
    session: &str,
    from_ms: u64,
) -> io::Result<(SpoolSource, MusicVideo)> {
    let playback = engine
        .playback
        .as_ref()
        .ok_or_else(|| io::Error::other("playback not available"))?;
    let sess = playback
        .manager
        .session(session)
        .ok_or_else(|| io::Error::other("session gone"))?;
    let (mut sink, source, file, cancel) = create(engine).await?;
    let (size_tx, video_size) = watch::channel(None);
    let index = Arc::new(Mutex::new(MuxIndex::default()));
    let mv = MusicVideo {
        video_size,
        index: index.clone(),
        file,
    };
    let manager = playback.manager.clone();
    let id = session.to_owned();
    tokio::spawn(async move {
        let from = from_ms as f64 / 1000.0;
        // A stream that cannot seek plays from 0: the deck seeks libvlc instead.
        let starts = (from > 0.0)
            .then(|| {
                Some((
                    manager.seek_start(&id, StreamKind::Video, from)?,
                    manager.seek_start(&id, StreamKind::Audio, from)?,
                ))
            })
            .flatten();
        let (video_ctx, mut audio_ctx) = (
            StreamContext::new(cancel.clone()),
            StreamContext::new(cancel.clone()),
        );
        // The clear-AAC path numbers its fragments from here; the encrypted one keeps Apple's times.
        audio_ctx.seek_actual_start = starts.map(|(_, a)| a);
        let (mut video_w, video_r) = tokio::io::duplex(MUX_PIPE);
        let (mut audio_w, audio_r) = tokio::io::duplex(MUX_PIPE);
        let feed = async |kind, ctx: &StreamContext, w: &mut tokio::io::DuplexStream| {
            let r = match starts {
                Some(_) => manager
                    .stream_from(ctx, StreamClass::Foreground, &id, kind, from, w)
                    .await
                    .map(drop),
                None => {
                    manager
                        .stream(ctx, StreamClass::Foreground, &id, kind, w)
                        .await
                }
            };
            let _ = w.shutdown().await;
            r
        };
        let plan = MuxPlan {
            video_start_s: starts.map_or(0.0, |(v, _)| v),
            audio_start_s: starts.map_or(0.0, |(_, a)| a),
            duration_ms: u64::from(sess.duration_ms),
        };
        let (video, audio, muxed) = tokio::join!(
            feed(StreamKind::Video, &video_ctx, &mut video_w),
            feed(StreamKind::Audio, &audio_ctx, &mut audio_w),
            async {
                let r = mvmux::mux(video_r, audio_r, plan, &mut sink, &size_tx, &index).await;
                // Unblocks the feeds when the mux stopped first.
                cancel.cancel();
                r
            },
        );
        // The mux ends once both inputs ended, so a feed's error is real (a failed download cut
        // its track short): the source then does not end cleanly.
        let result = match (video, audio, muxed) {
            (_, _, Err(e)) => Err(e.to_string()),
            (Err(e), _, _) | (_, Err(e), _) => Err(e.to_string()),
            _ => Ok(()),
        };
        debug!(session = id, from_ms, ?result, "[spool] music video ended");
        sink.finish(result).await;
    });
    Ok((source, mv))
}

/// Writes the stream and publishes how much of it is in the file. Bytes count once flushed:
/// tokio's file writes in the background, and a reader must not see a length the file lacks.
struct SpoolWriter {
    file: tokio::fs::File,
    unflushed: u64,
    progress: watch::Sender<Progress>,
}

impl SpoolWriter {
    fn poll_publish(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.unflushed > 0 {
            ready!(Pin::new(&mut self.file).poll_flush(cx))?;
            let n = std::mem::take(&mut self.unflushed);
            self.progress.send_modify(|p| p.written += n);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SpoolWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self.poll_publish(cx))?;
        let n = ready!(Pin::new(&mut self.file).poll_write(cx, buf))?;
        self.unflushed += n as u64;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_publish(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_publish(cx)
    }
}

impl Sink for SpoolWriter {}

impl SpoolWriter {
    /// Publishes the last bytes and how the stream ended.
    async fn finish(mut self, end: Result<(), String>) {
        let end = match (end, self.flush().await) {
            (Ok(()), Err(e)) => Err(e.to_string()),
            (end, _) => end,
        };
        self.progress.send_modify(|p| p.end = Some(end));
    }
}

/// libvlc's reader on a spool: `head` (empty but for a music-video view), then the file from
/// `base`. Reads past the written end wait for the writer; the end of the stream is a clean end
/// only when the stream ended cleanly.
pub(crate) struct SpoolSource {
    file: std::fs::File,
    /// Position in what this reader shows.
    pos: u64,
    head: Arc<[u8]>,
    base: u64,
    progress: watch::Receiver<Progress>,
    runtime: Handle,
    abort: CancellationToken,
    _guard: Arc<Guard>,
}

impl Read for SpoolSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let head = self.head.len() as u64;
        if self.pos < head {
            let rest = &self.head[self.pos as usize..];
            let n = rest.len().min(buf.len());
            buf[..n].copy_from_slice(&rest[..n]);
            self.pos += n as u64;
            return Ok(n);
        }
        loop {
            let at = self.base + (self.pos - head);
            let progress = self.progress.borrow_and_update().clone();
            if at < progress.written {
                let n = buf
                    .len()
                    .min(usize::try_from(progress.written - at).unwrap_or(usize::MAX));
                self.file.seek(SeekFrom::Start(at))?;
                let got = self.file.read(&mut buf[..n])?;
                self.pos += got as u64;
                return Ok(got);
            }
            match progress.end {
                Some(Ok(())) => return Ok(0),
                Some(Err(e)) => return Err(io::Error::other(e)),
                None => {}
            }
            let (progress, abort) = (&mut self.progress, &self.abort);
            self.runtime.block_on(async {
                tokio::select! {
                    biased;
                    () = abort.cancelled() => Err(io::Error::other("source aborted")),
                    changed = progress.changed() => changed.map_err(|_| io::Error::other("spool writer gone")),
                }
            })?;
        }
    }
}

impl Seek for SpoolSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => match self.size() {
                Some(size) => size.checked_add_signed(d),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek from the end of a growing spool",
                    ));
                }
            },
        };
        self.pos = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "negative seek offset"))?;
        Ok(self.pos)
    }
}

impl Source for SpoolSource {
    fn size(&self) -> Option<u64> {
        let p = self.progress.borrow();
        matches!(p.end, Some(Ok(())))
            .then(|| self.head.len() as u64 + p.written.saturating_sub(self.base))
    }

    fn abort_handle(&self) -> AbortHandle {
        self.abort.clone().into()
    }
}

/// Deletes spool files a crashed engine left behind. Blocking.
pub(crate) fn sweep(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().ends_with(".spool") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
