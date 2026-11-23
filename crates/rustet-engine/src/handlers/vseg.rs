//! Segmented music video: the decrypted video stream as individual fMP4 fragments for MSE.
//!
//! ```text
//! GET    /api/v1/playback/{id}/vseg/init      ftyp+moov init segment (waits until ready)
//! GET    /api/v1/playback/{id}/vseg/seg/{n}   fragment n (streams as it is written)
//! GET    /api/v1/playback/{id}/vseg/manifest  {codecs, timescale, frags, done, err}, no wait
//! GET    /api/v1/playback/{id}/vseg/seek?t=T  switch producers; returns {n, t, reinit, …}
//! DELETE /api/v1/playback/{id}/vseg           stop the session's producers
//! ```
//!
//! Two producers: a base producer always runs from 0 and fills the disk cache, re-fragmented
//! by FFmpeg for B-frame safety. A seek the base has already indexed switches to it at once; a
//! seek beyond it starts a seek producer (fragment-level and FFmpeg-free when the codec
//! allows, else FFmpeg from the segment boundary) without stopping the base. The fragment
//! requests always read the active producer.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rustet_core::pipeline::{DirectInfo, Source, StreamContext, StreamKind};
use rustet_diskcache::{StreamingEntry, StreamingPut};
use rustet_playback::StreamClass;
use rustet_streams::{MvLiveIndex, VsegFragTiming, with_seek_priority};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::handlers::playback::playback;
use crate::http::{ApiError, ApiResult, json, query_param};
use crate::state::AppState;
use crate::stream::{CHUNK, guarded_body, pipe};

/// Bytes kept back from the write frontier while a fragment's end is unknown. Must exceed the
/// largest moof header; long GOPs produce moofs of several KiB.
const SAFE_STREAM_MARGIN: i64 = 64 << 10;
/// Mapping the segment for a fragment-level seek: a few small range reads.
const DIRECT_PLAN_TIMEOUT: Duration = Duration::from_secs(6);
/// From the plan to the first complete fragment.
const DIRECT_FIRST_TIMEOUT: Duration = Duration::from_secs(8);
/// The longest the from-0 download waits for a seek producer.
const SEEK_HOLD_MAX: Duration = Duration::from_secs(12);
/// Fragments a seek producer makes before the from-0 download may resume.
const SEEK_HOLD_FRAGMENTS: usize = 4;

/// One producer: its growing file and live fragment index.
pub(crate) struct Producer {
    /// Read side of the file. Holding it keeps the file open, so a fragment request after the
    /// producer committed (or was discarded) still reads the bytes.
    entry: StreamingEntry,
    idx: MvLiveIndex,
    cancel: CancellationToken,
    /// Never committed to the disk cache (seek producers, and a base producer started while
    /// another session still writes the key).
    ephemeral: bool,
    /// A fragment-level seek: its output is the decrypted raw stream (no FFmpeg remux), so it
    /// has its own init segment and a raw timeline.
    direct: bool,
    /// Set once the producer finished: `Ok` for a clean end, `Err` for a failure.
    outcome: OnceLock<Result<(), String>>,
}

impl Producer {
    fn new(
        entry: StreamingEntry,
        ephemeral: bool,
        direct: bool,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            entry,
            idx: MvLiveIndex::new(),
            cancel,
            ephemeral,
            direct,
            outcome: OnceLock::new(),
        })
    }

    fn done(&self) -> bool {
        self.outcome.get().is_some()
    }

    fn failed(&self) -> bool {
        matches!(self.outcome.get(), Some(Err(_)))
    }

    fn written(&self) -> i64 {
        i64::try_from(self.entry.written()).unwrap_or(i64::MAX)
    }

    fn finish(&self, res: Result<(), String>) {
        let _ = self.outcome.set(res);
    }

    /// The producer has produced the fragment covering `t`: an indexed fragment starts after
    /// `t`, or it finished. Without this a lookup returns the last fragment so far even when
    /// `t` lies far beyond it.
    fn is_past(&self, t: f64) -> bool {
        self.done() || self.idx.all_frag_timings().iter().any(|f| f.t > t)
    }
}

/// The producers of one playback session.
struct VsegState {
    /// The from-0 producer; `None` when the first request was a seek.
    base: Option<Arc<Producer>>,
    /// The producer the fragment requests read: the base or the latest seek producer.
    active: Arc<Producer>,
    /// The init segment the player's SourceBuffer was last told to use: false for the FFmpeg
    /// one, true for a direct session's raw one. The player re-appends the init when it flips.
    sb_direct: bool,
}

impl VsegState {
    /// Records the init kind handed out with a seek answer; true when it changed.
    fn mark_init_kind(&mut self, direct: bool) -> bool {
        std::mem::replace(&mut self.sb_direct, direct) != direct
    }

