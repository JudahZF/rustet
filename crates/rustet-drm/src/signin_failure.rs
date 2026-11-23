//! Sign-in failure classification and the attempt limiter.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::backend::BackendError;
use crate::session::SessionManager;

/// Starts the message of every sign-in failure about this installation (missing libraries)
/// rather than the account. `drm-autoauth.mjs` looks for it.
pub const SETUP_ERROR_PREFIX: &str = "FairPlay setup: ";

/// Why a sign-in did not complete, in terms the UI can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    InvalidCredentials,
    Locked,
    Terms,
    NotStoreAccount,
    Timeout,
    Setup,
    Unknown,
}

/// The classified outcome of a failed sign-in, published with the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SignInFailure {
    pub kind: FailureKind,
    /// Fit to show the user as is.
    pub message: String,
    /// Where to fix it, when there is such a place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<&'static str>,
}

impl SignInFailure {
    fn known(kind: FailureKind, message: &str, url: Option<&'static str>) -> Self {
        Self {
            kind,
            message: message.into(),
            url,
        }
    }
}

/// Turns what the backend reported (Apple's own wording where it gave some) into a
/// [`SignInFailure`]. Matching is on Apple's text because its numeric error codes are not
/// readable from the library.
pub fn classify_sign_in_error(err: &BackendError) -> SignInFailure {
    use FailureKind::*;
    match err {
        BackendError::Setup(why) => {
            return SignInFailure {
                kind: Setup,
                message: why.clone(),
                url: None,
            };
        }
        BackendError::TimedOut => return SignInFailure::known(Timeout, TIMEOUT_TEXT, None),
        _ => {}
    }
    let raw = err.to_string();
    let t = raw.to_lowercase();
    let has = |subs: &[&str]| subs.iter().any(|s| t.contains(s));
    if has(&[
        "forgot your password",
        "more than once",
        "too many",
        "locked",
        "disabled",
        "temporarily",
    ]) {
        SignInFailure::known(
            Locked,
            "Apple has blocked sign-ins for now after several wrong attempts. Wait a while, or reset your password, then try again.",
            Some("https://iforgot.apple.com"),
        )
    } else if has(&["terms", "agree"]) {
        SignInFailure::known(
            Terms,
            "Apple needs you to accept its terms before this account can sign in. Accept them at account.apple.com, then try again.",
            Some("https://account.apple.com"),
        )
    } else if has(&[
        "not yet been used",
        "has not been used",
        "never been used",
        "itunes store",
        "not been set up",
    ]) {
        SignInFailure::known(
            NotStoreAccount,
            "This Apple Account has not been used with Apple Music yet. Open music.apple.com, sign in once to set it up, then try again.",
            Some("https://music.apple.com"),
        )
    } else if has(&[
        "check the account information",
        "incorrect",
        "invalid",
        "wrong password",
        "verification failed",
    ]) {
        SignInFailure::known(
            InvalidCredentials,
            "Apple did not accept that email and password. Check them and try again.",
            Some("https://iforgot.apple.com"),
        )
    } else if has(&["timed out", "deadline exceeded", "did not answer"]) {
        SignInFailure::known(Timeout, TIMEOUT_TEXT, None)
    } else {
        SignInFailure {
            kind: Unknown,
            message: raw,
            url: None,
        }
    }
}

const TIMEOUT_TEXT: &str = "Apple did not answer in time. Check your connection and try again.";

// ── Attempt limiter ─────────────────────────────────────────────────────────────
//
// Apple counts wrong passwords per account and answers "Forgot Your Password?" after two. The
// engine therefore stops sending sign-ins for a while after repeated rejections, also across
// restarts (restarting the app and trying again is how an account gets locked). The state lives
// in a file next to the session, which a sign-out leaves alone.

const PAUSE_FILE_NAME: &str = "signin-pause";
const MAX_WRONG_ATTEMPTS: u32 = 2;
const PAUSE_AFTER_WRONG: Duration = Duration::minutes(15);
const PAUSE_AFTER_LOCKED: Duration = Duration::minutes(30);
const WRONG_ATTEMPTS_EXPIRE: Duration = Duration::hours(24);

