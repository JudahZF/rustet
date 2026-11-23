//! The foundational types of the media engine.
//!
//! A pipeline is:
//!
//! ```text
//! Source → Stage → Stage → Stage → Sink
//! ```
//!
//! The [`Source`] produces encrypted bytes. Each [`Stage`] reads from the previous one and writes
//! to the next. The last stage writes to whatever [`Sink`] the caller gives: an HTTP response
//! body, a file, the stdin of FFmpeg.
//!
//! Decryption is just a stage ([`DecryptStage`]). So are transcode, repair, remux and gain
//! normalisation. Adding a processing step never changes the pipeline interface.
//!
//! [`Stream`] bundles a source, its stages and the stream's type metadata, so [`run`] is the
//! only call site needed at every layer above.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio_util::sync::CancellationToken;

/// Buffer of each in-memory pipe between two links of the chain.
const PIPE_BUFFER: usize = 64 * 1024;

// ── Stream type metadata ─────────────────────────────────────────────────────

/// The media track a stream carries. Callers ask a session for a track by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    /// Primary audio (ALAC, AAC, Atmos).
    Audio,
    /// Video track (H.264, HEVC).
    Video,
}

impl StreamKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The stream's output format. For a stream with a decrypt stage this is the codec inside the
/// fMP4; for a transcoded stream it is the transcoded format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Alac,
    Aac,
    Atmos,
    H264,
}

impl Codec {
    /// The wire name used in the engine API (`"alac"`, `"aac"`, `"atmos"`, `"h264"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alac => "alac",
            Self::Aac => "aac",
            Self::Atmos => "atmos",
            Self::H264 => "h264",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Any error that a source, stage or provider wraps into [`PipelineError::Other`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The part of the chain that panicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Source,
    Stage,
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Source => "source",
            Self::Stage => "stage",
        })
    }
}

/// The error of a pipeline run. Crates that implement sources, stages or providers convert their
/// own errors into this type, usually through `Other`.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The [`StreamContext`] was cancelled: the client went away. Not a failure of the stream.
    #[error("cancelled")]
    Cancelled,
    /// Reading or writing failed. `BrokenPipe` here usually means the consumer stopped reading.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A source or stage panicked, typically on malformed network data. The panic is confined
    /// to this one stream.
    #[error("pipeline {component} panicked: {message}")]
    Panicked {
        component: Component,
        message: String,
    },
    /// Any other failure. Downcast to find the original error.
    #[error(transparent)]
    Other(#[from] BoxError),
}

// ── Context ──────────────────────────────────────────────────────────────────

/// Per-run state passed to every source and stage.
#[derive(Debug, Clone, Default)]
pub struct StreamContext {
    /// Cancelled when the consumer goes away. [`run`] stops when it fires.
    pub cancel: CancellationToken,
    /// On the seek path: the actual (segment-granular) start time in seconds of the first
    /// segment. The passthrough decryptor seeds its tfdt accumulator from it, so timestamps
    /// track the song position instead of starting at the segment-relative 0.
    pub seek_actual_start: Option<f64>,
    /// On the seek path: the exact requested time in seconds, so the passthrough decryptor can
    /// drop leading fragments before it (sub-segment accuracy).
    pub seek_target: Option<f64>,
}

impl StreamContext {
    /// A context with no seek values.
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            seek_actual_start: None,
            seek_target: None,
        }
    }

    /// `Err(Cancelled)` once the context is cancelled. For loops in sources and stages.
    pub fn check(&self) -> Result<(), PipelineError> {
        if self.cancel.is_cancelled() {
            Err(PipelineError::Cancelled)
        } else {
            Ok(())
        }
    }
}

// ── Sink ─────────────────────────────────────────────────────────────────────

/// The writer a source or stage writes into. A sink that fronts an HTTP response can also take
/// response headers; sources use that to forward `Content-Length`. Every other sink ignores them.
pub trait Sink: AsyncWrite + Send + Unpin {
    /// Sets a response header before the first byte is written. The default does nothing.
    fn set_header(&mut self, _name: &str, _value: &str) {}
}

impl<S: Sink + ?Sized> Sink for &mut S {
    fn set_header(&mut self, name: &str, value: &str) {
        (**self).set_header(name, value);
    }
}

