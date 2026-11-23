//! The FairPlay CBCS source: downloads the single encrypted fMP4 of a lossless track and
//! streams it out decrypted, one fragment at a time, through a DRM connection.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::TryStreamExt;
use reqwest::StatusCode;
use reqwest::header::ACCEPT_ENCODING;
use rustet_core::pipeline::{PipelineError, Seekable, Sink, Source, StreamContext};
use rustet_hls::CbcsMedia;
use rustet_mp4::{Fragment, InitSegment, Mp4Box, Tfdt};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufStream, ReadBuf};
use tokio::time::{Instant, Sleep};
use tokio_util::io::StreamReader;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::alac;
use crate::dial::{CbcsDialer, DrmStream};

/// The generic FairPlay key URI of every Apple Music enhanced-HLS variant (it encrypts
/// segment 0). The decryptor opens it under adamID `"0"`, not the track's ID.
pub const PREFETCH_KEY: &str = "skd://itunes.apple.com/P000000000/s1/e1";

/// How long the download may go without a read of at least [`STALL_THRESHOLD`] bytes before
/// it counts as hung. Apple's CDN sometimes keeps a connection open but stops sending.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const STALL_THRESHOLD: usize = 256;

/// How long a socket DRM connection may sit idle between fragments before it is replaced
/// proactively, like the Android app refreshes its playback lease before it expires: the
/// wrapper process may drop idle connections.
const DRM_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Fragments decrypted on one DRM connection before it is replaced proactively, against
/// per-connection resource limits in the wrapper. `None` disables the limit.
const DRM_FRAG_LIMIT: Option<u32> = None;

/// Download + decrypt attempts per stream; the waits between them are 1 s and 2 s.
const MAX_ATTEMPTS: u32 = 3;

/// The trun sample duration assumed when the tfhd has none (4096 frames per ALAC packet).
const DEFAULT_SAMPLE_DURATION: u32 = 4096;

