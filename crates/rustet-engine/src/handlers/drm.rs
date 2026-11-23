//! The DRM subsystem: status, sign-in, verification codes, sign-out.
//!
//! The renderer expresses intent (sign in, submit a code); the engine orchestrates and reports
//! progress through `drm` SSE events.

use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rustet_drm::{
    Credentials, DrmManager, DrmSnapshot, Error as DrmError, ServerSignOut, SignInPausedError,
    SignOutResult,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::epoch::DRM_SESSION_TTL;
use crate::http::{ApiError, ApiResult, BodyError, json, read_json};
use crate::state::{AppState, unix_ms};

/// The DRM snapshot plus backend selection and the FairPlay session age, for proactive
/// refreshes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse<'a> {
    #[serde(flatten)]
    snapshot: DrmSnapshot,
    /// False when the build has no DRM layer: the session can never become ready, which the
    /// other fields cannot tell apart from "not ready yet".
    available: bool,
    backend: Backend<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ready_since_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_age_ms: Option<i64>,
    #[serde(rename = "sessionTTLMs", skip_serializing_if = "Option::is_none")]
    session_ttl_ms: Option<i64>,
}

#[derive(Serialize)]
struct Backend<'a> {
    selected: &'a str,
}

pub(crate) async fn status(State(s): State<AppState>) -> Response {
    let since = s.lifecycle.drm_ready_since();
    let resp = StatusResponse {
        snapshot: s.drm_status(),
        available: s.drm.is_some(),
        backend: Backend {
            selected: s.drm.as_ref().map_or("", |d| d.backend_name),
        },
        ready_since_ms: since.map(unix_ms),
        session_age_ms: since.map(|t| {
            i64::try_from(
                SystemTime::now()
                    .duration_since(t)
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(i64::MAX)
        }),
        session_ttl_ms: since.map(|_| DRM_SESSION_TTL.as_millis() as i64),
    };
    json(StatusCode::OK, &resp)
}

/// 4 KiB: an email and a password.
const SMALL_BODY: usize = 4 << 10;

#[derive(Deserialize)]
struct SignIn {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
}

fn manager(s: &AppState) -> Result<&DrmManager, ApiError> {
    s.drm
        .as_ref()
        .map(|d| &d.manager)
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "DRM backend not available"))
}

fn bad_body(_: BodyError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid request body")
}

/// Starts a sign-in and answers 202 at once; progress and the outcome arrive as `drm` events.
pub(crate) async fn authenticate(State(s): State<AppState>, body: Body) -> ApiResult {
    let req = read_json::<SignIn>(body, SMALL_BODY)
        .await
        .map_err(bad_body)?;
    if req.email.is_empty() || req.password.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "email and password required",
        ));
    }
    let Some(drm) = &s.drm else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "DRM backend not available — drm binary not found at startup",
        ));
    };
    let dm = &drm.manager;
    if let Some(left) = dm.sign_in_paused_for() {
        let until = time::OffsetDateTime::now_utc() + left;
        let mut resp = ApiError::new(StatusCode::TOO_MANY_REQUESTS, SignInPausedError { until })
            .into_response();
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(left.as_secs() + 1));
        return Ok(resp);
    }
    if dm.sign_in_running() {
        return Err(ApiError::new(StatusCode::CONFLICT, DrmError::SignInRunning));
    }
    // Its own task, not the request's: the request ends with the 202, and the sign-in must
    // run to completion (the manager's shutdown still ends it).
    let task = dm.clone();
    tokio::spawn(async move {
        if let Err(e) = task
            .authenticate(Credentials {
                email: req.email,
                password: req.password,
            })
            .await
        {
            error!("drm login error: {e}");
        }
    });
    Ok(json(StatusCode::ACCEPTED, &dm.status()))
}

#[derive(Deserialize)]
struct Challenge {
    #[serde(default)]
    reply: String,
}

pub(crate) async fn challenge(State(s): State<AppState>, body: Body) -> ApiResult {
    let req = read_json::<Challenge>(body, SMALL_BODY)
        .await
        .map_err(bad_body)?;
    if req.reply.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "reply required"));
    }
    let dm = manager(&s)?;
    dm.submit_challenge(req.reply)
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, e))?;
    Ok(json(StatusCode::OK, &dm.status()))
}

/// `POST /drm/logout` and `DELETE /drm/session`: the same full sign-out. Clearing the files
/// while the backend runs would leave the account signed in inside the Android libraries.
pub(crate) async fn logout(State(s): State<AppState>) -> ApiResult {
    let dm = manager(&s)?;
    s.tokens.forget_storefront();
    Ok(sign_out(dm, dm.logout().await))
}

pub(crate) async fn forget_device(State(s): State<AppState>) -> ApiResult {
    let dm = manager(&s)?;
    s.tokens.forget_storefront();
    Ok(sign_out(dm, dm.forget_device().await))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignOutResponse {
    #[serde(flatten)]
    snapshot: DrmSnapshot,
    server_sign_out: ServerSignOut,
}

/// How long the answer gets to reach the client before a restart ends the process.
const RESTART_GRACE: Duration = Duration::from_millis(200);
/// The exit code that tells the shell to restart the engine.
const RESTART_EXIT_CODE: i32 = 3;

/// Answers a sign-out. When it must finish in the next process, the answer goes out first and
/// then the engine exits; the shell restarts it and the new process completes the recorded
/// sign-out.
fn sign_out(dm: &DrmManager, result: Result<SignOutResult, DrmError>) -> Response {
    let (status, res) = match &result {
        Ok(res) => (StatusCode::OK, *res),
        Err(e) => {
            error!("drm sign-out: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, SignOutResult::default())
        }
    };
    if matches!(result, Err(DrmError::RestartRequired(_))) {
        tokio::spawn(async {
            tokio::time::sleep(RESTART_GRACE).await;
            std::process::exit(RESTART_EXIT_CODE);
        });
    }
    json(
        status,
        &SignOutResponse {
            snapshot: dm.status(),
            server_sign_out: res.server_sign_out,
        },
    )
}
