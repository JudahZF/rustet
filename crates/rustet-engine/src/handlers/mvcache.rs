//! `/api/v1/cache/mv`: the music-video segment cache switch, capacity and usage, persisted in
//! `~/.cache/rustet/engine/mv-prefs.json`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use rustet_streams::{DEFAULT_MV_CACHE_MAX_BYTES, Streams};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::http::{ApiError, ApiResult, json, read_json};
use crate::state::AppState;

/// The file format of `mv-prefs.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    enabled: bool,
    max_bytes: i64,
}

/// The music-video cache preferences.
pub(crate) struct MvPrefs {
    path: PathBuf,
    /// The last capacity the user asked for, re-applied when the cache is switched back on.
    max_bytes: AtomicI64,
}

impl MvPrefs {
    /// Reads the saved preferences and applies them to `streams`. A missing or unreadable file
    /// keeps the defaults (enabled, 2 GiB).
    pub async fn load(path: PathBuf, streams: &Streams) -> Self {
        let prefs = Self {
            path,
            max_bytes: AtomicI64::new(DEFAULT_MV_CACHE_MAX_BYTES),
        };
        let Ok(data) = tokio::fs::read(&prefs.path).await else {
            return prefs;
        };
        let saved: Saved = match serde_json::from_slice(&data) {
            Ok(s) => s,
            Err(e) => {
                warn!("[mv-cache] prefs parse error: {e}");
                return prefs;
            }
        };
        if saved.max_bytes > 0 {
            prefs.max_bytes.store(saved.max_bytes, Ordering::Relaxed);
        }
        if !saved.enabled {
            streams.set_mv_cache_max_bytes(0);
        } else if saved.max_bytes > 0 {
            streams.set_mv_cache_max_bytes(saved.max_bytes);
        }
        prefs
    }

    /// Turns the cache on (at the remembered capacity) or off, and persists the choice.
    pub async fn set_enabled(&self, streams: &Streams, enabled: bool) {
        streams.set_mv_cache_max_bytes(if enabled {
            self.max_bytes.load(Ordering::Relaxed)
        } else {
            0
        });
        self.save(streams).await;
    }

    /// Sets the capacity in bytes (0 or less disables the cache) and persists it.
    pub async fn set_max_bytes(&self, streams: &Streams, n: i64) {
        if n > 0 {
            self.max_bytes.store(n, Ordering::Relaxed);
        }
        streams.set_mv_cache_max_bytes(n);
        self.save(streams).await;
    }

    async fn save(&self, streams: &Streams) {
        let saved = Saved {
            enabled: streams.mv_cache_enabled(),
            max_bytes: self.max_bytes.load(Ordering::Relaxed),
        };
        let Ok(data) = serde_json::to_vec(&saved) else {
            return;
        };
        let path = self.path.clone();
        let res = tokio::task::spawn_blocking(move || {
            if let Some(dir) = path.parent() {
                let mut b = std::fs::DirBuilder::new();
                b.recursive(true);
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
                let _ = b.create(dir);
            }
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            std::io::Write::write_all(&mut o.open(&path)?, &data)
        })
        .await;
        if let Ok(Err(e)) = res {
            warn!("[mv-cache] prefs save error: {e}");
        }
    }
}

/// The answer of every `/cache/mv` route. Fields in Go's order (it wrote a map: sorted keys).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Info {
    enabled: bool,
    max_bytes: i64,
    quality: String,
    size_bytes: i64,
}

async fn info(s: &AppState) -> Info {
    let streams = s.streams.clone();
    // The first count of the legacy directory touches the disk.
    let size_bytes = tokio::task::spawn_blocking(move || streams.mv_cache_total_bytes())
        .await
        .unwrap_or(0);
    Info {
        enabled: s.streams.mv_cache_enabled(),
        max_bytes: s.streams.mv_cache_max_bytes(),
        size_bytes,
        quality: rustet_ampapi::mvlabel::get(),
    }
}

/// `GET /api/v1/cache/mv`.
pub(crate) async fn get(State(s): State<AppState>) -> ApiResult {
    Ok(json(StatusCode::OK, &info(&s).await))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Update {
    enabled: Option<bool>,
    max_bytes: Option<i64>,
}

/// `PUT /api/v1/cache/mv` with `{"enabled"?: bool, "maxBytes"?: n}`: capacity first, then the
/// switch.
pub(crate) async fn put(State(s): State<AppState>, body: Body) -> ApiResult {
    let u: Update = read_json(body, 4 << 10)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid request body"))?;
    if let Some(n) = u.max_bytes {
        s.mv_prefs.set_max_bytes(&s.streams, n).await;
    }
    if let Some(enabled) = u.enabled {
        s.mv_prefs.set_enabled(&s.streams, enabled).await;
    }
    Ok(json(StatusCode::OK, &info(&s).await))
}

/// `DELETE /api/v1/cache/mv`: deletes the music-video segment cache.
pub(crate) async fn clear(State(s): State<AppState>) -> ApiResult {
    s.streams
        .clear_mv_cache()
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
