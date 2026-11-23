//! The Apple Music web sign-in window: a small web view that captures the music user token.
//!
//! [`run`] is the `rustet signin` process role. It opens Apple's own sign-in page on
//! music.apple.com in a system web view (WebKitGTK on Linux, WebView2 on Windows) with its own
//! persistent profile directory, waits until the web player holds a music user token, and
//! prints one [`SignInResult`] as a JSON line on standard output:
//!
//! ```text
//! {"result":"token","musicUserToken":"…","storefront":"us","source":"cookie"}
//! {"result":"cancelled","reason":"windowClosed"}
//! {"result":"error","message":"…"}
//! ```
//!
//! Nothing else goes to stdout. Logs (`tracing`) go wherever the binary sends them (stderr) and
//! never contain the token. The process ends on the first token, when the window is closed,
//! when standard input reaches end of file (the parent closed the pipe or died), and on
//! SIGTERM / SIGINT / SIGHUP. The parent must therefore give it a stdin pipe it keeps open
//! (or a terminal), not `/dev/null`.
//!
//! Frames may only navigate to `music.apple.com` and `idmsa.apple.com` (the Apple ID frame);
//! pop-ups, downloads and permission requests are denied. Apple's page accepts WebKit's own
//! user agent. On Linux the web view renders in software (see `window.rs`).
//!
//! [`sign_out`] deletes the profile, so the next sign-in starts signed out.
#![forbid(unsafe_code)]

mod capture;
mod window;

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

/// File that marks a directory as a sign-in profile, so [`sign_out`] never deletes a directory
/// it did not create.
const PROFILE_MARKER: &str = ".rustet-signin-profile";

/// What the sign-in window needs from its caller.
#[derive(Debug, Clone)]
pub struct Options {
    /// The web view's own profile (cookies, `localStorage`, cache). Created if missing. Used by
    /// nothing else; the same directory on the next run keeps the Apple session.
    pub profile_dir: PathBuf,
    /// Storefront for the start page (`https://music.apple.com/{storefront}/login`). `None` or
    /// an invalid code means `us`; Apple moves the user to their own storefront after sign-in.
    pub storefront: Option<String>,
}

/// The one line [`run`] prints. The parent parses it with `serde_json`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(
    tag = "result",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SignInResult {
    /// The user is signed in.
    Token {
        music_user_token: String,
        /// From Apple's `itua` cookie, lowercased; absent when the cookie is not there.
        storefront: Option<String>,
        source: TokenSource,
    },
    /// The window closed without a token.
    Cancelled { reason: CancelReason },
    /// The window could not start.
    Error { message: String },
}

/// Where the token was found.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TokenSource {
    /// The `media-user-token` cookie of `https://music.apple.com`.
    Cookie,
    /// MusicKit's `localStorage` key `music.<id>.u`, used when the cookie cannot be read.
    LocalStorage,
}

/// Why the window closed without a token.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CancelReason {
    /// The user closed the window.
    WindowClosed,
    /// Standard input reached end of file.
    StdinClosed,
    /// SIGTERM, SIGINT or SIGHUP.
    Signal,
}

/// Redacts the token, so a `{:?}` in a log cannot leak it.
impl fmt::Debug for SignInResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token {
                music_user_token,
                storefront,
                source,
            } => f
                .debug_struct("Token")
                .field("music_user_token_len", &music_user_token.len())
                .field("storefront", storefront)
                .field("source", source)
                .finish(),
            Self::Cancelled { reason } => {
                f.debug_struct("Cancelled").field("reason", reason).finish()
            }
            Self::Error { message } => f.debug_struct("Error").field("message", message).finish(),
        }
    }
}

