//! `/api/v1/vlc/*`: the in-process libvlc player for lossless tracks the browser cannot decode.
//! Without libvlc every route answers 503 and the renderer falls back to MSE.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use rustet_diskcache::StreamingReader;
use rustet_vlc::{AbortHandle, Player, Source};
use serde::Deserialize;
use serde_json::json;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::alacgate::START_BYTES;
use crate::handlers::playback::{playback, qualifier, start_cache_download};
use crate::http::{ApiError, ApiResult, json};
use crate::state::{AppState, Engine, Vlc};

/// Bumped whenever a fix changes which bytes are written to the lossless disk cache. On a
/// mismatch every `*-alac.m4a` file is deleted, so VLC never replays bytes an older, broken key
/// path wrote.
const ALAC_CACHE_VERSION: &str = "cbcs-fairplay-upstream-v10";

/// Deletes every committed lossless cache file when the `.alac_ver` marker in `dir` differs
/// from [`ALAC_CACHE_VERSION`], then writes the marker: a one-time migration after a
/// decryption fix. Blocking.
pub(crate) fn invalidate_alac_cache_if_stale(dir: &FsPath) {
    let marker = dir.join(".alac_ver");
    if std::fs::read_to_string(&marker).is_ok_and(|v| v.trim() == ALAC_CACHE_VERSION) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let deleted = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .filter(|e| e.file_name().to_string_lossy().ends_with("-alac.m4a"))
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count();
    if deleted > 0 {
        info!("[vlc] invalidated {deleted} stale ALAC cache file(s) (FairPlay key fix)");
    }
    let _ = std::fs::write(marker, ALAC_CACHE_VERSION);
}

/// Which playback session owns the player, so a delayed seek from a track the user already
/// skipped cannot move the next one.
#[derive(Default)]
pub(crate) struct VlcGate(Mutex<String>);

impl VlcGate {
    pub fn set(&self, id: &str) {
        id.clone_into(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner));
    }

    pub fn clear(&self) {
        self.set("");
    }

    /// Whether `id` may drive the player. An empty ID never does: every caller of `/vlc/seek`
    /// names the session it was started for.
    pub fn owns(&self, id: &str) -> bool {
        !id.is_empty() && *self.0.lock().unwrap_or_else(PoisonError::into_inner) == id
    }
}

fn vlc(s: &AppState) -> Result<&Vlc, ApiError> {
    s.vlc
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "libvlc not available"))
}

/// Runs a blocking player call off the runtime workers.
async fn on_player<R: Send + 'static>(
    player: &Arc<Player>,
    f: impl FnOnce(&Player) -> R + Send + 'static,
) -> R {
    let player = player.clone();
    match tokio::task::spawn_blocking(move || f(&player)).await {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

async fn read_body<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    crate::http::read_json(body, 4 << 10)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadRequest {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    start_ms: i64,
}

/// `POST /api/v1/vlc/load` with `{"sessionId", "startMs"?}`: plays the session's audio from
/// the disk cache (committed file, in-progress download, or a download started now), or over
/// the loopback HTTP endpoint when there is no cache.
pub(crate) async fn load(State(s): State<AppState>, body: Body) -> ApiResult {
    let vlc = vlc(&s)?;
    let req: LoadRequest = read_body(body).await?;
    if req.session_id.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "sessionId required"));
    }
    let sess = playback(&s)?
        .manager
        .session(&req.session_id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "session not found or expired"))?;
    let q = qualifier(&sess);
    let id = req.session_id;
    s.routes_take_libvlc().await;
    s.alac
        .foreground(&id, needs_hold(&s, &sess.asset_id, q).await);
    let source = match open_source(&s, &id, &sess.asset_id, q).await {
        Ok(src) => src,
        Err(e) => {
            s.alac.done(&id);
            return Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e));
        }
    };
    let loaded = match source {
        Some(src) => {
            debug!(
                session = id,
                start_ms = req.start_ms,
                "[vlc] load in-process"
            );
            on_player(&vlc.player, move |p| p.load_source(src)).await
        }
        None => {
            // Served over HTTP without a cache: nothing of ours to wait for.
            s.alac.done(&id);
            let url = format!(
                "http://127.0.0.1:{}/api/v1/playback/{id}/audio",
                s.guard.port
            );
            info!(url, start_ms = req.start_ms, "[vlc] load (no disk cache)");
            on_player(&vlc.player, move |p| p.load(&url)).await
        }
    };
    loaded.map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    vlc.gate.set(&id);
    // The seek is applied once VLC plays.
    if req.start_ms > 0 {
        let ms = req.start_ms;
        on_player(&vlc.player, move |p| p.set_time(ms)).await;
    }
    Ok(StatusCode::OK.into_response())
}

