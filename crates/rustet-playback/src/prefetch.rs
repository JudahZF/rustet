//! Background pre-warming of upcoming tracks.
//!
//! The renderer sends the context the user is looking at (`PUT /api/v1/playback/context`,
//! a [`ContextPayload`]). The [`Scheduler`] picks tracks from it and its workers open a
//! private playback session for each. Real playback then claims the session with
//! [`Scheduler::take_pre_warmed`] and skips the provider round-trip (~1–3 s).
//!
//! Streaming a track ahead of time does not help: Apple rotates the access tokens in signed
//! segment URLs between webplayback calls, so pre-fetched segments would never be cache hits.
//! Holding the opened session is what saves time.
//!
//! The renderer sends context and UI signals; all scheduling policy (which tracks, order,
//! concurrency, retries) lives here. Progress is reported as [`Event`]s for SSE.

mod queue;
mod select;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rustet_core::media::OpenRequest;
use rustet_core::ring;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::{OnceCell, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::warn;

use crate::manager::{Manager, OpenError};
use crate::session::Session;
use queue::WorkQueue;
use select::{classify_error, score_track, select_tracks};

/// Default number of concurrent warming workers.
pub const DEFAULT_WORKERS: usize = 3;

/// Additional attempts after the first failure.
const MAX_RETRIES: u32 = 3;

/// Bound on queued warm items. Workers yield to playback for up to [`PLAYBACK_YIELD_CAP`] per
/// item, so without a bound every context update (up to 25 items) would pile up while the
/// user listens.
const MAX_QUEUE_DEPTH: usize = 512;

/// How long an unclaimed pre-warmed session is kept.
const PRE_WARM_TTL: Duration = Duration::from_secs(15 * 60);

/// Cap on background re-opens of expired or mismatched entries. Re-warming is an optimisation;
/// a burst of claims must not become a burst of network calls.
const MAX_CONCURRENT_REWARMS: usize = 4;

/// Bound on one background re-open, so a stalled upstream cannot pin a task.
const REWARM_TIMEOUT: Duration = Duration::from_secs(90);

/// How often unclaimed, expired pre-warmed sessions are released.
const PRUNE_EVERY: Duration = Duration::from_secs(5 * 60);

/// How long [`Scheduler::stop`] waits for the workers.
const STOP_WAIT: Duration = Duration::from_secs(10);

// Yielding to real playback.
//
// Apple's Android client caches the next item only once the active period has stopped
// reading from the network (`PlayerLoadControl.shouldPrepareNextPeriodForCaching` ends in
// `return !period.isReadingFromNetwork()`). Without that, warming competed with the stream the
// user was listening to: music-video segments took 11–22 s and the player stalled with a 2 s
// buffer while a 10-track warm ran. The cap keeps a stream that never ends (a stuck or very
// long download) from starving warming forever.
const PLAYBACK_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PLAYBACK_YIELD_CAP: Duration = Duration::from_secs(2 * 60);

/// Retry delays after the first failure are 1 s, 2 s, 4 s.
fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(1 << (attempt - 1))
}

// ── Wire types ──────────────────────────────────────────────────────────────

/// Stable reason code of a [`EventKind::TrackFailed`] event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailReason {
    Network,
    Timeout,
    Auth,
    NotFound,
    RetryExhausted,
}

impl FailReason {
    /// Auth and not-found failures are permanent; retrying them wastes a worker.
    fn is_retryable(self) -> bool {
        !matches!(self, Self::Auth | Self::NotFound)
    }
}

/// The kind of a prefetch event. It is also the SSE event name, so clients can subscribe to
/// one kind directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EventKind {
    #[serde(rename = "prefetch.started")]
    JobStarted,
    #[serde(rename = "prefetch.cached")]
    TrackCached,
    #[serde(rename = "prefetch.failed")]
    TrackFailed,
    #[serde(rename = "prefetch.done")]
    JobDone,
    #[serde(rename = "prefetch.cancelled")]
    JobCancelled,
}

