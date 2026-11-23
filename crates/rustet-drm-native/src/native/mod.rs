//! The FairPlay session through libhybris (Linux).
//!
//! Module map, with the C each one ports:
//!
//! | Module | C source |
//! |---|---|
//! | [`loader`] | `embedded_loader.c`, `hybris_backend_init` in `drm_hybris.c`, `sym()` / `hybris_init_libs` in `native/hybris_stubs.c` |
//! | [`hooks`] | `hybris_libc_shim_hook` and the curl patcher in `drm_hybris.c` |
//! | [`api`] | the forwarding stubs in `native/hybris_stubs.c`, the declarations in `native/import.h` |
//! | [`cxx`] | the libc++ layouts in `native/import.h` / `hybris_types.h`, `make_shared_manual` |
//! | [`callbacks`] | the `std::function` objects in `native/hybris_stubs.c` |
//! | [`session`] | `native/main.c`, `native/drm_lib.c`, the FairPlay half of `drm_hybris.c` |
//! | [`recovery`] | `native/main.cpp` |
//! | [`guard`] | `native/guard.cpp` |
//! | `tls` (AArch64) | none: keeps Rust's thread-locals out of bionic's TLS slots |
//! | [`wire`] | `native/music_token_request.h`, `native/subscription_status.h` |

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

mod api;
mod callbacks;
mod cxx;
mod guard;
mod hooks;
mod loader;
mod recovery;
mod session;
#[cfg(target_arch = "aarch64")]
mod tls;
mod wire;

pub use loader::{Hybris, HybrisPaths, set_env_before_threads};
pub use session::{FairPlay, KeyContext};

/// What [`Hybris::start_fair_play`] needs.
pub struct FairPlayConfig {
    /// Holds `mpl_db/` and the session files; also the FairPlay directory.
    pub base_dir: PathBuf,
    /// `rootfs/system/lib64` with Apple's Android libraries.
    pub lib64_dir: PathBuf,
    /// The 9-field slash-separated device string. Empty: the wrapper's shared default.
    pub device_info: String,
    /// Set for a fresh sign-in; `None` reuses the session in `mpl_db/`.
    pub credentials: Option<Credentials>,
    /// Answers Apple's questions during a sign-in.
    pub prompt: Arc<dyn SignInPrompt>,
}

/// Apple ID credentials for one sign-in. Forgotten when the sign-in ends.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Where the session asks the user during a sign-in. Both methods block until the user has
/// answered (or the caller gave up); Apple's sign-in waits on them.
pub trait SignInPrompt: Send + Sync {
    /// Apple wants the verification code sent to the user's devices.
    fn verification_code(&self) -> VerificationReply;

    /// Apple raised a dialog with these buttons (for example the "Text code / Get a call"
    /// menu after "Can't get to your devices?"). Returns the chosen button's index; an index
    /// out of range picks the first button.
    fn dialog_choice(&self, title: &str, buttons: &[String]) -> usize;
}

/// The answer to [`SignInPrompt::verification_code`].
#[derive(Clone, PartialEq, Eq)]
pub enum VerificationReply {
    /// The code the user typed. Only its first 6 characters are sent.
    Code(String),
    /// Send the notification to the trusted devices again.
    Resend,
    /// The user cannot reach a trusted device: Apple then offers the phone options as a dialog.
    CantReachDevices,
    /// No code (the user cancelled, or the wait ran out). The sign-in is cancelled rather than
    /// answered: a wrong attempt counts against the account.
    Cancel,
}

impl fmt::Debug for VerificationReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Code(_) => "Code(<redacted>)",
            Self::Resend => "Resend",
            Self::CantReachDevices => "CantReachDevices",
            Self::Cancel => "Cancel",
        })
    }
}

/// Why loading the libraries or starting the session failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `libhybris-core.so` did not load or lacks an entry point.
    #[error("libhybris-core: {0}")]
    Core(String),
    /// An Android library did not load, or `libandroidappmusic.so` has no decrypt entry point.
    #[error("no usable FairPlay backend: {0}")]
    Backend(String),
    /// A function the session calls is missing from the Android libraries.
    #[error("{0} not found in the Android libraries")]
    MissingSymbol(String),
    /// The device string does not have 9 non-empty fields.
    #[error("device info must have 9 fields separated by '/'")]
    DeviceInfo,
    /// The sign-in ended because no verification code was given.
    #[error("sign-in cancelled")]
    SignInCancelled,
    /// The session did not start. Holds why, in words fit for the sign-in form (Apple's own
    /// wording when it gave some), or `None` when nothing is known.
    #[error("{}", .0.as_deref().unwrap_or("Apple's sign-in did not complete"))]
    Failed(Option<String>),
    /// A worker thread could not be started.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Why [`FairPlay::open_key_context`] gave no context.
#[derive(Debug, thiserror::Error)]
pub enum KeyContextError {
    /// The playback lease is being renewed; retry after a short pause.
    #[error("key context refused: lease recovery in progress")]
    RecoveryInProgress,
    /// Apple's library returned no context, or aborted.
    #[error("FairPlay key context failed")]
    Failed,
}

/// Why [`FairPlay::decrypt`] failed.
#[derive(Debug, thiserror::Error)]
pub enum DecryptError {
    /// A lease refresh freed the context; open the key again.
    #[error("FairPlay key context invalidated by a lease refresh")]
    StaleContext,
    /// The context holds no key.
    #[error("FairPlay decrypt failed")]
    Failed,
}
