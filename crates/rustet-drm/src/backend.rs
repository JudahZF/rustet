//! The backend interface: the transport that actually talks to FairPlay.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthCoordinator;
use crate::signin_failure::SETUP_ERROR_PREFIX;
use crate::state::DrmEvent;

/// The DRM transport. [`crate::DrmManager`] is its only user and never knows which backend it
/// drives.
///
/// The backend decides whether authentication is needed: [`start`](Self::start) reuses the
/// session on disk when there is one, and signs in with [`BackendConfig::credentials`] when
/// they are set. Input Apple asks for during sign-in (a verification code, a choice) goes
/// through [`AuthCoordinator::challenge`], which blocks the calling thread until the user
/// answers.
///
/// Blocking: the native library's calls block, some for seconds (sign-in, the first key
/// context). Implementations run them on their own threads or in
/// [`tokio::task::spawn_blocking`], never on the calling task. The manager awaits each method
/// to completion (it does not drop a start or stop half-way), so implementations need not be
/// cancel-safe; a start must instead watch its [`CancellationToken`].
#[async_trait]
pub trait DrmBackend: Send + Sync + 'static {
    /// Connects the backend to the manager. Called once, before any other method.
    fn attach(&self, link: BackendLink);

    /// Starts the backend: session reuse when `cfg.credentials` is `None`, a fresh sign-in
    /// otherwise. `cancel` fires at the manager's deadline (startup or sign-in timeout) or at
    /// shutdown; the backend then returns ([`BackendError::TimedOut`] for a sign-in), even if a
    /// native call has to finish in the background.
    async fn start(
        &self,
        cfg: BackendConfig,
        cancel: CancellationToken,
    ) -> Result<(), BackendError>;

    /// Shuts the backend down, for a restart or a sign-out. New calls are refused from the
    /// moment it begins. It does not remove session files: [`crate::SessionManager`] owns them.
    async fn stop(&self) -> Result<(), BackendError>;

    /// Whether the backend is operational.
    fn running(&self) -> bool;

    /// The HLS playlist URL for a catalog item.
    async fn get_m3u8(&self, adam_id: u64) -> Result<String, BackendError>;

    /// The account's storefront and tokens.
    async fn get_account(&self) -> Result<AccountInfo, BackendError>;

    /// The progressive download URL of a music video.
    async fn get_progressive_mv_url(&self, adam_id: u64) -> Result<ProgressiveMvUrl, BackendError>;

    /// Decrypts itun-encrypted samples in place. Needs a prior
    /// [`get_progressive_mv_url`](Self::get_progressive_mv_url) for the same item. Empty samples
    /// stay empty.
    async fn decrypt_itun_samples(
        &self,
        adam_id: u64,
        samples: Vec<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>, BackendError>;

    /// Opens one CBCS decryption session: the returned stream speaks the protocol of
    /// [`crate::serve_cbcs`]. The session ends when `cancel` fires or the client closes it.
    async fn dial_cbcs(&self, cancel: CancellationToken) -> Result<DuplexStream, BackendError>;

    /// Whether [`dial_cbcs`](Self::dial_cbcs) streams are in-process pipes, which lets the
    /// decrypt loop skip idle reconnects.
    fn in_process(&self) -> bool {
        false
    }

    /// Drops every key context, so sessions dialled afterwards get fresh keys. Called when a
    /// key decrypted to garbage: Apple's library hands out an expired cached key as valid.
    /// Sessions that are open end with a decrypt error and their clients dial again.
    async fn refresh_keys(&self) -> Result<(), BackendError> {
        Ok(())
    }

    /// Registers where [`get_account`](Self::get_account) gets the account when there are no
    /// credential files: the web player's token.
    fn set_account_fallback(&self, _fallback: AccountFallback) {}

    /// Retries FairPlay with a fresh media-user-token when the backend runs but FairPlay
    /// failed at start (the token was missing then). The manager runs it in the background.
    async fn retry_fair_play(&self, _music_user_token: String) {}
}