impl EventKind {
    /// The wire name, for example `prefetch.cached`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JobStarted => "prefetch.started",
            Self::TrackCached => "prefetch.cached",
            Self::TrackFailed => "prefetch.failed",
            Self::JobDone => "prefetch.done",
            Self::JobCancelled => "prefetch.cancelled",
        }
    }
}

/// A prefetch lifecycle event, forwarded to SSE clients.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub kind: EventKind,
    pub job_id: String,
    /// Increments with every submitted context, so clients can discard stale events.
    pub generation: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub asset_id: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub total: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub cached: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub failed: usize,
    /// Set on [`EventKind::TrackFailed`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<FailReason>,
}

impl Event {
    fn new(kind: EventKind, job: &Job) -> Self {
        Self {
            kind,
            job_id: job.id.clone(),
            generation: job.generation,
            asset_id: String::new(),
            total: 0,
            cached: 0,
            failed: 0,
            reason: None,
        }
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// UI-derived hints sent with each track.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackSignals {
    pub favorite: bool,
    pub apple_popularity: f64,
    pub play_count: i64,
    pub skip_count: i64,
    pub last_played: i64,
    pub queue_distance: i64,
    pub visible: bool,
    pub explicit_selection: bool,
    pub recent_interaction: bool,
}

/// Fixed facts about a track.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackMetadata {
    pub album_track_index: i64,
}

/// One track of a context.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackItem {
    pub asset_id: String,
    pub storefront: String,
    pub metadata: TrackMetadata,
    pub signals: TrackSignals,
}

/// What the user is looking at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PayloadContext {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    /// Why the context was sent (`queue-change`, `album-open`, …). Decides the selection.
    pub reason: String,
}

/// The body of `PUT /api/v1/playback/context`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextPayload {
    pub context: PayloadContext,
    pub current_index: i64,
    pub tracks: Vec<TrackItem>,
    /// The renderer has lossless DRM and prefers lossless.
    pub lossless: bool,
}

/// The lifecycle state of a warm job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Queued,
    Running,
    Done,
}

/// A snapshot of a warm job, as `GET /api/v1/jobs/{id}` returns it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmJob {
    #[serde(rename = "jobId")]
    pub id: String,
    pub generation: u64,
    pub status: Phase,
    pub total: usize,
    pub warming: usize,
    pub cached: usize,
    pub failed: usize,
    pub cancelled: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Scheduler metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub active_workers: u64,
    pub queue_depth: usize,
    pub dedup_hits: u64,
    /// Warm items deferred so as not to compete with playback.
    pub playback_yields: u64,
    pub total_cached: u64,
    pub total_failed: u64,
    pub total_cancelled: u64,
    /// Retry attempts, not first tries.
    pub total_retries: u64,
    /// Mean time from worker start to opened session, over the last 200 successes.
    pub avg_warm_ms: f64,
    pub p95_warm_ms: f64,
    /// Mean time from enqueue to worker start.
    pub avg_queue_wait_ms: f64,
    pub p95_queue_wait_ms: f64,
    /// cached / (cached + failed).
    pub cache_hit_ratio: f64,
}

/// User cache limits from `PUT /api/v1/cache/config`. Zero means unlimited or the engine
/// default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    #[serde(rename = "prewarmLimitMB", skip_serializing_if = "is_zero_i64")]
    pub prewarm_limit_mb: i64,
    #[serde(rename = "persistLimitMB", skip_serializing_if = "is_zero_i64")]
    pub persist_limit_mb: i64,
    #[serde(rename = "persistTTLDays", skip_serializing_if = "is_zero_i64")]
    pub persist_ttl_days: i64,
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

/// The largest limit whose conversion to bytes fits an `i64`.
pub const MAX_CACHE_LIMIT_MB: i64 = i64::MAX / (1024 * 1024);
/// The largest TTL whose conversion to nanoseconds fits an `i64`.
pub const MAX_CACHE_TTL_DAYS: i64 = i64::MAX / (24 * 60 * 60 * 1_000_000_000);

