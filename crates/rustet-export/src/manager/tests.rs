use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use futures::future::BoxFuture;
use rustet_core::media::{Metadata, OpenTrack, Provider, Session, SessionKind, Track};
use rustet_core::pipeline::{Codec, PipelineError, Source as PipelineSource, Stream};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use super::*;
use crate::TailReader;

/// Replaces the worker's execution of a popped item.
pub(super) type Hook =
    Arc<dyn Fn(Arc<Inner>, Arc<WorkItem>) -> BoxFuture<'static, ()> + Send + Sync>;

// ── Fakes ────────────────────────────────────────────────────────────────────

const NETWORK: &[u8] = b"NETWORK-AUDIO-BYTES";

/// Writes `head`, waits for `gate` (if any), then writes `tail`.
#[derive(Clone)]
struct Payload {
    head: Vec<u8>,
    tail: Vec<u8>,
    gate: Option<CancellationToken>,
}

#[async_trait]
impl PipelineSource for Payload {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        w.write_all(&self.head).await?;
        if let Some(gate) = &self.gate {
            tokio::select! {
                () = gate.cancelled() => {}
                () = ctx.cancel.cancelled() => return Err(PipelineError::Cancelled),
            }
        }
        w.write_all(&self.tail).await?;
        Ok(())
    }
}

fn payload(head: &[u8], tail: &[u8], gate: Option<CancellationToken>) -> Payload {
    Payload {
        head: head.to_vec(),
        tail: tail.to_vec(),
        gate,
    }
}

struct Opener(Arc<Payload>, StreamKind);

#[async_trait]
impl OpenTrack for Opener {
    async fn open(&self, _: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(Stream {
            source: Arc::clone(&self.0) as Arc<dyn PipelineSource>,
            stages: Vec::new(),
            kind: self.1,
            codec: Codec::Aac,
            duration_ms: 0,
            codec_string: None,
        })
    }
}

fn track(kind: StreamKind, payload: Payload) -> Track {
    Track {
        kind,
        codec: Codec::Aac,
        codec_string: String::new(),
        sample_rate: 0,
        bit_depth: 0,
        bit_rate: 0,
        channel_count: 0,
        codec_mime_type: String::new(),
        spatial_audio: None,
        open: Arc::new(Opener(Arc::new(payload), kind)),
    }
}

/// Serves per-asset payloads (default: [`NETWORK`]) and counts opens, so tests can prove the
/// network path was or was not taken.
#[derive(Default)]
struct FakeProvider {
    opens: Mutex<HashMap<String, usize>>,
    video: bool,
    sources: Mutex<HashMap<String, Payload>>,
}

