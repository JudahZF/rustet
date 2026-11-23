//! The coordination layer between the transport (the engine's HTTP handlers, exports) and the
//! media internals.
//!
//! The [`Manager`]:
//! 1. asks a [`Provider`] to open an asset as a media session with typed tracks,
//! 2. opens every track to get a ready-to-run pipeline [`Stream`],
//! 3. keeps the streams under a session ID,
//! 4. runs a stream into the caller's sink on request.
//!
//! It knows nothing about Apple Music, HLS, FairPlay or HTTP.

use std::collections::HashMap;
use std::io;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::FutureExt;
use rustet_core::media::{self, OpenRequest, Provider, Track};
use rustet_core::pipeline::{
    self, DirectInfo, PipelineError, Sink, Source, Stream, StreamContext, StreamKind,
};
use tokio::io::AsyncWrite;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::session::{Capabilities, Session, StreamUrls};

/// How long an unused session lives.
pub const SESSION_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// Use of a session pushes its expiry back out at most this often.
const TOUCH_EVERY: Duration = Duration::from_secs(5 * 60);

/// How often the reaper sweeps expired sessions.
const REAP_EVERY: Duration = Duration::from_secs(60);

/// Required rates (bytes/s) of a foreground stream whose session reports no bit rate.
const DEFAULT_AUDIO_NEED_BPS: u64 = 1_500_000 / 8;
const DEFAULT_VIDEO_NEED_BPS: u64 = 8_000_000 / 8;

/// Who a stream is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamClass {
    /// Real playback. It owns the bandwidth budget.
    #[default]
    Foreground,
    /// Opportunistic work (exports) that must yield to playback. Background streams never make
    /// [`Manager::is_streaming`] true and are not in [`Manager::foreground_stats`].
    Background,
}

/// Raw counters of the foreground streams. Measuring throughput is the caller's job (the
/// export bandwidth controller).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForegroundStats {
    /// Foreground streams running now.
    pub active: usize,
    /// Bytes written by foreground streams since start (monotonic).
    pub bytes: u64,
    /// Sum of the required rates (bytes/s) of the active foreground streams.
    pub need_bps: u64,
}

/// Why a session could not be opened. Cheap to clone: every caller that joined one open gets
/// the same error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum OpenError {
    /// The caller's token was cancelled while it waited for another caller's open.
    #[error("cancelled")]
    Cancelled,
    /// The provider failed to resolve the asset.
    #[error(transparent)]
    Provider(Arc<PipelineError>),
    /// A track of the resolved asset failed to open.
    #[error("open {kind} stream: {source}")]
    Track {
        kind: StreamKind,
        #[source]
        source: Arc<PipelineError>,
    },
    /// The provider or a track opener panicked, typically on a malformed manifest.
    #[error("open {asset_id}: panic: {message}")]
    Panicked { asset_id: String, message: String },
}

impl OpenError {
    /// True when the open stopped because a cancellation token fired, not because it failed.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::Provider(e) | Self::Track { source: e, .. } => {
                matches!(**e, PipelineError::Cancelled)
            }
            Self::Panicked { .. } => false,
        }
    }
}

/// Why a stream operation on a session failed.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("session {0} not found or expired")]
    SessionNotFound(String),
    #[error("session {id} has no {kind} stream")]
    NoStream { id: String, kind: StreamKind },
    #[error("session {0} stream is not seekable")]
    NotSeekable(String),
    #[error("session {0} stream has no fragment-level seek")]
    NoDirectSeek(String),
    /// The pipeline run (or the direct-seek planning) failed.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
}

/// The fields that decide whether two opens produce the same kind of session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpenKey {
    asset_id: String,
    storefront: String,
    lossless: bool,
    hi_res: bool,
    atmos: bool,
    video: bool,
    mv_max_height: u32,
}

