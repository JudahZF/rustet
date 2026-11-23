//! AutoEQ headphone correction, binaural (HRTF) rendering through FFmpeg, and Cavern.
//!
//! These are finished but not yet wired into playback or the UI.
#![forbid(unsafe_code)]

pub mod autoeq;
pub mod binaural;
pub mod cavern;
pub mod hrtf;