impl FakeProvider {
    fn opens(&self) -> usize {
        self.opens.lock().unwrap().values().sum()
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn open(
        &self,
        _: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<Session, PipelineError> {
        *self
            .opens
            .lock()
            .unwrap()
            .entry(req.asset_id.clone())
            .or_default() += 1;
        let audio = self
            .sources
            .lock()
            .unwrap()
            .get(&req.asset_id)
            .cloned()
            .unwrap_or_else(|| payload(NETWORK, b"", None));
        let mut tracks = vec![track(StreamKind::Audio, audio)];
        if self.video {
            tracks.push(track(StreamKind::Video, payload(b"VIDEO", b"", None)));
        }
        Ok(Session {
            kind: if self.video {
                SessionKind::MusicVideo
            } else {
                SessionKind::Song
            },
            metadata: Metadata::default(),
            tracks,
            video_heights: Vec::new(),
            mv_progressive_url: None,
            mv_download_key: None,
        })
    }
}

fn manager_with(
    provider: Arc<FakeProvider>,
    opts: Options,
) -> (Manager, Arc<rustet_playback::Manager>) {
    let pm = rustet_playback::Manager::new(provider);
    (
        Manager::new(Arc::clone(&pm), reqwest::Client::new(), opts),
        pm,
    )
}

fn manager() -> Manager {
    manager_with(Arc::default(), Options::default()).0
}

fn request(asset: &str, dir: &Path, priority: i32) -> ExportRequest {
    ExportRequest {
        asset_id: asset.into(),
        output_dir: dir.to_string_lossy().into_owned(),
        priority,
        ..ExportRequest::default()
    }
}

// ── Queue ────────────────────────────────────────────────────────────────────

/// A manager whose worker hands popped items to the test instead of running them, so ordering
/// and races are deterministic. Each item waits for one `proceed()` and then ends done.
struct QueueHarness {
    m: Manager,
    dir: tempfile::TempDir,
    popped: mpsc::UnboundedReceiver<Arc<WorkItem>>,
    proceed: mpsc::UnboundedSender<()>,
}

impl QueueHarness {
    fn new() -> Self {
        Self::with(|inner, item| {
            inner.advance(&item.id, Phase::Done, 100);
        })
    }

    /// `after` runs on each item once the test lets it proceed.
    fn with(after: fn(&Arc<Inner>, Arc<WorkItem>)) -> Self {
        Self::with_async(move |inner, item| {
            after(&inner, item);
            async {}.boxed()
        })
    }

    fn with_async(
        after: impl Fn(Arc<Inner>, Arc<WorkItem>) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        let m = manager();
        let (popped_tx, popped) = mpsc::unbounded_channel();
        let (proceed, proceed_rx) = mpsc::unbounded_channel::<()>();
        let proceed_rx = Arc::new(tokio::sync::Mutex::new(proceed_rx));
        let after = Arc::new(after);
        let hook: Hook = Arc::new(move |inner, item| {
            let (popped_tx, proceed_rx, after) = (
                popped_tx.clone(),
                Arc::clone(&proceed_rx),
                Arc::clone(&after),
            );
            async move {
                let _ = popped_tx.send(Arc::clone(&item));
                proceed_rx.lock().await.recv().await;
                after(inner, item).await;
            }
            .boxed()
        });
        *m.inner.hook.lock().unwrap() = Some(hook);
        Self {
            m,
            dir: tempfile::tempdir().unwrap(),
            popped,
            proceed,
        }
    }

    fn enqueue(&self, asset: &str, priority: i32) -> ExportJob {
        self.m
            .enqueue(request(asset, self.dir.path(), priority))
            .unwrap()
    }

    /// Waits for the worker to pop an item (not yet released).
    async fn next(&mut self) -> Arc<WorkItem> {
        tokio::time::timeout(Duration::from_secs(2), self.popped.recv())
            .await
            .expect("worker did not pop")
            .unwrap()
    }

    fn proceed(&self) {
        self.proceed.send(()).unwrap();
    }

    fn index(&self, id: &str) -> usize {
        self.m.get(id).unwrap().queue_index
    }
}

#[tokio::test]
async fn priority_order_with_fifo_ties() {
    let mut h = QueueHarness::new();
    h.enqueue("x", 0);
    let first = h.next().await; // the worker is busy with x; the rest queue up
    h.enqueue("a", 0);
    h.enqueue("b", 5);
    h.enqueue("c", 5);
    h.enqueue("d", 10);
    let mut got = vec![first.req.asset_id.clone()];
    h.proceed();
    for _ in 0..4 {
        got.push(h.next().await.req.asset_id.clone());
        h.proceed();
    }
    assert_eq!(got, ["x", "d", "b", "c", "a"]);
}

#[tokio::test]
async fn queue_index_and_prioritize() {
    let mut h = QueueHarness::new();
    let running = h.enqueue("run", 0);
    h.next().await;
    let a = h.enqueue("a", 0);
    let b = h.enqueue("b", 0);
    assert_eq!(
        (h.index(&a.id), h.index(&b.id), h.index(&running.id)),
        (1, 2, 0)
    );

    let snap = h.m.prioritize(&b.id, 7).unwrap();
    assert_eq!((snap.priority, snap.queue_index), (7, 1));
    assert_eq!(h.index(&a.id), 2, "a after b moved ahead");

    assert_eq!(
        h.m.prioritize(&running.id, 9),
        Err(PrioritizeError::NotQueued)
    );
    assert_eq!(h.m.prioritize("nope", 1), Err(PrioritizeError::NotFound));
    h.proceed();
    assert_eq!(h.next().await.req.asset_id, "b");
    h.proceed();
}

#[tokio::test]
async fn cancel_queued_removes_immediately() {
    let mut h = QueueHarness::new();
    h.enqueue("run", 0);
    h.next().await;
    let a = h.enqueue("a", 0);
    let b = h.enqueue("b", 0);
    assert!(h.m.cancel(&a.id));
    assert_eq!(h.m.inner.queue.len(), 1);
    let j = h.m.get(&a.id).unwrap();
    assert_eq!((j.phase, j.queue_index), (Phase::Cancelled, 0));
    assert_eq!(h.index(&b.id), 1);
    h.proceed();
    assert_eq!(h.next().await.req.asset_id, "b", "the cancelled job ran");
    h.proceed();

    // Cancelling a finished job clears it from the list.
    assert!(h.m.cancel(&a.id));
    assert!(h.m.get(&a.id).is_none());
    assert!(!h.m.cancel(&a.id));
}

// Cancel after the worker popped the job but before it runs: the queue no longer holds it, so it
// counts as running; its token is cancelled and the real execute path ends it cancelled without
// doing any work.
#[tokio::test]
async fn cancel_between_pop_and_execute() {
    let mut h =
        QueueHarness::with_async(|inner, item| async move { inner.execute(item).await }.boxed());
    let j = h.enqueue("a", 0);
    h.next().await;
    assert!(
        h.m.inner.queue.remove(&j.id).is_none(),
        "popped job still queued"
    );
    assert!(h.m.cancel(&j.id));
    assert_eq!(
        h.m.get(&j.id).unwrap().phase,
        Phase::Queued,
        "a running job was marked as if queued"
    );
    h.proceed();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let got = h.m.get(&j.id).expect("job vanished");
        match got.phase {
            Phase::Cancelled => break,
            Phase::Failed => panic!("job failed: {}", got.error),
            _ if tokio::time::Instant::now() > deadline => panic!("job ended {:?}", got.phase),
            _ => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

#[tokio::test]
async fn retry_keeps_priority() {
    let mut h = QueueHarness::new();
    h.enqueue("run", 0);
    h.next().await;
    let a = h.enqueue("a", 4);
    h.m.cancel(&a.id);
    let retried = h.m.retry(&a.id, "tok", "mut").expect("retry failed");
    assert_eq!(retried.priority, 4);
    assert!(h.m.retry("nope", "tok", "mut").is_none());
    assert!(
        h.m.retry(&retried.id, "tok", "mut").is_none(),
        "a queued job is not retryable"
    );
    h.proceed();
    h.next().await;
    h.proceed();
}

#[tokio::test]
async fn credentials_are_not_retained_and_retry_uses_fresh_ones() {
    let mut h = QueueHarness::with(|inner, item| {
        // End it cancelled, as a running job whose token fired would.
        inner.fail(&item.id, &JobError::Cancelled);
    });
    let mut req = request("x", h.dir.path(), 0);
    req.token = "old-tok".into();
    req.mut_token = "old-mut".into();
    let j = h.m.enqueue(req).unwrap();
    let item = h.next().await;
    assert_eq!(
        item.req.token, "old-tok",
        "the running job lost its credentials"
    );
    {
        let jobs = h.m.inner.lock();
        let stored = &jobs[&j.id].request;
        assert!(
            stored.token.is_empty() && stored.mut_token.is_empty(),
            "credentials retained"
        );
    }
    h.m.cancel(&j.id);
    h.proceed();
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.m.get(&j.id).unwrap().phase != Phase::Cancelled {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    h.m.retry(&j.id, "new-tok", "new-mut")
        .expect("retry failed");
    let item = h.next().await;
    assert_eq!(
        (item.req.token.as_str(), item.req.mut_token.as_str()),
        ("new-tok", "new-mut")
    );
    h.proceed();
}

#[tokio::test]
async fn enqueue_events_and_snapshot() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let (m, _pm) = manager_with(
        Arc::default(),
        Options {
            events: Some(Box::new(move |ev| sink.lock().unwrap().push(ev))),
            ..Options::default()
        },
    );
    *m.inner.hook.lock().unwrap() = Some(Arc::new(|_, _| std::future::pending().boxed()));
    let dir = tempfile::tempdir().unwrap();
    let mut req = request("1", dir.path(), 2);
    req.hint_title = "Hint".into();
    let first = m.enqueue(req).unwrap();
    let second = m.enqueue(request("2", dir.path(), 0)).unwrap();
    assert_eq!(
        (first.phase, first.title.as_str(), first.priority),
        (Phase::Queued, "Hint", 2)
    );
    assert!(second.queue_pos > first.queue_pos);
    let ev = events.lock().unwrap()[0].clone();
    assert_eq!((ev.job_id, ev.phase), (first.id.clone(), Phase::Queued));
    let ids: Vec<String> = m.list().into_iter().map(|j| j.id).collect();
    assert_eq!(ids, [first.id, second.id]);
    assert!(matches!(
        m.enqueue(ExportRequest::default()),
        Err(EnqueueError::MissingAssetId)
    ));
    let mut bad = request("1", dir.path(), 0);
    bad.options.ffmpeg_path = "/bin/sh".into();
    assert!(matches!(m.enqueue(bad), Err(EnqueueError::FfmpegName)));
}

#[tokio::test]
async fn enqueue_refuses_output_dir_outside_roots() {
    let root = tempfile::tempdir().unwrap();
    let (m, _pm) = manager_with(
        Arc::default(),
        Options {
            output_roots: vec![root.path().to_owned()],
            ..Options::default()
        },
    );
    let err = m
        .enqueue(request("1", Path::new("/etc"), 0))
        .expect_err("accepted /etc");
    assert!(
        err.to_string().contains("allowed export directories"),
        "{err}"
    );
}

#[tokio::test]
async fn stop_cancels_running_job() {
    let mut h = QueueHarness::new();
    h.enqueue("long", 0);
    let item = h.next().await;
    h.m.stop().await; // the hook ignores cancellation, so this waits out the bound
    assert!(
        item.cancel.is_cancelled(),
        "stop left the running job's token live"
    );
    assert!(matches!(
        h.m.enqueue(request("late", h.dir.path(), 0)),
        Err(EnqueueError::Closed)
    ));
    h.proceed();
}

// Stop cancels running work, which ends its tool, and waits for the work to finish.
#[cfg(unix)]
#[tokio::test]
async fn stop_joins_running_work_and_its_tool() {
    let started = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let m = manager();
    let (s, f) = (Arc::clone(&started), Arc::clone(&finished));
    *m.inner.hook.lock().unwrap() = Some(Arc::new(move |_, item| {
        let (s, f) = (Arc::clone(&s), Arc::clone(&f));
        async move {
            s.notify_one();
            let res = tool::run_tool(
                &item.cancel,
                "sleep",
                &["30".as_ref()],
                tool::Stdout::Discard,
            )
            .await;
            assert!(matches!(res, Err(ToolError::Cancelled)));
            f.store(true, Ordering::SeqCst);
        }
        .boxed()
    }));
    let dir = tempfile::tempdir().unwrap();
    m.enqueue(request("1", dir.path(), 0)).unwrap();
    started.notified().await;
    let start = std::time::Instant::now();
    m.stop().await;
    assert!(
        finished.load(Ordering::SeqCst),
        "stop returned while the tool still ran"
    );
    assert!(
        start.elapsed() < STOP_WAIT,
        "stop took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn idle_snapshot_is_unthrottled() {
    let m = manager();
    let dir = tempfile::tempdir().unwrap();
    *m.inner.hook.lock().unwrap() = Some(Arc::new(|_, _| std::future::pending().boxed()));
    let j = m.enqueue(request("1", dir.path(), 0)).unwrap();
    m.inner.update(&j.id, |j| {
        j.phase = Phase::Downloading;
        j.source = Some(Source::Network);
    });
    let got = m.get(&j.id).unwrap();
    assert_eq!((got.limit_bps, got.throttled), (None, false));
}

// ── Byte reuse ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeCache {
    path: Option<PathBuf>,
    tail: Option<Box<dyn Fn() -> TailReader + Send + Sync>>,
    calls: AtomicUsize,
    qualifier: Mutex<String>,
}

#[async_trait]
impl AudioCache for FakeCache {
    async fn path(&self, _: &str, q: &str) -> Option<PathBuf> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        q.clone_into(&mut self.qualifier.lock().unwrap());
        self.path.clone()
    }

    fn tail_reader(&self, _: &str, _: &str) -> Option<TailReader> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.tail.as_ref().map(|f| f())
    }
}

/// Yields some bytes, then fails like an abandoned producer.
struct FailingTail(Option<Vec<u8>>);

impl AsyncRead for FailingTail {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.0.take() {
            Some(data) => {
                buf.put_slice(&data);
                Poll::Ready(Ok(()))
            }
            None => Poll::Ready(Err(io::Error::other(
                "streaming cache download failed or was abandoned",
            ))),
        }
    }
}

/// A job entry for a download run outside the worker.
fn adopt(m: &Manager, req: ExportRequest) -> Arc<WorkItem> {
    let id = new_id();
    let cancel = CancellationToken::new();
    let mut job = crate::job::tests::job();
    job.id.clone_from(&id);
    m.inner.lock().insert(
        id.clone(),
        Entry {
            job,
            request: req.clone(),
            cancel: cancel.clone(),
        },
    );
    Arc::new(WorkItem { id, req, cancel })
}

async fn run_download(m: &Manager, req: ExportRequest) -> (ExportJob, Result<Vec<u8>, String>) {
    let item = adopt(m, req);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out").join("track.m4a");
    let res = m
        .inner
        .download(&item, "us", "en-US", &dest.to_string_lossy(), 0)
        .await
        .map_err(|e| e.to_string());
    let bytes = match res {
        Ok(tmp) => Ok(tokio::fs::read(tmp).await.unwrap()),
        Err(e) => Err(e),
    };
    (m.get(&item.id).unwrap(), bytes)
}

fn audio(asset: &str, lossless: bool) -> ExportRequest {
    ExportRequest {
        asset_id: asset.into(),
        capabilities: ExportCapabilities {
            lossless,
            ..ExportCapabilities::default()
        },
        ..ExportRequest::default()
    }
}

fn with_cache(cache: FakeCache, provider: Arc<FakeProvider>) -> (Manager, Arc<FakeCache>) {
    let cache = Arc::new(cache);
    let (m, _) = manager_with(
        provider,
        Options {
            cache: Some(Arc::clone(&cache) as Arc<dyn AudioCache>),
            ..Options::default()
        },
    );
    (m, cache)
}

#[tokio::test]
async fn cache_hit_skips_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let cached = dir.path().join("cached.m4a");
    std::fs::write(&cached, "CACHED-BYTES").unwrap();
    let provider = Arc::new(FakeProvider::default());
    let (m, cache) = with_cache(
        FakeCache {
            path: Some(cached),
            ..FakeCache::default()
        },
        Arc::clone(&provider),
    );
    let (job, got) = run_download(&m, audio("a", true)).await;
    assert_eq!(got.unwrap(), b"CACHED-BYTES");
    assert_eq!(provider.opens(), 0, "opened the network on a cache hit");
    assert_eq!(job.source, Some(Source::Cache));
    assert_eq!(*cache.qualifier.lock().unwrap(), "alac");
}

