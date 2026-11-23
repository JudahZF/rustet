//! The native backend: Apple's Android FairPlay libraries, loaded in-process through
//! `rustet-drm-native` (port of `rustetplay_backend.go` and `rustetplay_cgo_bridge.go`).
//!
//! A start has two halves. The AAC half is bookkeeping: prepare the base directory, open the
//! call gate, report `RUNNING`. The FairPlay half loads libhybris and Apple's libraries and
//! starts their session; it blocks for seconds. A sign-in runs it before the start returns, so
//! the caller learns whether Apple accepted the account. A session-reuse start runs it in the
//! background, so AAC plays while FairPlay loads (~11 s); a CBCS dial waits for it.
//!
//! Every native call runs on a blocking thread and holds a [`CallGuard`] of the current run, so
//! a stop never tears the session down under a call, and a key context from an earlier run is
//! never used after a restart.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rustet_drm_native::{
    DecryptError, Error as NativeError, FairPlay, FairPlayConfig, Hybris, HybrisPaths, KeyContext,
    KeyContextError, SignInPrompt, VerificationReply, stored_account,
};
use tokio::io::DuplexStream;
use tokio_util::io::SyncIoBridge;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::auth::AuthCoordinator;
use crate::backend::{
    AccountFallback, AccountInfo, BackendConfig, BackendError, BackendLink, Credentials,
    DrmBackend, ProgressiveMvUrl,
};
use crate::cbcs_serve::serve_cbcs;
use crate::deviceid::device_info;
use crate::inflight::{CallGate, Generation};
use crate::session::{create_private_dir, write_private};
use crate::state::{
    DrmEvent, DrmState, FairPlayState, ManagerState, ProcessState, RecoveryState, StateUpdate,
};

/// How long a stop waits for calls still inside the libraries before it goes ahead anyway.
const STOP_DRAIN: Duration = Duration::from_secs(15);
/// How long a CBCS dial waits for a background FairPlay start (the user pressed play on a
/// lossless track while the app was still starting).
const DIAL_INIT_WAIT: Duration = Duration::from_secs(60);
/// How long a FairPlay retry waits for a background FairPlay start to end first.
const RETRY_INIT_WAIT: Duration = Duration::from_secs(30);
/// The key every ALAC init segment names; its context is shared by all tracks.
const PREFETCH_KEY_URI: &str = "skd://itunes.apple.com/P000000000/s1/e1";
/// Buffer of the in-process CBCS pipe, each way.
const CBCS_PIPE_BUFFER: usize = 64 << 10;

/// Deleted from `mpl_db/` before every start. Apple's persistent FPS keys expire within hours,
/// and a key cached in `kvs.sqlitedb` by an earlier session is handed out as if it were valid:
/// AES-CBC with an expired key gives garbage audio. Without the store the library negotiates
/// a fresh key on every start (the prefetch warm-up overlaps it with the CDN download).
/// `httpcache` caches the CKC response, which can carry stale keys too. `accounts.sqlitedb`
/// and `cookies.sqlitedb` (the Apple ID sign-in) stay.
const STALE_KEY_STORE: [&str; 7] = [
    "kvs.sqlitedb",
    "kvs.sqlitedb-wal",
    "kvs.sqlitedb-shm",
    "kvs.db",
    "httpcache.sqlitedb",
    "httpcache.sqlitedb-wal",
    "httpcache.sqlitedb-shm",
];

/// Apple's Android FairPlay libraries as the backend drives them. Every call blocks.
pub(crate) trait FairPlayLib: Send + Sync + 'static {
    /// An open key context.
    type Key: Send + 'static;

    /// Loads libhybris and Apple's libraries (`hybris_backend_init`). Idempotent.
    fn load(&self, paths: &HybrisPaths) -> Result<(), NativeError>;
    /// Starts the FairPlay session, or signs in to a running one (`hybris_fairplay_init`).
    fn start(&self, cfg: FairPlayConfig) -> Result<(), NativeError>;
    /// Opens the key context for (adam ID, `skd://` URI).
    fn open_key(&self, adam_id: &str, uri: &str) -> Result<Self::Key, KeyContextError>;
    /// Decrypts one sample in place.
    fn decrypt(&self, key: &Self::Key, sample: &mut [u8]) -> Result<(), DecryptError>;
    /// Drops every key context so the next opens get fresh keys. Returns whether it worked.
    fn refresh_keys(&self) -> bool;
}

/// The real libraries. Process-wide like Apple's session: one per backend, one backend per
/// process.
#[derive(Default)]
pub(crate) struct Android {
    hybris: OnceLock<&'static Hybris>,
    fair_play: OnceLock<FairPlay>,
}

impl FairPlayLib for Android {
    type Key = KeyContext;

    fn load(&self, paths: &HybrisPaths) -> Result<(), NativeError> {
        let hybris = Hybris::load(paths)?;
        let _ = self.hybris.set(hybris);
        Ok(())
    }

    fn start(&self, cfg: FairPlayConfig) -> Result<(), NativeError> {
        let hybris = self
            .hybris
            .get()
            .ok_or_else(|| NativeError::Core("libhybris is not loaded".into()))?;
        let fair_play = hybris.start_fair_play(cfg)?;
        let _ = self.fair_play.set(fair_play); // every start returns the same session
        Ok(())
    }

    fn open_key(&self, adam_id: &str, uri: &str) -> Result<KeyContext, KeyContextError> {
        self.fair_play
            .get()
            .ok_or(KeyContextError::Failed)?
            .open_key_context(adam_id, uri)
    }

    fn decrypt(&self, key: &KeyContext, sample: &mut [u8]) -> Result<(), DecryptError> {
        self.fair_play
            .get()
            .ok_or(DecryptError::Failed)?
            .decrypt(key, sample)
    }

    fn refresh_keys(&self) -> bool {
        self.fair_play.get().is_some_and(FairPlay::refresh_keys)
    }
}

/// The DRM backend on Apple's Android FairPlay libraries.
pub(crate) struct NativeBackend<L: FairPlayLib>(Arc<Shared<L>>);

struct Shared<L> {
    lib: L,
    /// The DRM directory: `files/` (the default base directory) and `rootfs/` are below it.
    drm_dir: PathBuf,
    /// libhybris. `lib64_dir` is replaced at each start: it sits beside the base directory.
    hybris: HybrisPaths,
    gate: Arc<CallGate>,
    link: OnceLock<BackendLink>,
    /// Held for a whole start, and by a stop, so neither runs under the other.
    start_lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    /// A FairPlay retry runs: every web-session note would otherwise start another.
    retrying: AtomicBool,
}

