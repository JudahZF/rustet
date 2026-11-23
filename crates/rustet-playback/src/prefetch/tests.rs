use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use rustet_core::media::{self, Metadata, OpenTrack, Provider, SessionKind, Track};
use rustet_core::pipeline::{
    BoxError, Codec, PipelineError, Sink, Source, Stream, StreamContext, StreamKind,
};
use serde_json::json;

use super::*;

// ── Fakes ───────────────────────────────────────────────────────────────────

struct Empty;

#[async_trait]
impl Source for Empty {
    async fn stream(&self, _: &StreamContext, _: &mut dyn Sink) -> Result<(), PipelineError> {
        Ok(())
    }
}

struct Ready;

#[async_trait]
impl OpenTrack for Ready {
    async fn open(&self, _: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(Stream {
            source: Arc::new(Empty),
            stages: Vec::new(),
            kind: StreamKind::Audio,
            codec: Codec::Aac,
            duration_ms: 0,
            codec_string: None,
        })
    }
}

fn song() -> media::Session {
    media::Session {
        kind: SessionKind::Song,
        metadata: Metadata::default(),
        tracks: vec![Track {
            kind: StreamKind::Audio,
            codec: Codec::Aac,
            codec_string: String::new(),
            sample_rate: 0,
            bit_depth: 0,
            bit_rate: 0,
            channel_count: 0,
            codec_mime_type: String::new(),
            spatial_audio: None,
            open: Arc::new(Ready),
        }],
        video_heights: Vec::new(),
        mv_progressive_url: None,
        mv_download_key: None,
    }
}

/// How the fake provider answers.
enum Behaviour {
    Succeed,
    /// Hold every open until its token is cancelled, like a stalled upstream.
    Block,
    Fail(&'static str),
}

struct Fake {
    behaviour: Behaviour,
    opens: AtomicUsize,
    current: AtomicUsize,
    peak: AtomicUsize,
    lossless: Mutex<Vec<bool>>,
}

impl Fake {
    fn new(behaviour: Behaviour) -> Arc<Self> {
        Arc::new(Self {
            behaviour,
            opens: AtomicUsize::new(0),
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            lossless: Mutex::default(),
        })
    }
}

#[async_trait]
impl Provider for Fake {
    async fn open(
        &self,
        cancel: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<media::Session, PipelineError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.lossless.lock().unwrap().push(req.lossless);
        match self.behaviour {
            Behaviour::Succeed => Ok(song()),
            Behaviour::Fail(msg) => Err(PipelineError::Other(BoxError::from(msg))),
            Behaviour::Block => {
                let n = self.current.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                // A cancelled open may also just be dropped: count it finished either way.
                let _running = Running(&self.current);
                cancel.cancelled().await;
                Err(PipelineError::Cancelled)
            }
        }
    }
}

struct Running<'a>(&'a AtomicUsize);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

type Events = Arc<Mutex<Vec<Event>>>;

fn scheduler(p: Arc<Fake>, workers: usize) -> (Scheduler, Arc<Manager>, Events) {
    let pm = Manager::new(p);
    let events = Events::default();
    let sink = {
        let events = Arc::clone(&events);
        move |ev| events.lock().unwrap().push(ev)
    };
    let s = Scheduler::start(Arc::clone(&pm), || "t".into(), || "m".into(), sink, workers);
    (s, pm, events)
}

fn album(assets: &[&str]) -> ContextPayload {
    let mut p = ContextPayload {
        tracks: assets
            .iter()
            .map(|a| TrackItem {
                asset_id: (*a).into(),
                storefront: "us".into(),
                ..TrackItem::default()
            })
            .collect(),
        ..ContextPayload::default()
    };
    p.context.reason = "album-open".into();
    p
}