/// What a backend gets from the manager.
pub struct BackendLink {
    /// Where to ask for sign-in input.
    pub auth: Arc<AuthCoordinator>,
    /// Where to report state changes. Send with `try_send`: when the manager falls behind, an
    /// event is dropped rather than block a native thread.
    pub events: mpsc::Sender<DrmEvent>,
}

/// Returns the web player's account, for [`DrmBackend::set_account_fallback`].
pub type AccountFallback = Arc<dyn Fn() -> AccountInfo + Send + Sync>;

/// Returns the web player's current media-user-token (empty when it has none).
pub type MusicTokenSource = Arc<dyn Fn() -> String + Send + Sync>;

/// What the backend needs to start. Transport details belong to each backend, not here.
#[derive(Clone, Default)]
pub struct BackendConfig {
    /// The directory with `mpl_db/` and the derived token files.
    pub base_dir: PathBuf,
    /// The 9-field device string for storeservicescore. Empty: the backend default.
    pub device_info: String,
    /// Single-use Apple ID for a fresh sign-in, set by [`crate::DrmManager::authenticate`] for
    /// one start. Crash restarts and lazy starts reuse the session and leave it `None`.
    pub credentials: Option<Credentials>,
    /// Report hi-res lossless as unavailable. The account carries no subscription tier, so
    /// hi-res is assumed once FairPlay is ready unless the embedder opts out.
    pub disable_hi_res: bool,
    /// Gives the web player's media-user-token during a start. When `MUSIC_TOKEN` is missing,
    /// the backend writes it there as a fallback for the Android library.
    pub music_token: Option<MusicTokenSource>,
}

/// Apple ID credentials for one sign-in attempt. Kept in memory only, never persisted.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The account's identifiers, as the native library reports them.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    #[serde(default)]
    pub storefront_id: String,
    #[serde(default)]
    pub dev_token: String,
    #[serde(default)]
    pub music_token: String,
}

impl fmt::Debug for AccountInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountInfo")
            .field("storefront_id", &self.storefront_id)
            .field("dev_token", &redacted(&self.dev_token))
            .field("music_token", &redacted(&self.music_token))
            .finish()
    }
}

fn redacted(s: &str) -> &'static str {
    if s.is_empty() { "" } else { "<redacted>" }
}

/// A music video's progressive download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressiveMvUrl {
    pub url: String,
    /// The FairPlay download key (base64). `None`: the file is itun-encrypted and must go
    /// through [`DrmBackend::decrypt_itun_samples`].
    pub download_key: Option<String>,
}

/// A backend failure.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("DRM backend is not running")]
    NotRunning,
    /// This installation cannot sign in (missing libraries, the Android runtime did not start).
    /// The app keeps a saved password through it, because retrying later can work.
    #[error("{SETUP_ERROR_PREFIX}{0}")]
    Setup(String),
    /// The sign-in ended because no verification code was given (cancelled, or the wait ran
    /// out). Not a failure of the account.
    #[error("sign-in cancelled")]
    SignInCancelled,
    /// A start or sign-in did not finish before its deadline.
    #[error("sign-in timed out")]
    TimedOut,
    /// Any other failure. For a sign-in, Apple's own wording when it gave some; the sign-in
    /// failure classification reads it.
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The native backend: Apple's Android FairPlay libraries in-process (Linux).
///
/// `drm_dir` holds `files/` (the base directory when [`BackendConfig::base_dir`] is empty) and
/// `rootfs/system/lib64`, Apple's libraries. libhybris is found beside the executable:
/// `libhybris-core.so` and `hybris-linker/q.so`. Missing files do not stop the engine: AAC
/// plays, and a sign-in fails with a [`BackendError::Setup`] that names the missing file.
#[cfg(target_os = "linux")]
pub fn native_backend(drm_dir: &Path) -> Option<Arc<dyn DrmBackend>> {
    Some(Arc::new(crate::native_backend::NativeBackend::installed(
        drm_dir,
    )))
}

/// The native backend, when this platform has one. Without it there is no DRM layer: the
/// engine reports DRM as unavailable and plays AAC only.
#[cfg(not(target_os = "linux"))]
pub fn native_backend(_drm_dir: &Path) -> Option<Arc<dyn DrmBackend>> {
    None
}