/// A [`CacheConfig`] field out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CacheConfigError {
    #[error("prewarmLimitMB must be between 0 and {MAX_CACHE_LIMIT_MB}")]
    PrewarmLimit,
    #[error("persistLimitMB must be between 0 and {MAX_CACHE_LIMIT_MB}")]
    PersistLimit,
    #[error("persistTTLDays must be between 0 and {MAX_CACHE_TTL_DAYS}")]
    PersistTtl,
}

impl CacheConfig {
    /// Rejects negative values and values whose unit conversion would overflow.
    pub fn validate(&self) -> Result<(), CacheConfigError> {
        let limit = 0..=MAX_CACHE_LIMIT_MB;
        if !limit.contains(&self.prewarm_limit_mb) {
            return Err(CacheConfigError::PrewarmLimit);
        }
        if !limit.contains(&self.persist_limit_mb) {
            return Err(CacheConfigError::PersistLimit);
        }
        if !(0..=MAX_CACHE_TTL_DAYS).contains(&self.persist_ttl_days) {
            return Err(CacheConfigError::PersistTtl);
        }
        Ok(())
    }
}

// ── Internal state ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    warming: usize,
    cached: usize,
    failed: usize,
    cancelled: usize,
}

impl Counts {
    fn settled(&self) -> usize {
        self.cached + self.failed + self.cancelled
    }
}

/// How one warm item ended.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Cached,
    Failed(FailReason),
    Cancelled,
}

/// One submitted context.
struct Job {
    id: String,
    generation: u64,
    created_at: OffsetDateTime,
    total: usize,
    /// Cancels the job's queued and running items. A child of the scheduler root.
    cancel: CancellationToken,
    counts: Mutex<Counts>,
}

impl Job {
    fn counts(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start_one(&self) {
        self.counts().warming += 1;
    }

    /// Records how one item ended (`started`: whether it reached a worker and counted as
    /// warming). Returns the counts after it and whether this item completed the job: each
    /// item settles exactly once, so only the last one sees the job complete.
    fn settle(&self, outcome: Outcome, started: bool) -> (Counts, bool) {
        let mut c = self.counts();
        match outcome {
            Outcome::Cached => c.cached += 1,
            Outcome::Failed(_) => c.failed += 1,
            Outcome::Cancelled => c.cancelled += 1,
        }
        if started {
            c.warming -= 1;
        }
        (*c, c.settled() >= self.total && c.warming == 0)
    }

    fn snapshot(&self) -> WarmJob {
        let c = *self.counts();
        let status = if c.settled() >= self.total && c.warming == 0 {
            Phase::Done
        } else if c.warming > 0 {
            Phase::Running
        } else {
            Phase::Queued
        };
        WarmJob {
            id: self.id.clone(),
            generation: self.generation,
            status,
            total: self.total,
            warming: c.warming,
            cached: c.cached,
            failed: c.failed,
            cancelled: c.cancelled,
            created_at: self.created_at,
        }
    }
}

/// One track of a job, waiting for a worker.
struct WorkItem {
    job: Arc<Job>,
    track: TrackItem,
    token: String,
    mut_token: String,
    lossless: bool,
    enqueued_at: Instant,
}

/// What is needed to open a pre-warmed session again (credentials are read fresh).
#[derive(Debug, Clone, PartialEq, Eq)]
struct WarmRequest {
    asset_id: String,
    storefront: String,
    lossless: bool,
}

impl WarmRequest {
    fn open_request(&self, token: String, mut_token: String) -> OpenRequest {
        OpenRequest {
            asset_id: self.asset_id.clone(),
            storefront: self.storefront.clone(),
            token,
            mut_token,
            lossless: self.lossless,
            ..OpenRequest::default()
        }
    }
}

/// A session opened by the prefetcher, waiting to be claimed by playback.
struct PreWarmed {
    session_id: String,
    expires_at: Instant,
    /// Also the quality tier of the session.
    req: WarmRequest,
}

#[derive(Default)]
struct State {
    /// Jobs not yet complete. Completed jobs are pruned so the map stays bounded.
    jobs: HashMap<String, Arc<Job>>,
    /// Assets being warmed by a worker now.
    warming: HashSet<String>,
    /// Asset ID → pre-opened session, consumed on first claim.
    pre_warmed: HashMap<String, PreWarmed>,
}

type Credential = Box<dyn Fn() -> String + Send + Sync>;
type EventSink = Box<dyn Fn(Event) + Send + Sync>;

struct Inner {
    pm: Arc<Manager>,
    token: Credential,
    mut_token: Credential,
    sink: EventSink,

