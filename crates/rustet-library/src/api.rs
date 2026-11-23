//! Apple Music API item types (used by [`IngestPayload`](crate::IngestPayload)) and the
//! live playlist fetch.

use std::collections::HashSet;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::PlaylistTrack;
use crate::disk::nullable;

const API_BASE: &str = "https://amp-api.music.apple.com";
const PAGE_LIMIT: u32 = 100;
/// Bounds pagination: 100 tracks per page covers far more than any real playlist.
const MAX_PLAYLIST_PAGES: usize = 1000;

/// One library resource (song, album, playlist or playlist entry) as the Apple Music
/// API returns it. Only the fields the cache uses are kept.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AmItem {
    #[serde(deserialize_with = "nullable")]
    pub(crate) id: String,
    #[serde(deserialize_with = "nullable")]
    pub(crate) attributes: AmAttributes,
    #[serde(deserialize_with = "nullable")]
    pub(crate) relationships: AmRelationships,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct AmAttributes {
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub artist_name: String,
    #[serde(deserialize_with = "nullable")]
    pub album_name: String,
    #[serde(deserialize_with = "nullable")]
    pub duration_in_millis: u64,
    #[serde(deserialize_with = "nullable")]
    pub track_count: u32,
    #[serde(deserialize_with = "nullable")]
    pub track_number: u32,
    #[serde(deserialize_with = "nullable")]
    pub disc_number: u32,
    #[serde(deserialize_with = "nullable")]
    pub play_params: AmPlayParams,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct AmPlayParams {
    #[serde(deserialize_with = "nullable")]
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    pub catalog_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct AmRelationships {
    #[serde(deserialize_with = "nullable")]
    pub albums: AmRelation,
    #[serde(deserialize_with = "nullable")]
    pub catalog: AmRelation,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct AmRelation {
    #[serde(deserialize_with = "nullable")]
    pub data: Vec<AmRef>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct AmRef {
    #[serde(deserialize_with = "nullable")]
    pub id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AmResponse {
    #[serde(deserialize_with = "nullable")]
    data: Vec<AmItem>,
    #[serde(deserialize_with = "nullable")]
    next: String,
}

impl AmItem {
    /// The catalog ID from `playParams`: `catalogId`, else `id`.
    pub(crate) fn play_params_catalog_id(&self) -> &str {
        let pp = &self.attributes.play_params;
        if pp.catalog_id.is_empty() {
            &pp.id
        } else {
            &pp.catalog_id
        }
    }

    /// [`Self::play_params_catalog_id`], falling back to the `catalog` relationship.
    pub(crate) fn catalog_id(&self) -> &str {
        match self.play_params_catalog_id() {
            "" => self
                .relationships
                .catalog
                .data
                .first()
                .map_or("", |r| r.id.as_str()),
            cid => cid,
        }
    }
}

/// Errors from [`fetch_playlist_tracks_once`].
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("request: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API {path}: {status}")]
    Status {
        path: String,
        status: reqwest::StatusCode,
    },
    /// The server repeated a `next` cursor or never stopped paging.
    #[error("playlist pagination did not terminate (page {page})")]
    Pagination { page: usize },
    #[error("cancelled")]
    Cancelled,
}

/// Fetches one library playlist's ordered track list straight from the Apple Music API,
/// without a full sync. The live fallback when a playlist is not cached yet.
///
/// `token` is the developer JWT, `music_user_token` the MusicKit user token. The caller
/// must have checked that `playlist_id` is safe to put in a URL path.
pub async fn fetch_playlist_tracks_once(
    client: &reqwest::Client,
    cancel: &CancellationToken,
    token: &str,
    music_user_token: &str,
    playlist_id: &str,
) -> Result<Vec<PlaylistTrack>, FetchError> {
    fetch_playlist_tracks(
        client,
        cancel,
        API_BASE,
        token,
        music_user_token,
        playlist_id,
    )
    .await
}

pub(crate) async fn fetch_playlist_tracks(
    client: &reqwest::Client,
    cancel: &CancellationToken,
    base: &str,
    token: &str,
    music_user_token: &str,
    playlist_id: &str,
) -> Result<Vec<PlaylistTrack>, FetchError> {
    let mut all = Vec::new();
    let mut path = format!("/v1/me/library/playlists/{playlist_id}/tracks");
    let mut seen = HashSet::new();
    for page in 0.. {
        // A server that repeats a cursor, or never ends, must not spin forever.
        if page >= MAX_PLAYLIST_PAGES || !seen.insert(path.clone()) {
            return Err(FetchError::Pagination { page });
        }
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else {
            format!("{base}{path}")
        };
        let mut req = client
            .get(url)
            .bearer_auth(token)
            .header("Music-User-Token", music_user_token);
        if page == 0 {
            req = req.query(&[("limit", PAGE_LIMIT)]);
        }
        let fetch = async {
            let resp = req.send().await?;
            let status = resp.status();
            if status.is_client_error() || status.is_server_error() {
                return Err(FetchError::Status {
                    path: path.clone(),
                    status,
                });
            }
            Ok(resp.json::<AmResponse>().await?)
        };
        let resp = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(FetchError::Cancelled),
            resp = fetch => resp?,
        };
        all.extend(resp.data.iter().map(|item| PlaylistTrack {
            library_id: item.id.clone(),
            catalog_id: item.play_params_catalog_id().to_owned(),
        }));
        if resp.next.is_empty() {
            break;
        }
        // Apple's next URL omits the limit param on page 2+; re-attach it.
        path = if resp.next.contains("limit=") {
            resp.next
        } else {
            format!("{}&limit={PAGE_LIMIT}", resp.next)
        };
    }
    Ok(all)
}
