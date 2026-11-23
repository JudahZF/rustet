//! The DRM manager: backend lifecycle, sign-in, and the state snapshot.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::io::DuplexStream;
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::auth::{AuthCoordinator, ChallengeError};
use crate::backend::{
    AccountInfo, BackendConfig, BackendError, BackendLink, Credentials, DrmBackend,
    MusicTokenSource, ProgressiveMvUrl,
};
use crate::error::Error;
use crate::session::{SessionManager, WebSessionSource};
use crate::signin_failure::{SignInPausedError, classify_sign_in_error};
use crate::signout::{GateInner, SignOutTiming, wait_op};
use crate::state::{
    AuthState, CapabilityState, DrmEvent, DrmSnapshot, DrmState, FairPlayState, ManagerState,
    ProcessState, SessionState, StateUpdate,
};

/// A backend that stays up this long after a start was healthy: the next crash starts a fresh
/// restart budget (the budget counts consecutive crash loops, not crashes over the process
/// lifetime).
const CRASH_RESET_WINDOW: Duration = Duration::from_secs(2 * 60);

/// Snapshots buffered per subscriber before a slow one starts missing them.
const EVENT_CAPACITY: usize = 64;
/// Backend events buffered before the backend's sends are dropped.
const BACKEND_EVENT_CAPACITY: usize = 16;

/// How the manager responds to backend crashes, and how long starts may take.
#[derive(Clone, Debug)]
pub struct RestartPolicy {
    /// Consecutive crash restarts before the manager gives up (`failed`). 0: unlimited.
    pub max_crash_restarts: u32,
    /// The wait before each crash restart, indexed by the crash count; the last entry is reused.
    pub restart_backoff: Vec<Duration>,
    /// How long a session-reuse start may take.
    pub startup_timeout: Duration,
    /// How long a sign-in may take, including the wait for a verification code.
    pub auth_timeout: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_crash_restarts: 5,
            restart_backoff: [1, 2, 5, 10, 30].map(Duration::from_secs).to_vec(),
            startup_timeout: Duration::from_secs(60),
            // Covers the 5-minute wait for a verification code.
            auth_timeout: Duration::from_secs(7 * 60),
        }
    }
}

/// The current snapshot and its subscribers. The authentication coordinator publishes through
/// it too, which is why it is separate from the manager.
pub(crate) struct StateHub {
    snapshot: Mutex<DrmSnapshot>,
    events: broadcast::Sender<DrmSnapshot>,
    /// Set once by shutdown. From then on the manager state stays `shutting_down`: a crash
    /// handler or sign-in already in flight must not flip it back to ready or failed.
    closed: AtomicBool,
    disable_hi_res: bool,
}

impl StateHub {
    pub(crate) fn lock(&self) -> MutexGuard<'_, DrmSnapshot> {
        self.snapshot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Sets the manager state without publishing.
    pub(crate) fn set_manager_state(&self, s: ManagerState) {
        if self.is_closed() && s != ManagerState::ShuttingDown {
            return;
        }
        self.lock().state.manager = s;
    }

    /// Merges an update into the snapshot and publishes the result. The snapshot is sent while
    /// its lock is held, so subscribers see snapshots in the order the state changed.
    pub(crate) fn merge(&self, u: StateUpdate) {
        let mut s = self.lock();
        let (new, cur) = (u.state, &mut s.state);
        if new.manager != ManagerState::Unknown
            && (!self.is_closed() || new.manager == ManagerState::ShuttingDown)
        {
            cur.manager = new.manager;
        }
        if new.process != ProcessState::Unknown {
            cur.process = new.process;
        }
        if new.authentication != AuthState::Unknown {
            cur.authentication = new.authentication;
        }
        if new.fair_play != FairPlayState::Unknown {
            cur.fair_play = new.fair_play;
        }
        if new.session != SessionState::Unknown {
            cur.session = new.session;
        }
        if new.recovery != crate::state::RecoveryState::Unknown {
            cur.recovery = new.recovery;
        }
        let merged_auth = cur.authentication;
        let fair_play_ready = cur.fair_play == FairPlayState::Ready;
        match u.challenge {
            Some(c) => s.challenge = Some(c),
            // Judged by the merged state: an unrelated update must not erase a challenge that
            // still waits for the user.
            None if merged_auth != AuthState::Challenging => s.challenge = None,
            None => {}
        }
        match u.failure {
            Some(f) => s.failure = Some(f),
            None if new.authentication != AuthState::Unknown
                && new.authentication != AuthState::Failed =>
            {
                s.failure = None
            }
            None => {}
        }
        // Capabilities belong to a ready FairPlay session; a failed or restarting one must not
        // keep advertising them. Derived here, so the event announcing "ready" carries them.
        s.capabilities = if fair_play_ready {
            CapabilityState {
                cbcs: true,
                alac: true,
                atmos: true,
                hi_res: !self.disable_hi_res,
            }
        } else {
            CapabilityState::default()
        };
        s.timestamp = Some(OffsetDateTime::now_utc());
        s.message = u.message;
        let _ = self.events.send(s.clone());
    }

    /// The signed-out state after a completed sign-out. Message, failure and timestamp stay.
    pub(crate) fn reset_signed_out(&self) {
        let mut s = self.lock();
        let manager = if self.is_closed() {
            ManagerState::ShuttingDown
        } else {
            ManagerState::Ready
        };
        s.state = DrmState {
            manager,
            process: ProcessState::Stopped,
            authentication: AuthState::LoggedOut,
            session: SessionState::Empty,
            ..DrmState::default()
        };
        s.capabilities = CapabilityState::default();
        s.challenge = None;
        let _ = self.events.send(s.clone());
    }
}