#[tokio::test]
async fn tail_hit_skips_the_network() {
    let provider = Arc::new(FakeProvider::default());
    let (m, _) = with_cache(
        FakeCache {
            tail: Some(Box::new(|| Box::pin(&b"PLAYBACK-TAIL-BYTES"[..]))),
            ..FakeCache::default()
        },
        Arc::clone(&provider),
    );
    let (job, got) = run_download(&m, audio("a", false)).await;
    assert_eq!(got.unwrap(), b"PLAYBACK-TAIL-BYTES");
    assert_eq!(provider.opens(), 0);
    assert_eq!(job.source, Some(Source::Playback));
}

// A failed tail must not leave its partial bytes in front of the network copy.
#[tokio::test]
async fn tail_failure_falls_back_cleanly() {
    let provider = Arc::new(FakeProvider::default());
    let (m, _) = with_cache(
        FakeCache {
            tail: Some(Box::new(|| {
                Box::pin(FailingTail(Some(b"PARTIAL-".to_vec())))
            })),
            ..FakeCache::default()
        },
        Arc::clone(&provider),
    );
    let (job, got) = run_download(&m, audio("a", false)).await;
    assert_eq!(got.unwrap(), NETWORK, "want exactly the network payload");
    assert_eq!(provider.opens(), 1);
    assert_eq!(job.source, Some(Source::Network));
    assert_eq!(
        job.bytes_done,
        NETWORK.len() as u64,
        "progress must reset after the failed tail"
    );
}

