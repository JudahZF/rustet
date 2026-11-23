//! The `mpris:update` payload: the page's now-playing state.
//!
//! The renderer sends partial updates (`{ status, position, seeked }`, `{ metadata, shuffle }`,
//! `{ volume, repeat, shuffle, loved }`, …). Each field is read on its own and a field of the
//! wrong type is ignored, so one bad field never drops the rest. Keys the integrations do not use
//! (`loved`, `qualityBadge`) are for the mini player, which gets the raw payload from the shell.

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::command::LoopStatus;

/// MPRIS `PlaybackStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl PlaybackStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "Playing" => Some(Self::Playing),
            "Paused" => Some(Self::Paused),
            "Stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

/// One `mpris:update` from the page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MprisUpdate {
    pub status: Option<PlaybackStatus>,
    /// MPRIS metadata as the page builds it (`mpris:trackid`, `mpris:length` in µs,
    /// `mpris:artUrl`, `xesam:*`). Converted to D-Bus types by the MPRIS service.
    pub metadata: Option<Map<String, Value>>,
    /// Playback position in microseconds, never negative.
    pub position_us: Option<i64>,
    pub shuffle: Option<bool>,
    /// From MusicKit's `repeatMode` (`repeat` in the payload).
    pub loop_status: Option<LoopStatus>,
    /// 0.0–1.0, from the page's 0–100.
    pub volume: Option<f64>,
    /// The page asks for the MPRIS `Seeked` signal (after a seek, or on resume).
    pub seeked: bool,
}

impl MprisUpdate {
    /// Reads the fields of a payload; anything that is not an object is an empty update.
    pub fn from_page(v: &Value) -> Self {
        let num = |k: &str| v.get(k).and_then(Value::as_f64).filter(|n| n.is_finite());
        Self {
            status: v
                .get("status")
                .and_then(Value::as_str)
                .and_then(PlaybackStatus::parse),
            metadata: v.get("metadata").and_then(Value::as_object).cloned(),
            position_us: num("position").map(|p| p.max(0.0).floor() as i64),
            shuffle: v.get("shuffle").and_then(Value::as_bool),
            loop_status: num("repeat").map(|r| LoopStatus::from_repeat_mode(r as i64)),
            volume: num("volume").map(|n| (n / 100.0).clamp(0.0, 1.0)),
            seeked: v.get("seeked").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

impl<'de> Deserialize<'de> for MprisUpdate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::from_page(&Value::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fields_are_read_independently() {
        let u = MprisUpdate::from_page(&json!({
            "status": "Playing", "position": 1_500_000.7, "seeked": true,
            "shuffle": "yes", "repeat": 2, "volume": 150, "loved": true,
        }));
        assert_eq!(u.status, Some(PlaybackStatus::Playing));
        assert_eq!(u.position_us, Some(1_500_000));
        assert!(u.seeked);
        assert_eq!(u.shuffle, None, "wrong type ignored");
        assert_eq!(u.loop_status, Some(LoopStatus::Playlist));
        assert_eq!(u.volume, Some(1.0), "clamped");
        assert!(u.metadata.is_none());

        let u =
            MprisUpdate::from_page(&json!({ "status": "Buffering", "position": -4, "volume": 25 }));
        assert_eq!(u.status, None);
        assert_eq!(u.position_us, Some(0));
        assert_eq!(u.volume, Some(0.25));
        assert_eq!(MprisUpdate::from_page(&json!("x")), MprisUpdate::default());
    }
}