/// `POST /api/v1/vlc/pause`.
pub(crate) async fn pause(State(s): State<AppState>) -> ApiResult {
    on_player(&vlc(&s)?.player, Player::pause).await;
    Ok(StatusCode::OK.into_response())
}

/// `POST /api/v1/vlc/resume`.
pub(crate) async fn resume(State(s): State<AppState>) -> ApiResult {
    let vlc = vlc(&s)?;
    s.routes_take_libvlc().await;
    on_player(&vlc.player, Player::resume).await;
    Ok(StatusCode::OK.into_response())
}

/// `GET /api/v1/vlc/time`: position, length, state, the live mixer state, and whether the
/// source ended cleanly (then an "ended" state is the real end of the data, not a failed read).
pub(crate) async fn time(State(s): State<AppState>) -> ApiResult {
    let (timing, volume, eof) = on_player(&vlc(&s)?.player, |p| {
        (p.timing(), p.volume(), p.source_ended())
    })
    .await;
    Ok(json(
        StatusCode::OK,
        &json!({
            "posMs": timing.position_ms,
            "lengthMs": timing.length_ms,
            "state": timing.state.as_str(),
            "volume": volume.level,
            "muted": volume.muted,
            "eof": eof,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeekRequest {
    #[serde(default)]
    pos_ms: i64,
    #[serde(default)]
    session_id: String,
}

/// `POST /api/v1/vlc/seek` with `{"posMs", "sessionId"}`: seeks within the loaded media. 409
/// when the session no longer owns the player.
pub(crate) async fn seek(State(s): State<AppState>, body: Body) -> ApiResult {
    let vlc = vlc(&s)?;
    let req: SeekRequest = read_body(body).await?;
    debug!(
        pos_ms = req.pos_ms,
        session = req.session_id,
        "[vlc seek] API recv"
    );
    if !vlc.gate.owns(&req.session_id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "session does not own the player",
        ));
    }
    let ms = req.pos_ms;
    on_player(&vlc.player, move |p| p.set_time(ms)).await;
    Ok(json(StatusCode::OK, &json!({"actualStartMs": req.pos_ms})))
}

/// `POST /api/v1/vlc/stop`.
pub(crate) async fn stop(State(s): State<AppState>) -> ApiResult {
    let vlc = vlc(&s)?;
    vlc.gate.clear();
    on_player(&vlc.player, Player::stop).await;
    Ok(StatusCode::OK.into_response())
}

#[derive(Deserialize)]
struct VolumeRequest {
    #[serde(default)]
    volume: i64,
}

/// `POST /api/v1/vlc/volume` with `{"volume": 0–200}`.
pub(crate) async fn volume(State(s): State<AppState>, body: Body) -> ApiResult {
    let vlc = vlc(&s)?;
    let req: VolumeRequest = read_body(body).await?;
    on_player(&vlc.player, move |p| p.set_volume(req.volume)).await;
    Ok(StatusCode::OK.into_response())
}

/// Whether the player is about to wait on this session's bytes, so its download goes first.
/// Nothing to protect when the file is cached or the download is already well ahead.
pub(crate) async fn needs_hold(s: &Engine, asset_id: &str, q: &str) -> bool {
    match &s.disk_cache {
        Some(c) => {
            c.path(asset_id, q).await.is_none()
                && c.get_streaming(asset_id, q)
                    .is_none_or(|e| e.written() < START_BYTES)
        }
        None => true,
    }
}

/// A libvlc source for the session's audio: the committed cache file, a reader on the
/// in-progress download, or a download started now. `None` when no cache reader can be had;
/// the caller then falls back to the HTTP endpoint.
pub(crate) async fn open_source(
    s: &AppState,
    id: &str,
    asset_id: &str,
    q: &str,
) -> io::Result<Option<CacheSource>> {
    let Some(cache) = &s.disk_cache else {
        return Ok(None);
    };
    // A download can commit between the checks below; the second round picks up the file.
    for _ in 0..2 {
        if let Some(file) = cache.get(asset_id, q).await {
            let file = file.into_std().await;
            let size = file.metadata()?.len();
            return Ok(Some(CacheSource::File { file, size }));
        }
        if let Some(entry) = cache.get_streaming(asset_id, q) {
            if let Some(reader) = entry.reader_if_active() {
                return Ok(Some(CacheSource::growing(reader)));
            }
            continue;
        }
        if let Some(put) = cache.begin_streaming_put(asset_id, q).await? {
            let reader = put.entry().reader_at(0);
            start_cache_download(s, id, put);
            return Ok(Some(CacheSource::growing(reader)));
        }
    }
    // A plain pre-cache holds the key: the HTTP endpoint streams uncached.
    Ok(None)
}

/// What libvlc reads from: a committed cache file, or an entry still being downloaded.
pub(crate) enum CacheSource {
    /// Reads never block.
    File { file: std::fs::File, size: u64 },
    /// Reads wait for the download; the abort token unblocks them.
    Growing {
        reader: StreamingReader,
        runtime: Handle,
        abort: CancellationToken,
    },
}

impl CacheSource {
    /// Must be called inside the runtime, which then drives the reader's futures.
    pub(crate) fn growing(reader: StreamingReader) -> Self {
        Self::Growing {
            reader,
            runtime: Handle::current(),
            abort: CancellationToken::new(),
        }
    }
}

impl Read for CacheSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File { file, .. } => file.read(buf),
            Self::Growing {
                reader,
                runtime,
                abort,
            } => runtime.block_on(async {
                tokio::select! {
                    biased;
                    () = abort.cancelled() => Err(io::Error::other("source aborted")),
                    r = reader.read(buf.len()) => match r {
                        Ok(Some(chunk)) => {
                            buf[..chunk.len()].copy_from_slice(&chunk);
                            Ok(chunk.len())
                        }
                        Ok(None) => Ok(0),
                        Err(e) => Err(io::Error::from(e)),
                    },
                }
            }),
        }
    }
}