const WARM_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a CBCS stream failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cancelled")]
    Cancelled,
    #[error("download: {0}")]
    Download(#[source] reqwest::Error),
    #[error("download: HTTP {}", .0.as_u16())]
    Status(StatusCode),
    #[error("dial: {0}")]
    Dial(#[source] io::Error),
    #[error("read init: {0}")]
    ReadInit(#[source] alac::Error),
    #[error("transform init: {0}")]
    TransformInit(#[source] alac::Error),
    #[error("encode init: {0}")]
    EncodeInit(#[source] rustet_mp4::Error),
    #[error("seek: skip fragment {index}: {source}")]
    SkipFragment {
        index: usize,
        #[source]
        source: alac::Error,
    },
    #[error("seek: EOF before start fragment {start} (at {at})")]
    SeekPastEnd { start: usize, at: usize },
    #[error("read fragment {index}: {source}")]
    ReadFragment {
        index: usize,
        #[source]
        source: alac::Error,
    },
    #[error("send key for fragment {index}: {source}")]
    Key {
        index: usize,
        #[source]
        source: io::Error,
    },
    #[error("reconnect after fragment {index}: {source}")]
    Redial {
        index: usize,
        #[source]
        source: io::Error,
    },
    #[error("decrypt fragment {index} after reconnect: {source}")]
    Decrypt {
        index: usize,
        #[source]
        source: alac::Error,
    },
    #[error("encode fragment {index}: {source}")]
    EncodeFragment {
        index: usize,
        #[source]
        source: rustet_mp4::Error,
    },
    #[error("write output: {0}")]
    Output(#[source] io::Error),
}

impl From<Error> for PipelineError {
    fn from(e: Error) -> Self {
        match e {
            Error::Cancelled => Self::Cancelled,
            Error::Output(e) => Self::Io(e),
            e => Self::Other(Box::new(e)),
        }
    }
}

// ── Source ───────────────────────────────────────────────────────────────────

/// A [`Source`] for a FairPlay CBCS (lossless) track. It downloads the single encrypted fMP4
/// and writes it out decrypted, fragment by fragment, through connections opened by the
/// dialer. It is [`Seekable`]: a seek downloads the whole file again but drops the fragments
/// before the seek point, keeping their decode times.
///
/// The output is a clean fMP4 for VLC / mpv / ffmpeg: one sample entry, an `mvex`, the
/// declared duration in the moov, and continuous 64-bit decode times.
#[derive(Clone)]
pub struct CbcsSource {
    adam_id: String,
    dialer: Arc<dyn CbcsDialer>,
    client: reqwest::Client,
    file_url: String,
    /// Key URIs of the fragments; see [`upstream_keys`].
    keys: Vec<Option<String>>,
    segment_durations: Vec<f64>,
    /// Patched into the moov; 0 leaves it unchanged.
    duration_ms: u32,
    start: Start,
}

/// Where a stream starts.
#[derive(Debug, Clone, PartialEq)]
enum Start {
    Beginning,
    /// A seek: fragments before `index` are skipped, and `key` (the key in force at `index`)
    /// is selected before the first kept one.
    Fragment {
        index: usize,
        key: Option<String>,
    },
}

impl CbcsSource {
    /// A source for `media`, played as track `adam_id`. `duration_ms` is the track length
    /// written into the moov (Apple ships 0, and mpv then stops after the first fragment);
    /// 0 leaves the moov unchanged.
    pub fn new(
        adam_id: impl Into<String>,
        dialer: Arc<dyn CbcsDialer>,
        client: reqwest::Client,
        media: &CbcsMedia,
        duration_ms: u32,
    ) -> Self {
        Self {
            adam_id: adam_id.into(),
            dialer,
            client,
            file_url: media.file_url.clone(),
            keys: upstream_keys(media),
            segment_durations: media.segments.iter().map(|s| s.duration).collect(),
            duration_ms,
            start: Start::Beginning,
        }
    }

    /// One download + decrypt cycle.
    async fn attempt(
        &self,
        cancel: &CancellationToken,
        out: &mut CountingWriter<'_>,
    ) -> Result<(), Error> {
        // Like Go's DisableCompression: the body is already-compressed media.
        let resp = self
            .client
            .get(&self.file_url)
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(Error::Download)?;
        if resp.status() != StatusCode::OK {
            return Err(Error::Status(resp.status()));
        }
        // The stall clock starts now, so it also covers the dial below.
        let body = StreamReader::new(resp.bytes_stream().map_err(io::Error::other));
        let mut body = BufReader::new(StallGuard::new(body, STALL_TIMEOUT));

        let conn = self.dialer.dial_cbcs(cancel).await.map_err(Error::Dial)?;
        let mut drm = DrmConn::new(&*self.dialer, cancel, conn);
        let res = self.decrypt_stream(&mut body, &mut drm, out).await;
        drm.close().await;
        res
    }

    async fn decrypt_stream<R: AsyncRead + Unpin>(
        &self,
        r: &mut R,
        drm: &mut DrmConn<'_>,
        out: &mut CountingWriter<'_>,
    ) -> Result<(), Error> {
        let (mut init, mut offset) = alac::read_init_segment(r).await.map_err(Error::ReadInit)?;
        let info = alac::transform_init(&mut init).map_err(Error::TransformInit)?;
        if let Err(e) = alac::sanitize_init(&mut init) {
            warn!("cbcs: sanitize init: {e}");
        }
        patch_moov_duration(&mut init, self.duration_ms);
        write_out(out, &init.to_bytes().map_err(Error::EncodeInit)?).await?;

        let (first, mut tfdt) = match &self.start {
            Start::Beginning => (0, 0),
            Start::Fragment { index, key } => {
                let (tfdt, end) = skip_fragments(r, offset, *index).await?;
                offset = end;
                send_key_handshake(&mut drm.io, &self.adam_id, key.as_deref())
                    .await
                    .map_err(|source| Error::Key {
                        index: *index,
                        source,
                    })?;
                (*index, tfdt)
            }
        };
        // The key most recently selected on the connection (full stream only).
        let mut armed: Option<String> = None;

        for i in first.. {
            let next = alac::read_next_fragment(r, offset)
                .await
                .map_err(|source| Error::ReadFragment { index: i, source })?;
            let Some((mut frag, end)) = next else { break };
            offset = end;
            let key_err = |source| Error::Key { index: i, source };

            // A fresh connection has no key: it gets a handshake for the key in force at i.
            // Otherwise only a key change at i is sent.
            if drm.refresh_if_needed().await {
                let key = effective_key(&self.keys, i);
                send_key_handshake(&mut drm.io, &self.adam_id, key)
                    .await
                    .map_err(key_err)?;
                armed = key.map(String::from);
            } else {
                match &self.start {
                    Start::Beginning => self.send_frag_key(i, &mut armed, &mut drm.io).await,
                    Start::Fragment { index, .. } => self
                        .send_seek_frag_key(i, *index, &mut drm.io)
                        .await
                        .map_err(key_err)?,
                }
            }

            // Decryption is in place: keep the ciphertext to retry on a new connection.
            let backup = frag.mdat().map(|m| m.data.clone()).unwrap_or_default();
            if let Err(e) = alac::decrypt_fragment(&mut frag, &info, &mut drm.io).await {
                warn!("cbcs: decrypt fragment {i} failed ({e}), reconnecting DRM connection");
                // A reconnect alone would open the same expired key again.
                if matches!(e, alac::Error::WrongKey { .. })
                    && let Err(e) = self.dialer.refresh_keys().await
                {
                    warn!("cbcs: key refresh failed: {e}");
                }
                drm.redial(i).await?;
                if let Some(mdat) = frag.mdat_mut() {
                    mdat.data = backup;
                }
                let key = effective_key(&self.keys, i);
                send_key_handshake(&mut drm.io, &self.adam_id, key)
                    .await
                    .map_err(key_err)?;
                armed = key.map(String::from);
                alac::decrypt_fragment(&mut frag, &info, &mut drm.io)
                    .await
                    .map_err(|source| Error::Decrypt { index: i, source })?;
                info!("cbcs: fragment {i} decrypted after reconnect");
            }
            // Patch decode times only after decrypting: a grown tfdt changes the moof size,
            // and the samples are located by the original offsets.
            tfdt = match self.start {
                Start::Beginning => patch_alac_fragment_seq(&mut frag, i as u32, tfdt),
                Start::Fragment { .. } => patch_alac_fragment(&mut frag, tfdt),
            };
            drm.record_use();
            let bytes = frag
                .to_bytes()
                .map_err(|source| Error::EncodeFragment { index: i, source })?;
            write_out(out, &bytes).await?;
        }
        Ok(())
    }

    /// Selects the key of fragment `i` on the full stream when it differs from the armed
    /// one. Best effort: a failed write shows up as a decrypt error on this fragment.
    async fn send_frag_key<W: AsyncWrite + Unpin>(
        &self,
        i: usize,
        armed: &mut Option<String>,
        conn: &mut W,
    ) {
        let Some(Some(uri)) = self.keys.get(i) else {
            return;
        };
        if armed.as_ref() == Some(uri) {
            return;
        }
        if armed.is_some() {
            let _ = alac::switch_keys(conn).await;
        }
        let _ = send_key_handshake(conn, &self.adam_id, Some(uri)).await;
        *armed = Some(uri.clone());
    }

    /// Switches keys at fragment `i` of a seek stream when fragment `i` declares one. The
    /// first kept fragment's key was selected before the loop.
    async fn send_seek_frag_key<W: AsyncWrite + Unpin>(
        &self,
        i: usize,
        start: usize,
        conn: &mut W,
    ) -> io::Result<()> {
        if i <= start {
            return Ok(());
        }
        let Some(Some(uri)) = self.keys.get(i) else {
            return Ok(());
        };
        alac::switch_keys(conn).await?;
        send_key_handshake(conn, &self.adam_id, Some(uri)).await
    }
}

#[async_trait]
impl Source for CbcsSource {
    /// Retries the whole download + decrypt cycle, but only while nothing has been written:
    /// after the init segment is out, a retry would write a second one into the output.
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        debug!(adam_id = %self.adam_id, file_url = %self.file_url, start = ?self.start, "cbcs: stream start");
        let mut out = CountingWriter {
            inner: w,
            written: 0,
        };
        let mut attempt = 0;
        loop {
            let res = tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => Err(Error::Cancelled),
                r = self.attempt(&ctx.cancel, &mut out) => r,
            };
            let err = match res {
                Ok(()) => {
                    debug!(adam_id = %self.adam_id, "cbcs: stream done");
                    return Ok(());
                }
                Err(Error::Cancelled) => return Err(PipelineError::Cancelled),
                Err(e) => e,
            };
            attempt += 1;
            if out.written > 0 {
                warn!(
                    "cbcs: attempt {attempt} failed after {} bytes written, not retrying: {err}",
                    out.written
                );
                return Err(err.into());
            }
            if attempt == MAX_ATTEMPTS {
                return Err(err.into());
            }
            let wait = Duration::from_secs(1 << (attempt - 1));
            warn!("cbcs: attempt {attempt} failed ({err}), retrying in {wait:?}");
            tokio::select! {
                () = ctx.cancel.cancelled() => return Err(PipelineError::Cancelled),
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    fn seekable(&self) -> Option<&dyn Seekable> {
        (self.start == Start::Beginning).then_some(self as &dyn Seekable)
    }
}

impl Seekable for CbcsSource {
    /// Starts at the segment holding `start_sec`, from its first fragment. Past the end, the
    /// last segment is used.
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64) {
        let (src, actual_start) = self.seek_to(start_sec);
        (Arc::new(src), actual_start)
    }
}

impl CbcsSource {
    fn seek_to(&self, start_sec: f64) -> (Self, f64) {
        if start_sec <= 0.0 || self.segment_durations.is_empty() {
            return (self.clone(), 0.0);
        }
        let (index, actual_start) = start_fragment(&self.segment_durations, start_sec);
        let key = effective_key(&self.keys, index).map(String::from);
        let src = Self {
            start: Start::Fragment { index, key },
            ..self.clone()
        };
        (src, actual_start)
    }
}

/// The segment holding `start_sec` and its start time; the last segment when past the end.
/// `durations` must not be empty.
fn start_fragment(durations: &[f64], start_sec: f64) -> (usize, f64) {
    let mut acc = 0.0;
    for (i, d) in durations.iter().enumerate() {
        if acc + d > start_sec {
            return (i, acc);
        }
        acc += d;
    }
    let last = durations.len() - 1;
    (last, acc - durations[last])
}

async fn write_out(out: &mut CountingWriter<'_>, bytes: &[u8]) -> Result<(), Error> {
    out.write_all(bytes).await.map_err(Error::Output)?;
    out.flush().await.map_err(Error::Output)
}

/// Reads and drops the first `count` fragments of a seek. Returns the decode time they cover
/// and the stream position after them.
async fn skip_fragments<R: AsyncRead + Unpin>(
    r: &mut R,
    mut offset: u64,
    count: usize,
) -> Result<(u64, u64), Error> {
    let mut tfdt = 0;
    for index in 0..count {
        let (frag, end) = alac::read_next_fragment(r, offset)
            .await
            .map_err(|source| Error::SkipFragment { index, source })?
            .ok_or(Error::SeekPastEnd {
                start: count,
                at: index,
            })?;
        offset = end;
        tfdt += skip_frag_tfdt_delta(&frag);
    }
    Ok((tfdt, offset))
}

/// Counts the bytes that reached the output, so a failed attempt knows whether it may retry.
struct CountingWriter<'a> {
    inner: &'a mut dyn Sink,
    written: u64,
}

impl AsyncWrite for CountingWriter<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let res = Pin::new(&mut *this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            this.written += n as u64;
        }
        res
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

// ── Stall detection ──────────────────────────────────────────────────────────

/// Fails reads once `timeout` has passed without a read of at least [`STALL_THRESHOLD`]
/// bytes. The clock runs from creation and also while nobody reads, as Go's timer did: a
/// read after a long pause fails even if data is waiting.
struct StallGuard<R> {
    inner: R,
    timeout: Duration,
    deadline: Pin<Box<Sleep>>,
}

impl<R> StallGuard<R> {
    fn new(inner: R, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            deadline: Box::pin(tokio::time::sleep(timeout)),
        }
    }
}

fn stalled() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "response timed out")
}

impl<R: AsyncRead + Unpin> AsyncRead for StallGuard<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if Instant::now() >= this.deadline.deadline() {
            return Poll::Ready(Err(stalled()));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() - before >= STALL_THRESHOLD {
                    let next = Instant::now() + this.timeout;
                    this.deadline.as_mut().reset(next);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending if this.deadline.as_mut().poll(cx).is_ready() => {
                Poll::Ready(Err(stalled()))
            }
            other => other,
        }
    }
}

