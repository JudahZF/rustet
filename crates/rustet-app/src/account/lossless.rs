//! The lossless (FairPlay) account: the sign-in sheet as a state machine, and the driver that
//! talks to the engine (port of `account-sheet.js`, `verification-code.js`, `sign-out.js`).
//!
//! The engine runs Apple's sign-in. It reports progress as DRM snapshots: a `two_factor`
//! challenge asks for the six-digit code, a `dialog_choice` for one of Apple's options
//! ("Text code", "Call"), `failed` carries a message that says what to do. [`Sheet`] turns
//! snapshots and user input into the [`SheetView`] the UI draws; [`LosslessAccount`] does the
//! engine calls around it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustet_desktop::drm::{self as saved, SystemClock};
use rustet_desktop::store::Store;
use serde_json::json;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::engine::client::{SPECIAL_RESEND, SPECIAL_SMS};
use crate::engine::types::{AuthState, ChallengeKind, DrmStatus, SignInFailure};
use crate::engine::{ClientError, EngineClient, EngineEvent};

/// Digits in a verification code.
pub const CODE_LENGTH: usize = 6;
/// The password shows itself for this long after "show" (`REVEAL_MS`).
pub const PASSWORD_REVEAL: Duration = Duration::from_secs(8);
/// While the sheet waits for Apple, the status is also polled, so an outcome whose event was
/// missed still ends the wait.
const POLL: Duration = Duration::from_secs(4);

/// The digits of `text`, at most [`CODE_LENGTH`] of them (typing, paste, autofill).
pub fn normalize_code(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_digit)
        .take(CODE_LENGTH)
        .collect()
}

/// An error line on the sheet, with the place to fix it when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetError {
    pub message: String,
    /// "Open iforgot.apple.com" and the like.
    pub help_url: Option<String>,
}

impl SheetError {
    fn text(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            help_url: None,
        }
    }

    /// Apple's classified reason, else the snapshot's message, else `fallback`.
    fn from_snapshot(snap: &DrmStatus, fallback: &str) -> Self {
        match &snap.failure {
            Some(SignInFailure { message, url, .. }) if !message.is_empty() => Self {
                message: message.clone(),
                help_url: url.clone(),
            },
            _ if !snap.message.is_empty() => Self::text(&snap.message),
            _ => Self::text(fallback),
        }
    }
}

/// What the code page is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeWait {
    /// "Verifying…"
    Verifying,
    /// "Resending…"
    Resending,
    /// "Sending SMS…"
    SendingSms,
}

/// The sheet, as the UI draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SheetView {
    Closed,
    /// Email and password. `known_account`: the email belongs to the account signed in before;
    /// the title is "Sign-In Required" and only the password is asked for.
    SignIn {
        email: String,
        known_account: bool,
        /// "Signing In…": the form is disabled.
        busy: bool,
        error: Option<SheetError>,
    },
    /// The six-digit code.
    Code {
        busy: Option<CodeWait>,
        error: Option<SheetError>,
        /// The code was wrong: clear the digits and shake. Cleared by the next input.
        wrong_code: bool,
        /// "Can't get to your devices?" is open (the "Text me a verification code" option).
        phone_options: bool,
    },
    /// One of Apple's options, by button index.
    Choice {
        title: String,
        options: Vec<String>,
        busy: bool,
    },
}

/// What a snapshot did to the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetOutcome {
    /// FairPlay is ready: the sheet closed after a sign-in.
    SignedIn,
}

/// The sheet's state machine (`promptAction` and the sheet's own bookkeeping). Challenges are
/// told apart by ID: snapshots repeat a pending challenge until the sign-in moves on.
#[derive(Debug, Clone)]
pub struct Sheet {
    view: SheetView,
    /// The challenge on screen.
    challenge_id: Option<u64>,
    /// The challenge a code or choice was sent for: a new ID after it means "asked again".
    submitted_id: Option<u64>,
    /// The challenge the user cancelled: never reopened by itself.
    dismissed_id: Option<u64>,
}

impl Default for Sheet {
    fn default() -> Self {
        Self {
            view: SheetView::Closed,
            challenge_id: None,
            submitted_id: None,
            dismissed_id: None,
        }
    }
}

impl Sheet {
    pub fn view(&self) -> &SheetView {
        &self.view
    }

    pub fn is_open(&self) -> bool {
        self.view != SheetView::Closed
    }

    fn busy(&self) -> bool {
        match &self.view {
            SheetView::Closed => false,
            SheetView::SignIn { busy, .. } | SheetView::Choice { busy, .. } => *busy,
            SheetView::Code { busy, .. } => busy.is_some(),
        }
    }

    /// The form, with the saved account's email filled in.
    pub fn open(&mut self, saved_email: &str) {
        self.show_sign_in(saved_email.to_owned(), None);
    }