impl From<&OpenRequest> for OpenKey {
    fn from(r: &OpenRequest) -> Self {
        Self {
            asset_id: r.asset_id.clone(),
            storefront: r.storefront.clone(),
            lossless: r.lossless,
            hi_res: r.hi_res,
            atmos: r.atmos,
            video: r.video,
            mv_max_height: r.mv_max_height,
        }
    }
}

/// One live session: its public descriptor and its private stream state.
struct Entry {
    session: Arc<Session>,
    streams: HashMap<StreamKind, Stream>,
    expiry: Instant,
    /// The reuse key, for shared sessions only.
    key: Option<OpenKey>,
}

#[derive(Default)]
struct Table {
    entries: HashMap<String, Entry>,
    /// Reuse index of shared sessions.
    by_key: HashMap<OpenKey, String>,
}

impl Table {
    /// Removes a session and its index entry. Every removal path goes through here, so the
    /// index can never outlive its session. Returns false if the session was already gone.
    fn remove(&mut self, id: &str) -> bool {
        let Some(entry) = self.entries.remove(id) else {
            return false;
        };
        // A newer session for the same key may have replaced this one in the index.
        if let Some(key) = entry.key
            && self.by_key.get(&key).is_some_and(|cur| cur == id)
        {
            self.by_key.remove(&key);
        }
        true
    }

    fn live_by_key(&self, key: &OpenKey, now: Instant) -> Option<Arc<Session>> {
        let entry = self.entries.get(self.by_key.get(key)?)?;
        (now <= entry.expiry).then(|| Arc::clone(&entry.session))
    }
}

type FlightResult = Result<Arc<Session>, OpenError>;

/// The receiving side of an open in progress. `None` until the leader finishes; a closed
/// channel without a value means the leader's future was dropped.
type Flight = watch::Receiver<Option<FlightResult>>;

type ReleaseHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Creates and manages playback sessions. The single entry point for the transport layer.
pub struct Manager {
    provider: Arc<dyn Provider>,
    table: Mutex<Table>,
    /// Opens in progress, so that concurrent opens of one asset share one provider call and
    /// do not orphan DRM sessions. Lock order: `inflight` before `table`.
    inflight: Mutex<HashMap<OpenKey, Flight>>,
    release_hook: RwLock<Option<ReleaseHook>>,

    /// Every running stream, of every class (for metrics).
    active_streams: AtomicUsize,
    /// Foreground streams. Background cache warming checks it so that it never competes for
    /// bandwidth with what the user is listening to: the rule Apple's Android client applies
    /// in `PlayerLoadControl.shouldPrepareNextPeriodForCaching`.
    fg_active: AtomicUsize,
    fg_bytes: AtomicU64,
    fg_need_bps: AtomicU64,

    /// Stops the reaper task.
    reaper: CancellationToken,
}

