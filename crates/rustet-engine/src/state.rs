//! The engine state shared by every handler, and its construction.
//!
//! [`Engine`] is built once by [`Engine::build`] and shared as [`AppState`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rustet_apple::{AccountTokenSource, AppleMusicProvider, ProgressiveMvUrl};
use rustet_core::appdir;
use rustet_core::config::Config;
use rustet_core::pipeline::BoxError;
use rustet_core::ring;
use rustet_diskcache::Cache;
use rustet_drm::{
    BackendConfig, DrmManager, DrmSnapshot, FairPlayState, RestartPolicy, SessionLock,
    SessionManager, WebSession, native_backend, normalize_storefront_id,
};
use rustet_fairplay::{CbcsDialer, DrmStream};
use rustet_library::Store;
use rustet_playback::Manager;
use rustet_playback::prefetch::{DEFAULT_WORKERS, Scheduler};
use rustet_streams::Streams;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::alacgate::AlacGate;
use crate::handlers::mvcache::MvPrefs;
use crate::handlers::vlc::{VlcGate, invalidate_alac_cache_if_stale};
use crate::handlers::vseg::VsegStates;

use crate::breaker::CircuitBreaker;
use crate::epoch::{DRM_REFRESH_LEAD, DRM_SESSION_TTL, Epoch, Lifecycle};
use crate::events::EventBus;
use crate::guard::GuardConfig;

/// What every handler receives.
pub(crate) type AppState = Arc<Engine>;

/// The engine: everything the handlers share.
pub(crate) struct Engine {
    pub guard: Arc<GuardConfig>,
    pub epoch: Arc<Epoch>,
    pub lifecycle: Lifecycle,
    pub events: Arc<EventBus>,
    pub tokens: Arc<Tokens>,
    /// `None` when this build has no DRM backend: lossless can never become ready.
    pub drm: Option<Drm>,
    /// The shared HTTP client (rustls).
    pub http: reqwest::Client,
    pub amp: rustet_ampapi::Client,
    /// Base URL of the Apple Music API for the requests the handlers build themselves.
    pub amp_base: String,
    /// The AAC / music-video stream context.
    pub streams: Arc<Streams>,
    /// The persisted music-video cache settings.
    pub mv_prefs: MvPrefs,
    /// Decrypted-track disk cache; `None` when its directory cannot be created.
    pub disk_cache: Option<Cache>,
    /// The library metadata cache; `None` when it could not be opened.
    pub library: Option<Store>,
    /// The playback manager and pre-warm scheduler. `None` only in tests.
    pub playback: Option<Playback>,
    /// Track and music-video exports. `None` only in tests.
    pub export: Option<rustet_export::Manager>,
    /// Parks background audio cache downloads while the player's track starts.
    pub alac: Arc<AlacGate>,
    /// The segmented music-video producers, by session.
    pub vseg: VsegStates,
    /// The libvlc player; `None` when libvlc is not available.
    pub vlc: Option<Vlc>,
    /// Session-open latencies in ms (last 100), recorded by the playback handler.
    pub open_latency: ring::Buffer,
    /// Session-open circuit breaker, consulted by the playback handler.
    pub open_breaker: CircuitBreaker,
    /// Cancelled when the engine stops: ends SSE streams and background tasks.
    pub shutdown: CancellationToken,
    /// Where the engine keeps its own files (`<config>/engine-data`): the player's snapshot,
    /// history and preferences.
    pub data_dir: PathBuf,
    /// Temp files of deck streams outside the disk cache (`player::spool`).
    pub spool_dir: PathBuf,
    /// Where the decks put music-video frame rings (`rustet_framering`), private to the user.
    /// `None` when no private directory could be made: music videos then play as audio.
    pub video_dir: Option<PathBuf>,
    /// The typed Apple client with the engine's tokens: the player's catalog calls and play
    /// reports, and the account's storefront. Made at first use.
    pub(crate) apple: OnceLock<Arc<crate::player::AppleLink>>,
    /// The player controller, once [`Engine::start_player`] ran.
    pub player: OnceLock<rustet_player::PlayerHandle>,
    /// The player's two decks, stopped when the engine stops.
    pub player_decks: OnceLock<[Arc<dyn rustet_player::ports::Deck>; 2]>,
    /// The capabilities last sent to the player.
    pub player_caps: Mutex<Option<rustet_player::model::Capabilities>>,
    /// Which side drives libvlc: the player's decks (true) or the `/api/v1/vlc/*` routes. See
    /// [`Engine::player_takes_libvlc`].
    pub player_owns_libvlc: AtomicBool,
}

