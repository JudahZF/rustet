//! Playback sessions: create, audio stream, gapless pre-cache, keep-alive, release, and the
//! pre-warm context.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use rustet_core::media::OpenRequest;
use rustet_core::pipeline::{PipelineError, StreamContext, StreamKind};
use rustet_diskcache::{PutWriter, StreamingPut};
use rustet_playback::prefetch::ContextPayload;
use rustet_playback::{OpenError, Session, StreamClass};
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::http::{ApiError, ApiResult, json, lang, query_param, read_json};
use crate::state::{AppState, Engine, Playback};
use crate::stream::{CHUNK, pipe, range_start, reader_body, serve_file, stream_media};

/// How long a background cache download of a playing track may run.
const CACHE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long a gapless pre-cache download may run.
const PRECACHE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The total size claimed by a ranged response from an in-progress lossless download: no ALAC
/// file is this large, and the real size is unknown until the download commits.
const RANGE_SENTINEL: u64 = 1 << 30;

pub(crate) fn playback(s: &AppState) -> Result<&Playback, ApiError> {
    s.playback
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "playback not available"))
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "session not found or expired")
}

/// The disk-cache qualifier of a session: its codec name.
pub(crate) fn qualifier(sess: &Session) -> &'static str {
    sess.codec.map_or("", |c| c.as_str())
}

// ── Create ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CreateRequest {
    asset_id: String,
    storefront: String,
    /// Optional overrides of the bearer token and media-user-token, which MusicKit already has.
    token: String,
    #[serde(rename = "mediaUserToken")]
    mut_token: String,
    capabilities: Capabilities,
    /// 0 = automatic (1080p in the provider).
    mv_max_height: i64,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(default, rename_all = "camelCase")]
struct Capabilities {
    lossless: bool,
    /// Hi-res ALAC permitted; otherwise lossless is capped at 44.1 kHz / 16-bit.
    hi_res: bool,
    video: bool,
    atmos: bool,
}

/// How a failed session open is answered, and whether it counts against the circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenFailure {
    pub status: StatusCode,
    /// Apple's servers (or the path to them) are unreachable. Only these trip the breaker: a
    /// per-track failure says nothing about reachability, and counting those once let three
    /// bad tracks in a row lock out every later open with "servers appear unreachable".
    pub transport: bool,
}

/// The class of a failed open, from the provider's typed error (Go matched the message text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenClass {
    /// The caller cancelled the open.
    Cancelled,
    NotFound,
    Unavailable,
    LicenceRefused,
    Authentication,
    Transport,
    /// Any other provider failure, a cancellation, a panic, an I/O error.
    Other,
}

impl OpenClass {
    pub fn of(err: &OpenError) -> Self {
        use rustet_apple::Error as Apple;
        let (OpenError::Provider(e) | OpenError::Track { source: e, .. }) = err else {
            return Self::Other;
        };
        let PipelineError::Other(boxed) = &**e else {
            return Self::Other;
        };
        match boxed.downcast_ref::<Apple>() {
            Some(Apple::NotFound(_)) => Self::NotFound,
            Some(Apple::Unavailable(_)) => Self::Unavailable,
            Some(Apple::LicenceRefused(_)) => Self::LicenceRefused,
            Some(Apple::Authentication(_)) => Self::Authentication,
            Some(Apple::Transport(_)) => Self::Transport,
            Some(Apple::Other(_) | Apple::Cancelled) | None => Self::Other,
        }
    }

    pub fn answer(self) -> OpenFailure {
        let (status, transport) = match self {
            // A missing item, and one with nothing this client can play: both are per-track and
            // permanent. 404 tells the renderer not to retry (Go answered 500 for the latter).
            Self::NotFound | Self::Unavailable => (StatusCode::NOT_FOUND, false),
            Self::Transport => (StatusCode::INTERNAL_SERVER_ERROR, true),
            Self::LicenceRefused | Self::Authentication | Self::Cancelled | Self::Other => {
                (StatusCode::INTERNAL_SERVER_ERROR, false)
            }
        };
        OpenFailure { status, transport }
    }
}