async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    // Longer than the retry back-off, for the tests that run on paused time.
    tokio::time::timeout(Duration::from_secs(30), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn insert(s: &Scheduler, asset: &str, session_id: &str, lossless: bool, expires_in: i64) {
    let now = Instant::now();
    let expires_at = if expires_in >= 0 {
        now + Duration::from_secs(expires_in.unsigned_abs())
    } else {
        now.checked_sub(Duration::from_secs(expires_in.unsigned_abs()))
            .unwrap_or(now)
    };
    s.inner.lock().pre_warmed.insert(
        asset.into(),
        PreWarmed {
            session_id: session_id.into(),
            expires_at,
            req: WarmRequest {
                asset_id: asset.into(),
                storefront: "us".into(),
                lossless,
            },
        },
    );
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

/// Stop must end running jobs (their sessions open under the scheduler's token) and the
/// workers, and be safe to repeat.
#[tokio::test]
async fn stop_cancels_running_jobs_and_workers() {
    let p = Fake::new(Behaviour::Block);
    let (s, _pm, _) = scheduler(Arc::clone(&p), 2);
    s.submit(&album(&["A", "B"]));
    eventually("a job to reach the provider", || {
        p.current.load(Ordering::SeqCst) > 0
    })
    .await;

    tokio::time::timeout(Duration::from_secs(5), s.stop())
        .await
        .expect("stop did not return: a job ignored cancellation");
    assert_eq!(
        p.current.load(Ordering::SeqCst),
        0,
        "provider calls still running"
    );
    assert!(s.inner.tasks.is_empty());
    s.stop().await; // idempotent
}

/// A burst of expired or mismatched claims must not become a burst of re-opens, and a stalled
/// upstream must not pin a re-warm.
#[tokio::test]
async fn rewarm_is_bounded_and_cancellable() {
    let p = Fake::new(Behaviour::Block);
    let (s, _pm, _) = scheduler(Arc::clone(&p), 1);
    for _ in 0..20 {
        Inner::spawn_rewarm(
            &s.inner,
            WarmRequest {
                asset_id: "X".into(),
                storefront: "us".into(),
                lossless: false,
            },
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let peak = p.peak.load(Ordering::SeqCst);
    assert!(
        peak <= MAX_CONCURRENT_REWARMS,
        "{peak} re-warms ran at once"
    );
    tokio::time::timeout(Duration::from_secs(5), s.stop())
        .await
        .expect("stop left re-warms blocked on a stalled provider");
    assert_eq!(p.current.load(Ordering::SeqCst), 0);
}

// ── Claiming pre-warmed sessions ────────────────────────────────────────────

#[tokio::test]
async fn take_pre_warmed_consumes_a_valid_entry() {
    let (s, _pm, _) = scheduler(Fake::new(Behaviour::Succeed), 1);
    insert(&s, "a", "sess-2", false, 3600);
    assert_eq!(s.take_pre_warmed("a", false).as_deref(), Some("sess-2"));
    assert_eq!(
        s.take_pre_warmed("a", false),
        None,
        "must be consumed on first take"
    );
    assert_eq!(s.take_pre_warmed("missing", false), None);
}

#[tokio::test]
async fn take_pre_warmed_rejects_an_expired_entry() {
    let (s, _pm, _) = scheduler(Fake::new(Behaviour::Succeed), 1);
    tokio::time::sleep(Duration::from_millis(5)).await; // so a past instant exists
    insert(&s, "a", "sess-1", false, -1);
    assert_eq!(s.take_pre_warmed("a", false), None);
}

/// A session warmed at one quality tier must never serve the other: it would play the wrong
/// codec. The mismatched entry is consumed and re-warmed at the requested tier.
#[tokio::test]
async fn take_pre_warmed_rejects_a_quality_mismatch() {
    let p = Fake::new(Behaviour::Succeed);
    let (s, _pm, _) = scheduler(Arc::clone(&p), 1);
    insert(&s, "a", "sess-3", false, 3600);
    assert_eq!(s.take_pre_warmed("a", true), None);
    eventually("the re-warm", || s.prewarm_count() == 1).await;
    assert_eq!(*p.lossless.lock().unwrap(), [true]);
    assert!(
        s.take_pre_warmed("a", true).is_some(),
        "re-warmed at the wrong tier"
    );

    insert(&s, "b", "sess-4", true, 3600);
    assert_eq!(s.take_pre_warmed("b", true).as_deref(), Some("sess-4"));
}

#[tokio::test]
async fn rewarm_keeps_the_lossless_tier() {
    let (s, _pm, _) = scheduler(Fake::new(Behaviour::Succeed), 1);
    s.inner
        .rewarm(WarmRequest {
            asset_id: "L".into(),
            storefront: "us".into(),
            lossless: true,
        })
        .await;
    assert!(
        s.take_pre_warmed("L", true).is_some(),
        "re-warmed lossless session was stored as AAC and rejected"
    );
}

// ── Warming ─────────────────────────────────────────────────────────────────

/// Prefetch of a track that playback already opened must not capture (and later release)
/// playback's session.
#[tokio::test]
async fn prefetch_never_releases_the_playback_session() {
    let (s, pm, events) = scheduler(Fake::new(Behaviour::Succeed), 1);
    let playing = pm
        .open(
            &CancellationToken::new(),
            OpenRequest {
                asset_id: "Y".into(),
                storefront: "us".into(),
                ..OpenRequest::default()
            },
        )
        .await
        .unwrap();
    let job = s.submit(&album(&["Y"]));
    eventually("the pre-warm", || s.prewarm_count() == 1).await;
    let warmed = s.inner.lock().pre_warmed["Y"].session_id.clone();
    assert_ne!(warmed, playing.id, "prefetch captured the playback session");

    s.inner.lock().pre_warmed.get_mut("Y").unwrap().expires_at = Instant::now();
    tokio::time::sleep(Duration::from_millis(2)).await;
    s.inner.prune_expired_pre_warmed();
    assert_eq!(s.prewarm_count(), 0);
    assert!(
        pm.session(&warmed).is_none(),
        "the pruned session was not released"
    );
    assert!(
        pm.session(&playing.id).is_some(),
        "pruning deleted the session the user is playing"
    );

    eventually("the done event", || events.lock().unwrap().len() == 3).await;
    let kinds: Vec<_> = events.lock().unwrap().iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::JobStarted,
            EventKind::TrackCached,
            EventKind::JobDone
        ]
    );
    assert!(s.status(&job).is_none(), "a completed job must be pruned");
    assert_eq!(s.stats().total_cached, 1);
}

/// Permanent failures are not retried.
#[tokio::test]
async fn auth_failure_is_not_retried() {
    let (s, _pm, events) = scheduler(
        Fake::new(Behaviour::Fail("webplayback 1440340123: HTTP 401")),
        1,
    );
    s.submit(&album(&["A"]));
    eventually("the done event", || events.lock().unwrap().len() == 3).await;
    let failed = events.lock().unwrap()[1].clone();
    assert_eq!(failed.kind, EventKind::TrackFailed);
    assert_eq!(failed.reason, Some(FailReason::Auth));
    assert_eq!((failed.failed, failed.total), (1, 1));
    assert_eq!(s.stats().total_retries, 0);
}

#[tokio::test(start_paused = true)]
async fn transient_failure_is_retried_then_reported() {
    let p = Fake::new(Behaviour::Fail("connection reset by peer"));
    let (s, _pm, events) = scheduler(Arc::clone(&p), 1);
    s.submit(&album(&["A"]));
    eventually("the done event", || events.lock().unwrap().len() == 3).await;
    assert_eq!(p.opens.load(Ordering::SeqCst), 1 + MAX_RETRIES as usize);
    assert_eq!(s.stats().total_retries, u64::from(MAX_RETRIES));
    assert_eq!(events.lock().unwrap()[1].reason, Some(FailReason::Network));
}

#[tokio::test]
async fn an_empty_selection_is_never_registered() {
    let (s, _pm, events) = scheduler(Fake::new(Behaviour::Succeed), 1);
    let id = s.submit(&album(&[]));
    assert!(s.status(&id).is_none());
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancel_reports_and_unknown_job_is_false() {
    let p = Fake::new(Behaviour::Block);
    let (s, _pm, events) = scheduler(Arc::clone(&p), 1);
    let id = s.submit(&album(&["A", "B"]));
    eventually("a job to reach the provider", || {
        p.current.load(Ordering::SeqCst) > 0
    })
    .await;
    assert_eq!(s.status(&id).unwrap().status, Phase::Running);
    assert!(s.cancel(&id));
    eventually("the job to finish", || s.status(&id).is_none()).await;
    let kinds: Vec<_> = events.lock().unwrap().iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::JobStarted,
            EventKind::JobCancelled,
            EventKind::JobDone
        ]
    );
    assert_eq!(s.stats().total_cancelled, 2);
    assert!(!s.cancel("nope"));
}

/// With no worker draining the queue (as when workers yield to playback), the queue stays
/// bounded and every item is accounted for: queued, or counted cancelled.
#[tokio::test]
async fn queue_depth_bounded_and_jobs_accounted() {
    let (s, _pm, _) = scheduler(Fake::new(Behaviour::Succeed), 0);
    let mut p = album(&[]);
    p.tracks = (0..25)
        .map(|i| TrackItem {
            asset_id: format!("t{i}"),
            signals: TrackSignals {
                play_count: i,
                ..TrackSignals::default()
            },
            ..TrackItem::default()
        })
        .collect();
    let ids: Vec<String> = (0..40).map(|_| s.submit(&p)).collect(); // 1000 items
    assert_eq!(s.stats().queue_depth, MAX_QUEUE_DEPTH);

    let live: Vec<WarmJob> = ids.iter().filter_map(|id| s.status(id)).collect();
    s.inner.queue.close();
    let mut queued: HashMap<String, usize> = HashMap::new();
    while let Some(item) = s.inner.queue.pop().await {
        *queued.entry(item.job.id.clone()).or_default() += 1;
    }
    assert_eq!(queued.values().sum::<usize>(), MAX_QUEUE_DEPTH);
    for job in live {
        let q = queued.get(&job.id).copied().unwrap_or(0);
        assert_eq!(job.cancelled + q, job.total, "job {}", job.id);
    }
    // Fully evicted jobs are complete and pruned.
    for id in queued.keys() {
        assert!(s.status(id).is_some());
    }
}

// ── Wire formats ────────────────────────────────────────────────────────────

#[test]
fn wire_formats() {
    let ev = Event {
        asset_id: "1".into(),
        cached: 2,
        total: 5,
        reason: Some(FailReason::NotFound),
        ..Event {
            kind: EventKind::TrackFailed,
            job_id: "j".into(),
            generation: 3,
            asset_id: String::new(),
            total: 0,
            cached: 0,
            failed: 0,
            reason: None,
        }
    };
    assert_eq!(
        serde_json::to_value(&ev).unwrap(),
        json!({"kind": "prefetch.failed", "jobId": "j", "generation": 3, "assetId": "1",
               "total": 5, "cached": 2, "reason": "NOT_FOUND"})
    );
    assert_eq!(EventKind::JobCancelled.as_str(), "prefetch.cancelled");

    let job = WarmJob {
        id: "j".into(),
        generation: 1,
        status: Phase::Running,
        total: 3,
        warming: 1,
        cached: 1,
        failed: 0,
        cancelled: 0,
        created_at: OffsetDateTime::UNIX_EPOCH,
    };
    assert_eq!(
        serde_json::to_value(&job).unwrap(),
        json!({"jobId": "j", "generation": 1, "status": "running", "total": 3, "warming": 1,
               "cached": 1, "failed": 0, "cancelled": 0, "createdAt": "1970-01-01T00:00:00Z"})
    );

    let stats = serde_json::to_value(Stats::default()).unwrap();
    let mut keys: Vec<&str> = stats
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "activeWorkers",
            "avgQueueWaitMs",
            "avgWarmMs",
            "cacheHitRatio",
            "dedupHits",
            "p95QueueWaitMs",
            "p95WarmMs",
            "playbackYields",
            "queueDepth",
            "totalCached",
            "totalCancelled",
            "totalFailed",
            "totalRetries"
        ]
    );

    let payload: ContextPayload = serde_json::from_value(json!({
        "context": {"type": "album", "id": "x", "reason": "queue-change"},
        "currentIndex": 1,
        "lossless": true,
        "tracks": [{"assetId": "9", "storefront": "gb",
                    "metadata": {"albumTrackIndex": 2},
                    "signals": {"favorite": true, "queueDistance": -1, "applePopularity": 0.5}}]
    }))
    .unwrap();
    assert_eq!(payload.context.kind, "album");
    assert!(payload.lossless && payload.tracks[0].signals.favorite);
    assert_eq!(payload.tracks[0].signals.queue_distance, -1);
}