// ── DRM connection ───────────────────────────────────────────────────────────

/// A DRM connection that is replaced before it is expected to fail (after an idle period or
/// a fragment count) instead of only after a failure.
struct DrmConn<'a> {
    dialer: &'a dyn CbcsDialer,
    cancel: &'a CancellationToken,
    in_process: bool,
    io: BufStream<DrmStream>,
    last_used: std::time::Instant,
    frags_since_connect: u32,
}

impl<'a> DrmConn<'a> {
    fn new(dialer: &'a dyn CbcsDialer, cancel: &'a CancellationToken, conn: DrmStream) -> Self {
        Self {
            dialer,
            cancel,
            in_process: dialer.in_process(),
            io: BufStream::new(conn),
            last_used: std::time::Instant::now(),
            frags_since_connect: 0,
        }
    }

    fn replace(&mut self, conn: DrmStream) {
        self.io = BufStream::new(conn);
        self.frags_since_connect = 0;
        self.last_used = std::time::Instant::now();
    }

    /// Replaces the connection when it has been idle too long or served too many fragments.
    /// Returns whether it did; on a dial failure the current connection stays, and a decrypt
    /// error on it triggers the reactive [`redial`](Self::redial).
    async fn refresh_if_needed(&mut self) -> bool {
        let frag_limit_hit = DRM_FRAG_LIMIT.is_some_and(|l| self.frags_since_connect >= l);
        // An in-process pipe has no socket for a peer to drop, so idleness proves nothing,
        // and a reconnect is expensive: it opens fresh key contexts in the Android library
        // (seconds the first time), right when a network-starved track is already late.
        if self.in_process && !frag_limit_hit {
            return false;
        }
        if self.last_used.elapsed() < DRM_IDLE_TIMEOUT && !frag_limit_hit {
            return false;
        }
        let reason = if frag_limit_hit {
            "fragment limit"
        } else {
            "idle"
        };
        match self.dialer.dial_cbcs(self.cancel).await {
            Ok(conn) => {
                let _ = alac::close(self.io.get_mut()).await;
                self.replace(conn);
                info!("cbcs: proactive reconnect ({reason}) succeeded");
                true
            }
            Err(e) => {
                warn!(
                    "cbcs: proactive reconnect ({reason}) failed: {e}, continuing with current connection"
                );
                false
            }
        }
    }