/// The DRM subsystem, present when the build has a backend.
pub(crate) struct Drm {
    pub manager: DrmManager,
    /// The backend's name for `/api/v1/drm/status`.
    pub backend_name: &'static str,
    /// The session directory, guarded by the session lock while the engine runs.
    pub session_dir: PathBuf,
    /// A session existed at startup: start the backend at once instead of on first use.
    pub eager_start: bool,
}

/// The libvlc player and the session that owns it.
pub(crate) struct Vlc {
    pub player: Arc<rustet_vlc::Player>,
    pub gate: VlcGate,
}

/// The playback side: the session manager and the pre-warm scheduler over it.
pub(crate) struct Playback {
    pub manager: Arc<Manager>,
    pub scheduler: Scheduler,
}

impl Playback {
    /// Starts the pre-warm scheduler over `manager`. Credentials are read when a context is
    /// submitted, so token rotations are picked up. Each prefetch event goes out under its own
    /// kind as the SSE event name (`prefetch.cached`, `prefetch.done`, …).
    pub fn new(
        manager: Arc<Manager>,
        tokens: &Arc<Tokens>,
        epoch: Arc<Epoch>,
        events: Arc<EventBus>,
    ) -> Self {
        let (t1, t2) = (tokens.clone(), tokens.clone());
        let scheduler = Scheduler::new(
            manager.clone(),
            move || t1.developer(),
            move || t2.music_user_token(),
            move |ev| events.emit(&epoch, ev.kind.as_str(), &ev),
            DEFAULT_WORKERS,
        );
        Self { manager, scheduler }
    }
}

