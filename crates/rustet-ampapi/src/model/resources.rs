//! Attributes, relationships and views of each resource type.
//!
//! The library (`library-*`) types come from Apple's documentation and the web player's
//! code, not from recorded responses: no signed-in call was made while writing them.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::de::{lenient, null_default};
use super::{Empty, Item, Page, Resource, View};

/// Artwork: a URL template with `{w}`, `{h}` and sometimes `{c}` (crop) and `{f}`
/// (format) placeholders, on Apple's image CDN.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Artwork {
    pub url: String,
    /// The largest size the source has.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Hex RGB without `#`, for example `"495d7c"`.
    pub bg_color: Option<String>,
    pub text_color1: Option<String>,
    pub text_color2: Option<String>,
    pub text_color3: Option<String>,
    pub text_color4: Option<String>,
}

impl Artwork {
    /// The image URL for `width` × `height` pixels, as JPEG with Apple's default crop (`bb`).
    ///
    /// Pass the drawn size times the display scale factor. Apple scales down only: a size
    /// above the source's `width`/`height` returns the source size.
    pub fn url(&self, width: u32, height: u32) -> String {
        self.url
            .replace("{w}", &width.to_string())
            .replace("{h}", &height.to_string())
            .replace("{c}", "bb")
            .replace("{f}", "jpg")
    }
}

/// Editorial text: `{short, standard, name, tagline}`. Playlist descriptions use it too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EditorialNotes {
    pub short: Option<String>,
    pub standard: Option<String>,
    pub name: Option<String>,
    pub tagline: Option<String>,
}

/// What the player needs to start an item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlayParams {
    pub id: String,
    /// `song`, `album`, `playlist`, `musicVideo`, `radioStation`, …
    pub kind: String,
    /// Stations: `"stream"` (live radio) or `"tracks"`.
    pub format: Option<String>,
    pub station_hash: Option<String>,
    pub has_drm: Option<bool>,
    pub streaming_kind: Option<u32>,
    /// Catalog playlists: the version the play-activity report names.
    pub version_hash: Option<String>,
    /// Library items.
    pub is_library: Option<bool>,
    pub catalog_id: Option<String>,
    pub global_id: Option<String>,
    pub reporting: Option<bool>,
    pub reporting_id: Option<String>,
}

impl PlayParams {
    /// The catalog ID to stream a library item with. When Apple replaces a catalog song, the
    /// library item keeps the old `catalogId` (the catalog answers 404 for it) and `reportingId`
    /// names the replacement, so a numeric `reportingId` wins.
    pub fn stream_catalog_id(&self) -> Option<&str> {
        stream_catalog_id(self.reporting_id.as_deref(), self.catalog_id.as_deref())
    }
}

/// See [`PlayParams::stream_catalog_id`].
pub(crate) fn stream_catalog_id<'a>(
    reporting_id: Option<&'a str>,
    catalog_id: Option<&'a str>,
) -> Option<&'a str> {
    reporting_id
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .or(catalog_id.filter(|id| !id.is_empty()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Preview {
    pub url: String,
}

/// An audio variant a song offers (`attributes.audioTraits`), for the quality marks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(from = "String")]
pub enum AudioTrait {
    Lossless,
    HiResLossless,
    LossyStereo,
    Atmos,
    Spatial,
    Other(String),
}

impl From<String> for AudioTrait {
    fn from(s: String) -> Self {
        match s.as_str() {
            "lossless" => Self::Lossless,
            "hi-res-lossless" => Self::HiResLossless,
            "lossy-stereo" => Self::LossyStereo,
            "atmos" => Self::Atmos,
            "spatial" => Self::Spatial,
            _ => Self::Other(s),
        }
    }
}

/// Motion artwork: an HLS video and its first frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MotionVideo {
    pub video: Option<String>,
    pub preview_frame: Option<Artwork>,
}

/// Named editorial artworks (`subscriptionHero`, `staticDetailSquare`, `bannerUber`, …).
pub type EditorialArtwork = BTreeMap<String, Artwork>;
/// Named motion artworks (`motionDetailSquare`, `motionDetailTall`, …).
pub type EditorialVideo = BTreeMap<String, MotionVideo>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtendedAssetUrls {
    /// The master HLS playlist with the lossless and spatial variants.
    pub enhanced_hls: Option<String>,
}

/// An editorial link (`{label, url}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Link {
    pub label: String,
    pub url: String,
    /// `"internal"` or `"external"`, when Apple says.
    pub target: Option<String>,
}