    /// Replaces the connection after a decrypt failure on fragment `index`.
    async fn redial(&mut self, index: usize) -> Result<(), Error> {
        let _ = alac::close(self.io.get_mut()).await;
        // When playback is torn down the pipe closes because the server side exited: not a
        // transient error, and dialing into a half-shut DRM backend can crash it.
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let conn = self
            .dialer
            .dial_cbcs(self.cancel)
            .await
            .map_err(|source| Error::Redial { index, source })?;
        self.replace(conn);
        Ok(())
    }

    fn record_use(&mut self) {
        self.last_used = std::time::Instant::now();
        self.frags_since_connect += 1;
    }

    async fn close(mut self) {
        let _ = alac::close(self.io.get_mut()).await;
    }
}

// ── Keys ─────────────────────────────────────────────────────────────────────

/// The key URIs of segments 0 (prefetch) and 1 (track content key) only, like
/// zhaarey/apple-music-downloader: later EXT-X-KEY tags are ignored there, and every later
/// fragment decrypts with the segment-1 key.
fn upstream_keys(media: &CbcsMedia) -> Vec<Option<String>> {
    media
        .segments
        .iter()
        .take(2)
        .map(|s| s.key_uri.clone())
        .collect()
}

/// The key in force at fragment `i`: the last key declared at or before it.
fn effective_key(keys: &[Option<String>], i: usize) -> Option<&str> {
    let last = i.min(keys.len().checked_sub(1)?);
    keys[..=last].iter().rev().find_map(Option::as_deref)
}

