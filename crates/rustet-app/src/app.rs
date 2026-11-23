//! The application: one [`App`] that the view layer starts once and holds. It ties the parts of
//! this crate together and gives a view only what it draws and what it does.
//!
//! [`App::start`] makes the engine's TLS identity, starts and supervises the engine, opens its
//! event stream, builds the Apple Music client, loads the settings and keeps the engine's
//! player settings in step, runs the account, opens the library cache, and starts the desktop
//! integrations. [`App::stop`] stops them in order.
//!
//! # Runtime
//!
//! `App::start` must be awaited inside a tokio runtime (multi-threaded), and that runtime must
//! run until [`App::stop`] returns. The async methods of the models (`Player::send`,
//! `AccountModel::sign_in`, `Downloads::download`, [`App::act`], …) do HTTP through reqwest and
//! must be polled on that runtime: a view spawns them with [`App::runtime`] and may await the
//! `JoinHandle` on its own executor. Everything else works from any thread: the watch
//! receivers (`borrow`, and `changed()` on any executor), the broadcast streams, and the
//! [`Nav`] methods, which spawn on the runtime themselves.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustet_ampapi::{Api, Client, Locale};
use rustet_desktop::engine::{EngineLauncher, RustetEngine, SupervisorTiming};
use rustet_desktop::platform::Env;
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::Store;
use rustet_library::LibraryError;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::account::lossless::LosslessAccount;
use crate::account::{Account, AccountModel, SignInProgram, signin_profile_dir};
use crate::downloads::Downloads;
use crate::engine::client::BuildError;
use crate::engine::{Connection, Engine, EngineClient, EngineIdentity, IdentityError};
use crate::integrations::{Inputs, Integrations, Intent, Scrobblers, StoreSecrets};
use crate::library::{Library, Syncer};
use crate::nav::Nav;
use crate::pages::actions::{self, Action, ActionError, Outcome};
use crate::route::Route;
use crate::settings::{self, SettingsStore};

/// Events a slow receiver of [`App::intents`] or [`App::outcomes`] may fall behind.
const CHANNEL: usize = 64;

/// Where the app keeps its data and how it starts the engine.
#[derive(Clone)]
pub struct AppConfig {
    /// `~/.config/rustet` (after the shell's migration of older layouts).
    pub config_dir: PathBuf,
    /// The library cache's directory, `~/.cache/rustet`.
    pub cache_dir: PathBuf,
    /// The encrypted store (tokens, switches the page kept, scrobbler keys).
    pub store: Store,
    pub prefs: Arc<PrefsFile>,
    pub engine_port: u16,
    pub timing: SupervisorTiming,
    /// The engine's DRM session directory, for a sign-out the engine could not finish.
    pub drm_session_dir: PathBuf,
    /// The web sign-in window (an engine without FairPlay).
    pub sign_in: SignInProgram,
    /// The UI language as a BCP-47 tag ([`language_tag`]).
    pub language: String,
    /// Start MPRIS, the tray and the media keys (`false`: tests, a headless run).
    pub desktop: bool,
    /// Where `fetch-fairplay-libs.sh` is (beside the executable): FairPlay's libraries are
    /// fetched on first run. `None`: never.
    pub exe_dir: Option<PathBuf>,
    /// The first page.
    pub start: Route,
}

impl AppConfig {
    /// An installed Rustet: the executable's own engine role and sign-in window, the library in
    /// Rustet's cache directory, the store and prefs the binary opened in `config_dir`.
    pub fn installed(
        config_dir: &Path,
        store: Store,
        prefs: Arc<PrefsFile>,
        env: &Env,
    ) -> io::Result<Self> {
        let exe = std::env::current_exe()?;
        Ok(Self {
            config_dir: config_dir.to_owned(),
            cache_dir: rustet_core::appdir::cache_dir().to_owned(),
            store,
            prefs,
            engine_port: crate::engine::engine_port(env),
            timing: SupervisorTiming::default(),
            // `engine::launcher`'s layout: the session lives in the config directory.
            drm_session_dir: config_dir.join("drm/files"),
            sign_in: SignInProgram::current_exe()?,
            language: language_tag(env),
            desktop: true,
            exe_dir: exe.parent().map(Path::to_path_buf),
            start: Route::New,
        })
    }
}

/// The BCP-47 tag of the user's locale (`LC_ALL`, `LC_MESSAGES`, `LANG`): `en_GB.UTF-8` is
/// `en-GB`. `C`, `POSIX` or nothing is `en-US`.
pub fn language_tag(env: &Env) -> String {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|k| env.get(k))
        .find(|v| !v.is_empty())
        .unwrap_or_default();
    let tag = raw
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .replace('_', "-");
    match tag.as_str() {
        "" | "C" | "POSIX" => "en-US".to_owned(),
        _ => tag,
    }
}