    fn is_base(&self, p: &Arc<Producer>) -> bool {
        self.base.as_ref().is_some_and(|b| Arc::ptr_eq(b, p))
    }
}

/// Every session's producers.
#[derive(Default)]
pub(crate) struct VsegStates(Mutex<HashMap<String, VsegState>>);

impl VsegStates {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, VsegState>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn active(&self, id: &str) -> Option<Arc<Producer>> {
        self.lock().get(id).map(|st| st.active.clone())
    }

    /// Cancels every producer of the session and forgets them.
    pub fn stop(&self, id: &str) {
        let Some(st) = self.lock().remove(id) else {
            return;
        };
        if let Some(base) = &st.base {
            base.cancel.cancel();
        }
        if !st.is_base(&st.active) {
            st.active.cancel.cancel();
        }
        info!(id, "[vseg] cancelled producers");
    }

    /// Makes `p` the active producer, cancelling the previous one unless it is the base.
    fn set_active(&self, id: &str, p: Arc<Producer>) {
        let mut states = self.lock();
        let old = match states.get_mut(id) {
            Some(st) => {
                let old = std::mem::replace(&mut st.active, p);
                (!st.is_base(&old)).then_some(old)
            }
            None => {
                states.insert(
                    id.to_owned(),
                    VsegState {
                        base: None,
                        active: p,
                        sb_direct: false,
                    },
                );
                None
            }
        };
        drop(states);
        if let Some(old) = old {
            old.cancel.cancel();
        }
    }

    fn mark_init_kind(&self, id: &str, direct: bool) -> bool {
        self.lock()
            .get_mut(id)
            .is_some_and(|st| st.mark_init_kind(direct))
    }
}

fn unique() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// The first codec of a comma-separated HLS `CODECS` value (`avc1.64001f,mp4a.40.2` →
/// `avc1.64001f`).
pub(crate) fn extract_video_codec(codecs: &str) -> &str {
    codecs.split(',').next().unwrap_or("").trim()
}

/// Whether the video can be handed to the browser as is: the FFmpeg path stream-copies H.264
/// and transcodes anything else (Chrome on Linux has no HEVC MSE decoder); the direct path
/// never transcodes.
pub(crate) fn codec_is_copyable(codecs: &str) -> bool {
    let c = extract_video_codec(codecs);
    c.starts_with("avc1") || c.starts_with("avc3")
}

/// Fragment-level seeks are on unless `RUSTET_MV_DIRECT_SEEK=0`, or the request says `direct=0`
/// (the player sets it after its own check of the direct output failed).
fn direct_seek_enabled(env: Option<&str>, query_direct: &str) -> bool {
    env != Some("0") && query_direct != "0"
}

/// A non-negative, finite seek time. (`"NaN"` parses, and slips past a `< 0` check.)
pub(crate) fn parse_seek_seconds(s: &str) -> Option<f64> {
    s.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}

