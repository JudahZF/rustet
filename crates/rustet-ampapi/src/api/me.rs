//! Requests that need the music user token (`/v1/me/…`).
//!
//! **Unverified.** None of these was called while writing this crate: no sign-in was
//! allowed. Paths, methods, parameters and bodies come from the web player's code
//! (`index~*.js`), MusicKit JS v3 and Apple's documentation (native-UI spec §2.3); each
//! method names its source. The `#[ignore]` tests in `tests/live.rs` check them
//! with a token from the environment.
//!
//! Signed out, each returns [`Error::MissingUserToken`] without a request. A 401 or 403
//! with the token sent is [`Error::Unauthorized`]. Nothing here is cached.

use std::collections::BTreeMap;

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::catalog::recommendation_shelves;
use super::{Api, Host};
use crate::Error;
use crate::model::{
    Autoplay, LibraryPlaylist, LibrarySearchResponse, LibrarySearchResults, Page, Resource, Shelf,
    Storefront,
};

/// Items per library page.
const LIBRARY_PAGE: &str = "100";
/// MusicKit sends at most this many seed items to autoplay (`maxQueueSizeInRequest`).
const MAX_AUTOPLAY_SEEDS: usize = 10;
/// The root of the user's playlist folders.
const PLAYLIST_ROOT: &str = "p.playlistsroot";

/// A resource type in a request (paths, `ids[…]`, `{id, type}` bodies).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceType {
    Songs,
    Albums,
    Artists,
    Playlists,
    MusicVideos,
    Stations,
    LibrarySongs,
    LibraryAlbums,
    LibraryArtists,
    LibraryPlaylists,
    LibraryMusicVideos,
}

impl ResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Songs => "songs",
            Self::Albums => "albums",
            Self::Artists => "artists",
            Self::Playlists => "playlists",
            Self::MusicVideos => "music-videos",
            Self::Stations => "stations",
            Self::LibrarySongs => "library-songs",
            Self::LibraryAlbums => "library-albums",
            Self::LibraryArtists => "library-artists",
            Self::LibraryPlaylists => "library-playlists",
            Self::LibraryMusicVideos => "library-music-videos",
        }
    }
}

/// A resource named in a request: `{id, type}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceRef {
    pub kind: ResourceType,
    pub id: String,
}

impl ResourceRef {
    pub fn new(kind: ResourceType, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    fn json(&self) -> Value {
        json!({ "id": self.id, "type": self.kind.as_str() })
    }
}

fn refs_json(refs: &[ResourceRef]) -> Value {
    Value::Array(refs.iter().map(ResourceRef::json).collect())
}

/// A list of the user's library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibraryKind {
    Songs,
    Albums,
    Artists,
    Playlists,
    MusicVideos,
}

impl LibraryKind {
    fn segment(self) -> &'static str {
        match self {
            Self::Songs => "songs",
            Self::Albums => "albums",
            Self::Artists => "artists",
            Self::Playlists => "playlists",
            Self::MusicVideos => "music-videos",
        }
    }

    /// What a list page includes: the catalog version, and for tracks their albums.
    fn include(self) -> &'static str {
        match self {
            Self::Songs | Self::MusicVideos => "catalog,albums",
            Self::Albums | Self::Artists | Self::Playlists => "catalog",
        }
    }
}

/// A library item that has tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibraryContainer {
    Album,
    Playlist,
}

impl LibraryContainer {
    fn segment(self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Playlist => "playlists",
        }
    }
}

/// The user's listening history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum History {
    /// Recently played albums, playlists and stations (`/v1/me/recent/played`).
    RecentlyPlayed,
    /// Recently played tracks (`/v1/me/recent/played/tracks`, Apple docs).
    RecentTracks,
    /// Recently played radio stations (`/v1/me/recent/radio-stations`, web player).
    RecentStations,
    /// Most played (`/v1/me/history/heavy-rotation`).
    HeavyRotation,
}

impl History {
    fn segments(self) -> &'static [&'static str] {
        match self {
            Self::RecentlyPlayed => &["v1", "me", "recent", "played"],
            Self::RecentTracks => &["v1", "me", "recent", "played", "tracks"],
            Self::RecentStations => &["v1", "me", "recent", "radio-stations"],
            Self::HeavyRotation => &["v1", "me", "history", "heavy-rotation"],
        }
    }

    /// The page size. Apple refuses more than 10 for heavy rotation.
    fn limit(self) -> &'static str {
        match self {
            Self::HeavyRotation => "10",
            _ => "20",
        }
    }
}

/// Love or dislike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rating {
    Love,
    Dislike,
}

impl Rating {
    fn value(self) -> i8 {
        match self {
            Self::Love => 1,
            Self::Dislike => -1,
        }
    }
}