pub(crate) fn auth_state(authentication: AuthState) -> DrmState {
    DrmState {
        authentication,
        ..DrmState::default()
    }
}

fn signed_in() -> DrmState {
    DrmState {
        session: SessionState::Valid,
        authentication: AuthState::LoggedIn,
        ..DrmState::default()
    }
}

fn start_failed() -> DrmState {
    DrmState {
        manager: ManagerState::Failed,
        process: ProcessState::Failed,
        authentication: AuthState::Failed,
        fair_play: FairPlayState::Failed,
        session: SessionState::Expired,
        ..DrmState::default()
    }
}

/// The engine's DRM coordinator. It owns the backend lifecycle (lazy start, crash restarts),
/// the sign-in flow, sign-out, and the [`DrmSnapshot`] clients see. It is the only user of the
/// backend.
///
/// Cheap to clone; clones share one manager. Must be used inside a tokio runtime.
#[derive(Clone)]
pub struct DrmManager {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) hub: Arc<StateHub>,
    pub(crate) backend: Arc<dyn DrmBackend>,
    pub(crate) session: Arc<SessionManager>,
    pub(crate) auth: Arc<AuthCoordinator>,
    cfg: BackendConfig,
    policy: RestartPolicy,
    pub(crate) timing: SignOutTiming,

    /// One background start at a time when the web session appears.
    warming: AtomicBool,
    signing_in: AtomicBool,
    pub(crate) restart_pending: AtomicBool,

    /// Held by every backend start, and by a sign-out once the sign-in it aborted returned:
    /// concurrent starts would stop and start the backend under each other, and a sign-out
    /// must never stop or wipe under a start.
    pub(crate) auth_sem: Arc<Semaphore>,
    /// Serialises sign-out requests.
    pub(crate) sign_out_sem: Semaphore,
    /// Serialises crash handling; `crash_count` is only touched while it is held.
    restart_sem: Semaphore,
    crash_count: AtomicU32,
    pub(crate) last_start: Mutex<Option<Instant>>,

    /// Whether sessions may be started. Held only for decisions, never across a wait.
    pub(crate) gate: Mutex<GateInner>,
    /// Cancelled by shutdown: ends a crash restart's wait and any start in progress.
    pub(crate) shutdown: CancellationToken,
    music_token: Mutex<Option<MusicTokenSource>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl DrmManager {
    /// Creates the manager and attaches it to `backend`.
    ///
    /// A sign-out left on disk (interrupted, or recorded by the Electron main process) is
    /// completed first, so a stale session is never shown. That wait is bounded; when it does
    /// not finish in time sessions stay blocked and the pass finishes in the background.
    pub async fn new(
        backend: Arc<dyn DrmBackend>,
        session: Arc<SessionManager>,
        cfg: BackendConfig,
        policy: RestartPolicy,
    ) -> Self {
        Self::with_timing(backend, session, cfg, policy, SignOutTiming::default()).await
    }

    pub(crate) async fn with_timing(
        backend: Arc<dyn DrmBackend>,
        session: Arc<SessionManager>,
        cfg: BackendConfig,
        policy: RestartPolicy,
        timing: SignOutTiming,
    ) -> Self {
        let hub = Arc::new(StateHub {
            snapshot: Mutex::default(),
            events: broadcast::channel(EVENT_CAPACITY).0,
            closed: AtomicBool::new(false),
            disable_hi_res: cfg.disable_hi_res,
        });
        let auth = Arc::new(AuthCoordinator::new({
            let hub = hub.clone();
            move |u| hub.merge(u)
        }));
        let (events, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
        backend.attach(BackendLink {
            auth: auth.clone(),
            events,
        });
        hub.set_manager_state(ManagerState::Ready);

        let m = Self {
            inner: Arc::new(Inner {
                hub,
                backend,
                session,
                auth,
                cfg,
                policy,
                timing,
                warming: AtomicBool::new(false),
                signing_in: AtomicBool::new(false),
                restart_pending: AtomicBool::new(false),
                auth_sem: Arc::new(Semaphore::new(1)),
                sign_out_sem: Semaphore::new(1),
                restart_sem: Semaphore::new(1),
                crash_count: AtomicU32::new(0),
                last_start: Mutex::default(),
                gate: Mutex::default(),
                shutdown: CancellationToken::new(),
                music_token: Mutex::default(),
            }),
        };
        let inner = &m.inner;

        let replay = {
            let mut g = lock(&inner.gate);
            inner
                .session
                .has_pending()
                .then(|| m.start_cleanup_locked(&mut g))
        };
        if let Some(op) = replay
            && let Err(e) = wait_op(op, timing.replay_wait).await
        {
            warn!("drm: pending sign-out not complete at start: {e}");
            inner.hub.lock().message = "sign-out incomplete".into();
        }

        // Reflect the session at once, so the status is right before anything starts.
        {
            let mut s = inner.hub.lock();
            if !inner.backend.running() {
                // The backend starts lazily: "stopped" is accurate, "unknown" made a healthy
                // idle engine look broken.
                s.state.process = ProcessState::Stopped;
            }
            let (session, authentication) = if inner.session.has_session() {
                (SessionState::Valid, AuthState::LoggedIn)
            } else {
                (SessionState::Empty, AuthState::LoggedOut)
            };
            s.state.session = session;
            s.state.authentication = authentication;
        }

        tokio::spawn(watch_events(Arc::downgrade(&m.inner), backend_events));
        m
    }

    /// The current snapshot.
    pub fn status(&self) -> DrmSnapshot {
        self.inner.hub.lock().clone()
    }

    /// Every snapshot from now on, one per state change. A subscriber that falls more than 64
    /// snapshots behind misses the oldest ones; [`status`](Self::status) has the latest.
    pub fn subscribe(&self) -> broadcast::Receiver<DrmSnapshot> {
        self.inner.hub.events.subscribe()
    }

    // ── DRM operations ─────────────────────────────────────────────────────────

    /// The HLS URL for an item. Starts the backend first if a session exists.
    pub async fn get_m3u8(&self, adam_id: u64) -> Result<String, Error> {
        self.ensure_running().await?;
        Ok(self.inner.backend.get_m3u8(adam_id).await?)
    }

    /// The account's storefront and tokens. Starts the backend first if a session exists.
    pub async fn get_account(&self) -> Result<AccountInfo, Error> {
        self.ensure_running().await?;
        Ok(self.inner.backend.get_account().await?)
    }

    /// A music video's progressive download. Starts the backend first if a session exists.
    pub async fn get_progressive_mv_url(&self, adam_id: u64) -> Result<ProgressiveMvUrl, Error> {
        self.ensure_running().await?;
        Ok(self.inner.backend.get_progressive_mv_url(adam_id).await?)
    }

    /// Decrypts itun-encrypted music-video samples; needs a prior
    /// [`get_progressive_mv_url`](Self::get_progressive_mv_url) for the same item.
    pub async fn decrypt_itun_samples(
        &self,
        adam_id: u64,
        samples: Vec<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>, Error> {
        self.ensure_running().await?;
        Ok(self
            .inner
            .backend
            .decrypt_itun_samples(adam_id, samples)
            .await?)
    }

    /// Opens one CBCS decryption session for an ALAC or Atmos stream. Starts the backend first
    /// if a session exists.
    pub async fn dial_cbcs(&self, cancel: CancellationToken) -> Result<DuplexStream, Error> {
        self.ensure_running().await?;
        Ok(self.inner.backend.dial_cbcs(cancel).await?)
    }

    /// Has the backend drop its key contexts (see [`DrmBackend::refresh_keys`]).
    pub async fn refresh_keys(&self) -> Result<(), Error> {
        Ok(self.inner.backend.refresh_keys().await?)
    }

    /// Whether CBCS sessions are in-process pipes (see [`DrmBackend::in_process`]).
    pub fn in_process(&self) -> bool {
        self.inner.backend.in_process()
    }

    // ── The web player's session ───────────────────────────────────────────────

    /// Wires in the token the web player gives the engine: the session reads it when there are
    /// no token files, and so does the backend's account lookup.
    pub fn set_web_session(&self, source: WebSessionSource) {
        self.inner.session.set_web_session(source.clone());
        self.inner.backend.set_account_fallback(Arc::new(move || {
            let w = source();
            AccountInfo {
                music_token: w.music_user_token,
                storefront_id: w.storefront,
                ..AccountInfo::default()
            }
        }));
    }

    /// Registers the web player's current media-user-token, which
    /// [`note_web_session`](Self::note_web_session) hands the backend to retry FairPlay.
    pub fn set_music_token_source(&self, source: MusicTokenSource) {
        *lock(&self.inner.music_token) = Some(source);
    }

    /// Called when the web player supplies or changes its token. When the app is signed in
    /// (`session-ready`), it reports the session as valid and starts the backend in the
    /// background, or, when it already runs, retries FairPlay with the fresh token. A web token
    /// alone does nothing: after a sign-out the page keeps sending it, and it must not sign the
    /// engine back in.
    pub fn note_web_session(&self) {
        let m = &self.inner;
        if m.hub.is_closed() || !m.session.has_session() {
            return;
        }
        // While a sign-in runs (or waits for its code) the page's token says nothing about it:
        // an older session marker must not report "logged in" and close the sign-in sheet early.
        let signing_in = matches!(
            m.hub.lock().state.authentication,
            AuthState::Challenging | AuthState::LoggingIn
        );
        if !signing_in {
            m.hub.merge(StateUpdate::new(signed_in(), "web session"));
        }
        if m.backend.running() {
            // FairPlay may have failed at start (an eager start before the page loaded).
            let source = lock(&m.music_token).clone();
            if let Some(token) = source.map(|f| f()).filter(|t| !t.is_empty()) {
                let backend = m.backend.clone();
                tokio::spawn(async move { backend.retry_fair_play(token).await });
            }
            return;
        }
        if m.warming.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.ensure_running().await {
                warn!("drm: starting after web session failed: {e}");
            }
            me.inner.warming.store(false, Ordering::SeqCst);
        });
    }

    // ── Starting the backend ───────────────────────────────────────────────────

    /// Starts the backend for session reuse if it is not running. Never signs in:
    /// [`Error::NotAuthenticated`] when there is no session.
    pub(crate) async fn ensure_running(&self) -> Result<(), Error> {
        let m = &self.inner;
        // The gate first, even for a running backend: once a sign-out has begun no DRM
        // operation may reuse the session.
        let (signing_out, clear) = {
            let g = lock(&m.gate);
            (g.signing_out(), g.is_clear() && !m.session.has_pending())
        };
        if signing_out {
            return Err(Error::LogoutPending { cause: None });
        }
        if clear && m.backend.running() {
            return Ok(());
        }
        self.start_guarded(true, |me| async move {
            let m = &me.inner;
            if m.backend.running() {
                return Ok(());
            }
            if !m.session.has_session() {
                return Err(Error::NotAuthenticated);
            }
            if let Err(e) = me
                .start_backend(m.cfg.clone(), m.policy.startup_timeout)
                .await
            {
                m.hub.merge(StateUpdate::new(
                    start_failed(),
                    format!("session restore failed: {e}"),
                ));
                return Err(e.into());
            }
            me.note_start();
            // The session is on disk: show it at once, without waiting for a backend event.
            m.hub.merge(StateUpdate::new(
                signed_in(),
                "session reuse — mpl_db present",
            ));
            Ok(())
        })
        .await
    }

    /// The only way a backend start runs: it holds `auth_sem` for the whole start, and starts
    /// only once the sign-out gate is open (completing a pending sign-out first when `replay`
    /// is set). A start that passed the gate just before a sign-out closed it still holds
    /// `auth_sem`, so that sign-out waits for it.
    ///
    /// Once it holds `auth_sem`, the start runs in its own task to completion, even when the
    /// caller stops waiting: a backend start is never abandoned half-way.
    pub(crate) async fn start_guarded<F, Fut>(&self, replay: bool, start: F) -> Result<(), Error>
    where
        F: FnOnce(DrmManager) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), Error>> + Send + 'static,
    {
        let permit = Arc::clone(&self.inner.auth_sem)
            .acquire_owned()
            .await
            .map_err(|_| Error::Shutdown)?;
        let me = self.clone();
        let task = tokio::spawn(async move {
            let _permit = permit;
            me.await_clear_gate(replay).await?;
            start(me).await
        });
        match task.await {
            Ok(r) => r,
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(_) => Err(Error::Shutdown),
        }
    }

    /// Runs one backend start with a deadline: its token fires after `limit` or at shutdown.
    async fn start_backend(&self, cfg: BackendConfig, limit: Duration) -> Result<(), BackendError> {
        let cancel = self.inner.shutdown.child_token();
        let timer = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                tokio::time::sleep(limit).await;
                cancel.cancel();
            }
        });
        let result = self.inner.backend.start(cfg, cancel).await;
        timer.abort();
        result
    }

    fn note_start(&self) {
        *lock(&self.inner.last_start) = Some(Instant::now());
    }

    // ── Sign-in ────────────────────────────────────────────────────────────────

    /// Signs in to Apple with `creds`. The backend asks for anything else it needs (a
    /// verification code) through challenges in the snapshot; see
    /// [`submit_challenge`](Self::submit_challenge). Progress and the outcome are published as
    /// snapshots.
    ///
    /// Drive it to completion (the request handler spawns it): the start itself always
    /// completes, but a dropped call lets another sign-in queue behind it.
    pub async fn authenticate(&self, creds: Credentials) -> Result<(), Error> {
        let m = &self.inner;
        // One sign-in at a time: a second request would queue behind the first and then send
        // another attempt to Apple, which counts toward its limit on wrong passwords.
        if m.signing_in.swap(true, Ordering::SeqCst) {
            return Err(Error::SignInRunning);
        }
        let result = match m.session.sign_in_paused_until() {
            Some(until) => Err(SignInPausedError { until }.into()),
            None => {
                self.start_guarded(true, |me| async move { me.sign_in(creds).await })
                    .await
            }
        };
        if matches!(result, Err(Error::LogoutPending { .. })) {
            m.hub.merge(StateUpdate::new(
                auth_state(AuthState::Failed),
                "Finish signing out first, then sign in again.",
            ));
        }
        m.signing_in.store(false, Ordering::SeqCst);
        result
    }

    async fn sign_in(&self, creds: Credentials) -> Result<(), Error> {
        let m = &self.inner;
        // Kept here in case the backend asks for credentials; forgotten when the attempt ends.
        m.auth.set_credentials(creds.clone());
        let result = self.sign_in_attempt(creds).await;
        m.auth.clear_credentials();
        result
    }

    async fn sign_in_attempt(&self, creds: Credentials) -> Result<(), Error> {
        let m = &self.inner;
        let fresh = !creds.email.is_empty();
        m.hub.set_manager_state(ManagerState::Initializing);
        // Replace the previous attempt's outcome at once: a client that polls must not read an
        // earlier failure as this attempt's result.
        m.hub.merge(StateUpdate::new(
            auth_state(AuthState::LoggingIn),
            "signing in",
        ));

        // A clean slate for the start below.
        if m.backend.running() {
            let _ = m.backend.stop().await;
        }
        // The credentials go to this one start only. Crash restarts reuse the session.
        let cfg = BackendConfig {
            credentials: fresh.then_some(creds),
            ..m.cfg.clone()
        };
        match self.start_backend(cfg, m.policy.auth_timeout).await {
            Ok(()) => {}
            Err(BackendError::SignInCancelled) => {
                // Nobody failed: the code was not given. Back to "signed out", without a failure.
                m.hub.set_manager_state(ManagerState::Ready);
                m.hub.merge(StateUpdate::new(
                    auth_state(AuthState::LoggedOut),
                    "sign-in cancelled",
                ));
                return Err(Error::SignIn(BackendError::SignInCancelled));
            }
            Err(e) => {
                m.hub.set_manager_state(ManagerState::Failed);
                if fresh {
                    let f = classify_sign_in_error(&e);
                    m.session.note_sign_in_outcome(Err(&f));
                    m.hub.merge(StateUpdate {
                        state: auth_state(AuthState::Failed),
                        message: f.message.clone(),
                        failure: Some(f),
                        ..StateUpdate::default()
                    });
                }
                return Err(Error::SignIn(e));
            }
        }
        if fresh {
            m.session.note_sign_in_outcome(Ok(()));
        }
        self.note_start();
        if fresh {
            // The backend verified the credentials; the session files are on disk now.
            m.hub
                .merge(StateUpdate::new(signed_in(), "login successful"));
        } else if m.session.has_session() {
            m.hub.merge(StateUpdate::new(
                signed_in(),
                "session reuse — mpl_db present",
            ));
        }
        m.hub.set_manager_state(ManagerState::Ready);
        Ok(())
    }

    /// How long sign-ins stay paused after repeated rejections; `None` when they are not.
    pub fn sign_in_paused_for(&self) -> Option<Duration> {
        let until = self.inner.session.sign_in_paused_until()?;
        Some(
            (until - OffsetDateTime::now_utc())
                .try_into()
                .unwrap_or_default(),
        )
    }

    /// Whether a sign-in is in progress.
    pub fn sign_in_running(&self) -> bool {
        self.inner.signing_in.load(Ordering::SeqCst)
    }

    /// Delivers the user's reply to the challenge in the snapshot (a verification code, a
    /// button index).
    pub fn submit_challenge(&self, reply: String) -> Result<(), ChallengeError> {
        self.inner.auth.submit_reply(reply)
    }

    // ── Lifecycle ──────────────────────────────────────────────────────────────

    /// Stops the backend on a clean exit. The session stays on disk for the next start. From
    /// here the manager state stays `shutting_down`.
    pub async fn shutdown(&self) {
        let m = &self.inner;
        m.shutdown.cancel(); // ends a crash restart's wait and starts in progress
        m.hub.set_manager_state(ManagerState::ShuttingDown);
        m.hub.closed.store(true, Ordering::SeqCst); // after: nothing but shutting_down from here
        if m.restart_pending.load(Ordering::SeqCst) {
            // A sign-out waits for this process to end, possibly because a start is stuck in
            // native code holding the backend: stopping could hang, and the next process
            // completes the sign-out anyway.
            return;
        }
        if m.backend.running() {
            let _ = m.backend.stop().await;
        }
    }

    /// Restarts the backend after a crash, within the restart budget.
    pub(crate) async fn handle_crash(&self) {
        let m = &self.inner;
        let Ok(_serial) = m.restart_sem.acquire().await else {
            return;
        };
        // A concurrent crash handler, or a sign-in, already started a fresh backend.
        if m.backend.running() {
            return;
        }
        if lock(&m.last_start).is_some_and(|t| t.elapsed() > CRASH_RESET_WINDOW) {
            m.crash_count.store(0, Ordering::SeqCst); // a healthy run, not a crash loop
        }
        let count = m.crash_count.fetch_add(1, Ordering::SeqCst);
        if m.policy.max_crash_restarts > 0 && count >= m.policy.max_crash_restarts {
            m.hub.set_manager_state(ManagerState::Failed);
            let state = DrmState {
                manager: ManagerState::Failed,
                process: ProcessState::Failed,
                ..DrmState::default()
            };
            m.hub.merge(StateUpdate::new(
                state,
                format!("backend crashed {count} times; giving up"),
            ));
            return;
        }
        let backoff = &m.policy.restart_backoff;
        let delay = backoff
            .get(count as usize)
            .or(backoff.last())
            .copied()
            .unwrap_or_default();
        let state = DrmState {
            process: ProcessState::Stopped,
            ..DrmState::default()
        };
        m.hub.merge(StateUpdate::new(
            state,
            format!(
                "backend crashed (attempt {}); restarting in {delay:?}",
                count + 1
            ),
        ));

        tokio::select! {
            () = m.shutdown.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }

        // A crash restart is a start like any other: never during a sign-out, and never to
        // reuse a session a pending sign-out is about to remove.
        let result = self
            .start_guarded(false, |me| async move {
                let m = &me.inner;
                if m.backend.running() {
                    return Ok(());
                }
                if let Err(e) = me
                    .start_backend(m.cfg.clone(), m.policy.startup_timeout)
                    .await
                {
                    m.hub.merge(StateUpdate::new(
                        start_failed(),
                        format!("restart failed: {e}"),
                    ));
                    return Err(e.into());
                }
                me.note_start();
                Ok(())
            })
            .await;
        if matches!(result, Err(Error::LogoutPending { .. })) {
            info!("drm: crash restart skipped during sign-out");
        }
    }
}