impl<S: Sink + ?Sized> Sink for Box<S> {
    fn set_header(&mut self, name: &str, value: &str) {
        (**self).set_header(name, value);
    }
}

impl Sink for Vec<u8> {}
impl Sink for DuplexStream {}
impl Sink for tokio::io::Sink {}
impl Sink for tokio::fs::File {}
impl Sink for tokio::process::ChildStdin {}

/// Adapts any [`AsyncWrite`] into a [`Sink`] that ignores headers.
#[derive(Debug, Default)]
pub struct HeaderlessSink<W>(pub W);

impl<W: AsyncWrite + Unpin> AsyncWrite for HeaderlessSink<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

impl<W: AsyncWrite + Send + Unpin> Sink for HeaderlessSink<W> {}

// ── Traits ───────────────────────────────────────────────────────────────────

/// Assembles a complete encrypted byte stream and writes it to `w`. It handles segment
/// fetching, ordering, caching and retry. It never touches key material.
#[async_trait]
pub trait Source: Send + Sync {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError>;

    /// The seek capability. A seekable source returns `Some(self)`.
    fn seekable(&self) -> Option<&dyn Seekable> {
        None
    }
}

/// A source that can produce a new source starting near a time offset.
pub trait Seekable: Send + Sync {
    /// A source that starts at the segment holding `start_sec`, and the presentation time of
    /// that segment's start (`actual_start`, at or slightly before `start_sec`). The new source
    /// uses the same transport and must run through the same stages as the original stream.
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64);

    /// The cumulative presentation start time in seconds of each HLS segment: entry `i` is the
    /// start of segment `i`, so entry 0 is always 0. Callers use it to compute `actual_start`
    /// for any position without waiting for the producer to index past it.
    fn segment_timings(&self) -> Option<Vec<f64>> {
        None
    }

    /// The fragment-level seek capability. A source that has it returns `Some(self)`.
    fn direct_seekable(&self) -> Option<&dyn DirectSeekable> {
        None
    }
}

/// Where a fragment-level seek starts. The source emits only the fragments from the one holding
/// the target onward, still on the stream's raw timeline.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DirectInfo {
    /// Playlist time of the first emitted fragment.
    pub start_sec: f64,
    /// Seconds to add to emitted timestamps to get playlist time.
    pub ts_offset: f64,
}

/// Seeks at fragment granularity instead of segment granularity.
#[async_trait]
pub trait DirectSeekable: Send + Sync {
    /// Plans the seek with a few small network reads. Callers fall back to
    /// [`Seekable::source_from`] on any error.
    async fn direct_source_from(
        &self,
        cancel: &CancellationToken,
        start_sec: f64,
    ) -> Result<(Arc<dyn Source>, DirectInfo), PipelineError>;
}

/// One transformation step: reads the previous link's output from `r` and writes to `w`.
#[async_trait]
pub trait Stage: Send + Sync {
    async fn process(
        &self,
        ctx: &StreamContext,
        r: &mut (dyn AsyncRead + Send + Unpin),
        w: &mut dyn Sink,
    ) -> Result<(), PipelineError>;
}

/// Decrypts an encrypted fMP4 stream into clear output. It is the sole holder of key bytes in
/// the engine. Only the FairPlay crate creates decryptors; everything else uses this trait.
#[async_trait]
pub trait Decryptor: Send + Sync {
    async fn decrypt(
        &self,
        ctx: &StreamContext,
        r: &mut (dyn AsyncRead + Send + Unpin),
        w: &mut dyn Sink,
    ) -> Result<(), PipelineError>;
}

/// A [`Decryptor`] used as a [`Stage`]. The only stage that touches key material.
pub struct DecryptStage<D>(pub D);

#[async_trait]
impl<D: Decryptor> Stage for DecryptStage<D> {
    async fn process(
        &self,
        ctx: &StreamContext,
        r: &mut (dyn AsyncRead + Send + Unpin),
        w: &mut dyn Sink,
    ) -> Result<(), PipelineError> {
        self.0.decrypt(ctx, r, w).await
    }
}

// ── Stream ───────────────────────────────────────────────────────────────────