#[tokio::test]
async fn no_cache_uses_the_network() {
    let (job, got) = run_download(&manager(), audio("a", false)).await;
    assert_eq!(got.unwrap(), NETWORK);
    assert_eq!(job.source, Some(Source::Network));
}

#[tokio::test]
async fn video_never_consults_the_audio_cache() {
    let provider = Arc::new(FakeProvider {
        video: true,
        ..FakeProvider::default()
    });
    let (m, cache) = with_cache(
        FakeCache {
            path: Some("/nonexistent-but-reported".into()),
            ..FakeCache::default()
        },
        Arc::clone(&provider),
    );
    let mut req = audio("a", false);
    req.capabilities.video = true;
    req.options.ffmpeg_path = "/nonexistent/ffmpeg".into();
    // The mux needs FFmpeg and fails here; only the source choice matters.
    let (_, got) = run_download(&m, req).await;
    assert!(got.unwrap_err().contains("ffmpeg"));
    assert_eq!(cache.calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.opens(), 1);
}

// A job cancelled while waiting on a stalled tail returns promptly and leaves no temp file.
#[tokio::test]
async fn cancel_while_the_tail_is_blocked() {
    let provider = Arc::new(FakeProvider::default());
    let (stalled, _keep) = tokio::io::duplex(64); // never written
    let stalled = Mutex::new(Some(stalled));
    let (m, _) = with_cache(
        FakeCache {
            tail: Some(Box::new(move || {
                Box::pin(stalled.lock().unwrap().take().unwrap())
            })),
            ..FakeCache::default()
        },
        Arc::clone(&provider),
    );
    let item = adopt(&m, audio("a", false));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("track.m4a").to_string_lossy().into_owned();
    let cancel = item.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    let res = tokio::time::timeout(
        Duration::from_secs(2),
        m.inner.download(&item, "us", "en-US", &dest, 0),
    )
    .await
    .expect("download did not return after cancellation");
    assert!(matches!(res, Err(JobError::Cancelled)));
    assert_eq!(
        provider.opens(),
        0,
        "the cancelled job fell through to the network"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "leftover files"
    );
}