/// Why the app could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("the engine launcher: {0}")]
    Launcher(#[source] io::Error),
    #[error(transparent)]
    Client(#[from] BuildError),
    #[error("the HTTP client: {0}")]
    Http(#[source] reqwest::Error),
    #[error("the library cache: {0}")]
    Library(#[from] LibraryError),
}

/// A finished library or playlist action, for a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    pub action: Action,
    pub result: Result<Outcome, ActionError>,
}

impl ActionOutcome {
    /// "Added to Library", or what failed and what to do.
    pub fn toast(&self) -> &str {
        match &self.result {
            Ok(outcome) => &outcome.toast,
            Err(e) => &e.toast,
        }
    }
}

struct Inner<L> {
    runtime: Handle,
    engine: Engine<L>,
    api: Api,
    store: Store,
    prefs: Arc<PrefsFile>,
    settings: SettingsStore,
    account: AccountModel,
    player: crate::player::Player,
    nav: Nav,
    downloads: Downloads,
    scrobblers: Arc<Scrobblers>,
    integrations: tokio::sync::Mutex<Option<Integrations>>,
    intents: broadcast::Sender<Intent>,
    outcomes: broadcast::Sender<ActionOutcome>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// The running application. Cheap to clone.
pub struct App<L = RustetEngine>(Arc<Inner<L>>);

impl<L> Clone for App<L> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl App<RustetEngine> {
    /// Starts the app with the executable's own engine (`rustet engine`). See the module
    /// documentation for the runtime it needs.
    pub async fn start(config: AppConfig, env: Env) -> Result<Self, StartError> {
        let (dir, port) = (config.config_dir.clone(), config.engine_port);
        Self::start_with(config, |identity| {
            crate::engine::launcher(&dir, port, identity, env)
        })
        .await
    }
}

impl<L: EngineLauncher> App<L> {
    /// Starts the app with the engine `launcher` builds. The launcher must hand the engine the
    /// identity's PEM on standard input.
    pub async fn start_with(
        config: AppConfig,
        launcher: impl FnOnce(&EngineIdentity) -> io::Result<L>,
    ) -> Result<Self, StartError> {
        let identity = EngineIdentity::generate()?;
        let launcher = launcher(&identity).map_err(StartError::Launcher)?;
        let client = EngineClient::new(config.engine_port, identity.cert_der())?;
        let engine = Engine::start(launcher, client.clone(), config.timing);
        // Every follower subscribes now, before the first snapshot can arrive.
        let account_events = (engine.events(), engine.events(), engine.events());
        let (player_events, download_events, settings_events, integration_events) = (
            engine.events(),
            engine.events(),
            engine.events(),
            engine.events(),
        );

        let http = reqwest::Client::builder()
            .build()
            .map_err(StartError::Http)?;
        let api = Api::new(
            Client::new(http.clone()),
            Locale::new("us", config.language.as_str()),
        );
        let settings = SettingsStore::load(config.prefs.clone(), config.store.clone());
        let library = Library::open_in(&config.cache_dir).await?;

        let syncer = Syncer::new(library, api.clone());
        let lossless = LosslessAccount::new(
            client.clone(),
            config.store.clone(),
            config.drm_session_dir.clone(),
            {
                let engine = engine.clone();
                move || engine.reload()
            },
        );
        let web = Account::load(
            config.store.clone(),
            client.clone(),
            config.sign_in.clone(),
            signin_profile_dir(&config.config_dir),
        );
        let account = AccountModel::new(
            client.clone(),
            api.clone(),
            syncer,
            config.language.clone(),
            lossless,
            web,
        );
        let mut tasks = vec![
            tokio::spawn(
                account
                    .clone()
                    .run(account_events.0, account_events.1, account_events.2),
            ),
            tokio::spawn(settings::sync_engine(
                client.clone(),
                settings.subscribe(),
                settings_events,
            )),
        ];
        #[cfg(target_os = "linux")]
        if let Some(exe_dir) = config.exe_dir.clone() {
            let (engine, config_dir) = (engine.clone(), config.config_dir.clone());
            tasks.push(tokio::spawn(async move {
                engine.fetch_fairplay_libs(&config_dir, &exe_dir).await;
            }));
        }

        let player = crate::player::Player::start(client.clone(), player_events);
        let downloads = Downloads::start(
            client.clone(),
            api.clone(),
            settings.subscribe(),
            download_events,
        );
        let nav = Nav::new(
            api.clone(),
            Handle::current(),
            config.start.clone(),
            account.session(),
        );

        let scrobblers = Arc::new(Scrobblers::new(
            http,
            Arc::new(StoreSecrets(config.store.clone())),
        ));
        let inputs = Inputs {
            client,
            events: integration_events,
            settings: settings.subscribe(),
            session: account.session(),
            desktop: config.desktop,
        };
        let (integrations, mut intent_rx) = Integrations::start(inputs, scrobblers.clone()).await;
        let intents = broadcast::channel(CHANNEL).0;
        tasks.push(tokio::spawn({
            let intents = intents.clone();
            async move {
                while let Some(intent) = intent_rx.recv().await {
                    let _ = intents.send(intent);
                }
            }
        }));
        info!("app started");
        Ok(Self(Arc::new(Inner {
            runtime: Handle::current(),
            engine,
            api,
            store: config.store,
            prefs: config.prefs,
            settings,
            account,
            player,
            nav,
            downloads,
            scrobblers,
            integrations: tokio::sync::Mutex::new(Some(integrations)),
            intents,
            outcomes: broadcast::channel(CHANNEL).0,
            tasks: Mutex::new(tasks),
        })))
    }

    /// The runtime the async methods run on.
    pub fn runtime(&self) -> &Handle {
        &self.0.runtime
    }

    /// The link to the engine (the offline view, Settings → Engine Status).
    pub fn connection(&self) -> watch::Receiver<Connection> {
        self.0.engine.connection()
    }

    /// After [`Connection::Failed`]: "Try again".
    pub async fn retry_engine(&self) {
        self.0.engine.retry().await;
    }

    /// The engine's own routes a view calls directly: metadata (the quality popover), lyrics
    /// text, cache statistics, FFmpeg check.
    pub fn engine(&self) -> &EngineClient {
        self.0.engine.client()
    }

    /// The Apple Music client: lyrics ([`crate::pages::lyrics::load`]), playlist folders
    /// ([`crate::library::playlist_folder`]), play targets.
    pub fn api(&self) -> &Api {
        &self.0.api
    }

    pub fn settings(&self) -> &SettingsStore {
        &self.0.settings
    }

    /// The preferences file, for what is kept there outside [`settings`](Self::settings):
    /// the theme (`rustet_desktop::theme::ThemeService`) and the link schemes.
    pub fn prefs(&self) -> &Arc<PrefsFile> {
        &self.0.prefs
    }

    /// Sign-in state, the sign-in sheet, the library sync.
    pub fn account(&self) -> &AccountModel {
        &self.0.account
    }

    pub fn player(&self) -> &crate::player::Player {
        &self.0.player
    }

    pub fn nav(&self) -> &Nav {
        &self.0.nav
    }

    pub fn downloads(&self) -> &Downloads {
        &self.0.downloads
    }

    /// Last.fm and ListenBrainz for Settings → Integrations (connect, status, disconnect).
    pub fn scrobblers(&self) -> &Scrobblers {
        &self.0.scrobblers
    }

    /// What the tray, MPRIS and the mini player ask the window to do (show, quit, …).
    pub fn intents(&self) -> broadcast::Receiver<Intent> {
        self.0.intents.subscribe()
    }

    /// Every finished [`App::act`], for toasts.
    pub fn outcomes(&self) -> broadcast::Receiver<ActionOutcome> {
        self.0.outcomes.subscribe()
    }

    /// Runs a library, playlist or rating action. The pages it changed reload; the outcome
    /// also goes to [`App::outcomes`].
    pub async fn act(&self, action: Action) -> Result<Outcome, ActionError> {
        let result = actions::perform(&self.0.api, &action).await;
        if result.is_ok() {
            self.0.nav.invalidate(|route| action.changes(route));
        }
        let _ = self.0.outcomes.send(ActionOutcome {
            action,
            result: result.clone(),
        });
        result
    }

    /// The tray menu says "Hide Mini Player" while it is open.
    pub async fn set_mini_player_open(&self, open: bool) {
        if let Some(i) = self.0.integrations.lock().await.as_ref() {
            i.set_mini_player_open(open).await;
        }
    }

    /// Stops everything, in order: the desktop integrations (MPRIS, tray, Discord), the
    /// app's tasks, the engine (its event stream, then the child), then the store is written.
    /// A second call does nothing more than wait for the engine.
    pub async fn stop(&self) {
        if let Some(integrations) = self.0.integrations.lock().await.take() {
            integrations.stop().await;
        }
        let tasks = std::mem::take(&mut *self.0.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for task in tasks {
            task.abort();
        }
        self.0.engine.stop().await;
        self.0.store.flush_now().await;
        info!("app stopped");
    }
}