/// Whether `uri` is the shared prefetch key. It is a real key (segment 0), but the DRM
/// backend opens it once under adamID "0", so per-track warming skips it.
fn is_sentinel_key(uri: &str) -> bool {
    uri.eq_ignore_ascii_case(PREFETCH_KEY)
}

/// The adamID sent with a key URI: `"0"` for the prefetch key, else the track's.
fn key_adam_id<'a>(adam_id: &'a str, uri: &str) -> &'a str {
    if uri == PREFETCH_KEY { "0" } else { adam_id }
}

/// Selects `uri` on a connection that has no key yet (fresh or just reconnected). Unlike a
/// mid-stream switch it has no `switch_keys` before it: the server's first read on a new
/// connection is the adamID. `None` sends nothing.
async fn send_key_handshake<W: AsyncWrite + Unpin>(
    conn: &mut W,
    adam_id: &str,
    uri: Option<&str>,
) -> io::Result<()> {
    let Some(uri) = uri else { return Ok(()) };
    alac::send_string(conn, key_adam_id(adam_id, uri)).await?;
    alac::send_string(conn, uri).await
}

/// The track's own content key (the first key of segments 0 and 1 that is not the shared
/// prefetch key), to pre-warm with [`warm_cbcs_key`].
pub fn first_audio_key_uri(media: &CbcsMedia) -> Option<&str> {
    media
        .segments
        .iter()
        .take(2)
        .filter_map(|s| s.key_uri.as_deref())
        .find(|k| !is_sentinel_key(k))
}

