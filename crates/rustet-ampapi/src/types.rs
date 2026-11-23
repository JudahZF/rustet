//! Serde types shared by the catalog resources.
//!
//! Field names and order follow the Go engine's JSON tags. The album and playlist
//! responses are re-serialised to the renderer unchanged, so absent and `null` arrays
//! must come back out as `null` (Go's nil slice), see [`List`].

use std::ops::{Deref, DerefMut};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A JSON array that remembers whether it was absent or `null`.
///
/// It reads like a slice. An absent or `null` array serialises as `null`, a present one
/// (even empty) as an array, as Go's `[]T` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List<T>(Option<Vec<T>>);

impl<T> Default for List<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> Deref for List<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.0.as_deref().unwrap_or_default()
    }
}

impl<T> DerefMut for List<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.0.as_deref_mut().unwrap_or_default()
    }
}

impl<T> From<Vec<T>> for List<T> {
    fn from(v: Vec<T>) -> Self {
        Self(Some(v))
    }
}

impl<T> Extend<T> for List<T> {
    /// Like Go's `append`: appending nothing to an absent list keeps it absent.
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let mut iter = iter.into_iter().peekable();
        if iter.peek().is_some() {
            self.0.get_or_insert_with(Vec::new).extend(iter);
        }
    }
}

impl<T> IntoIterator for List<T> {
    type Item = T;
    type IntoIter = std::iter::Flatten<std::option::IntoIter<Vec<T>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter().flatten()
    }
}

impl<'a, T> IntoIterator for &'a List<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: Serialize> Serialize for List<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for List<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::deserialize(deserializer).map(Self)
    }
}

/// A list response: `{href, next, data}`. `next` is the relative URL of the next page,
/// empty on the last one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Page<T> {
    pub href: String,
    pub next: String,
    pub data: List<T>,
}

/// A relationship: `{href, data}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Relationship<T> {
    pub href: String,
    pub data: List<T>,
}

/// A resource without relationships: `{id, type, href, attributes}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Resource<A> {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub href: String,
    pub attributes: A,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Artwork {
    pub width: u32,
    pub height: u32,
    /// Template URL with `{w}` and `{h}` placeholders.
    pub url: String,
    pub bg_color: String,
    pub text_color1: String,
    pub text_color2: String,
    pub text_color3: String,
    pub text_color4: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayParams {
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preview {
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExtendedAssetUrls {
    /// The master HLS playlist with the lossless and spatial variants.
    pub enhanced_hls: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtistAttributes {
    pub name: String,
}

/// A related artist: `{id, type, href, attributes: {name}}`.
pub type Artist = Resource<ArtistAttributes>;

/// The album a song or music video belongs to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RelatedAlbumAttributes {
    pub artist_name: String,
    pub artwork: Artwork,
    pub genre_names: List<String>,
    pub is_compilation: bool,
    pub is_complete: bool,
    pub is_mastered_for_itunes: bool,
    pub is_prerelease: bool,
    pub is_single: bool,
    pub name: String,
    pub play_params: PlayParams,
    pub release_date: String,
    pub record_label: String,
    pub copyright: String,
    pub track_count: u32,
    pub upc: String,
    pub url: String,
}

pub type RelatedAlbum = Resource<RelatedAlbumAttributes>;

/// Relationships of a song or music video.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaRelationships {
    pub artists: Relationship<Artist>,
    pub albums: Relationship<RelatedAlbum>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_keeps_null_and_empty_apart() {
        let absent: Page<u8> = serde_json::from_str("{}").unwrap();
        let null: Page<u8> = serde_json::from_str(r#"{"data":null}"#).unwrap();
        let empty: Page<u8> = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert!(absent.data.is_empty() && null.data.is_empty() && empty.data.is_empty());
        assert_eq!(
            serde_json::to_string(&absent).unwrap(),
            r#"{"href":"","next":"","data":null}"#
        );
        assert_eq!(
            serde_json::to_string(&null).unwrap(),
            r#"{"href":"","next":"","data":null}"#
        );
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"href":"","next":"","data":[]}"#
        );

        let mut list = List::default();
        list.extend(Vec::<u8>::new());
        assert_eq!(
            list,
            List::default(),
            "appending nothing keeps the list absent"
        );
        list.extend([1, 2]);
        assert_eq!(&*list, &[1, 2]);
    }
}
