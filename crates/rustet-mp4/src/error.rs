use crate::FourCC;

/// Everything that can go wrong while decoding, rewriting or decrypting MP4 data.
///
/// Malformed input is always an error, never a panic: fragments come straight from the
/// network.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The input ended inside a box (header or body). A clean end between two boxes is
    /// not an error: the readers return `Ok(None)` for it.
    #[error("input ended inside a box")]
    Truncated,
    #[error("invalid {kind} box: {reason}")]
    InvalidBox { kind: FourCC, reason: &'static str },
    #[error("missing {0} box")]
    MissingBox(&'static str),
    #[error("scheme type {0} not supported")]
    UnsupportedScheme(FourCC),
    /// The traf of an encrypted track has no `senc` box. Callers treat this as a clear
    /// fragment (Apple mixes clear and encrypted fragments).
    #[error("no senc box in traf")]
    NoSenc,
    #[error("senc: {0}")]
    Senc(&'static str),
    #[error("offset from saio ({saio}) relative moof start differs from senc data start {senc}")]
    SaioMismatch { saio: u64, senc: u64 },
    #[error("sample group: {0}")]
    SampleGroup(&'static str),
    #[error("sample data outside mdat: {0}")]
    SampleOutOfRange(&'static str),
    #[error("subsample {index} exceeds sample ({end} > {len})")]
    SubsampleOutOfRange { index: usize, end: u64, len: usize },
    #[error("{0} sample entry is not encrypted")]
    NotEncrypted(FourCC),
    #[error("trun data offset not set")]
    TrunDataOffsetNotSet,
    #[error("content key must be 16 bytes, got {0}")]
    KeyLength(usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
