//! Widevine CDM and key requests for AAC streams.
//!
//! [`wvkey::acquire_key`] is the entry point: it builds the init data for a track's key ID
//! ([`pssh::init_data`]), signs a licence request with the device [`Identity`] ([`Cdm`]), posts it
//! to Apple and unwraps the content key from the licence.
#![forbid(unsafe_code)]

mod cdm;
mod identity;
mod proto;
pub mod pssh;
pub mod wvkey;

pub use cdm::{Cdm, Key, KeyType};
pub use identity::{Identity, identity_dir};

/// Errors of the CDM and the licence request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid base64 KID")]
    KidEncoding,
    #[error("invalid KID length {0}, want 16 bytes")]
    KidLength(usize),
    #[error("init data not long enough")]
    InitDataTooShort,
    #[error("invalid PSSH header: {0}")]
    Pssh(#[source] prost::DecodeError),
    #[error("decode licence message: {0}")]
    Decode(#[source] prost::DecodeError),
    #[error("licence response has no licence")]
    MissingLicense,
    #[error("session key has invalid length {0}")]
    SessionKeyLength(usize),
    #[error("licence holds a malformed key")]
    InvalidKeyContainer,
    #[error("RSA: {0}")]
    Rsa(#[from] rsa::Error),
    #[error("licence request: {0}")]
    Http(#[from] reqwest::Error),
    #[error("licence request cancelled")]
    Cancelled,
    #[error("failed to parse response JSON: {0}")]
    ResponseJson(#[source] serde_json::Error),
    /// Apple's licence server answered with an error: it refused to grant the key (for example a
    /// quality tier this client may not play), as opposed to a request that failed in transit.
    #[error("error in license response, code: {code}, status: {status}")]
    LicenceRefused { code: i64, status: i64 },
    #[error("failed to decode license: {0}")]
    LicenceBase64(#[source] base64::DecodeError),
    #[error("licence holds no content key")]
    NoContentKey,
}
