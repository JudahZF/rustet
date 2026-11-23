//! The one account the views draw (spec D3).
//!
//! On Linux the Apple ID sign-in of the lossless (FairPlay) path is the Apple Music sign-in:
//! its music token works for every `/v1/me/*` request. The engine hands it out with
//! `GET /api/v1/auth/session`. The web sign-in window ([`super::Account`]) is used only by an
//! engine without the FairPlay layer (`/api/v1/drm/status` says `available: false`, Windows).
//! Which of the two an engine has is asked once, at its first snapshot.
//!
//! Signed in, the token and the storefront's locale go to the `rustet-ampapi` [`Api`] and the
//! library sync starts. Signed out, or when Apple refuses a token that worked
//! (`auth.expired`), the token leaves the `Api`, its response cache and the library cache are
//! emptied, and [`AccountModel::session`] turns `None` (the page cache follows it). An
//! `auth.expired` without a token is the engine finding none: the account stays signed out.

use std::sync::{Arc, Mutex};

use rustet_ampapi::api::UserToken;
use rustet_ampapi::{Api, Locale};
use rustet_player::model::{Command, Event as PlayerEvent, SetQueue};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::lossless::{LosslessAccount, SheetView, SignOutResult};
use super::{Account, MusicUserToken, WebAccount, WebSession, WebSignInError};
use crate::engine::{EngineClient, EngineEvent};
use crate::library::Syncer;

/// The storefront when the engine does not know the account's.
const DEFAULT_STOREFRONT: &str = "us";
const EXPIRED: &str = "Apple Music no longer accepts your sign-in. Sign in again.";

/// Which sign-in the engine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The lossless sign-in (email, password, code), whose token is the Apple Music token.
    FairPlay,
    /// The web sign-in window (an engine without the FairPlay layer).
    Web,
}

/// What a sign-in in progress shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningIn {
    /// The sign-in sheet; its input goes to [`AccountModel::lossless`].
    Sheet(SheetView),
    /// The web sign-in window is open; [`Account::cancel_sign_in`] closes it.
    WebWindow,
}

/// The account, as Settings → Account, the sidebar and the first-run screen draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountState {
    /// The engine has not answered yet.
    Starting,
    SignedOut,
    SigningIn(SigningIn),
    SignedIn {
        storefront: String,
    },
    /// Signed out by a problem; `message` says what to do. Browsing goes on.
    Error {
        message: String,
    },
}

struct Base {
    /// Never `SigningIn`: that comes from the sheet or the window.
    state: AccountState,
    /// The token Apple refused: the engine still has it, but it is not used again.
    refused: Option<MusicUserToken>,
}

struct Inner {
    client: EngineClient,
    api: Api,
    syncer: Syncer,
    /// The UI language (`en-GB`), used when the storefront supports it.
    language: String,
    lossless: LosslessAccount,
    web: Account,
    sheet: watch::Receiver<SheetView>,
    web_state: watch::Receiver<WebAccount>,
    backend: watch::Sender<Option<Backend>>,
    base: Mutex<Base>,
    state: watch::Sender<AccountState>,
    session: watch::Sender<Option<WebSession>>,
    /// One session change at a time.
    apply: tokio::sync::Mutex<()>,
}

/// The account model. Cheap to clone.
#[derive(Clone)]
pub struct AccountModel(Arc<Inner>);

impl AccountModel {
    /// The model over both sign-ins; call [`AccountModel::run`] with the engine's events.
    pub fn new(
        client: EngineClient,
        api: Api,
        syncer: Syncer,
        language: String,
        lossless: LosslessAccount,
        web: Account,
    ) -> Self {
        Self(Arc::new(Inner {
            sheet: lossless.view(),
            web_state: web.state(),
            client,
            api,
            syncer,
            language,
            lossless,
            web,
            backend: watch::channel(None).0,
            base: Mutex::new(Base {
                state: AccountState::Starting,
                refused: None,
            }),
            state: watch::channel(AccountState::Starting).0,
            session: watch::channel(None).0,
            apply: tokio::sync::Mutex::new(()),
        }))
    }

