//! The JSON dump inside `library.enc`. Its layout is shared with the Go engine, so a
//! cache written by either one loads in the other.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{PlaylistInfo, PlaylistTrack};

/// The layout version of the dump. A dump from a newer layout is ignored (and rebuilt
/// by the next sync) rather than half-understood.
pub(crate) const CACHE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct DiskCache {
    #[serde(skip_serializing_if = "is_zero")]
    pub version: u32,
    #[serde(deserialize_with = "nullable")]
    pub songs: Vec<SongInfo>,
    #[serde(deserialize_with = "nullable")]
    pub albums: Vec<AlbumInfo>,
    #[serde(deserialize_with = "nullable")]
    pub playlists: Vec<PlaylistInfo>,
    #[serde(deserialize_with = "nullable")]
    pub playlist_tracks: BTreeMap<String, Vec<PlaylistTrack>>,
    #[serde(with = "go_time")]
    pub synced_at: Option<DateTime<Utc>>,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub revision: String,
}

/// One cached library song.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SongInfo {
    #[serde(deserialize_with = "nullable")]
    pub lid: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub cid: String,
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub artist: String,
    #[serde(deserialize_with = "nullable")]
    pub album: String,
    #[serde(
        rename = "albumId",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub album_id: String,
    #[serde(deserialize_with = "nullable")]
    pub ms: u64,
    #[serde(skip_serializing_if = "is_zero", deserialize_with = "nullable")]
    pub track: u32,
    #[serde(skip_serializing_if = "is_zero", deserialize_with = "nullable")]
    pub disc: u32,
}

/// One cached library album.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AlbumInfo {
    #[serde(deserialize_with = "nullable")]
    pub lid: String,
    #[serde(
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub cid: String,
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(deserialize_with = "nullable")]
    pub artist: String,
    #[serde(rename = "trackCount", deserialize_with = "nullable")]
    pub track_count: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Reads JSON `null` as the default value, as Go's decoder does.
pub(crate) fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Go `time.Time` in JSON: RFC 3339, with the zero time `0001-01-01T00:00:00Z` meaning
/// "never". `None` maps to the zero time in both directions.
pub(crate) mod go_time {
    use super::*;
    use serde::{Serializer, de::Error};

    const GO_ZERO: &str = "0001-01-01T00:00:00Z";
    /// Unix seconds of Go's zero time.
    const GO_ZERO_UNIX: i64 = -62_135_596_800;

    pub(crate) fn serialize<S: Serializer>(
        t: &Option<DateTime<Utc>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&t.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
            None => s.serialize_str(GO_ZERO),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => parse(&s).map_err(D::Error::custom),
            None => Ok(None),
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Option<DateTime<Utc>>, chrono::ParseError> {
        let t = DateTime::parse_from_rfc3339(s)?.to_utc();
        Ok((t.timestamp() != GO_ZERO_UNIX || t.timestamp_subsec_nanos() != 0).then_some(t))
    }
}
