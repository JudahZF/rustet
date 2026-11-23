//! The sign-in challenge model and the coordinator that connects the backend's requests for
//! input with the user's replies.
//!
//! 1. [`crate::DrmManager::authenticate`] stores the credentials here.
//! 2. The backend starts. When Apple needs input, the native auth callback calls
//!    [`AuthCoordinator::challenge`] (from a foreign thread) and blocks.
//! 3. The challenge is published in the snapshot (`authentication: challenging`).
//! 4. The renderer posts the reply; [`AuthCoordinator::submit_reply`] wakes the waiter, which
//!    returns the reply to the backend.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tracing::info;

use crate::backend::Credentials;
use crate::state::{AuthState, DrmState, StateUpdate};

/// How long sign-in waits for the code the user types in.
pub const TWO_FACTOR_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Codes the sign-in sheet sends instead of digits: "send a new code" and "I cannot reach my
/// devices". They reach the backend unchanged.
const SPECIAL_CODES: [&str; 2] = ["__resend__", "__cant_devices__"];

/// A structured authentication prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AuthChallenge {
    /// Numbers each announced challenge, so a client can tell Apple asking again (a wrong code)
    /// from the same challenge repeated in later snapshots. 0: not numbered.
    #[serde(skip_serializing_if = "is_zero")]
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: AuthChallengeType,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Per-type fields, for example `{"buttons": "<btn0>\n<btn1>"}` for a dialog choice.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The kind of prompt, and the reply it expects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthChallengeType {
    /// Apple ID email and password. Reply: `"email\0password"`.
    #[default]
    Credentials,
    /// The 6-digit verification code. Reply: the digits.
    TwoFactor,
    /// Approval on a trusted device. Reply: `"approved"` or `"denied"`.
    DeviceApproval,
    /// A choice from a dialog Apple raises during sign-in ("Text code / Get a call / ...").
    /// Reply: the decimal button index.
    DialogChoice,
}

/// Why a challenge or a reply did not go through.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeError {
    #[error("no authentication challenge pending")]
    NonePending,
    #[error("authentication challenge already answered")]
    AlreadyAnswered,
    #[error("authentication challenge already pending")]
    AlreadyPending,
    /// The sign-in was aborted (a sign-out started).
    #[error("sign-in cancelled")]
    Cancelled,
    #[error("no reply in time")]
    TimedOut,
}

enum Reply {
    Code(String),
    Abort,
}

struct Pending {
    id: u64,
    reply: SyncSender<Reply>,
}

#[derive(Default)]
struct Inner {
    stored: Option<Credentials>,
    /// The challenge waiting for a reply, if any.
    pending: Option<Pending>,
    seq: u64,
    /// Set by `abort`: verification-code challenges fail at once until the next
    /// `set_credentials`.
    aborted: bool,
}

type Emitter = Box<dyn Fn(StateUpdate) + Send + Sync>;

/// Connects the backend's requests for sign-in input with the user's replies. All methods are
/// synchronous: [`challenge`](Self::challenge) blocks the calling (foreign) thread, the others
/// return at once.
pub struct AuthCoordinator {
    inner: Mutex<Inner>,
    emit: Emitter,
}