/// Applies backend events to the snapshot, and restarts the backend when it crashed. Ends when
/// the manager is gone.
async fn watch_events(manager: Weak<Inner>, mut events: mpsc::Receiver<DrmEvent>) {
    while let Some(ev) = events.recv().await {
        let Some(inner) = manager.upgrade() else {
            return;
        };
        let m = DrmManager { inner };
        // A planned stop (a sign-in's relaunch) is not a crash: restarting would race it.
        if ev.update.state.process == ProcessState::Stopped && !ev.intentional {
            let m = m.clone();
            tokio::spawn(async move { m.handle_crash().await });
        }
        m.inner.hub.merge(ev.update);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::auth::{AuthChallenge, AuthChallengeType};
    use crate::session::WebSession;
    use crate::signin_failure::{FailureKind, SignInFailure};
    use crate::test_support::{Fake, make_session, manager, policy, wait_until, write};

    fn fast_policy(max_crash_restarts: u32) -> RestartPolicy {
        RestartPolicy {
            max_crash_restarts,
            restart_backoff: vec![Duration::from_millis(5)],
            ..policy()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn initial_state_reflects_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = manager(dir.path(), Fake::new(), policy()).await;
        let st = m.status().state;
        assert_eq!(
            (st.session, st.authentication, st.process),
            (
                SessionState::Empty,
                AuthState::LoggedOut,
                ProcessState::Stopped
            )
        );
        assert_eq!(st.manager, ManagerState::Ready);

        make_session(dir.path());
        let (m, _) = manager(dir.path(), Fake::new(), policy()).await;
        let st = m.status().state;
        assert_eq!(
            (st.session, st.authentication),
            (SessionState::Valid, AuthState::LoggedIn)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_lazy_start_reports_the_session_and_backend_events() {
        let dir = tempfile::tempdir().unwrap();
        make_session(dir.path());
        let fake = Fake::new();
        let ready = DrmState {
            process: ProcessState::Running,
            fair_play: FairPlayState::Ready,
            ..DrmState::default()
        };
        fake.set_emit_on_start(vec![DrmEvent {
            update: StateUpdate::new(ready, "ready"),
            intentional: false,
        }]);
        *fake.account.lock().unwrap() = AccountInfo {
            storefront_id: "143441".into(),
            music_token: "tok".into(),
            ..Default::default()
        };
        let (m, fake) = manager(dir.path(), fake, policy()).await;

        assert_eq!(m.get_account().await.unwrap().storefront_id, "143441");
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
        wait_until(|| m.status().capabilities.cbcs).await;
        let s = m.status();
        assert_eq!(
            (s.state.session, s.state.authentication),
            (SessionState::Valid, AuthState::LoggedIn)
        );
        assert!(s.capabilities.alac && s.capabilities.atmos && s.capabilities.hi_res);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_start_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        make_session(dir.path());
        let fake = Fake::new();
        fake.set_start(|_| async { Err(BackendError::Failed("binary not found".into())) });
        let (m, _) = manager(dir.path(), fake, policy()).await;
        assert!(matches!(m.get_account().await, Err(Error::Backend(_))));
        let s = m.status();
        assert_eq!(s.state.manager, ManagerState::Failed);
        assert_eq!(s.message, "session restore failed: binary not found");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nothing_starts_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let (m, fake) = manager(dir.path(), Fake::new(), policy()).await;
        assert!(matches!(
            m.get_account().await,
            Err(Error::NotAuthenticated)
        ));
        assert!(matches!(
            m.get_m3u8(12345).await,
            Err(Error::NotAuthenticated)
        ));
        assert!(matches!(
            m.dial_cbcs(CancellationToken::new()).await,
            Err(Error::NotAuthenticated)
        ));
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    }

    // A fresh sign-in stops a running backend first, and gives the start the credentials.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn authenticate_restarts_the_backend_with_the_credentials() {
        let dir = tempfile::tempdir().unwrap();
        make_session(dir.path());
        let fake = Fake::new();
        fake.running.store(true, Ordering::SeqCst);
        let (m, fake) = manager(dir.path(), fake, policy()).await;
        let creds = Credentials {
            email: "test@example.com".into(),
            password: "hunter2".into(),
        };
        m.authenticate(creds.clone()).await.unwrap();
        assert_eq!(fake.stops.load(Ordering::SeqCst), 1);
        assert_eq!(fake.last_credentials.lock().unwrap().clone(), Some(creds));
        let s = m.status();
        assert_eq!(
            (s.state.authentication, s.state.manager),
            (AuthState::LoggedIn, ManagerState::Ready)
        );
        assert_eq!(s.message, "login successful");
    }

    // A finished sign-in must not leave the password where a later credentials request would
    // silently resubmit it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn authenticate_forgets_the_credentials() {
        for fail in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let fake = Fake::new();
            let seen = Arc::new(Mutex::new(String::new()));
            fake.set_start({
                let seen = seen.clone();
                move |auth| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().unwrap() = auth
                            .challenge(AuthChallenge::default(), Duration::from_secs(1))
                            .unwrap();
                        if fail {
                            Err(BackendError::Failed("Apple rejected the sign-in".into()))
                        } else {
                            Ok(())
                        }
                    }
                }
            });
            let (m, fake) = manager(dir.path(), fake, policy()).await;
            let _ = m
                .authenticate(Credentials {
                    email: "a@b.c".into(),
                    password: "pw".into(),
                })
                .await;
            assert_eq!(
                *seen.lock().unwrap(),
                "a@b.c\0pw",
                "the backend was not offered the credentials"
            );
            let auth = fake.auth();
            assert_eq!(
                auth.challenge(AuthChallenge::default(), Duration::from_secs(1))
                    .unwrap(),
                "\0"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_one_sign_in_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let (started_tx, started) = tokio::sync::oneshot::channel::<()>();
        let release = Arc::new(tokio::sync::Notify::new());
        let started_tx = Mutex::new(Some(started_tx));
        fake.set_start({
            let release = release.clone();
            move |_| {
                let release = release.clone();
                let started = started_tx.lock().unwrap().take();
                async move {
                    started.map(|s| s.send(()));
                    release.notified().await;
                    Ok(())
                }
            }
        });
        let (m, _) = manager(dir.path(), fake, policy()).await;
        let creds = Credentials {
            email: "a@b.c".into(),
            password: "pw".into(),
        };
        let first = tokio::spawn({
            let (m, creds) = (m.clone(), creds.clone());
            async move { m.authenticate(creds).await }
        });
        started.await.unwrap();
        assert!(m.sign_in_running());
        assert!(matches!(
            m.authenticate(creds).await,
            Err(Error::SignInRunning)
        ));
        release.notify_one();
        first.await.unwrap().unwrap();
        assert!(!m.sign_in_running());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn paused_sign_ins_are_refused_and_a_cancel_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (m, fake) = manager(dir.path(), Fake::new(), policy()).await;
        let creds = Credentials {
            email: "a@b.c".into(),
            password: "pw".into(),
        };
        let locked = SignInFailure {
            kind: FailureKind::Locked,
            message: String::new(),
            url: None,
        };
        m.inner.session.note_sign_in_outcome(Err(&locked));
        assert!(matches!(
            m.authenticate(creds.clone()).await,
            Err(Error::SignInPaused(_))
        ));
        assert!(m.sign_in_paused_for().is_some());
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            0,
            "a paused sign-in reached the backend"
        );

        m.inner.session.note_sign_in_outcome(Ok(()));
        fake.set_start(|_| async { Err(BackendError::SignInCancelled) });
        let _ = m.authenticate(creds.clone()).await;
        let s = m.status();
        assert!(
            s.state.authentication == AuthState::LoggedOut && s.failure.is_none(),
            "{s:?}"
        );
        assert!(
            m.inner.session.sign_in_paused_until().is_none(),
            "a cancelled sign-in counted against the account"
        );

        fake.set_start(|_| async {
            Err(BackendError::Failed(
                "Check the account information you entered and try again.".into(),
            ))
        });
        let _ = m.authenticate(creds).await;
        let s = m.status();
        assert_eq!(
            s.failure.as_ref().map(|f| f.kind),
            Some(FailureKind::InvalidCredentials)
        );
        assert_eq!(s.message, s.failure.unwrap().message);
        assert_eq!(s.state.manager, ManagerState::Failed);
    }

    // The web player's token must not report "logged in" while a sign-in runs: an older session
    // marker would close the sign-in sheet before Apple answered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_web_session_does_not_answer_for_a_running_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "session-ready", "1");
        let fake = Fake::new();
        fake.running.store(true, Ordering::SeqCst);
        let (m, _) = manager(dir.path(), fake, policy()).await;
        m.inner
            .hub
            .merge(StateUpdate::new(auth_state(AuthState::LoggingIn), ""));
        m.note_web_session();
        assert_eq!(m.status().state.authentication, AuthState::LoggingIn);
        m.inner
            .hub
            .merge(StateUpdate::new(auth_state(AuthState::Failed), ""));
        m.note_web_session();
        assert_eq!(m.status().state.authentication, AuthState::LoggedIn);
    }

    // A signed-out engine stays signed out while the page keeps sending its token; only a real
    // sign-in (session-ready) lets the token start the backend.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_web_token_starts_the_backend_only_when_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let (m, fake) = manager(dir.path(), Fake::new(), policy()).await;
        m.set_web_session(Arc::new(|| WebSession {
            music_user_token: "web-token".into(),
            storefront: String::new(),
        }));

        m.note_web_session();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let st = m.status().state;
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
        assert_eq!(
            (st.session, st.authentication),
            (SessionState::Empty, AuthState::LoggedOut)
        );

        write(dir.path(), "session-ready", "1");
        m.note_web_session();
        wait_until(|| fake.starts.load(Ordering::SeqCst) == 1).await;
        let st = m.status().state;
        assert_eq!(
            (st.session, st.authentication),
            (SessionState::Valid, AuthState::LoggedIn)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_web_token_retries_fair_play_on_a_running_backend() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "session-ready", "1");
        let fake = Fake::new();
        fake.running.store(true, Ordering::SeqCst);
        let (m, fake) = manager(dir.path(), fake, policy()).await;
        m.note_web_session();
        m.set_music_token_source(Arc::new(|| "mut".into()));
        m.note_web_session();
        wait_until(|| fake.retries.lock().unwrap().len() == 1).await;
        assert_eq!(fake.retries.lock().unwrap()[0], "mut");
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn note_web_session_after_shutdown_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "session-ready", "1");
        let (m, fake) = manager(dir.path(), Fake::new(), policy()).await;
        m.shutdown().await;
        m.note_web_session();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    }

    // Before shutdown the manager moves freely (a sign-out passes through shutting_down on its
    // way back to ready). After shutdown it stays at shutting_down whatever late callbacks say.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_is_absorbing() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = manager(dir.path(), Fake::new(), policy()).await;
        let hub = &m.inner.hub;
        hub.set_manager_state(ManagerState::ShuttingDown);
        hub.set_manager_state(ManagerState::Ready);
        assert_eq!(m.status().state.manager, ManagerState::Ready);

        m.shutdown().await;
        for late in [
            ManagerState::Ready,
            ManagerState::Failed,
            ManagerState::Initializing,
            ManagerState::Disabled,
        ] {
            hub.set_manager_state(late);
            hub.merge(StateUpdate::new(
                DrmState {
                    manager: late,
                    ..DrmState::default()
                },
                "",
            ));
            assert_eq!(
                m.status().state.manager,
                ManagerState::ShuttingDown,
                "a late {late} flipped the state"
            );
        }
        hub.merge(StateUpdate::new(
            DrmState {
                process: ProcessState::Running,
                ..DrmState::default()
            },
            "",
        ));
        assert_eq!(
            m.status().state.process,
            ProcessState::Running,
            "other dimensions still update"
        );
    }

    // ── Merging ──────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_pending_challenge_survives_unrelated_updates() {
        let dir = tempfile::tempdir().unwrap();
        let (m, fake) = manager(dir.path(), Fake::new(), policy()).await;
        let challenge = AuthChallenge {
            kind: AuthChallengeType::TwoFactor,
            title: "2FA".into(),
            ..Default::default()
        };
        fake.emit(StateUpdate {
            state: auth_state(AuthState::Challenging),
            challenge: Some(challenge),
            ..Default::default()
        });
        wait_until(|| m.status().challenge.is_some()).await;
        fake.emit(StateUpdate::new(
            DrmState {
                process: ProcessState::Running,
                ..DrmState::default()
            },
            "",
        ));
        wait_until(|| m.status().state.process == ProcessState::Running).await;
        let s = m.status();
        assert!(
            s.challenge.is_some() && s.state.authentication == AuthState::Challenging,
            "challenge lost: {s:?}"
        );

        fake.emit(StateUpdate::new(auth_state(AuthState::LoggedIn), ""));
        wait_until(|| m.status().state.authentication == AuthState::LoggedIn).await;
        assert!(
            m.status().challenge.is_none(),
            "the challenge must clear once authentication moves on"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn capabilities_follow_fair_play() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = manager(dir.path(), Fake::new(), policy()).await;
        let mut events = m.subscribe();
        let hub = &m.inner.hub;
        hub.merge(StateUpdate::new(
            DrmState {
                fair_play: FairPlayState::Ready,
                ..DrmState::default()
            },
            "",
        ));
        // The renderer picks lossless or AAC from each event: the one announcing "ready" carries
        // the capabilities.
        let ev = events.recv().await.unwrap();
        assert!(
            ev.capabilities.cbcs
                && ev.capabilities.alac
                && ev.capabilities.atmos
                && ev.capabilities.hi_res
        );
        hub.merge(StateUpdate::new(
            DrmState {
                fair_play: FairPlayState::Failed,
                ..DrmState::default()
            },
            "",
        ));
        assert_eq!(m.status().capabilities, CapabilityState::default());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disable_hi_res() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let cfg = BackendConfig {
            disable_hi_res: true,
            ..BackendConfig::default()
        };
        let m = DrmManager::new(
            fake.clone(),
            Arc::new(SessionManager::new(dir.path())),
            cfg,
            policy(),
        )
        .await;
        m.inner.hub.merge(StateUpdate::new(
            DrmState {
                fair_play: FairPlayState::Ready,
                ..DrmState::default()
            },
            "",
        ));
        let caps = m.status().capabilities;
        assert!(caps.alac && caps.cbcs && !caps.hi_res, "{caps:?}");
    }

    // Concurrent updates reach subscribers in state order: once a delivered snapshot has ALAC,
    // no later one reverts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delivery_order_never_regresses_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..200 {
            let (m, _) = manager(dir.path(), Fake::new(), policy()).await;
            let mut events = m.subscribe();
            let a = tokio::spawn({
                let m = m.clone();
                async move {
                    m.inner.hub.merge(StateUpdate::new(
                        DrmState {
                            session: SessionState::Valid,
                            ..DrmState::default()
                        },
                        "",
                    ))
                }
            });
            let b = tokio::spawn({
                let m = m.clone();
                async move {
                    m.inner.hub.merge(StateUpdate::new(
                        DrmState {
                            fair_play: FairPlayState::Ready,
                            ..DrmState::default()
                        },
                        "",
                    ))
                }
            });
            a.await.unwrap();
            b.await.unwrap();
            let seq = [
                events.recv().await.unwrap().capabilities.alac,
                events.recv().await.unwrap().capabilities.alac,
            ];
            assert_ne!(
                seq,
                [true, false],
                "capabilities regressed after being delivered"
            );
        }
    }

    // ── Crashes ──────────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_crash_restarts_the_backend_and_a_planned_stop_does_not() {
        let dir = tempfile::tempdir().unwrap();
        make_session(dir.path());
        let (m, fake) = manager(dir.path(), Fake::new(), fast_policy(2)).await;
        let stopped = DrmState {
            process: ProcessState::Stopped,
            ..DrmState::default()
        };
        fake.send(DrmEvent {
            update: StateUpdate::new(stopped, "stopped"),
            intentional: true,
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            0,
            "a planned stop must not restart the backend"
        );
        assert_ne!(m.status().state.manager, ManagerState::Failed);

        fake.send(DrmEvent {
            update: StateUpdate::new(stopped, "process crashed"),
            intentional: false,
        });
        wait_until(|| fake.starts.load(Ordering::SeqCst) == 1).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn repeated_crashes_exhaust_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        make_session(dir.path());
        let (m, fake) = manager(dir.path(), Fake::new(), fast_policy(2)).await;
        let crash = DrmEvent {
            update: StateUpdate::new(
                DrmState {
                    process: ProcessState::Stopped,
                    ..DrmState::default()
                },
                "crash",
            ),
            intentional: false,
        };
        for i in 0..=2 {
            fake.running.store(false, Ordering::SeqCst);
            fake.send(crash.clone());
            if i < 2 {
                wait_until(|| fake.running.load(Ordering::SeqCst)).await;
            }
        }
        wait_until(|| m.status().state.manager == ManagerState::Failed).await;
        assert_eq!(m.status().message, "backend crashed 2 times; giving up");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_crash_budget_counts_only_consecutive_crashes() {
        let dir = tempfile::tempdir().unwrap();
        let (m, fake) = manager(dir.path(), Fake::new(), fast_policy(2)).await;
        // A crash loop: every restart crashes again.
        for _ in 0..3 {
            m.handle_crash().await;
            fake.running.store(false, Ordering::SeqCst);
        }
        assert_eq!(fake.starts.load(Ordering::SeqCst), 2);
        assert_eq!(m.status().state.manager, ManagerState::Failed);

        // A crash after a long healthy run gets a fresh budget.
        m.inner.hub.set_manager_state(ManagerState::Ready);
        *m.inner.last_start.lock().unwrap() = Instant::now().checked_sub(2 * CRASH_RESET_WINDOW);
        m.handle_crash().await;
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            3,
            "the budget must reset after a healthy run"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_crash_restart_waits_out_its_backoff_but_not_a_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let p = RestartPolicy {
            restart_backoff: vec![Duration::from_secs(60)],
            ..policy()
        };
        let (m, fake) = manager(dir.path(), Fake::new(), p).await;
        let crash = tokio::spawn({
            let m = m.clone();
            async move { m.handle_crash().await }
        });
        wait_until(|| m.status().message.contains("restarting in 60s")).await;
        m.shutdown().await;
        tokio::time::timeout(Duration::from_secs(2), crash)
            .await
            .expect("shutdown ends the wait")
            .unwrap();
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn default_policy() {
        let p = RestartPolicy::default();
        assert_eq!(p.max_crash_restarts, 5);
        assert_eq!(
            p.restart_backoff,
            [1, 2, 5, 10, 30].map(Duration::from_secs)
        );
        assert_eq!(
            (p.startup_timeout, p.auth_timeout),
            (Duration::from_secs(60), Duration::from_secs(420))
        );
    }
}