/// Go's zero time, which the pause file uses for "never".
const ZERO_TIME: OffsetDateTime = time::macros::datetime!(0001-01-01 0:00 UTC);

/// The `signin-pause` file: `{"failures":N,"last":"<RFC 3339>","until":"<RFC 3339>"}`.
#[derive(Serialize, Deserialize)]
struct PauseState {
    #[serde(default)]
    failures: u32,
    #[serde(default = "zero_time", with = "time::serde::rfc3339")]
    last: OffsetDateTime,
    #[serde(default = "zero_time", with = "time::serde::rfc3339")]
    until: OffsetDateTime,
}

fn zero_time() -> OffsetDateTime {
    ZERO_TIME
}

impl Default for PauseState {
    fn default() -> Self {
        Self {
            failures: 0,
            last: ZERO_TIME,
            until: ZERO_TIME,
        }
    }
}

/// Returned by [`crate::DrmManager::authenticate`] while sign-ins are paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignInPausedError {
    pub until: OffsetDateTime,
}

impl std::fmt::Display for SignInPausedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Rounded to the minute, half away from zero, and at least one.
        let left = (self.until - OffsetDateTime::now_utc()).whole_seconds();
        let minutes = ((left + 30).div_euclid(60)).max(1);
        write!(
            f,
            "Sign-in is paused for {minutes} more minutes to protect your Apple Account from being locked. Reset your password at iforgot.apple.com if you are unsure of it."
        )
    }
}

impl std::error::Error for SignInPausedError {}

impl SessionManager {
    fn pause_path(&self) -> PathBuf {
        self.base_dir().join(PAUSE_FILE_NAME)
    }