    fn show_sign_in(&mut self, email: String, error: Option<SheetError>) {
        self.view = SheetView::SignIn {
            known_account: !email.is_empty(),
            email,
            busy: false,
            error,
        };
    }

    fn show_code(&mut self) {
        self.view = SheetView::Code {
            busy: None,
            error: None,
            wrong_code: false,
            phone_options: false,
        };
    }

    /// "Sign In" pressed: the request goes out.
    pub fn sign_in_started(&mut self, email: &str) {
        if let SheetView::SignIn {
            busy,
            error,
            email: e,
            ..
        } = &mut self.view
        {
            *busy = true;
            *error = None;
            email.clone_into(e);
        }
    }

    /// The engine refused to start the sign-in.
    pub fn sign_in_refused(&mut self, message: &str) {
        if let SheetView::SignIn { busy, error, .. } = &mut self.view {
            *busy = false;
            let message = if message.is_empty() {
                "Could not sign in. Try again."
            } else {
                message
            };
            *error = Some(SheetError::text(message));
        }
    }

    /// The code goes out (`submitCodeNow`).
    pub fn code_started(&mut self, wait: CodeWait) {
        if let SheetView::Code {
            busy,
            error,
            wrong_code,
            phone_options,
        } = &mut self.view
        {
            *busy = Some(wait);
            *error = None;
            *wrong_code = false;
            *phone_options = false;
            if wait == CodeWait::Verifying {
                self.submitted_id = self.challenge_id;
            }
        }
    }

    /// The engine did not take the code (or the special reply).
    pub fn code_refused(&mut self) {
        if let SheetView::Code { busy, error, .. } = &mut self.view {
            let message = if *busy == Some(CodeWait::Verifying) {
                self.submitted_id = None;
                "Could not send the code. Try again."
            } else {
                "Could not send. Try again."
            };
            *busy = None;
            *error = Some(SheetError::text(message));
        }
    }

    /// Opens or closes the "Can't get to your devices?" options.
    pub fn toggle_phone_options(&mut self) {
        if let SheetView::Code {
            phone_options,
            busy: None,
            ..
        } = &mut self.view
        {
            *phone_options = !*phone_options;
        }
    }

    /// An option of a dialog choice was picked.
    pub fn choice_started(&mut self) {
        if let SheetView::Choice { busy, .. } = &mut self.view {
            *busy = true;
            self.submitted_id = self.challenge_id;
        }
    }

    /// Cancel (or Esc). Returns whether the engine is running a sign-in that must be stopped.
    pub fn cancel(&mut self) -> bool {
        let running = match &self.view {
            SheetView::Code { .. } | SheetView::Choice { .. } => {
                self.dismissed_id = self.challenge_id;
                true
            }
            SheetView::SignIn { busy, .. } => *busy,
            SheetView::Closed => false,
        };
        self.close();
        running
    }

    fn close(&mut self) {
        self.view = SheetView::Closed;
        self.submitted_id = None;
    }

    /// A DRM snapshot (event or poll).
    pub fn on_snapshot(&mut self, snap: &DrmStatus) -> Option<SheetOutcome> {
        let open = self.is_open();
        if snap.signed_in() {
            if open {
                self.close();
                return Some(SheetOutcome::SignedIn);
            }
            return None;
        }
        let challenge = snap.challenge.as_ref().filter(|c| {
            matches!(
                c.kind,
                ChallengeKind::TwoFactor | ChallengeKind::DialogChoice
            )
        });
        if let Some(ch) = challenge {
            // ID 0 is "not numbered": it never matches a remembered ID.
            let id = Some(ch.id).filter(|id| *id != 0);
            let matches = |remembered: Option<u64>| id.is_some() && id == remembered;
            let previous = self.challenge_id;
            self.challenge_id = id;
            if ch.kind == ChallengeKind::TwoFactor {
                let on_code = matches!(self.view, SheetView::Code { .. });
                if !open {
                    if !matches(self.dismissed_id) {
                        self.show_code();
                    }
                } else if !on_code {
                    self.show_code();
                } else if self.submitted_id.is_some() && id != self.submitted_id {
                    self.wrong_code();
                } else if self.submitted_id.is_none() && previous.is_some() && id != previous {
                    // A new challenge after a special reply (resend, text me): fresh entry.
                    self.show_code();
                }
            } else {
                let on_choice = matches!(self.view, SheetView::Choice { .. });
                if (!open && !matches(self.dismissed_id)) || (open && !on_choice) {
                    let title = if ch.title.is_empty() {
                        "Recovery Options"
                    } else {
                        &ch.title
                    };
                    self.view = SheetView::Choice {
                        title: title.to_owned(),
                        options: ch.options(),
                        busy: false,
                    };
                }
            }
            return None;
        }
        if snap.state.authentication != AuthState::Failed || !open {
            return None;
        }
        if matches!(self.view, SheetView::Code { .. }) {
            // Apple or the engine gave up waiting for the code.
            let error = SheetError::from_snapshot(
                snap,
                "The sign-in ended before the code was accepted. Sign in again.",
            );
            self.show_sign_in(String::new(), Some(error));
        } else if !self.busy() {
            // A failure from an earlier attempt: nothing was submitted now.
        } else if let SheetView::SignIn { busy, error, .. } = &mut self.view {
            *busy = false;
            *error = Some(SheetError::from_snapshot(
                snap,
                "Could not sign in. Check your Apple Account and password.",
            ));
        } else {
            let error = SheetError::from_snapshot(snap, "Sign-in failed. Try again.");
            self.show_sign_in(String::new(), Some(error));
        }
        None
    }