fn read<T: Clone>(l: &RwLock<T>) -> T {
    l.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The tokens the handlers use. Shared with the DRM manager's token sources.
pub(crate) struct Tokens {
    /// The DRM session files: the canonical source of the media-user-token and storefront.
    pub session: Arc<SessionManager>,
    /// The developer token from the most recent renderer push or scrape.
    developer: RwLock<String>,
    /// MusicKit JS's Music-User-Token (web auth, not the Android DRM session).
    music_kit: RwLock<String>,
    /// The storefront code (`us`) and language (`en-US`) the UI last pushed with its token.
    locale: RwLock<(String, String)>,
}

impl Tokens {
    pub fn new(session: Arc<SessionManager>) -> Self {
        Self {
            session,
            developer: RwLock::default(),
            music_kit: RwLock::default(),
            locale: RwLock::default(),
        }
    }

    /// The cached developer token; empty when none was pushed or scraped yet.
    pub fn developer(&self) -> String {
        read(&self.developer)
    }

    /// Caches a developer token. Empty values are ignored.
    pub fn set_developer(&self, token: &str) {
        if !token.is_empty() {
            *self
                .developer
                .write()
                .unwrap_or_else(PoisonError::into_inner) = token.to_owned();
        }
    }

    /// Stores the MusicKit token. Returns whether it changed; empty values are ignored.
    pub fn set_music_kit(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let mut cur = self
            .music_kit
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let changed = *cur != token;
        token.clone_into(&mut cur);
        changed
    }

    /// Forgets the MusicKit token (sign-out). Returns whether there was one.
    pub fn clear_music_kit(&self) -> bool {
        !std::mem::take(
            &mut *self
                .music_kit
                .write()
                .unwrap_or_else(PoisonError::into_inner),
        )
        .is_empty()
    }

    /// Whether the UI pushed a web token.
    pub fn has_music_kit(&self) -> bool {
        !read(&self.music_kit).is_empty()
    }

    /// Keeps the storefront code and the language the UI uses. Empty values keep the old ones.
    pub fn set_locale(&self, storefront: &str, language: &str) {
        let mut cur = self.locale.write().unwrap_or_else(PoisonError::into_inner);
        if !storefront.is_empty() {
            storefront.clone_into(&mut cur.0);
        }
        if !language.is_empty() {
            language.clone_into(&mut cur.1);
        }
    }

    /// Forgets the storefront code at a sign-out: the next account can have another one.
    pub fn forget_storefront(&self) {
        self.locale
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .clear();
    }

    /// The storefront code the UI pushed (or the player asked Apple for), if any.
    pub fn pushed_storefront(&self) -> Option<String> {
        Some(read(&self.locale).0).filter(|s| !s.is_empty())
    }

    /// The storefront for Apple requests that have none of their own: what the UI pushed
    /// (`gb`), else the FairPlay session's (`143444`). Empty when neither is known.
    pub fn playback_storefront(&self) -> String {
        self.pushed_storefront()
            .unwrap_or_else(|| self.storefront())
    }

    /// The storefront code the UI pushed (`us`), else `us`.
    pub fn storefront_code(&self) -> String {
        let code = read(&self.locale).0;
        if code.is_empty() {
            "us".to_owned()
        } else {
            code
        }
    }

    /// The language the UI pushed, else `en-US`.
    pub fn language(&self) -> String {
        let language = read(&self.locale).1;
        if language.is_empty() {
            "en-US".to_owned()
        } else {
            language
        }
    }

    /// The media-user-token from the DRM session files, read fresh so rotations are seen.
    pub fn media_user_token(&self) -> String {
        self.session.read_music_token()
    }

    /// The MusicKit token if the renderer pushed one, else the DRM session token.
    pub fn music_user_token(&self) -> String {
        let mk = read(&self.music_kit);
        if mk.is_empty() {
            self.media_user_token()
        } else {
            mk
        }
    }

    /// Only what the web player supplied: the DRM session's web token source.
    pub fn web_session(&self) -> WebSession {
        WebSession {
            music_user_token: read(&self.music_kit),
            storefront: String::new(),
        }
    }

    /// The session's storefront without the platform suffix (`143467-2,31` → `143467`).
    pub fn storefront(&self) -> String {
        normalize_storefront_id(&self.session.read_storefront_id()).to_owned()
    }
}

/// Milliseconds since the Unix epoch.
pub(crate) fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshDue {
    ready_since_ms: i64,
    refresh_at_ms: i64,
}

/// Where the DRM layer keeps its files.
pub(crate) struct DrmDirs {
    /// Holds `files/` and `rootfs/`. Only its location is used.
    pub drm_dir: PathBuf,
    /// The FairPlay session directory.
    pub session_dir: PathBuf,
}

impl DrmDirs {
    /// From the config, or `drm/` under the working directory (the shell starts the engine in
    /// its data directory).
    pub(crate) fn from_config(cfg: &Config) -> Self {
        let drm_binary_path = cfg
            .drm_binary_path
            .clone()
            .or_else(|| std::path::absolute("drm").ok().filter(|p| p.exists()));
        let session_dir = cfg
            .drm_base_dir
            .clone()
            .or_else(|| drm_binary_path.as_ref().map(|p| p.join("files")))
            .unwrap_or_else(|| std::env::temp_dir().join("rustet-drm").join("files"));
        let drm_dir = drm_binary_path.unwrap_or_else(|| std::env::temp_dir().join("rustet-drm"));
        Self {
            drm_dir,
            session_dir,
        }
    }
}

impl Engine {
    /// Builds the engine from the config. Nothing is served yet; see [`crate::server`].
    pub async fn build(port: u16, cfg: &Config) -> anyhow::Result<Arc<Self>> {
        let http = reqwest::Client::builder().build()?;
        let shutdown = CancellationToken::new();

        let DrmDirs {
            drm_dir,
            session_dir,
        } = DrmDirs::from_config(cfg);

        let session = Arc::new(SessionManager::new(&session_dir));
        let tokens = Arc::new(Tokens::new(session.clone()));
        let drm = match native_backend(&drm_dir) {
            Some(backend) => {
                info!(name = "rustetplay", "DRM backend");
                Some(Self::build_drm(backend, session, session_dir, &tokens).await)
            }
            None => {
                warn!("DRM backend unavailable in this build; DRM features disabled");
                None
            }
        };

        let streams = Arc::new(Streams::new(http.clone()));
        streams.warm_caches();
        let cache_base = appdir::cache_dir();
        let mv_prefs =
            MvPrefs::load(cache_base.join("engine").join("mv-prefs.json"), &streams).await;

        // Limits (persistLimitMB, persistTTLDays) come from the renderer at startup through
        // PUT /api/v1/cache/config; zero means unlimited / no expiry.
        let playback_dir = cache_base.join("playback");
        let disk_cache = Cache::new(&playback_dir)
            .inspect_err(|e| warn!("disk cache disabled: {e}"))
            .ok();
        if disk_cache.is_some() {
            let _ =
                tokio::task::spawn_blocking(move || invalidate_alac_cache_if_stale(&playback_dir))
                    .await;
        }
        // Library metadata cache: songs and playlist membership for instant queue building.
        // Sync is driven by the renderer through POST /api/v1/library/ingest.
        let library = open_library(cache_base).await;

        // The DRM manager opens the FairPlay connections of lossless tracks and supplies the
        // progressive music-video URL. Without a backend those requests fail.
        let (cbcs, account) = match &drm {
            Some(d) => (
                Some(Arc::new(DrmLink(d.manager.clone())) as Arc<dyn CbcsDialer>),
                Some(Arc::new(DrmLink(d.manager.clone())) as Arc<dyn AccountTokenSource>),
            ),
            None => (None, None),
        };
        let provider = AppleMusicProvider::new(http.clone(), streams.clone(), cbcs, account);
        let manager = Manager::new(Arc::new(provider));
        let (epoch, events) = (Arc::new(Epoch::new()), Arc::new(EventBus::new()));
        let playback = Playback::new(manager.clone(), &tokens, epoch.clone(), events.clone());
        let export = {
            let (epoch, events) = (epoch.clone(), events.clone());
            rustet_export::Manager::new(
                manager.clone(),
                http.clone(),
                rustet_export::Options {
                    events: Some(Box::new(move |ev| events.emit(&epoch, "export", &ev))),
                    cache: disk_cache
                        .clone()
                        .map(|c| Arc::new(c) as Arc<dyn rustet_export::AudioCache>),
                    floor_bps: u64::from(cfg.export_throttle_floor_kbps.unwrap_or(0)) << 10,
                    output_roots: crate::handlers::export::export_roots(),
                    more_roots: Some(Box::new(crate::handlers::export::picked_export_roots)),
                },
            )
        };

        // libvlc plays lossless tracks the browser cannot decode. Without it the VLC routes
        // answer 503 and the renderer falls back to MSE.
        let vlc = match tokio::task::spawn_blocking(rustet_vlc::Player::new).await {
            Ok(Ok(player)) => Some(Vlc {
                player: Arc::new(player),
                gate: VlcGate::default(),
            }),
            Ok(Err(e)) => {
                info!("{e}; VLC playback disabled");
                None
            }
            Err(e) => {
                warn!("libvlc init: {e}");
                None
            }
        };

        let engine = Arc::new(Self {
            guard: Arc::new(GuardConfig::from_env(port)),
            epoch,
            lifecycle: Lifecycle::new(),
            events,
            tokens,
            drm,
            amp: rustet_ampapi::Client::new(http.clone()),
            amp_base: rustet_ampapi::AMP_BASE.to_owned(),
            http,
            streams,
            mv_prefs,
            disk_cache,
            library,
            playback: Some(playback),
            export: Some(export),
            alac: Arc::default(),
            vseg: VsegStates::default(),
            vlc,
            open_latency: ring::Buffer::new(100),
            open_breaker: CircuitBreaker::new(3, Duration::from_secs(60)),
            shutdown,
            data_dir: Config::default_path()
                .parent()
                .map_or_else(|| appdir::config_dir().to_owned(), Path::to_owned),
            spool_dir: cache_base.join("engine").join("spool"),
            video_dir: rustet_framering::default_dir()
                .inspect_err(|e| warn!("music videos play as audio only: {e}"))
                .ok(),
            apple: OnceLock::new(),
            player: OnceLock::new(),
            player_decks: OnceLock::new(),
            player_caps: Mutex::new(None),
            player_owns_libvlc: AtomicBool::new(false),
        });
        engine.install_release_hook();
        engine.spawn_background();
        Ok(engine)
    }

    async fn build_drm(
        backend: Arc<dyn rustet_drm::DrmBackend>,
        session: Arc<SessionManager>,
        session_dir: PathBuf,
        tokens: &Arc<Tokens>,
    ) -> Drm {
        let mut_source = {
            let tokens = tokens.clone();
            Arc::new(move || tokens.music_user_token())
        };
        let cfg = BackendConfig {
            base_dir: session_dir.clone(),
            disable_hi_res: std::env::var("RUSTET_DISABLE_HIRES").is_ok_and(|v| v == "1"),
            music_token: Some(mut_source.clone()),
            ..BackendConfig::default()
        };
        // A session on disk at startup: the backend starts at once (after the session lock is
        // held), so its state is visible before the first playback request.
        let eager_start = session.has_session();
        let manager = DrmManager::new(backend, session, cfg, RestartPolicy::default()).await;
        // The web player's token counts as the account: there is no separate DRM sign-in.
        let web = tokens.clone();
        manager.set_web_session(Arc::new(move || web.web_session()));
        // Lets a late web session write MUSIC_TOKEN and retry FairPlay when the backend
        // started before the page loaded.
        manager.set_music_token_source(mut_source);
        Drm {
            manager,
            backend_name: "rustetplay",
            session_dir,
            eager_start,
        }
    }

    /// Background upkeep that lives as long as the engine.
    fn spawn_background(self: &Arc<Self>) {
        // Pre-warm the TLS connection to the licence server so the first key request skips
        // the handshake.
        {
            let (streams, cancel) = (self.streams.clone(), self.shutdown.clone());
            tokio::spawn(async move { streams.warm_licence_pool(&cancel).await });
        }
        if let Some(cache) = self.disk_cache.clone() {
            let cancel = self.shutdown.clone();
            tokio::spawn(async move {
                let hour = Duration::from_secs(60 * 60);
                let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + hour, hour);
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        _ = tick.tick() => cache.evict_expired().await,
                    }
                }
            });
        }
    }

    /// Emits an SSE event to every subscriber.
    pub fn emit(&self, kind: &'static str, data: &impl Serialize) {
        self.events.emit(&self.epoch, kind, data);
    }

    /// The DRM snapshot, or the empty one without a backend.
    pub fn drm_status(&self) -> DrmSnapshot {
        self.drm
            .as_ref()
            .map(|d| d.manager.status())
            .unwrap_or_default()
    }

    /// Stores the MusicKit token. A new one tells the DRM manager, which reports the session
    /// and starts the backend if it is not running.
    pub fn set_music_user_token(&self, token: &str) {
        if self.tokens.set_music_kit(token)
            && let Some(drm) = &self.drm
        {
            drm.manager.note_web_session();
        }
        self.notify_player_caps();
    }

    /// What the player can do now: lossless once FairPlay is ready, autoplay, stations and
    /// play reports with a music user token.
    pub fn player_capabilities(&self) -> rustet_player::model::Capabilities {
        rustet_player::model::Capabilities {
            lossless_ready: self.drm_status().capabilities.alac,
            signed_in: !self.tokens.music_user_token().is_empty(),
        }
    }

    /// Sends the capabilities to the player when they changed.
    pub fn notify_player_caps(&self) {
        let Some(player) = self.player.get() else {
            return;
        };
        let caps = self.player_capabilities();
        if self
            .player_caps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(caps)
            == Some(caps)
        {
            return;
        }
        let player = player.clone();
        tokio::spawn(async move {
            let _ = player
                .send(rustet_player::model::Command::Capabilities(caps))
                .await;
        });
    }

    /// The ownership rule between the player and the `/api/v1/vlc/*` routes, which the web shell
    /// still drives: they play on separate libvlc players, and whoever starts playback last owns
    /// the sound. A player load stops the routes' player and clears its seek gate, so a late
    /// `/vlc/seek` gets 409. This is the player's side.
    pub async fn player_takes_libvlc(&self) {
        if self.player_owns_libvlc.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(vlc) = &self.vlc {
            vlc.gate.clear();
            let player = vlc.player.clone();
            let _ = tokio::task::spawn_blocking(move || player.stop()).await;
        }
    }

    /// The routes' side of [`Engine::player_takes_libvlc`]: `/vlc/load` and `/vlc/resume` pause
    /// the player when it is playing. A paused player keeps its queue and position.
    pub async fn routes_take_libvlc(&self) {
        if !self.player_owns_libvlc.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(player) = self.player.get() {
            use rustet_player::model::Status;
            if matches!(
                player.state().status,
                Status::Playing | Status::Loading | Status::Opening
            ) {
                let _ = player.send(rustet_player::model::Command::Pause).await;
            }
        }
    }

    /// The developer token: the cached one, or one scraped from the web player (then cached).
    /// The storefront for Apple requests that have none of their own, asked from Apple when
    /// the UI pushed none (see [`AppleLink::storefront`](crate::player::AppleLink::storefront)).
    pub(crate) async fn account_storefront(self: &Arc<Self>) -> String {
        self.apple().storefront().await
    }

    pub(crate) fn apple(self: &Arc<Self>) -> &Arc<crate::player::AppleLink> {
        self.apple
            .get_or_init(|| Arc::new(crate::player::AppleLink::new(self)))
    }

    pub async fn resolve_token(&self) -> anyhow::Result<String> {
        let cached = self.tokens.developer();
        if !cached.is_empty() {
            return Ok(cached);
        }
        let token = self
            .amp
            .token()
            .await
            .map_err(|e| anyhow::anyhow!("could not resolve bearer token: {e}"))?;
        self.tokens.set_developer(&token);
        Ok(token)
    }

    /// Applies a DRM snapshot: epoch, FairPlay session age, and the `drm` SSE event.
    pub fn on_drm_snapshot(self: &Arc<Self>, snap: &DrmSnapshot) {
        self.lifecycle
            .on_drm_state_changed(&self.epoch, snap.state.session.as_str());
        if snap.state.fair_play == FairPlayState::Ready {
            let me = Arc::downgrade(self);
            self.lifecycle.on_fair_play_ready(move || {
                // Fired shortly before the expected FairPlay session expiry, so the renderer can
                // pre-warm a session while the current one still works.
                let Some(me) = me.upgrade() else { return };
                let since = me
                    .lifecycle
                    .drm_ready_since()
                    .unwrap_or_else(SystemTime::now);
                me.emit(
                    "drm.refresh_due",
                    &RefreshDue {
                        ready_since_ms: unix_ms(since),
                        refresh_at_ms: unix_ms(since + DRM_SESSION_TTL - DRM_REFRESH_LEAD),
                    },
                );
            });
        }
        self.emit("drm", snap);
        self.notify_player_caps();
    }

    /// Takes the session lock: no two engines may own the single-user Apple session.
    pub fn acquire_session_lock(&self) -> anyhow::Result<Option<SessionLock>> {
        let Some(drm) = &self.drm else {
            return Ok(None);
        };
        SessionLock::acquire(&drm.session_dir)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("acquire session lock: {e}"))
    }

    /// Starts DRM work that must wait for the session lock: the snapshot watcher and, when a
    /// session exists, the eager backend start.
    pub fn start_drm(self: &Arc<Self>) {
        let Some(drm) = &self.drm else { return };
        let mut rx = drm.manager.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                let snap = tokio::select! {
                    () = me.shutdown.cancelled() => return,
                    r = rx.recv() => match r {
                        Ok(snap) => snap,
                        // Missed snapshots: the latest state is what matters.
                        Err(RecvError::Lagged(_)) => me.drm_status(),
                        Err(RecvError::Closed) => return,
                    },
                };
                me.on_drm_snapshot(&snap);
            }
        });

        if drm.eager_start {
            // The backend initialises FairPlay asynchronously, so one immediate account lookup
            // would fail with "not ready". Retry with backoff for up to 30 s.
            let manager = drm.manager.clone();
            tokio::spawn(async move {
                let budget = tokio::time::sleep(Duration::from_secs(30));
                tokio::pin!(budget);
                let mut delay = Duration::from_millis(500);
                loop {
                    let err = tokio::select! {
                        r = manager.get_account() => match r {
                            Ok(_) => return,
                            Err(e) => e,
                        },
                        () = &mut budget => {
                            warn!("DRM auto-start: backend not ready after 30s");
                            return;
                        }
                    };
                    tokio::select! {
                        () = &mut budget => {
                            warn!("DRM auto-start: backend not ready after 30s: {err}");
                            return;
                        }
                        () = tokio::time::sleep(delay) => {}
                    }
                    if delay < Duration::from_secs(5) {
                        delay *= 2;
                    }
                }
            });
        }
    }

    /// Frees a session's server-side resources however the session ends. DELETE
    /// /playback/{id} does this itself; a session that simply expires, or whose client
    /// vanished, would otherwise leave its producers running and its cache file pinned.
    pub fn install_release_hook(self: &Arc<Self>) {
        let Some(p) = &self.playback else { return };
        let me = Arc::downgrade(self);
        p.manager.set_release_hook(move |id| {
            if let Some(me) = me.upgrade() {
                me.vseg.stop(id);
            }
        });
    }

    /// Stops the background work: audio, SSE streams, exports, the scheduler and the DRM
    /// backend. The session stays on disk for the next start.
    pub async fn stop(&self) {
        // Audio stops first, before the rest of the shutdown. The player saves its snapshot
        // and sends its last play reports.
        if let Some(player) = self.player.get() {
            player.shutdown().await;
            crate::player::stop_decks(self).await;
        }
        if let Some(vlc) = &self.vlc {
            let player = vlc.player.clone();
            let _ = tokio::task::spawn_blocking(move || player.stop()).await;
        }
        self.shutdown.cancel();
        // Exports stop before the playback they stream through.
        if let Some(export) = &self.export {
            export.stop().await;
        }
        if let Some(playback) = &self.playback {
            playback.scheduler.stop().await;
        }
        if let Some(drm) = &self.drm {
            drm.manager.shutdown().await;
        }
    }
}