    state: Mutex<State>,
    generation: AtomicU64,
    queue: WorkQueue<WorkItem>,

    /// Every job and re-warm runs under it, so stopping cancels them all.
    root: CancellationToken,
    /// Workers, re-warms and the prune ticker.
    tasks: TaskTracker,
    rewarm_slots: Semaphore,

    prewarm_limit_mb: AtomicI64,
    persist_limit_mb: AtomicI64,
    persist_ttl_days: AtomicI64,

    active_workers: AtomicU64,
    dedup_hits: AtomicU64,
    total_cached: AtomicU64,
    total_failed: AtomicU64,
    total_cancelled: AtomicU64,
    total_retries: AtomicU64,
    playback_yields: AtomicU64,
    /// Worker start to opened session, in ms.
    latencies: ring::Buffer,
    /// Enqueue to worker start, in ms.
    queue_waits: ring::Buffer,
}

/// The cache-warming scheduler: a bounded worker pool over a scored queue.
pub struct Scheduler {
    inner: Arc<Inner>,
    stopped: OnceCell<()>,
}

impl Scheduler {
    /// Starts a scheduler with `workers` workers (0 means [`DEFAULT_WORKERS`]). `token` and
    /// `mut_token` give the current bearer and Music-User-Token; they are read when a context
    /// is submitted, so token rotations are picked up. `sink` receives every [`Event`].
    ///
    /// Must be called inside a tokio runtime.
    pub fn new(
        pm: Arc<Manager>,
        token: impl Fn() -> String + Send + Sync + 'static,
        mut_token: impl Fn() -> String + Send + Sync + 'static,
        sink: impl Fn(Event) + Send + Sync + 'static,
        workers: usize,
    ) -> Self {
        let workers = if workers == 0 {
            DEFAULT_WORKERS
        } else {
            workers
        };
        Self::start(pm, token, mut_token, sink, workers)
    }

    /// [`Self::new`] with exactly `workers` workers; 0 leaves the queue undrained (tests).
    fn start(
        pm: Arc<Manager>,
        token: impl Fn() -> String + Send + Sync + 'static,
        mut_token: impl Fn() -> String + Send + Sync + 'static,
        sink: impl Fn(Event) + Send + Sync + 'static,
        workers: usize,
    ) -> Self {
        let inner = Arc::new(Inner {
            pm,
            token: Box::new(token),
            mut_token: Box::new(mut_token),
            sink: Box::new(sink),
            state: Mutex::default(),
            generation: AtomicU64::new(0),
            queue: WorkQueue::new(MAX_QUEUE_DEPTH),
            root: CancellationToken::new(),
            tasks: TaskTracker::new(),
            rewarm_slots: Semaphore::new(MAX_CONCURRENT_REWARMS),
            prewarm_limit_mb: AtomicI64::new(0),
            persist_limit_mb: AtomicI64::new(0),
            persist_ttl_days: AtomicI64::new(0),
            active_workers: AtomicU64::new(0),
            dedup_hits: AtomicU64::new(0),
            total_cached: AtomicU64::new(0),
            total_failed: AtomicU64::new(0),
            total_cancelled: AtomicU64::new(0),
            total_retries: AtomicU64::new(0),
            playback_yields: AtomicU64::new(0),
            latencies: ring::Buffer::new(200),
            queue_waits: ring::Buffer::new(200),
        });
        for _ in 0..workers {
            let inner = Arc::clone(&inner);
            inner.tasks.clone().spawn(async move {
                while let Some(item) = inner.queue.pop().await {
                    inner.await_playback_idle(&item.job.cancel).await;
                    inner.warm(item).await;
                }
            });
        }
        // Releases abandoned pre-warmed sessions.
        let pruner = Arc::clone(&inner);
        inner.tasks.spawn(async move {
            let mut tick = tokio::time::interval_at(Instant::now() + PRUNE_EVERY, PRUNE_EVERY);
            loop {
                tokio::select! {
                    () = pruner.root.cancelled() => return,
                    _ = tick.tick() => pruner.prune_expired_pre_warmed(),
                }
            }
        });
        Self {
            inner,
            stopped: OnceCell::new(),
        }
    }