#[derive(Default)]
struct State {
    base_dir: PathBuf,
    /// The caller's device string; empty: this install's (`device_info`).
    device_info: String,
    /// The Android FairPlay path is up. It outlives a stop: Apple's session is process-wide,
    /// so a restart reuses it.
    fair_play_ready: bool,
    /// Fires when the background FairPlay start of a session-reuse start has ended, either
    /// way. `None` until one ran.
    init_done: Option<CancellationToken>,
    account_fallback: Option<AccountFallback>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The installed layout: `libhybris-core.so` beside the executable and `q.so` in
/// `hybris-linker/` there. `lib64_dir` is empty: it depends on the base directory.
fn installed_hybris() -> HybrisPaths {
    HybrisPaths::beside_executable(PathBuf::new()).unwrap_or_else(|e| {
        warn!("drm: executable path unknown ({e}); looking for libhybris in the working directory");
        HybrisPaths {
            core: "libhybris-core.so".into(),
            linker_dir: "hybris-linker".into(),
            lib64_dir: PathBuf::new(),
        }
    })
}

/// Sets the environment the FairPlay loader needs, for the installed layout and the session
/// directory `base_dir`. Call it first thing in `main`, while the process has one thread:
/// writing the environment later is not sound. Without this call the first FairPlay start
/// writes it.
pub fn set_native_env_before_threads(base_dir: &Path) -> std::io::Result<()> {
    rustet_drm_native::set_env_before_threads(&HybrisPaths {
        lib64_dir: lib64_dir(base_dir),
        ..installed_hybris()
    })
}

impl NativeBackend<Android> {
    /// The backend of the installed layout; Apple's libraries in `<drm_dir>/rootfs/system/lib64`.
    pub(crate) fn installed(drm_dir: &Path) -> Self {
        Self::new(Android::default(), drm_dir, installed_hybris())
    }
}

impl<L: FairPlayLib> NativeBackend<L> {
    pub(crate) fn new(lib: L, drm_dir: &Path, hybris: HybrisPaths) -> Self {
        Self(Arc::new(Shared {
            lib,
            drm_dir: drm_dir.to_owned(),
            hybris,
            gate: Arc::default(),
            link: OnceLock::new(),
            start_lock: tokio::sync::Mutex::new(()),
            state: Mutex::default(),
            retrying: AtomicBool::new(false),
        }))
    }
}

/// `<base_dir>/../rootfs/system/lib64`, where `fetch-fairplay-libs.sh` installs Apple's
/// libraries.
fn lib64_dir(base_dir: &Path) -> PathBuf {
    base_dir
        .parent()
        .unwrap_or(base_dir)
        .join("rootfs")
        .join("system")
        .join("lib64")
}

/// Creates `mpl_db/` and deletes the stale key store. storeservicescore opens its account
/// database below `mpl_db/` while it builds the request context, and a sign-out removes the
/// directory: without it a fresh sign-in can look successful yet fail to save the account.
fn prepare_base_dir(base_dir: &Path) -> Result<(), BackendError> {
    let mpl_db = base_dir.join("mpl_db");
    create_private_dir(&mpl_db)
        .map_err(|e| BackendError::Failed(format!("create DRM session directory: {e}")))?;
    let removed: Vec<&str> = STALE_KEY_STORE
        .into_iter()
        .filter(|name| fs::remove_file(mpl_db.join(name)).is_ok())
        .collect();
    if !removed.is_empty() {
        info!(
            "drm: cleared stale FairPlay key store: {}",
            removed.join(", ")
        );
    }
    Ok(())
}

/// The backend's error for a failed FairPlay start.
fn start_error(e: NativeError) -> BackendError {
    match e {
        NativeError::SignInCancelled => BackendError::SignInCancelled,
        // Apple's own wording when it gave a reason (wrong password, account locked, ...).
        NativeError::Failed(_) | NativeError::DeviceInfo | NativeError::Io(_) => {
            BackendError::Failed(e.to_string())
        }
        // The libraries on disk are not usable: an installation problem, not the account's.
        NativeError::Core(_) | NativeError::Backend(_) | NativeError::MissingSymbol(_) => {
            BackendError::Setup(format!("the Android libraries are not usable ({e})"))
        }
    }
}

/// Asks the user through the sign-in form (`answerChallenge`). Called on Apple's threads,
/// which wait for the answer: Apple's sign-in blocks on it by design.
struct Prompt(Option<Arc<AuthCoordinator>>);

impl SignInPrompt for Prompt {
    fn verification_code(&self) -> VerificationReply {
        let Some(auth) = &self.0 else {
            warn!("drm: Apple asked for a verification code but no sign-in form is connected");
            return VerificationReply::Cancel;
        };
        match auth.answer_native_prompt("2fa").as_str() {
            "" => VerificationReply::Cancel,
            "__resend__" => VerificationReply::Resend,
            "__cant_devices__" => VerificationReply::CantReachDevices,
            code => VerificationReply::Code(code.to_owned()),
        }
    }

    fn dialog_choice(&self, title: &str, buttons: &[String]) -> usize {
        let Some(auth) = &self.0 else {
            warn!("drm: Apple raised a dialog ({title}) but no sign-in form is connected");
            return 0;
        };
        let kind = format!("dialog_choice:{title}\n{}", buttons.join("\n"));
        auth.answer_native_prompt(&kind).trim().parse().unwrap_or(0)
    }
}

impl<L: FairPlayLib> Shared<L> {
    /// Reports a state change. Never blocks: when the manager falls behind, the event is
    /// dropped.
    fn emit(&self, event: DrmEvent) {
        if let Some(link) = self.link.get() {
            let _ = link.events.try_send(event);
        }
    }

    fn emit_state(&self, fair_play: FairPlayState, message: &str) {
        let state = DrmState {
            manager: ManagerState::Ready,
            process: ProcessState::Running,
            fair_play,
            recovery: RecoveryState::Idle,
            ..DrmState::default()
        };
        self.emit(DrmEvent {
            update: StateUpdate::new(state, message),
            intentional: false,
        });
    }

    fn set_fair_play_ready(&self) {
        lock(&self.state).fair_play_ready = true;
        self.emit_state(FairPlayState::Ready, "hybris FairPlay ready");
    }

