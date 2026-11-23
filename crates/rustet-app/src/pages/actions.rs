//! What the user can do to an item besides playing it: library and playlist edits, ratings,
//! and the context menu that offers them (spec §6.2 `ContextMenu`).
//!
//! Every [`Action`] needs the music user token. The requests (`rustet-ampapi`'s `/v1/me/…`
//! writes) come from the web player's code and Apple's documentation; they are checked only by
//! the `#[ignore]` signed-in test.

use rustet_ampapi::Api;
use rustet_ampapi::api::{NewPlaylist, PlaylistEdit, Rating, ResourceRef, ResourceType};
use tokio_util::sync::CancellationToken;

use super::error::LoadError;
use super::model::{Tile, TileKind, TrackRow};
use super::play::PlayTarget;
use crate::route::Route;

/// A change to the user's library or ratings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Adds catalog items (songs, albums, playlists, music videos).
    AddToLibrary(Vec<ResourceRef>),
    /// Removes a library item (`library-songs`, `library-albums`, …, with its library ID).
    RemoveFromLibrary(ResourceRef),
    /// Loves or dislikes an item; `None` clears the rating.
    Rate {
        item: ResourceRef,
        rating: Option<Rating>,
    },
    /// Appends songs (catalog or library) to a library playlist.
    AddToPlaylist {
        playlist_id: String,
        tracks: Vec<ResourceRef>,
    },
    CreatePlaylist {
        name: String,
        description: Option<String>,
        tracks: Vec<ResourceRef>,
    },
    /// Renames a library playlist or changes its description; `None` keeps a value.
    EditPlaylist {
        id: String,
        name: Option<String>,
        description: Option<String>,
    },
    DeletePlaylist {
        id: String,
    },
    /// Replaces a library playlist's tracks with `tracks` in this order (reorder).
    SetPlaylistOrder {
        id: String,
        tracks: Vec<ResourceRef>,
    },
    /// Removes every occurrence of a library song from a library playlist.
    RemoveFromPlaylist {
        id: String,
        library_song_id: String,
    },
}

/// A finished action: the toast to show and, for a new playlist, its library ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub toast: String,
    pub created: Option<String>,
}

/// A failed action: the toast says what failed and what to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{toast}")]
pub struct ActionError {
    pub toast: String,
    pub cause: LoadError,
}

impl Action {
    /// What the action does, after "Could not …".
    fn verb(&self) -> &'static str {
        match self {
            Self::AddToLibrary(_) => "add to your library",
            Self::RemoveFromLibrary(_) => "remove from your library",
            Self::Rate {
                rating: Some(Rating::Love),
                ..
            } => "love this",
            Self::Rate {
                rating: Some(Rating::Dislike),
                ..
            } => "dislike this",
            Self::Rate { rating: None, .. } => "remove the rating",
            Self::AddToPlaylist { .. } => "add to the playlist",
            Self::CreatePlaylist { .. } => "create the playlist",
            Self::EditPlaylist { .. } => "change the playlist",
            Self::DeletePlaylist { .. } => "delete the playlist",
            Self::SetPlaylistOrder { .. } => "reorder the playlist",
            Self::RemoveFromPlaylist { .. } => "remove from the playlist",
        }
    }

    /// Whether the page of `route` shows something this action changed, so a cached copy is
    /// stale. Ratings are drawn on no page.
    pub fn changes(&self, route: &Route) -> bool {
        use crate::route::LibrarySection;
        let playlist = |id: &str| match route {
            Route::LibraryPlaylist { id: shown } => shown == id,
            Route::Library(LibrarySection::Playlists) => true,
            _ => false,
        };
        match self {
            Self::AddToLibrary(_) | Self::RemoveFromLibrary(_) => matches!(
                route,
                Route::Library(_)
                    | Route::LibrarySearch { .. }
                    | Route::LibraryAlbum { .. }
                    | Route::LibraryPlaylist { .. }
            ),
            Self::Rate { .. } => false,
            Self::CreatePlaylist { .. } => {
                matches!(route, Route::Library(LibrarySection::Playlists))
            }
            Self::AddToPlaylist {
                playlist_id: id, ..
            }
            | Self::EditPlaylist { id, .. }
            | Self::DeletePlaylist { id }
            | Self::SetPlaylistOrder { id, .. }
            | Self::RemoveFromPlaylist { id, .. } => playlist(id),
        }
    }

    fn done(&self) -> &'static str {
        match self {
            Self::AddToLibrary(_) => "Added to Library",
            Self::RemoveFromLibrary(_) => "Removed from Library",
            Self::Rate {
                rating: Some(Rating::Love),
                ..
            } => "Loved",
            Self::Rate {
                rating: Some(Rating::Dislike),
                ..
            } => "Disliked",
            Self::Rate { rating: None, .. } => "Rating removed",
            Self::AddToPlaylist { .. } => "Added to Playlist",
            Self::CreatePlaylist { .. } => "Playlist created",
            Self::EditPlaylist { .. } => "Playlist changed",
            Self::DeletePlaylist { .. } => "Playlist deleted",
            Self::SetPlaylistOrder { .. } => "Playlist reordered",
            Self::RemoveFromPlaylist { .. } => "Removed from Playlist",
        }
    }
}

