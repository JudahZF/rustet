//! The provider's errors, classified the way the playback handlers need them.
//!
//! Each [`Error`] variant is one class: the handler picks the HTTP status and decides whether a failure counts
//! against the session-open circuit breaker by matching the variant, never the message. The message keeps the Go
//! engine's wording because it reaches the renderer in the response body.

use std::fmt;

use reqwest::StatusCode;
use rustet_core::pipeline::PipelineError;

/// A provider failure. Reaches callers as [`PipelineError::Other`] (downcast to this type), except
/// [`Error::Cancelled`], which becomes [`PipelineError::Cancelled`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The caller's cancellation token fired: the client went away. Not a failure of Apple's servers.
    #[error("cancelled")]
    Cancelled,
    /// The item does not exist in this storefront: catalog 404 or empty catalog response, or a master playlist with
    /// no audio rendition or no video variant within the request. A per-track problem (the Go handler's 404).
    #[error(transparent)]
    NotFound(Failure),
    /// The item exists but offers nothing this provider can play here: no HLS URL, no webplayback playlist or
    /// asset of the needed flavour, no variant of the needed codec. A per-track problem.
    #[error(transparent)]
    Unavailable(Failure),
    /// Apple's licence server refused the key, for example for a video tier this client may not play.
    #[error(transparent)]
    LicenceRefused(Failure),
    /// Apple rejected the credentials (HTTP 401 / 403), or no developer token could be obtained.
    #[error(transparent)]
    Authentication(Failure),
    /// Apple's servers or the network path to them failed: connect errors, timeouts, HTTP 5xx. Only this class may
    /// trip the session-open circuit breaker.
    #[error(transparent)]
    Transport(Failure),
    /// Anything else: malformed responses, a lossless request without a CBCS dialer.
    #[error(transparent)]
    Other(Failure),
}

/// What failed: a message in the Go engine's wording, and the typed error underneath when there is one.
#[derive(Debug)]
pub struct Failure {
    context: String,
    cause: Option<Cause>,
}

/// The error underneath a [`Failure`].
#[derive(Debug, thiserror::Error)]
pub enum Cause {
    #[error(transparent)]
    Catalog(rustet_ampapi::Error),
    #[error(transparent)]
    Playlist(rustet_hls::HlsError),
    #[error(transparent)]
    Http(reqwest::Error),
    /// An Apple endpoint answered with a status other than 200.
    #[error("HTTP {}", .0.as_u16())]
    Status(StatusCode),
    #[error(transparent)]
    Json(serde_json::Error),
    #[error(transparent)]
    Licence(rustet_streams::Error),
}

impl Failure {
    pub(crate) fn new(context: impl Into<String>, cause: Option<Cause>) -> Self {
        Self {
            context: context.into(),
            cause,
        }
    }

    /// The typed error underneath, if any. Its text is already part of this failure's message.
    pub fn cause(&self) -> Option<&Cause> {
        self.cause.as_ref()
    }
}

// The cause is part of the message (Go's `%w` chain), so it is not also reported as `source`: an error chain
// printer would repeat it.
impl std::error::Error for Failure {}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.context)?;
        match &self.cause {
            Some(cause) => write!(f, ": {cause}"),
            None => Ok(()),
        }
    }
}

/// The class of a failure, before it is bound to a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Cancelled,
    NotFound,
    LicenceRefused,
    Authentication,
    Transport,
    Other,
}

impl Class {
    fn of_status(status: StatusCode) -> Self {
        match status.as_u16() {
            404 => Self::NotFound,
            401 | 403 => Self::Authentication,
            500..=599 => Self::Transport,
            _ => Self::Other,
        }
    }

    /// A request that could not be built or whose body did not decode says nothing about reachability; every other
    /// reqwest failure (connect, timeout, reset, TLS) does.
    fn of_reqwest(e: &reqwest::Error) -> Self {
        if let Some(status) = e.status() {
            Self::of_status(status)
        } else if e.is_builder() || e.is_decode() {
            Self::Other
        } else {
            Self::Transport
        }
    }