impl AuthCoordinator {
    /// `emit` publishes challenge updates. It is called without any lock held, so it may call
    /// back into the coordinator.
    pub fn new(emit: impl Fn(StateUpdate) + Send + Sync + 'static) -> Self {
        Self {
            inner: Mutex::default(),
            emit: Box::new(emit),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores credentials for the next sign-in attempt and lifts an earlier abort.
    pub fn set_credentials(&self, creds: Credentials) {
        let mut inner = self.lock();
        inner.stored = Some(creds);
        inner.aborted = false;
    }

    /// Forgets the stored credentials once an attempt is over, so a later request for
    /// credentials cannot silently resubmit an old password.
    pub fn clear_credentials(&self) {
        self.lock().stored = None;
    }

    /// Ends a sign-in in progress: forgets the credentials, ends a pending challenge (the
    /// backend gets no reply and that attempt fails) and refuses further challenges until
    /// [`set_credentials`](Self::set_credentials). A sign-out calls this first, because
    /// stopping the backend waits for a sign-in that is blocked on a challenge.
    pub fn abort(&self) {
        let mut inner = self.lock();
        inner.stored = None;
        inner.aborted = true;
        if let Some(p) = &inner.pending {
            // A full slot holds a reply already: that reply wins.
            let _ = p.reply.try_send(Reply::Abort);
        }
    }

    /// Delivers the user's reply (a code, a button index) to the waiting challenge. A
    /// challenge accepts exactly one reply.
    pub fn submit_reply(&self, reply: String) -> Result<(), ChallengeError> {
        let mut inner = self.lock();
        let p = inner.pending.take().ok_or(ChallengeError::NonePending)?;
        match p.reply.try_send(Reply::Code(reply)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                inner.pending = Some(p);
                Err(ChallengeError::AlreadyAnswered)
            }
            Err(TrySendError::Disconnected(_)) => Err(ChallengeError::NonePending),
        }
    }

    /// Whether the backend is waiting for user input.
    pub fn pending(&self) -> bool {
        self.lock().pending.is_some()
    }

    /// Asks for sign-in input and blocks until the reply, an abort or the timeout.
    ///
    /// A credentials request answers at once with the stored credentials (`"email\0password"`).
    /// Without stored credentials it announces the request, so the renderer shows the sign-in
    /// form, and answers `"\0"`, which fails this attempt.
    ///
    /// Every other request is announced (numbered) in the snapshot and waits for
    /// [`submit_reply`](Self::submit_reply).
    pub fn challenge(
        &self,
        mut req: AuthChallenge,
        timeout: Duration,
    ) -> Result<String, ChallengeError> {
        if req.kind == AuthChallengeType::Credentials {
            let stored = self.lock().stored.clone();
            if let Some(c) = stored.filter(|c| !c.email.is_empty()) {
                return Ok(format!("{}\0{}", c.email, c.password));
            }
            (self.emit)(challenging(req));
            return Ok("\0".into());
        }

        // The reply slot is registered before the challenge is announced, so a reply sent right
        // after the announcement is accepted and a stale or duplicate one is rejected.
        let (tx, rx) = mpsc::sync_channel(1);
        let id = {
            let mut inner = self.lock();
            if inner.aborted {
                return Err(ChallengeError::Cancelled);
            }
            if inner.pending.is_some() {
                return Err(ChallengeError::AlreadyPending);
            }
            inner.seq += 1;
            let id = inner.seq;
            inner.pending = Some(Pending { id, reply: tx });
            id
        };
        req.id = id;
        (self.emit)(challenging(req));

        let outcome = rx.recv_timeout(timeout);
        {
            let mut inner = self.lock();
            if inner.pending.as_ref().is_some_and(|p| p.id == id) {
                inner.pending = None;
            }
        }
        match outcome {
            Ok(Reply::Code(reply)) => {
                // Answered: withdraw the challenge while Apple checks the reply, so a reloaded
                // page does not ask for the same code again.
                (self.emit)(StateUpdate::new(
                    DrmState {
                        authentication: AuthState::LoggingIn,
                        ..DrmState::default()
                    },
                    "",
                ));
                Ok(reply)
            }
            Ok(Reply::Abort) | Err(RecvTimeoutError::Disconnected) => {
                Err(ChallengeError::Cancelled)
            }
            Err(RecvTimeoutError::Timeout) => Err(ChallengeError::TimedOut),
        }
    }

    /// Answers a prompt from the native library's auth callback. `kind` is what the library
    /// passes: `"2fa"` for the verification code, or `"dialog_choice:<title>\n<btn0>\n<btn1>..."`
    /// for a dialog. Returns the reply to write into the library's buffer: the code's digits (or a
    /// special code unchanged), the chosen button index (`"0"` when none was chosen), or `""`
    /// when nothing was entered, which fails that attempt.
    pub fn answer_native_prompt(&self, kind: &str) -> String {
        if let Some(rest) = kind.strip_prefix("dialog_choice:") {
            let (title, buttons) = rest.split_once('\n').unwrap_or((rest, ""));
            let req = AuthChallenge {
                kind: AuthChallengeType::DialogChoice,
                title: title.into(),
                description: "Choose how to verify your identity.".into(),
                metadata: BTreeMap::from([("buttons".into(), buttons.into())]),
                ..AuthChallenge::default()
            };
            return self.challenge(req, TWO_FACTOR_TIMEOUT).unwrap_or_else(|e| {
                info!("drm: dialog choice not answered: {e}");
                "0".into()
            });
        }
        if kind != "2fa" {
            return String::new();
        }
        let req = AuthChallenge {
            kind: AuthChallengeType::TwoFactor,
            title: "Apple ID verification code".into(),
            description: "Enter the 6-digit code sent to your Apple devices.".into(),
            metadata: BTreeMap::from([("issuer".into(), "Apple ID".into())]),
            ..AuthChallenge::default()
        };
        match self.challenge(req, TWO_FACTOR_TIMEOUT) {
            Ok(code) if SPECIAL_CODES.contains(&code.as_str()) => code,
            Ok(code) => verification_digits(&code),
            Err(e) => {
                info!("drm: verification code not entered: {e}");
                String::new()
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn has_stored_credentials(&self) -> bool {
        self.lock().stored.is_some()
    }
}

fn challenging(req: AuthChallenge) -> StateUpdate {
    StateUpdate {
        state: DrmState {
            authentication: AuthState::Challenging,
            ..DrmState::default()
        },
        challenge: Some(req),
        ..StateUpdate::default()
    }
}

/// Keeps only the digits of a typed code, so "123 456", "123-456" and a pasted code with a
/// trailing newline all reach Apple as "123456".
fn verification_digits(code: &str) -> String {
    code.chars().filter(char::is_ascii_digit).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn two_factor() -> AuthChallenge {
        AuthChallenge {
            kind: AuthChallengeType::TwoFactor,
            ..AuthChallenge::default()
        }
    }

    /// A coordinator whose emitter sees the coordinator itself, like the browser answering a
    /// prompt as soon as it sees it.
    fn answering(
        on_emit: impl Fn(&AuthCoordinator, &StateUpdate) + Send + Sync + 'static,
    ) -> Arc<AuthCoordinator> {
        let cell: Arc<OnceLock<std::sync::Weak<AuthCoordinator>>> = Arc::default();
        let c = Arc::new(AuthCoordinator::new({
            let cell = cell.clone();
            move |u| {
                if let Some(a) = cell.get().and_then(std::sync::Weak::upgrade) {
                    on_emit(&a, &u);
                }
            }
        }));
        cell.set(Arc::downgrade(&c)).ok();
        c
    }

    #[test]
    fn rejects_unsolicited_and_duplicate_replies() {
        let auth = answering(|a, u| {
            if u.challenge.is_none() {
                return; // the "answered" update that withdraws the challenge
            }
            a.submit_reply("123456".into())
                .expect("reply right after the prompt");
            assert_eq!(
                a.submit_reply("654321".into()),
                Err(ChallengeError::NonePending),
                "duplicate reply accepted"
            );
        });
        assert!(
            auth.submit_reply("stale".into()).is_err(),
            "unsolicited reply accepted"
        );
        for _ in 0..2 {
            assert_eq!(
                auth.challenge(two_factor(), SECOND).as_deref(),
                Ok("123456")
            );
        }
        assert!(
            auth.submit_reply("stale".into()).is_err(),
            "completed challenge accepted a reply"
        );
    }

    #[test]
    fn expired_challenge_does_not_take_a_late_reply() {
        let auth = AuthCoordinator::new(|_| {});
        assert_eq!(
            auth.challenge(two_factor(), Duration::from_millis(10)),
            Err(ChallengeError::TimedOut)
        );
        assert!(auth.submit_reply("123456".into()).is_err());
        assert!(!auth.pending());
    }

    #[test]
    fn challenges_are_numbered_and_withdrawn_once_answered() {
        let ids = Arc::new(Mutex::new(Vec::new()));
        let last = Arc::new(Mutex::new(None));
        let auth = answering({
            let (ids, last) = (ids.clone(), last.clone());
            move |a, u| {
                *last.lock().unwrap() = Some(u.clone());
                if let Some(c) = &u.challenge {
                    ids.lock().unwrap().push(c.id);
                    a.submit_reply("123456".into()).unwrap();
                }
            }
        });
        for _ in 0..2 {
            auth.challenge(two_factor(), SECOND).unwrap();
        }
        let ids = ids.lock().unwrap();
        assert!(
            ids.len() == 2 && ids[0] != 0 && ids[1] != ids[0],
            "challenge IDs {ids:?}"
        );
        let last = last.lock().unwrap().clone().unwrap();
        assert!(
            last.challenge.is_none() && last.state.authentication == AuthState::LoggingIn,
            "{last:?}"
        );
    }

    #[test]
    fn abort_ends_a_waiting_challenge_and_refuses_new_ones() {
        let auth = Arc::new(AuthCoordinator::new(|_| {}));
        auth.set_credentials(Credentials {
            email: "a@b.c".into(),
            password: "pw".into(),
        });
        let waiter = std::thread::spawn({
            let auth = auth.clone();
            move || auth.challenge(two_factor(), Duration::from_secs(10))
        });
        while !auth.pending() {
            std::thread::sleep(Duration::from_millis(1));
        }
        auth.abort();
        assert_eq!(waiter.join().unwrap(), Err(ChallengeError::Cancelled));
        assert_eq!(
            auth.challenge(two_factor(), SECOND),
            Err(ChallengeError::Cancelled)
        );
        assert!(!auth.has_stored_credentials());
        assert_eq!(
            auth.challenge(AuthChallenge::default(), SECOND).as_deref(),
            Ok("\0")
        );
    }

    #[test]
    fn credentials_answer_at_once() {
        let announced = Arc::new(Mutex::new(0));
        let auth = AuthCoordinator::new({
            let announced = announced.clone();
            move |_| *announced.lock().unwrap() += 1
        });
        auth.set_credentials(Credentials {
            email: "a@b.c".into(),
            password: "pw".into(),
        });
        assert_eq!(
            auth.challenge(AuthChallenge::default(), SECOND).as_deref(),
            Ok("a@b.c\0pw")
        );
        assert_eq!(*announced.lock().unwrap(), 0);
        auth.clear_credentials();
        assert_eq!(
            auth.challenge(AuthChallenge::default(), SECOND).as_deref(),
            Ok("\0")
        );
        assert_eq!(
            *announced.lock().unwrap(),
            1,
            "a credentials request without credentials shows the form"
        );
    }

    #[test]
    fn native_prompts() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(vec!["2", "__resend__", " 12 3-456\n"]));
        let auth = answering({
            let (seen, replies) = (seen.clone(), replies.clone());
            move |a, u| {
                if let Some(c) = &u.challenge {
                    seen.lock().unwrap().push(c.clone());
                    let r = replies.lock().unwrap().remove(0);
                    a.submit_reply(r.into()).unwrap();
                }
            }
        });
        assert_eq!(
            auth.answer_native_prompt("dialog_choice:Recovery Options\nText\nCall"),
            "2"
        );
        assert_eq!(auth.answer_native_prompt("2fa"), "__resend__");
        assert_eq!(auth.answer_native_prompt("2fa"), "123456");
        assert_eq!(auth.answer_native_prompt("something_else"), "");

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].kind, AuthChallengeType::DialogChoice);
        assert_eq!(seen[0].title, "Recovery Options");
        assert_eq!(seen[0].metadata["buttons"], "Text\nCall");
        assert_eq!(seen[1].kind, AuthChallengeType::TwoFactor);
        assert_eq!(seen[1].metadata["issuer"], "Apple ID");
    }

    #[test]
    fn verification_digits_keeps_only_digits() {
        for (input, want) in [
            ("123456", "123456"),
            ("123 456", "123456"),
            ("123-456\n", "123456"),
            (" 12 3456", "123456"),
            ("abc", ""),
            ("", ""),
        ] {
            assert_eq!(verification_digits(input), want, "{input:?}");
        }
    }
}