// ---- catalog ----

pub type Song = Item<SongAttributes, SongRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SongAttributes {
    pub name: String,
    pub artist_name: String,
    pub artist_url: Option<String>,
    pub album_name: Option<String>,
    pub composer_name: Option<String>,
    pub artwork: Option<Artwork>,
    pub duration_in_millis: Option<u64>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub release_date: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub genre_names: Vec<String>,
    /// `"explicit"` or `"clean"`; absent for unrated.
    pub content_rating: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub has_lyrics: bool,
    #[serde(deserialize_with = "null_default")]
    pub has_time_synced_lyrics: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_apple_digital_master: bool,
    #[serde(deserialize_with = "null_default")]
    pub audio_traits: Vec<AudioTrait>,
    pub isrc: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub previews: Vec<Preview>,
    pub play_params: Option<PlayParams>,
    pub url: Option<String>,
    pub extended_asset_urls: Option<ExtendedAssetUrls>,
    pub lyrics_excerpt: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SongRelationships {
    pub albums: Option<Page<Album>>,
    pub artists: Option<Page<Artist>>,
    pub composers: Option<Page<Artist>>,
    pub credits: Option<Page<RoleCategory>>,
    #[serde(rename = "music-videos")]
    pub music_videos: Option<Page<MusicVideo>>,
}

/// A credits group ("PERFORMING ARTISTS", "COMPOSITION AND LYRICS", …).
pub type RoleCategory = Item<RoleCategoryAttributes, RoleCategoryRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoleCategoryAttributes {
    pub kind: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoleCategoryRelationships {
    #[serde(rename = "credit-artists")]
    pub credit_artists: Option<Page<CreditArtist>>,
}

pub type CreditArtist = Item<CreditArtistAttributes>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CreditArtistAttributes {
    pub name: String,
    pub artwork: Option<Artwork>,
    #[serde(deserialize_with = "null_default")]
    pub role_names: Vec<String>,
}

pub type Album = Item<AlbumAttributes, AlbumRelationships, AlbumViews>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AlbumAttributes {
    pub name: String,
    pub artist_name: String,
    pub artist_url: Option<String>,
    pub artwork: Option<Artwork>,
    pub track_count: Option<u32>,
    pub release_date: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub genre_names: Vec<String>,
    pub content_rating: Option<String>,
    pub record_label: Option<String>,
    pub copyright: Option<String>,
    pub upc: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub is_single: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_compilation: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_complete: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_prerelease: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_apple_digital_master: bool,
    #[serde(deserialize_with = "null_default")]
    pub audio_traits: Vec<AudioTrait>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_artwork: EditorialArtwork,
    #[serde(deserialize_with = "lenient")]
    pub editorial_video: EditorialVideo,
    pub play_params: Option<PlayParams>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AlbumRelationships {
    pub artists: Option<Page<Artist>>,
    /// Songs and music videos.
    pub tracks: Option<Page<Resource>>,
    #[serde(rename = "record-labels")]
    pub record_labels: Option<Page<Resource>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AlbumViews {
    #[serde(rename = "appears-on")]
    pub appears_on: Option<View>,
    #[serde(rename = "more-by-artist")]
    pub more_by_artist: Option<View>,
    #[serde(rename = "other-versions")]
    pub other_versions: Option<View>,
    #[serde(rename = "related-videos")]
    pub related_videos: Option<View>,
    #[serde(rename = "you-might-also-like")]
    pub you_might_also_like: Option<View>,
}

pub type Artist = Item<ArtistAttributes, ArtistRelationships, ArtistViews>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ArtistAttributes {
    pub name: String,
    pub artwork: Option<Artwork>,
    #[serde(deserialize_with = "null_default")]
    pub genre_names: Vec<String>,
    pub url: Option<String>,
    /// Plain-text biography (`extend=artistBio`).
    pub artist_bio: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_artwork: EditorialArtwork,
    #[serde(deserialize_with = "lenient")]
    pub editorial_video: EditorialVideo,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ArtistRelationships {
    pub albums: Option<Page<Album>>,
}

/// The artist page sections, each also reachable on its own ("see all", [`ArtistView`]).
///
/// [`ArtistView`]: crate::ArtistView
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ArtistViews {
    #[serde(rename = "top-songs")]
    pub top_songs: Option<View>,
    #[serde(rename = "latest-release")]
    pub latest_release: Option<View>,
    #[serde(rename = "full-albums")]
    pub full_albums: Option<View>,
    pub singles: Option<View>,
    #[serde(rename = "appears-on-albums")]
    pub appears_on_albums: Option<View>,
    #[serde(rename = "top-music-videos")]
    pub top_music_videos: Option<View>,
    #[serde(rename = "featured-playlists")]
    pub featured_playlists: Option<View>,
    #[serde(rename = "similar-artists")]
    pub similar_artists: Option<View>,
}

pub type Playlist = Item<PlaylistAttributes, PlaylistRelationships, PlaylistViews>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlaylistAttributes {
    pub name: String,
    pub curator_name: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub description: Option<EditorialNotes>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
    pub artwork: Option<Artwork>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_artwork: EditorialArtwork,
    #[serde(deserialize_with = "lenient")]
    pub editorial_video: EditorialVideo,
    /// `editorial`, `external`, `personal-mix`, `user-shared`, …
    pub playlist_type: Option<String>,
    pub last_modified_date: Option<String>,
    pub track_count: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    pub is_chart: bool,
    #[serde(deserialize_with = "null_default")]
    pub has_collaboration: bool,
    #[serde(deserialize_with = "null_default")]
    pub audio_traits: Vec<AudioTrait>,
    pub play_params: Option<PlayParams>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PlaylistRelationships {
    /// Songs and music videos; inline up to `limit[tracks]`, then [`Page::next`].
    pub tracks: Option<Page<Resource>>,
    pub curator: Option<Page<Resource>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PlaylistViews {
    #[serde(rename = "featured-artists")]
    pub featured_artists: Option<View>,
    #[serde(rename = "more-by-curator")]
    pub more_by_curator: Option<View>,
}

pub type MusicVideo = Item<MusicVideoAttributes, MusicVideoRelationships, MusicVideoViews>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MusicVideoAttributes {
    pub name: String,
    pub artist_name: String,
    pub album_name: Option<String>,
    pub artwork: Option<Artwork>,
    pub duration_in_millis: Option<u64>,
    pub release_date: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub genre_names: Vec<String>,
    #[serde(rename = "has4K", deserialize_with = "null_default")]
    pub has_4k: bool,
    #[serde(rename = "hasHDR", deserialize_with = "null_default")]
    pub has_hdr: bool,
    pub isrc: Option<String>,
    pub content_rating: Option<String>,
    /// For example `"4K"`, `"HDR"`.
    #[serde(deserialize_with = "null_default")]
    pub video_traits: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    pub previews: Vec<Preview>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
    pub play_params: Option<PlayParams>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MusicVideoRelationships {
    pub artists: Option<Page<Artist>>,
    pub albums: Option<Page<Album>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MusicVideoViews {
    #[serde(rename = "more-by-artist")]
    pub more_by_artist: Option<View>,
    #[serde(rename = "more-in-genre")]
    pub more_in_genre: Option<View>,
}

pub type Station = Item<StationAttributes, StationRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StationAttributes {
    pub name: String,
    pub artwork: Option<Artwork>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_artwork: EditorialArtwork,
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
    /// A live broadcast (Apple Music 1, …).
    #[serde(deserialize_with = "null_default")]
    pub is_live: bool,
    #[serde(deserialize_with = "null_default")]
    pub requires_subscription: bool,
    /// `"streaming"`, …
    pub kind: Option<String>,
    /// `"audio"` or `"video"`.
    pub media_kind: Option<String>,
    pub station_provider_name: Option<String>,
    /// Episodes.
    pub duration_in_millis: Option<u64>,
    #[serde(deserialize_with = "lenient")]
    pub air_time: Option<AirTime>,
    pub play_params: Option<PlayParams>,
    pub url: Option<String>,
}

/// When an episode aired (ISO 8601).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AirTime {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StationRelationships {
    #[serde(rename = "radio-show")]
    pub radio_show: Option<Page<Resource>>,
    pub events: Option<Page<Resource>>,
}

/// `apple-curators` and `curators`.
pub type Curator = Item<CuratorAttributes, CuratorRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CuratorAttributes {
    pub name: String,
    pub short_name: Option<String>,
    /// `"Genre"`, `"Show"`, `"Curator"`, …
    pub kind: Option<String>,
    pub artwork: Option<Artwork>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_artwork: EditorialArtwork,
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
    pub show_host_name: Option<String>,
    pub playlist_count: Option<u32>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CuratorRelationships {
    pub playlists: Option<Page<Playlist>>,
    /// The curator's editorial page (Apple curators only).
    pub grouping: Option<Page<Grouping>>,
}

pub type RecordLabel = Item<RecordLabelAttributes>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RecordLabelAttributes {
    pub name: String,
    pub artwork: Option<Artwork>,
    #[serde(deserialize_with = "lenient")]
    pub description: Option<EditorialNotes>,
    pub url: Option<String>,
}

pub type Genre = Item<GenreAttributes>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GenreAttributes {
    pub name: String,
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
    pub url: Option<String>,
}

/// An editorial tile that links somewhere (a search-landing category, a marketing item).
pub type EditorialItem = Item<EditorialItemAttributes>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EditorialItemAttributes {
    #[serde(deserialize_with = "lenient")]
    pub editorial_notes: Option<EditorialNotes>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_artwork: EditorialArtwork,
    #[serde(deserialize_with = "lenient")]
    pub link: Option<Link>,
    pub url: Option<String>,
}

pub type UploadedVideo = Item<UploadedVideoAttributes>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UploadedVideoAttributes {
    pub name: String,
    pub artist_name: Option<String>,
    pub artwork: Option<Artwork>,
    pub duration_in_milliseconds: Option<u64>,
    pub upload_date: Option<String>,
    pub play_params: Option<PlayParams>,
}

// ---- editorial ----

/// An editorial page: `New`, `Radio`, a curator's page. Its first tab holds the shelves.
pub type Grouping = Item<GroupingAttributes, GroupingRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GroupingAttributes {
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub genre_names: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GroupingRelationships {
    pub tabs: Option<Page<EditorialElement>>,
}

/// A node of an editorial page: a tab, a shelf, or a tile inside a shelf. Its meaning is
/// in `editorialElementKind`; see [`Shelf`](crate::model::Shelf).
pub type EditorialElement = Item<EditorialElementAttributes, EditorialElementRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EditorialElementAttributes {
    pub name: Option<String>,
    pub title: Option<String>,
    /// A number as a string: `"326"` (tile row), `"327"` (song list), `"316"` (hero), …
    pub editorial_element_kind: Option<String>,
    /// `"compact"` or `"expanded"`.
    pub display_style: Option<String>,
    /// A tile's label, for example `"NEW ALBUM"`.
    pub design_badge: Option<String>,
    pub design_tag: Option<String>,
    pub artwork: Option<Artwork>,
    #[serde(deserialize_with = "lenient")]
    pub editorial_artwork: EditorialArtwork,
    #[serde(deserialize_with = "lenient")]
    pub links: Vec<Link>,
    #[serde(deserialize_with = "null_default")]
    pub resource_types: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EditorialElementRelationships {
    pub children: Option<Page<Resource>>,
    pub contents: Option<Page<Resource>>,
    /// The shelf's "see all" room.
    pub room: Option<Page<Resource>>,
}

/// A "see all" list of an editorial shelf.
pub type Room = Item<RoomAttributes, RoomRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RoomAttributes {
    pub title: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub resource_types: Vec<String>,
    /// Sorts the room offers: `alphabet`, `releaseDate`, `featured`.
    #[serde(deserialize_with = "null_default")]
    pub sorts: Vec<String>,
    pub default_sort: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoomRelationships {
    pub contents: Option<Page<Resource>>,
}

/// A `personal-recommendation` shelf (Home, search landing).
pub type Recommendation = Item<RecommendationAttributes, RecommendationRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RecommendationAttributes {
    #[serde(deserialize_with = "lenient")]
    pub title: Option<RecommendationTitle>,
    #[serde(deserialize_with = "lenient")]
    pub display: Option<RecommendationDisplay>,
    /// `music-recommendations`, …
    pub kind: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub has_see_all: bool,
    #[serde(deserialize_with = "null_default")]
    pub resource_types: Vec<String>,
    pub next_update_date: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RecommendationTitle {
    pub string_for_display: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RecommendationDisplay {
    /// Apple's shelf component, for example `"MusicNamedBrickShelf"`.
    pub kind: Option<String>,
    pub size: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RecommendationRelationships {
    pub contents: Option<Page<Resource>>,
}

pub type Storefront = Item<StorefrontAttributes>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StorefrontAttributes {
    pub name: String,
    pub default_language_tag: String,
    #[serde(deserialize_with = "null_default")]
    pub supported_language_tags: Vec<String>,
    /// `"allowed"` or `"prohibited"`.
    pub explicit_content_policy: Option<String>,
}

// ---- library (unverified shapes) ----

/// `library-songs`.
pub type LibrarySong = Item<LibraryTrackAttributes, LibraryTrackRelationships<Song>>;
/// `library-music-videos`.
pub type LibraryMusicVideo = Item<LibraryTrackAttributes, LibraryTrackRelationships<MusicVideo>>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryTrackAttributes {
    pub name: String,
    pub artist_name: Option<String>,
    pub album_name: Option<String>,
    pub artwork: Option<Artwork>,
    pub duration_in_millis: Option<u64>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub release_date: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub genre_names: Vec<String>,
    pub content_rating: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub has_lyrics: bool,
    pub date_added: Option<String>,
    /// `catalogId` is the ID to stream; `id` is the library ID.
    pub play_params: Option<PlayParams>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, bound(deserialize = "C: Deserialize<'de>"))]
pub struct LibraryTrackRelationships<C> {
    pub catalog: Option<Page<C>>,
    pub albums: Option<Page<Resource>>,
    pub artists: Option<Page<Resource>>,
}

impl<C> Default for LibraryTrackRelationships<C> {
    fn default() -> Self {
        Self {
            catalog: None,
            albums: None,
            artists: None,
        }
    }
}

pub type LibraryAlbum = Item<LibraryAlbumAttributes, LibraryAlbumRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryAlbumAttributes {
    pub name: String,
    pub artist_name: Option<String>,
    pub artwork: Option<Artwork>,
    pub track_count: Option<u32>,
    pub release_date: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub genre_names: Vec<String>,
    pub content_rating: Option<String>,
    pub date_added: Option<String>,
    pub play_params: Option<PlayParams>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LibraryAlbumRelationships {
    pub catalog: Option<Page<Album>>,
    pub tracks: Option<Page<Resource>>,
    pub artists: Option<Page<Resource>>,
}

pub type LibraryArtist = Item<LibraryArtistAttributes, LibraryArtistRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LibraryArtistAttributes {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LibraryArtistRelationships {
    pub catalog: Option<Page<Artist>>,
    pub albums: Option<Page<Resource>>,
}

pub type LibraryPlaylist = Item<LibraryPlaylistAttributes, LibraryPlaylistRelationships>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryPlaylistAttributes {
    pub name: String,
    #[serde(deserialize_with = "lenient")]
    pub description: Option<EditorialNotes>,
    pub artwork: Option<Artwork>,
    /// The user may rename it and change its tracks.
    #[serde(deserialize_with = "null_default")]
    pub can_edit: bool,
    #[serde(deserialize_with = "null_default")]
    pub is_public: bool,
    #[serde(deserialize_with = "null_default")]
    pub has_catalog: bool,
    pub date_added: Option<String>,
    pub last_modified_date: Option<String>,
    pub play_params: Option<PlayParams>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LibraryPlaylistRelationships {
    pub catalog: Option<Page<Playlist>>,
    pub tracks: Option<Page<Resource>>,
}

pub type LibraryPlaylistFolder = Item<LibraryPlaylistFolderAttributes, Empty>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryPlaylistFolderAttributes {
    pub name: String,
    pub date_added: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_fills_every_placeholder() {
        let art = Artwork {
            url: "https://is1-ssl.mzstatic.com/image/thumb/x/{w}x{h}{c}.{f}".into(),
            ..Default::default()
        };
        assert_eq!(
            art.url(300, 200),
            "https://is1-ssl.mzstatic.com/image/thumb/x/300x200bb.jpg"
        );
        let fixed = Artwork {
            url: "https://is1-ssl.mzstatic.com/image/thumb/y/{w}x{h}bb.jpg".into(),
            ..Default::default()
        };
        assert_eq!(
            fixed.url(64, 64),
            "https://is1-ssl.mzstatic.com/image/thumb/y/64x64bb.jpg"
        );
    }

    #[test]
    fn replaced_library_song_streams_by_reporting_id() {
        // A real library item whose catalog song Apple replaced: the old `catalogId` answers 404.
        let pp: PlayParams = serde_json::from_str(
            r#"{"catalogId":"1717441936","id":"i.AWPN7JqfLBkO936","isLibrary":true,"kind":"song",
                "reporting":true,"reportingId":"1872459410"}"#,
        )
        .unwrap();
        assert_eq!(pp.stream_catalog_id(), Some("1872459410"));
        let plain: PlayParams =
            serde_json::from_str(r#"{"id":"i.1","catalogId":"11","reportingId":"i.1"}"#).unwrap();
        assert_eq!(plain.stream_catalog_id(), Some("11"));
    }
}