    /// Cancels every queued and running job, stops the workers and releases the pre-warmed
    /// sessions. Returns when every task has ended, or after a bounded wait. Safe to call more
    /// than once.
    pub async fn stop(&self) {
        let inner = &self.inner;
        self.stopped
            .get_or_init(|| async {
                inner.root.cancel();
                inner.queue.close();
                inner.tasks.close();
                if tokio::time::timeout(STOP_WAIT, inner.tasks.wait())
                    .await
                    .is_err()
                {
                    warn!("prefetch stop: workers still running after {STOP_WAIT:?}");
                }
                inner.clear_pre_warmed();
            })
            .await;
    }

    /// Queues a context for warming and returns the job ID at once (202 Accepted semantics).
    pub fn submit(&self, payload: &ContextPayload) -> String {
        let inner = &self.inner;
        let tracks = select_tracks(payload);
        let generation = inner.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let id = crate::new_id();
        if tracks.is_empty() {
            // Nothing to warm: never registered, so it cannot linger in the job map.
            return id;
        }
        let job = Arc::new(Job {
            id: id.clone(),
            generation,
            created_at: OffsetDateTime::now_utc(),
            total: tracks.len(),
            cancel: inner.root.child_token(),
            counts: Mutex::default(),
        });
        inner.lock().jobs.insert(id.clone(), Arc::clone(&job));

        let (token, mut_token) = ((inner.token)(), (inner.mut_token)());
        (inner.sink)(Event {
            total: job.total,
            ..Event::new(EventKind::JobStarted, &job)
        });

        let now = Instant::now();
        for track in tracks {
            let score = score_track(&track);
            let item = WorkItem {
                job: Arc::clone(&job),
                track,
                token: token.clone(),
                mut_token: mut_token.clone(),
                lossless: payload.lossless,
                enqueued_at: now,
            };
            if let Some(evicted) = inner.queue.push(score, item) {
                inner.settle(&evicted, Outcome::Cancelled, false);
            }
        }
        id
    }

    /// A snapshot of a job that is not yet complete.
    pub fn status(&self, id: &str) -> Option<WarmJob> {
        let job = self.inner.lock().jobs.get(id).cloned()?;
        Some(job.snapshot())
    }

    /// Stops the remaining work of a job. Returns false for an unknown or completed job.
    pub fn cancel(&self, id: &str) -> bool {
        let Some(job) = self.inner.lock().jobs.get(id).cloned() else {
            return false;
        };
        job.cancel.cancel();
        (self.inner.sink)(Event::new(EventKind::JobCancelled, &job));
        true
    }

    /// Claims the pre-opened session for `asset_id` when its quality tier (lossless or AAC)
    /// matches, so playback can skip the webplayback round-trip. The entry is consumed either
    /// way. An expired or mismatched entry is released, and a background re-warm (at the
    /// requested tier) prepares a fresh session for the next request.
    ///
    /// Must be called inside a tokio runtime.
    pub fn take_pre_warmed(&self, asset_id: &str, lossless: bool) -> Option<String> {
        let inner = &self.inner;
        let entry = inner.lock().pre_warmed.remove(asset_id)?;
        if Instant::now() > entry.expires_at {
            inner.pm.release(&entry.session_id);
            Inner::spawn_rewarm(inner, entry.req);
            return None;
        }
        if entry.req.lossless != lossless {
            // Serving a lossless session to an AAC request (or the reverse) would play the
            // wrong codec.
            inner.pm.release(&entry.session_id);
            Inner::spawn_rewarm(
                inner,
                WarmRequest {
                    lossless,
                    ..entry.req
                },
            );
            return None;
        }
        Some(entry.session_id)
    }

