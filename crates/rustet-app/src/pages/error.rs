//! Why a page did not load, and what the error state says about it.

use rustet_ampapi::{Error, ErrorKind};

use crate::route::Route;

/// Why a loader failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// Signed out, or Apple refused the music user token.
    #[error("sign-in required")]
    SignInRequired,
    /// Apple has no such item in this storefront.
    #[error("not found")]
    NotFound,
    #[error("rate limited")]
    RateLimited,
    /// No answer: offline, DNS, timeout.
    #[error("no connection: {0}")]
    Offline(String),
    /// Apple's answer did not decode.
    #[error("unreadable answer: {0}")]
    Decode(String),
    /// Anything else Apple answered (5xx, a refused developer token, …).
    #[error("{0}")]
    Apple(String),
    /// Downloads and Settings are drawn from local state, not loaded.
    #[error("{0:?} has no loader")]
    LocalRoute(Route),
}

impl From<Error> for LoadError {
    fn from(err: Error) -> Self {
        match err.kind() {
            ErrorKind::Unauthorized => Self::SignInRequired,
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::RateLimited => Self::RateLimited,
            ErrorKind::Transport => Self::Offline(err.to_string()),
            ErrorKind::Decode => Self::Decode(err.to_string()),
            ErrorKind::Cancelled | ErrorKind::Other => Self::Apple(err.to_string()),
        }
    }
}

/// What the user can do about an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recovery {
    Retry,
    SignIn,
    GoBack,
}

/// The error state of a page: what failed and what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorState {
    pub title: String,
    pub message: String,
    pub action: Recovery,
}

impl LoadError {
    /// The error state for `route`, for example "Rustet could not load this album." with
    /// "Check the internet connection, then try again."
    pub fn state(&self, route: &Route) -> ErrorState {
        let noun = route.noun();
        let title = format!("Rustet could not load {noun}.");
        let (message, action) = match self {
            Self::SignInRequired => (
                format!("Sign in to Apple Music to see {noun}."),
                Recovery::SignIn,
            ),
            Self::NotFound => (
                "Apple Music has no such item in your country or region. It may have been removed."
                    .to_owned(),
                Recovery::GoBack,
            ),
            Self::RateLimited => (
                "Apple Music is answering too many requests. Wait a minute, then try again."
                    .to_owned(),
                Recovery::Retry,
            ),
            Self::Offline(_) => (
                "Apple Music did not answer. Check the internet connection, then try again."
                    .to_owned(),
                Recovery::Retry,
            ),
            Self::Decode(_) | Self::Apple(_) => (
                "Apple Music sent an answer Rustet did not expect. Try again later.".to_owned(),
                Recovery::Retry,
            ),
            Self::LocalRoute(_) => (
                "This page has no content to load.".to_owned(),
                Recovery::GoBack,
            ),
        };
        ErrorState {
            title,
            message,
            action,
        }
    }
}