/// `POST /api/v1/playback`: opens a session (or claims a pre-warmed one) and returns its
/// descriptor with 201.
pub(crate) async fn create(State(s): State<AppState>, headers: HeaderMap, body: Body) -> ApiResult {
    let req: CreateRequest = read_json(body, 64 << 10)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid request body"))?;
    if req.asset_id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "assetId is required",
        ));
    }
    let p = playback(&s)?;

    // Request tokens (the renderer's live MusicKit session) win over the cached ones.
    let token = if req.token.is_empty() {
        s.tokens.developer()
    } else {
        req.token
    };
    let mut_token = if req.mut_token.is_empty() {
        s.tokens.music_user_token()
    } else {
        req.mut_token
    };
    if token.is_empty() || mut_token.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "not authenticated — provide token+mediaUserToken in request body or configure them",
        ));
    }
    s.tokens.set_developer(&token);
    // The media-user-token cookie of the web session pairs with the web developer token.
    s.set_music_user_token(&mut_token);
    let storefront = if req.storefront.is_empty() {
        s.tokens.playback_storefront()
    } else {
        req.storefront
    };
    let caps = req.capabilities;

    // A session the prefetcher already opened at the matching quality skips the webplayback
    // round trip (1–3 s).
    let pre_warmed = (!caps.atmos && !caps.video)
        .then(|| p.scheduler.take_pre_warmed(&req.asset_id, caps.lossless))
        .flatten()
        .and_then(|id| p.manager.session(&id));

    let sess = match pre_warmed {
        Some(sess) => sess,
        None => {
            let open = OpenRequest {
                asset_id: req.asset_id,
                storefront,
                token,
                mut_token,
                language: lang(&headers),
                lossless: caps.lossless,
                hi_res: caps.hi_res,
                atmos: caps.atmos,
                video: caps.video,
                mv_max_height: u32::try_from(req.mv_max_height).unwrap_or(0),
                mv_audio_priorities: Vec::new(),
            };
            // Cancelled when the client goes away.
            let cancel = CancellationToken::new();
            let _guard = cancel.clone().drop_guard();
            match open_session(&s, &cancel, open).await {
                Ok(sess) => sess,
                Err(f) => {
                    if f.class == OpenClass::Authentication {
                        s.emit("auth.expired", &json!({}));
                    }
                    return Err(f.into());
                }
            }
        }
    };

    s.emit(
        "playback.created",
        &json!({"sessionId": sess.id, "assetId": sess.asset_id, "codec": qualifier(&sess)}),
    );
    Ok(json(StatusCode::CREATED, &*sess))
}

/// A failed [`open_session`].
#[derive(Debug)]
pub(crate) struct OpenFailed {
    pub class: OpenClass,
    pub status: StatusCode,
    pub message: String,
}

impl From<OpenFailed> for ApiError {
    fn from(f: OpenFailed) -> Self {
        ApiError::new(f.status, f.message)
    }
}

/// Opens a session through the circuit breaker, recording the latency: the path of
/// `POST /api/v1/playback` and of the player's opener.
pub(crate) async fn open_session(
    s: &Engine,
    cancel: &CancellationToken,
    open: OpenRequest,
) -> Result<Arc<Session>, OpenFailed> {
    let p = s.playback.as_ref().ok_or(OpenFailed {
        class: OpenClass::Other,
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "playback not available".into(),
    })?;
    // Fast-fail while Apple's servers are repeatedly unreachable.
    if !s.open_breaker.allow() {
        return Err(OpenFailed {
            class: OpenClass::Transport,
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "playback resolution failed: Apple servers appear unreachable (circuit open)"
                .into(),
        });
    }
    let t0 = Instant::now();
    match p.manager.open(cancel, open).await {
        Ok(sess) => {
            s.open_breaker.record_success();
            s.open_latency
                .record(i64::try_from(t0.elapsed().as_millis()).unwrap_or(i64::MAX));
            Ok(sess)
        }
        Err(e) => {
            let class = if e.is_cancelled() {
                OpenClass::Cancelled
            } else {
                OpenClass::of(&e)
            };
            let f = class.answer();
            if f.transport {
                s.open_breaker.record_failure();
            } else if class != OpenClass::Cancelled {
                // A per-track failure proves the round trip worked.
                s.open_breaker.record_success();
            }
            Err(OpenFailed {
                class,
                status: f.status,
                message: format!("playback resolution failed: {e}"),
            })
        }
    }
}

// ── Audio ────────────────────────────────────────────────────────────────────

