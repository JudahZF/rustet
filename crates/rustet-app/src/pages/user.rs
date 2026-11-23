//! Pages of the signed-in user: Home and the library. Every loader here needs the music user
//! token; signed out it fails with [`LoadError::SignInRequired`] without a request. These
//! requests are never cached.
//!
//! The `/v1/me/library/…` shapes come from Apple's documentation and the web player's code;
//! the tests use hand-written responses in that shape.

use rustet_ampapi::Api;
use rustet_ampapi::api::{History, LibraryKind};
use rustet_ampapi::model::{Page, Resource};
use tokio_util::sync::CancellationToken;

use super::catalog::{BrowsePage, Disc, SearchGroup, SearchPage, discs, search_section};
use super::error::LoadError;
use super::map::{self, artwork, notes};
use super::model::{EmptyState, Header, Linked, List, Listing, QualityMarks, TrackRow};
use super::more::LoadMore;
use crate::route::{LibrarySection, Route};

/// What every library list's `next` link must keep (the first request's parameters).
const LIST_PARAMS: &[(&str, &str)] = &[("include", "catalog"), ("limit", "100")];
const TRACK_LIST_PARAMS: &[(&str, &str)] = &[("include", "catalog,albums"), ("limit", "100")];
const RECENT_PARAMS: &[(&str, &str)] = &[("include", "catalog"), ("limit", "25")];
const HISTORY_PARAMS: &[(&str, &str)] = &[("limit", "20")];
const CONTAINER_TRACK_PARAMS: &[(&str, &str)] = &[("include", "catalog"), ("limit", "100")];

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

fn signed_in(api: &Api) -> Result<(), LoadError> {
    if api.is_signed_in() {
        Ok(())
    } else {
        Err(LoadError::SignInRequired)
    }
}

/// Home (Listen Now): Apple's personal recommendation shelves. Needs the music user token.
pub async fn home(api: &Api) -> Result<BrowsePage, LoadError> {
    signed_in(api)?;
    let shelves = api.home(&cancel()).await?;
    Ok(BrowsePage {
        title: "Home".to_owned(),
        shelves: map::editorial_shelves(shelves),
    })
}

/// A library section: a grid of albums, artists or playlists, or a song list.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryPage {
    pub section: LibrarySection,
    pub listing: Listing,
}

impl LibraryPage {
    pub fn empty_state(&self) -> Option<EmptyState> {
        if !self.listing.is_empty() || self.has_more() {
            return None;
        }
        let what = match self.section {
            LibrarySection::RecentlyAdded => "Nothing was added recently",
            LibrarySection::RecentlyPlayed => "Nothing was played recently",
            LibrarySection::Albums => "No albums in your library",
            LibrarySection::Songs => "No songs in your library",
            LibrarySection::Artists => "No artists in your library",
            LibrarySection::Playlists => "No playlists in your library",
            LibrarySection::MusicVideos => "No music videos in your library",
        };
        Some(EmptyState::new(
            what,
            "Add music from Apple Music with “Add to Library”, and it shows here.",
        ))
    }

    fn has_more(&self) -> bool {
        match &self.listing {
            Listing::Tiles(l) => l.more.is_some(),
            Listing::Tracks(l) => l.more.is_some(),
        }
    }
}

fn tile_list(page: Page<Resource>, params: &'static [(&'static str, &'static str)]) -> Listing {
    Listing::Tiles(List {
        items: map::tiles(&page.data),
        more: LoadMore::next(page.next, params),
    })
}

fn track_list(page: Page<Resource>, params: &'static [(&'static str, &'static str)]) -> Listing {
    Listing::Tracks(List {
        items: map::tracks(&page.data),
        more: LoadMore::next(page.next, params),
    })
}

/// The first page (100 items; 25 for Recently Added, 20 for Recently Played) of a library
/// section. Needs the music user token.
pub async fn library(api: &Api, section: LibrarySection) -> Result<LibraryPage, LoadError> {
    signed_in(api)?;
    let cancel = cancel();
    let listing = match section {
        LibrarySection::RecentlyAdded => {
            tile_list(api.library_recently_added(&cancel).await?, RECENT_PARAMS)
        }
        LibrarySection::RecentlyPlayed => tile_list(
            api.history(&cancel, History::RecentlyPlayed).await?,
            HISTORY_PARAMS,
        ),
        LibrarySection::Albums => tile_list(
            api.library(&cancel, LibraryKind::Albums).await?,
            LIST_PARAMS,
        ),
        LibrarySection::Artists => tile_list(
            api.library(&cancel, LibraryKind::Artists).await?,
            LIST_PARAMS,
        ),
        LibrarySection::Playlists => tile_list(
            api.library(&cancel, LibraryKind::Playlists).await?,
            LIST_PARAMS,
        ),
        LibrarySection::Songs => track_list(
            api.library(&cancel, LibraryKind::Songs).await?,
            TRACK_LIST_PARAMS,
        ),
        LibrarySection::MusicVideos => track_list(
            api.library(&cancel, LibraryKind::MusicVideos).await?,
            TRACK_LIST_PARAMS,
        ),
    };
    Ok(LibraryPage { section, listing })
}

/// Searches the library: songs, albums, artists, playlists. Needs the music user token.
pub async fn library_search(api: &Api, term: &str) -> Result<SearchPage, LoadError> {
    signed_in(api)?;
    let term = term.trim();
    let results = api.library_search(&cancel(), term).await?;
    let groups = [
        (SearchGroup::Songs, results.songs),
        (SearchGroup::Albums, results.albums),
        (SearchGroup::Artists, results.artists),
        (SearchGroup::Playlists, results.playlists),
        (SearchGroup::MusicVideos, results.music_videos),
    ];
    Ok(SearchPage {
        term: term.to_owned(),
        sections: groups
            .into_iter()
            .filter_map(|(group, page)| search_section(group, None, &page?.data, None))
            .collect(),
    })
}