    /// The number of pre-warmed sessions waiting to be claimed.
    pub fn prewarm_count(&self) -> usize {
        self.inner.lock().pre_warmed.len()
    }

    /// Releases every pre-warmed session.
    pub fn clear_pre_warmed(&self) {
        self.inner.clear_pre_warmed();
    }

    /// Applies user cache limits. A field out of range is ignored.
    pub fn set_cache_config(&self, cfg: CacheConfig) {
        let inner = &self.inner;
        let limit = 0..=MAX_CACHE_LIMIT_MB;
        if limit.contains(&cfg.prewarm_limit_mb) {
            inner
                .prewarm_limit_mb
                .store(cfg.prewarm_limit_mb, Ordering::Relaxed);
        }
        if limit.contains(&cfg.persist_limit_mb) {
            inner
                .persist_limit_mb
                .store(cfg.persist_limit_mb, Ordering::Relaxed);
        }
        if (0..=MAX_CACHE_TTL_DAYS).contains(&cfg.persist_ttl_days) {
            inner
                .persist_ttl_days
                .store(cfg.persist_ttl_days, Ordering::Relaxed);
        }
    }

    /// The cache limits in force.
    pub fn cache_config(&self) -> CacheConfig {
        let inner = &self.inner;
        CacheConfig {
            prewarm_limit_mb: inner.prewarm_limit_mb.load(Ordering::Relaxed),
            persist_limit_mb: inner.persist_limit_mb.load(Ordering::Relaxed),
            persist_ttl_days: inner.persist_ttl_days.load(Ordering::Relaxed),
        }
    }

    /// A snapshot of the scheduler metrics.
    pub fn stats(&self) -> Stats {
        let inner = &self.inner;
        let warm = inner.latencies.stats();
        let wait = inner.queue_waits.stats();
        let cached = inner.total_cached.load(Ordering::Relaxed);
        let failed = inner.total_failed.load(Ordering::Relaxed);
        Stats {
            active_workers: inner.active_workers.load(Ordering::Relaxed),
            queue_depth: inner.queue.len(),
            dedup_hits: inner.dedup_hits.load(Ordering::Relaxed),
            playback_yields: inner.playback_yields.load(Ordering::Relaxed),
            total_cached: cached,
            total_failed: failed,
            total_cancelled: inner.total_cancelled.load(Ordering::Relaxed),
            total_retries: inner.total_retries.load(Ordering::Relaxed),
            avg_warm_ms: warm.avg,
            p95_warm_ms: warm.p95,
            avg_queue_wait_ms: wait.avg,
            p95_queue_wait_ms: wait.p95,
            cache_hit_ratio: if cached + failed > 0 {
                cached as f64 / (cached + failed) as f64
            } else {
                0.0
            },
        }
    }
}

impl Drop for Scheduler {
    /// Without [`Scheduler::stop`], the tasks still end: they all run under the root token.
    fn drop(&mut self) {
        self.inner.root.cancel();
        self.inner.queue.close();
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // No critical section can leave the maps inconsistent, so a poisoned lock is safe.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits while real playback is pulling from the network, so warming never competes with
    /// it for bandwidth. Returns early when `cancel` fires or the cap elapses.
    async fn await_playback_idle(&self, cancel: &CancellationToken) {
        if !self.pm.is_streaming() {
            return;
        }
        self.playback_yields.fetch_add(1, Ordering::Relaxed);
        let cap = tokio::time::sleep(PLAYBACK_YIELD_CAP);
        tokio::pin!(cap);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = &mut cap => return, // proceed rather than starve
                () = tokio::time::sleep(PLAYBACK_POLL_INTERVAL) => {
                    if !self.pm.is_streaming() {
                        return;
                    }
                }
            }
        }
    }

