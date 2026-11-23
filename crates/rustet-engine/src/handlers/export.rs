//! `/api/v1/export…`: the export queue, and the folders exports may write into.

use std::ffi::OsStr;
use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use rustet_export::{ExportRequest, Manager, PrioritizeError};
use serde::Deserialize;

use crate::http::{ApiError, ApiResult, json, lang, read_json};
use crate::state::AppState;

fn manager(s: &AppState) -> Result<&Manager, ApiError> {
    s.export
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "export not available"))
}

fn job_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "job not found")
}

/// The folders exports may write into. The request that names the output folder comes from a
/// web page, so by default it is confined to the user's home; `RUSTET_EXPORT_ROOTS` (a
/// platform path list) replaces that.
pub(crate) fn export_roots() -> Vec<PathBuf> {
    roots_from(
        std::env::var_os("RUSTET_EXPORT_ROOTS").as_deref(),
        std::env::home_dir(),
    )
}

fn roots_from(env: Option<&OsStr>, home: Option<PathBuf>) -> Vec<PathBuf> {
    match env.filter(|v| !v.is_empty()) {
        Some(v) => std::env::split_paths(v).collect(),
        None => match home {
            Some(home) => vec![home],
            // No home to anchor to: refuse rather than allow everything.
            None => vec![PathBuf::from("/nonexistent")],
        },
    }
}

/// The folders the user chose in the app's native folder dialog. The shell writes them, one
/// per line, to the file named by `RUSTET_EXPORT_PICKED`; the page cannot write it, so it cannot
/// widen the roots itself. Read on every check, so a folder picked after start counts.
pub(crate) fn picked_export_roots() -> Vec<PathBuf> {
    picked_from(std::env::var_os("RUSTET_EXPORT_PICKED").as_deref())
}

fn picked_from(file: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(file) = file.filter(|f| !f.is_empty()) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect()
}

/// `POST /api/v1/export`: queues an export (202 with the job).
pub(crate) async fn create(State(s): State<AppState>, headers: HeaderMap, body: Body) -> ApiResult {
    let m = manager(&s)?;
    let bad = || ApiError::new(StatusCode::BAD_REQUEST, "invalid request body");
    let bytes = axum::body::to_bytes(body, 64 << 10)
        .await
        .map_err(|_| bad())?;
    let mut req = ExportRequest::from_json(&bytes).map_err(|_| bad())?;
    // Request tokens (the renderer's) win over the engine's own. The native UI sends none.
    if req.token.is_empty() {
        req.token = s.resolve_token().await.unwrap_or_default();
    }
    if req.mut_token.is_empty() {
        req.mut_token = s.tokens.music_user_token();
    }
    if req.token.is_empty() || req.mut_token.is_empty() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "not signed in"));
    }
    s.tokens.set_developer(&req.token);
    if req.storefront.is_empty() {
        req.storefront = s.account_storefront().await;
    }
    if req.language.is_empty() {
        req.language = lang(&headers);
    }
    let job = m
        .enqueue(req)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    Ok(json(StatusCode::ACCEPTED, &job))
}

/// `GET /api/v1/export`: every job.
pub(crate) async fn list(State(s): State<AppState>) -> ApiResult {
    Ok(json(StatusCode::OK, &manager(&s)?.list()))
}

/// `GET /api/v1/export/{id}`.
pub(crate) async fn get(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(json(
        StatusCode::OK,
        &manager(&s)?.get(&id).ok_or_else(job_not_found)?,
    ))
}

/// `DELETE /api/v1/export/{id}`: cancels a queued or running job, removes a finished one.
pub(crate) async fn cancel(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    if !manager(&s)?.cancel(&id) {
        return Err(job_not_found());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/v1/export/{id}/retry`: requeues a failed or cancelled job with current tokens.
pub(crate) async fn retry(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let m = manager(&s)?;
    let (token, mut_token) = (
        s.resolve_token().await.unwrap_or_default(),
        s.tokens.music_user_token(),
    );
    if token.is_empty() || mut_token.is_empty() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "not signed in"));
    }
    match m.retry(&id, &token, &mut_token) {
        Some(job) => Ok(json(StatusCode::ACCEPTED, &job)),
        None if m.get(&id).is_none() => Err(job_not_found()),
        None => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "job is not in a retryable state (must be failed or cancelled)",
        )),
    }
}

#[derive(Deserialize)]
struct PriorityBody {
    priority: Option<i32>,
}

/// `POST /api/v1/export/{id}/priority` with `{"priority": N}`: higher runs sooner, equal
/// priorities stay FIFO. 404 unknown job, 409 running or finished.
pub(crate) async fn priority(
    State(s): State<AppState>,
    Path(id): Path<String>,
    body: Body,
) -> ApiResult {
    let m = manager(&s)?;
    let priority = read_json::<PriorityBody>(body, 1 << 10)
        .await
        .ok()
        .and_then(|b| b.priority)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                r#"invalid request body: want {"priority": N}"#,
            )
        })?;
    match m.prioritize(&id, priority) {
        Ok(job) => Ok(json(StatusCode::OK, &job)),
        Err(PrioritizeError::NotFound) => Err(job_not_found()),
        Err(PrioritizeError::NotQueued) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "job is not queued (already running or finished)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picked_roots_keep_only_absolute_lines() {
        let dir = tempfile::tempdir().unwrap();
        let abs = dir.path().join("picked");
        let f = dir.path().join("export-folders.txt");
        std::fs::write(&f, format!("{}\n\nrelative/dir\n  \n", abs.display())).unwrap();
        assert_eq!(picked_from(Some(f.as_os_str())), [abs]);
        assert!(picked_from(Some(dir.path().join("missing").as_os_str())).is_empty());
        assert!(picked_from(Some(OsStr::new(""))).is_empty());
        assert!(picked_from(None).is_empty());
    }

    #[test]
    fn roots_default_to_home_and_never_to_everything() {
        let home = PathBuf::from("/home/u");
        assert_eq!(
            roots_from(None, Some(home.clone())),
            std::slice::from_ref(&home)
        );
        assert_eq!(
            roots_from(Some(OsStr::new("")), Some(home)),
            [PathBuf::from("/home/u")]
        );
        assert_eq!(roots_from(None, None), [PathBuf::from("/nonexistent")]);
        let list = std::env::join_paths(["/a", "/b"]).unwrap();
        assert_eq!(
            roots_from(Some(&list), None),
            [PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }
}
