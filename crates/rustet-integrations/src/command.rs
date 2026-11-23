//! Commands sent to the Apple Music page on the `mpris:cmd` channel.
//!
//! Every integration that controls playback (MPRIS, media keys, the tray, the mini player,
//! Windows media controls) produces a [`PageCommand`]. The shell serialises it with serde and
//! sends it as the single argument of `mpris:cmd`; the renderer (`engine-playback.js`) handles it.

use serde::{Deserialize, Serialize};

/// A command for the page. Serialises to the JSON the renderer expects: a bare string for the
/// transport commands, an object with a `type` field for the others.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PageCommand {
    Transport(Transport),
    Action(Action),
}

/// The plain transport commands: `"play"`, `"pause"`, `"playpause"`, `"next"`, `"previous"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Play,
    Pause,
    #[serde(rename = "playpause")]
    PlayPause,
    Next,
    Previous,
}

/// Commands that carry a value: `{ "type": "seek", "deltaMs": … }` and so on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Action {
    /// Relative seek in milliseconds (MPRIS `Seek`).
    Seek {
        #[serde(rename = "deltaMs")]
        delta_ms: f64,
    },
    /// Absolute position in milliseconds (MPRIS `SetPosition`, mini player, SMTC).
    SetPosition {
        ms: f64,
    },
    Shuffle {
        value: bool,
    },
    SetLoopStatus {
        value: LoopStatus,
    },
    /// Volume 0–100.
    SetVolume {
        value: f64,
    },
    /// Cycle the repeat mode (mini player).
    Repeat,
    /// Toggle the favourite state of the current track (mini player).
    Love,
}

/// MPRIS `LoopStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LoopStatus {
    #[default]
    None,
    Track,
    Playlist,
}

impl LoopStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Track => "Track",
            Self::Playlist => "Playlist",
        }
    }

    /// MusicKit `repeatMode` (0 none, 1 one, 2 all) to MPRIS. Unknown values are `None`.
    pub fn from_repeat_mode(mode: i64) -> Self {
        match mode {
            1 => Self::Track,
            2 => Self::Playlist,
            _ => Self::None,
        }
    }
}

impl std::str::FromStr for LoopStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "None" => Ok(Self::None),
            "Track" => Ok(Self::Track),
            "Playlist" => Ok(Self::Playlist),
            _ => Err(()),
        }
    }
}

impl From<Transport> for PageCommand {
    fn from(t: Transport) -> Self {
        Self::Transport(t)
    }
}

impl From<Action> for PageCommand {
    fn from(a: Action) -> Self {
        Self::Action(a)
    }
}

impl PageCommand {
    /// `media:cmd` (the page's `navigator.mediaSession` handlers, Windows SMTC): only the
    /// transport commands and a finite, non-negative `setPosition` pass.
    pub fn from_media_session(value: &serde_json::Value) -> Option<Self> {
        match Self::deserialize(value).ok()? {
            cmd @ Self::Transport(_) => Some(cmd),
            cmd @ Self::Action(Action::SetPosition { ms }) if ms.is_finite() && ms >= 0.0 => {
                Some(cmd)
            }
            Self::Action(_) => None,
        }
    }

    /// `miniplayer:cmd`: any well-formed command with finite numbers. The Electron shell
    /// forwarded the payload unchecked; the renderer ignored what it did not know.
    pub fn from_mini_player(value: &serde_json::Value) -> Option<Self> {
        let cmd = Self::deserialize(value).ok()?;
        let finite = match cmd {
            Self::Action(Action::Seek { delta_ms: n })
            | Self::Action(Action::SetPosition { ms: n })
            | Self::Action(Action::SetVolume { value: n }) => n.is_finite(),
            _ => true,
        };
        finite.then_some(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_format_matches_the_renderer() {
        let cases = [
            (PageCommand::from(Transport::PlayPause), json!("playpause")),
            (Transport::Previous.into(), json!("previous")),
            (
                Action::Seek { delta_ms: -5000.0 }.into(),
                json!({ "type": "seek", "deltaMs": -5000.0 }),
            ),
            (
                Action::SetPosition { ms: 1.5 }.into(),
                json!({ "type": "setPosition", "ms": 1.5 }),
            ),
            (
                Action::Shuffle { value: true }.into(),
                json!({ "type": "shuffle", "value": true }),
            ),
            (
                Action::SetLoopStatus {
                    value: LoopStatus::Playlist,
                }
                .into(),
                json!({ "type": "setLoopStatus", "value": "Playlist" }),
            ),
            (
                Action::SetVolume { value: 40.0 }.into(),
                json!({ "type": "setVolume", "value": 40.0 }),
            ),
            (Action::Repeat.into(), json!({ "type": "repeat" })),
            (Action::Love.into(), json!({ "type": "love" })),
        ];
        for (cmd, wire) in cases {
            assert_eq!(serde_json::to_value(cmd).unwrap(), wire);
            assert_eq!(PageCommand::deserialize(&wire).unwrap(), cmd);
        }
    }

    #[test]
    fn media_session_accepts_only_transport_and_set_position() {
        for c in ["play", "pause", "playpause", "next", "previous"] {
            assert!(PageCommand::from_media_session(&json!(c)).is_some(), "{c}");
        }
        assert!(PageCommand::from_media_session(&json!("stop")).is_none());
        assert!(
            PageCommand::from_media_session(&json!({ "type": "setPosition", "ms": 0 })).is_some()
        );
        assert!(
            PageCommand::from_media_session(&json!({ "type": "setPosition", "ms": -1 })).is_none()
        );
        assert!(
            PageCommand::from_media_session(&json!({ "type": "setPosition", "ms": "5" })).is_none()
        );
        assert!(
            PageCommand::from_media_session(&json!({ "type": "setVolume", "value": 5 })).is_none()
        );
        assert!(PageCommand::from_media_session(&json!(null)).is_none());
    }

    #[test]
    fn mini_player_commands_are_checked() {
        for wire in [
            json!("next"),
            json!({ "type": "shuffle", "value": false }),
            json!({ "type": "repeat" }),
            json!({ "type": "love" }),
            json!({ "type": "setPosition", "ms": 1234 }),
            json!({ "type": "setVolume", "value": 0 }),
        ] {
            assert!(PageCommand::from_mini_player(&wire).is_some(), "{wire}");
        }
        for wire in [
            json!("rm -rf"),
            json!({ "type": "eval", "code": "x" }),
            json!({ "type": "shuffle", "value": "yes" }),
            json!({ "type": "setLoopStatus", "value": "Forever" }),
            json!(42),
        ] {
            assert!(PageCommand::from_mini_player(&wire).is_none(), "{wire}");
        }
    }

    #[test]
    fn repeat_mode_maps_one_to_track_and_all_to_playlist() {
        // MusicKit: 0 none, 1 one, 2 all (the old map swapped one and all).
        assert_eq!(LoopStatus::from_repeat_mode(0), LoopStatus::None);
        assert_eq!(LoopStatus::from_repeat_mode(1), LoopStatus::Track);
        assert_eq!(LoopStatus::from_repeat_mode(2), LoopStatus::Playlist);
        assert_eq!(LoopStatus::from_repeat_mode(7), LoopStatus::None);
    }
}
