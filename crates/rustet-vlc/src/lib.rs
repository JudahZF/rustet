//! In-process libvlc playback: lossless (ALAC) and AAC tracks, from engine memory or disk, and
//! music videos, whose frames go to a shared-memory frame ring ([`Deck::open_video`]).
//!
//! libvlc is loaded at run time, so the engine starts without VLC and reports it through
//! [`Error::Unavailable`]. The lookup order is the bundled `vlc/` directory next to the executable
//! (the resources layout of `scripts/bundle-vlc.sh` and `scripts/build-windows.ps1`), then the
//! platform library path (`LD_LIBRARY_PATH` on Linux, `PATH` on Windows). On Linux the bundled
//! plugins are found through `VLC_PLUGIN_PATH`, which the shell sets for the engine.
//!
//! One [`Instance`] carries any number of [`Deck`]s (media players). Two decks give gapless
//! pre-roll ([`Start::Paused`]) and crossfades ([`Ramp`]); each deck reports [`DeckEvent`]s on a
//! channel instead of being polled.

mod callbacks;
mod deck;
mod events;
mod ffi;
mod instance;
#[cfg(test)]
mod live_tests;
mod ramp;
mod source;
mod video;

pub use deck::{Deck, Start, State, Timing, Volume};
pub use events::{DeckEvent, DeckId, Event, LoadId};
pub use instance::Instance;
pub use ramp::Ramp;
pub use source::{AbortHandle, AsyncSource, Source};
pub use video::{VideoOutput, VideoRing};

/// The single-track player: a [`Deck`] on its own [`Instance`], without events
/// ([`Deck::new`]). Only one track plays at a time; loading replaces the current media.
pub type Player = Deck;

/// Why a libvlc operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// libvlc could not be loaded: lossless playback is unavailable.
    #[error("vlc: libvlc not available: {0}")]
    Unavailable(String),
    #[error("vlc: libvlc_new failed")]
    InstanceFailed,
    #[error("vlc: libvlc_media_player_new failed")]
    PlayerFailed,
    /// The player has been released.
    #[error("vlc: player closed")]
    Closed,
    #[error("vlc: URL contains a NUL byte")]
    InvalidUrl,
    #[error("vlc: libvlc_media_new_location failed for {url}")]
    MediaFailed { url: String },
    #[error("vlc: libvlc_media_new_callbacks failed")]
    CallbacksFailed,
    #[error("vlc: play failed (ret {0})")]
    PlayFailed(i32),
}