// ── Integration: real playback manager and disk cache ────────────────────────

/// Runs a foreground playback stream of `asset` into a streaming disk-cache entry, as the
/// engine's playback handler does. Returns the session ID and the stream's result.
async fn start_playback(
    pm: &Arc<rustet_playback::Manager>,
    dc: &rustet_diskcache::Cache,
    asset: &str,
) -> (String, tokio::task::JoinHandle<Result<(), String>>) {
    let req = OpenRequest {
        asset_id: asset.into(),
        storefront: "us".into(),
        ..OpenRequest::default()
    };
    let sess = pm.open(&CancellationToken::new(), req).await.unwrap();
    let codec = sess.codec.unwrap().as_str();
    let mut put = dc.begin_streaming_put(asset, codec).await.unwrap().unwrap();
    let (pm, id) = (Arc::clone(pm), sess.id.clone());
    let done = tokio::spawn(async move {
        let (mut w, mut r) = tokio::io::duplex(64 << 10);
        let ctx = StreamContext::new(CancellationToken::new());
        let stream = async {
            let res = pm
                .stream(
                    &ctx,
                    StreamClass::Foreground,
                    &id,
                    StreamKind::Audio,
                    &mut w,
                )
                .await;
            drop(w);
            res.map_err(|e| e.to_string())
        };
        let copy = async {
            let mut buf = vec![0; 64 << 10];
            loop {
                let n = r.read(&mut buf).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    return Ok::<_, String>(());
                }
                put.write_all(&buf[..n]).await.map_err(|e| e.to_string())?;
            }
        };
        let (s, c) = tokio::join!(stream, copy);
        s.and(c)?;
        put.commit().await.map_err(|e| e.to_string())
    });
    (sess.id.clone(), done)
}