    pub fn state(&self) -> watch::Receiver<AccountState> {
        self.0.state.subscribe()
    }

    /// `None` until the engine's first snapshot.
    pub fn backend(&self) -> watch::Receiver<Option<Backend>> {
        self.0.backend.subscribe()
    }

    /// The token and storefront the `Api` uses; `None` signed out. Every change (sign-in,
    /// sign-out, expiry) invalidates what was loaded with the old one.
    pub fn session(&self) -> watch::Receiver<Option<WebSession>> {
        self.0.session.subscribe()
    }

    /// The lossless account: the sheet's input (email and password, code, choices, cancel)
    /// and the DRM status for Settings → Account.
    pub fn lossless(&self) -> &LosslessAccount {
        &self.0.lossless
    }

    /// The web account (an engine without FairPlay): [`Account::cancel_sign_in`].
    pub fn web(&self) -> &Account {
        &self.0.web
    }

    /// The library sync started at sign-in (Settings → Library: status, "Sync now").
    pub fn syncer(&self) -> &Syncer {
        &self.0.syncer
    }

    /// "Sign in": opens the sign-in sheet, or on an engine without FairPlay the web window
    /// (and waits for it). A window that cannot open becomes [`AccountState::Error`].
    pub async fn sign_in(&self) {
        let backend = *self.0.backend.borrow();
        match backend {
            Some(Backend::FairPlay) => self.0.lossless.open().await,
            Some(Backend::Web) => match self.0.web.sign_in().await {
                // The session watch signs the `Api` in.
                Ok(_) | Err(WebSignInError::Cancelled | WebSignInError::AlreadyOpen) => {}
                Err(e) => {
                    warn!("web sign-in: {e}");
                    let message = format!("{e}. Try again.");
                    self.set_base(AccountState::Error { message }, None);
                }
            },
            None => debug!("sign-in before the engine answered"),
        }
    }

    /// "Sign Out", or "Forget This Device" (`forget_device`, lossless only). The queue, the
    /// play history and the library cache are cleared; settings stay.
    pub async fn sign_out(&self, forget_device: bool) -> SignOutResult {
        let backend = *self.0.backend.borrow();
        if backend.is_none() {
            return SignOutResult::Done;
        }
        // First: the FairPlay sign-out restarts the engine, and the player must not come back
        // with the last account's queue and history.
        let empty = SetQueue {
            items: Vec::new(),
            start_index: 0,
            context: None,
            shuffle: None,
            start_ms: None,
            play: false,
        };
        if let Err(e) = self.0.client.command(&Command::SetQueue(empty)).await {
            warn!("could not clear the queue: {e}");
        }
        if let Err(e) = self.0.client.clear_history().await {
            warn!("could not clear the play history: {e}");
        }
        let result = match backend {
            Some(Backend::FairPlay) => {
                let result = self.0.lossless.sign_out(forget_device).await;
                if matches!(result, SignOutResult::Failed(_)) {
                    return result;
                }
                result
            }
            Some(Backend::Web) => {
                self.0.web.sign_out().await;
                SignOutResult::Done
            }
            None => return SignOutResult::Done,
        };
        let _apply = self.0.apply.lock().await;
        self.clear().await;
        self.set_base(AccountState::SignedOut, None);
        result
    }