async fn open_library(cache_base: &Path) -> Option<Store> {
    // The directory holds library.key: private.
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    if let Err(e) = dir.create(cache_base) {
        warn!(
            "library cache disabled: create {}: {e}",
            cache_base.display()
        );
        return None;
    }
    Store::open(cache_base)
        .await
        .inspect_err(|e| warn!("library cache disabled: {e}"))
        .ok()
}

/// The DRM manager as the provider sees it: the CBCS dialer of lossless tracks and the native
/// progressive music-video lookup.
struct DrmLink(DrmManager);

#[async_trait]
impl CbcsDialer for DrmLink {
    async fn dial_cbcs(&self, cancel: &CancellationToken) -> std::io::Result<DrmStream> {
        // Starts the backend when a session exists, then returns an in-process pipe speaking
        // the FairPlay sample protocol.
        match self.0.dial_cbcs(cancel.clone()).await {
            Ok(pipe) => Ok(Box::new(pipe)),
            Err(e) => Err(std::io::Error::other(e)),
        }
    }

    fn in_process(&self) -> bool {
        self.0.in_process()
    }

    async fn refresh_keys(&self) -> std::io::Result<()> {
        self.0.refresh_keys().await.map_err(std::io::Error::other)
    }
}

#[async_trait]
impl AccountTokenSource for DrmLink {
    async fn progressive_mv_url(&self, adam_id: u64) -> Result<ProgressiveMvUrl, BoxError> {
        let u = self.0.get_progressive_mv_url(adam_id).await?;
        Ok(ProgressiveMvUrl {
            url: u.url,
            download_key: u.download_key,
        })
    }
}
