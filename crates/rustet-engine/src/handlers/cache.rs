//! Cache limits, usage and clearing, and the cache-warming jobs. The music-video cache routes
//! are in [`super::mvcache`].

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use rustet_playback::prefetch::CacheConfig;
use serde_json::json;
use tracing::warn;

use crate::http::{ApiError, ApiResult, json, query_param, read_json};
use crate::state::{AppState, Playback};

const MB: i64 = 1024 * 1024;

fn playback(s: &AppState) -> Result<&Playback, ApiError> {
    s.playback
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "playback not available"))
}

/// `PUT /api/v1/cache/config`: the user's cache limits, pushed by the renderer at startup.
pub(crate) async fn config(State(s): State<AppState>, body: Body) -> ApiResult {
    let cfg = read_json::<CacheConfig>(body, 4 << 10)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid request body"))?;
    cfg.validate().map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid cache config: {e}"),
        )
    })?;
    let p = playback(&s)?;
    p.scheduler.set_cache_config(cfg);
    if let Some(dc) = &s.disk_cache {
        // Validated: both are non-negative.
        dc.set_config(
            cfg.persist_limit_mb.unsigned_abs(),
            cfg.persist_ttl_days.unsigned_abs(),
        );
    }
    Ok(json(StatusCode::OK, &p.scheduler.cache_config()))
}

/// `GET /api/v1/cache/stats`: pre-warm and persistent cache usage.
pub(crate) async fn stats(State(s): State<AppState>) -> ApiResult {
    let p = playback(&s)?;
    let cfg = p.scheduler.cache_config();
    // The defaults the settings UI shows when no limit is set.
    let or_default = |mb: i64, default_mb: i64| if mb == 0 { default_mb * MB } else { mb * MB };
    let persistent = match &s.disk_cache {
        Some(dc) => {
            let st = dc.stats().await;
            json!({
                "available": true,
                "sizeBytes": st.total_bytes,
                "limitBytes": or_default(cfg.persist_limit_mb, 500),
                "ttlDays": cfg.persist_ttl_days,
                "count": st.count,
            })
        }
        None => json!({"available": false}),
    };
    Ok(json(
        StatusCode::OK,
        &json!({
            "persistent": persistent,
            "prewarm": {
                "entries": p.scheduler.prewarm_count(),
                "sizeBytes": 0,
                "limitBytes": or_default(cfg.prewarm_limit_mb, 1024),
            },
        }),
    ))
}

/// `DELETE /api/v1/cache/playback?what=prewarm|persistent|segments` (all when absent).
pub(crate) async fn clear_playback(State(s): State<AppState>, uri: Uri) -> ApiResult {
    let what = query_param(&uri, "what");
    let all = what.is_empty();
    if all || what == "prewarm" {
        playback(&s)?.scheduler.clear_pre_warmed();
    }
    if (all || what == "persistent")
        && let Some(dc) = &s.disk_cache
        && let Err(e) = dc.clear().await
    {
        warn!("clear disk cache: {e}");
    }
    if (all || what == "segments")
        && let Err(e) = s.streams.clear_segment_cache().await
    {
        warn!("clear segment cache: {e}");
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /api/v1/jobs/{id}`: a snapshot of a cache-warming job, for debugging and progress UI.
pub(crate) async fn job_status(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let job = playback(&s)?
        .scheduler
        .status(&id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "job not found"))?;
    Ok(json(StatusCode::OK, &job))
}

/// `DELETE /api/v1/jobs/{id}`: cancels a job's remaining work (the renderer navigated away).
pub(crate) async fn job_cancel(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    if !playback(&s)?.scheduler.cancel(&id) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "job not found"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
