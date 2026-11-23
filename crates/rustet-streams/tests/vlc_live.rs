//! The AAC init length against a real libvlc (ignored by default; nothing is heard).
//!
//! Run with `cargo test -p rustet-streams --test vlc_live -- --ignored --nocapture`, with
//! `libvlc.so.5` on `LD_LIBRARY_PATH` (`nix build nixpkgs#libvlc`), its plugins in
//! `VLC_PLUGIN_PATH`, and ffmpeg in `FFMPEG` or on `PATH`.

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustet_core::pipeline::{Sink, Source as _, StreamContext};
use rustet_hls::{Media, Segment};
use rustet_mp4::{FourCC, Mp4Box};
use rustet_streams::Streams;
use rustet_streams::license::HlsSource;
use rustet_vlc::{AbortHandle, Event, Instance, Source, Start};
use tokio::io::AsyncWrite;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

const ARGS: &[&CStr] = &[c"--aout=adummy", c"--vout=vdummy", c"--no-video-title-show"];
const SECONDS: u64 = 20;

/// AAC-LC 256 kbit/s in 2 s fragments with an empty moov (duration 0): the shape of Apple's CMAF AAC.
fn aac_fmp4(dir: &Path) -> Vec<u8> {
    let out = dir.join("aac.m4a");
    let ffmpeg = std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let src = format!("sine=frequency=440:sample_rate=44100:duration={SECONDS}");
    let status = std::process::Command::new(ffmpeg)
        .args([
            "-v", "error", "-y", "-f", "lavfi", "-i", &src, "-ac", "2", "-c:a", "aac", "-b:a",
            "256k",
        ])
        .args([
            "-f",
            "mp4",
            "-movflags",
            "+empty_moov+default_base_moof",
            "-frag_duration",
            "2000000",
        ])
        .arg(&out)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
    std::fs::read(out).unwrap()
}

/// Splits the file into the init segment (ftyp + moov) and one segment per moof + mdat.
fn split(data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let (mut init, mut segs, mut at) = (Vec::new(), Vec::new(), 0);
    let mut cur = Vec::new();
    while at < data.len() {
        let (b, used) = Mp4Box::decode(&data[at..], at as u64).unwrap();
        let bytes = &data[at..at + used];
        match b.kind() {
            k if k == FourCC::FTYP || k == FourCC::MOOV => init.extend_from_slice(bytes),
            k if k == FourCC::MOOF => cur = bytes.to_vec(),
            k if k == FourCC::MDAT => {
                cur.extend_from_slice(bytes);
                segs.push(std::mem::take(&mut cur));
            }
            _ => {}
        }
        at += used;
    }
    (init, segs)
}

/// Writes into a file at the track's play rate (as a cache download over a slow link would), and flags the end.
struct Paced {
    file: File,
    bytes_per_sec: u64,
    written: u64,
    started: Instant,
}

