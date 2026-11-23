//! Lossless (FairPlay) streams: the CBCS fragment decryption pipeline and ALAC sample
//! decryption through the DRM connection.
//!
//! - [`CbcsSource`] is the pipeline [`Source`](rustet_core::pipeline::Source) of a lossless
//!   track: it downloads the encrypted fMP4 and writes clear fMP4, seekable by segment.
//! - [`CbcsDialer`] opens the connections to the FairPlay decryptor; the DRM crate
//!   implements it. [`TcpDialer`] connects to a TCP address (tests).
//! - [`warm_cbcs_key`] and [`first_audio_key_uri`] pre-open the track's key before playback.
//! - [`alac`] has the fragment reader and the connection protocol.
#![forbid(unsafe_code)]

pub mod alac;
mod cbcs;
mod dial;

pub use cbcs::{CbcsSource, Error, PREFETCH_KEY, first_audio_key_uri, warm_cbcs_key};
pub use dial::{CbcsDialer, DrmIo, DrmStream, TcpDialer};
