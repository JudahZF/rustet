//! The provider abstraction that decouples the engine from any one streaming service or DRM
//! system.
//!
//! A [`Provider`] turns an opaque asset ID into a [`Session`] of ready-to-run pipeline
//! [`Stream`]s. It handles service authentication, catalog resolution, variant selection and
//! key acquisition internally; none of that appears in the session. So playback, export and
//! queueing work the same whatever the source is.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::pipeline::{Codec, PipelineError, Stream, StreamKind};

/// The public properties of a media item. Holds no DRM material.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    pub title: String,
    pub artist_name: String,
    pub album_name: String,
    /// The catalog length, or 0 when unknown.
    pub duration_ms: u32,
    pub artwork_url: String,
    pub has_lyrics: bool,
}

/// How far a stream's declared length may differ from the catalog length before the stream's
/// length is reported instead. Smaller gaps are rounding and padding, not a real mismatch.
pub const DURATION_TOLERANCE_MS: u32 = 1000;

/// The duration to report for an item: the catalog length, unless the opened stream declares a
/// length that differs by more than [`DURATION_TOLERANCE_MS`]. Then the stream's length wins,
/// because that is where the player actually reaches the end. A length of 0 means unknown.
pub fn reconcile_duration_ms(catalog_ms: u32, stream_ms: u32) -> u32 {
    if stream_ms == 0 || (catalog_ms > 0 && catalog_ms.abs_diff(stream_ms) <= DURATION_TOLERANCE_MS)
    {
        catalog_ms
    } else {
        stream_ms
    }
}

/// The spatial rendering of an audio track. No value means normal stereo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialAudio {
    Binaural,
    BinauralLossless,
}

impl SpatialAudio {
    /// The wire name in the engine API.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binaural => "binaural",
            Self::BinauralLossless => "binaural-lossless",
        }
    }
}

/// Lazily opens one track: acquires its connections and licences and returns the stream.
#[async_trait]
pub trait OpenTrack: Send + Sync {
    /// The opened stream may keep using `cancel` for as long as it lives.
    async fn open(&self, cancel: &CancellationToken) -> Result<Stream, PipelineError>;
}

/// One decodable track of a [`Session`]. The provider sets `kind` and `codec`; callers pick a
/// track by the kind they want and never route on the codec.
#[derive(Clone)]
pub struct Track {
    pub kind: StreamKind,
    pub codec: Codec,
    /// MSE codec string from the HLS `CODECS=` attribute, for example `avc1.640028`.
    pub codec_string: String,
    /// Meaningful only for lossless audio; 0 otherwise.
    pub sample_rate: u32,
    /// Meaningful only for lossless audio; 0 otherwise.
    pub bit_depth: u32,
    /// Average bit rate in bits/s, 0 when unknown.
    pub bit_rate: u32,
    /// Number of audio channels, 0 when unknown.
    pub channel_count: u32,
    /// MIME type with codec parameter, for example `audio/mp4; codecs="mp4a.40.2"`.
    pub codec_mime_type: String,
    pub spatial_audio: Option<SpatialAudio>,
    pub open: Arc<dyn OpenTrack>,
}

/// What a session holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Song,
    MusicVideo,
}

impl SessionKind {
    /// The wire name in the engine API (`"song"`, `"mv"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Song => "song",
            Self::MusicVideo => "mv",
        }
    }
}

impl fmt::Display for SessionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The result of [`Provider::open`]: the item's metadata and every available track.
#[derive(Clone)]
pub struct Session {
    pub kind: SessionKind,
    pub metadata: Metadata,
    pub tracks: Vec<Track>,
    /// Available video variant heights from the HLS master playlist (music videos only).
    pub video_heights: Vec<u32>,
    /// The progressive CDN URL of the video (music videos only).
    pub mv_progressive_url: Option<String>,
    /// The token sent as the `downloadKey` cookie to the CDN, which uses it to authorise
    /// server-side decryption. Not key material, but auth material: never serialise or log it.
    pub mv_download_key: Option<String>,
}

/// What a provider needs to find and open an item. Not every provider uses every field.
/// Holds tokens, so it has no `Debug`.
#[derive(Clone, Default)]
pub struct OpenRequest {
    pub asset_id: String,
    pub storefront: String,
    /// Service bearer token, without the `Bearer ` prefix.
    pub token: String,
    /// Apple Media User Token. Other providers ignore it.
    pub mut_token: String,
    pub language: String,

    // Quality and capability hints: the provider picks the best match it supports.
    pub lossless: bool,
    /// With `lossless`: allow hi-res ALAC. Otherwise lossless is capped at 44.1 kHz / 16-bit.
    pub hi_res: bool,
    pub atmos: bool,
    /// Ask for the music video when there is one.
    pub video: bool,

    // Music-video variant selection.
    /// Highest video variant to pick, 0 for no limit.
    pub mv_max_height: u32,
    pub mv_audio_priorities: Vec<String>,
}

/// Turns an opaque asset ID into a media [`Session`]. Must be safe to call concurrently.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn open(
        &self,
        cancel: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<Session, PipelineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_duration() {
        for (name, catalog, stream, want) in [
            (
                "stream unknown keeps the catalog length",
                200_000,
                0,
                200_000,
            ),
            ("no catalog length takes the stream's", 0, 187_500, 187_500),
            ("same length", 200_000, 200_000, 200_000),
            (
                "padding inside the tolerance keeps the catalog length",
                200_000,
                200_900,
                200_000,
            ),
            (
                "exactly at the tolerance keeps the catalog length",
                200_000,
                199_000,
                200_000,
            ),
            ("shorter stream wins", 200_000, 187_000, 187_000),
            ("longer stream wins", 200_000, 214_500, 214_500),
            ("preview-length stream wins", 214_000, 30_000, 30_000),
        ] {
            assert_eq!(reconcile_duration_ms(catalog, stream), want, "{name}");
        }
    }
}
