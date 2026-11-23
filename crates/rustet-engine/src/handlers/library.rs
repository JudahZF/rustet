//! The local library metadata cache: songs, playlists, playlist tracks.
//!
//! Like Android's MediaLibrary: instant queue building from a local store instead of live
//! Apple Music API calls. MusicKit JS owns authentication and fills the store through
//! `POST /api/v1/library/ingest`.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use rustet_library::{
    IngestPayload, LibraryError, LibraryStats, PlaylistTrack, Store, fetch_playlist_tracks_once,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::handlers::catalog::CATALOG_ID;
use crate::http::{ApiError, ApiResult, json, read_json};
use crate::state::AppState;

fn store(s: &AppState) -> Result<&Store, ApiError> {
    s.library.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "library store not initialised",
        )
    })
}

impl From<LibraryError> for ApiError {
    fn from(e: LibraryError) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, e)
    }
}

/// Deprecated: sync is driven by the renderer through `POST /api/v1/library/ingest`.
pub(crate) async fn sync() -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "use Settings → Library → Sync Now (JS-driven sync via /api/v1/library/ingest)",
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    #[serde(flatten)]
    stats: LibraryStats,
    needs_sync: bool,
}

pub(crate) async fn status(State(s): State<AppState>) -> ApiResult {
    let store = store(&s)?;
    let stats = store.stats().await?;
    Ok(json(
        StatusCode::OK,
        &StatusResponse {
            stats,
            needs_sync: store.needs_sync().await,
        },
    ))
}

pub(crate) async fn playlists(State(s): State<AppState>) -> ApiResult {
    let playlists = store(&s)?.playlists().await?;
    Ok(json(StatusCode::OK, &json!({"playlists": playlists})))
}

#[derive(Serialize)]
struct Tracks<'a> {
    tracks: &'a [PlaylistTrack],
    cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The ordered tracks of a playlist, `{"tracks": [{lid, cid}, …], "cached": bool}`. The
/// renderer's `setQueue` wrapper builds `{songs: [...catalogIds]}` from it at once. A playlist
/// that is not cached is fetched live once when the tokens allow it.
pub(crate) async fn playlist_tracks(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult {
    let store = store(&s)?;
    let cached = store.playlist_tracks(&id).await?;
    if !cached.is_empty() {
        return Ok(json(
            StatusCode::OK,
            &Tracks {
                tracks: &cached,
                cached: true,
                error: None,
            },
        ));
    }
    // The live fetch puts the ID into a /v1/me/library path sent with the user's token.
    if !CATALOG_ID.is_match(&id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid playlist id",
        ));
    }
    let mut_token = s.tokens.music_user_token();
    let token = match s.resolve_token().await {
        Ok(t) if !mut_token.is_empty() => t,
        _ => {
            return Ok(json(
                StatusCode::OK,
                &Tracks {
                    tracks: &[],
                    cached: false,
                    error: None,
                },
            ));
        }
    };
    Ok(
        match fetch_playlist_tracks_once(
            &s.http,
            &CancellationToken::new(),
            &token,
            &mut_token,
            &id,
        )
        .await
        {
            Ok(fetched) => {
                if let Err(e) = store.set_playlist_tracks(&id, fetched.clone()).await {
                    warn!("[library] cache playlist {id}: {e}");
                }
                json(
                    StatusCode::OK,
                    &Tracks {
                        tracks: &fetched,
                        cached: false,
                        error: None,
                    },
                )
            }
            Err(e) => {
                warn!("[library] live fetch {id}: {e}");
                json(
                    StatusCode::OK,
                    &Tracks {
                        tracks: &[],
                        cached: false,
                        error: Some(e.to_string()),
                    },
                )
            }
        },
    )
}

/// The ordered tracks of a library album, from the local store. Library album IDs carry an
/// `l.` prefix, which the renderer may leave off.
pub(crate) async fn album_tracks(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let album_id = if id.starts_with("l.") {
        id
    } else {
        format!("l.{id}")
    };
    let tracks = store(&s)?.songs_by_album(&album_id).await?;
    Ok(json(
        StatusCode::OK,
        &Tracks {
            tracks: &tracks,
            cached: true,
            error: None,
        },
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenPush {
    #[serde(default)]
    music_user_token: String,
    #[serde(default)]
    developer_token: String,
    /// The storefront code of the account (`us`), for the player's Apple calls.
    #[serde(default)]
    storefront: String,
    /// The UI's language (`en-GB`); else the request's `Accept-Language`.
    #[serde(default)]
    language: String,
}

/// Stores the MusicKit tokens the renderer pushes at startup, so the engine uses the web
/// session's credentials.
pub(crate) async fn token(State(s): State<AppState>, headers: HeaderMap, body: Body) -> ApiResult {
    // 16 KiB: two JWT-sized tokens.
    let push = read_json::<TokenPush>(body, 16 << 10)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad request"))?;
    let language = if push.language.is_empty() && headers.contains_key(header::ACCEPT_LANGUAGE) {
        crate::http::lang(&headers)
    } else {
        push.language
    };
    s.tokens.set_locale(&push.storefront, &language);
    s.set_music_user_token(&push.music_user_token);
    s.tokens.set_developer(&push.developer_token);
    Ok(json(StatusCode::OK, &json!({"ok": true})))
}

/// Replaces the library with the renderer's sync payload. A body that does not decode is a 400
/// and changes nothing: the renderer's album fallback must not wipe the library.
pub(crate) async fn ingest(State(s): State<AppState>, body: Body) -> ApiResult {
    let store = store(&s)?;
    let payload = read_json::<IngestPayload>(body, 100 << 20)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("bad request: {e}")))?;
    store.ingest(payload).await?;
    Ok(json(StatusCode::OK, &store.stats().await?))
}