/// Waits until the entry has more than `from` bytes or is complete, or `cancel` fires (the
/// client went away). Fails on cancellation and when the writer gave up.
pub(crate) async fn wait_for_growth(
    cancel: &CancellationToken,
    entry: &StreamingEntry,
    from: u64,
) -> Result<(), WaitError> {
    tokio::select! {
        () = cancel.cancelled() => Err(WaitError::Cancelled),
        r = entry.wait_for_growth(from) => r.map_err(WaitError::Writer),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WaitError {
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Writer(rustet_diskcache::Error),
}

// ── Producers ────────────────────────────────────────────────────────────────

/// Copies what the pipeline writes into `put`, then `idx`, in that order: the fragment
/// handler relies on the index never being ahead of the file.
async fn record(
    mut r: tokio::io::DuplexStream,
    put: &mut StreamingPut,
    idx: &MvLiveIndex,
) -> std::io::Result<()> {
    let mut buf = vec![0; CHUNK];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        put.write_all(&buf[..n]).await?;
        idx.feed(&buf[..n]);
    }
}

/// Where an FFmpeg producer starts.
#[derive(Clone, Copy)]
enum From {
    Start,
    /// A seek producer: from the HLS segment holding this time.
    Seek(f64),
}

/// Streams the decrypted video through FFmpeg into the producer's file and index.
/// From the start the file is committed to the disk cache on success; a non-ephemeral seek
/// producer is committed too, for backward-seek reuse.
async fn run_ffmpeg_producer(
    s: AppState,
    id: String,
    from: From,
    p: Arc<Producer>,
    mut put: StreamingPut,
) {
    let Some(pb) = &s.playback else { return };
    // H.264: stream copy, first fragment out in ~50 ms. HEVC and the rest: H.264 baseline
    // without B-frames (Chrome on Linux has no HEVC MSE decoder).
    let copy = pb
        .manager
        .session(&id)
        .is_some_and(|sess| codec_is_copyable(&sess.capabilities.video_codec));
    let v_args: &[&str] = if copy {
        &["-c:v", "copy"]
    } else {
        &[
            "-c:v", "libx264", "-preset", "fast", "-crf", "23", "-bf", "0",
        ]
    };
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        "pipe:0",
        "-map",
        "0:v:0",
    ])
    .args(v_args)
    .args([
        "-movflags",
        "frag_keyframe+empty_moov+default_base_moof",
        "-f",
        "mp4",
        "pipe:1",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!(id, "[vseg] producer setup error: ffmpeg start: {e}");
            p.finish(Err(format!("ffmpeg start: {e}")));
            return; // dropping the put discards it
        }
    };
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return;
    };
    let ctx = StreamContext::new(p.cancel.clone());

    let feed = async {
        // The stream's own error is not the producer's: FFmpeg's exit decides.
        let _ = match from {
            From::Start => {
                pb.manager
                    .stream(
                        &ctx,
                        StreamClass::Foreground,
                        &id,
                        StreamKind::Video,
                        &mut stdin,
                    )
                    .await
            }
            From::Seek(t) => pb
                .manager
                .stream_from(
                    &ctx,
                    StreamClass::Foreground,
                    &id,
                    StreamKind::Video,
                    t,
                    &mut stdin,
                )
                .await
                .map(drop),
        };
        drop(stdin);
    };
    let copy_out = async {
        let mut buf = vec![0; CHUNK];
        loop {
            let n = tokio::select! {
                () = p.cancel.cancelled() => return Ok(()),
                r = stdout.read(&mut buf) => r?,
            };
            if n == 0 {
                return Ok::<_, std::io::Error>(());
            }
            put.write_all(&buf[..n]).await?;
            p.idx.feed(&buf[..n]);
        }
    };
    let run = async { tokio::join!(feed, copy_out).1 };
    let copied = match from {
        From::Start => run.await,
        From::Seek(_) => with_seek_priority(run).await,
    };
    drop(stdout);
    let exited = tokio::select! {
        r = child.wait() => r,
        () = p.cancel.cancelled() => {
            let _ = child.kill().await;
            child.wait().await
        }
    };

    let written = p.written();
    // Finalize, then mark done, then commit or discard.
    p.idx.finalize(written);
    let cancelled = p.cancel.is_cancelled();
    let err = match (copied, exited) {
        (Err(e), _) if !cancelled => Some(format!("copy ffmpeg output: {e}")),
        (_, Ok(st)) if !st.success() && !cancelled => Some(format!("ffmpeg: {st}")),
        (_, Err(e)) if !cancelled => Some(format!("ffmpeg: {e}")),
        _ => None,
    };
    p.finish(err.clone().map_or(Ok(()), Err));
    let frags = p.idx.frag_count();
    if let Some(e) = err {
        warn!(id, "[vseg] producer error: {e}");
    } else if p.ephemeral || cancelled {
        // Ephemeral or cancelled mid-stream: discard the incomplete data.
        debug!(
            id,
            written,
            ephemeral = p.ephemeral,
            cancelled,
            "[vseg] producer discarded"
        );
    } else {
        let what = if matches!(from, From::Start) {
            "base"
        } else {
            "seek"
        };
        info!(id, written, frags, "[vseg] {what} producer done (cached)");
        if let Err(e) = put.commit().await {
            warn!(id, "[vseg] commit: {e}");
        }
    }
}

/// Feeds a fragment-level seek source through the session's stages straight into the
/// producer's file and index, the same sink the FFmpeg producer writes to. Never cached.
async fn run_direct_producer(
    s: AppState,
    id: String,
    src: Arc<dyn Source>,
    p: Arc<Producer>,
    mut put: StreamingPut,
) {
    let Some(pb) = &s.playback else { return };
    let ctx = StreamContext::new(p.cancel.clone());
    let (mut w, r) = pipe();
    let produce = async {
        let res = pb
            .manager
            .stream_source(
                &ctx,
                StreamClass::Foreground,
                &id,
                StreamKind::Video,
                src,
                &mut w,
            )
            .await;
        drop(w);
        res
    };
    let (streamed, recorded) =
        with_seek_priority(async { tokio::join!(produce, record(r, &mut put, &p.idx)) }).await;
    p.idx.finalize(p.written());
    let cancelled = p.cancel.is_cancelled();
    let err = match (streamed, recorded) {
        (Err(e), _) if !cancelled => Some(format!("direct stream: {e}")),
        (_, Err(e)) if !cancelled => Some(format!("direct stream: {e}")),
        _ => None,
    };
    match &err {
        Some(e) => warn!(id, "[vseg] direct producer error: {e}"),
        None => debug!(
            id,
            written = p.written(),
            frags = p.idx.frag_count(),
            cancelled,
            "[vseg] direct producer done"
        ),
    }
    p.finish(err.map_or(Ok(()), Err));
    drop(put); // direct sessions are never cached
}