    fn of_cause(cause: &Cause) -> Self {
        use rustet_ampapi::Error as Amp;
        use rustet_hls::HlsError as Hls;
        use rustet_streams::Error as Streams;
        use rustet_widevine::Error as Widevine;
        match cause {
            Cause::Catalog(e) => match e {
                Amp::Cancelled => Self::Cancelled,
                Amp::Http(e) => Self::of_reqwest(e),
                Amp::Status(s) | Amp::Feed { status: s, .. } | Amp::LibraryStatus(s) => {
                    Self::of_status(*s)
                }
                Amp::Empty(_) => Self::NotFound,
                Amp::TokenScriptNotFound | Amp::TokenNotFound | Amp::MissingUserToken => {
                    Self::Authentication
                }
                _ => Self::Other,
            },
            Cause::Playlist(e) => match e {
                Hls::Cancelled => Self::Cancelled,
                Hls::Http(e) => Self::of_reqwest(e),
                Hls::Status { status, .. } => Self::of_status(*status),
                Hls::NoAudioAlternative | Hls::NoVideoVariant { .. } => Self::NotFound,
                _ => Self::Other,
            },
            Cause::Http(e) => Self::of_reqwest(e),
            Cause::Status(s) => Self::of_status(*s),
            Cause::Json(_) => Self::Other,
            Cause::Licence(e) => match e {
                _ if e.is_licence_refused() => Self::LicenceRefused,
                Streams::Cancelled | Streams::Licence(Widevine::Cancelled) => Self::Cancelled,
                Streams::Licence(Widevine::Http(e)) | Streams::Fetch(e) | Streams::Body(e) => {
                    Self::of_reqwest(e)
                }
                Streams::HeaderTimeout => Self::Transport,
                Streams::Status(code) => {
                    StatusCode::from_u16(*code).map_or(Self::Other, Self::of_status)
                }
                _ => Self::Other,
            },
        }
    }
}

impl Error {
    /// A failure classified by its cause.
    pub(crate) fn caused(context: impl Into<String>, cause: Cause) -> Self {
        let class = Class::of_cause(&cause);
        Self::of_class(class, Failure::new(context, Some(cause)))
    }

    pub(crate) fn not_found(context: impl Into<String>) -> Self {
        Self::NotFound(Failure::new(context, None))
    }

    pub(crate) fn unavailable(context: impl Into<String>) -> Self {
        Self::Unavailable(Failure::new(context, None))
    }

    fn of_class(class: Class, failure: Failure) -> Self {
        match class {
            Class::Cancelled => Self::Cancelled,
            Class::NotFound => Self::NotFound(failure),
            Class::LicenceRefused => Self::LicenceRefused(failure),
            Class::Authentication => Self::Authentication(failure),
            Class::Transport => Self::Transport(failure),
            Class::Other => Self::Other(failure),
        }
    }

    /// Prefixes the message with `context`, keeping the class (Go's `fmt.Errorf("context: %w", err)`).
    pub(crate) fn context(self, context: &str) -> Self {
        match self {
            Self::Cancelled => Self::Cancelled,
            Self::NotFound(f) => Self::NotFound(f.prefixed(context)),
            Self::Unavailable(f) => Self::Unavailable(f.prefixed(context)),
            Self::LicenceRefused(f) => Self::LicenceRefused(f.prefixed(context)),
            Self::Authentication(f) => Self::Authentication(f.prefixed(context)),
            Self::Transport(f) => Self::Transport(f.prefixed(context)),
            Self::Other(f) => Self::Other(f.prefixed(context)),
        }
    }
}

impl Failure {
    fn prefixed(mut self, context: &str) -> Self {
        self.context = format!("{context}: {}", self.context);
        self
    }
}

impl From<Error> for PipelineError {
    fn from(e: Error) -> Self {
        match e {
            Error::Cancelled => Self::Cancelled,
            other => Self::Other(Box::new(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_the_handler_classes() {
        for (code, want) in [
            (404, Class::NotFound),
            (401, Class::Authentication),
            (403, Class::Authentication),
            (500, Class::Transport),
            (503, Class::Transport),
            (400, Class::Other),
            (429, Class::Other),
        ] {
            let status = StatusCode::from_u16(code).unwrap();
            assert_eq!(Class::of_status(status), want, "HTTP {code}");
        }
    }

    #[test]
    fn context_keeps_the_class_and_reads_like_go() {
        let e = Error::caused("webplayback", Cause::Status(StatusCode::UNAUTHORIZED))
            .context("webplayback API");
        assert!(matches!(e, Error::Authentication(_)));
        assert_eq!(e.to_string(), "webplayback API: webplayback: HTTP 401");
    }

    #[test]
    fn cancellation_is_not_a_pipeline_failure() {
        let e = Error::caused(
            "open media playlist",
            Cause::Playlist(rustet_hls::HlsError::Cancelled),
        );
        assert!(matches!(PipelineError::from(e), PipelineError::Cancelled));
        let e = Error::not_found("song 1 not found in us");
        let PipelineError::Other(boxed) = PipelineError::from(e) else {
            panic!("want Other");
        };
        assert!(matches!(
            boxed.downcast_ref::<Error>(),
            Some(Error::NotFound(_))
        ));
    }
}
