//! Typed Apple Music API responses for the native UI.
//!
//! One shape for every resource: [`Item`] with typed attributes, relationships and views.
//! Lists that mix resource types decode into [`Resource`], an enum over the `type` tag. A
//! resource of an unknown type, or one whose attributes do not decode, becomes
//! [`Resource::Unknown`] instead of failing the page (risk R2 of the native-UI spec).
//!
//! Unknown fields are ignored. Fields Apple may leave out are `Option`; lists and flags
//! default to empty and `false`, also when Apple sends `null`.
//!
//! These types are separate from the crate-root types (`Album`, `Song`, …), which keep the
//! Go engine's JSON shape for the engine's routes.

mod de;
mod editorial;
mod resources;
mod responses;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

pub use editorial::{DisplayStyle, EditorialPage, Shelf, Tile};
pub use resources::*;
pub use responses::{
    Autoplay, Chart, Charts, LibrarySearchResults, SearchGroup, SearchResults, Suggestion,
};
pub(crate) use responses::{ChartsResponse, LibrarySearchResponse, SuggestionsResponse};

use de::null_default;

/// The relative URL of the next page of a list (`next` in Apple's responses), for
/// [`Api::next_page`](crate::Api::next_page).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub struct Cursor(pub(crate) String);

impl Cursor {
    /// A cursor from a path and query, for a caller that adds parameters to one.
    pub fn new(path_and_query: impl Into<String>) -> Self {
        Self(path_and_query.into())
    }

    /// The path and query, for example `/v1/catalog/us/playlists/pl.x/tracks?offset=100`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A list response or a relationship: `{href, next, data}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct Page<T> {
    #[serde(default)]
    pub href: Option<String>,
    /// The next page; `None` on the last one.
    #[serde(default)]
    pub next: Option<Cursor>,
    #[serde(default = "Vec::new", deserialize_with = "null_default")]
    pub data: Vec<T>,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self {
            href: None,
            next: None,
            data: Vec::new(),
        }
    }
}

/// A resource: `{id, href, attributes, relationships, views, meta}`. The `type` tag is in
/// the [`Resource`] variant (or implied by the relationship that holds the item).
///
/// `attributes` is `None` when Apple sent only a reference (`{id, type, href}`), as it does
/// for relationships it was not asked to include.
#[derive(Debug, Clone, Deserialize)]
#[serde(
    bound(deserialize = "A: Deserialize<'de>, R: Deserialize<'de> + Default, \
                             V: Deserialize<'de> + Default")
)]
pub struct Item<A, R = Empty, V = Empty> {
    pub id: String,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub attributes: Option<A>,
    #[serde(default, deserialize_with = "null_default")]
    pub relationships: R,
    #[serde(default, deserialize_with = "null_default")]
    pub views: V,
    #[serde(default, deserialize_with = "null_default")]
    pub meta: ItemMeta,
}

/// Relationships or views of a resource that has none the UI uses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Empty {}

/// The `meta` fields the UI uses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ItemMeta {
    /// Track popularity on an album, 0..=1 (`meta[albums:tracks]=popularity`).
    pub popularity: Option<f64>,
    /// The order Apple's own page lays the views out in.
    pub views: Option<ViewOrder>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ViewOrder {
    #[serde(deserialize_with = "null_default")]
    pub order: Vec<String>,
}

/// A view ("more by artist", "top songs", …): a titled list with a "see all" cursor.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct View {
    pub href: Option<String>,
    pub next: Option<Cursor>,
    pub attributes: Option<ViewAttributes>,
    #[serde(deserialize_with = "null_default")]
    pub data: Vec<Resource>,
}