/// Opens a throwaway connection, selects `key_uri` and closes it. The DRM backend then
/// fetches the FairPlay key from Apple and caches it in the Android store, so the stream's
/// own connection gets it in under 10 ms instead of about 2 s. Spawn it as soon as the track
/// and key are known, so the fetch overlaps the CDN download. The prefetch key is skipped:
/// the backend warms it itself.
pub async fn warm_cbcs_key(dialer: &dyn CbcsDialer, adam_id: &str, key_uri: &str) {
    if adam_id.is_empty() || key_uri.is_empty() || is_sentinel_key(key_uri) {
        return;
    }
    let id = key_adam_id(adam_id, key_uri);
    let (Ok(id_len), Ok(uri_len)) = (alac::short_len(id), alac::short_len(key_uri)) else {
        return;
    };
    // One write, so the server reads both fields before it sees the close.
    let mut msg = Vec::with_capacity(2 + id.len() + key_uri.len());
    msg.push(id_len);
    msg.extend_from_slice(id.as_bytes());
    msg.push(uri_len);
    msg.extend_from_slice(key_uri.as_bytes());
    let cancel = CancellationToken::new();
    let _ = tokio::time::timeout(WARM_TIMEOUT, async {
        let mut conn = dialer.dial_cbcs(&cancel).await?;
        conn.write_all(&msg).await?;
        conn.shutdown().await
    })
    .await;
}

// ── Output patching ──────────────────────────────────────────────────────────