/// A complete media stream, ready to [`run`]. Every layer above the pipeline deals only in
/// streams. Cloning is cheap: the source and stages are shared.
///
/// `kind` and `codec` are set by the provider that built the track, not by the DRM layer,
/// because the provider knows what the content is.
#[derive(Clone)]
pub struct Stream {
    pub source: Arc<dyn Source>,
    pub stages: Vec<Arc<dyn Stage>>,
    pub kind: StreamKind,
    pub codec: Codec,
    /// The length the stream's own playlist declares (the sum of its segment durations), or 0
    /// when unknown. It can differ from the catalog length, and it is what the player reaches.
    pub duration_ms: u32,
    /// The MSE codec string of the variant actually opened, when it can differ from the one the
    /// track was advertised with (a video that fell back to a lower variant). `None` means the
    /// track's own string applies.
    pub codec_string: Option<String>,
}

// ── Run ──────────────────────────────────────────────────────────────────────

/// Runs `stream` into `dst`: source → stages\[0\] → stages\[1\] → … → `dst`, with an in-memory
/// pipe between each pair of links. Returns when the chain completes, or with
/// [`PipelineError::Cancelled`] as soon as `ctx` is cancelled.
///
/// A panic in the source or a stage becomes [`PipelineError::Panicked`] for this stream only.
pub async fn run(
    ctx: &StreamContext,
    stream: &Stream,
    dst: &mut dyn Sink,
) -> Result<(), PipelineError> {
    tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => Err(PipelineError::Cancelled),
        r = run_chain(ctx, &*stream.source, &stream.stages, dst) => r,
    }
}

/// Feeds everything before the last stage through a pipe into the last stage, which writes
/// straight to `dst`. Both halves run concurrently in the caller's task.
fn run_chain<'a>(
    ctx: &'a StreamContext,
    src: &'a dyn Source,
    stages: &'a [Arc<dyn Stage>],
    dst: &'a mut dyn Sink,
) -> BoxFuture<'a, Result<(), PipelineError>> {
    Box::pin(async move {
        let Some((last, upstream)) = stages.split_last() else {
            return guarded(Component::Source, src.stream(ctx, dst)).await;
        };
        let (mut pipe_w, pipe_r) = tokio::io::duplex(PIPE_BUFFER);
        let stage_done = &AtomicBool::new(false);
        let upstream_failed = &AtomicBool::new(false);

        let feed = async move {
            let res = run_chain(ctx, src, upstream, &mut pipe_w).await;
            // Set before the writer drops, so the stage sees it at end of input.
            upstream_failed.store(res.is_err(), Ordering::Release);
            // Dropping the writer is the end-of-stream signal for the stage.
            drop(pipe_w);
            (res, stage_done.load(Ordering::Acquire))
        };
        let consume = async move {
            let mut input = PipeReader {
                inner: pipe_r,
                upstream_failed,
            };
            let res = guarded(Component::Stage, last.process(ctx, &mut input, dst)).await;
            // A stage may return before draining its input. Dropping the reader makes the
            // upstream writes fail with BrokenPipe instead of blocking forever.
            drop(input);
            stage_done.store(true, Ordering::Release);
            res
        };
        let ((feed_res, feed_ended_after_stage), stage_res) = tokio::join!(feed, consume);

        // An upstream failure that happened while the stage still ran is the root cause: the
        // stage saw only a truncated input. One that happened after the stage returned is just
        // the closed pipe, so the stage's result stands.
        match feed_res {
            Err(e) if !feed_ended_after_stage => Err(e),
            _ => stage_res,
        }
    })
}

/// The read half of a pipe. When the upstream link failed, end of input is an error instead of
/// a clean EOF, so a stage never takes a truncated stream for a complete one.
struct PipeReader<'a> {
    inner: DuplexStream,
    upstream_failed: &'a AtomicBool,
}

impl AsyncRead for PipeReader<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        let at_eof = matches!(res, Poll::Ready(Ok(())))
            && buf.filled().len() == before
            && buf.remaining() > 0;
        if at_eof && self.upstream_failed.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::other("pipeline upstream failed")));
        }
        res
    }
}

