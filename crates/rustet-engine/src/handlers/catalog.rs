//! Catalog entity endpoints for the renderer: proxies of the Apple Music catalog API.

use std::sync::LazyLock;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::IntoResponse;
use regex::Regex;

use crate::http::{ApiError, ApiResult, json, lang, query_param};
use crate::state::AppState;

/// Apple catalog and library IDs and storefront codes. These values go into Apple Music API
/// URL paths, so anything else (`../`, `?`, `#`) is rejected rather than forwarded with the
/// user's tokens. The first character may not be a dot, which also rejects `.` and `..`.
pub(crate) static CATALOG_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$").expect("valid regex"));

/// The browser identity Apple's API expects from the web player.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

/// Deadline of the requests these handlers proxy: a slow CDN must not hang them.
pub(crate) const PROXY_TIMEOUT: Duration = Duration::from_secs(10);

/// A validated storefront and ID.
pub(crate) struct CatalogParams {
    pub storefront: String,
    pub id: String,
}

/// The storefront (query `sf`, then what the UI pushed, then the session's, then `us`) and the `{id}` path value, both
/// validated; a 400 when either is malformed.
pub(crate) fn catalog_params(
    s: &AppState,
    uri: &Uri,
    id: String,
) -> Result<CatalogParams, ApiError> {
    let mut storefront = query_param(uri, "sf");
    if storefront.is_empty() {
        storefront = s.tokens.playback_storefront();
    }
    if storefront.is_empty() {
        storefront = "us".to_owned();
    }
    if !CATALOG_ID.is_match(&id) || !CATALOG_ID.is_match(&storefront) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid id or storefront",
        ));
    }
    Ok(CatalogParams { storefront, id })
}

pub(crate) async fn album(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let p = catalog_params(&s, &uri, id)?;
    let album = s
        .amp
        .catalog_album(&p.storefront, &p.id, &lang(&headers), &s.tokens.developer())
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("album fetch failed: {e}")))?;
    Ok(json(StatusCode::OK, &album))
}

pub(crate) async fn playlist(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let p = catalog_params(&s, &uri, id)?;
    let playlist = s
        .amp
        .catalog_playlist(&p.storefront, &p.id, &lang(&headers), &s.tokens.developer())
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("playlist fetch failed: {e}"),
            )
        })?;
    Ok(json(StatusCode::OK, &playlist))
}

/// An artist with its albums and songs. Apple's answer, status and body, is passed through.
pub(crate) async fn artist(
    State(s): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let p = catalog_params(&s, &uri, id)?;
    let up = s
        .http
        .get(format!(
            "{}/v1/catalog/{}/artists/{}",
            s.amp_base, p.storefront, p.id
        ))
        .timeout(PROXY_TIMEOUT)
        .bearer_auth(s.tokens.developer())
        .header(header::USER_AGENT, BROWSER_USER_AGENT)
        .header(header::ORIGIN, "https://music.apple.com")
        .query(&[
            ("include", "albums,songs"),
            ("l", &lang(&headers)),
            ("limit[albums]", "20"),
            ("limit[songs]", "10"),
        ])
        .send()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("artist fetch failed: {e}")))?;
    Ok((
        StatusCode::from_u16(up.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        Body::from_stream(up.bytes_stream()),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_id_pattern() {
        for (s, want) in [
            ("1440935467", true),
            ("pl.u-abc123", true),
            ("p.xyz", true),
            ("i.AbC_9", true),
            ("us", true),
            ("a..b", true),
            ("", false),
            (".", false),
            ("..", false),
            (".hidden", false),
            ("a/b", false),
            ("a?b", false),
            ("a#b", false),
            ("a b", false),
        ] {
            assert_eq!(CATALOG_ID.is_match(s), want, "{s:?}");
        }
        assert!(!CATALOG_ID.is_match(&"a".repeat(65)));
    }
}