impl Manager {
    /// A manager backed by `provider`. Starts the expiry reaper, so it must be called inside a
    /// tokio runtime. The reaper stops when the manager is dropped.
    pub fn new(provider: Arc<dyn Provider>) -> Arc<Self> {
        let m = Arc::new(Self::unreaped(provider));
        let weak = Arc::downgrade(&m);
        let stop = m.reaper.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(Instant::now() + REAP_EVERY, REAP_EVERY);
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    _ = tick.tick() => {}
                }
                let Some(m) = weak.upgrade() else { return };
                m.sweep_expired(Instant::now());
            }
        });
        m
    }

    /// A manager without the reaper task.
    fn unreaped(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            table: Mutex::default(),
            inflight: Mutex::default(),
            release_hook: RwLock::default(),
            active_streams: AtomicUsize::new(0),
            fg_active: AtomicUsize::new(0),
            fg_bytes: AtomicU64::new(0),
            fg_need_bps: AtomicU64::new(0),
            reaper: CancellationToken::new(),
        }
    }

    // ── Open ────────────────────────────────────────────────────────────────

    /// Opens a session for playback, or returns the live session already open for the same
    /// asset and capabilities. Reuse avoids a full DRM round-trip when the user pauses and
    /// resumes a track (Android's `SVFootHillSessionController.getExistingContextKey`).
    ///
    /// Concurrent opens of the same asset and capabilities share one provider call. When the
    /// caller that made that call goes away (a skipped track), the others retry rather than
    /// inherit its cancellation.
    pub async fn open(
        &self,
        cancel: &CancellationToken,
        req: OpenRequest,
    ) -> Result<Arc<Session>, OpenError> {
        let key = OpenKey::from(&req);
        loop {
            let joined = {
                let mut inflight = lock(&self.inflight);
                // Checked under the in-flight lock: a leader stores its session before it
                // leaves the in-flight map, so a finished open is never repeated.
                if let Some(s) = lock(&self.table).live_by_key(&key, Instant::now()) {
                    return Ok(s);
                }
                match inflight.get(&key) {
                    Some(flight) => Ok(flight.clone()),
                    None => {
                        let (tx, rx) = watch::channel(None);
                        inflight.insert(key.clone(), rx);
                        Err(tx)
                    }
                }
            };
            let mut flight = match joined {
                Ok(flight) => flight,
                Err(tx) => return self.lead(cancel, req, key, tx).await,
            };
            let result = tokio::select! {
                () = cancel.cancelled() => return Err(OpenError::Cancelled),
                r = flight.wait_for(Option::is_some) => r.ok().and_then(|v| v.clone()),
            };
            match result {
                // The leader's own caller went away. That cancellation is not ours: retry,
                // possibly as the new leader.
                None => {}
                Some(Err(e)) if e.is_cancelled() => {}
                Some(result) => return result,
            }
            if cancel.is_cancelled() {
                return Err(OpenError::Cancelled);
            }
        }
    }

    /// Opens a session owned by the caller alone. It never reuses a session, never joins an
    /// open in progress, and is never reused by later opens. Exports and the prefetcher use
    /// it, so that their release can never delete a session that playback is serving.
    pub async fn open_private(
        &self,
        cancel: &CancellationToken,
        req: OpenRequest,
    ) -> Result<Arc<Session>, OpenError> {
        self.open_guarded(cancel, req, None).await
    }

    /// Runs the open as the leader of its flight and hands the result to the joined callers.
    async fn lead(
        &self,
        cancel: &CancellationToken,
        req: OpenRequest,
        key: OpenKey,
        tx: watch::Sender<Option<FlightResult>>,
    ) -> FlightResult {
        // Declared after `tx`, so it drops first: if this future is dropped mid-open, the
        // flight leaves the map before the channel closes, and the waiters' retry finds it gone.
        let _flight = FlightGuard {
            inflight: &self.inflight,
            key: key.clone(),
        };
        let result = self.open_guarded(cancel, req, Some(key)).await;
        tx.send_replace(Some(result.clone()));
        result
    }

    /// [`Self::open_direct`] with a provider panic turned into an error.
    async fn open_guarded(
        &self,
        cancel: &CancellationToken,
        req: OpenRequest,
        key: Option<OpenKey>,
    ) -> FlightResult {
        let asset_id = req.asset_id.clone();
        AssertUnwindSafe(self.open_direct(cancel, req, key))
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| {
                let message = crate::panic_message(&*payload);
                warn!(%asset_id, %message, "provider panicked during open");
                Err(OpenError::Panicked { asset_id, message })
            })
    }

    async fn open_direct(
        &self,
        cancel: &CancellationToken,
        req: OpenRequest,
        key: Option<OpenKey>,
    ) -> FlightResult {
        let ms = self
            .provider
            .open(cancel, &req)
            .await
            .map_err(|e| OpenError::Provider(Arc::new(e)))?;
        let opened = open_tracks(cancel, &req.asset_id, &ms.tracks).await?;

        let id = crate::new_id();
        let mut sess = Session {
            id: id.clone(),
            asset_id: req.asset_id.clone(),
            storefront: req.storefront,
            kind: ms.kind,
            codec: None,
            sample_rate: 0,
            bit_depth: 0,
            bit_rate: 0,
            channel_count: 0,
            codec_mime_type: String::new(),
            spatial_audio: None,
            capabilities: Capabilities {
                lyrics: ms.metadata.has_lyrics,
                ..Capabilities::default()
            },
            streams: StreamUrls {
                audio: Some(format!("/api/v1/playback/{id}/audio")),
                video: None,
            },
            title: ms.metadata.title,
            artist_name: ms.metadata.artist_name,
            album_name: ms.metadata.album_name,
            duration_ms: ms.metadata.duration_ms,
            artwork_url: ms.metadata.artwork_url,
            video_heights: ms.video_heights,
            mv_max_height: req.mv_max_height,
            expires_in: SESSION_TTL.as_secs(),
        };

        // Report the length the streams really have when it differs from the catalog's, so the
        // player's progress, fades and end-of-track handling match where the audio ends.
        let declared = opened.iter().map(|s| s.duration_ms).max().unwrap_or(0);
        let reconciled = media::reconcile_duration_ms(sess.duration_ms, declared);
        if reconciled != sess.duration_ms {
            info!(
                asset_id = %req.asset_id,
                stream_ms = reconciled,
                catalog_ms = sess.duration_ms,
                "stream length differs from the catalog: reporting the stream's"
            );
            sess.duration_ms = reconciled;
        }

        let mut streams = HashMap::with_capacity(opened.len());
        for (track, stream) in ms.tracks.iter().zip(opened) {
            match track.kind {
                StreamKind::Audio => {
                    sess.capabilities.audio = true;
                    // The first audio track describes the session.
                    if sess.codec.is_none() {
                        sess.codec = Some(track.codec);
                        sess.sample_rate = track.sample_rate;
                        sess.bit_depth = track.bit_depth;
                        sess.bit_rate = track.bit_rate;
                        sess.channel_count = track.channel_count;
                        sess.codec_mime_type.clone_from(&track.codec_mime_type);
                        sess.spatial_audio = track.spatial_audio;
                    }
                    sess.capabilities.seekable = stream.source.seekable().is_some();
                }
                StreamKind::Video => {
                    sess.capabilities.video = true;
                    // A video that fell back to a lower variant has that variant's codec.
                    sess.capabilities.video_codec = stream
                        .codec_string
                        .clone()
                        .unwrap_or_else(|| track.codec_string.clone());
                    sess.streams.video = Some(format!("/api/v1/playback/{id}/video"));
                }
            }
            streams.insert(track.kind, stream);
        }

        let session = Arc::new(sess);
        let mut table = lock(&self.table);
        if let Some(key) = &key {
            table.by_key.insert(key.clone(), id.clone());
        }
        table.entries.insert(
            id,
            Entry {
                session: Arc::clone(&session),
                streams,
                expiry: Instant::now() + SESSION_TTL,
                key,
            },
        );
        Ok(session)
    }

    // ── Lookup ──────────────────────────────────────────────────────────────

    /// The session's public descriptor. A lookup also keeps the session alive.
    pub fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.lookup(id, |e| Arc::clone(&e.session))
    }

    /// The segment-granular start time that a seek to `start_sec` gets, computed from the
    /// session's already-fetched playlist: the same computation [`Self::stream_from`] does, so
    /// callers can set response headers before streaming. `None` when the session or stream
    /// does not exist or is not seekable.
    pub fn seek_start(&self, id: &str, kind: StreamKind, start_sec: f64) -> Option<f64> {
        let (_, stream) = self.stream_of(id, kind).ok()?;
        let (_, actual) = stream.source.seekable()?.source_from(start_sec);
        Some(actual)
    }

    /// The cumulative HLS segment start times of the stream: entry `i` is the start (seconds)
    /// of segment `i`. `None` when the stream does not exist or has no timing data.
    pub fn segment_timings(&self, id: &str, kind: StreamKind) -> Option<Vec<f64>> {
        let (_, stream) = self.stream_of(id, kind).ok()?;
        stream.source.seekable()?.segment_timings()
    }

    /// Runs `f` on a live session's entry and slides its expiry. An expired session is
    /// removed instead.
    fn lookup<R>(&self, id: &str, f: impl FnOnce(&Entry) -> R) -> Option<R> {
        let now = Instant::now();
        let mut table = lock(&self.table);
        let entry = table.entries.get_mut(id)?;
        if now > entry.expiry {
            table.remove(id);
            drop(table);
            self.notify_released(vec![id.to_owned()]);
            return None;
        }
        // Sliding expiry: a session someone keeps using must not die at a fixed deadline in
        // the middle of a long listen. Refreshed at most once per TOUCH_EVERY.
        if entry.expiry - now < SESSION_TTL - TOUCH_EVERY {
            entry.expiry = now + SESSION_TTL;
        }
        Some(f(entry))
    }

    fn stream_of(&self, id: &str, kind: StreamKind) -> Result<(Arc<Session>, Stream), StreamError> {
        let (session, stream) = self
            .lookup(id, |e| {
                (Arc::clone(&e.session), e.streams.get(&kind).cloned())
            })
            .ok_or_else(|| StreamError::SessionNotFound(id.to_owned()))?;
        let stream = stream.ok_or_else(|| StreamError::NoStream {
            id: id.to_owned(),
            kind,
        })?;
        Ok((session, stream))
    }

    // ── Streaming ───────────────────────────────────────────────────────────

    /// Runs the session's `kind` stream into `dst`.
    pub async fn stream(
        &self,
        ctx: &StreamContext,
        class: StreamClass,
        id: &str,
        kind: StreamKind,
        dst: &mut dyn Sink,
    ) -> Result<(), StreamError> {
        debug!(session = id, %kind, "stream start");
        let (session, stream) = self.stream_of(id, kind).inspect_err(|e| warn!("{e}"))?;
        let res = self.run(ctx, class, &session, &stream, dst).await;
        debug!(session = id, ?res, "stream end");
        res
    }

    /// Runs the session's `kind` stream from about `start_sec` into `dst`, through the same
    /// stages as [`Self::stream`]. Only seekable sources support it. Returns the actual start
    /// (segment-granular, at or slightly before `start_sec`).
    ///
    /// The caller sets `ctx.seek_actual_start` and `ctx.seek_target` when the stages need them.
    pub async fn stream_from(
        &self,
        ctx: &StreamContext,
        class: StreamClass,
        id: &str,
        kind: StreamKind,
        start_sec: f64,
        dst: &mut dyn Sink,
    ) -> Result<f64, StreamError> {
        let (session, stream) = self.stream_of(id, kind)?;
        let seekable = stream
            .source
            .seekable()
            .ok_or_else(|| StreamError::NotSeekable(id.to_owned()))?;
        let (source, actual) = seekable.source_from(start_sec);
        let seek_stream = Stream { source, ..stream };
        self.run(ctx, class, &session, &seek_stream, dst).await?;
        Ok(actual)
    }

    /// Plans a fragment-level seek of the session's stream. Source errors come back unchanged
    /// as [`StreamError::Pipeline`], so callers can fall back to [`Self::stream_from`]. Run the
    /// returned source with [`Self::stream_source`].
    pub async fn prepare_direct_seek(
        &self,
        cancel: &CancellationToken,
        id: &str,
        kind: StreamKind,
        start_sec: f64,
    ) -> Result<(Arc<dyn Source>, DirectInfo), StreamError> {
        let (_, stream) = self.stream_of(id, kind)?;
        let direct = stream
            .source
            .seekable()
            .and_then(|s| s.direct_seekable())
            .ok_or_else(|| StreamError::NoDirectSeek(id.to_owned()))?;
        Ok(direct.direct_source_from(cancel, start_sec).await?)
    }

    /// Runs `source` (typically from [`Self::prepare_direct_seek`]) through the session
    /// stream's stages into `dst`.
    pub async fn stream_source(
        &self,
        ctx: &StreamContext,
        class: StreamClass,
        id: &str,
        kind: StreamKind,
        source: Arc<dyn Source>,
        dst: &mut dyn Sink,
    ) -> Result<(), StreamError> {
        let (session, stream) = self.stream_of(id, kind)?;
        let stream = Stream { source, ..stream };
        self.run(ctx, class, &session, &stream, dst).await
    }

    /// Runs one stream, registered in the stream counters for its whole life, including when
    /// it fails or its future is dropped.
    async fn run(
        &self,
        ctx: &StreamContext,
        class: StreamClass,
        session: &Session,
        stream: &Stream,
        dst: &mut dyn Sink,
    ) -> Result<(), StreamError> {
        let need = match (class, stream.kind) {
            (StreamClass::Background, _) => None,
            (StreamClass::Foreground, StreamKind::Video) => Some(DEFAULT_VIDEO_NEED_BPS),
            (StreamClass::Foreground, StreamKind::Audio) if session.bit_rate > 0 => {
                Some(u64::from(session.bit_rate) / 8)
            }
            (StreamClass::Foreground, StreamKind::Audio) => Some(DEFAULT_AUDIO_NEED_BPS),
        };
        let _registered = StreamGuard::begin(self, need);
        match need {
            None => pipeline::run(ctx, stream, dst).await?,
            Some(_) => {
                let mut counted = CountingSink {
                    inner: dst,
                    bytes: &self.fg_bytes,
                };
                pipeline::run(ctx, stream, &mut counted).await?;
            }
        }
        Ok(())
    }

    // ── Release and expiry ──────────────────────────────────────────────────

    /// Deletes a session. Releasing an unknown or already released session does nothing.
    pub fn release(&self, id: &str) {
        let removed = lock(&self.table).remove(id);
        if removed {
            self.notify_released(vec![id.to_owned()]);
        }
    }

    /// Registers `hook` to be called with the ID of every session that goes away for any
    /// reason: released, expired on lookup, or swept. It runs outside the manager's locks, so
    /// it may call back into the manager. Only one hook is kept.
    pub fn set_release_hook(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        *self
            .release_hook
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
    }

    fn notify_released(&self, ids: Vec<String>) {
        let hook = self
            .release_hook
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            for id in &ids {
                hook(id);
            }
        }
    }

    /// Removes every session expired at `now`.
    fn sweep_expired(&self, now: Instant) {
        let removed: Vec<String> = {
            let mut table = lock(&self.table);
            let expired: Vec<String> = table
                .entries
                .iter()
                .filter(|(_, e)| now > e.expiry)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &expired {
                table.remove(id);
            }
            expired
        };
        if !removed.is_empty() {
            self.notify_released(removed);
        }
    }

    // ── Metrics ─────────────────────────────────────────────────────────────

    /// True while any foreground (playback) stream runs. The prefetcher defers its work while
    /// this is true. Background streams never make it true.
    pub fn is_streaming(&self) -> bool {
        self.fg_active.load(Ordering::Relaxed) > 0
    }

    /// The number of running streams of every class.
    pub fn active_streams(&self) -> usize {
        self.active_streams.load(Ordering::Relaxed)
    }

    /// A snapshot of the foreground stream counters.
    pub fn foreground_stats(&self) -> ForegroundStats {
        ForegroundStats {
            active: self.fg_active.load(Ordering::Relaxed),
            bytes: self.fg_bytes.load(Ordering::Relaxed),
            need_bps: self.fg_need_bps.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.reaper.cancel();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // No critical section can leave the maps inconsistent, so a poisoned lock is safe to use.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Opens every track concurrently and returns the streams in track order. A music video has an
/// audio and a video track, each with its own playlist round-trip; opening them one after the
/// other added a round-trip to every start. If any open fails, the others are cancelled and the
/// first failure is returned.
async fn open_tracks(
    cancel: &CancellationToken,
    asset_id: &str,
    tracks: &[Track],
) -> Result<Vec<Stream>, OpenError> {
    let fail = |kind, e| {
        warn!(%asset_id, %kind, "track open failed: {e}");
        OpenError::Track {
            kind,
            source: Arc::new(e),
        }
    };
    match tracks {
        [] => Ok(Vec::new()),
        [track] => match track.open.open(cancel).await {
            Ok(stream) => Ok(vec![stream]),
            Err(e) => Err(fail(track.kind, e)),
        },
        _ => {
            // Cancelled with the parent, or here on failure. On success the opened streams may
            // keep using it, so it stays live.
            let token = cancel.child_token();
            let first_failed = OnceLock::new();
            let results = futures::future::join_all(tracks.iter().enumerate().map(|(i, t)| {
                let (token, first_failed) = (&token, &first_failed);
                async move {
                    let r = t.open.open(token).await;
                    if r.is_err() {
                        // Recorded before cancelling the others: their cancellation errors
                        // are a consequence, not the cause.
                        let _ = first_failed.set(i);
                        token.cancel();
                    }
                    r
                }
            }))
            .await;
            let first_failed = first_failed.get().copied();
            let mut streams = Vec::with_capacity(tracks.len());
            let mut failure = None;
            for (i, (track, r)) in tracks.iter().zip(results).enumerate() {
                match r {
                    Ok(stream) => streams.push(stream),
                    Err(e) if first_failed == Some(i) => failure = Some((track.kind, e)),
                    Err(_) => {}
                }
            }
            match failure {
                Some((kind, e)) => Err(fail(kind, e)),
                None => Ok(streams),
            }
        }
    }
}

/// Removes a flight from the in-flight map when its leader finishes, fails, panics or is
/// dropped, so no later open can join a flight that never ends.
struct FlightGuard<'a> {
    inflight: &'a Mutex<HashMap<OpenKey, Flight>>,
    key: OpenKey,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        lock(self.inflight).remove(&self.key);
    }
}

