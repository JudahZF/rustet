//! DRM manager: FairPlay account state, sign-in, sign-out and the native backend.
//!
//! ```text
//! request handlers / playback → DrmManager → dyn DrmBackend → native backend (libhybris)
//! ```
//!
//! [`DrmManager`] owns the backend lifecycle (lazy start, crash restarts), the sign-in flow
//! (credentials, verification codes, the attempt limiter), sign-out and its on-disk recovery,
//! and the state snapshot the renderer sees. Every state change is published as a full
//! [`DrmSnapshot`] on a broadcast channel ([`DrmManager::subscribe`]).
//!
//! [`DrmBackend`] is the transport. Its only real implementation loads Apple's Android FairPlay
//! libraries in-process (`rustet-drm-native`, Linux only). Elsewhere [`native_backend`] returns
//! `None` and the engine plays AAC only.
#![forbid(unsafe_code)]

mod auth;
mod backend;
mod cbcs_serve;
mod deviceid;
mod error;
mod inflight;
mod logout_files;
mod manager;
mod session;
mod sessionlock;
mod signin_failure;
mod signout;
mod state;

#[cfg(target_os = "linux")]
mod native_backend;
#[cfg(target_os = "linux")]
pub use native_backend::set_native_env_before_threads;

#[cfg(test)]
mod test_support;

pub use auth::{
    AuthChallenge, AuthChallengeType, AuthCoordinator, ChallengeError, TWO_FACTOR_TIMEOUT,
};
pub use backend::{
    AccountFallback, AccountInfo, BackendConfig, BackendError, BackendLink, Credentials,
    DrmBackend, MusicTokenSource, ProgressiveMvUrl, native_backend,
};
pub use cbcs_serve::{CbcsError, MAX_CBCS_SAMPLE, serve_cbcs};
pub use deviceid::device_info;
pub use error::Error;
pub use inflight::{CallGate, CallGuard, Generation};
pub use manager::{DrmManager, RestartPolicy};
pub use session::{
    SessionError, SessionManager, WebSession, WebSessionSource, normalize_storefront_id,
};
pub use sessionlock::{SessionLock, SessionLockError};
pub use signin_failure::{
    FailureKind, SETUP_ERROR_PREFIX, SignInFailure, SignInPausedError, classify_sign_in_error,
};
pub use signout::{ServerSignOut, SignOutResult};
pub use state::{
    AuthState, CapabilityState, DrmEvent, DrmSnapshot, DrmState, FairPlayState, ManagerState,
    ProcessState, RecoveryState, SessionState, StateFileResult, StateUpdate, parse_state_file,
};