/// Parks the from-0 producer's downloads until the seek producer can play (a few fragments),
/// finished, or was cancelled; at most [`SEEK_HOLD_MAX`].
fn hold_background_until_playable(s: &AppState, seek: &Arc<Producer>) {
    let hold = s.streams.hold_background(SEEK_HOLD_MAX);
    let seek = seek.clone();
    tokio::spawn(async move {
        let _hold = hold;
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                () = seek.cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            if seek.done() || seek.idx.frag_count() >= SEEK_HOLD_FRAGMENTS {
                return;
            }
        }
    });
}

/// The active producer of the session, starting the base producer on first use.
async fn ensure_session(s: &AppState, id: &str, asset_id: &str) -> Result<Arc<Producer>, String> {
    if let Some(p) = s.vseg.active(id) {
        return Ok(p);
    }
    let cache = s.disk_cache.as_ref().ok_or("no disk cache")?;
    let (put, ephemeral) = match cache.begin_streaming_put(asset_id, "mv-vseg").await {
        Err(e) => return Err(format!("begin streaming put: {e}")),
        Ok(Some(put)) => (put, false),
        Ok(None) => {
            // Another request of this session got there first.
            if let Some(p) = s.vseg.active(id) {
                return Ok(p);
            }
            // A different session (an earlier play of the same video) still owns the key:
            // produce into a private, uncached file instead of failing.
            let q = format!("mv-vseg-dup-{}", unique());
            match cache.begin_streaming_put(asset_id, &q).await {
                Ok(Some(put)) => (put, true),
                r => {
                    return Err(format!(
                        "vseg streaming put for assetID={asset_id}: {:?}",
                        r.err()
                    ));
                }
            }
        }
    };
    let base = Producer::new(
        put.entry().clone(),
        ephemeral,
        false,
        s.shutdown.child_token(),
    );
    {
        let mut states = s.vseg.lock();
        if let Some(st) = states.get(id) {
            // Another request stored a state first: ours goes (the put is dropped, discarded).
            return Ok(st.active.clone());
        }
        states.insert(
            id.to_owned(),
            VsegState {
                base: Some(base.clone()),
                active: base.clone(),
                sb_direct: false,
            },
        );
    }
    tokio::spawn(run_ffmpeg_producer(
        s.clone(),
        id.to_owned(),
        From::Start,
        base.clone(),
        put,
    ));
    info!(id, asset_id, "[vseg] base producer started");
    Ok(base)
}

/// Loads a committed seek file and rebuilds its fragment index from the bytes.
async fn load_cached_seek(s: &AppState, asset_id: &str, q: &str) -> Result<Arc<Producer>, String> {
    let cache = s.disk_cache.as_ref().ok_or("no disk cache")?;
    let entry = cache
        .open_committed(asset_id, q)
        .await
        .map_err(|e| format!("open committed seek: {e}"))?;
    let p = Producer::new(entry.clone(), false, false, CancellationToken::new());
    let mut reader = entry.reader_at(0);
    while let Some(chunk) = reader
        .read(CHUNK)
        .await
        .map_err(|e| format!("index rebuild: {e}"))?
    {
        p.idx.feed(&chunk);
    }
    p.idx.finalize(p.written());
    if p.idx.frag_count() == 0 {
        return Err("no fragments in cached seek file".into());
    }
    p.finish(Ok(()));
    Ok(p)
}

/// Starts an FFmpeg seek producer from the segment holding `start`, without stopping the base.
/// Returns the segment-aligned start.
async fn start_seek_producer(
    s: &AppState,
    id: &str,
    asset_id: &str,
    start: f64,
) -> Result<f64, String> {
    let pb = playback(s).map_err(|_| "no playback")?;
    let actual = pb
        .manager
        .seek_start(id, StreamKind::Video, start)
        .ok_or_else(|| format!("session {id} has no seekable video stream"))?;
    let cache = s.disk_cache.as_ref().ok_or("no disk cache")?;
    // The same segment boundary always maps to the same file, so a finished seek producer is
    // reused for backward seeks.
    let mut q = format!("mv-vseg-seek-{actual:.3}");
    if cache.path(asset_id, &q).await.is_some() {
        match load_cached_seek(s, asset_id, &q).await {
            Ok(p) => {
                info!(
                    id,
                    actual,
                    frags = p.idx.frag_count(),
                    "[vseg] seek cache hit"
                );
                s.vseg.set_active(id, p);
                return Ok(actual);
            }
            Err(e) => debug!(id, "[vseg] cached seek unusable: {e}"),
        }
    }
    let (put, ephemeral) = match cache.begin_streaming_put(asset_id, &q).await {
        Err(e) => return Err(format!("begin streaming put: {e}")),
        Ok(Some(put)) => (put, false),
        Ok(None) => {
            // Another task writes this key: a unique ephemeral file instead.
            q = format!("mv-vseg-seek-eph-{}", unique());
            match cache.begin_streaming_put(asset_id, &q).await {
                Ok(Some(put)) => (put, true),
                r => {
                    return Err(format!(
                        "vseg seek put fallback assetID={asset_id}: {:?}",
                        r.err()
                    ));
                }
            }
        }
    };
    let p = Producer::new(
        put.entry().clone(),
        ephemeral,
        false,
        s.shutdown.child_token(),
    );
    s.vseg.set_active(id, p.clone());
    hold_background_until_playable(s, &p);
    // The original time, not `actual`: the source steps back one segment for overlap itself.
    tokio::spawn(run_ffmpeg_producer(
        s.clone(),
        id.to_owned(),
        From::Seek(start),
        p,
        put,
    ));
    info!(id, start, actual, ephemeral, "[vseg] seek producer started");
    Ok(actual)
}

