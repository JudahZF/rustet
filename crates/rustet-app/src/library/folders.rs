//! The user's playlist folders (the sidebar's tree), loaded one folder at a time from Apple.
//!
//! `GET /v1/me/library/playlist-folders/{id}/children` lists a folder's playlists and
//! subfolders; the top level is the folder `p.playlistsroot`. A real library had 44 folders,
//! so the tree is loaded lazily: the top level first, a folder when it is opened.

use rustet_ampapi::Api;
use rustet_ampapi::model::Resource;
use tokio_util::sync::CancellationToken;

use crate::pages::LoadError;
use crate::pages::map::artwork;
use crate::pages::model::Artwork;
use crate::pages::more::with_params;
use crate::route::Route;

/// What a `next` link of a folder must keep (the first request's parameters).
const FOLDER_PARAMS: &[(&str, &str)] = &[
    ("include", "catalog"),
    ("omit[resource]", "autos"),
    ("platform", "web"),
];
/// Bounds the pages of one folder.
const MAX_PAGES: usize = 50;

/// An entry of a playlist folder.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaylistEntry {
    /// A folder; open it with [`playlist_folder`] and its `id`.
    Folder { id: String, name: String },
    Playlist {
        id: String,
        name: String,
        artwork: Option<Artwork>,
        /// The user may rename it and change its tracks (a drop target in the sidebar).
        editable: bool,
    },
}

impl PlaylistEntry {
    /// Where a click goes; `None` for a folder, which opens in place.
    pub fn route(&self) -> Option<Route> {
        match self {
            Self::Folder { .. } => None,
            Self::Playlist { id, .. } => Some(Route::LibraryPlaylist { id: id.clone() }),
        }
    }
}

fn entry(resource: &Resource) -> Option<PlaylistEntry> {
    match resource {
        Resource::LibraryPlaylistFolder(f) => Some(PlaylistEntry::Folder {
            id: f.id.clone(),
            name: f.attributes.as_ref()?.name.clone(),
        }),
        Resource::LibraryPlaylist(p) => {
            let a = p.attributes.as_ref()?;
            Some(PlaylistEntry::Playlist {
                id: p.id.clone(),
                name: a.name.clone(),
                artwork: artwork(a.artwork.as_ref()),
                editable: a.can_edit,
            })
        }
        _ => None,
    }
}

/// The entries of `folder` (`None`: the top level), in Apple's order, every page. Needs the
/// music user token.
pub async fn playlist_folder(
    api: &Api,
    folder: Option<&str>,
) -> Result<Vec<PlaylistEntry>, LoadError> {
    if !api.is_signed_in() {
        return Err(LoadError::SignInRequired);
    }
    let cancel = CancellationToken::new();
    let mut page = match api.library_playlist_folder(&cancel, folder).await {
        // An empty folder answers 404.
        Err(rustet_ampapi::Error::NotFound) => return Ok(Vec::new()),
        other => other?,
    };
    let mut entries: Vec<PlaylistEntry> = page.data.iter().filter_map(entry).collect();
    for _ in 1..MAX_PAGES {
        let Some(next) = page.next.take() else { break };
        page = api
            .next_page(&cancel, &with_params(&next, FOLDER_PARAMS))
            .await?;
        entries.extend(page.data.iter().filter_map(entry));
    }
    Ok(entries)
}
