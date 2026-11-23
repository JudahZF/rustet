//! Search response types.

use serde::{Deserialize, Serialize};

use crate::album::Album;
use crate::song::Song;
use crate::types::{List, Page, Resource};

/// The top-level response of the search API.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchResp {
    pub results: SearchResults,
}

/// The result groups; a group Apple did not return is absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchResults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub songs: Option<Page<Song>>,
    #[serde(rename = "music-videos", skip_serializing_if = "Option::is_none")]
    pub music_videos: Option<Page<SearchMusicVideo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub albums: Option<Page<Album>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artists: Option<Page<SearchArtist>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchMusicVideoAttributes {
    pub name: String,
    pub artist_name: String,
    pub url: String,
    pub genre_names: List<String>,
    pub duration_in_millis: u64,
}

pub type SearchMusicVideo = Resource<SearchMusicVideoAttributes>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchArtistAttributes {
    pub name: String,
    pub genre_names: List<String>,
    pub url: String,
}

pub type SearchArtist = Resource<SearchArtistAttributes>;