/// A playlist to create.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPlaylist {
    pub name: String,
    pub description: Option<String>,
    /// Catalog songs (`Songs`) or library songs (`LibrarySongs`).
    pub tracks: Vec<ResourceRef>,
}

/// Changes to a library playlist; `None` keeps the current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistEdit {
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Deserialize)]
struct RatingResource {
    attributes: RatingAttributes,
}

#[derive(Deserialize)]
struct RatingAttributes {
    value: i8,
}

impl Api {
    /// The account's storefront (`GET /v1/me/storefront`, Apple docs).
    pub async fn account_storefront(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Storefront, Error> {
        let call = self.call(Method::GET, Host::Amp, &["v1", "me", "storefront"], &[])?;
        let page: Page<Storefront> = self.fetch(cancel, call).await?;
        page.data.into_iter().next().ok_or(Error::NotFound)
    }

    /// The first page (100 items) of a library list with each item's catalog version.
    /// Follow `next` with [`Api::next_page`], or read all with [`Api::pages`].
    /// (Rustet's renderer library sync, web player.)
    pub async fn library(
        &self,
        cancel: &CancellationToken,
        kind: LibraryKind,
    ) -> Result<Page<Resource>, Error> {
        let locale = self.locale();
        let segments = ["v1", "me", "library", kind.segment()];
        let query = [
            ("include", kind.include()),
            ("limit", LIBRARY_PAGE),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        self.fetch(cancel, call).await
    }

    /// Recently added albums and playlists (web player).
    pub async fn library_recently_added(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Page<Resource>, Error> {
        let locale = self.locale();
        let segments = ["v1", "me", "library", "recently-added"];
        let query = [
            ("limit", "25"),
            ("include", "catalog"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        self.fetch(cancel, call).await
    }

    /// The playlists and folders in a folder; `None` is the top level (web player).
    pub async fn library_playlist_folder(
        &self,
        cancel: &CancellationToken,
        folder: Option<&str>,
    ) -> Result<Page<Resource>, Error> {
        let locale = self.locale();
        let folder = folder.unwrap_or(PLAYLIST_ROOT);
        let segments = [
            "v1",
            "me",
            "library",
            "playlist-folders",
            folder,
            "children",
        ];
        let query = [
            ("include", "catalog"),
            ("omit[resource]", "autos"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        self.fetch(cancel, call).await
    }

    /// One library item with its catalog version and, for albums and playlists, its tracks
    /// (web player).
    pub async fn library_item(
        &self,
        cancel: &CancellationToken,
        kind: LibraryKind,
        id: &str,
    ) -> Result<Resource, Error> {
        let locale = self.locale();
        let include = match kind {
            LibraryKind::Albums | LibraryKind::Playlists => "catalog,tracks",
            LibraryKind::Songs | LibraryKind::MusicVideos | LibraryKind::Artists => "catalog",
        };
        let segments = ["v1", "me", "library", kind.segment(), id];
        let query = [("include", include), ("l", &locale.language)];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        let page: Page<Resource> = self.fetch(cancel, call).await?;
        page.data.into_iter().next().ok_or(Error::NotFound)
    }

    /// The tracks of a library album or playlist, paged by `next` (web player).
    pub async fn library_tracks(
        &self,
        cancel: &CancellationToken,
        container: LibraryContainer,
        id: &str,
    ) -> Result<Page<Resource>, Error> {
        let segments = ["v1", "me", "library", container.segment(), id, "tracks"];
        let query = [("include", "catalog"), ("limit", LIBRARY_PAGE)];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        self.fetch(cancel, call).await
    }

    /// Searches the library (path from the web player, `types` from Apple docs).
    pub async fn library_search(
        &self,
        cancel: &CancellationToken,
        term: &str,
    ) -> Result<LibrarySearchResults, Error> {
        let locale = self.locale();
        let query = [
            ("term", term),
            (
                "types",
                "library-songs,library-albums,library-artists,library-playlists",
            ),
            ("limit", "21"),
            ("include[library-songs]", "catalog"),
            ("include[library-artists]", "catalog"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let segments = ["v1", "me", "library", "search"];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        Ok(self
            .fetch::<LibrarySearchResponse>(cancel, call)
            .await?
            .results)
    }

    /// Home ("Listen Now") shelves (web player, `class Cme`). Apple refuses the request
    /// without the local UTC offset, and refuses the web player's `displayFilter[kind]`.
    pub async fn home(&self, cancel: &CancellationToken) -> Result<Vec<Shelf>, Error> {
        let locale = self.locale();
        let timezone = chrono::Local::now().format("%:z").to_string();
        let query = [
            ("timezone", timezone.as_str()),
            ("name", "listen-now"),
            ("extend[playlists]", "artistNames"),
            ("include[albums]", "artists"),
            ("include[library-playlists]", "catalog"),
            ("include[stations]", "radio-show"),
            ("include[personal-recommendation]", "primary-content"),
            ("fields[artists]", "name,artwork,url"),
            ("omit[resource]", "autos"),
            ("extend[stations]", "airTime,supportsAirTimeUpdates"),
            ("meta[stations]", "inflectionPoints"),
            ("platform", "web"),
            ("l", &locale.language),
        ];
        let segments = ["v1", "me", "recommendations"];
        let call = self.call(Method::GET, Host::Amp, &segments, &query)?;
        let page: Page<Resource> = self.fetch(cancel, call).await?;
        Ok(recommendation_shelves(page.data))
    }

    /// A page of listening history.
    pub async fn history(
        &self,
        cancel: &CancellationToken,
        history: History,
    ) -> Result<Page<Resource>, Error> {
        let locale = self.locale();
        let query = [("limit", history.limit()), ("l", &locale.language)];
        let call = self.call(Method::GET, Host::Amp, history.segments(), &query)?;
        self.fetch(cancel, call).await
    }

    /// Adds catalog items to the library (`POST /v1/me/library?ids[songs]=…`, web player).
    pub async fn add_to_library(
        &self,
        cancel: &CancellationToken,
        items: &[ResourceRef],
    ) -> Result<(), Error> {
        let mut by_type: BTreeMap<ResourceType, Vec<&str>> = BTreeMap::new();
        for item in items {
            by_type.entry(item.kind).or_default().push(&item.id);
        }
        let keys: Vec<_> = by_type
            .iter()
            .map(|(kind, ids)| (format!("ids[{}]", kind.as_str()), ids.join(",")))
            .collect();
        let mut query: Vec<(&str, &str)> =
            keys.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        query.push(("representation", "ids"));
        let call = self.call(Method::POST, Host::Amp, &["v1", "me", "library"], &query)?;
        self.execute(cancel, call).await.map(drop)
    }

    /// Removes an item from the library; for a playlist this deletes it. The web player
    /// sends the catalog type with the library ID (`DELETE /v1/me/library/songs/i.…`).
    pub async fn remove_from_library(
        &self,
        cancel: &CancellationToken,
        item: &ResourceRef,
    ) -> Result<(), Error> {
        let segments = ["v1", "me", "library", item.kind.as_str(), &item.id];
        let call = self.call(Method::DELETE, Host::Amp, &segments, &[])?;
        self.execute(cancel, call).await.map(drop)
    }

    /// Creates a library playlist (Apple docs; body as the web player sends it).
    pub async fn create_playlist(
        &self,
        cancel: &CancellationToken,
        playlist: &NewPlaylist,
    ) -> Result<LibraryPlaylist, Error> {
        let mut body = json!({
            "attributes": { "name": playlist.name, "description": playlist.description },
        });
        if !playlist.tracks.is_empty() {
            body["relationships"] = json!({ "tracks": { "data": refs_json(&playlist.tracks) } });
        }
        let segments = ["v1", "me", "library", "playlists"];
        let mut call = self.call(Method::POST, Host::Amp, &segments, &[])?;
        call.body = Some(body);
        let page: Page<LibraryPlaylist> = self.fetch(cancel, call).await?;
        page.data.into_iter().next().ok_or(Error::NotFound)
    }

    /// Renames a library playlist or changes its description (`PATCH`, web player).
    pub async fn edit_playlist(
        &self,
        cancel: &CancellationToken,
        id: &str,
        edit: &PlaylistEdit,
    ) -> Result<(), Error> {
        let mut attributes = serde_json::Map::new();
        if let Some(name) = &edit.name {
            attributes.insert("name".into(), name.as_str().into());
        }
        if let Some(description) = &edit.description {
            attributes.insert("description".into(), description.as_str().into());
        }
        let segments = ["v1", "me", "library", "playlists", id];
        let mut call = self.call(Method::PATCH, Host::Amp, &segments, &[])?;
        call.body = Some(json!({ "attributes": attributes }));
        self.execute(cancel, call).await.map(drop)
    }

    /// Appends tracks to a library playlist (`POST …/tracks`, Apple docs).
    pub async fn add_playlist_tracks(
        &self,
        cancel: &CancellationToken,
        id: &str,
        tracks: &[ResourceRef],
    ) -> Result<(), Error> {
        let segments = ["v1", "me", "library", "playlists", id, "tracks"];
        let mut call = self.call(Method::POST, Host::Amp, &segments, &[])?;
        call.body = Some(json!({ "data": refs_json(tracks) }));
        self.execute(cancel, call).await.map(drop)
    }

    /// Replaces a library playlist's tracks with `tracks` in this order: reorder, or
    /// remove several at once (`PUT …/tracks`, web player "UpdatePlaylistTracksIntent").
    pub async fn set_playlist_tracks(
        &self,
        cancel: &CancellationToken,
        id: &str,
        tracks: &[ResourceRef],
    ) -> Result<(), Error> {
        let segments = ["v1", "me", "library", "playlists", id, "tracks"];
        let mut call = self.call(Method::PUT, Host::Amp, &segments, &[])?;
        call.body = Some(json!({ "data": refs_json(tracks) }));
        self.execute(cancel, call).await.map(drop)
    }

    /// Removes every occurrence of a library song from a library playlist (web player).
    pub async fn remove_playlist_track(
        &self,
        cancel: &CancellationToken,
        id: &str,
        library_song_id: &str,
    ) -> Result<(), Error> {
        let segments = ["v1", "me", "library", "playlists", id, "tracks"];
        let query = [("ids[library-songs]", library_song_id), ("mode", "all")];
        let call = self.call(Method::DELETE, Host::Amp, &segments, &query)?;
        self.execute(cancel, call).await.map(drop)
    }

    /// The user's rating of an item; `None` when there is none (Apple answers 404).
    pub async fn rating(
        &self,
        cancel: &CancellationToken,
        item: &ResourceRef,
    ) -> Result<Option<Rating>, Error> {
        let segments = ["v1", "me", "ratings", item.kind.as_str(), &item.id];
        let call = self.call(Method::GET, Host::Amp, &segments, &[])?;
        let page: Page<RatingResource> = match self.fetch(cancel, call).await {
            Err(Error::NotFound) => return Ok(None),
            other => other?,
        };
        Ok(page.data.first().and_then(|r| match r.attributes.value {
            1 => Some(Rating::Love),
            -1 => Some(Rating::Dislike),
            _ => None,
        }))
    }

    /// Loves or dislikes an item (web player `qwe()`; no `l` or `platform`).
    pub async fn set_rating(
        &self,
        cancel: &CancellationToken,
        item: &ResourceRef,
        rating: Rating,
    ) -> Result<(), Error> {
        let segments = ["v1", "me", "ratings", item.kind.as_str(), &item.id];
        let mut call = self.call(Method::PUT, Host::Amp, &segments, &[])?;
        call.body = Some(json!({ "type": "rating", "attributes": { "value": rating.value() } }));
        self.execute(cancel, call).await.map(drop)
    }

    /// Removes the user's rating of an item.
    pub async fn clear_rating(
        &self,
        cancel: &CancellationToken,
        item: &ResourceRef,
    ) -> Result<(), Error> {
        let segments = ["v1", "me", "ratings", item.kind.as_str(), &item.id];
        let call = self.call(Method::DELETE, Host::Amp, &segments, &[])?;
        self.execute(cancel, call).await.map(drop)
    }

    /// Adds an item to or removes it from Favourites (web player "UpdateFavoritesIntent":
    /// `POST`/`DELETE /v1/me/favorites?ids[type]=id`).
    pub async fn set_favorite(
        &self,
        cancel: &CancellationToken,
        item: &ResourceRef,
        favorite: bool,
    ) -> Result<(), Error> {
        let method = if favorite {
            Method::POST
        } else {
            Method::DELETE
        };
        let key = format!("ids[{}]", item.kind.as_str());
        let query = [(key.as_str(), item.id.as_str())];
        let call = self.call(method, Host::Amp, &["v1", "me", "favorites"], &query)?;
        self.execute(cancel, call).await.map(drop)
    }

    /// The next tracks of a station (MusicKit `loadStationNextTracks`).
    pub async fn station_next_tracks(
        &self,
        cancel: &CancellationToken,
        station_id: &str,
        limit: u32,
    ) -> Result<Vec<Resource>, Error> {
        let limit = limit.to_string();
        let segments = ["v1", "me", "stations", "next-tracks", station_id];
        let call = self.call(Method::POST, Host::Amp, &segments, &[("limit", &limit)])?;
        Ok(self.fetch::<Page<Resource>>(cancel, call).await?.data)
    }

    /// An autoplay station seeded by the last items of the queue, with up to `limit`
    /// tracks (MusicKit `createAutoplayStation`). MusicKit seeds with songs (`Songs` or
    /// `LibrarySongs`) only; only the last ten seeds are sent.
    pub async fn autoplay(
        &self,
        cancel: &CancellationToken,
        seeds: &[ResourceRef],
        limit: u32,
    ) -> Result<Autoplay, Error> {
        let seeds = &seeds[seeds.len().saturating_sub(MAX_AUTOPLAY_SEEDS)..];
        let limit = limit.to_string();
        let query = [
            ("limit[results:tracks]", limit.as_str()),
            ("with", "tracks"),
        ];
        let segments = ["v1", "me", "stations", "continuous"];
        let mut call = self.call(Method::POST, Host::Amp, &segments, &query)?;
        call.body = Some(json!({ "data": refs_json(seeds) }));
        self.fetch(cancel, call).await
    }
}