    fn read_pause(&self) -> PauseState {
        fs::read(self.pause_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// When sign-ins resume; `None` when they are not paused.
    pub fn sign_in_paused_until(&self) -> Option<OffsetDateTime> {
        let until = self.read_pause().until;
        (OffsetDateTime::now_utc() < until).then_some(until)
    }

    /// Records how a sign-in ended. Only rejections count: a cancelled, timed-out or
    /// broken-install sign-in says nothing about the password.
    pub fn note_sign_in_outcome(&self, outcome: Result<(), &SignInFailure>) {
        let f = match outcome {
            Ok(()) => {
                let _ = fs::remove_file(self.pause_path());
                return;
            }
            Err(f) => f,
        };
        if !matches!(
            f.kind,
            FailureKind::InvalidCredentials | FailureKind::Locked
        ) {
            return;
        }
        let now = OffsetDateTime::now_utc();
        let mut p = self.read_pause();
        if now - p.last > WRONG_ATTEMPTS_EXPIRE || (p.until != ZERO_TIME && now > p.until) {
            p = PauseState::default();
        }
        p.failures += 1;
        p.last = now;
        if f.kind == FailureKind::Locked {
            p.until = now + PAUSE_AFTER_LOCKED;
        } else if p.failures >= MAX_WRONG_ATTEMPTS {
            p.until = now + PAUSE_AFTER_WRONG;
        }
        if let Ok(b) = serde_json::to_vec(&p) {
            let _ = crate::session::write_private(&self.pause_path(), &b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(text: &str) -> BackendError {
        BackendError::Failed(text.into())
    }

    #[test]
    fn classify() {
        let cases = [
            (
                "Forgot Your Password?: It has been incorrectly entered more than once. Tap Get Password to retrieve or reset it.",
                FailureKind::Locked,
            ),
            (
                "authenticate: Check the account information you entered and try again.",
                FailureKind::InvalidCredentials,
            ),
            (
                "You must agree to the Terms and Conditions",
                FailureKind::Terms,
            ),
            (
                "This Apple Account has not yet been used with the iTunes Store",
                FailureKind::NotStoreAccount,
            ),
            (
                "sign-in timed out: context deadline exceeded",
                FailureKind::Timeout,
            ),
            ("Apple rejected the sign-in", FailureKind::Unknown),
        ];
        for (text, want) in cases {
            assert_eq!(classify_sign_in_error(&failed(text)).kind, want, "{text:?}");
        }
        assert_eq!(
            classify_sign_in_error(&BackendError::TimedOut).kind,
            FailureKind::Timeout
        );

        let setup = classify_sign_in_error(&BackendError::Setup(
            "the FairPlay libraries are not installed".into(),
        ));
        assert_eq!(setup.kind, FailureKind::Setup);
        assert_eq!(setup.message, "the FairPlay libraries are not installed");
        assert_eq!(
            BackendError::Setup("x".into()).to_string(),
            "FairPlay setup: x",
            "the prefix drm-autoauth.mjs looks for"
        );

        let locked = classify_sign_in_error(&failed("Forgot Your Password?"));
        assert!(
            locked.url.is_some() && !locked.message.is_empty(),
            "a locked account must say what to do and where"
        );
        assert_eq!(
            classify_sign_in_error(&failed("Apple rejected the sign-in")).message,
            "Apple rejected the sign-in"
        );
    }

    #[test]
    fn pause_after_repeated_rejections() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionManager::new(dir.path());
        let wrong = SignInFailure::known(FailureKind::InvalidCredentials, "", None);
        s.note_sign_in_outcome(Err(&SignInFailure::known(FailureKind::Unknown, "", None)));
        s.note_sign_in_outcome(Err(&SignInFailure::known(FailureKind::Setup, "", None)));
        s.note_sign_in_outcome(Err(&wrong));
        assert!(
            s.sign_in_paused_until().is_none(),
            "paused after one wrong password"
        );
        s.note_sign_in_outcome(Err(&wrong));
        assert!(
            s.sign_in_paused_until().is_some(),
            "not paused after the second wrong password"
        );
        assert!(
            SessionManager::new(dir.path())
                .sign_in_paused_until()
                .is_some(),
            "the pause must survive a restart"
        );
        s.note_sign_in_outcome(Ok(()));
        assert!(
            s.sign_in_paused_until().is_none(),
            "a successful sign-in must lift the pause"
        );
        s.note_sign_in_outcome(Err(&SignInFailure::known(FailureKind::Locked, "", None)));
        assert!(
            s.sign_in_paused_until().is_some(),
            "a locked account must pause at once"
        );
    }

    // The file is shared with earlier (Go) versions of the engine, which wrote local offsets.
    #[test]
    fn pause_file_format() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionManager::new(dir.path());
        let until = (OffsetDateTime::now_utc() + Duration::minutes(10))
            .to_offset(time::UtcOffset::from_hms(-5, 0, 0).unwrap());
        let go = format!(
            r#"{{"failures":2,"last":"2026-11-19T09:05:00.123456789-05:00","until":"{}"}}"#,
            until
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        );
        fs::write(dir.path().join("signin-pause"), go).unwrap();
        assert_eq!(s.sign_in_paused_until(), Some(until));

        s.note_sign_in_outcome(Ok(()));
        s.note_sign_in_outcome(Err(&SignInFailure::known(
            FailureKind::InvalidCredentials,
            "",
            None,
        )));
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("signin-pause")).unwrap()).unwrap();
        assert_eq!(v["failures"], 1);
        assert_eq!(v["until"], "0001-01-01T00:00:00Z");
        assert!(v["last"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn paused_message_rounds_to_minutes() {
        let e = SignInPausedError {
            until: OffsetDateTime::now_utc() + Duration::seconds(14 * 60 + 40),
        };
        assert!(
            e.to_string()
                .starts_with("Sign-in is paused for 15 more minutes"),
            "{e}"
        );
        let e = SignInPausedError {
            until: OffsetDateTime::now_utc() + Duration::seconds(5),
        };
        assert!(
            e.to_string()
                .starts_with("Sign-in is paused for 1 more minutes"),
            "{e}"
        );
    }
}
