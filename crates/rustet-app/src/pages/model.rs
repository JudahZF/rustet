//! The building blocks every page model is made of (native-UI spec §6.2): artwork, headers,
//! track rows, tiles, shelves, lists, and the empty and error states.

use std::time::Duration;

use rustet_ampapi::api::{ResourceRef, ResourceType};
use rustet_ampapi::model::AudioTrait;

use super::more::LoadMore;
use crate::route::Route;

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// `"495d7c"` or `"#495d7c"`.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.trim().trim_start_matches('#');
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Self {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
        })
    }
}

/// Colours Apple picked from an artwork: a background and four text colours, most prominent
/// first. Pages tint their header with these (album-art theming).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Palette {
    pub background: Rgb,
    pub text: [Rgb; 4],
}

/// An image on Apple's CDN: a URL template the view sizes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Artwork {
    /// Apple's template with `{w}`, `{h}` and sometimes `{c}` and `{f}`.
    pub template: String,
    /// The source size, when Apple gives it. Apple scales down only.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub palette: Option<Palette>,
}

impl Artwork {
    /// The image URL for `width` × `height` pixels (the drawn size times the scale factor),
    /// as JPEG with Apple's default crop.
    pub fn url(&self, width: u32, height: u32) -> String {
        self.template
            .replace("{w}", &width.to_string())
            .replace("{h}", &height.to_string())
            .replace("{c}", "bb")
            .replace("{f}", "jpg")
    }

    /// Width over height of the source, when known.
    pub fn aspect_ratio(&self) -> Option<f32> {
        match (self.width, self.height) {
            (Some(w), Some(h)) if h > 0 => Some(w as f32 / h as f32),
            _ => None,
        }
    }
}

/// A text with the route it opens (an artist name, an album name, a curator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    pub text: String,
    pub route: Option<Route>,
}

/// Editorial text, as plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notes {
    /// One or two lines for the header.
    pub short: Option<String>,
    /// The full text ("more").
    pub full: Option<String>,
}

/// A calendar date as Apple gives it (`2019-09-27`, sometimes only `2019`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub year: u16,
    pub month: Option<u8>,
    pub day: Option<u8>,
}

impl Date {
    /// `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, or the date part of an ISO 8601 time.
    pub fn parse(s: &str) -> Option<Self> {
        let date = s.split('T').next()?;
        let mut parts = date.split('-');
        let year = parts.next()?.parse().ok()?;
        let month = parts
            .next()
            .and_then(|m| m.parse().ok())
            .filter(|m| (1..=12).contains(m));
        let day = parts
            .next()
            .and_then(|d| d.parse().ok())
            .filter(|d| (1..=31).contains(d));
        Some(Self { year, month, day })
    }
}

/// The lossless mark of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lossless {
    /// Up to 24-bit/48 kHz.
    Lossless,
    /// Up to 24-bit/192 kHz.
    HiRes,
}

/// The quality marks drawn next to a title, from Apple's `audioTraits`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct QualityMarks {
    pub lossless: Option<Lossless>,
    pub dolby_atmos: bool,
    pub apple_digital_master: bool,
}

impl QualityMarks {
    pub fn new(traits: &[AudioTrait], apple_digital_master: bool) -> Self {
        let lossless = if traits.contains(&AudioTrait::HiResLossless) {
            Some(Lossless::HiRes)
        } else if traits.contains(&AudioTrait::Lossless) {
            Some(Lossless::Lossless)
        } else {
            None
        };
        Self {
            lossless,
            dolby_atmos: traits.contains(&AudioTrait::Atmos),
            apple_digital_master,
        }
    }
}

/// The header of an album, playlist, artist, station, curator or video page.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub title: String,
    /// The artist or curator under the title.
    pub subtitle: Option<Linked>,
    pub artwork: Option<Artwork>,
    pub notes: Option<Notes>,
    pub explicit: bool,
    pub quality: QualityMarks,
}

impl Header {
    /// The header colours, from the artwork.
    pub fn palette(&self) -> Option<Palette> {
        self.artwork.as_ref()?.palette
    }
}

/// Song or music video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Song,
    MusicVideo,
}

/// Which song a row is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TrackId {
    /// A catalog song or video.
    Catalog(String),
    /// A library song (`i.…`) and, when it is in the catalog, its catalog ID.
    Library {
        library_id: String,
        catalog_id: Option<String>,
    },
}

impl TrackId {
    /// The catalog ID the engine opens.
    pub fn catalog_id(&self) -> Option<&str> {
        match self {
            Self::Catalog(id) => Some(id),
            Self::Library { catalog_id, .. } => catalog_id.as_deref(),
        }
    }

    pub fn library_id(&self) -> Option<&str> {
        match self {
            Self::Catalog(_) => None,
            Self::Library { library_id, .. } => Some(library_id),
        }
    }
}

/// Whether a track can be played here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Availability {
    Playable,
    /// Not streamable in this storefront, or a library song that is not in the catalog. The
    /// row is drawn dimmed and play skips it.
    Unavailable,
}

