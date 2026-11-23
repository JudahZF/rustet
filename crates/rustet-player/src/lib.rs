//! The player controller: queue, playback state machine, quality choice, end-of-track rules, history and resume.
//!
//! Pure logic over the traits in [`ports`]; one task ([`Player::spawn`]) owns the state and runs
//! commands one at a time (spec §4.1). `rustet-engine` implements the ports and serves
//! [`model::Command`], [`model::PlayerState`] and [`model::Event`] over HTTP and SSE.
//!
//! Later work (spec §8 phase 3): crossfade and Sound Check are stored in the settings and
//! published, not applied; live (stream) stations are refused with `unsupported`.
//!
//! Music videos (spec §4.9) open in video mode and load with video on unless the user chose
//! audio only ([`PlayerHandle::set_mv_audio_only`]); the deck reports the frame ring it draws into
//! ([`ports::DeckEvent::Video`]) and [`model::PlayerState::video`] names it while that load is the
//! track.
#![forbid(unsafe_code)]

pub mod activity;
mod controller;
pub mod history;
pub mod model;
pub mod ports;
pub mod queue;
pub mod rules;
pub mod snapshot;

pub use controller::{Player, PlayerHandle};