    fn device_info_for(&self, base_dir: &Path) -> String {
        let configured = lock(&self.state).device_info.clone();
        if !configured.is_empty() {
            return configured;
        }
        device_info(base_dir).unwrap_or_else(|e| {
            warn!("drm: device ID unavailable ({e}); using the shared default");
            String::new()
        })
    }

    /// Loads the libraries and starts FairPlay (`runHybrisInit`). Errors about this
    /// installation are [`BackendError::Setup`]. Blocks.
    fn init_fair_play(
        &self,
        base_dir: &Path,
        credentials: Option<Credentials>,
    ) -> Result<(), BackendError> {
        // libhybris-core.so calls exit(1) when q.so is missing.
        let q_so = self.hybris.linker_dir.join("q.so");
        if fs::metadata(&q_so).is_err() {
            return Err(BackendError::Setup(format!(
                "the Android linker shim is missing ({}); reinstall Rustet",
                q_so.display()
            )));
        }
        if fs::metadata(&self.hybris.core).is_err() {
            return Err(BackendError::Setup(format!(
                "the Android runtime is missing ({}); reinstall Rustet",
                self.hybris.core.display()
            )));
        }
        let lib64_dir = lib64_dir(base_dir);
        if fs::metadata(&lib64_dir).is_err() {
            return Err(BackendError::Setup(format!(
                "the FairPlay libraries are not installed (looked in {}); restart Rustet to download them, or run scripts/fetch-fairplay-libs.sh",
                lib64_dir.display()
            )));
        }
        let paths = HybrisPaths {
            lib64_dir: lib64_dir.clone(),
            ..self.hybris.clone()
        };
        self.lib.load(&paths).map_err(|e| {
            BackendError::Setup(format!("the Android runtime could not start ({e})"))
        })?;

        // Session reuse needs a completed sign-in. The marker is checked, not the account
        // database: Apple's library creates empty SQLite files while it starts.
        if credentials.is_none() && fs::metadata(base_dir.join("session-ready")).is_err() {
            let _ = fs::remove_file(base_dir.join("MUSIC_TOKEN"));
            return Err(BackendError::Failed(
                "not signed in: no session-ready marker, sign in to enable lossless".into(),
            ));
        }

        let cfg = FairPlayConfig {
            base_dir: base_dir.to_owned(),
            lib64_dir: lib64_dir.clone(),
            device_info: self.device_info_for(base_dir),
            credentials: credentials.map(|c| rustet_drm_native::Credentials {
                username: c.email,
                password: c.password,
            }),
            prompt: Arc::new(Prompt(self.link.get().map(|l| Arc::clone(&l.auth)))),
        };
        self.lib.start(cfg).map_err(start_error)?;
        let _ = write_private(&base_dir.join("session-ready"), b"1");
        info!(
            "drm: hybris: FairPlay ready via Android libraries ({})",
            lib64_dir.display()
        );
        Ok(())
    }

    /// Opens the shared prefetch key context under adam ID "0" as soon as FairPlay is up, as
    /// the wrapper's `refresh_decrypt_ctx()` does. The first open pays Android's ~5 s class
    /// loading and the library caches the context, so segment 0 of every ALAC track decrypts
    /// without waiting.
    fn warm_prefetch_key(&self) {
        let Ok(_call) = self.gate.enter(Some(self.gate.generation())) else {
            return;
        };
        let started = Instant::now();
        let ok = self.lib.open_key("0", PREFETCH_KEY_URI).is_ok();
        info!(
            "drm: FairPlay prefetch key context warmed in {:?} (ok={ok})",
            started.elapsed()
        );
    }

    /// Opens one key for a CBCS session of run `generation`, and returns its sample decrypter.
    fn open_cbcs_key(
        self: &Arc<Self>,
        generation: Generation,
        adam_id: &str,
        uri: &str,
    ) -> Result<impl FnMut(&mut [u8]) -> Result<(), BackendError> + use<L>, BackendError> {
        let key = {
            let _call = self.gate.enter(Some(generation))?;
            self.lib.open_key(adam_id, uri).map_err(|e| {
                BackendError::Failed(format!(
                    "FairPlay key context failed for adamID={adam_id} uri={uri}: {e}"
                ))
            })?
        };
        debug!("drm: CBCS: FairPlay key context opened adamID={adam_id} uri={uri}");
        let s = Arc::clone(self);
        Ok(move |sample: &mut [u8]| {
            if sample.len() < 16 {
                return Ok(()); // no whole block: nothing is encrypted
            }
            let _call = s.gate.enter(Some(generation))?;
            s.lib.decrypt(&key, sample).map_err(|e| match e {
                // The streaming loop reconnects on the error and opens the key again.
                DecryptError::StaleContext => BackendError::Failed(e.to_string()),
                DecryptError::Failed => {
                    BackendError::Failed(format!("FairPlay decrypt failed (size={})", sample.len()))
                }
            })
        })
    }

    /// Starts FairPlay for a session-reuse start in the background.
    fn spawn_background_init(self: &Arc<Self>, base_dir: PathBuf) {
        let done = CancellationToken::new();
        lock(&self.state).init_done = Some(done.clone());
        let s = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            match s.init_fair_play(&base_dir, None) {
                Ok(()) => {
                    s.set_fair_play_ready();
                    let warm = Arc::clone(&s);
                    tokio::task::spawn_blocking(move || warm.warm_prefetch_key());
                }
                Err(e) => info!("drm: FairPlay not ready, ALAC disabled: {e}"),
            }
            done.cancel();
        });
    }

    /// Runs a sign-in and gives up at `cancel`. The native call cannot be interrupted: after
    /// the deadline it finishes in the background and its outcome is only logged.
    async fn sign_in(
        self: &Arc<Self>,
        base_dir: PathBuf,
        creds: Credentials,
        cancel: &CancellationToken,
    ) -> Result<(), BackendError> {
        let s = Arc::clone(self);
        let mut job = tokio::task::spawn_blocking(move || s.init_fair_play(&base_dir, Some(creds)));
        tokio::select! {
            joined = &mut job => joined.unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic())),
            () = cancel.cancelled() => {
                tokio::spawn(async move {
                    match job.await {
                        Ok(Ok(())) => info!("drm: sign-in finished after the timeout; restart Rustet to use it"),
                        Ok(Err(e)) => info!("drm: sign-in finished after the timeout: {e}"),
                        Err(_) => {}
                    }
                });
                Err(BackendError::TimedOut)
            }
        }
    }

    /// The blocking half of a FairPlay retry with the web player's token.
    fn retry_fair_play_blocking(&self, base_dir: &Path, music_token: &str) {
        // A web token alone cannot fetch persistent keys: getPersistentKey uses the Apple ID
        // session in mpl_db. Without one the retry would report FairPlay ready while ALAC
        // still fails with "Invalid CKC".
        if fs::metadata(base_dir.join("session-ready")).is_err() {
            info!("drm: FairPlay retry: no session-ready marker, skipping (sign in for ALAC)");
            return;
        }
        if write_private(&base_dir.join("MUSIC_TOKEN"), music_token.as_bytes()).is_ok() {
            info!(
                "drm: FairPlay retry: wrote the web token to MUSIC_TOKEN ({} bytes)",
                music_token.len()
            );
        }
        let cfg = FairPlayConfig {
            base_dir: base_dir.to_owned(),
            lib64_dir: lib64_dir(base_dir),
            device_info: self.device_info_for(base_dir),
            credentials: None,
            prompt: Arc::new(Prompt(self.link.get().map(|l| Arc::clone(&l.auth)))),
        };
        match self.lib.start(cfg) {
            Ok(()) => {
                self.set_fair_play_ready();
                info!("drm: FairPlay retry: ready with the web token");
            }
            Err(e) => info!("drm: FairPlay retry: still failing: {e}"),
        }
    }
}

