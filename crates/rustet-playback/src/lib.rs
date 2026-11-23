//! Playback session lifecycle and the background pre-warm scheduler.
//!
//! - [`Manager`] opens playback sessions through a [`Provider`](rustet_core::media::Provider),
//!   keeps them alive while they are used, and runs their streams into any sink.
//! - [`prefetch::Scheduler`] opens sessions for upcoming tracks in the background so that real
//!   playback can claim them and skip the provider round-trip.
#![forbid(unsafe_code)]

pub mod manager;
pub mod prefetch;
pub mod session;

pub use manager::{ForegroundStats, Manager, OpenError, StreamClass, StreamError};
pub use session::{Capabilities, Session, StreamUrls};

/// A random 16-hex-digit ID for sessions and jobs.
fn new_id() -> String {
    let mut b = [0u8; 8];
    // Without the OS random source the IDs would collide; nothing useful can continue.
    getrandom::fill(&mut b).expect("OS random source unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Best-effort text of a caught panic.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}