impl View {
    pub fn title(&self) -> Option<&str> {
        self.attributes.as_ref()?.title.as_deref()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ViewAttributes {
    pub title: Option<String>,
}

/// Any resource, by its `type` tag.
#[derive(Debug, Clone)]
pub enum Resource {
    Song(Box<Song>),
    Album(Box<Album>),
    Artist(Box<Artist>),
    Playlist(Box<Playlist>),
    MusicVideo(Box<MusicVideo>),
    Station(Box<Station>),
    /// `apple-curators`: Apple's own curators (genres, shows).
    AppleCurator(Box<Curator>),
    /// `curators`: other curators (labels, brands).
    Curator(Box<Curator>),
    RecordLabel(Box<RecordLabel>),
    Genre(Box<Genre>),
    EditorialItem(Box<EditorialItem>),
    EditorialElement(Box<EditorialElement>),
    Room(Box<Room>),
    Grouping(Box<Grouping>),
    /// `personal-recommendation`: a shelf of Home or the search landing page.
    Recommendation(Box<Recommendation>),
    UploadedVideo(Box<UploadedVideo>),
    LibrarySong(Box<LibrarySong>),
    LibraryMusicVideo(Box<LibraryMusicVideo>),
    LibraryAlbum(Box<LibraryAlbum>),
    LibraryArtist(Box<LibraryArtist>),
    LibraryPlaylist(Box<LibraryPlaylist>),
    LibraryPlaylistFolder(Box<LibraryPlaylistFolder>),
    /// A type this crate does not model, or a resource whose attributes did not decode.
    Unknown {
        id: String,
        kind: String,
    },
}

impl Resource {
    pub fn id(&self) -> &str {
        match self {
            Self::Song(r) => &r.id,
            Self::Album(r) => &r.id,
            Self::Artist(r) => &r.id,
            Self::Playlist(r) => &r.id,
            Self::MusicVideo(r) => &r.id,
            Self::Station(r) => &r.id,
            Self::AppleCurator(r) | Self::Curator(r) => &r.id,
            Self::RecordLabel(r) => &r.id,
            Self::Genre(r) => &r.id,
            Self::EditorialItem(r) => &r.id,
            Self::EditorialElement(r) => &r.id,
            Self::Room(r) => &r.id,
            Self::Grouping(r) => &r.id,
            Self::Recommendation(r) => &r.id,
            Self::UploadedVideo(r) => &r.id,
            Self::LibrarySong(r) => &r.id,
            Self::LibraryMusicVideo(r) => &r.id,
            Self::LibraryAlbum(r) => &r.id,
            Self::LibraryArtist(r) => &r.id,
            Self::LibraryPlaylist(r) => &r.id,
            Self::LibraryPlaylistFolder(r) => &r.id,
            Self::Unknown { id, .. } => id,
        }
    }

    /// Apple's `type` tag, for example `"albums"`.
    pub fn kind(&self) -> &str {
        match self {
            Self::Song(_) => "songs",
            Self::Album(_) => "albums",
            Self::Artist(_) => "artists",
            Self::Playlist(_) => "playlists",
            Self::MusicVideo(_) => "music-videos",
            Self::Station(_) => "stations",
            Self::AppleCurator(_) => "apple-curators",
            Self::Curator(_) => "curators",
            Self::RecordLabel(_) => "record-labels",
            Self::Genre(_) => "genres",
            Self::EditorialItem(_) => "editorial-items",
            Self::EditorialElement(_) => "editorial-elements",
            Self::Room(_) => "rooms",
            Self::Grouping(_) => "groupings",
            Self::Recommendation(_) => "personal-recommendation",
            Self::UploadedVideo(_) => "uploaded-videos",
            Self::LibrarySong(_) => "library-songs",
            Self::LibraryMusicVideo(_) => "library-music-videos",
            Self::LibraryAlbum(_) => "library-albums",
            Self::LibraryArtist(_) => "library-artists",
            Self::LibraryPlaylist(_) => "library-playlists",
            Self::LibraryPlaylistFolder(_) => "library-playlist-folders",
            Self::Unknown { kind, .. } => kind,
        }
    }

    /// The display name, when the resource has one and its attributes were sent.
    pub fn name(&self) -> Option<&str> {
        let name = match self {
            Self::Song(r) => &r.attributes.as_ref()?.name,
            Self::Album(r) => &r.attributes.as_ref()?.name,
            Self::Artist(r) => &r.attributes.as_ref()?.name,
            Self::Playlist(r) => &r.attributes.as_ref()?.name,
            Self::MusicVideo(r) => &r.attributes.as_ref()?.name,
            Self::Station(r) => &r.attributes.as_ref()?.name,
            Self::AppleCurator(r) | Self::Curator(r) => &r.attributes.as_ref()?.name,
            Self::RecordLabel(r) => &r.attributes.as_ref()?.name,
            Self::Genre(r) => &r.attributes.as_ref()?.name,
            Self::UploadedVideo(r) => &r.attributes.as_ref()?.name,
            Self::LibrarySong(r) => &r.attributes.as_ref()?.name,
            Self::LibraryMusicVideo(r) => &r.attributes.as_ref()?.name,
            Self::LibraryAlbum(r) => &r.attributes.as_ref()?.name,
            Self::LibraryArtist(r) => &r.attributes.as_ref()?.name,
            Self::LibraryPlaylist(r) => &r.attributes.as_ref()?.name,
            Self::LibraryPlaylistFolder(r) => &r.attributes.as_ref()?.name,
            Self::Room(r) => r.attributes.as_ref()?.title.as_ref()?,
            Self::EditorialElement(r) => {
                let a = r.attributes.as_ref()?;
                a.name.as_ref().or(a.title.as_ref())?
            }
            Self::EditorialItem(_)
            | Self::Grouping(_)
            | Self::Recommendation(_)
            | Self::Unknown { .. } => return None,
        };
        Some(name.as_str()).filter(|n| !n.is_empty())
    }

    /// The tile artwork, when the resource has one and its attributes were sent.
    pub fn artwork(&self) -> Option<&Artwork> {
        match self {
            Self::Song(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::Album(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::Artist(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::Playlist(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::MusicVideo(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::Station(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::AppleCurator(r) | Self::Curator(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::RecordLabel(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::UploadedVideo(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::LibrarySong(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::LibraryMusicVideo(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::LibraryAlbum(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::LibraryPlaylist(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::EditorialElement(r) => r.attributes.as_ref()?.artwork.as_ref(),
            Self::EditorialItem(r) => r.attributes.as_ref()?.editorial_artwork.values().next(),
            Self::Genre(_)
            | Self::Room(_)
            | Self::Grouping(_)
            | Self::Recommendation(_)
            | Self::LibraryArtist(_)
            | Self::LibraryPlaylistFolder(_)
            | Self::Unknown { .. } => None,
        }
    }

    fn from_value(value: &Value) -> Self {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let decoded = match kind {
            "songs" => decode(value, Self::Song),
            "albums" => decode(value, Self::Album),
            "artists" => decode(value, Self::Artist),
            "playlists" => decode(value, Self::Playlist),
            "music-videos" => decode(value, Self::MusicVideo),
            "stations" => decode(value, Self::Station),
            "apple-curators" => decode(value, Self::AppleCurator),
            "curators" => decode(value, Self::Curator),
            "record-labels" => decode(value, Self::RecordLabel),
            "genres" => decode(value, Self::Genre),
            "editorial-items" => decode(value, Self::EditorialItem),
            "editorial-elements" => decode(value, Self::EditorialElement),
            "rooms" => decode(value, Self::Room),
            "groupings" => decode(value, Self::Grouping),
            "personal-recommendation" => decode(value, Self::Recommendation),
            "uploaded-videos" => decode(value, Self::UploadedVideo),
            "library-songs" => decode(value, Self::LibrarySong),
            "library-music-videos" => decode(value, Self::LibraryMusicVideo),
            "library-albums" => decode(value, Self::LibraryAlbum),
            "library-artists" => decode(value, Self::LibraryArtist),
            "library-playlists" => decode(value, Self::LibraryPlaylist),
            "library-playlist-folders" => decode(value, Self::LibraryPlaylistFolder),
            _ => None,
        };
        decoded.unwrap_or_else(|| Self::Unknown {
            id: value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            kind: kind.to_owned(),
        })
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    value: &Value,
    wrap: fn(Box<T>) -> Resource,
) -> Option<Resource> {
    match T::deserialize(value) {
        Ok(item) => Some(wrap(Box::new(item))),
        Err(err) => {
            let kind = value.get("type").and_then(Value::as_str);
            let id = value.get("id").and_then(Value::as_str);
            tracing::debug!(?kind, ?id, "resource kept as unknown: {err}");
            None
        }
    }
}

impl<'de> Deserialize<'de> for Resource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(&Value::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_types_and_bad_attributes_do_not_fail_the_list() {
        let page: Page<Resource> = serde_json::from_str(
            r#"{"data":[
                {"id":"1","type":"albums","attributes":{"name":"A","trackCount":3}},
                {"id":"2","type":"concerts","attributes":{"name":"C"}},
                {"id":"3","type":"songs","attributes":{"name":7}},
                {"id":"4","type":"songs"}
            ],"next":"/v1/x?offset=4"}"#,
        )
        .unwrap();
        assert_eq!(page.next.unwrap().as_str(), "/v1/x?offset=4");
        let [album, concert, bad, reference] = &page.data[..] else {
            panic!("four items")
        };
        assert!(matches!(album, Resource::Album(a)
            if a.attributes.as_ref().unwrap().track_count == Some(3)));
        assert!(
            matches!(concert, Resource::Unknown { id, kind } if id == "2" && kind == "concerts")
        );
        assert!(matches!(bad, Resource::Unknown { kind, .. } if kind == "songs"));
        assert!(matches!(reference, Resource::Song(s) if s.attributes.is_none()));
        assert_eq!(album.name(), Some("A"));
    }

    #[test]
    fn null_lists_and_flags_read_as_empty() {
        let song: Song = serde_json::from_str(
            r#"{"id":"1","attributes":{"name":"S","genreNames":null,"hasLyrics":null,
                "audioTraits":["lossless","hi-res-lossless","spatial-x"]},"relationships":null}"#,
        )
        .unwrap();
        let a = song.attributes.unwrap();
        assert!(a.genre_names.is_empty() && !a.has_lyrics);
        assert_eq!(
            a.audio_traits,
            [
                AudioTrait::Lossless,
                AudioTrait::HiResLossless,
                AudioTrait::Other("spatial-x".into())
            ]
        );
    }
}