#[async_trait]
impl<L: FairPlayLib> DrmBackend for NativeBackend<L> {
    fn attach(&self, link: BackendLink) {
        let _ = self.0.link.set(link);
    }

    async fn start(
        &self,
        cfg: BackendConfig,
        cancel: CancellationToken,
    ) -> Result<(), BackendError> {
        let s = &self.0;
        let credentials = cfg.credentials.filter(|c| !c.email.is_empty());
        if credentials.is_some() {
            // A session-reuse start may still be loading FairPlay in the background; a sign-in
            // must not drive the same libraries at the same time.
            let pending = lock(&s.state).init_done.clone();
            if let Some(done) = pending {
                tokio::select! {
                    () = done.cancelled() => {}
                    () = cancel.cancelled() => return Err(BackendError::TimedOut),
                }
            }
        }

        let _start = s.start_lock.lock().await;
        let base_dir = if cfg.base_dir.as_os_str().is_empty() {
            s.drm_dir.join("files")
        } else {
            cfg.base_dir
        };
        prepare_base_dir(&base_dir)?;
        {
            let mut st = lock(&s.state);
            st.base_dir = base_dir.clone();
            st.device_info = cfg.device_info;
        }
        s.emit_state(
            FairPlayState::Unknown,
            "native backend initialized (ALAC init pending)",
        );

        match credentials {
            // A sign-in runs FairPlay before returning, so the caller learns whether Apple
            // accepted the account. It runs even when FairPlay is up (an expired session,
            // another account): the library then signs in instead of starting again.
            Some(creds) => {
                s.sign_in(base_dir, creds, &cancel).await?;
                s.gate.open();
                s.set_fair_play_ready();
                let warm = Arc::clone(s);
                tokio::task::spawn_blocking(move || warm.warm_prefetch_key());
            }
            None => {
                s.gate.open();
                if !lock(&s.state).fair_play_ready {
                    s.spawn_background_init(base_dir);
                }
            }
        }
        // drm_init's state callback: AAC works.
        let fair_play_ready = lock(&s.state).fair_play_ready;
        s.emit(DrmEvent::from_native_state("RUNNING", fair_play_ready));
        Ok(())
    }

    async fn stop(&self) -> Result<(), BackendError> {
        let s = &self.0;
        let _start = s.start_lock.lock().await;
        let gate = Arc::clone(&s.gate);
        let drained = tokio::task::spawn_blocking(move || gate.quiesce(STOP_DRAIN))
            .await
            .unwrap_or(false);
        if !drained {
            warn!(
                "drm: stop: calls still inside the DRM library after {STOP_DRAIN:?}; stopping anyway"
            );
        }
        // drm_shutdown only cleared bookkeeping, and Apple's session cannot be torn down: the
        // libraries stay loaded for the next start.
        Ok(())
    }

    fn running(&self) -> bool {
        self.0.gate.is_open()
    }

    async fn get_m3u8(&self, adam_id: u64) -> Result<String, BackendError> {
        let _call = self.0.gate.enter(None)?;
        // drm_get_hls_url always failed: the Apple provider builds the HLS URL from the catalog.
        Err(BackendError::Failed(format!(
            "failed to get HLS URL for asset {adam_id}"
        )))
    }

    async fn get_account(&self) -> Result<AccountInfo, BackendError> {
        let s = &self.0;
        let _call = s.gate.enter(None)?;
        let base_dir = lock(&s.state).base_dir.clone();
        if let Some(account) = stored_account(&base_dir) {
            // The developer token is not stored; the engine fetches it.
            return Ok(AccountInfo {
                storefront_id: account.storefront_id,
                dev_token: String::new(),
                music_token: account.music_token,
            });
        }
        // No credential files: the web player's token, when the engine has one.
        let fallback = lock(&s.state).account_fallback.clone();
        if let Some(account) = fallback.map(|f| f()).filter(|a| !a.music_token.is_empty()) {
            return Ok(account);
        }
        Err(BackendError::Failed("failed to get account info".into()))
    }

    async fn get_progressive_mv_url(&self, adam_id: u64) -> Result<ProgressiveMvUrl, BackendError> {
        let _call = self.0.gate.enter(None)?;
        // drm_get_progressive_url always failed (the offline path was never ported to it).
        Err(BackendError::Failed(format!(
            "failed to get progressive URL for asset {adam_id}"
        )))
    }