impl Seek for CacheSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            Self::File { file, .. } => file.seek(pos),
            // Like Go's streaming reader: from the start or the current position only. A seek
            // past the written length is allowed; the next read waits for it.
            Self::Growing { reader, .. } => {
                let target = match pos {
                    SeekFrom::Start(n) => Some(n),
                    SeekFrom::Current(d) => reader.position().checked_add_signed(d),
                    SeekFrom::End(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "seek from the end of a growing file",
                        ));
                    }
                };
                let target = target.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "negative seek offset")
                })?;
                reader.seek(target);
                Ok(target)
            }
        }
    }
}

impl Source for CacheSource {
    fn size(&self) -> Option<u64> {
        match self {
            Self::File { size, .. } => Some(*size),
            Self::Growing { reader, .. } => reader.size(),
        }
    }

    fn abort_handle(&self) -> AbortHandle {
        match self {
            Self::File { .. } => AbortHandle::none(),
            Self::Growing { abort, .. } => abort.clone().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidation_removes_only_alac_files_once() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let keep = [
            "123-aac.m4a",
            "456.mp4",
            "notes.txt",
            "in-progress-alac.m4a.tmp",
        ];
        let drop = ["123-alac.m4a", "789-alac.m4a"];
        for n in keep.iter().chain(&drop) {
            std::fs::write(d.join(n), "x").unwrap();
        }
        invalidate_alac_cache_if_stale(d);
        for n in drop {
            assert!(
                !d.join(n).exists(),
                "{n} from an older decrypt path must be deleted"
            );
        }
        for n in keep {
            assert!(d.join(n).exists(), "{n} must be kept");
        }
        assert_eq!(
            std::fs::read_to_string(d.join(".alac_ver")).unwrap().trim(),
            ALAC_CACHE_VERSION
        );
        // Files committed after the migration survive later starts.
        std::fs::write(d.join("999-alac.m4a"), "x").unwrap();
        invalidate_alac_cache_if_stale(d);
        assert!(
            d.join("999-alac.m4a").exists(),
            "a current-version cache file was deleted on the next start"
        );
    }

    #[test]
    fn invalidation_tolerates_a_missing_dir() {
        invalidate_alac_cache_if_stale(&tempfile::tempdir().unwrap().path().join("absent"));
    }

    #[test]
    fn gate_admits_only_the_current_session() {
        let g = VlcGate::default();
        assert!(!g.owns("") && !g.owns("a"), "a fresh gate owns nothing");
        g.set("a");
        assert!(g.owns("a") && !g.owns("b") && !g.owns(""));
        g.set("b");
        assert!(!g.owns("a"), "a superseded session must be rejected");
        g.clear();
        assert!(!g.owns("b"), "a cleared gate owns nothing");
    }

    // libvlc's input thread reads a growing download: data as it arrives, the end after the
    // commit, and an abort unblocks a pending read.
    #[test]
    fn growing_source_follows_the_download() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = rustet_diskcache::Cache::new(dir.path()).unwrap();
        let (mut put, mut src) = rt.block_on(async {
            let put = cache
                .begin_streaming_put("1", "alac")
                .await
                .unwrap()
                .unwrap();
            let src = CacheSource::growing(put.entry().reader_at(0));
            (put, src)
        });
        rt.block_on(put.write_all(b"hello")).unwrap();
        let mut buf = [0; 16];
        assert_eq!(src.read(&mut buf).unwrap(), 5);
        assert_eq!(src.size(), None, "size unknown while downloading");
        assert_eq!(src.seek(SeekFrom::Current(-2)).unwrap(), 3);
        assert!(src.seek(SeekFrom::End(0)).is_err());
        let writer = std::thread::spawn({
            let handle = rt.handle().clone();
            move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                handle.block_on(async {
                    put.write_all(b" world").await.unwrap();
                    put.commit().await.unwrap();
                });
            }
        });
        let mut got = Vec::new();
        src.read_to_end(&mut got).unwrap();
        writer.join().unwrap();
        assert_eq!(got, b"lo world");
        assert_eq!(src.size(), Some(11));

        // An abort unblocks a pending read with an error, never a clean end.
        let put = rt
            .block_on(cache.begin_streaming_put("2", "alac"))
            .unwrap()
            .unwrap();
        let mut src = rt.block_on(async { CacheSource::growing(put.entry().reader_at(0)) });
        let CacheSource::Growing { abort, .. } = &src else {
            unreachable!()
        };
        let abort = abort.clone();
        let reader = std::thread::spawn(move || src.read(&mut [0; 8]));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!reader.is_finished(), "read returned before the abort");
        abort.cancel();
        assert!(reader.join().unwrap().is_err());

        // An abandoned download is a failed read too.
        let mut src = rt.block_on(async { CacheSource::growing(put.entry().reader_at(0)) });
        drop(put);
        assert!(
            src.read(&mut [0; 8]).is_err(),
            "an abandoned download is a failed read, not an end"
        );
    }
}