/// `GET /api/v1/playback/{id}/audio[?t=<seconds>]`.
///
/// Songs come from the disk cache when cached (with byte ranges), else stream while the cache
/// fills in the background. `?t=` restarts the stream near that time, bypassing the cache, and
/// reports the real (segment-granular) start in `X-Actual-Start`. Music-video audio streams
/// straight through.
pub(crate) async fn audio(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let p = playback(&s)?;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    debug!(id, t = query_param(&uri, "t"), range, "[audio] GET");
    let sess = p.manager.session(&id).ok_or_else(not_found)?;
    if !sess.capabilities.audio {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no audio stream in this session",
        ));
    }
    let seek = query_param(&uri, "t")
        .parse::<f64>()
        .ok()
        .filter(|v| *v > 0.0);
    let manager = p.manager.clone();

    // Music-video audio goes straight to MSE: no disk cache, so it cannot collide with the
    // song cache entry of the same asset ID.
    if sess.kind == rustet_core::media::SessionKind::MusicVideo {
        return Ok(plain_stream(manager, id).await);
    }

    // Seeks bypass the cache: a cached file may be truncated from an earlier partial download.
    if let (Some(cache), None) = (&s.disk_cache, seek) {
        let q = qualifier(&sess);
        if let Some(file) = cache.get(&sess.asset_id, q).await {
            let range = (!range.is_empty()).then_some(range.as_str());
            return serve_file(file, "audio/mp4", range)
                .await
                .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e));
        }

        // Lossless: stream to VLC while the download fills the cache, so the first play waits
        // for the first buffer (~0.5 s) instead of the whole download.
        if sess.codec == Some(rustet_core::pipeline::Codec::Alac) {
            // A VLC seek during the download: serve from the in-progress file when the offset
            // is already written, else 416 at once so the renderer falls back to a reload
            // (vlc/load with startMs) instead of hanging until VLC's timeout. 206 with a
            // Content-Range keeps VLC's byte counter right.
            if !range.is_empty()
                && let Some(entry) = cache.get_streaming(&sess.asset_id, q)
            {
                let offset = range_start(&range);
                let written = entry.written();
                debug!(id, offset, written, "[audio] ALAC Range seek");
                if offset > written {
                    let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                    insert(
                        &mut resp,
                        header::CONTENT_RANGE,
                        format!("bytes */{written}"),
                    );
                    return Ok(resp);
                }
                let mut resp = (
                    StatusCode::PARTIAL_CONTENT,
                    reader_body(entry.reader_at(offset)),
                )
                    .into_response();
                insert(&mut resp, header::CONTENT_TYPE, "audio/mp4".into());
                insert(&mut resp, header::ACCEPT_RANGES, "bytes".into());
                insert(
                    &mut resp,
                    header::CONTENT_RANGE,
                    format!("bytes {offset}-{}/{RANGE_SENTINEL}", RANGE_SENTINEL - 1),
                );
                return Ok(resp);
            }
            if let Ok(Some(put)) = cache.begin_streaming_put(&sess.asset_id, q).await {
                let reader = put.entry().reader_at(0);
                start_cache_download(&s, &id, put);
                let mut resp = reader_body(reader).into_response();
                insert(&mut resp, header::CONTENT_TYPE, "audio/mp4".into());
                insert(&mut resp, header::ACCEPT_RANGES, "bytes".into());
                return Ok(resp);
            }
        }

        // AAC and the rest: stream while caching in the background, so the first byte waits
        // for the first segment rather than the whole track. MSE reads sequentially; replays
        // hit the committed file above.
        if let Ok(Some(put)) = cache.begin_streaming_put(&sess.asset_id, q).await {
            let mut reader = put.entry().reader_at(0);
            start_cache_download(&s, &id, put);
            // A download that fails before the first byte is a clean 502, not an empty 200.
            let first = match reader.read(CHUNK).await {
                Ok(first) => first,
                Err(e) => {
                    return Ok(json(
                        StatusCode::BAD_GATEWAY,
                        &json!({"error": e.to_string()}),
                    ));
                }
            };
            let body = match first {
                None => Body::empty(),
                Some(first) => {
                    let rest = reader_body(reader).into_data_stream();
                    Body::from_stream(futures::stream::once(async { Ok(first) }).chain(rest))
                }
            };
            let mut resp = body.into_response();
            insert(&mut resp, header::CONTENT_TYPE, "audio/mp4".into());
            return Ok(resp);
        }
        // Another task is caching this key: stream without caching.
        return Ok(plain_stream(manager, id).await);
    }

    if let Some(seek) = seek {
        info!(id, codec = qualifier(&sess), seek, "[engine] seek");
        let mut ctx = StreamContext::new(CancellationToken::new());
        let actual = manager.seek_start(&id, StreamKind::Audio, seek);
        if let Some(actual) = actual {
            ctx.seek_actual_start = Some(actual);
            // The exact target lets the passthrough stage trim leading fragments within the
            // segment, for sub-segment accuracy.
            ctx.seek_target = Some(seek);
        }
        let cancel = ctx.cancel.clone();
        let mut resp = stream_media(cancel, "audio/mp4", move |mut sink| async move {
            manager
                .stream_from(
                    &ctx,
                    StreamClass::Foreground,
                    &id,
                    StreamKind::Audio,
                    seek,
                    &mut sink,
                )
                .await
                .map(drop)
        })
        .await;
        if let Some(actual) = actual {
            insert(
                &mut resp,
                "x-actual-start".parse().expect("valid header name"),
                format!("{actual:.3}"),
            );
        }
        return Ok(resp);
    }
    Ok(plain_stream(manager, id).await)
}