    /// Opens a private session for one queued track, with retries, and stores it for playback
    /// to claim.
    async fn warm(&self, item: WorkItem) {
        let job = &item.job;
        if job.cancel.is_cancelled() {
            self.settle(&item, Outcome::Cancelled, false);
            return;
        }
        // Skip an asset another worker is already warming.
        let Some(_warming) = WarmingGuard::claim(self, &item.track.asset_id) else {
            self.dedup_hits.fetch_add(1, Ordering::Relaxed);
            self.settle(&item, Outcome::Cancelled, false);
            return;
        };

        self.queue_waits.record(millis(item.enqueued_at.elapsed()));
        job.start_one();
        self.active_workers.fetch_add(1, Ordering::Relaxed);
        let _busy = DecrementOnDrop(&self.active_workers);

        let req = WarmRequest {
            asset_id: item.track.asset_id.clone(),
            storefront: match item.track.storefront.as_str() {
                "" => "us".to_owned(),
                sf => sf.to_owned(),
            },
            lossless: item.lossless,
        };
        let started = Instant::now();
        let mut reason = FailReason::RetryExhausted;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                self.total_retries.fetch_add(1, Ordering::Relaxed);
                tokio::select! {
                    () = job.cancel.cancelled() => {}
                    () = tokio::time::sleep(retry_delay(attempt)) => {}
                }
            }
            if job.cancel.is_cancelled() {
                self.settle(&item, Outcome::Cancelled, true);
                return;
            }

            // Private: a shared open can return the very session playback is using, and the
            // scheduler later releases pre-warmed sessions (expiry, quality mismatch, cache
            // clear), which would delete it from under playback.
            let open_req = req.open_request(item.token.clone(), item.mut_token.clone());
            let session = match self.open_private(&job.cancel, None, open_req).await {
                Some(Ok(session)) => session,
                Some(Err(e)) if !job.cancel.is_cancelled() => {
                    reason = classify_error(&e.to_string());
                    if !reason.is_retryable() {
                        break;
                    }
                    continue;
                }
                Some(Err(_)) | None => {
                    self.settle(&item, Outcome::Cancelled, true);
                    return;
                }
            };