    async fn decrypt_itun_samples(
        &self,
        _adam_id: u64,
        samples: Vec<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>, BackendError> {
        let _call = self.0.gate.enter(None)?;
        // drm_decrypt_itun always failed; empty samples never reached it.
        match samples.iter().position(|s| !s.is_empty()) {
            Some(i) => Err(BackendError::Failed(format!(
                "itun decryption failed for sample {i}: not supported"
            ))),
            None => Ok(samples),
        }
    }

    async fn dial_cbcs(&self, cancel: CancellationToken) -> Result<DuplexStream, BackendError> {
        let s = &self.0;
        let generation = s.gate.generation(); // key contexts opened below belong to this run
        let (ready, init_done) = {
            let st = lock(&s.state);
            (st.fair_play_ready, st.init_done.clone())
        };
        let ready = match init_done {
            Some(done) if !ready => {
                info!(
                    "drm: CBCS dial: FairPlay is still starting; waiting (up to {DIAL_INIT_WAIT:?})"
                );
                tokio::select! {
                    () = done.cancelled() => {}
                    () = tokio::time::sleep(DIAL_INIT_WAIT) => info!("drm: CBCS dial: FairPlay start still running; going on without ALAC"),
                    () = cancel.cancelled() => return Err(BackendError::Failed("CBCS dial cancelled".into())),
                }
                lock(&s.state).fair_play_ready
            }
            _ => ready,
        };

        // ALAC is FairPlay-only: every key, the shared prefetch key included, is opened through
        // getKdContext as the wrapper's handle() does. There is no fallback: without FairPlay
        // the key fails and nothing plays.
        let (client, server) = tokio::io::duplex(CBCS_PIPE_BUFFER);
        let mut conn = SyncIoBridge::new(server);
        let s = Arc::clone(s);
        tokio::task::spawn_blocking(move || {
            let served = serve_cbcs(&cancel, &mut conn, |adam_id, uri| {
                if !ready {
                    return Err(BackendError::Failed(format!(
                        "FairPlay is not ready: cannot open ALAC key {uri}"
                    )));
                }
                s.open_cbcs_key(generation, adam_id, uri)
            });
            if let Err(e) = served {
                info!("drm: CBCS session ended: {e}");
            }
        });
        Ok(client)
    }

    fn in_process(&self) -> bool {
        true
    }

    async fn refresh_keys(&self) -> Result<(), BackendError> {
        let s = Arc::clone(&self.0);
        let refreshed = tokio::task::spawn_blocking(move || {
            let _call = s.gate.enter(Some(s.gate.generation()))?;
            Ok::<_, BackendError>(s.lib.refresh_keys())
        })
        .await
        .map_err(|e| BackendError::Failed(format!("FairPlay key refresh: {e}")))??;
        info!("drm: FairPlay keys refreshed after a key decrypted to garbage (ok={refreshed})");
        if refreshed {
            Ok(())
        } else {
            Err(BackendError::Failed("FairPlay key refresh failed".into()))
        }
    }

    fn set_account_fallback(&self, fallback: AccountFallback) {
        lock(&self.0.state).account_fallback = Some(fallback);
    }

    /// Retries FairPlay with the web player's token when the backend runs but FairPlay failed
    /// at start (an eager start before the page loaded), without a full restart.
    async fn retry_fair_play(&self, music_token: String) {
        let s = &self.0;
        if s.retrying.swap(true, Ordering::SeqCst) {
            return;
        }
        let pending = {
            let st = lock(&s.state);
            (!st.fair_play_ready && s.gate.is_open() && !music_token.is_empty())
                .then(|| st.init_done.clone())
        };
        if let Some(init_done) = pending {
            if let Some(done) = init_done {
                let _ = tokio::time::timeout(RETRY_INIT_WAIT, done.cancelled()).await;
            }
            let (ready, base_dir) = {
                let st = lock(&s.state);
                (st.fair_play_ready, st.base_dir.clone())
            };
            if !ready {
                let shared = Arc::clone(s);
                let _ = tokio::task::spawn_blocking(move || {
                    shared.retry_fair_play_blocking(&base_dir, &music_token)
                })
                .await;
            }
        }
        s.retrying.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc as std_mpsc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    use super::*;
    use crate::signin_failure::SETUP_ERROR_PREFIX;
    use crate::test_support::wait_until;

    type StartFn = Box<dyn FnMut(&FairPlayConfig) -> Result<(), NativeError> + Send>;

    /// The libraries, faked: a key XORs samples with its number (1, 2, ...).
    #[derive(Default)]
    struct FakeLib {
        /// One behaviour per start, in order; `Ok` once they run out.
        starts: Mutex<VecDeque<StartFn>>,
        start_count: AtomicUsize,
        opened: Mutex<Vec<(String, String)>>,
        /// While set, a key open waits for a message.
        hold_open: Mutex<Option<std_mpsc::Receiver<()>>>,
        /// Decrypts fail as after a lease refresh.
        stale: AtomicBool,
    }

    impl FairPlayLib for FakeLib {
        type Key = u8;

        fn load(&self, _: &HybrisPaths) -> Result<(), NativeError> {
            Ok(())
        }

        fn start(&self, cfg: FairPlayConfig) -> Result<(), NativeError> {
            self.start_count.fetch_add(1, Ordering::SeqCst);
            let next = lock(&self.starts).pop_front();
            next.map_or(Ok(()), |mut f| f(&cfg))
        }

        fn open_key(&self, adam_id: &str, uri: &str) -> Result<u8, KeyContextError> {
            let n = {
                let mut opened = lock(&self.opened);
                opened.push((adam_id.into(), uri.into()));
                opened.len() as u8
            };
            if let Some(release) = lock(&self.hold_open).take() {
                let _ = release.recv();
            }
            Ok(n)
        }

        fn decrypt(&self, key: &u8, sample: &mut [u8]) -> Result<(), DecryptError> {
            if self.stale.load(Ordering::SeqCst) {
                return Err(DecryptError::StaleContext);
            }
            sample.iter_mut().for_each(|b| *b ^= key);
            Ok(())
        }

        fn refresh_keys(&self) -> bool {
            true
        }
    }

    impl FakeLib {
        fn then_start(
            self,
            f: impl FnMut(&FairPlayConfig) -> Result<(), NativeError> + Send + 'static,
        ) -> Self {
            lock(&self.starts).push_back(Box::new(f));
            self
        }

        fn opened(&self) -> Vec<(String, String)> {
            lock(&self.opened).clone()
        }
    }

    struct Env {
        dir: tempfile::TempDir,
        base: PathBuf,
        backend: NativeBackend<FakeLib>,
        events: mpsc::Receiver<DrmEvent>,
    }

    /// An installed layout in a temporary directory: libhybris, `q.so`, the Android libraries,
    /// and the base directory `drm/files`.
    fn env_with(lib: FakeLib, auth: Arc<AuthCoordinator>) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("hybris-linker")).unwrap();
        fs::write(root.join("hybris-linker/q.so"), "").unwrap();
        fs::write(root.join("libhybris-core.so"), "").unwrap();
        fs::create_dir_all(root.join("drm/rootfs/system/lib64")).unwrap();
        let hybris = HybrisPaths {
            core: root.join("libhybris-core.so"),
            linker_dir: root.join("hybris-linker"),
            lib64_dir: PathBuf::new(),
        };
        let backend = NativeBackend::new(lib, &root.join("drm"), hybris);
        let (tx, events) = mpsc::channel(64);
        backend.attach(BackendLink { auth, events: tx });
        Env {
            base: root.join("drm/files"),
            dir,
            backend,
            events,
        }
    }