#[test]
fn cache_config_wire_and_validation() {
    let cfg: CacheConfig =
        serde_json::from_value(json!({"prewarmLimitMB": 10, "persistTTLDays": 7})).unwrap();
    assert_eq!(
        cfg,
        CacheConfig {
            prewarm_limit_mb: 10,
            persist_limit_mb: 0,
            persist_ttl_days: 7
        }
    );
    assert_eq!(
        serde_json::to_value(cfg).unwrap(),
        json!({"prewarmLimitMB": 10, "persistTTLDays": 7})
    );
    assert_eq!(cfg.validate(), Ok(()));
    for (bad, want) in [
        (
            CacheConfig {
                prewarm_limit_mb: -1,
                ..cfg
            },
            CacheConfigError::PrewarmLimit,
        ),
        (
            CacheConfig {
                persist_limit_mb: MAX_CACHE_LIMIT_MB + 1,
                ..cfg
            },
            CacheConfigError::PersistLimit,
        ),
        (
            CacheConfig {
                persist_ttl_days: MAX_CACHE_TTL_DAYS + 1,
                ..cfg
            },
            CacheConfigError::PersistTtl,
        ),
    ] {
        assert_eq!(bad.validate(), Err(want));
    }
    assert_eq!(MAX_CACHE_TTL_DAYS, 106_751);
}

#[tokio::test]
async fn set_cache_config_ignores_out_of_range_fields() {
    let (s, _pm, _) = scheduler(Fake::new(Behaviour::Succeed), 1);
    s.set_cache_config(CacheConfig {
        prewarm_limit_mb: 5,
        persist_limit_mb: -3,
        persist_ttl_days: 2,
    });
    assert_eq!(
        s.cache_config(),
        CacheConfig {
            prewarm_limit_mb: 5,
            persist_limit_mb: 0,
            persist_ttl_days: 2
        }
    );
}