/// Starts a fragment-level seek session and returns once its first fragment is indexed, so
/// the caller can still fall back to FFmpeg when anything fails before that.
pub(crate) async fn start_direct_session(
    s: &AppState,
    id: &str,
    asset_id: &str,
    start: f64,
) -> Result<DirectInfo, String> {
    let pb = playback(s).map_err(|_| "no playback")?;
    let plan_cancel = s.shutdown.child_token();
    let _plan_guard = plan_cancel.clone().drop_guard();
    let planned = with_seek_priority(tokio::time::timeout(
        DIRECT_PLAN_TIMEOUT,
        pb.manager
            .prepare_direct_seek(&plan_cancel, id, StreamKind::Video, start),
    ))
    .await;
    let (src, info) = match planned {
        Ok(Ok(planned)) => planned,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("planning the direct seek timed out".into()),
    };
    let cache = s.disk_cache.as_ref().ok_or("no disk cache")?;
    let put = match cache
        .begin_streaming_put(asset_id, &format!("mv-vseg-direct-{}", unique()))
        .await
    {
        Ok(Some(put)) => put,
        r => return Err(format!("direct streaming put: {:?}", r.err())),
    };
    let p = Producer::new(put.entry().clone(), true, true, s.shutdown.child_token());
    s.vseg.set_active(id, p.clone());
    hold_background_until_playable(s, &p);
    tokio::spawn(run_direct_producer(
        s.clone(),
        id.to_owned(),
        src,
        p.clone(),
        put,
    ));

    let deadline = tokio::time::sleep(DIRECT_FIRST_TIMEOUT);
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    loop {
        if p.idx.frag_off_by_index(0).is_some() && p.idx.init_size().is_some_and(|n| n > 0) {
            return Ok(info);
        }
        if let Some(outcome) = p.outcome.get() {
            p.cancel.cancel();
            return Err(match outcome {
                Err(e) => e.clone(),
                Ok(()) => "direct stream ended before its first fragment".into(),
            });
        }
        tokio::select! {
            () = &mut deadline => {
                p.cancel.cancel();
                return Err("timed out waiting for the first direct fragment".into());
            }
            () = s.shutdown.cancelled() => return Err("cancelled".into()),
            _ = tick.tick() => {}
        }
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// The session, which must have video.
fn video_session(s: &AppState, id: &str) -> Result<Arc<rustet_playback::Session>, ApiError> {
    let sess = playback(s)?
        .manager
        .session(id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "session not found or expired"))?;
    if !sess.capabilities.video {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no video stream in this session",
        ));
    }
    Ok(sess)
}

async fn ensure(s: &AppState, id: &str, asset_id: &str) -> Result<Arc<Producer>, ApiError> {
    ensure_session(s, id, asset_id).await.map_err(|e| {
        warn!(id, "[vseg] ensure session: {e}");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not start vseg producer",
        )
    })
}

/// `GET …/vseg/init`: the init segment, once the first moof is parsed.
pub(crate) async fn init(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let sess = video_session(&s, &id)?;
    let p = ensure(&s, &id, &sess.asset_id).await?;
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    debug!(id, "[vseg/init]");
    loop {
        if let Some(size) = p.idx.init_size().filter(|&n| n > 0) {
            let size = u64::try_from(size).unwrap_or(0);
            let reader = p.entry.reader_at(0);
            let body = exact_body(reader, size);
            let mut resp = body.into_response();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            resp.headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(size));
            return Ok(resp);
        }
        match p.outcome.get() {
            Some(Err(_)) => {
                return Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "vseg producer failed",
                ));
            }
            Some(Ok(())) => {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "vseg stream ended without init segment",
                ));
            }
            None => {}
        }
        if wait_for_growth(&cancel, &p.entry, p.entry.written())
            .await
            .is_err()
        {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "read error waiting for init segment",
            ));
        }
    }
}