impl AsyncWrite for Paced {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let n = buf.len().min(4096);
        self.file.write_all(&buf[..n])?;
        self.written += n as u64;
        let due = Duration::from_secs_f64(self.written as f64 / self.bytes_per_sec as f64);
        if let Some(wait) = due.checked_sub(self.started.elapsed()) {
            std::thread::sleep(wait); // a test writer on its own runtime thread
        }
        std::task::Poll::Ready(Ok(n))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl Sink for Paced {}

/// A file still being written: reads at the end wait until the writer is done.
struct Growing {
    file: File,
    done: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let done = self.done.load(Ordering::Acquire);
            let n = self.file.read(buf)?;
            if n > 0 || done {
                return Ok(n);
            }
            if self.abort.load(Ordering::Acquire) {
                return Err(io::Error::other("aborted"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Seek for Growing {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl Source for Growing {
    fn size(&self) -> Option<u64> {
        None
    }
    fn abort_handle(&self) -> AbortHandle {
        let abort = Arc::clone(&self.abort);
        AbortHandle::new(move || abort.store(true, Ordering::Release))
    }
}

/// Streams the track through `HlsSource` (or, with `raw`, Apple's bytes unchanged) into a file at `speed` × the
/// play rate, plays the file with libvlc while it grows, and returns the time from the open to `Playing` and to the
/// first position, and the write time.
async fn start_time(
    server: &MockServer,
    segs: usize,
    total: usize,
    speed: u64,
    raw: bool,
) -> (Duration, Duration, Duration) {
    let dir = tempfile::tempdir().unwrap();
    let streams = Arc::new(Streams::with_cache_root(
        reqwest::Client::new(),
        &dir.path().join("cache"),
    ));
    let media = Media {
        init_url: Some(format!("{}/init", server.uri())),
        segments: (0..segs)
            .map(|i| Segment {
                url: format!("{}/s{i}", server.uri()),
                duration: 2.0,
            })
            .collect(),
        encryption: None,
    };
    let source = HlsSource::new(streams, media);
    let out = dir.path().join("growing.m4a");
    let done = Arc::new(AtomicBool::new(false));
    let rate = total as u64 * speed / SECONDS;
    let writer = {
        let (out, done) = (out.clone(), done.clone());
        let urls: Vec<String> = std::iter::once("/init".to_owned())
            .chain((0..segs).map(|i| format!("/s{i}")))
            .collect();
        let base = server.uri();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut sink = Paced {
                file: File::create(&out).unwrap(),
                bytes_per_sec: rate,
                written: 0,
                started: Instant::now(),
            };
            rt.block_on(async {
                if raw {
                    let client = reqwest::Client::new();
                    for u in urls {
                        let body = client
                            .get(format!("{base}{u}"))
                            .send()
                            .await
                            .unwrap()
                            .bytes()
                            .await
                            .unwrap();
                        tokio::io::AsyncWriteExt::write_all(&mut sink, &body)
                            .await
                            .unwrap();
                    }
                } else {
                    source
                        .stream(&StreamContext::new(CancellationToken::new()), &mut sink)
                        .await
                        .unwrap();
                }
            });
            done.store(true, Ordering::Release);
            sink.started.elapsed()
        })
    };
    while std::fs::metadata(&out).map_or(true, |m| m.len() == 0) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let inst = Instance::with_args(ARGS).expect("libvlc");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let deck = Arc::new(inst.deck(tx).unwrap());
    let src = Growing {
        file: File::open(&out).unwrap(),
        done,
        abort: Arc::default(),
    };
    let opened = Instant::now();
    let d = deck.clone();
    let load = tokio::task::spawn_blocking(move || d.open_source(src, Start::Playing))
        .await
        .unwrap()
        .unwrap();
    let (mut playing, mut first) = (None, None);
    let deadline = opened + Duration::from_secs(SECONDS + 15);
    while first.is_none() && Instant::now() < deadline {
        let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await else {
            continue;
        };
        if ev.load != load {
            continue;
        }
        match ev.event {
            Event::Playing => playing = playing.or(Some(opened.elapsed())),
            Event::TimeChanged(t) if t > 0 => first = Some(opened.elapsed()),
            _ => {}
        }
    }
    tokio::task::spawn_blocking(move || deck.stop())
        .await
        .unwrap();
    let write = writer.await.unwrap();
    (
        playing.expect("never Playing"),
        first.expect("no position"),
        write,
    )
}

/// Apple's AAC init says 0 in mvhd; libvlc then waits for the whole growing file. With the length the source
/// writes, it starts within about 2 s of a real-time download.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs libvlc and ffmpeg"]
async fn aac_with_the_init_length_starts_while_it_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let file = aac_fmp4(dir.path());
    let (init, segs) = split(&file);
    let server = MockServer::start().await;
    Mock::given(path("/init"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(init.clone()))
        .mount(&server)
        .await;
    for (i, s) in segs.iter().enumerate() {
        Mock::given(path(format!("/s{i}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(s.clone()))
            .mount(&server)
            .await;
    }
    let total = init.len() + segs.iter().map(Vec::len).sum::<usize>();

    let (playing, first, write) = start_time(&server, segs.len(), total, 1, false).await;
    eprintln!(
        "patched init, 1x download: open→Playing {playing:?}, open→first position {first:?}, write {write:?}"
    );
    let (raw_playing, raw_first, raw_write) = start_time(&server, segs.len(), total, 4, true).await;
    eprintln!(
        "Apple's init (length 0), 4x download: open→Playing {raw_playing:?}, open→first position {raw_first:?}, write {raw_write:?}"
    );

    assert!(
        first < Duration::from_secs(4),
        "the patched file took {first:?} to start"
    );
    assert!(
        raw_first + Duration::from_millis(500) >= raw_write,
        "the control started before its file was complete"
    );
}