/// The catalog type the web player puts in a library removal path
/// (`DELETE /v1/me/library/songs/i.…`).
fn removal_type(kind: ResourceType) -> ResourceType {
    match kind {
        ResourceType::LibrarySongs => ResourceType::Songs,
        ResourceType::LibraryAlbums => ResourceType::Albums,
        ResourceType::LibraryArtists => ResourceType::Artists,
        ResourceType::LibraryPlaylists => ResourceType::Playlists,
        ResourceType::LibraryMusicVideos => ResourceType::MusicVideos,
        other => other,
    }
}

/// Runs `action`. Signed out it fails without a request.
pub async fn perform(api: &Api, action: &Action) -> Result<Outcome, ActionError> {
    let fail = |cause: LoadError| {
        let hint = match &cause {
            LoadError::SignInRequired => "Sign in to Apple Music, then try again.",
            LoadError::NotFound => "The item is no longer there.",
            LoadError::Offline(_) => "Check the internet connection, then try again.",
            LoadError::RateLimited => "Wait a minute, then try again.",
            _ => "Try again later.",
        };
        ActionError {
            toast: format!("Could not {}. {hint}", action.verb()),
            cause,
        }
    };
    if !api.is_signed_in() {
        return Err(fail(LoadError::SignInRequired));
    }
    let cancel = CancellationToken::new();
    let mut created = None;
    let result = match action {
        Action::AddToLibrary(items) => api.add_to_library(&cancel, items).await,
        Action::RemoveFromLibrary(item) => {
            let item = ResourceRef::new(removal_type(item.kind), item.id.clone());
            api.remove_from_library(&cancel, &item).await
        }
        Action::Rate {
            item,
            rating: Some(rating),
        } => api.set_rating(&cancel, item, *rating).await,
        Action::Rate { item, rating: None } => api.clear_rating(&cancel, item).await,
        Action::AddToPlaylist {
            playlist_id,
            tracks,
        } => api.add_playlist_tracks(&cancel, playlist_id, tracks).await,
        Action::CreatePlaylist {
            name,
            description,
            tracks,
        } => {
            let playlist = NewPlaylist {
                name: name.clone(),
                description: description.clone(),
                tracks: tracks.clone(),
            };
            api.create_playlist(&cancel, &playlist)
                .await
                .map(|p| created = Some(p.id))
        }
        Action::EditPlaylist {
            id,
            name,
            description,
        } => {
            let edit = PlaylistEdit {
                name: name.clone(),
                description: description.clone(),
            };
            api.edit_playlist(&cancel, id, &edit).await
        }
        Action::DeletePlaylist { id } => {
            let item = ResourceRef::new(ResourceType::Playlists, id.clone());
            api.remove_from_library(&cancel, &item).await
        }
        Action::SetPlaylistOrder { id, tracks } => {
            api.set_playlist_tracks(&cancel, id, tracks).await
        }
        Action::RemoveFromPlaylist {
            id,
            library_song_id,
        } => {
            api.remove_playlist_track(&cancel, id, library_song_id)
                .await
        }
    };
    result.map_err(|err| fail(err.into()))?;
    Ok(Outcome {
        toast: action.done().to_owned(),
        created,
    })
}

/// An entry of a context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    PlayNext,
    PlayLater,
    AddToLibrary(ResourceRef),
    RemoveFromLibrary(ResourceRef),
    /// Opens the playlist picker for these songs.
    AddToPlaylist(ResourceRef),
    Love(ResourceRef),
    Dislike(ResourceRef),
    GoToAlbum(Route),
    GoToArtist(Route),
    /// Exports the item (the downloads panel decides the format).
    Download(ResourceRef),
    /// Copies this music.apple.com link.
    ShareLink(String),
    /// Removes the row from the library playlist being shown.
    RemoveFromPlaylist {
        playlist_id: String,
        library_song_id: String,
    },
}

