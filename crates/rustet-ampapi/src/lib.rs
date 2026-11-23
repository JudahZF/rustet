//! Apple Music catalog API client, lyrics and music-video labels.
//!
//! [`Client`] wraps the caller's shared `reqwest::Client`. Each catalog resource adds
//! its methods in its own module. Requests carry the browser headers Apple expects and
//! a 30 s deadline (15 s for lyrics). Its failed requests are not retried.
//!
//! An empty developer token makes most calls scrape one from the web player first
//! ([`Client::token`]); each method documents whether it does.
//!
//! [`Api`] is the typed client of the native UI and the player: catalog, editorial,
//! library and play-activity requests with typed models ([`model`]), a token cache and a
//! response cache.
#![forbid(unsafe_code)]

mod album;
pub mod api;
mod client;
pub mod lyrics;
pub mod model;
mod music_video;
pub mod mvlabel;
mod playlist;
mod recommendations;
mod search;
mod song;
mod track;
mod types;

pub use album::{
    Album, AlbumArtist, AlbumArtistAttributes, AlbumAttributes, AlbumRelationships, AlbumResp,
    ArtworkUrl, EditorialVideo, VideoAsset,
};
pub use api::{Api, ApiConfig, ArtistView, BrowsePage, DeveloperToken, Locale, UserToken};
pub use client::{AMP_BASE, Client, Error, ErrorKind, WEB_ORIGIN};
pub use music_video::{MusicVideo, MusicVideoAttributes, MusicVideoResp};
pub use playlist::{
    LibraryPlayParams, LibraryPlaylistTrack, LibraryTrackAttributes, Playlist, PlaylistResp,
};
pub use recommendations::{RecommendationKind, RecommendationsOptions};
pub use search::{
    SearchArtist, SearchArtistAttributes, SearchMusicVideo, SearchMusicVideoAttributes, SearchResp,
    SearchResults,
};
pub use song::{Song, SongAttributes, SongResp};
pub use track::{
    Track, TrackAlbum, TrackAlbumAttributes, TrackAttributes, TrackPage, TrackRelationships,
};
pub use types::{
    Artist, ArtistAttributes, Artwork, ExtendedAssetUrls, List, MediaRelationships, Page,
    PlayParams, Preview, RelatedAlbum, RelatedAlbumAttributes, Relationship, Resource,
};
