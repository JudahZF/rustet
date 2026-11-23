//! The Apple Music account (spec §3.3, D3).
//!
//! - [`model::AccountModel`]: the one account a view draws, over the two sign-ins below. On
//!   Linux the lossless sign-in is the Apple Music sign-in; the web window is for an engine
//!   without FairPlay (Windows).
//! - [`WebAccount`]: the web sign-in. `rustet signin` (a child process, `rustet_signin`'s
//!   protocol) captures the music user token; it is kept in the encrypted store, pushed to the
//!   engine after every engine start, and published to whoever builds the `rustet-ampapi` client
//!   ([`Account::session`]). `auth.expired` marks it expired; sign-out removes it everywhere.
//! - [`lossless`]: the FairPlay account for lossless playback on Linux, the sign-in sheet as a
//!   typed state machine.

pub mod lossless;
pub mod model;

pub use model::{AccountModel, AccountState, Backend, SigningIn};

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use rustet_desktop::store::Store;
use rustet_player::model::Event as PlayerEvent;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use crate::engine::types::TokenPush;
use crate::engine::{EngineClient, EngineEvent};

/// The store key of the web music user token.
const TOKEN_KEY: &str = "web_music_user_token";
/// The store key of its storefront.
const STOREFRONT_KEY: &str = "web_storefront";
/// The storefront when nothing better is known (as `rustet_signin`'s start page).
const DEFAULT_STOREFRONT: &str = "us";

/// A music user token. Its `Debug` output never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct MusicUserToken(String);

impl MusicUserToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MusicUserToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MusicUserToken(len {})", self.0.len())
    }
}

/// A signed-in session: what an `rustet-ampapi` client for `/v1/me/*` needs. The web account
/// captures it; on the FairPlay path the engine hands it out ([`AccountModel::session`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSession {
    pub token: MusicUserToken,
    /// Lowercase storefront code (`us`, `gb`).
    pub storefront: String,
}

/// The web sign-in, as Settings → Account and the first-run screen show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebAccount {
    SignedOut,
    /// The sign-in window is open.
    SigningIn,
    SignedIn {
        storefront: String,
    },
    /// Apple refused a token that worked before (`auth.expired`). Browsing goes on; the UI
    /// offers "Sign in again".
    Expired {
        storefront: String,
    },
}

/// Why a web sign-in did not end signed in.
#[derive(Debug, thiserror::Error)]
pub enum WebSignInError {
    /// The user closed the window.
    #[error("sign-in cancelled")]
    Cancelled,
    #[error("a sign-in window is already open")]
    AlreadyOpen,
    /// The window could not start (no web view on this system, no display).
    #[error("the sign-in window could not open: {0}")]
    Window(String),
    #[error("the sign-in window could not start: {0}")]
    Spawn(#[source] io::Error),
    /// The window ended without its result line (it crashed).
    #[error("the sign-in window ended without an answer")]
    NoAnswer,
}

/// The result line of `rustet signin` (`rustet_signin::SignInResult`).
#[derive(Deserialize)]
#[serde(
    tag = "result",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum SignInLine {
    Token {
        music_user_token: String,
        storefront: Option<String>,
    },
    Cancelled {},
    Error {
        message: String,
    },
}

/// How to start the sign-in window: `<program> <args…> <profile dir> [storefront]`, and
/// `<program> <args…> --sign-out <profile dir>` to delete its profile.
#[derive(Debug, Clone)]
pub struct SignInProgram {
    pub program: PathBuf,
    /// Arguments before the role's own (`["signin"]`).
    pub args: Vec<OsString>,
}

impl SignInProgram {
    /// `rustet signin`, from `std::env::current_exe()`.
    pub fn current_exe() -> io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: vec!["signin".into()],
        })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).kill_on_drop(true);
        cmd
    }
}

/// The web view's own profile directory inside the config directory.
pub fn signin_profile_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("web-signin")
}

struct Inner {
    store: Store,
    client: EngineClient,
    program: SignInProgram,
    profile_dir: PathBuf,
    state: watch::Sender<WebAccount>,
    session: watch::Sender<Option<WebSession>>,
    /// The open window's standard input: closing it ends the window (`stdinClosed`).
    window: Mutex<Option<ChildStdin>>,
}

/// The web account. Cheap to clone.
#[derive(Clone)]
pub struct Account(Arc<Inner>);

impl Account {
    /// The account as stored. Run [`Account::run`] to keep the engine in step.
    pub fn load(
        store: Store,
        client: EngineClient,
        program: SignInProgram,
        profile_dir: PathBuf,
    ) -> Self {
        let token = match store.get(TOKEN_KEY) {
            Some(Value::String(t)) if !t.is_empty() => Some(MusicUserToken(t)),
            _ => None,
        };
        let storefront = match store.get(STOREFRONT_KEY) {
            Some(Value::String(s)) if !s.is_empty() => s,
            _ => DEFAULT_STOREFRONT.to_owned(),
        };
        let session = token.map(|token| WebSession { token, storefront });
        let state = match &session {
            Some(s) => WebAccount::SignedIn {
                storefront: s.storefront.clone(),
            },
            None => WebAccount::SignedOut,
        };
        Self(Arc::new(Inner {
            store,
            client,
            program,
            profile_dir,
            state: watch::channel(state).0,
            session: watch::channel(session).0,
            window: Mutex::new(None),
        }))
    }

    pub fn state(&self) -> watch::Receiver<WebAccount> {
        self.0.state.subscribe()
    }

    /// The token and storefront, `None` signed out. The catalog client (re)builds on change.
    pub fn session(&self) -> watch::Receiver<Option<WebSession>> {
        self.0.session.subscribe()
    }