/// Sources and stages parse network data (fMP4 from the CDN) with code that can panic on
/// malformed input. A panic must fail that one stream, not the engine.
async fn guarded(
    component: Component,
    fut: impl Future<Output = Result<(), PipelineError>>,
) -> Result<(), PipelineError> {
    AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .unwrap_or_else(|payload| {
            let message = panic_message(&*payload);
            tracing::error!(%component, %message, "pipeline component panicked");
            Err(PipelineError::Panicked { component, message })
        })
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Input<'a> = &'a mut (dyn AsyncRead + Send + Unpin);

    fn stream(source: impl Source + 'static, stages: Vec<Arc<dyn Stage>>) -> Stream {
        Stream {
            source: Arc::new(source),
            stages,
            kind: StreamKind::Audio,
            codec: Codec::Aac,
            duration_ms: 0,
            codec_string: None,
        }
    }

    async fn run_to_vec(s: &Stream) -> Result<Vec<u8>, PipelineError> {
        let mut dst = Vec::new();
        run(&StreamContext::default(), s, &mut dst)
            .await
            .map(|()| dst)
    }

    #[derive(Debug, thiserror::Error, PartialEq)]
    #[error("{0}")]
    struct Sentinel(&'static str);

    fn assert_sentinel(err: PipelineError, want: &'static str) {
        match err {
            PipelineError::Other(e) => {
                assert_eq!(e.downcast_ref::<Sentinel>(), Some(&Sentinel(want)))
            }
            other => panic!("expected the sentinel {want:?}, got {other:?}"),
        }
    }

    /// Writes a fixed payload.
    struct Payload(Vec<u8>);

    #[async_trait]
    impl Source for Payload {
        async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
            ctx.check()?;
            w.write_all(&self.0).await?;
            Ok(())
        }
    }

    /// Writes some bytes, then fails.
    struct FailingSource(&'static str);

    #[async_trait]
    impl Source for FailingSource {
        async fn stream(&self, _: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
            w.write_all(b"partial").await?;
            Err(BoxError::from(Sentinel(self.0)).into())
        }
    }

    /// Appends a suffix to everything it reads, and logs its name once its input is drained.
    struct Suffix {
        suffix: &'static str,
        name: &'static str,
        log: Option<Arc<Mutex<Vec<&'static str>>>>,
    }

    impl Suffix {
        fn stage(suffix: &'static str) -> Arc<dyn Stage> {
            Arc::new(Self {
                suffix,
                name: "",
                log: None,
            })
        }
    }

    #[async_trait]
    impl Stage for Suffix {
        async fn process(
            &self,
            _: &StreamContext,
            r: Input<'_>,
            w: &mut dyn Sink,
        ) -> Result<(), PipelineError> {
            let mut data = Vec::new();
            r.read_to_end(&mut data).await?;
            // Logged after reading, so the log follows data-flow order: s1, s2, s3.
            if let Some(log) = &self.log {
                log.lock().unwrap().push(self.name);
            }
            w.write_all(&data).await?;
            w.write_all(self.suffix.as_bytes()).await?;
            Ok(())
        }
    }

    /// Drains its input, then fails.
    struct FailingStage(&'static str);

    #[async_trait]
    impl Stage for FailingStage {
        async fn process(
            &self,
            _: &StreamContext,
            r: Input<'_>,
            _: &mut dyn Sink,
        ) -> Result<(), PipelineError> {
            tokio::io::copy(r, &mut tokio::io::sink()).await?;
            Err(BoxError::from(Sentinel(self.0)).into())
        }
    }

    struct Copy;

    #[async_trait]
    impl Stage for Copy {
        async fn process(
            &self,
            _: &StreamContext,
            r: Input<'_>,
            w: &mut dyn Sink,
        ) -> Result<(), PipelineError> {
            tokio::io::copy(r, w).await?;
            Ok(())
        }
    }

    /// Other crates call `run` inside `async_trait` methods and spawned tasks.
    #[test]
    fn run_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let (ctx, s, mut dst) = (
            StreamContext::default(),
            stream(Payload(vec![]), vec![]),
            Vec::new(),
        );
        assert_send(&run(&ctx, &s, &mut dst));
    }

    #[tokio::test]
    async fn zero_stages() {
        let s = stream(Payload(b"hello world".to_vec()), vec![]);
        assert_eq!(run_to_vec(&s).await.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn one_stage() {
        let s = stream(Payload(b"body".to_vec()), vec![Suffix::stage("-END")]);
        assert_eq!(run_to_vec(&s).await.unwrap(), b"body-END");
    }

    #[tokio::test]
    async fn three_stages_run_in_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mk = |name, suffix| -> Arc<dyn Stage> {
            Arc::new(Suffix {
                suffix,
                name,
                log: Some(Arc::clone(&log)),
            })
        };
        let s = stream(
            Payload(b"x".to_vec()),
            vec![mk("s1", "1"), mk("s2", "2"), mk("s3", "3")],
        );
        assert_eq!(run_to_vec(&s).await.unwrap(), b"x123");
        assert_eq!(*log.lock().unwrap(), ["s1", "s2", "s3"]);
    }

    #[tokio::test]
    async fn cancelled_context() {
        let ctx = StreamContext::default();
        ctx.cancel.cancel();
        let s = stream(Payload(b"data".to_vec()), vec![]);
        let err = run(&ctx, &s, &mut Vec::new()).await.unwrap_err();
        assert!(matches!(err, PipelineError::Cancelled), "got {err:?}");
    }

    #[tokio::test]
    async fn source_error_passes_through_a_stage() {
        let s = stream(FailingSource("source boom"), vec![Suffix::stage("x")]);
        assert_sentinel(run_to_vec(&s).await.unwrap_err(), "source boom");
    }

    /// Reads to the end and reports whether its input ended cleanly.
    struct EofProbe(tokio::sync::mpsc::Sender<bool>);

    #[async_trait]
    impl Stage for EofProbe {
        async fn process(
            &self,
            _: &StreamContext,
            r: Input<'_>,
            _: &mut dyn Sink,
        ) -> Result<(), PipelineError> {
            let clean = r.read_to_end(&mut Vec::new()).await.is_ok();
            let _ = self.0.send(clean).await;
            Ok(())
        }
    }

    /// Go closes the pipe with the source's error; the stage must not see a clean EOF either,
    /// or it could commit a truncated stream (a cache entry, a file).
    #[tokio::test]
    async fn stage_sees_upstream_failure_at_end_of_input() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let s = stream(FailingSource("source boom"), vec![Arc::new(EofProbe(tx))]);
        assert_sentinel(run_to_vec(&s).await.unwrap_err(), "source boom");
        assert_eq!(rx.recv().await, Some(false));
    }

    #[tokio::test]
    async fn stage_error() {
        let s = stream(
            Payload(b"data".to_vec()),
            vec![Arc::new(FailingStage("stage boom"))],
        );
        assert_sentinel(run_to_vec(&s).await.unwrap_err(), "stage boom");
    }

    #[tokio::test]
    async fn large_payload_through_stages_is_intact() {
        // 1 MiB: many times the pipe buffer, so every link has to stream.
        let payload: Vec<u8> = (0..1 << 20)
            .map(|i: u32| (i.wrapping_mul(31)) as u8)
            .collect();
        let s = stream(
            Payload(payload.clone()),
            vec![Arc::new(Copy), Arc::new(Copy)],
        );
        let got = run_to_vec(&s).await.unwrap();
        assert_eq!(got.len(), payload.len());
        assert_eq!(
            Sha256::digest(&got),
            Sha256::digest(&payload),
            "payload corrupted"
        );
    }

    struct Echo(Arc<AtomicBool>);

    #[async_trait]
    impl Decryptor for Echo {
        async fn decrypt(
            &self,
            _: &StreamContext,
            r: Input<'_>,
            w: &mut dyn Sink,
        ) -> Result<(), PipelineError> {
            self.0.store(true, Ordering::SeqCst);
            tokio::io::copy(r, w).await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn decrypt_stage_calls_the_decryptor() {
        let called = Arc::new(AtomicBool::new(false));
        let s = stream(
            Payload(b"encrypted-bytes".to_vec()),
            vec![Arc::new(DecryptStage(Echo(Arc::clone(&called))))],
        );
        assert_eq!(run_to_vec(&s).await.unwrap(), b"encrypted-bytes");
        assert!(called.load(Ordering::SeqCst));
    }

    // ── Panics (panic_test.go) ──

    struct PanicSource;

    #[async_trait]
    impl Source for PanicSource {
        async fn stream(&self, _: &StreamContext, _: &mut dyn Sink) -> Result<(), PipelineError> {
            panic!("slice bounds out of range")
        }
    }

    struct PanicStage;

    #[async_trait]
    impl Stage for PanicStage {
        async fn process(
            &self,
            _: &StreamContext,
            _: Input<'_>,
            _: &mut dyn Sink,
        ) -> Result<(), PipelineError> {
            panic!("bad box")
        }
    }

    /// A malformed fragment that makes a parser panic must fail that one stream, not crash
    /// the engine, including when the source sits behind stages.
    #[tokio::test]
    async fn panics_become_errors() {
        let cases = [
            (
                "source alone",
                stream(PanicSource, vec![]),
                Component::Source,
            ),
            (
                "source behind stages",
                stream(PanicSource, vec![Arc::new(Copy), Arc::new(Copy)]),
                Component::Source,
            ),
            (
                "stage",
                stream(Payload(b"audio".to_vec()), vec![Arc::new(PanicStage)]),
                Component::Stage,
            ),
        ];
        for (name, s, want) in cases {
            match run_to_vec(&s).await {
                Err(PipelineError::Panicked { component, .. }) => {
                    assert_eq!(component, want, "{name}")
                }
                other => panic!("{name}: got {other:?}, want a panic turned into an error"),
            }
        }
    }

    // ── Early stage exit (runchain_test.go) ──

    /// Writes forever, and reports the write error that finally stops it.
    struct Endless(tokio::sync::mpsc::Sender<io::ErrorKind>);

    #[async_trait]
    impl Source for Endless {
        async fn stream(&self, _: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
            let buf = [0u8; 1024];
            loop {
                if let Err(e) = w.write_all(&buf).await {
                    let _ = self.0.send(e.kind()).await;
                    return Err(e.into());
                }
            }
        }
    }

    /// Reads a little, then gives up.
    struct GivesUp;

    #[async_trait]
    impl Stage for GivesUp {
        async fn process(
            &self,
            _: &StreamContext,
            r: Input<'_>,
            _: &mut dyn Sink,
        ) -> Result<(), PipelineError> {
            let _ = r.read(&mut [0u8; 10]).await;
            Err(BoxError::from(Sentinel("stage failed")).into())
        }
    }

    #[tokio::test]
    async fn source_is_released_when_the_stage_stops_early() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let s = stream(Endless(tx), vec![Arc::new(GivesUp)]);
        let err = tokio::time::timeout(Duration::from_secs(2), run_to_vec(&s))
            .await
            .expect("run blocked after the stage returned")
            .unwrap_err();
        assert_eq!(err.to_string(), "stage failed");
        assert_eq!(rx.recv().await, Some(io::ErrorKind::BrokenPipe));
    }

    // ── Headers ──

    #[derive(Default)]
    struct HeaderSink {
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    }

    impl AsyncWrite for HeaderSink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.body).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Sink for HeaderSink {
        fn set_header(&mut self, name: &str, value: &str) {
            self.headers.push((name.to_owned(), value.to_owned()));
        }
    }

    struct SizedSource;

    #[async_trait]
    impl Source for SizedSource {
        async fn stream(&self, _: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
            w.set_header("Content-Length", "2");
            w.write_all(b"ok").await?;
            Ok(())
        }
    }

    /// A source that writes straight to the destination can set its headers; behind a stage
    /// the pipe swallows them, because the stage may change the length.
    #[tokio::test]
    async fn headers_reach_the_sink_only_without_stages() {
        let ctx = StreamContext::default();
        let mut direct = HeaderSink::default();
        run(&ctx, &stream(SizedSource, vec![]), &mut direct)
            .await
            .unwrap();
        assert_eq!(
            direct.headers,
            [("Content-Length".to_owned(), "2".to_owned())]
        );

        let mut staged = HeaderSink::default();
        run(
            &ctx,
            &stream(SizedSource, vec![Arc::new(Copy)]),
            &mut staged,
        )
        .await
        .unwrap();
        assert!(staged.headers.is_empty());
        assert_eq!(staged.body, b"ok");
    }
}
