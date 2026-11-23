//! Loads Apple's Android FairPlay libraries through libhybris and exposes sign-in, account
//! state, key contexts and sample decryption.
//!
//! This is the Rust port of `drm/` (`libdrm_client.so`). Two parts:
//!
//! - [`stored_account`]: the account files the session leaves in its base directory
//!   (`drm_linux.c`). Works on every platform.
//! - The FairPlay session (Linux only): [`Hybris::load`] loads `libhybris-core.so`, which loads
//!   Apple's Android libraries from `rootfs/system/lib64` (`drm_hybris.c`,
//!   `embedded_loader.c`). [`Hybris::start_fair_play`] runs the wrapper's start-up sequence:
//!   request context, sign-in, playback lease and its recovery state machine, account tokens
//!   (`native/main.c`, `native/drm_lib.c`, `native/main.cpp`). [`FairPlay`] then opens key
//!   contexts and decrypts samples.
//!
//! Every call into the Android libraries blocks, some for seconds, and Apple's libraries keep
//! process-wide state, so the session is a process-wide singleton. Run the calls on their own
//! threads, never on an async runtime worker.
//!
//! Windows (`drm/WINDOWS.md`) loads a MinGW `libhybris-core.dll` and needs the guest calls
//! declared `extern "sysv64"`. That port is not done yet: the session is compiled on Linux
//! only (x86-64, plus AArch64 with AArch64 Android libraries).

mod account;

pub use account::{StoredAccount, stored_account};

#[cfg(target_os = "linux")]
mod native;

#[cfg(target_os = "linux")]
pub use native::{
    Credentials, DecryptError, Error, FairPlay, FairPlayConfig, Hybris, HybrisPaths, KeyContext,
    KeyContextError, SignInPrompt, VerificationReply, set_env_before_threads,
};