/// One row of a track list.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackRow {
    pub id: TrackId,
    pub kind: TrackKind,
    /// The track number on its album.
    pub number: Option<u32>,
    pub disc: Option<u32>,
    pub title: String,
    pub explicit: bool,
    pub artist: Linked,
    pub album: Option<Linked>,
    pub duration: Option<Duration>,
    /// Apple's audio traits; [`TrackRow::quality`] gives the marks to draw.
    pub traits: Vec<AudioTrait>,
    pub apple_digital_master: bool,
    pub availability: Availability,
    pub artwork: Option<Artwork>,
    /// Popularity on its album, 0..=1 (album pages only).
    pub popularity: Option<f32>,
}

impl TrackRow {
    pub fn quality(&self) -> QualityMarks {
        QualityMarks::new(&self.traits, self.apple_digital_master)
    }

    pub fn is_playable(&self) -> bool {
        self.availability == Availability::Playable && self.id.catalog_id().is_some()
    }

    /// The catalog resource, for library adds and ratings.
    pub fn catalog_ref(&self) -> Option<ResourceRef> {
        let kind = match self.kind {
            TrackKind::Song => ResourceType::Songs,
            TrackKind::MusicVideo => ResourceType::MusicVideos,
        };
        Some(ResourceRef::new(kind, self.id.catalog_id()?))
    }

    /// The library resource, for playlist edits and library removal.
    pub fn library_ref(&self) -> Option<ResourceRef> {
        let kind = match self.kind {
            TrackKind::Song => ResourceType::LibrarySongs,
            TrackKind::MusicVideo => ResourceType::LibraryMusicVideos,
        };
        Some(ResourceRef::new(kind, self.id.library_id()?))
    }
}

/// What a tile shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TileKind {
    Album,
    Playlist,
    /// `live`: a live broadcast (Apple Music 1, …).
    Station {
        live: bool,
    },
    Artist,
    MusicVideo,
    Song,
    Curator,
    RecordLabel,
    /// An editorial category tile (search landing).
    Category,
}

/// A tile of a shelf or grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub kind: TileKind,
    pub title: String,
    pub subtitle: Option<String>,
    pub artwork: Option<Artwork>,
    /// Apple's label over a large tile ("NEW ALBUM").
    pub badge: Option<String>,
    pub explicit: bool,
    /// Where a click goes.
    pub route: Option<Route>,
    /// What the tile's play button plays.
    pub play: Option<super::play::PlayTarget>,
    /// The resource, for the context menu (library adds, ratings).
    pub item: Option<ResourceRef>,
}

/// A link of a links shelf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkItem {
    pub label: String,
    pub target: LinkTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Route(Route),
    /// A page Rustet does not show: open it in the browser.
    External(String),
}

/// How large a shelf's tiles are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TileSize {
    Regular,
    Large,
}

/// The items of a shelf.
#[derive(Debug, Clone, PartialEq)]
pub enum ShelfItems {
    /// A row of tiles.
    Tiles {
        tiles: Vec<Tile>,
        size: TileSize,
    },
    /// Large editorial tiles, one item each.
    Featured(Vec<Tile>),
    /// A compact track list.
    Tracks(Vec<TrackRow>),
    Links(Vec<LinkItem>),
}

impl ShelfItems {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Tiles { tiles, .. } | Self::Featured(tiles) => tiles.is_empty(),
            Self::Tracks(rows) => rows.is_empty(),
            Self::Links(links) => links.is_empty(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Tiles { tiles, .. } | Self::Featured(tiles) => tiles.len(),
            Self::Tracks(rows) => rows.len(),
            Self::Links(links) => links.len(),
        }
    }
}

/// A titled horizontal row. Pages never hold an empty shelf.
#[derive(Debug, Clone, PartialEq)]
pub struct Shelf {
    pub id: String,
    pub title: Option<String>,
    pub items: ShelfItems,
    pub see_all: Option<Route>,
}

/// A list that may continue: the items so far and how to get more.
#[derive(Debug, Clone, PartialEq)]
pub struct List<T> {
    pub items: Vec<T>,
    pub more: Option<LoadMore<T>>,
}

impl<T> List<T> {
    /// Appends a loaded page and takes its continuation.
    pub fn extend(&mut self, page: List<T>) {
        self.items.extend(page.items);
        self.more = page.more;
    }
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            more: None,
        }
    }
}

/// A full-page list: a grid of tiles or a track list.
#[derive(Debug, Clone, PartialEq)]
pub enum Listing {
    Tiles(List<Tile>),
    Tracks(List<TrackRow>),
}

impl Listing {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Tiles(list) => list.items.is_empty(),
            Self::Tracks(list) => list.items.is_empty(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Tiles(list) => list.items.len(),
            Self::Tracks(list) => list.items.len(),
        }
    }
}

/// What an empty page says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyState {
    pub title: String,
    pub message: String,
}

impl EmptyState {
    pub(crate) fn new(title: &str, message: &str) -> Self {
        Self {
            title: title.to_owned(),
            message: message.to_owned(),
        }
    }
}