    fn env(lib: FakeLib) -> Env {
        env_with(lib, Arc::new(AuthCoordinator::new(|_| {})))
    }

    impl Env {
        fn lib(&self) -> &FakeLib {
            &self.backend.0.lib
        }

        fn mark_signed_in(&self) {
            fs::create_dir_all(&self.base).unwrap();
            fs::write(self.base.join("session-ready"), "1").unwrap();
        }

        async fn start(&self) -> Result<(), BackendError> {
            self.backend
                .start(BackendConfig::default(), CancellationToken::new())
                .await
        }

        /// Waits for the background FairPlay start that a session-reuse start leaves running.
        /// Signed out, it deletes `MUSIC_TOKEN`, so a test that writes that file waits first.
        async fn background_init_done(&self) {
            let done = lock(&self.backend.0.state).init_done.clone();
            if let Some(done) = done {
                done.cancelled().await;
            }
        }

        async fn sign_in(&self) -> Result<(), BackendError> {
            let creds = Credentials {
                email: "a@b.c".into(),
                password: "pw".into(),
            };
            self.backend
                .start(
                    BackendConfig {
                        credentials: Some(creds),
                        ..BackendConfig::default()
                    },
                    CancellationToken::new(),
                )
                .await
        }

        /// Waits until a FairPlay-ready event arrives; returns the events before it.
        async fn until_fair_play_ready(&mut self) -> Vec<DrmEvent> {
            let mut seen = Vec::new();
            loop {
                let ev = tokio::time::timeout(Duration::from_secs(3), self.events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if ev.update.state.fair_play == FairPlayState::Ready {
                    return seen;
                }
                seen.push(ev);
            }
        }

        fn drain_events(&mut self) -> Vec<DrmEvent> {
            std::iter::from_fn(|| self.events.try_recv().ok()).collect()
        }
    }

    async fn select_key(c: &mut DuplexStream, adam_id: &str, uri: &str) {
        let mut header = vec![adam_id.len() as u8];
        header.extend_from_slice(adam_id.as_bytes());
        header.push(uri.len() as u8);
        header.extend_from_slice(uri.as_bytes());
        c.write_all(&header).await.unwrap();
    }

    /// Sends one sample; `None` when the server ended the session instead of answering.
    async fn round_trip(c: &mut DuplexStream, sample: &[u8]) -> Option<Vec<u8>> {
        c.write_all(&(sample.len() as u32).to_le_bytes())
            .await
            .ok()?;
        c.write_all(sample).await.ok()?;
        let mut out = vec![0; sample.len()];
        tokio::time::timeout(Duration::from_secs(3), c.read_exact(&mut out))
            .await
            .unwrap()
            .ok()?;
        Some(out)
    }

    /// Whether the server closed the session (EOF, not a hang).
    async fn closed(c: &mut DuplexStream) -> bool {
        let mut b = [0u8; 1];
        matches!(
            tokio::time::timeout(Duration::from_secs(3), c.read(&mut b)).await,
            Ok(Ok(0))
        )
    }

    // What a session-reuse start leaves on disk, and the order the manager hears about it:
    // AAC at once, FairPlay once the background start is done, then the prefetch warm-up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_reuse_starts_fair_play_in_the_background() {
        let mut e = env(FakeLib::default());
        e.mark_signed_in();
        let mpl_db = e.base.join("mpl_db");
        fs::create_dir_all(&mpl_db).unwrap();
        for f in [
            "kvs.sqlitedb",
            "kvs.sqlitedb-wal",
            "httpcache.sqlitedb",
            "accounts.sqlitedb",
            "cookies.sqlitedb",
        ] {
            fs::write(mpl_db.join(f), "x").unwrap();
        }

        e.start().await.unwrap();
        assert!(e.backend.running());
        let before = e.until_fair_play_ready().await;
        assert_eq!(
            before[0].update.message,
            "native backend initialized (ALAC init pending)"
        );
        assert_eq!(before[0].update.state.process, ProcessState::Running);

        let left: Vec<_> = fs::read_dir(&mpl_db)
            .unwrap()
            .map(|d| d.unwrap().file_name())
            .collect();
        assert_eq!(
            left.len(),
            2,
            "only the Apple ID files survive the start: {left:?}"
        );
        assert!(
            mpl_db.join("accounts.sqlitedb").exists() && mpl_db.join("cookies.sqlitedb").exists()
        );
        wait_until(|| e.lib().opened() == [("0".to_owned(), PREFETCH_KEY_URI.to_owned())]).await;
        assert_eq!(e.lib().start_count.load(Ordering::SeqCst), 1);

        // A restart reuses Apple's process-wide session and says so at once.
        e.backend.stop().await.unwrap();
        assert!(!e.backend.running());
        e.drain_events();
        e.start().await.unwrap();
        assert_eq!(
            e.lib().start_count.load(Ordering::SeqCst),
            1,
            "a restart must not start FairPlay again"
        );
        e.until_fair_play_ready().await;
    }

    // Signed out (no session-ready marker): AAC works, FairPlay stays off, the web token the
    // library would otherwise reuse is removed, and every ALAC key fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_a_session_marker_fair_play_stays_off() {
        let mut e = env(FakeLib::default());
        fs::create_dir_all(&e.base).unwrap();
        fs::write(e.base.join("MUSIC_TOKEN"), "web").unwrap();
        e.start().await.unwrap();