    /// Follows the engine until its event stream closes. `lossless_events` and `web_events`
    /// are receivers subscribed at the same time as `events`: the one of the engine's sign-in
    /// drives that account from the first snapshot on.
    pub async fn run(
        self,
        mut events: broadcast::Receiver<EngineEvent>,
        lossless_events: broadcast::Receiver<EngineEvent>,
        web_events: broadcast::Receiver<EngineEvent>,
    ) {
        let mut accounts = Some((lossless_events, web_events));
        let mut sheet = self.0.sheet.clone();
        let mut web_state = self.0.web_state.clone();
        let mut web_session = self.0.web.session();
        let mut drm = self.0.lossless.status();
        let mut drm_signed_in = false;
        let cancel = CancellationToken::new();
        let _stop = cancel.clone().drop_guard();
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(EngineEvent::Snapshot(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.0.backend.borrow().is_none() {
                            self.find_backend(&mut accounts, &cancel).await;
                        }
                        self.refresh().await;
                    }
                    Ok(EngineEvent::Player(PlayerEvent::AuthExpired)) => self.expired().await,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                Ok(()) = sheet.changed() => {
                    let closed = *sheet.borrow_and_update() == SheetView::Closed;
                    self.publish();
                    // A sign-in may have finished with FairPlay ready all along (after an
                    // expiry): its token is read here, not only when the DRM state flips.
                    if closed && self.is(Backend::FairPlay) {
                        self.refresh().await;
                    }
                }
                Ok(()) = web_state.changed() => {
                    let expired = matches!(*web_state.borrow_and_update(), WebAccount::Expired { .. });
                    if expired && self.is(Backend::Web) {
                        self.expired().await;
                    }
                    self.publish();
                }
                Ok(()) = web_session.changed() => {
                    let session = web_session.borrow_and_update().clone();
                    if self.is(Backend::Web) {
                        self.apply_session(session).await;
                    }
                }
                Ok(()) = drm.changed() => {
                    let signed_in = drm.borrow_and_update().as_ref().is_some_and(|s| s.signed_in());
                    // A sign-in finished or the account went away: ask for the token again.
                    if signed_in != drm_signed_in && self.is(Backend::FairPlay) {
                        drm_signed_in = signed_in;
                        self.refresh().await;
                    }
                }
            }
        }
    }

    fn is(&self, backend: Backend) -> bool {
        *self.0.backend.borrow() == Some(backend)
    }

    /// Asks the engine whether it has the FairPlay layer and starts that account's loop.
    async fn find_backend(
        &self,
        accounts: &mut Option<(
            broadcast::Receiver<EngineEvent>,
            broadcast::Receiver<EngineEvent>,
        )>,
        cancel: &CancellationToken,
    ) {
        let status = match self.0.client.drm_status().await {
            Ok(status) => status,
            Err(e) => return debug!("DRM status: {e}"),
        };
        let Some((lossless_events, web_events)) = accounts.take() else {
            return;
        };
        let backend = if status.available {
            Backend::FairPlay
        } else {
            Backend::Web
        };
        info!("account: {backend:?} sign-in");
        let cancel = cancel.clone();
        match backend {
            Backend::FairPlay => {
                let lossless = self.0.lossless.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        () = cancel.cancelled() => {}
                        () = lossless.run(lossless_events) => {}
                    }
                });
            }
            Backend::Web => {
                let web = self.0.web.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        () = cancel.cancelled() => {}
                        () = web.run(web_events) => {}
                    }
                });
            }
        }
        self.0.backend.send_replace(Some(backend));
    }

    /// Reads the session again: the engine's (FairPlay) or the stored one (web).
    async fn refresh(&self) {
        let backend = *self.0.backend.borrow();
        match backend {
            Some(Backend::FairPlay) => match self.0.client.session().await {
                Ok(session) => {
                    let session = session.map(|s| WebSession {
                        token: MusicUserToken(s.music_user_token),
                        storefront: match s.storefront.to_lowercase() {
                            sf if sf.is_empty() => DEFAULT_STOREFRONT.to_owned(),
                            sf => sf,
                        },
                    });
                    self.apply_session(session).await;
                }
                // The engine is (re)starting: its next snapshot asks again.
                Err(e) => debug!("account session: {e}"),
            },
            Some(Backend::Web) => {
                let session = self.0.web.session().borrow().clone();
                self.apply_session(session).await;
            }
            None => {}
        }
    }

    async fn apply_session(&self, new: Option<WebSession>) {
        let _apply = self.0.apply.lock().await;
        let current = self.0.session.borrow().clone();
        let Some(session) = new else {
            if current.is_some() {
                info!("signed out");
                self.clear().await;
            }
            let base = self
                .0
                .base
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .state
                .clone();
            // An expiry message stays until the user acts.
            if !matches!(base, AccountState::Error { .. }) {
                self.set_base(AccountState::SignedOut, None);
            }
            return;
        };
        let (was, refused) = {
            let base = self.0.base.lock().unwrap_or_else(|e| e.into_inner());
            (base.state.clone(), base.refused.clone())
        };
        if refused.as_ref() == Some(&session.token) || current.as_ref() == Some(&session) {
            return;
        }
        info!("signed in (storefront {})", session.storefront);
        let api = &self.0.api;
        api.set_user_token(Some(UserToken::new(session.token.as_str())));
        api.set_locale(self.locale(&session.storefront).await);
        // Signed-in pages (New, Radio) differ from the signed-out ones.
        api.clear_cache();
        self.set_base(
            AccountState::SignedIn {
                storefront: session.storefront.clone(),
            },
            None,
        );
        let switched = current.is_some_and(|c| c.token != session.token);
        self.0.session.send_replace(Some(session));
        // At launch with a known account the cache is current unless it has aged; a sign-in
        // during this run (or another account) fetches the library now.
        if was == AccountState::Starting && !switched {
            self.0.syncer.start_if_due().await;
        } else {
            self.0.syncer.start();
        }
    }

    /// `auth.expired`: Apple refused the token. Without a token nothing changes.
    async fn expired(&self) {
        let _apply = self.0.apply.lock().await;
        let Some(session) = self.0.session.borrow().clone() else {
            debug!("auth.expired without a token: signed out");
            return;
        };
        warn!("Apple refused the music user token");
        self.clear().await;
        self.set_base(
            AccountState::Error {
                message: EXPIRED.to_owned(),
            },
            Some(session.token),
        );
    }

    /// Forgets the token everywhere in the UI process: the `Api`, its cache, the library.
    async fn clear(&self) {
        self.0.api.set_user_token(None);
        self.0.api.clear_cache();
        self.0.session.send_replace(None);
        self.0.syncer.stop_and_clear().await;
    }

    /// The storefront's locale in the UI language when it supports it, else its default.
    async fn locale(&self, storefront: &str) -> Locale {
        match self
            .0
            .api
            .storefront(&CancellationToken::new(), storefront)
            .await
        {
            Ok(sf) => Locale::for_storefront(&sf, &self.0.language),
            Err(e) => {
                debug!("storefront {storefront}: {e}");
                Locale::new(storefront, self.0.language.as_str())
            }
        }
    }

    fn set_base(&self, state: AccountState, refused: Option<MusicUserToken>) {
        {
            let mut base = self.0.base.lock().unwrap_or_else(|e| e.into_inner());
            base.state = state;
            base.refused = refused;
        }
        self.publish();
    }

    /// The state: a sign-in in progress over the account's own state.
    fn publish(&self) {
        let base = self
            .0
            .base
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state
            .clone();
        let backend = *self.0.backend.borrow();
        let state = match backend {
            Some(Backend::FairPlay) => match &*self.0.sheet.borrow() {
                SheetView::Closed => base,
                view => AccountState::SigningIn(SigningIn::Sheet(view.clone())),
            },
            Some(Backend::Web) if *self.0.web_state.borrow() == WebAccount::SigningIn => {
                AccountState::SigningIn(SigningIn::WebWindow)
            }
            _ => base,
        };
        self.0.state.send_if_modified(|s| {
            let changed = *s != state;
            *s = state;
            changed
        });
    }
}
