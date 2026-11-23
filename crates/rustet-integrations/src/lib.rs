//! Media integrations of the shell: MPRIS, system tray, Discord Rich Presence, Last.fm and ListenBrainz scrobbling, Windows media controls.
//!
//! No web view here: the shell (`rustet-shell`) calls these services from its bridge handlers and
//! forwards what they emit to the page or acts on it. Each service starts, fails and stops on
//! its own; a missing session bus, no Discord or no network only disables that one service.
//!
//! | Service | Input | Output |
//! |---|---|---|
//! | [`mpris::Mpris`] (Linux) | [`now_playing::MprisUpdate`] | [`mpris::MprisEvent`] |
//! | [`media_keys::MediaKeys`] (Linux, X11) | — | [`command::Transport`] |
//! | [`tray::Tray`] (Linux) | mini player open | [`tray::TrayEvent`] |
//! | [`discord::Discord`] | [`discord::PresenceUpdate`] | — |
//! | [`lastfm::Lastfm`], [`listenbrainz::ListenBrainz`] | [`scrobble::ScrobbleTrack`] | replies |
//! | [`win::WinIntegration`] | [`now_playing::MprisUpdate`] | [`command::PageCommand`] |
//!
//! Everything that controls playback ends as a [`command::PageCommand`] for `mpris:cmd`.
#![forbid(unsafe_code)]

pub mod command;
pub mod discord;
pub mod lastfm;
pub mod listenbrainz;
pub mod now_playing;
pub mod scrobble;
pub mod store;
pub mod win;

#[cfg(target_os = "linux")]
pub mod media_keys;
#[cfg(target_os = "linux")]
pub mod mpris;
#[cfg(target_os = "linux")]
pub mod tray;

pub use command::{Action, LoopStatus, PageCommand, Transport};
pub use store::SecretStore;