/// Registers one running stream in the counters until dropped.
struct StreamGuard<'a> {
    m: &'a Manager,
    /// The required rate of a foreground stream; `None` for a background one.
    need: Option<u64>,
}

impl<'a> StreamGuard<'a> {
    fn begin(m: &'a Manager, need: Option<u64>) -> Self {
        m.active_streams.fetch_add(1, Ordering::Relaxed);
        if let Some(need) = need {
            m.fg_active.fetch_add(1, Ordering::Relaxed);
            m.fg_need_bps.fetch_add(need, Ordering::Relaxed);
        }
        Self { m, need }
    }
}

impl Drop for StreamGuard<'_> {
    fn drop(&mut self) {
        if let Some(need) = self.need {
            self.m.fg_need_bps.fetch_sub(need, Ordering::Relaxed);
            self.m.fg_active.fetch_sub(1, Ordering::Relaxed);
        }
        self.m.active_streams.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Adds every byte written to a counter. Forwards headers, so sources can still set
/// `Content-Length` through it.
struct CountingSink<'a> {
    inner: &'a mut dyn Sink,
    bytes: &'a AtomicU64,
}

impl AsyncWrite for CountingSink<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let res = Pin::new(&mut *this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            this.bytes.fetch_add(n as u64, Ordering::Relaxed);
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

impl Sink for CountingSink<'_> {
    fn set_header(&mut self, name: &str, value: &str) {
        self.inner.set_header(name, value);
    }
}

#[cfg(test)]
mod tests;
