//! Apple Music media provider: catalog to HLS to playable sessions.
//!
//! [`AppleMusicProvider`] implements [`rustet_core::media::Provider`]. It turns a catalog ID into a
//! [`Session`](rustet_core::media::Session):
//!
//! - Songs: the catalog's audio traits and the request pick AAC, lossless ALAC (CD or hi-res) or Atmos. AAC plays
//!   the webplayback API's CTR / Widevine asset through `rustet-streams`; ALAC and Atmos play the catalog's
//!   enhanced-HLS variant through the CBCS dialer of `rustet-fairplay`.
//! - Music videos: the webplayback master playlist, an audio rendition by priority and a video variant by height.
//!   A refused video licence steps down to the next lower tier and is remembered for an hour.
//!
//! Failures are [`Error`]s, classified for the playback handlers. The engine supplies the DRM session through
//! [`AccountTokenSource`] and [`rustet_fairplay::CbcsDialer`].
#![forbid(unsafe_code)]

mod error;
mod provider;
mod tracks;
mod webplayback;

pub use error::{Cause, Error, Failure};
pub use provider::{AccountTokenSource, AppleMusicProvider, ProgressiveMvUrl};
pub use webplayback::AssetFlavor;