/// Exactly `len` bytes from `reader`; a short read aborts the body.
fn exact_body(reader: rustet_diskcache::StreamingReader, len: u64) -> Body {
    let stream = futures::stream::try_unfold((reader, len), |(mut r, left)| async move {
        if left == 0 {
            return Ok(None);
        }
        let max = usize::try_from(left).unwrap_or(usize::MAX).min(CHUNK);
        match r.read(max).await? {
            Some(chunk) => {
                let left = left - chunk.len() as u64;
                Ok(Some((chunk, (r, left))))
            }
            None => Err(rustet_diskcache::Error::Io(
                std::io::ErrorKind::UnexpectedEof.into(),
            )),
        }
    });
    Body::from_stream(stream)
}

/// `GET …/vseg/seg/{n}`: fragment n, streamed as soon as it is indexed. 404 past the end
/// (the player's end-of-stream signal).
pub(crate) async fn seg(
    State(s): State<AppState>,
    Path((id, n)): Path<(String, String)>,
) -> ApiResult {
    let sess = video_session(&s, &id)?;
    let n: usize = n
        .parse()
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid segment index"))?;
    let p = ensure(&s, &id, &sess.asset_id).await?;
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();
    debug!(id, n, "[vseg/seg]");

    // Phase 1: wait until fragment n is indexed. That happens as soon as its moof is written,
    // well before its mdat payload.
    let off = loop {
        if let Some(off) = p.idx.frag_off_by_index(n) {
            break off;
        }
        if let Some(outcome) = p.outcome.get() {
            if outcome.is_err() {
                return Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "vseg producer failed",
                ));
            }
            if n >= p.idx.frag_count() {
                return Err(ApiError::new(StatusCode::NOT_FOUND, "404 page not found"));
            }
        }
        if wait_for_growth(&cancel, &p.entry, p.entry.written())
            .await
            .is_err()
        {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "read error waiting for segment",
            ));
        }
    };

    // Phase 2: stream the fragment without waiting for the next moof. No Content-Length, so
    // the browser decodes bytes as they arrive.
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        if let Err(e) = stream_fragment(&p, n, off, &cancel, &tx).await {
            if !cancel.is_cancelled() {
                warn!(n, "[vseg/seg] {e}");
            }
            // A short body would be appended as a truncated fragment: abort the response, so
            // the client sees a failed fetch instead.
            let _ = tx.send(Err(std::io::Error::other(e))).await;
        }
    });
    // Dropping the body (the client went away) cancels the sender's waits.
    let chunks =
        futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) });
    let mut resp = guarded_body(chunks, guard).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    Ok(resp)
}

/// Sends fragment `n` (at `off`) as it is written. The end is taken from the next moof, else
/// from the end of the fragment's own mdat; while neither is known only bytes the indexer has
/// examined are sent, minus [`SAFE_STREAM_MARGIN`]: the index is fed after the bytes are
/// written, and trusting the raw write frontier could run into the next moof when the producer
/// bursts ahead ("Failed to prepare video sample for decode"). `Err` means the body must be
/// aborted; `Ok` ends it cleanly.
async fn stream_fragment(
    p: &Producer,
    n: usize,
    off: i64,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(), String> {
    let mut reader = p.entry.reader_at(u64::try_from(off).unwrap_or(0));
    let mut sent: i64 = 0;
    let send = async |chunk: Bytes| tx.send(Ok(chunk)).await.is_ok();
    loop {
        // The frontier before the limit: if the limit is unknown now, any moof inside the bytes
        // about to be sent is already indexed (the frontier is capped at what was parsed).
        let mut frontier = p.written();
        let parsed = p.idx.parsed_bytes();
        let behind = parsed < frontier;
        frontier = frontier.min(parsed);

        if let Some(limit) = p.idx.frag_limit_by_index(n) {
            let mut left = limit - off - sent;
            while left > 0 {
                let max = usize::try_from(left).unwrap_or(CHUNK).min(CHUNK);
                match reader.read(max).await {
                    Ok(Some(chunk)) => {
                        left -= chunk.len() as i64;
                        if !send(chunk).await {
                            return Ok(());
                        }
                    }
                    Ok(None) => return Err(format!("short read: {left} bytes missing")),
                    Err(e) => return Err(format!("short read: {left} bytes missing: {e}")),
                }
            }
            return Ok(());
        }

        let safe = frontier - off - sent - SAFE_STREAM_MARGIN;
        if safe > 0 {
            let max = usize::try_from(safe).unwrap_or(CHUNK).min(CHUNK);
            match reader.read(max).await {
                Ok(Some(chunk)) => {
                    sent += chunk.len() as i64;
                    if !send(chunk).await {
                        return Ok(());
                    }
                }
                Ok(None) | Err(_) => return Ok(()),
            }
            continue; // re-check the limit before waiting
        }

        if let Some(outcome) = p.outcome.get() {
            if outcome.is_ok()
                && let Some(end) = p.idx.frag_end_by_index(n)
            {
                // Finalized: the end is known now.
                let mut left = end - off - sent;
                while left > 0 {
                    let max = usize::try_from(left).unwrap_or(CHUNK).min(CHUNK);
                    let Ok(Some(chunk)) = reader.read(max).await else {
                        break;
                    };
                    left -= chunk.len() as i64;
                    if !send(chunk).await {
                        break;
                    }
                }
            }
            return Ok(());
        }

        // The indexer is still parsing bytes already written. That can reveal the fragment's
        // end without another write, and waiting for growth would then stall until the
        // producer's next burst: poll briefly.
        if behind {
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                () = tokio::time::sleep(Duration::from_millis(2)) => {}
            }
            continue;
        }
        // Other wait errors are re-checked through the outcome on the next round.
        if let Err(WaitError::Cancelled) =
            wait_for_growth(cancel, &p.entry, p.entry.written()).await
        {
            return Ok(());
        }
    }
}

