//! The two MPRIS interfaces at `/org/mpris/MediaPlayer2`.

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::mpsc;
use zbus::{
    fdo, interface,
    object_server::SignalEmitter,
    zvariant::{ObjectPath, OwnedValue},
};

use super::MprisEvent;
use crate::{
    command::{Action, LoopStatus, PageCommand, Transport},
    now_playing::PlaybackStatus,
};

/// `org.mpris.MediaPlayer2`.
pub(super) struct Root {
    pub events: mpsc::UnboundedSender<MprisEvent>,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {
        let _ = self.events.send(MprisEvent::Raise);
    }

    fn quit(&self) {
        let _ = self.events.send(MprisEvent::Quit);
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn fullscreen(&self) -> bool {
        false
    }

    /// `CanSetFullscreen` is false, so a write has no effect (spec).
    #[zbus(property)]
    fn set_fullscreen(&self, _value: bool) {}

    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        "Rustet"
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        "rustet"
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        vec!["audio/mpeg".into(), "audio/flac".into()]
    }
}

/// D-Bus metadata: `a{sv}`.
pub(super) type Metadata = HashMap<String, OwnedValue>;

/// What the player interface reports.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PlayerState {
    pub status: PlaybackStatus,
    pub metadata: Metadata,
    /// µs at `position_at`. Read on every `Position` get; never announced (spec:
    /// EmitsChangedSignal=false).
    pub position: i64,
    pub position_at: Instant,
    pub shuffle: bool,
    pub loop_status: LoopStatus,
    pub volume: f64,
}

impl PlayerState {
    /// The position now, in µs. Updates arrive when the track or the state changes, not every
    /// second, so a playing track runs on the clock from the last one, up to its length.
    pub fn position_now(&self) -> i64 {
        if self.status != PlaybackStatus::Playing {
            return self.position;
        }
        let elapsed = i64::try_from(self.position_at.elapsed().as_micros()).unwrap_or(i64::MAX);
        let now = self.position.saturating_add(elapsed);
        match self
            .metadata
            .get("mpris:length")
            .and_then(|v| i64::try_from(v).ok())
        {
            Some(length) if length > 0 => now.min(length),
            _ => now,
        }
    }
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            metadata: Metadata::new(),
            position: 0,
            position_at: Instant::now(),
            shuffle: false,
            loop_status: LoopStatus::None,
            // The page's volume starts at 100 and is reported only when it changes.
            volume: 1.0,
        }
    }
}

/// `org.mpris.MediaPlayer2.Player`. Control requests become page commands.
pub(super) struct Player {
    pub state: PlayerState,
    pub events: mpsc::UnboundedSender<MprisEvent>,
}

impl Player {
    fn send(&self, cmd: impl Into<PageCommand>) {
        let _ = self.events.send(MprisEvent::Command(cmd.into()));
    }
}

/// Longest `OpenUri` argument passed on.
const URI_MAX: usize = 2048;

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        self.send(Transport::Next);
    }

    fn previous(&self) {
        self.send(Transport::Previous);
    }

    fn pause(&self) {
        self.send(Transport::Pause);
    }

    fn play_pause(&self) {
        self.send(Transport::PlayPause);
    }

    /// There is no stopped state to go to: the page pauses.
    fn stop(&self) {
        self.send(Transport::Pause);
    }

    fn play(&self) {
        self.send(Transport::Play);
    }

    /// Relative seek in µs. The page answers with a `seeked` update once it has moved.
    fn seek(&self, offset: i64) {
        self.send(Action::Seek {
            delta_ms: offset as f64 / 1000.0,
        });
    }

    /// Absolute position in µs. Ignored unless `track_id` is the current track and the
    /// position is within it (spec).
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        let current = self
            .state
            .metadata
            .get("mpris:trackid")
            .and_then(|v| ObjectPath::try_from(v.clone()).ok());
        if current.is_some_and(|c| c != track_id) || position < 0 {
            return;
        }
        let length = self
            .state
            .metadata
            .get("mpris:length")
            .and_then(|v| i64::try_from(v).ok());
        if length.is_some_and(|l| l > 0 && position > l) {
            return;
        }
        self.send(Action::SetPosition {
            ms: position as f64 / 1000.0,
        });
    }

    /// `SupportedUriSchemes` is empty, so desktop clients do not call this; the shell decides
    /// whether a link it is handed is one Rustet opens.
    fn open_uri(&self, uri: String) {
        if uri.len() <= URI_MAX {
            let _ = self.events.send(MprisEvent::OpenUri(uri));
        }
    }

    #[zbus(signal)]
    pub(super) async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> &str {
        self.state.status.as_str()
    }

    #[zbus(property)]
    fn loop_status(&self) -> &str {
        self.state.loop_status.as_str()
    }

    /// The page applies the mode and reports it back; the new value is shown right away.
    #[zbus(property)]
    fn set_loop_status(&mut self, value: String) -> fdo::Result<()> {
        let status = value
            .parse()
            .map_err(|()| fdo::Error::InvalidArgs(format!("invalid loop status: {value}")))?;
        self.state.loop_status = status;
        self.send(Action::SetLoopStatus { value: status });
        Ok(())
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    /// Only 1.0 is supported (`MinimumRate` = `MaximumRate` = 1.0): writes are ignored.
    #[zbus(property)]
    fn set_rate(&self, _value: f64) {}

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.state.shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&mut self, value: bool) {
        self.state.shuffle = value;
        self.send(Action::Shuffle { value });
    }

    #[zbus(property)]
    fn metadata(&self) -> Metadata {
        self.state.metadata.clone()
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.state.volume
    }

    /// 0.0–1.0 (spec: values below 0 are 0). The page takes 0–100.
    #[zbus(property)]
    fn set_volume(&mut self, value: f64) {
        let value = if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            return;
        };
        self.state.volume = value;
        self.send(Action::SetVolume {
            value: (value * 100.0).round(),
        });
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.state.position_now()
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn can_control(&self) -> bool {
        true
    }
}
