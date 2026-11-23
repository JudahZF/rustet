//! `/api/v1/auth`: which tokens the engine holds (spec §5.1 item 8). The UI pushes the web token
//! with `POST /api/v1/library/token`; `auth.expired` is raised when Apple refuses it.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use serde::Serialize;

use crate::http::{ApiError, ApiResult, json};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthState {
    /// A web music user token was pushed.
    web_token: bool,
    /// The FairPlay session has a music token (`MUSIC_TOKEN`).
    fairplay_token: bool,
    /// The storefront the UI pushed, else the FairPlay session's; empty when neither is known.
    storefront: String,
    language: String,
}

/// `GET /api/v1/auth`. No token is ever returned.
pub(crate) async fn get(State(s): State<AppState>) -> ApiResult {
    let t = &s.tokens;
    let state = AuthState {
        web_token: t.has_music_kit(),
        fairplay_token: !t.media_user_token().is_empty(),
        storefront: t.playback_storefront(),
        language: t.language(),
    };
    Ok(json(StatusCode::OK, &state))
}

/// `DELETE /api/v1/auth/token`: sign-out of the web session. The FairPlay sign-out stays
/// `POST /api/v1/drm/logout`.
pub(crate) async fn delete_token(State(s): State<AppState>) -> ApiResult {
    s.tokens.forget_storefront();
    if s.tokens.clear_music_kit() {
        s.notify_player_caps();
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    music_user_token: String,
    /// The account's storefront code (`gb`).
    storefront: String,
}

/// `GET /api/v1/auth/session`: the music user token and storefront, for the native UI's own
/// Apple Music requests. The FairPlay sign-in yields a token Apple's API accepts, so a user
/// who signed in for lossless needs no second sign-in on the web.
///
/// This is the one route that returns a token, so it is closed to every web page: a request
/// with an `Origin` header is refused, also from the origins the guard allows. The same-user
/// peer check at the socket still applies.
pub(crate) async fn session(State(s): State<AppState>, headers: HeaderMap) -> ApiResult {
    if headers.contains_key(header::ORIGIN) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "not available to web pages",
        ));
    }
    let music_user_token = s.tokens.music_user_token();
    if music_user_token.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not signed in"));
    }
    Ok(json(
        StatusCode::OK,
        &Session {
            music_user_token,
            storefront: s.account_storefront().await,
        },
    ))
}
