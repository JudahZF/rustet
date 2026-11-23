//! Personalised feeds. Deliberately dormant: nothing in the bundled renderer calls them. They
//! let other applications built on the engine show Listen Now, heavy rotation and recently
//! played without re-implementing Apple's authentication.
//!
//! Query: `limit` (1–50), `offset` (0–10000), `types` (comma-separated resource types). Needs
//! a Music-User-Token (pushed by the renderer or read from the DRM session); 401 otherwise.

use std::sync::LazyLock;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use regex::Regex;
use rustet_ampapi::{Error as AmpError, RecommendationKind, RecommendationsOptions};
use tokio_util::sync::CancellationToken;

use crate::http::{lang, query_param, text_error};
use crate::state::AppState;

static TYPES_FILTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z-]{1,32}(,[a-z-]{1,32}){0,9}$").expect("valid regex"));

/// A numeric query value in `range`; `Ok(None)` when absent.
fn bounded(uri: &Uri, key: &str, range: std::ops::RangeInclusive<u32>) -> Result<Option<u32>, ()> {
    let v = query_param(uri, key);
    if v.is_empty() {
        return Ok(None);
    }
    match v.parse::<i64>() {
        Ok(n) if n >= i64::from(*range.start()) && n <= i64::from(*range.end()) => {
            Ok(Some(n as u32))
        }
        _ => Err(()),
    }
}

/// One feed; the router binds `kind` per route.
pub(crate) async fn feed(
    kind: RecommendationKind,
    State(s): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let Ok(limit) = bounded(&uri, "limit", 1..=50) else {
        return text_error(StatusCode::BAD_REQUEST, "limit must be 1-50");
    };
    let Ok(offset) = bounded(&uri, "offset", 0..=10_000) else {
        return text_error(StatusCode::BAD_REQUEST, "invalid offset");
    };
    let types = query_param(&uri, "types");
    if !types.is_empty() && !TYPES_FILTER.is_match(&types) {
        return text_error(StatusCode::BAD_REQUEST, "invalid types filter");
    }
    let options = RecommendationsOptions {
        language: Some(lang(&headers)),
        limit,
        offset,
        types: (!types.is_empty()).then_some(types),
    };

    let mut_token = s.tokens.music_user_token();
    if mut_token.is_empty() {
        return text_error(
            StatusCode::UNAUTHORIZED,
            "no Music-User-Token available; sign in first",
        );
    }
    match s
        .amp
        .recommendations(
            &CancellationToken::new(),
            kind,
            &s.tokens.developer(),
            &mut_token,
            &options,
        )
        .await
    {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            body,
        )
            .into_response(),
        Err(AmpError::Feed { status, .. }) if matches!(status.as_u16(), 401 | 403) => text_error(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            "apple music rejected the user token",
        ),
        Err(e) => text_error(
            StatusCode::BAD_GATEWAY,
            format!("recommendations fetch failed: {e}"),
        ),
    }
}