/// Where a menu opens and who is asking.
#[derive(Debug, Clone, Copy)]
pub struct MenuContext<'a> {
    pub signed_in: bool,
    /// The storefront for share links.
    pub storefront: &'a str,
    /// The editable library playlist the row is shown in.
    pub playlist: Option<&'a str>,
}

/// The context menu of a track row.
pub fn track_menu(row: &TrackRow, ctx: MenuContext<'_>) -> Vec<MenuItem> {
    let mut menu = Vec::new();
    if row.is_playable() {
        menu.extend([MenuItem::PlayNext, MenuItem::PlayLater]);
    }
    let catalog = row.catalog_ref();
    let library = row.library_ref();
    if ctx.signed_in {
        match (&library, &catalog) {
            (Some(library), _) => menu.push(MenuItem::RemoveFromLibrary(library.clone())),
            (None, Some(catalog)) => menu.push(MenuItem::AddToLibrary(catalog.clone())),
            (None, None) => {}
        }
        if let Some(song) = library.clone().or_else(|| catalog.clone()) {
            menu.push(MenuItem::AddToPlaylist(song.clone()));
            menu.push(MenuItem::Love(song.clone()));
            menu.push(MenuItem::Dislike(song));
        }
    }
    if let Some(route) = row.album.as_ref().and_then(|a| a.route.clone()) {
        menu.push(MenuItem::GoToAlbum(route));
    }
    if let Some(route) = row.artist.route.clone() {
        menu.push(MenuItem::GoToArtist(route));
    }
    if let Some(catalog) = catalog.filter(|_| row.is_playable()) {
        menu.push(MenuItem::Download(catalog.clone()));
        let route = match row.kind {
            super::model::TrackKind::Song => Route::Song { id: catalog.id },
            super::model::TrackKind::MusicVideo => Route::MusicVideo { id: catalog.id },
        };
        menu.extend(route.share_link(ctx.storefront).map(MenuItem::ShareLink));
    }
    if ctx.signed_in
        && let (Some(playlist), Some(library)) = (ctx.playlist, library)
    {
        menu.push(MenuItem::RemoveFromPlaylist {
            playlist_id: playlist.to_owned(),
            library_song_id: library.id,
        });
    }
    menu
}

/// The context menu of a tile.
pub fn tile_menu(tile: &Tile, ctx: MenuContext<'_>) -> Vec<MenuItem> {
    let mut menu = Vec::new();
    if tile
        .play
        .as_ref()
        .is_some_and(|p| !matches!(p, PlayTarget::Station(_)))
    {
        menu.extend([MenuItem::PlayNext, MenuItem::PlayLater]);
    }
    let item = tile.item.clone();
    let library = item.as_ref().is_some_and(|i| {
        matches!(
            i.kind,
            ResourceType::LibrarySongs
                | ResourceType::LibraryAlbums
                | ResourceType::LibraryPlaylists
                | ResourceType::LibraryMusicVideos
                | ResourceType::LibraryArtists
        )
    });
    let rateable = matches!(
        tile.kind,
        TileKind::Album | TileKind::Playlist | TileKind::Song | TileKind::MusicVideo
    );
    if ctx.signed_in
        && let Some(item) = &item
    {
        let addable = !library && rateable;
        if library {
            menu.push(MenuItem::RemoveFromLibrary(item.clone()));
        } else if addable {
            menu.push(MenuItem::AddToLibrary(item.clone()));
        }
        if matches!(tile.kind, TileKind::Song | TileKind::MusicVideo) {
            menu.push(MenuItem::AddToPlaylist(item.clone()));
        }
        if rateable {
            menu.push(MenuItem::Love(item.clone()));
            menu.push(MenuItem::Dislike(item.clone()));
        }
    }
    if let Some(item) = item.filter(|_| !library && rateable && tile.play.is_some()) {
        menu.push(MenuItem::Download(item));
    }
    if let Some(link) = tile
        .route
        .as_ref()
        .and_then(|r| r.share_link(ctx.storefront))
    {
        menu.push(MenuItem::ShareLink(link));
    }
    menu
}
