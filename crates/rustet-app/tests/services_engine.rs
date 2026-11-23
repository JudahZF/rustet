//! Contract: the client, the event stream and the supervisor against the real engine.
//!
//! The engine runs as a child, as in the app: this test binary started again with only the
//! ignored `engine_child` test, which calls `rustet_engine::run` with `--tls-identity-stdin`
//! semantics (the identity arrives on standard input). A child is needed because the engine
//! reads its directories from the process environment (`XDG_*`, `HOME`), which the child gets
//! pointed at temporary directories; the user's `~/.config/rustet` and `~/.cache/rustet` are never
//! touched. No Apple account and no network are needed: nothing is played.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustet_app::account::{AccountState, SignInProgram};
use rustet_app::engine::types::{LyricsFormat, LyricsKind, TokenPush};
use rustet_app::engine::{
    ClientError, Connection, Engine, EngineClient, EngineEvent, EngineIdentity,
};
use rustet_app::nav::PageState;
use rustet_app::route::Route;
use rustet_app::settings::full_patch;
use rustet_app::{App, AppConfig};
use rustet_desktop::engine::{EngineError, EngineLauncher, SupervisorTiming, ensure_engine_config};
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::{Store, StoreCipher};
use rustet_player::model::{
    Command, Event as PlayerEvent, Mode, PlayerSettings, Repeat, Seek, Status, Volume,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const CHILD_PORT: &str = "RUSTET_APP_CONTRACT_ENGINE_PORT";

/// The engine role, run only as the contract test's child.
#[test]
#[ignore = "the engine child of `the_client_drives_the_real_engine`"]
fn engine_child() {
    let Some(port) = std::env::var(CHILD_PORT).ok().and_then(|p| p.parse().ok()) else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(rustet_engine::run(rustet_engine::Options {
        port,
        tls: rustet_engine::TlsSource::Stdin,
    }))
    .unwrap();
}

struct ChildEngine {
    port: u16,
    pem: String,
    root: PathBuf,
}

impl ChildEngine {
    fn config(&self) -> PathBuf {
        self.root.join("config")
    }
}

impl EngineLauncher for ChildEngine {
    async fn prepare(&self, _quit: &CancellationToken) -> Result<(), EngineError> {
        let data = self.config().join("rustet/engine-data");
        ensure_engine_config(&data, &self.root.join("drm/files")).map_err(EngineError::Config)
    }

    fn command(&self) -> tokio::process::Command {
        let dir = |p: &str| self.root.join(p);
        let mut cmd = tokio::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "engine_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(self.config().join("rustet/engine-data"))
        .env(CHILD_PORT, self.port.to_string())
        .env("HOME", dir("home"))
        .env("XDG_CONFIG_HOME", self.config())
        .env("XDG_CACHE_HOME", dir("cache"))
        .env("XDG_DATA_HOME", dir("data"))
        .env("TMPDIR", dir("tmp"))
        .env("RUSTET_EXPORT_ROOTS", dir("home"));
        cmd
    }

    fn stdin(&self) -> Option<Vec<u8>> {
        Some(self.pem.clone().into_bytes())
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn next_state(
    events: &mut broadcast::Receiver<EngineEvent>,
    f: impl Fn(&rustet_player::model::PlayerState) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(EngineEvent::Player(PlayerEvent::State(s))) = events.recv().await
                && f(&s)
            {
                return;
            }
        }
    })
    .await
    .expect("a player.state event");
}

async fn ready(engine: &Engine<ChildEngine>) {
    let mut c = engine.connection();
    tokio::time::timeout(
        Duration::from_secs(60),
        c.wait_for(|c| *c == Connection::Ready),
    )
    .await
    .expect("the engine came up")
    .unwrap();
}

