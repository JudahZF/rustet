//! What each screen shows: page models, their loaders, play intents and actions.
//!
//! A view holds one [`Page`] (from [`load`] or a route's own loader) and draws it; it needs no
//! knowledge of Apple's JSON. Loaders are plain async functions over an [`rustet_ampapi::Api`];
//! dropping the future cancels the request. Which loaders need the music user token:
//! [`Route::needs_user_token`] (Home and the library); the others work signed out.
//!
//! - [`model`]: artwork, headers, track rows, tiles, shelves, lists, empty and error states.
//! - [`catalog`], [`user`]: the page models and their loaders.
//! - [`more`]: "load more" for paged lists.
//! - [`play`]: what the player gets when something is played.
//! - [`actions`]: library, playlist and rating changes, and the context menus.
//! - [`lyrics`]: the lyrics model.

pub mod actions;
pub mod catalog;
pub mod error;
pub mod lyrics;
pub(crate) mod map;
pub mod model;
pub mod more;
pub mod play;
pub mod user;

use rustet_ampapi::Api;

pub use catalog::{
    AlbumPage, ArtistPage, BrowsePage, ChartsPage, CuratorPage, Hint, ListPage, MusicVideoPage,
    PlaylistPage, SearchPage, SongPage, StationPage,
};
pub use error::{ErrorState, LoadError, Recovery};
pub use user::{LibraryAlbumPage, LibraryPage, LibraryPlaylistPage};

use crate::route::Route;

/// The model of one screen.
// A view holds one page at a time, so the variants' sizes do not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Search(SearchPage),
    /// The search page before a term is typed.
    SearchLanding(BrowsePage),
    /// New, Radio and Home.
    Browse(BrowsePage),
    Charts(ChartsPage),
    Album(AlbumPage),
    Playlist(PlaylistPage),
    Artist(ArtistPage),
    /// An artist's "see all" list or an editorial room.
    List(ListPage),
    Song(SongPage),
    MusicVideo(MusicVideoPage),
    Station(StationPage),
    Curator(CuratorPage),
    Library(LibraryPage),
    LibrarySearch(SearchPage),
    LibraryAlbum(LibraryAlbumPage),
    LibraryPlaylist(LibraryPlaylistPage),
}

/// Loads the page of `route`. Downloads and Settings show local state and have no loader
/// ([`LoadError::LocalRoute`]).
pub async fn load(api: &Api, route: &Route) -> Result<Page, LoadError> {
    Ok(match route {
        Route::Search { term } if term.trim().is_empty() => {
            Page::SearchLanding(catalog::search_landing(api).await?)
        }
        Route::Search { term } => Page::Search(catalog::search(api, term).await?),
        Route::New => Page::Browse(catalog::new(api).await?),
        Route::Radio => Page::Browse(catalog::radio(api).await?),
        Route::Home => Page::Browse(user::home(api).await?),
        Route::Charts { genre } => Page::Charts(catalog::charts(api, genre.as_deref()).await?),
        Route::Album { id } => Page::Album(catalog::album(api, id).await?),
        Route::Playlist { id } => Page::Playlist(catalog::playlist(api, id).await?),
        Route::Artist { id } => Page::Artist(catalog::artist(api, id).await?),
        Route::ArtistView { id, view } => Page::List(catalog::artist_view(api, id, *view).await?),
        Route::Song { id } => Page::Song(catalog::song(api, id).await?),
        Route::MusicVideo { id } => Page::MusicVideo(catalog::music_video(api, id).await?),
        Route::Station { id } => Page::Station(catalog::station(api, id).await?),
        Route::Room { id } => Page::List(catalog::room(api, id).await?),
        Route::Curator { id } => Page::Curator(catalog::curator(api, id).await?),
        Route::Library(section) => Page::Library(user::library(api, *section).await?),
        Route::LibrarySearch { term } => {
            Page::LibrarySearch(user::library_search(api, term).await?)
        }
        Route::LibraryAlbum { id } => Page::LibraryAlbum(user::library_album(api, id).await?),
        Route::LibraryPlaylist { id } => {
            Page::LibraryPlaylist(user::library_playlist(api, id).await?)
        }
        Route::Downloads | Route::Settings { .. } => {
            return Err(LoadError::LocalRoute(route.clone()));
        }
    })
}
