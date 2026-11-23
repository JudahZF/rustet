use std::io;

use rustet_core::pipeline::PipelineError;

/// Errors of segment downloads, key acquisition and stream rewriting.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The caller's cancellation token fired.
    #[error("cancelled")]
    Cancelled,
    #[error("fetch: {0}")]
    Fetch(#[source] reqwest::Error),
    /// No response headers within the CDN header timeout (15 s).
    #[error("fetch: timed out waiting for response headers")]
    HeaderTimeout,
    /// A byte-range request was answered with the whole file.
    #[error("HTTP 200 for range {0} (server ignored Range)")]
    RangeIgnored(String),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("read body: {0}")]
    Body(#[source] reqwest::Error),
    #[error("short range body: {got} of {want} bytes")]
    ShortRange { got: u64, want: u64 },
    /// Writing to the output, or reading the input of a rewriting stage, failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A download of one segment of a list failed.
    #[error("segment {index}: {source}")]
    Segment {
        index: usize,
        #[source]
        source: Box<Error>,
    },
    /// Malformed or unsupported MP4 data. `context` names the step.
    #[error("{context}: {source}")]
    Mp4 {
        context: &'static str,
        #[source]
        source: rustet_mp4::Error,
    },
    #[error("init segment: no moov box found")]
    NoMoov,
    /// The licence request failed. [`Error::is_licence_refused`] tells a refusal from a transport failure.
    #[error("fairplay licence: {0}")]
    Licence(#[source] rustet_widevine::Error),
    /// The segment layout cannot be seeked at fragment level. Callers fall back to the whole-segment path.
    #[error("fragment-level seek unsupported for this segment: {0}")]
    DirectUnsupported(String),
}

impl Error {
    pub(crate) fn mp4(context: &'static str) -> impl FnOnce(rustet_mp4::Error) -> Self {
        move |source| Self::Mp4 { context, source }
    }

    /// Apple's licence server refused the key (for example a quality tier this client may not play), as opposed to
    /// a request that failed in transit.
    pub fn is_licence_refused(&self) -> bool {
        matches!(
            self,
            Self::Licence(rustet_widevine::Error::LicenceRefused { .. })
        )
    }

    /// Cancellation or a closed downstream: the consumer went away, the stream itself is not at fault.
    pub(crate) fn is_consumer_gone(&self) -> bool {
        match self {
            Self::Cancelled | Self::Licence(rustet_widevine::Error::Cancelled) => true,
            Self::Io(e) => e.kind() == io::ErrorKind::BrokenPipe,
            Self::Segment { source, .. } => source.is_consumer_gone(),
            _ => false,
        }
    }

    /// Whether another attempt of the same request can succeed. A request that could not even be built cannot.
    pub(crate) fn is_retryable(&self) -> bool {
        !matches!(self, Self::Fetch(e) if e.is_builder())
    }
}

impl From<Error> for PipelineError {
    fn from(e: Error) -> Self {
        match e {
            Error::Cancelled => PipelineError::Cancelled,
            Error::Io(e) => PipelineError::Io(e),
            other => PipelineError::Other(Box::new(other)),
        }
    }
}