fn mkdirs(root: &Path) {
    for d in [
        "home",
        "cache",
        "data",
        "tmp",
        "config/rustet/engine-data",
        "drm/files",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
}

/// The real engine over the temporary directory `root`, and a client that trusts it.
fn start_engine(root: &Path) -> (Engine<ChildEngine>, EngineClient) {
    mkdirs(root);
    let identity = EngineIdentity::generate().unwrap();
    let port = free_port();
    let launcher = ChildEngine {
        port,
        pem: identity.pem().to_owned(),
        root: root.to_owned(),
    };
    let client = EngineClient::new(port, identity.cert_der()).unwrap();
    let timing = SupervisorTiming {
        restart_delay: Duration::from_millis(200),
        kill_delay: Duration::from_secs(5),
    };
    (Engine::start(launcher, client.clone(), timing), client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_drives_the_real_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let (engine, client) = start_engine(tmp.path());
    let mut events = engine.events();
    ready(&engine).await;

    // The first frame is the snapshot, with the player in it.
    let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    let EngineEvent::Snapshot(snapshot) = first else {
        panic!("{first:?}")
    };
    assert!(
        snapshot.player.is_some(),
        "the snapshot carries the player state"
    );

    let state = client.player_state().await.unwrap();
    assert_eq!(state.status, Status::Idle);

    // Commands: accepted, refused as stale, refused as invalid.
    let repeat = if state.repeat == Repeat::All {
        Repeat::One
    } else {
        Repeat::All
    };
    client
        .command(&Command::Mode(Mode {
            repeat: Some(repeat),
            ..Mode::default()
        }))
        .await
        .unwrap();
    next_state(&mut events, |s| s.repeat == repeat).await;
    let stale = Command::Seek(Seek {
        position_ms: 1,
        expect_generation: Some(state.generation + 99),
    });
    assert!(matches!(
        client.command(&stale).await,
        Err(ClientError::StaleGeneration)
    ));
    let loud = Command::Volume(Volume {
        level: 101,
        muted: None,
    });
    assert!(matches!(
        client.command(&loud).await,
        Err(ClientError::BadRequest(_))
    ));
    let settings = PlayerSettings {
        mv_max_height: 1080,
        history_enabled: false,
        ..PlayerSettings::default()
    };
    client
        .command(&Command::Settings(full_patch(&settings)))
        .await
        .unwrap();
    assert_eq!(
        client.player_state().await.unwrap().settings.mv_max_height,
        1080
    );
    assert!(client.history().await.unwrap().is_empty());
    client.clear_history().await.unwrap();

    // The web token: pushed, reported (never returned), dropped.
    assert!(!client.auth().await.unwrap().web_token);
    let push = TokenPush {
        music_user_token: "contract-test-not-a-token",
        developer_token: "",
        storefront: "gb",
        language: "en-GB",
    };
    client.push_token(&push).await.unwrap();
    let auth = client.auth().await.unwrap();
    assert!(auth.web_token);
    assert_eq!(auth.storefront, "gb");
    // The session route hands out the engine's token (here the pushed one), 404 without one.
    let session = client.session().await.unwrap().expect("a session");
    assert_eq!(session.music_user_token, "contract-test-not-a-token");
    assert!(
        !format!("{session:?}").contains("contract-test"),
        "Debug never shows the token"
    );
    client.delete_token().await.unwrap();
    assert!(!client.auth().await.unwrap().web_token);
    assert!(client.session().await.unwrap().is_none());

    // Status routes decode.
    let drm = client.drm_status().await.unwrap();
    assert!(!drm.signed_in());
    client.capabilities().await.unwrap();
    client.ping().await.unwrap();
    assert!(matches!(
        client.export_job("nope").await,
        Err(ClientError::NotFound(_) | ClientError::Unavailable(_))
    ));

    // A restart: the link goes down, comes back, and the new engine sends a new snapshot.
    engine.reload();
    let mut conn = engine.connection();
    tokio::time::timeout(
        Duration::from_secs(20),
        conn.wait_for(|c| matches!(c, Connection::Restarting { .. })),
    )
    .await
    .expect("restarting")
    .unwrap();
    ready(&engine).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(events.recv().await, Ok(EngineEvent::Snapshot(_))) {}
    })
    .await
    .expect("a snapshot from the new engine");
    client.player_state().await.unwrap();

    engine.stop().await;
    assert_eq!(*engine.connection().borrow(), Connection::Stopped);
}