    /// Opens the sign-in window and waits for it. On success the token is stored, pushed to
    /// the engine, and the state is [`WebAccount::SignedIn`].
    pub async fn sign_in(&self) -> Result<WebSession, WebSignInError> {
        let before = self.0.state.borrow().clone();
        if before == WebAccount::SigningIn {
            return Err(WebSignInError::AlreadyOpen);
        }
        let storefront = self
            .0
            .session
            .borrow()
            .as_ref()
            .map(|s| s.storefront.clone());
        let mut cmd = self.0.program.command();
        cmd.arg(&self.0.profile_dir)
            .args(storefront)
            // A pipe kept open: the window ends when it closes (or this process dies).
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(WebSignInError::Spawn)?;
        *self.0.window.lock().unwrap_or_else(|e| e.into_inner()) = child.stdin.take();
        self.0.state.send_replace(WebAccount::SigningIn);
        let line = match child.stdout.take() {
            Some(out) => BufReader::new(out).lines().next_line().await.ok().flatten(),
            None => None,
        };
        self.0
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let _ = child.wait().await;
        let result = match line.as_deref().map(serde_json::from_str::<SignInLine>) {
            Some(Ok(SignInLine::Token {
                music_user_token,
                storefront,
            })) if !music_user_token.is_empty() => {
                let storefront = storefront.filter(|s| !s.is_empty());
                Ok(self
                    .signed_in(MusicUserToken(music_user_token), storefront)
                    .await)
            }
            Some(Ok(SignInLine::Cancelled {})) => Err(WebSignInError::Cancelled),
            Some(Ok(SignInLine::Error { message })) => Err(WebSignInError::Window(message)),
            _ => Err(WebSignInError::NoAnswer),
        };
        if result.is_err() {
            self.0.state.send_replace(before);
        }
        result
    }

    /// Closes an open sign-in window; [`Account::sign_in`] then ends with `Cancelled`.
    pub fn cancel_sign_in(&self) {
        self.0
            .window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    async fn signed_in(&self, token: MusicUserToken, storefront: Option<String>) -> WebSession {
        info!("web sign-in complete");
        let mut session = WebSession {
            token,
            storefront: storefront
                .clone()
                .unwrap_or_else(|| DEFAULT_STOREFRONT.to_owned()),
        };
        self.push(&session).await;
        // No `itua` cookie: the engine may know the account's storefront from FairPlay.
        if storefront.is_none()
            && let Ok(auth) = self.0.client.auth().await
            && !auth.storefront.is_empty()
        {
            session.storefront = auth.storefront.to_lowercase();
        }
        self.0.store.update(|m| {
            m.insert(TOKEN_KEY.into(), session.token.as_str().into());
            m.insert(STOREFRONT_KEY.into(), session.storefront.clone().into());
        });
        self.0.store.flush_now().await;
        self.0.state.send_replace(WebAccount::SignedIn {
            storefront: session.storefront.clone(),
        });
        self.0.session.send_replace(Some(session.clone()));
        session
    }

    /// Hands the stored token to the engine (`POST /api/v1/library/token`). The engine keeps
    /// it in memory only, so this runs after every engine start.
    pub async fn push_to_engine(&self) {
        let session = self.0.session.borrow().clone();
        if let Some(session) = session {
            self.push(&session).await;
        }
    }

    async fn push(&self, session: &WebSession) {
        let push = TokenPush {
            music_user_token: session.token.as_str(),
            developer_token: "",
            storefront: &session.storefront,
            language: "",
        };
        if let Err(e) = self.0.client.push_token(&push).await {
            warn!("could not hand the music user token to the engine: {e}");
        }
    }

    /// Signs out of the web session: the token leaves the store and the engine, the engine's
    /// play history is cleared, and the sign-in window's profile is deleted so the next sign-in
    /// starts fresh. Settings stay. The library cache is the library's to clear on
    /// [`WebAccount::SignedOut`]. The FairPlay account is separate ([`lossless`]).
    pub async fn sign_out(&self) {
        self.cancel_sign_in();
        self.0.store.update(|m| {
            m.remove(TOKEN_KEY);
            m.remove(STOREFRONT_KEY);
        });
        self.0.store.flush_now().await;
        self.0.session.send_replace(None);
        self.0.state.send_replace(WebAccount::SignedOut);
        if let Err(e) = self.0.client.delete_token().await {
            warn!("the engine did not drop the web token: {e}");
        }
        if let Err(e) = self.0.client.clear_history().await {
            warn!("could not clear the play history: {e}");
        }
        let mut cmd = self.0.program.command();
        cmd.arg("--sign-out")
            .arg(&self.0.profile_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        match cmd.status().await {
            Ok(s) if s.success() => {}
            Ok(s) => warn!("the sign-in profile was not deleted ({s})"),
            Err(e) => warn!("the sign-in profile was not deleted: {e}"),
        }
    }

    /// Reacts to engine events until the stream closes: pushes the token after every engine
    /// start (each connection's snapshot) and marks the session expired on `auth.expired`.
    pub async fn run(self, mut events: broadcast::Receiver<EngineEvent>) {
        loop {
            match events.recv().await {
                Ok(EngineEvent::Snapshot(_)) => self.push_to_engine().await,
                Ok(EngineEvent::Player(PlayerEvent::AuthExpired)) => {
                    self.0.state.send_if_modified(|s| match s {
                        WebAccount::SignedIn { storefront } => {
                            warn!("Apple refused the music user token");
                            *s = WebAccount::Expired {
                                storefront: std::mem::take(storefront),
                            };
                            true
                        }
                        _ => false,
                    });
                }
                Ok(_) => {}
                // Missed events: a token push is cheap and makes sure the engine has it.
                Err(broadcast::error::RecvError::Lagged(_)) => self.push_to_engine().await,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}