/// `GET …/vseg/manifest`: the active producer's fragment index, without waiting. `frags: []`
/// is valid while the producer starts; `done: true` is the end.
pub(crate) async fn manifest(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let sess = video_session(&s, &id)?;
    let codecs = extract_video_codec(&sess.capabilities.video_codec);
    let Some(p) = s.vseg.active(&id) else {
        let none: [VsegFragTiming; 0] = [];
        return Ok(json(
            StatusCode::OK,
            &json!({"codecs": codecs, "timescale": 0, "frags": none, "done": false, "err": false}),
        ));
    };
    Ok(json(
        StatusCode::OK,
        &json!({
            "codecs": codecs,
            "timescale": p.idx.timescale(),
            "frags": p.idx.all_frag_timings(),
            "done": p.done(),
            "err": p.failed(),
        }),
    ))
}

/// `GET …/vseg/seek?t=T[&direct=0]`: answers at once with where the player restarts its
/// fragment loop (`{n, t, reinit}`, plus `direct` and `tsOffset` for a fragment-level seek).
pub(crate) async fn seek(State(s): State<AppState>, Path(id): Path<String>, uri: Uri) -> ApiResult {
    let sess = video_session(&s, &id)?;
    let t = parse_seek_seconds(&query_param(&uri, "t"))
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid t parameter"))?;
    debug!(id, t, "[vseg/seek]");

    // The base has produced past t: switch to it, cancelling any seek producer.
    let from_base = {
        let mut states = s.vseg.lock();
        states.get_mut(&id).and_then(|st| {
            let base = st.base.clone()?;
            let (n, frag) = base.idx.frag_index_for_time(t, base.written())?;
            if !base.is_past(t) {
                return None;
            }
            let old = std::mem::replace(&mut st.active, base);
            let old = (!st.is_base(&old)).then_some(old);
            Some((n, frag.t, st.mark_init_kind(false), old))
        })
    };
    if let Some((n, ft, reinit, old)) = from_base {
        if let Some(old) = old {
            old.cancel.cancel();
        }
        debug!(id, t, n, ft, "[vseg/seek] instant from base");
        return Ok(json(
            StatusCode::OK,
            &json!({"n": n, "t": ft, "reinit": reinit}),
        ));
    }

    // The current seek producer already covers t (forward within its range, or backward to a
    // position it has passed). Direct sessions are never reused: their timeline is raw, their
    // init differs, and a new one is cheap.
    let from_active = {
        let mut states = s.vseg.lock();
        states.get_mut(&id).and_then(|st| {
            if st.is_base(&st.active) || st.active.direct {
                return None;
            }
            let (n, frag) = st.active.idx.frag_index_for_time(t, st.active.written())?;
            st.active
                .is_past(t)
                .then(|| (n, frag.t, st.mark_init_kind(false)))
        })
    };
    if let Some((n, ft, reinit)) = from_active {
        debug!(id, t, n, ft, "[vseg/seek] instant from active seek");
        return Ok(json(
            StatusCode::OK,
            &json!({"n": n, "t": ft, "reinit": reinit}),
        ));
    }

    // Fragment level: fetch and decrypt only the fragment holding t and what follows, no
    // FFmpeg. Any failure before its first fragment falls through to FFmpeg.
    let env = std::env::var("RUSTET_MV_DIRECT_SEEK").ok();
    if direct_seek_enabled(env.as_deref(), &query_param(&uri, "direct"))
        && codec_is_copyable(&sess.capabilities.video_codec)
    {
        match start_direct_session(&s, &id, &sess.asset_id, t).await {
            Ok(info) => {
                let reinit = s.vseg.mark_init_kind(&id, true);
                debug!(
                    id,
                    t,
                    start = info.start_sec,
                    ts_offset = info.ts_offset,
                    reinit,
                    "[vseg/seek] direct"
                );
                return Ok(json(
                    StatusCode::OK,
                    &json!({"n": 0, "t": info.start_sec, "direct": true, "tsOffset": info.ts_offset, "reinit": reinit}),
                ));
            }
            Err(e) => info!(
                id,
                t, "[vseg/seek] direct seek unavailable: {e}; falling back to FFmpeg"
            ),
        }
    }

    // The base has not reached t: a parallel seek producer from the segment boundary.
    let actual = start_seek_producer(&s, &id, &sess.asset_id, t)
        .await
        .map_err(|e| {
            warn!(id, "[vseg/seek] start seek producer: {e}");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not start vseg seek producer",
            )
        })?;
    let reinit = s.vseg.mark_init_kind(&id, false);
    Ok(json(
        StatusCode::OK,
        &json!({"n": 0, "t": actual, "reinit": reinit}),
    ))
}

