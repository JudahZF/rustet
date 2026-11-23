//! Responses that are not a plain `{data, next}` list.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::de::null_default;
use super::{Cursor, Page, Resource};

/// One chart: `{chart, name, data, next}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Chart {
    /// `most-played`, `city-top`, `daily-global-top`.
    pub chart: Option<String>,
    pub name: String,
    pub href: Option<String>,
    pub next: Option<Cursor>,
    #[serde(deserialize_with = "null_default")]
    pub data: Vec<Resource>,
}

/// `GET /v1/catalog/{sf}/charts`: each group is a list of charts (usually one).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Charts {
    #[serde(deserialize_with = "null_default")]
    pub songs: Vec<Chart>,
    #[serde(deserialize_with = "null_default")]
    pub albums: Vec<Chart>,
    #[serde(deserialize_with = "null_default")]
    pub playlists: Vec<Chart>,
    #[serde(rename = "music-videos", deserialize_with = "null_default")]
    pub music_videos: Vec<Chart>,
    /// City charts (playlists).
    #[serde(deserialize_with = "null_default")]
    pub city_charts: Vec<Chart>,
    /// Daily Top 100 (playlists).
    #[serde(deserialize_with = "null_default")]
    pub daily_global_top_charts: Vec<Chart>,
}

#[derive(Deserialize)]
pub(crate) struct ChartsResponse {
    #[serde(default)]
    pub results: Charts,
}

/// One group of search results: `top`, `artist`, `album`, `song`, `playlist`, `station`,
/// `music_video`, `radio_episode`, …
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchGroup {
    /// The group key, for example `music_video`.
    pub group_id: String,
    /// Apple's localised heading, for example `"Music Videos"`.
    pub name: Option<String>,
    pub href: Option<String>,
    /// More of this group ([`Api::search_next`](crate::Api::search_next)).
    pub next: Option<Cursor>,
    #[serde(deserialize_with = "null_default")]
    pub data: Vec<Resource>,
}

/// Search results in the order Apple's own page shows them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "RawSearch")]
pub struct SearchResults {
    pub groups: Vec<SearchGroup>,
}

impl SearchResults {
    pub fn group(&self, group_id: &str) -> Option<&SearchGroup> {
        self.groups.iter().find(|g| g.group_id == group_id)
    }
}

#[derive(Deserialize)]
struct RawSearch {
    #[serde(default)]
    results: BTreeMap<String, SearchGroup>,
    #[serde(default)]
    meta: Option<RawSearchMeta>,
}

#[derive(Deserialize)]
struct RawSearchMeta {
    results: Option<RawSearchOrder>,
}

#[derive(Deserialize)]
struct RawSearchOrder {
    #[serde(default)]
    order: Vec<String>,
}

impl From<RawSearch> for SearchResults {
    /// Groups in `meta.results.order` first, then any group the order does not name.
    fn from(raw: RawSearch) -> Self {
        let mut results = raw.results;
        for (key, group) in &mut results {
            if group.group_id.is_empty() {
                group.group_id.clone_from(key);
            }
        }
        let order = raw
            .meta
            .and_then(|m| m.results)
            .map(|r| r.order)
            .unwrap_or_default();
        let mut groups: Vec<_> = order.iter().filter_map(|k| results.remove(k)).collect();
        groups.extend(results.into_values());
        Self { groups }
    }
}

/// A search hint while typing.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum Suggestion {
    /// A completed search term.
    #[serde(rename = "terms", rename_all = "camelCase")]
    Term {
        search_term: String,
        display_term: String,
    },
    /// A resource to open directly.
    #[serde(rename = "topResults")]
    TopResult { content: Resource },
    /// A suggestion kind this crate does not know.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
pub(crate) struct SuggestionsResponse {
    #[serde(default)]
    pub results: SuggestionsResults,
}

#[derive(Default, Deserialize)]
pub(crate) struct SuggestionsResults {
    #[serde(default)]
    pub suggestions: Vec<Suggestion>,
}

/// Library search results by library type.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LibrarySearchResults {
    #[serde(rename = "library-songs")]
    pub songs: Option<Page<Resource>>,
    #[serde(rename = "library-albums")]
    pub albums: Option<Page<Resource>>,
    #[serde(rename = "library-artists")]
    pub artists: Option<Page<Resource>>,
    #[serde(rename = "library-playlists")]
    pub playlists: Option<Page<Resource>>,
    #[serde(rename = "library-music-videos")]
    pub music_videos: Option<Page<Resource>>,
}

#[derive(Deserialize)]
pub(crate) struct LibrarySearchResponse {
    #[serde(default)]
    pub results: LibrarySearchResults,
}

/// An autoplay station and its first tracks (`/v1/me/stations/continuous`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "RawAutoplay")]
pub struct Autoplay {
    pub station: Option<Resource>,
    pub tracks: Vec<Resource>,
}

#[derive(Deserialize)]
struct RawAutoplay {
    #[serde(default)]
    results: RawAutoplayResults,
}

#[derive(Default, Deserialize)]
struct RawAutoplayResults {
    /// MusicKit accepts an object or a list here; it uses the first.
    #[serde(default)]
    station: Value,
    #[serde(default, deserialize_with = "null_default")]
    tracks: Vec<Resource>,
}

impl From<RawAutoplay> for Autoplay {
    fn from(raw: RawAutoplay) -> Self {
        let station = match raw.results.station {
            Value::Array(list) => list.into_iter().next(),
            Value::Null => None,
            object => Some(object),
        };
        Self {
            station: station.map(|s| Resource::from_value(&s)),
            tracks: raw.results.tracks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_groups_follow_apple_order_then_the_rest() {
        let results: SearchResults = serde_json::from_str(
            r#"{"results":{
                "album":{"groupId":"album","data":[]},
                "song":{"data":[{"id":"1","type":"songs"}],"next":"/v1/catalog/us/search?offset=1"},
                "zzz":{"groupId":"zzz"}},
              "meta":{"results":{"order":["song","album","missing"]}}}"#,
        )
        .unwrap();
        let ids: Vec<_> = results.groups.iter().map(|g| g.group_id.as_str()).collect();
        assert_eq!(ids, ["song", "album", "zzz"]);
        assert!(results.group("song").unwrap().next.is_some());
    }

    #[test]
    fn suggestions_of_unknown_kind_are_kept_as_unknown() {
        let s: SuggestionsResponse = serde_json::from_str(
            r#"{"results":{"suggestions":[
                {"kind":"terms","searchTerm":"a b","displayTerm":"A B"},
                {"kind":"topResults","content":{"id":"1","type":"artists","attributes":{"name":"X"}}},
                {"kind":"lyrics","text":"…"}]}}"#,
        )
        .unwrap();
        let [term, top, unknown] = &s.results.suggestions[..] else {
            panic!("three suggestions")
        };
        assert!(matches!(term, Suggestion::Term { search_term, .. } if search_term == "a b"));
        assert!(matches!(top, Suggestion::TopResult { content } if content.name() == Some("X")));
        assert!(matches!(unknown, Suggestion::Unknown));
    }

    #[test]
    fn autoplay_station_may_be_an_object_or_a_list() {
        for station in [
            r#"{"id":"s","type":"stations"}"#,
            r#"[{"id":"s","type":"stations"}]"#,
        ] {
            let a: Autoplay = serde_json::from_str(&format!(
                r#"{{"results":{{"station":{station},"tracks":[{{"id":"1","type":"songs"}}]}}}}"#
            ))
            .unwrap();
            assert_eq!(a.station.unwrap().id(), "s");
            assert_eq!(a.tracks.len(), 1);
        }
    }
}