    fn wrong_code(&mut self) {
        self.submitted_id = None;
        self.view = SheetView::Code {
            busy: None,
            error: Some(SheetError::text(
                "Incorrect code. Enter the new code sent to your devices.",
            )),
            wrong_code: true,
            phone_options: false,
        };
    }
}

/// How a sign-out ended (`signOutFlow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignOutResult {
    Done,
    /// Signed out, with a note for the user.
    DoneWithNote(&'static str),
    Failed(&'static str),
}

const SIGN_OUT_MARKER: &str = "Couldn't sign out: the engine isn't responding and the sign-out couldn't be recorded. Try again.";
const SIGN_OUT_CREDENTIALS: &str =
    "Signed out of the engine, but the saved sign-in couldn't be removed. Try again.";
const SIGN_OUT_DURABILITY: &str =
    "Signed out. If Rustet is still signed in after a restart, sign out again.";

/// Email and password of a sign-in Apple has not accepted yet: saved only once it has.
struct Pending {
    email: String,
    password: String,
    remember: bool,
}

struct Inner {
    client: EngineClient,
    store: Store,
    /// The engine's DRM session directory, for a sign-out the engine could not do.
    session_dir: PathBuf,
    /// Restarts the engine (a sign-out finishes in a fresh process).
    reload_engine: Box<dyn Fn() + Send + Sync>,
    sheet: Mutex<Sheet>,
    view: watch::Sender<SheetView>,
    status: watch::Sender<Option<DrmStatus>>,
    pending: Mutex<Option<Pending>>,
}

/// The lossless account and its sign-in sheet. Cheap to clone.
#[derive(Clone)]
pub struct LosslessAccount(Arc<Inner>);

impl LosslessAccount {
    pub fn new(
        client: EngineClient,
        store: Store,
        session_dir: PathBuf,
        reload_engine: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            client,
            store,
            session_dir,
            reload_engine: Box::new(reload_engine),
            sheet: Mutex::new(Sheet::default()),
            view: watch::channel(SheetView::Closed).0,
            status: watch::channel(None).0,
            pending: Mutex::new(None),
        }))
    }

    /// The sheet to draw.
    pub fn view(&self) -> watch::Receiver<SheetView> {
        self.0.view.subscribe()
    }

    /// The latest DRM status (Settings → Account: signed in, FairPlay, session, lease).
    pub fn status(&self) -> watch::Receiver<Option<DrmStatus>> {
        self.0.status.subscribe()
    }

    fn with_sheet<R>(&self, f: impl FnOnce(&mut Sheet) -> R) -> R {
        let mut sheet = self.0.sheet.lock().unwrap_or_else(|e| e.into_inner());
        let out = f(&mut sheet);
        let view = sheet.view.clone();
        drop(sheet);
        self.0.view.send_if_modified(|v| {
            let changed = *v != view;
            *v = view;
            changed
        });
        out
    }

    /// Opens the sign-in form, the saved account's email filled in.
    pub async fn open(&self) {
        let email = saved::saved_email(&self.0.store).await;
        self.with_sheet(|s| s.open(&email));
    }

    /// "Sign In". The password is kept (in memory) until Apple accepts it; it is saved only
    /// with `remember`.
    pub async fn sign_in(&self, email: &str, password: &str, remember: bool) {
        let email = email.trim();
        if email.is_empty() || password.is_empty() {
            return;
        }
        self.with_sheet(|s| s.sign_in_started(email));
        match self.0.client.drm_sign_in(email, password).await {
            Ok(status) => {
                *self.0.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pending {
                    email: email.to_owned(),
                    password: password.to_owned(),
                    remember,
                });
                self.on_status(status).await;
                self.poll();
            }
            Err(e) => {
                let message = match &e {
                    ClientError::Unreachable(_) => String::new(),
                    e => e.to_string(),
                };
                self.with_sheet(|s| s.sign_in_refused(&message));
            }
        }
    }

    /// The six digits (other characters are dropped; fewer digits are ignored).
    pub async fn submit_code(&self, code: &str) {
        let code = normalize_code(code);
        if code.len() != CODE_LENGTH {
            return;
        }
        self.reply(&code, CodeWait::Verifying).await;
    }

    /// "Resend code".
    pub async fn resend_code(&self) {
        self.reply(SPECIAL_RESEND, CodeWait::Resending).await;
    }

    /// "Can't get to your devices?" → "Text me a verification code".
    pub async fn text_me_a_code(&self) {
        self.reply(SPECIAL_SMS, CodeWait::SendingSms).await;
    }

    async fn reply(&self, reply: &str, wait: CodeWait) {
        let ready = self.with_sheet(|s| {
            let ready = matches!(s.view, SheetView::Code { busy: None, .. });
            if ready {
                s.code_started(wait);
            }
            ready
        });
        if !ready {
            return;
        }
        if self.0.client.drm_reply(reply).await.is_err() {
            self.with_sheet(Sheet::code_refused);
        }
    }

    pub fn toggle_phone_options(&self) {
        self.with_sheet(Sheet::toggle_phone_options);
    }

    /// Picks option `index` of a dialog choice.
    pub async fn choose(&self, index: usize) {
        let ready = self.with_sheet(|s| {
            let ready = matches!(s.view, SheetView::Choice { busy: false, .. });
            if ready {
                s.choice_started();
            }
            ready
        });
        if ready && let Err(e) = self.0.client.drm_reply(&index.to_string()).await {
            warn!("could not send the choice: {e}");
        }
    }

    /// Cancel. A sign-in the engine runs is stopped, and saved details are removed so the next
    /// launch does not sign in by itself.
    pub async fn cancel(&self) {
        if !self.with_sheet(Sheet::cancel) {
            return;
        }
        self.0
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Err(e) = self.0.client.drm_sign_out(false).await {
            warn!("could not stop the sign-in: {e}");
        }
        saved::clear_credentials(
            &self.0.store,
            &self.0.session_dir,
            &json!({ "reload": false }),
            || {},
        )
        .await;
    }

    /// A DRM snapshot from the event stream or a poll.
    pub async fn on_status(&self, status: DrmStatus) {
        let outcome = self.with_sheet(|s| s.on_snapshot(&status));
        self.0.status.send_replace(Some(status));
        if outcome == Some(SheetOutcome::SignedIn) {
            let pending = self
                .0
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(p) = pending {
                let creds =
                    json!({ "email": p.email, "password": p.password, "remember": p.remember });
                saved::save_credentials(&self.0.store, &creds).await;
            }
        }
    }

    /// Polls the status while the sheet is open.
    fn poll(&self) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(POLL).await;
                if !me
                    .0
                    .sheet
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_open()
                {
                    return;
                }
                if let Ok(status) = me.0.client.drm_status().await {
                    me.on_status(status).await;
                }
            }
        });
    }

    /// Sign Out (`device: false`) or Forget This Device (`true`). When the engine route fails,
    /// a sign-out request is recorded for the engine's next start; then the saved details are
    /// removed and the engine restarts.
    pub async fn sign_out(&self, device: bool) -> SignOutResult {
        let engine_ok = self.0.client.drm_sign_out(device).await.is_ok();
        let scope = if device { "device" } else { "account" };
        let opts = json!({ "reload": true, "backstop": (!engine_ok).then_some(scope) });
        let res = saved::clear_credentials(&self.0.store, &self.0.session_dir, &opts, || {
            (self.0.reload_engine)()
        })
        .await;
        match (res.ok, res.error, res.warning) {
            (true, _, Some("durability")) => SignOutResult::DoneWithNote(SIGN_OUT_DURABILITY),
            (true, ..) => SignOutResult::Done,
            (false, Some("credentials"), _) => SignOutResult::Failed(SIGN_OUT_CREDENTIALS),
            (false, ..) => SignOutResult::Failed(SIGN_OUT_MARKER),
        }
    }

    /// Follows the engine: `drm` events update the sheet; after every engine start, the status
    /// is fetched, and the first time saved details are used to sign in again
    /// (`rustet_desktop::drm::sign_in_on_launch`).
    pub async fn run(self, mut events: broadcast::Receiver<EngineEvent>) {
        let mut launched = false;
        loop {
            match events.recv().await {
                Ok(EngineEvent::Drm(status)) => self.on_status(*status).await,
                Ok(EngineEvent::Snapshot(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Ok(status) = self.0.client.drm_status().await {
                        self.on_status(status).await;
                    }
                    if !launched {
                        launched = true;
                        let me = self.clone();
                        tokio::spawn(async move {
                            saved::sign_in_on_launch(&me.0.store, &me.0.client, &SystemClock).await;
                        });
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}