/// Writes the track length into mvhd, tkhd and mdhd: Apple's CMAF moov says 0, and mpv then
/// stops after the first fragment. 0 leaves the moov unchanged.
fn patch_moov_duration(init: &mut InitSegment, duration_ms: u32) {
    let Some(moov) = init.moov_mut() else { return };
    if duration_ms == 0 {
        return;
    }
    let ms = u64::from(duration_ms);
    let Some(mvhd) = moov.mvhd_mut() else { return };
    let timescale = match mvhd.timescale() {
        0 => 1000,
        ts => u64::from(ts),
    };
    let total = ms * timescale / 1000;
    mvhd.set_duration(total);
    for trak in moov.traks_mut() {
        if let Some(tkhd) = trak.tkhd_mut() {
            tkhd.set_duration(total);
        }
        if let Some(mdhd) = trak.mdhd_mut() {
            let d = ms * u64::from(mdhd.timescale()) / 1000;
            mdhd.set_duration(d);
        }
    }
}

/// The sample duration of `frag`'s first traf: the tfhd default when set and non-zero.
fn default_duration(frag: &Fragment) -> u32 {
    frag.moof()
        .and_then(|m| m.traf())
        .and_then(|t| t.tfhd())
        .filter(|h| h.has_default_sample_duration() && h.default_sample_duration > 0)
        .map_or(DEFAULT_SAMPLE_DURATION, |h| h.default_sample_duration)
}

/// The decode time `frag` covers (all truns of its first traf), without changing it. Keeps
/// the decode time running through the fragments a seek drops.
fn skip_frag_tfdt_delta(frag: &Fragment) -> u64 {
    let dur = default_duration(frag);
    frag.moof()
        .and_then(|m| m.traf())
        .map_or(0, |t| t.truns().map(|r| r.duration(dur)).sum())
}

/// Sets the first traf's decode time to `tfdt` (as a 64-bit tfdt), normalises its track ID
/// and sample description index to 1, moves the trun data offsets by the moof size change,
/// and returns the decode time of the next fragment.
///
/// All truns count: Apple's ALAC moofs carry 11 (10 × 16 + 1 frames), and counting only the
/// first makes VLC report a "Fragment sequence discontinuity" about 28 s in.
fn patch_alac_fragment(frag: &mut Fragment, tfdt: u64) -> u64 {
    let dur = default_duration(frag);
    let Some(moof) = frag.moof_mut() else {
        return tfdt;
    };
    let old_size = moof.size();
    let Some(traf) = moof.traf_mut() else {
        return tfdt;
    };
    if let Some(t) = traf.tfdt_mut() {
        t.set_base_media_decode_time(tfdt);
        t.version = 1;
    } else {
        // ISOBMFF wants tfdt before trun. Like the Go code, a copy goes before every trun.
        let mut t = Tfdt::new(tfdt);
        t.version = 1;
        let children = std::mem::take(&mut traf.children);
        for child in children {
            if matches!(child, Mp4Box::Trun(_)) {
                traf.children.push(t.into());
            }
            traf.children.push(child);
        }
    }
    // Apple's CDN changes the track ID and sample description index in later fragments,
    // which makes ffmpeg silently skip the trun.
    if let Some(tfhd) = traf.tfhd_mut() {
        tfhd.track_id = 1;
        if tfhd.has_sample_description_index() {
            tfhd.sample_description_index = 1;
        }
    }

    let diff = (moof.size() as i32).wrapping_sub(old_size as i32);
    let Some(traf) = moof.traf_mut() else {
        return tfdt;
    };
    if diff != 0 {
        for trun in traf.truns_mut().filter(|t| t.has_data_offset()) {
            trun.data_offset = trun.data_offset.wrapping_add(diff);
        }
    }
    tfdt + traf.truns().map(|t| t.duration(dur)).sum::<u64>()
}

/// [`patch_alac_fragment`] plus the mfhd sequence number: Apple's start at 1 (legal), but
/// VLC counts from 0 and reports a discontinuity on the first fragment otherwise.
fn patch_alac_fragment_seq(frag: &mut Fragment, seq: u32, tfdt: u64) -> u64 {
    if let Some(mfhd) = frag.moof_mut().and_then(|m| m.mfhd_mut()) {
        mfhd.sequence_number = seq;
    }
    patch_alac_fragment(frag, tfdt)
}

#[cfg(test)]
mod tests;