/// The whole app against the real engine, signed out: it reaches a ready state with the player
/// state present, and stops in order. Nothing is fetched from Apple (the first page is local).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_starts_signed_out_against_the_real_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_owned();
    mkdirs(&root);
    let config_dir = root.join("config/rustet");
    let config = AppConfig {
        store: Store::open(&config_dir, StoreCipher::Plain).await,
        prefs: std::sync::Arc::new(PrefsFile::new(&config_dir)),
        config_dir,
        cache_dir: root.join("cache/rustet"),
        engine_port: free_port(),
        timing: SupervisorTiming {
            restart_delay: Duration::from_millis(200),
            kill_delay: Duration::from_secs(5),
        },
        drm_session_dir: root.join("drm/files"),
        sign_in: SignInProgram {
            program: "false".into(),
            args: Vec::new(),
        },
        language: "en-US".into(),
        desktop: false,
        exe_dir: None,
        start: Route::Downloads,
    };
    let port = config.engine_port;
    let app = App::start_with(config, |identity| {
        Ok(ChildEngine {
            port,
            pem: identity.pem().to_owned(),
            root: root.clone(),
        })
    })
    .await
    .unwrap();
    let mut connection = app.connection();
    tokio::time::timeout(
        Duration::from_secs(60),
        connection.wait_for(|c| *c == Connection::Ready),
    )
    .await
    .expect("the engine came up")
    .unwrap();
    let mut account = app.account().state();
    tokio::time::timeout(
        Duration::from_secs(10),
        account.wait_for(|s| *s == AccountState::SignedOut),
    )
    .await
    .expect("signed out")
    .unwrap();
    assert!(
        app.account().backend().borrow().is_some(),
        "the engine said which sign-in it has"
    );
    let mut player = app.player().state();
    let state = tokio::time::timeout(Duration::from_secs(10), player.wait_for(Option::is_some))
        .await
        .expect("the player state")
        .unwrap()
        .clone()
        .unwrap();
    assert_eq!(state.status, Status::Idle);
    assert_eq!(app.nav().state().borrow().page, PageState::Local);
    assert!(!app.api().is_signed_in());
    app.stop().await;
    assert_eq!(*app.connection().borrow(), Connection::Stopped);
}

/// Needs the user's web music user token (never in the repository) and the network:
///
/// ```text
/// RUSTET_TEST_MUSIC_USER_TOKEN=… RUSTET_TEST_STOREFRONT=us RUSTET_TEST_LYRICS_SONG_ID=<a song with lyrics> \
///   cargo test -p rustet-app --test services_engine -- --ignored web_token_reaches_apple_through_the_engine
/// ```
///
/// The token goes to the engine as the UI pushes it; the engine fetches the song's lyrics from
/// Apple with it (lyrics need the user token). Nothing is played.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs RUSTET_TEST_MUSIC_USER_TOKEN, RUSTET_TEST_STOREFRONT, RUSTET_TEST_LYRICS_SONG_ID and the network"]
async fn web_token_reaches_apple_through_the_engine() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let (token, storefront, song) = (
        var("RUSTET_TEST_MUSIC_USER_TOKEN"),
        var("RUSTET_TEST_STOREFRONT"),
        var("RUSTET_TEST_LYRICS_SONG_ID"),
    );
    let tmp = tempfile::tempdir().unwrap();
    let (engine, client) = start_engine(tmp.path());
    ready(&engine).await;
    let push = TokenPush {
        music_user_token: &token,
        developer_token: "",
        storefront: &storefront,
        language: "",
    };
    client.push_token(&push).await.unwrap();
    assert!(client.auth().await.unwrap().web_token);
    let lyrics = client
        .lyrics(&song, &storefront, LyricsFormat::Lrc, LyricsKind::Lines)
        .await
        .expect("Apple answered the lyrics request with the token");
    assert!(!lyrics.trim().is_empty());
    client.delete_token().await.unwrap();
    engine.stop().await;
}