/// A library album.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryAlbumPage {
    pub id: String,
    pub header: Header,
    /// The tracks of the album that are in the library, by disc.
    pub discs: Vec<Disc>,
    /// The catalog album, when there is one.
    pub catalog: Option<Route>,
}

impl LibraryAlbumPage {
    pub fn tracks(&self) -> impl Iterator<Item = &TrackRow> {
        self.discs.iter().flat_map(|d| &d.tracks)
    }
}

/// Every page of a library container's tracks after `first`.
async fn all_tracks(api: &Api, first: Page<Resource>) -> Result<Vec<TrackRow>, LoadError> {
    let mut rows = map::tracks(&first.data);
    let mut more = LoadMore::<TrackRow>::next(first.next, CONTAINER_TRACK_PARAMS);
    while let Some(next) = more {
        let page = next.load(api).await?;
        rows.extend(page.items);
        more = page.more;
    }
    Ok(rows)
}

/// A library album with all its library tracks. Needs the music user token.
pub async fn library_album(api: &Api, id: &str) -> Result<LibraryAlbumPage, LoadError> {
    signed_in(api)?;
    let Resource::LibraryAlbum(album) =
        api.library_item(&cancel(), LibraryKind::Albums, id).await?
    else {
        return Err(LoadError::NotFound);
    };
    let a = album.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let r = &album.relationships;
    let catalog = r.catalog.as_ref().and_then(|p| p.data.first());
    let catalog_attributes = catalog.and_then(|c| c.attributes.as_ref());
    let artist_route = catalog_attributes.and_then(|c| map::artist_route(c.artist_url.as_ref()));
    let mut rows = all_tracks(api, r.tracks.clone().unwrap_or_default()).await?;
    rows.sort_by_key(|row| (row.disc.unwrap_or(1), row.number.unwrap_or(u32::MAX)));
    Ok(LibraryAlbumPage {
        id: album.id.clone(),
        header: Header {
            title: a.name.clone(),
            subtitle: a.artist_name.clone().map(|text| Linked {
                text,
                route: artist_route,
            }),
            artwork: artwork(a.artwork.as_ref()),
            notes: catalog_attributes.and_then(|c| notes(c.editorial_notes.as_ref())),
            explicit: map::explicit(a.content_rating.as_ref()),
            quality: catalog_attributes
                .map(|c| QualityMarks::new(&c.audio_traits, c.is_apple_digital_master))
                .unwrap_or_default(),
        },
        discs: discs(rows),
        catalog: catalog.map(|c| Route::Album { id: c.id.clone() }),
    })
}

/// A library playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryPlaylistPage {
    pub id: String,
    pub header: Header,
    /// The user may rename it and change its tracks.
    pub editable: bool,
    pub tracks: List<TrackRow>,
    /// The catalog playlist it follows, when there is one.
    pub catalog: Option<Route>,
}

impl LibraryPlaylistPage {
    /// The tracks in the order after moving the track at `from` to `to`, for
    /// [`Action::SetPlaylistOrder`](super::actions::Action::SetPlaylistOrder). `None` when an
    /// index is outside the list.
    pub fn moved(&self, from: usize, to: usize) -> Option<Vec<TrackRow>> {
        let items = &self.tracks.items;
        if from >= items.len() || to >= items.len() {
            return None;
        }
        let mut order = items.clone();
        let row = order.remove(from);
        order.insert(to, row);
        Some(order)
    }
}

/// A library playlist with its first tracks (more with `tracks.more`). Needs the music user
/// token.
pub async fn library_playlist(api: &Api, id: &str) -> Result<LibraryPlaylistPage, LoadError> {
    signed_in(api)?;
    let item = api
        .library_item(&cancel(), LibraryKind::Playlists, id)
        .await?;
    let Resource::LibraryPlaylist(playlist) = item else {
        return Err(LoadError::NotFound);
    };
    let a = playlist.attributes.as_ref().ok_or(LoadError::NotFound)?;
    let r = &playlist.relationships;
    let tracks = r.tracks.clone().unwrap_or_default();
    let catalog = r.catalog.as_ref().and_then(|p| p.data.first());
    Ok(LibraryPlaylistPage {
        id: playlist.id.clone(),
        header: Header {
            title: a.name.clone(),
            subtitle: catalog
                .and_then(|c| c.attributes.as_ref()?.curator_name.clone())
                .map(|text| Linked { text, route: None }),
            artwork: artwork(a.artwork.as_ref())
                .or_else(|| catalog.and_then(|c| artwork(c.attributes.as_ref()?.artwork.as_ref()))),
            notes: notes(a.description.as_ref()),
            explicit: false,
            quality: QualityMarks::default(),
        },
        editable: a.can_edit,
        tracks: List {
            items: map::tracks(&tracks.data),
            more: LoadMore::next(tracks.next, CONTAINER_TRACK_PARAMS),
        },
        catalog: catalog.map(|c| Route::Playlist { id: c.id.clone() }),
    })
}

/// Every track of a library playlist (play intents of a tile). Needs the music user token.
pub(crate) async fn library_playlist_tracks(
    api: &Api,
    id: &str,
) -> Result<Vec<TrackRow>, LoadError> {
    signed_in(api)?;
    let first = match api
        .library_tracks(
            &cancel(),
            rustet_ampapi::api::LibraryContainer::Playlist,
            id,
        )
        .await
    {
        // Apple answers 404 for the tracks of an empty playlist.
        Err(rustet_ampapi::Error::NotFound) => Page::default(),
        other => other?,
    };
    all_tracks(api, first).await
}