/// The session's whole audio stream through [`stream_media`].
async fn plain_stream(manager: Arc<rustet_playback::Manager>, id: String) -> Response {
    let cancel = CancellationToken::new();
    let ctx = StreamContext::new(cancel.clone());
    stream_media(cancel, "audio/mp4", move |mut sink| async move {
        manager
            .stream(
                &ctx,
                StreamClass::Foreground,
                &id,
                StreamKind::Audio,
                &mut sink,
            )
            .await
    })
    .await
}

fn insert(resp: &mut Response, name: header::HeaderName, value: String) {
    if let Ok(v) = HeaderValue::try_from(value) {
        resp.headers_mut().insert(name, v);
    }
}

/// What a cache download writes into: a streaming put that readers follow, or a plain put.
pub(crate) trait CacheWrite: Send {
    async fn write_chunk(&mut self, buf: &[u8]) -> std::io::Result<()>;
    async fn finish(self) -> Result<(), rustet_diskcache::Error>;
}

impl CacheWrite for StreamingPut {
    async fn write_chunk(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.write_all(buf).await
    }
    async fn finish(self) -> Result<(), rustet_diskcache::Error> {
        self.commit().await
    }
}

impl CacheWrite for PutWriter {
    async fn write_chunk(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.write_all(buf).await
    }
    async fn finish(self) -> Result<(), rustet_diskcache::Error> {
        self.commit().await
    }
}

/// Runs the session's audio into `put` through the ALAC gate, then commits it. Dropping the
/// put on failure discards it. Returns whether it was committed.
async fn fill_cache(
    s: &AppState,
    cancel: &CancellationToken,
    id: &str,
    mut put: impl CacheWrite,
) -> bool {
    let Some(p) = &s.playback else { return false };
    let (mut w, mut r) = pipe();
    let ctx = StreamContext::new(cancel.clone());
    let produce = async {
        let res = p
            .manager
            .stream(&ctx, StreamClass::Foreground, id, StreamKind::Audio, &mut w)
            .await;
        drop(w);
        res
    };
    // Parking happens here, before each write: back-pressure through the pipe stops the
    // stream pulling from the CDN.
    let consume = async {
        let mut buf = vec![0; CHUNK];
        loop {
            let n = r.read(&mut buf).await?;
            if n == 0 {
                return Ok::<_, std::io::Error>(());
            }
            s.alac
                .wait(cancel, id)
                .await
                .map_err(std::io::Error::other)?;
            put.write_chunk(&buf[..n]).await?;
            s.alac.wrote(id, n);
        }
    };
    let (produced, consumed) = tokio::join!(produce, consume);
    let ok = match (produced, consumed) {
        (Ok(()), Ok(())) => match put.finish().await {
            Ok(()) => true,
            Err(e) => {
                warn!(id, "[cache] commit: {e}");
                false
            }
        },
        (Err(e), _) => {
            warn!(id, "[cache] download: {e}");
            false
        }
        (_, Err(e)) => {
            warn!(id, "[cache] write: {e}");
            false
        }
    };
    s.alac.done(id);
    ok
}

/// Fills `put` from the session's audio in the background. The download is detached from the
/// client, so the cache still commits when the listener skips mid-track.
pub(crate) fn start_cache_download(s: &AppState, id: &str, put: StreamingPut) {
    let (s, id) = (s.clone(), id.to_owned());
    tokio::spawn(async move {
        let cancel = s.shutdown.child_token();
        debug!(id, "[cache] download start");
        let filled =
            tokio::time::timeout(CACHE_DOWNLOAD_TIMEOUT, fill_cache(&s, &cancel, &id, put)).await;
        cancel.cancel();
        match filled {
            Ok(true) => debug!(id, "[cache] download committed"),
            Ok(false) => {}
            Err(_) => {
                warn!(id, "[cache] download timed out");
                s.alac.done(&id);
            }
        }
    });
}

// ── Pre-cache, touch, delete, context ────────────────────────────────────────