/// `DELETE …/vseg`: stops the session's producers without releasing the session (the player
/// left the video before the session DELETE).
pub(crate) async fn stop(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    s.vseg.stop(&id);
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_seconds_must_be_finite_and_non_negative() {
        for (s, ok) in [
            ("0", true),
            ("12.5", true),
            ("1e3", true),
            ("", false),
            ("-1", false),
            ("NaN", false),
            ("nan", false),
            ("inf", false),
            ("Inf", false),
            ("+Inf", false),
            ("-Inf", false),
            ("infinity", false),
            ("abc", false),
        ] {
            assert_eq!(parse_seek_seconds(s).is_some(), ok, "{s:?}");
        }
    }

    #[test]
    fn copyable_codecs() {
        for (c, want) in [
            ("avc1.640028", true),
            ("avc1.64001f,mp4a.40.2", true),
            ("avc3.640028", true),
            ("hvc1.2.4.L123.B0", false),
            ("hev1.1.6.L93.B0", false),
            ("", false),
        ] {
            assert_eq!(codec_is_copyable(c), want, "{c}");
        }
        assert_eq!(
            extract_video_codec(" avc1.64001f , mp4a.40.2"),
            "avc1.64001f"
        );
    }

    #[test]
    fn direct_seek_switches() {
        assert!(direct_seek_enabled(None, ""), "on by default");
        assert!(direct_seek_enabled(Some("1"), "1"));
        assert!(
            !direct_seek_enabled(None, "0"),
            "?direct=0 disables it for the request"
        );
        assert!(
            !direct_seek_enabled(Some("0"), ""),
            "RUSTET_MV_DIRECT_SEEK=0 disables it"
        );
    }

    #[tokio::test]
    async fn wait_for_growth_ends_on_cancel_and_on_growth() {
        let dir = tempfile::tempdir().unwrap();
        let cache = rustet_diskcache::Cache::new(dir.path()).unwrap();
        let mut put = cache
            .begin_streaming_put("mv", "vseg")
            .await
            .unwrap()
            .unwrap();
        let entry = put.entry().clone();

        let cancel = CancellationToken::new();
        let waiter = tokio::spawn({
            let (cancel, entry) = (cancel.clone(), entry.clone());
            async move { wait_for_growth(&cancel, &entry, 0).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel(); // client disconnect with no producer progress
        let r = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("ignored the disconnect")
            .unwrap();
        assert!(matches!(r, Err(WaitError::Cancelled)));

        let waiter = tokio::spawn({
            let entry = entry.clone();
            async move { wait_for_growth(&CancellationToken::new(), &entry, 0).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        put.write_all(b"x").await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("did not wake on new bytes")
            .unwrap();
        assert!(r.is_ok());
    }

    // A fragment request after the producer committed must still read the bytes: the producer
    // keeps the file open.
    #[tokio::test]
    async fn the_file_stays_readable_after_commit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = rustet_diskcache::Cache::new(dir.path()).unwrap();
        let mut put = cache
            .begin_streaming_put("asset", "mv-vseg")
            .await
            .unwrap()
            .unwrap();
        let p = Producer::new(put.entry().clone(), false, false, CancellationToken::new());
        put.write_all(b"0123456789").await.unwrap();
        put.commit().await.unwrap();
        let mut r = p.entry.reader_at(2);
        let mut got = Vec::new();
        while let Some(c) = r.read(64).await.unwrap() {
            got.extend_from_slice(&c);
        }
        assert_eq!(got, b"23456789");
    }

    #[test]
    fn init_kind_flips_report_a_reinit() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cache = rustet_diskcache::Cache::new(dir.path()).unwrap();
        let put = rt
            .block_on(cache.begin_streaming_put("a", "q"))
            .unwrap()
            .unwrap();
        let p = Producer::new(put.entry().clone(), false, false, CancellationToken::new());
        let mut st = VsegState {
            base: None,
            active: p,
            sb_direct: false,
        };
        assert!(!st.mark_init_kind(false));
        assert!(st.mark_init_kind(true));
        assert!(!st.mark_init_kind(true));
        assert!(st.mark_init_kind(false));
    }
}