        let mut c = e.backend.dial_cbcs(CancellationToken::new()).await.unwrap(); // waits for the start
        assert!(!e.base.join("MUSIC_TOKEN").exists());
        select_key(&mut c, "0", PREFETCH_KEY_URI).await;
        assert!(closed(&mut c).await, "a key was served without FairPlay");
        assert_eq!(e.lib().start_count.load(Ordering::SeqCst), 0);
        assert!(e.lib().opened().is_empty());
        assert!(
            e.drain_events()
                .iter()
                .all(|ev| ev.update.state.fair_play != FairPlayState::Ready)
        );
    }

    // A lease refresh frees the key contexts: the session ends, and the reconnect opens the
    // key again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lease_refresh_ends_the_cbcs_session_and_a_new_one_reopens_the_key() {
        let mut e = env(FakeLib::default());
        e.mark_signed_in();
        e.start().await.unwrap();
        e.until_fair_play_ready().await;
        wait_until(|| e.lib().opened().len() == 1).await; // the warm-up

        let mut c = e.backend.dial_cbcs(CancellationToken::new()).await.unwrap();
        select_key(&mut c, "1440833098", "skd://real").await;
        assert_eq!(round_trip(&mut c, &[0; 16]).await.unwrap(), [2; 16]);
        assert_eq!(
            round_trip(&mut c, &[7; 15]).await.unwrap(),
            [7; 15],
            "a partial block is not encrypted"
        );

        e.lib().stale.store(true, Ordering::SeqCst);
        assert_eq!(round_trip(&mut c, &[0; 16]).await, None);
        e.lib().stale.store(false, Ordering::SeqCst);

        let mut c = e.backend.dial_cbcs(CancellationToken::new()).await.unwrap();
        select_key(&mut c, "1440833098", "skd://real").await;
        assert_eq!(round_trip(&mut c, &[0; 16]).await.unwrap(), [3; 16]);
        assert_eq!(e.lib().opened().len(), 3);
    }

    // A session dialled before a restart belongs to the old run: it cannot open a key in the
    // new one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_from_a_previous_run_is_refused() {
        let mut e = env(FakeLib::default());
        e.mark_signed_in();
        e.start().await.unwrap();
        e.until_fair_play_ready().await;
        wait_until(|| e.lib().opened().len() == 1).await;

        let mut old = e.backend.dial_cbcs(CancellationToken::new()).await.unwrap();
        e.backend.stop().await.unwrap();
        e.start().await.unwrap();
        select_key(&mut old, "1", "skd://a").await;
        assert!(closed(&mut old).await);
        assert_eq!(
            e.lib().opened().len(),
            1,
            "the old session reached the library"
        );
    }

    // A stop refuses new calls at once, but waits for a call already inside the library.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_waits_for_a_key_open_in_progress() {
        let mut e = env(FakeLib::default());
        e.mark_signed_in();
        e.start().await.unwrap();
        e.until_fair_play_ready().await;
        wait_until(|| e.lib().opened().len() == 1).await;

        let (release, held) = std_mpsc::channel();
        *lock(&e.lib().hold_open) = Some(held);
        let mut c = e.backend.dial_cbcs(CancellationToken::new()).await.unwrap();
        select_key(&mut c, "1", "skd://a").await;
        wait_until(|| e.lib().opened().len() == 2).await;

        let backend = NativeBackend(Arc::clone(&e.backend.0));
        let stop = tokio::spawn(async move { backend.stop().await });
        wait_until(|| !e.backend.running()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !stop.is_finished(),
            "stop returned with a call inside the library"
        );
        assert!(matches!(
            e.backend.get_account().await,
            Err(BackendError::NotRunning)
        ));

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), stop)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    // Missing files are installation problems, reported with the prefix that keeps the saved
    // password.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sign_in_without_the_libraries_is_a_setup_error() {
        let e = env(FakeLib::default());
        fs::remove_file(e.dir.path().join("hybris-linker/q.so")).unwrap();
        let err = e.sign_in().await.unwrap_err();
        assert!(
            matches!(&err, BackendError::Setup(m) if m.contains("q.so")),
            "{err:?}"
        );
        assert!(err.to_string().starts_with(SETUP_ERROR_PREFIX));

        fs::write(e.dir.path().join("hybris-linker/q.so"), "").unwrap();
        fs::remove_dir(e.dir.path().join("drm/rootfs/system/lib64")).unwrap();
        let err = e.sign_in().await.unwrap_err();
        assert!(
            matches!(&err, BackendError::Setup(m) if m.contains("fetch-fairplay-libs.sh")),
            "{err:?}"
        );
        assert!(
            !e.backend.running(),
            "a failed sign-in left the backend running"
        );
        assert_eq!(e.lib().start_count.load(Ordering::SeqCst), 0);
    }

    // Apple's questions reach the sign-in form, and the answers come back in the form the
    // library wants: digits only, and a button index.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sign_in_asks_the_user_and_reports_fair_play_ready() {
        let replies = Arc::new(Mutex::new(VecDeque::from(["123 456", "1"])));
        let auth = Arc::new(OnceLock::<Arc<AuthCoordinator>>::new());
        let coordinator = Arc::new(AuthCoordinator::new({
            let (auth, replies) = (Arc::clone(&auth), Arc::clone(&replies));
            move |u: StateUpdate| {
                if u.challenge.is_some()
                    && let Some(reply) = lock(&replies).pop_front()
                {
                    auth.get().unwrap().submit_reply(reply.into()).unwrap();
                }
            }
        }));
        let _ = auth.set(Arc::clone(&coordinator));

        let answers = Arc::new(Mutex::new(None));
        let lib = FakeLib::default().then_start({
            let answers = Arc::clone(&answers);
            move |cfg| {
                assert_eq!(cfg.credentials.as_ref().unwrap().username, "a@b.c");
                let code = cfg.prompt.verification_code();
                let choice = cfg
                    .prompt
                    .dialog_choice("Verify", &["Text".into(), "Call".into()]);
                *lock(&answers) = Some((code, choice));
                Ok(())
            }
        });
        let mut e = env_with(lib, coordinator);

        e.sign_in().await.unwrap();
        assert_eq!(
            *lock(&answers),
            Some((VerificationReply::Code("123456".into()), 1))
        );
        assert!(e.backend.running());
        assert!(e.base.join("session-ready").exists());
        e.until_fair_play_ready().await;
        wait_until(|| e.lib().opened().len() == 1).await; // the warm-up runs after a sign-in too
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_or_rejected_sign_in_leaves_the_backend_stopped() {
        let lib = FakeLib::default()
            .then_start(|_| Err(NativeError::SignInCancelled))
            .then_start(|_| {
                Err(NativeError::Failed(Some(
                    "Your Apple ID or password was incorrect.".into(),
                )))
            });
        let e = env(lib);
        assert!(matches!(
            e.sign_in().await,
            Err(BackendError::SignInCancelled)
        ));
        assert!(!e.backend.running());
        let err = e.sign_in().await.unwrap_err();
        assert!(
            matches!(&err, BackendError::Failed(m) if m == "Your Apple ID or password was incorrect."),
            "{err:?}"
        );
        assert!(!e.backend.running() && !e.base.join("session-ready").exists());
    }

    // The native sign-in cannot be interrupted: the start gives up at the deadline and the
    // sign-in finishes on its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sign_in_past_its_deadline_times_out() {
        let (release, held) = std_mpsc::channel::<()>();
        let held = Mutex::new(held);
        let e = env(FakeLib::default().then_start(move |_| {
            let _ = lock(&held).recv();
            Ok(())
        }));
        let cancel = CancellationToken::new();
        let creds = Credentials {
            email: "a@b.c".into(),
            password: "pw".into(),
        };
        let start = e.backend.start(
            BackendConfig {
                credentials: Some(creds),
                ..BackendConfig::default()
            },
            cancel.clone(),
        );
        let (result, ()) = tokio::join!(start, async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        assert!(matches!(result, Err(BackendError::TimedOut)), "{result:?}");
        assert!(!e.backend.running());
        release.send(()).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn account_comes_from_the_session_files_then_the_web_player() {
        let e = env(FakeLib::default());
        assert!(matches!(
            e.backend.get_account().await,
            Err(BackendError::NotRunning)
        ));
        e.start().await.unwrap();
        e.background_init_done().await;
        assert!(matches!(
            e.backend.get_account().await,
            Err(BackendError::Failed(_))
        ));

        let web = AccountInfo {
            storefront_id: "us".into(),
            dev_token: String::new(),
            music_token: "web".into(),
        };
        e.backend
            .set_account_fallback(Arc::new(move || web.clone()));
        assert_eq!(e.backend.get_account().await.unwrap().music_token, "web");

        fs::write(e.base.join("MUSIC_TOKEN"), "file\n").unwrap();
        fs::write(e.base.join("STOREFRONT_ID"), "143441-1,32\n").unwrap();
        let account = e.backend.get_account().await.unwrap();
        assert_eq!(
            (account.music_token.as_str(), account.storefront_id.as_str()),
            ("file", "143441-1,32")
        );

        // What the C library never supported fails, rather than pretending.
        assert!(e.backend.get_m3u8(1).await.is_err());
        assert!(e.backend.get_progressive_mv_url(1).await.is_err());
        assert_eq!(
            e.backend
                .decrypt_itun_samples(1, vec![vec![]])
                .await
                .unwrap(),
            [Vec::<u8>::new()]
        );
        assert!(
            e.backend
                .decrypt_itun_samples(1, vec![vec![], vec![1]])
                .await
                .is_err()
        );
    }
}