/// `POST /api/v1/playback/{id}/precache`: downloads the session's audio into the disk cache in
/// the background, so the next track starts from the cache (gapless). 202 when started, 204
/// when already cached or there is no cache.
pub(crate) async fn precache(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let sess = playback(&s)?.manager.session(&id).ok_or_else(not_found)?;
    Ok(if start_precache(&s, id, &sess).await {
        StatusCode::ACCEPTED
    } else {
        StatusCode::NO_CONTENT
    }
    .into_response())
}

/// Starts the background disk-cache download of `sess` (gapless pre-cache). False when it is
/// already cached or there is no cache.
pub(crate) async fn start_precache(s: &AppState, id: String, sess: &Session) -> bool {
    let Some(cache) = s.disk_cache.clone() else {
        return false;
    };
    let q = qualifier(sess);
    if cache.path(&sess.asset_id, q).await.is_some() {
        return false;
    }
    let (s2, asset_id) = (s.clone(), sess.asset_id.clone());
    tokio::spawn(async move {
        let cancel = s2.shutdown.child_token();
        let work = async {
            match cache.begin_put(&asset_id, q).await {
                Ok(Some(put)) => {
                    if fill_cache(&s2, &cancel, &id, put).await {
                        info!(asset_id, session = id, "[precache] disk cache populated");
                    }
                }
                _ => s2.alac.done(&id),
            }
        };
        if tokio::time::timeout(PRECACHE_TIMEOUT, work).await.is_err() {
            s2.alac.done(&id);
        }
        cancel.cancel();
    });
    true
}

/// `POST /api/v1/playback/{id}/touch`: keeps a session alive while the renderer has it open (a
/// track paused for hours). 404 tells the renderer to reopen the track at its saved position.
pub(crate) async fn touch(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    playback(&s)?.manager.session(&id).ok_or_else(not_found)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /api/v1/playback/{id}`: stops the session's video producers and releases it.
pub(crate) async fn delete(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    release_session(&s, playback(&s)?, &id);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Stops a session's video producers, releases it and announces it.
pub(crate) fn release_session(s: &Engine, p: &Playback, id: &str) {
    s.vseg.stop(id);
    p.manager.release(id);
    s.emit("playback.deleted", &json!({"sessionId": id}));
}

/// `PUT /api/v1/playback/context`: the user is looking at this content; the scheduler decides
/// what to warm.
pub(crate) async fn context(State(s): State<AppState>, body: Body) -> ApiResult {
    let payload: ContextPayload = read_json(body, 512 << 10)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid request body"))?;
    let job_id = playback(&s)?.scheduler.submit(&payload);
    Ok(json(StatusCode::ACCEPTED, &json!({"jobId": job_id})))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The breaker fails every later open once it trips, so a per-track failure counted as an
    /// outage locks the whole app out after three bad tracks. Guard both directions.
    #[test]
    fn open_failures_map_to_status_and_breaker() {
        for (class, status, transport) in [
            (OpenClass::NotFound, 404, false),
            (OpenClass::Unavailable, 404, false),
            (OpenClass::LicenceRefused, 500, false),
            (OpenClass::Authentication, 500, false),
            (OpenClass::Transport, 500, true),
            (OpenClass::Other, 500, false),
        ] {
            let got = class.answer();
            assert_eq!(
                (got.status.as_u16(), got.transport),
                (status, transport),
                "{class:?}"
            );
        }
        // Errors that are not the provider's never count as an outage.
        for err in [
            OpenError::Cancelled,
            OpenError::Panicked {
                asset_id: "1".into(),
                message: "boom".into(),
            },
            OpenError::Provider(Arc::new(PipelineError::Cancelled)),
            OpenError::Provider(Arc::new(PipelineError::Io(std::io::Error::other("reset")))),
            OpenError::Track {
                kind: StreamKind::Audio,
                source: Arc::new(PipelineError::Other("x".into())),
            },
        ] {
            assert_eq!(OpenClass::of(&err), OpenClass::Other, "{err}");
        }
    }

    // A content failure resets the breaker: it proves the round trip worked.
    #[test]
    fn content_failures_do_not_accumulate() {
        let cb = crate::breaker::CircuitBreaker::new(3, Duration::from_secs(60));
        let record = |class: OpenClass| {
            if class.answer().transport {
                cb.record_failure()
            } else {
                cb.record_success()
            }
        };
        for _ in 0..10 {
            record(OpenClass::NotFound);
        }
        assert!(
            cb.allow(),
            "a run of unavailable tracks must not fail every later open"
        );
        for _ in 0..3 {
            record(OpenClass::Transport);
        }
        assert!(!cb.allow(), "genuine outages must still fast-fail");
    }
}
