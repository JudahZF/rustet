//! The track payload of `lastfm:nowplaying`, `lastfm:scrobble`, `lb:nowplaying` and `lb:scrobble`.
//!
//! When a track counts is decided by the renderer (`engine-playback.js`, Last.fm's rules: the
//! track is longer than 30 s and has played for min(4 min, half its length)); the services
//! here only submit what the page sends.

use serde::Deserialize;

/// A track as the page describes it. Payloads with wrong field types are rejected whole.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrobbleTrack {
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub track: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    /// Seconds.
    #[serde(default)]
    pub duration: Option<f64>,
    /// Unix seconds when the track started. Required for a scrobble.
    #[serde(default)]
    pub timestamp: Option<serde_json::Number>,
}

impl ScrobbleTrack {
    /// Artist and title are both required (`if (!t?.artist || !t?.track) return`).
    pub(crate) fn is_complete(&self) -> bool {
        !self.artist.is_empty() && !self.track.is_empty()
    }

    /// The scrobble timestamp, unless missing or zero (`!t?.timestamp`).
    pub(crate) fn timestamp(&self) -> Option<&serde_json::Number> {
        self.timestamp.as_ref().filter(|n| n.as_f64() != Some(0.0))
    }

    pub(crate) fn album(&self) -> Option<&str> {
        self.album.as_deref().filter(|s| !s.is_empty())
    }

    pub(crate) fn album_artist(&self) -> Option<&str> {
        self.album_artist.as_deref().filter(|s| !s.is_empty())
    }

    /// Whole seconds, when known and non-zero (`String(Math.round(t.duration))`).
    pub(crate) fn duration_secs(&self) -> Option<i64> {
        self.duration
            .filter(|d| d.is_finite() && *d != 0.0)
            // JavaScript's Math.round rounds halves up.
            .map(|d| (d + 0.5).floor() as i64)
    }
}