            self.store_pre_warmed(&session, req);
            // A cancellation that raced the open: release what was just opened.
            if job.cancel.is_cancelled() {
                {
                    let mut st = self.lock();
                    if st
                        .pre_warmed
                        .get(&item.track.asset_id)
                        .is_some_and(|cur| cur.session_id == session.id)
                    {
                        st.pre_warmed.remove(&item.track.asset_id);
                    }
                }
                self.pm.release(&session.id);
                self.settle(&item, Outcome::Cancelled, true);
                return;
            }
            self.latencies.record(millis(started.elapsed()));
            self.settle(&item, Outcome::Cached, true);
            return;
        }
        self.settle(&item, Outcome::Failed(reason), true);
    }

    /// Stores `session` as the pre-warmed session of its asset, releasing the one it
    /// supersedes (private, so nobody else holds it).
    fn store_pre_warmed(&self, session: &Session, req: WarmRequest) {
        let prev = self.lock().pre_warmed.insert(
            req.asset_id.clone(),
            PreWarmed {
                session_id: session.id.clone(),
                expires_at: Instant::now() + PRE_WARM_TTL,
                req,
            },
        );
        if let Some(prev) = prev
            && prev.session_id != session.id
        {
            self.pm.release(&prev.session_id);
        }
    }

    /// Opens a private session, abandoning it when `stop` fires or `limit` elapses (`None`).
    ///
    /// The session's own token is a child of the root, not of `stop`: the opened streams may
    /// keep using it, and a pre-warmed session must outlive the job that opened it.
    async fn open_private(
        &self,
        stop: &CancellationToken,
        limit: Option<Duration>,
        req: OpenRequest,
    ) -> Option<Result<Arc<Session>, OpenError>> {
        let token = self.root.child_token();
        let open = self.pm.open_private(&token, req);
        let bounded = async {
            match limit {
                Some(limit) => tokio::time::timeout(limit, open).await.ok(),
                None => Some(open.await),
            }
        };
        let res = tokio::select! {
            () = stop.cancelled() => None,
            r = bounded => r,
        };
        if res.is_none() {
            token.cancel();
        }
        res
    }

    fn spawn_rewarm(this: &Arc<Self>, req: WarmRequest) {
        let inner = Arc::clone(this);
        this.tasks.spawn(async move { inner.rewarm(req).await });
    }

    /// Opens a fresh session for an expired or mismatched pre-warmed entry and stores it,
    /// unless something else filled the slot meanwhile.
    async fn rewarm(&self, req: WarmRequest) {
        // Enough re-warms are running; the next claim will try again.
        let Ok(_slot) = self.rewarm_slots.try_acquire() else {
            return;
        };
        let open_req = req.open_request((self.token)(), (self.mut_token)());
        let Some(Ok(session)) = self
            .open_private(&self.root, Some(REWARM_TIMEOUT), open_req)
            .await
        else {
            return;
        };
        let mut st = self.lock();
        if st.pre_warmed.contains_key(&req.asset_id) {
            drop(st);
            self.pm.release(&session.id);
        } else {
            st.pre_warmed.insert(
                req.asset_id.clone(),
                PreWarmed {
                    session_id: session.id.clone(),
                    expires_at: Instant::now() + PRE_WARM_TTL,
                    req,
                },
            );
        }
    }

    /// Releases pre-warmed sessions that expired unclaimed.
    fn prune_expired_pre_warmed(&self) {
        let now = Instant::now();
        let expired: Vec<String> = {
            let mut st = self.lock();
            let mut ids = Vec::new();
            st.pre_warmed.retain(|_, e| {
                let keep = now <= e.expires_at;
                if !keep {
                    ids.push(e.session_id.clone());
                }
                keep
            });
            ids
        };
        for id in expired {
            self.pm.release(&id);
        }
    }

    fn clear_pre_warmed(&self) {
        let all = std::mem::take(&mut self.lock().pre_warmed);
        for entry in all.into_values() {
            self.pm.release(&entry.session_id);
        }
    }

    /// Records how an item ended, emits its event, and prunes its job when complete.
    fn settle(&self, item: &WorkItem, outcome: Outcome, started: bool) {
        let job = &item.job;
        let (counts, done) = job.settle(outcome, started);
        let total = match outcome {
            Outcome::Cached => &self.total_cached,
            Outcome::Failed(_) => &self.total_failed,
            Outcome::Cancelled => &self.total_cancelled,
        };
        total.fetch_add(1, Ordering::Relaxed);

        let track_event = |kind| Event {
            asset_id: item.track.asset_id.clone(),
            total: job.total,
            ..Event::new(kind, job)
        };
        match outcome {
            Outcome::Cached => (self.sink)(Event {
                cached: counts.cached,
                ..track_event(EventKind::TrackCached)
            }),
            Outcome::Failed(reason) => (self.sink)(Event {
                failed: counts.failed,
                reason: Some(reason),
                ..track_event(EventKind::TrackFailed)
            }),
            Outcome::Cancelled => {}
        }
        if done {
            (self.sink)(Event {
                total: job.total,
                cached: counts.cached,
                failed: counts.failed,
                ..Event::new(EventKind::JobDone, job)
            });
            self.lock().jobs.remove(&job.id);
        }
    }
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Marks an asset as being warmed until dropped.
struct WarmingGuard<'a> {
    inner: &'a Inner,
    asset_id: String,
}

impl<'a> WarmingGuard<'a> {
    /// `None` when another worker is already warming the asset.
    fn claim(inner: &'a Inner, asset_id: &str) -> Option<Self> {
        inner
            .lock()
            .warming
            .insert(asset_id.to_owned())
            .then(|| Self {
                inner,
                asset_id: asset_id.to_owned(),
            })
    }
}

impl Drop for WarmingGuard<'_> {
    fn drop(&mut self) {
        self.inner.lock().warming.remove(&self.asset_id);
    }
}

struct DecrementOnDrop<'a>(&'a AtomicU64);

impl Drop for DecrementOnDrop<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
