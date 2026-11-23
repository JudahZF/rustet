use std::sync::Arc;

use crate::backend::BackendError;
use crate::session::SessionError;
use crate::signin_failure::SignInPausedError;

/// A [`crate::DrmManager`] failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No session exists, so the backend cannot start on its own; a sign-in is needed.
    #[error("DRM session not authenticated")]
    NotAuthenticated,
    /// A sign-out is running, or a pending one could not be completed yet (`cause` says why):
    /// no session may be started or reused until it finishes.
    #[error("sign-out in progress or incomplete{}", cause.as_ref().map(|c| format!(": {c}")).unwrap_or_default())]
    LogoutPending { cause: Option<Arc<SessionError>> },
    #[error("a sign-in is already running")]
    SignInRunning,
    #[error(transparent)]
    SignInPaused(#[from] SignInPausedError),
    /// The sign-in itself failed. The snapshot carries the classified reason.
    #[error("authenticate: {0}")]
    SignIn(BackendError),
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("another sign-out is still running")]
    SignOutBusy,
    #[error("record sign-out: {0}")]
    RecordSignOut(std::io::Error),
    /// The sign-out could not finish safely in this process. The request is recorded on disk:
    /// answer the request, then end the process; the next one completes the sign-out before any
    /// session exists. See [`crate::DrmManager::restart_pending`].
    #[error("sign-out finishes after the engine restarts: {0}")]
    RestartRequired(&'static str),
    /// The account files could not all be removed. The request stays on disk; later starts
    /// retry it.
    #[error("clear session: {clear}{}", stop.as_ref().map(|s| format!("; logout stop: {s}")).unwrap_or_default())]
    SignOutClear {
        clear: Arc<SessionError>,
        stop: Option<BackendError>,
    },
    /// The account is signed out, but the backend reported an error while stopping.
    #[error("logout stop: {0}")]
    SignOutStop(BackendError),
    /// The engine is shutting down.
    #[error("DRM manager shut down")]
    Shutdown,
}