/// The real libraries, without an account. Run with `RUSTET_HYBRIS_DIR` (the folder with
/// `libhybris-core.so` and `hybris-linker/`) and `RUSTET_FAIRPLAY_LIB64` (Apple's Android
/// libraries) set: `cargo test -p rustet-drm -- --ignored --nocapture real_libraries`.
#[cfg(test)]
mod real {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::{DrmManager, Error, RestartPolicy, SessionManager};

    // Signed out, nothing starts: the renderer shows "signed out". With a session marker but no
    // account (an expired session) the backend starts: the lease passes, the storefront and a
    // developer token arrive, the silent sign-in finds no credentials and FairPlay stays off
    // ("Your Apple session has expired"), so only AAC is offered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "loads Apple's FairPlay libraries through libhybris and talks to Apple; needs RUSTET_HYBRIS_DIR and RUSTET_FAIRPLAY_LIB64"]
    async fn real_libraries_signed_out_start() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_test_writer()
            .try_init();
        let hybris_dir =
            PathBuf::from(std::env::var_os("RUSTET_HYBRIS_DIR").expect("RUSTET_HYBRIS_DIR"));
        let lib64 = std::env::var_os("RUSTET_FAIRPLAY_LIB64").expect("RUSTET_FAIRPLAY_LIB64");
        let dir = tempfile::tempdir().unwrap();
        let drm = dir.path().join("drm");
        fs::create_dir_all(drm.join("rootfs/system")).unwrap();
        std::os::unix::fs::symlink(lib64, drm.join("rootfs/system/lib64")).unwrap();
        let base = drm.join("files");
        let hybris = HybrisPaths {
            core: hybris_dir.join("libhybris-core.so"),
            linker_dir: hybris_dir.join("hybris-linker"),
            lib64_dir: PathBuf::new(),
        };
        let backend = Arc::new(NativeBackend::new(Android::default(), &drm, hybris));
        let m = DrmManager::new(
            backend,
            Arc::new(SessionManager::new(&base)),
            BackendConfig {
                base_dir: base.clone(),
                ..BackendConfig::default()
            },
            RestartPolicy::default(),
        )
        .await;

        let signed_out = serde_json::to_value(m.status()).unwrap();
        println!("signed out: {signed_out}");
        assert_eq!(signed_out["state"]["session"], "empty");
        assert_eq!(signed_out["state"]["authentication"], "logged_out");
        assert_eq!(signed_out["state"]["process"], "stopped");
        assert!(matches!(
            m.dial_cbcs(CancellationToken::new()).await,
            Err(Error::NotAuthenticated)
        ));

        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("session-ready"), "1").unwrap();
        let started = Instant::now();
        let mut c = m.dial_cbcs(CancellationToken::new()).await.unwrap(); // waits for FairPlay
        println!("FairPlay start ended after {:?}", started.elapsed());
        c.write_all(b"\x010\x27skd://itunes.apple.com/P000000000/s1/e1")
            .await
            .unwrap();
        let mut b = [0u8; 1];
        assert_eq!(
            c.read(&mut b).await.unwrap(),
            0,
            "a key was served without an account"
        );

        let snap = serde_json::to_value(m.status()).unwrap();
        println!("expired session: {snap}");
        assert_eq!(snap["state"]["process"], "running");
        assert_eq!(snap["state"]["manager"], "ready");
        assert_ne!(snap["state"]["fairplay"], "ready");
        assert_eq!(
            snap["capabilities"],
            serde_json::json!({"cbcs": false, "alac": false, "atmos": false, "hiRes": false})
        );
        assert!(
            !base.join("MUSIC_TOKEN").exists(),
            "the expired session wrote a music token"
        );
        m.shutdown().await;
    }
}
