//! The public session descriptor returned to API clients.

use rustet_core::media::{SessionKind, SpatialAudio};
use rustet_core::pipeline::Codec;
use serde::{Serialize, Serializer};

/// The public descriptor of one playback session, as `POST /api/v1/playback` returns it.
///
/// It holds no DRM material and is safe to serialise. Every field is client-visible; the JSON
/// field names are a contract with the renderer scripts.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(rename = "sessionId")]
    pub id: String,
    pub asset_id: String,
    pub storefront: String,
    #[serde(rename = "type", serialize_with = "wire")]
    pub kind: SessionKind,

    /// Format of the first audio track.
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "wire_opt")]
    pub codec: Option<Codec>,
    #[serde(skip_serializing_if = "is_zero")]
    pub sample_rate: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub bit_depth: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub bit_rate: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub channel_count: u32,
    #[serde(rename = "codecMimeType", skip_serializing_if = "String::is_empty")]
    pub codec_mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "wire_opt")]
    pub spatial_audio: Option<SpatialAudio>,

    pub capabilities: Capabilities,
    pub streams: StreamUrls,

    pub title: String,
    pub artist_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub album_name: String,
    pub duration_ms: u32,
    #[serde(rename = "artworkUrl")]
    pub artwork_url: String,

    /// Available H.264 heights from the HLS master playlist (music videos only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub video_heights: Vec<u32>,
    /// The requested maximum height of a music-video session, 0 for no limit.
    #[serde(rename = "mvMaxHeight", skip_serializing_if = "is_zero")]
    pub mv_max_height: u32,

    /// Seconds until the session expires if nobody uses it.
    pub expires_in: u64,
}

/// What a session can do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub audio: bool,
    pub video: bool,
    pub lyrics: bool,
    /// The audio stream supports the `?t=` restart.
    pub seekable: bool,
    /// fMP4 codec string for the MSE `SourceBuffer` (music videos only), empty otherwise.
    pub video_codec: String,
}

/// The engine routes that serve the session's streams.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StreamUrls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// A type with a fixed wire name in the engine API.
trait WireName {
    fn wire_name(&self) -> &'static str;
}

impl WireName for SessionKind {
    fn wire_name(&self) -> &'static str {
        self.as_str()
    }
}

impl WireName for Codec {
    fn wire_name(&self) -> &'static str {
        self.as_str()
    }
}

impl WireName for SpatialAudio {
    fn wire_name(&self) -> &'static str {
        self.as_str()
    }
}

fn wire<T: WireName, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(v.wire_name())
}

fn wire_opt<T: WireName, S: Serializer>(v: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_str(v.wire_name()),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The renderer reads these names; a rename breaks playback silently.
    #[test]
    fn json_field_names() {
        let s = Session {
            id: "abc".into(),
            asset_id: "1".into(),
            storefront: "us".into(),
            kind: SessionKind::MusicVideo,
            codec: Some(Codec::Alac),
            sample_rate: 96_000,
            bit_depth: 24,
            bit_rate: 0,
            channel_count: 2,
            codec_mime_type: String::new(),
            spatial_audio: Some(SpatialAudio::BinauralLossless),
            capabilities: Capabilities {
                audio: true,
                video: true,
                lyrics: false,
                seekable: true,
                video_codec: "avc1.640028".into(),
            },
            streams: StreamUrls {
                audio: Some("/api/v1/playback/abc/audio".into()),
                video: None,
            },
            title: "T".into(),
            artist_name: "A".into(),
            album_name: String::new(),
            duration_ms: 1000,
            artwork_url: "u".into(),
            video_heights: vec![720, 1080],
            mv_max_height: 0,
            expires_in: 14_400,
        };
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({
                "sessionId": "abc",
                "assetId": "1",
                "storefront": "us",
                "type": "mv",
                "codec": "alac",
                "sampleRate": 96000,
                "bitDepth": 24,
                "channelCount": 2,
                "spatialAudio": "binaural-lossless",
                "capabilities": {
                    "audio": true, "video": true, "lyrics": false, "seekable": true,
                    "videoCodec": "avc1.640028"
                },
                "streams": { "audio": "/api/v1/playback/abc/audio" },
                "title": "T",
                "artistName": "A",
                "durationMs": 1000,
                "artworkUrl": "u",
                "videoHeights": [720, 1080],
                "expiresIn": 14400
            })
        );
    }
}