// Exporting the track that is playing rides playback's tail, playback keeps going, and its
// session survives the export.
#[tokio::test]
async fn export_of_the_playing_track_rides_the_playback_tail() {
    let head = vec![0xA1; 1 << 20];
    let tail = vec![0xB2; 64 << 10];
    let gate = CancellationToken::new();
    let provider = Arc::new(FakeProvider::default());
    provider
        .sources
        .lock()
        .unwrap()
        .insert("A".into(), payload(&head, &tail, Some(gate.clone())));
    let cache_dir = tempfile::tempdir().unwrap();
    let dc = rustet_diskcache::Cache::new(cache_dir.path()).unwrap();
    let pm = rustet_playback::Manager::new(Arc::clone(&provider) as Arc<dyn Provider>);
    let m = Manager::new(
        Arc::clone(&pm),
        reqwest::Client::new(),
        Options {
            cache: Some(Arc::new(dc.clone())),
            ..Options::default()
        },
    );

    let (play_id, play_done) = start_playback(&pm, &dc, "A").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while dc
            .get_streaming("A", "aac")
            .is_none_or(|e| e.written() < head.len() as u64)
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("playback did not write the head");
    assert!(pm.is_streaming());

    let item = adopt(&m, audio("A", false));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("A.m4a").to_string_lossy().into_owned();
    let inner = Arc::clone(&m.inner);
    let it = Arc::clone(&item);
    let export = tokio::spawn(async move {
        inner
            .download(&it, "us", "en-US", &dest, 0)
            .await
            .map_err(|e| e.to_string())
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        while m
            .get(&item.id)
            .is_none_or(|j| j.source != Some(Source::Playback) || j.bytes_done < head.len() as u64)
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("export did not attach to the playback tail");
    assert_eq!(
        provider.opens(),
        1,
        "the export opened its own session on a tail hit"
    );

    gate.cancel(); // playback continues and finishes
    play_done.await.unwrap().unwrap();
    let tmp = export.await.unwrap().unwrap();
    let got = std::fs::read(tmp).unwrap();
    assert_eq!(got.len(), head.len() + tail.len());
    assert!(
        got[..head.len()] == head[..] && got[head.len()..] == tail[..],
        "export bytes differ from playback bytes"
    );
    assert!(
        pm.session(&play_id).is_some(),
        "the playback session was removed"
    );
    assert!(!pm.is_streaming());
}

// The export's network stream is throttled while playback streams, from the first byte.
#[tokio::test]
async fn network_export_is_throttled_during_playback() {
    let gate = CancellationToken::new();
    let provider = Arc::new(FakeProvider::default());
    {
        let mut sources = provider.sources.lock().unwrap();
        sources.insert("A".into(), payload(b"A", b"A", Some(gate.clone())));
        // Larger than the limiter burst, so the 1 MiB/s start rate has to pace it.
        sources.insert("B".into(), payload(&[1; 384 << 10], b"", None));
    }
    let cache_dir = tempfile::tempdir().unwrap();
    let dc = rustet_diskcache::Cache::new(cache_dir.path()).unwrap();
    let (m, pm) = manager_with(Arc::clone(&provider), Options::default());
    let (_, play_done) = start_playback(&pm, &dc, "A").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pm.is_streaming() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();

    // A job reusing playback bytes is never throttled, even while the controller holds a limit.
    let reuse = adopt(&m, audio("C", false));
    m.inner.update(&reuse.id, |j| {
        j.phase = Phase::Downloading;
        j.source = Some(Source::Playback);
    });

    let start = std::time::Instant::now();
    let item = adopt(&m, audio("B", false));
    m.inner.update(&item.id, |j| j.phase = Phase::Downloading);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("B.m4a").to_string_lossy().into_owned();
    let inner = Arc::clone(&m.inner);
    let it = Arc::clone(&item);
    let export = tokio::spawn(async move {
        inner
            .download(&it, "us", "en-US", &dest, 0)
            .await
            .map_err(|e| e.to_string())
    });
    let snap = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let j = m.get(&item.id).unwrap();
            if j.throttled {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the snapshot never reported a limit during playback");
    assert_eq!(snap.limit_bps, Some(1 << 20), "start rate");
    assert_eq!(snap.source, Some(Source::Network));
    assert!(
        m.get(&reuse.id).unwrap().limit_bps.is_none(),
        "a reuse job reported a limit"
    );
    let tmp = export.await.unwrap().unwrap();
    assert_eq!(std::fs::read(tmp).unwrap().len(), 384 << 10);
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "not throttled ({:?})",
        start.elapsed()
    );
    assert_eq!(
        m.inner.bw.current_limit(),
        None,
        "controller still running after the export"
    );
    assert_eq!(
        pm.foreground_stats().active,
        1,
        "only playback is foreground"
    );
    gate.cancel();
    play_done.await.unwrap().unwrap();
}