/// Why the window could not start.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sign-in profile {path}: {source}")]
    Profile { path: PathBuf, source: io::Error },
    #[error("display: {0}")]
    Display(String),
    #[error("window: {0}")]
    Window(#[from] tao::error::OsError),
    #[error("web view: {0}")]
    WebView(#[from] wry::Error),
    #[error("signal handler: {0}")]
    Signals(io::Error),
}

/// Why [`sign_out`] failed.
#[derive(Debug, thiserror::Error)]
pub enum SignOutError {
    /// The directory has files but no sign-in profile marker: it is not ours to delete.
    #[error("{0} is not a sign-in profile")]
    NotAProfile(PathBuf),
    #[error("delete {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

/// The `rustet signin` process role: runs the window on the calling (main) thread, prints one
/// [`SignInResult`] line on stdout, and returns the process exit code (failure only for
/// [`SignInResult::Error`]).
pub fn run(options: &Options) -> ExitCode {
    let result = prepare_profile(&options.profile_dir)
        .and_then(|()| window::sign_in(options))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "sign-in window failed");
            SignInResult::Error {
                message: error.to_string(),
            }
        });
    tracing::info!(?result, "sign-in finished");
    let line = serde_json::to_string(&result).expect("SignInResult serializes");
    // A parent that already closed stdout does not want the line; nothing else to do.
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}").and_then(|()| stdout.flush());
    match result {
        SignInResult::Error { .. } => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

fn prepare_profile(dir: &Path) -> Result<(), Error> {
    let profile_error = |source| Error::Profile {
        path: dir.to_owned(),
        source,
    };
    fs::create_dir_all(dir).map_err(profile_error)?;
    fs::write(dir.join(PROFILE_MARKER), b"").map_err(profile_error)
}

/// Signs out: deletes the sign-in profile (cookies, `localStorage`, cache), so the next
/// [`run`] starts signed out. Must not run while a sign-in window uses the profile. A missing
/// or empty directory is already signed out. A directory with files but without the marker
/// [`run`] writes is refused, so a wrong path cannot delete someone else's data.
pub fn sign_out(profile_dir: &Path) -> Result<(), SignOutError> {
    let io_error = |source| SignOutError::Io {
        path: profile_dir.to_owned(),
        source,
    };
    let mut entries = match fs::read_dir(profile_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    let is_empty = entries.next().is_none();
    if !is_empty && !profile_dir.join(PROFILE_MARKER).is_file() {
        return Err(SignOutError::NotAProfile(profile_dir.to_owned()));
    }
    fs::remove_dir_all(profile_dir).map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_wire_format() {
        let token = SignInResult::Token {
            music_user_token: "tok".into(),
            storefront: Some("us".into()),
            source: TokenSource::Cookie,
        };
        let json = serde_json::to_string(&token).unwrap();
        assert_eq!(
            json,
            r#"{"result":"token","musicUserToken":"tok","storefront":"us","source":"cookie"}"#
        );
        assert_eq!(serde_json::from_str::<SignInResult>(&json).unwrap(), token);
        assert_eq!(
            serde_json::to_string(&SignInResult::Cancelled {
                reason: CancelReason::StdinClosed
            })
            .unwrap(),
            r#"{"result":"cancelled","reason":"stdinClosed"}"#
        );
        assert_eq!(
            serde_json::to_string(&SignInResult::Error {
                message: "x".into()
            })
            .unwrap(),
            r#"{"result":"error","message":"x"}"#
        );
    }

    #[test]
    fn debug_never_shows_the_token() {
        let token = SignInResult::Token {
            music_user_token: "SECRET".into(),
            storefront: None,
            source: TokenSource::LocalStorage,
        };
        assert!(!format!("{token:?}").contains("SECRET"));
    }

    #[test]
    fn sign_out_deletes_only_profiles() {
        let root = std::env::temp_dir().join(format!("rustet-signin-test-{}", std::process::id()));
        let profile = root.join("profile");
        prepare_profile(&profile).unwrap();
        fs::write(profile.join("cookies"), "x").unwrap();
        sign_out(&profile).unwrap();
        assert!(!profile.exists());
        sign_out(&profile).unwrap();

        let other = root.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("data"), "x").unwrap();
        assert!(matches!(
            sign_out(&other),
            Err(SignOutError::NotAProfile(_))
        ));
        assert!(other.join("data").exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
