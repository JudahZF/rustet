//! The services a view uses, against fakes: the engine client and its event stream against a
//! fake HTTPS engine (wiremock behind a TLS terminator with the UI-made identity), the
//! supervisor with fake children, both account state machines, settings over old files, and
//! the integrations' mappings and scrobbling.

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustet_app::account::lossless::{CodeWait, Sheet, SheetOutcome, SheetView, normalize_code};
use rustet_app::account::{Account, SignInProgram, WebAccount, WebSignInError};
use rustet_app::engine::events::{self, EngineEvent};
use rustet_app::engine::types::DrmStatus;
use rustet_app::engine::{ClientError, Connection, Engine, EngineClient, EngineIdentity};
use rustet_app::integrations::{self, Inputs, Integrations, Intent, Routed, Scrobblers};
use rustet_app::settings::{Key, SettingsError, SettingsStore, ShortcutAction};
use rustet_desktop::engine::{EngineError, EngineLauncher, SupervisorTiming};
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::{Store, StoreCipher};
use rustet_integrations::SecretStore;
use rustet_integrations::command::{Action, LoopStatus, PageCommand, Transport};
use rustet_integrations::lastfm::Lastfm;
use rustet_integrations::listenbrainz::ListenBrainz;
use rustet_integrations::now_playing::PlaybackStatus;
use rustet_player::model::{
    CatalogId, Command, CrossfadeMode, Event as PlayerEvent, ItemKind, ItemSource, Mode,
    PlayerState, QueueItem, Repeat, ScrobbleDue, Seek, SetQueue, Status, StreamingQuality,
    TrackMeta, TrackStarted, VideoMode, Volume,
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod fake_engine;
use fake_engine::{Bodies, FakeEngine, free_port, sse, wait_until};

// ── Client ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn client_sends_the_route_shapes_and_maps_errors() {
    let e = FakeEngine::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/player/queue"))
        .and(body_json(
            json!({"items": [], "startIndex": 0, "context": null, "shuffle": true,
            "startMs": null, "play": true}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&e.mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/player/video"))
        .and(body_json(json!({"audioOnly": true})))
        .respond_with(ResponseTemplate::new(202))
        .mount(&e.mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/player/play"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&e.mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/player/seek"))
        .and(body_json(
            json!({"positionMs": 5000, "expectGeneration": 3}),
        ))
        .respond_with(ResponseTemplate::new(409).set_body_string("stale generation (current 4)\n"))
        .mount(&e.mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/drm/challenge"))
        .respond_with(
            ResponseTemplate::new(409).set_body_string("no authentication challenge pending\n"),
        )
        .mount(&e.mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/drm/authenticate"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "120")
                .set_body_string("Sign-in is paused for 2 more minutes\n"),
        )
        .mount(&e.mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/export"))
        .respond_with(ResponseTemplate::new(503).set_body_string("export not available\n"))
        .mount(&e.mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/player"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "generation": 7, "status": "paused", "track": null, "positionMs": 1, "durationMs": 2,
            "positionAt": 3, "quality": null, "volume": 80, "muted": false, "repeat": "all",
            "shuffle": false, "autoplay": true, "queue": {"items": [], "currentIndex": null},
            "error": null, "video": null, "settings": {}, "capabilities": {},
            "canPrevious": false, "canNext": true, "restored": false,
        })))
        .mount(&e.mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/metadata/1440857781"))
        .and(wiremock::matchers::query_param("sf", "gb"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1440857781", "type": "song", "title": "T", "artistName": "A", "durationMs": 1,
            "artworkUrl": "u", "hasLyrics": true,
            "availableStreams": [{"codec": "ALAC", "sampleRate": 96000, "bitDepth": 24}, {"codec": "AAC", "bitrate": 256}],
        })))
        .mount(&e.mock)
        .await;

    let c = e.client();
    let queue = SetQueue {
        items: vec![],
        start_index: 0,
        context: None,
        shuffle: Some(true),
        start_ms: None,
        play: true,
    };
    c.command(&Command::SetQueue(queue)).await.unwrap();
    c.command(&Command::Play).await.unwrap();
    c.set_video_mode(VideoMode { audio_only: true })
        .await
        .unwrap();
    let seek = Command::Seek(Seek {
        position_ms: 5000,
        expect_generation: Some(3),
    });
    assert!(matches!(
        c.command(&seek).await,
        Err(ClientError::StaleGeneration)
    ));
    assert!(matches!(
        c.drm_reply("123456").await,
        Err(ClientError::Conflict(m)) if m == "no authentication challenge pending"
    ));
    assert!(matches!(
        c.drm_sign_in("a@b.c", "pw").await,
        Err(ClientError::SignInPaused { retry_after: Some(d), .. }) if d == Duration::from_secs(120)
    ));
    assert!(matches!(
        c.exports().await,
        Err(ClientError::Unavailable(_))
    ));
    let state = c.player_state().await.unwrap();
    assert_eq!(
        (state.generation, state.status, state.repeat),
        (7, Status::Paused, Repeat::All)
    );
    let meta = c.metadata("1440857781", "gb").await.unwrap();
    assert!(meta.has_lyrics);
    assert_eq!(meta.available_streams.unwrap()[0].sample_rate, 96000);
    assert!(matches!(
        c.command(&Command::Capabilities(Default::default())).await,
        Err(ClientError::NoRoute)
    ));
}

#[tokio::test]
async fn the_client_trusts_only_its_own_engine() {
    let e = FakeEngine::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .mount(&e.mock)
        .await;
    e.client().ping().await.unwrap();
    // Another engine's certificate (another Rustet on the port): refused at the handshake.
    let other = EngineIdentity::generate().unwrap();
    let stranger = EngineClient::new(e.port, other.cert_der()).unwrap();
    assert!(matches!(
        stranger.ping().await,
        Err(ClientError::Unreachable(_))
    ));
    let nobody = EngineClient::new(free_port(), other.cert_der()).unwrap();
    assert!(matches!(
        nobody.ping().await,
        Err(ClientError::Unreachable(_))
    ));
}

// ── Event stream ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_event_stream_reconnects_with_last_event_id() {
    let e = FakeEngine::start().await;
    let snapshot = json!({"version": 1, "snapshot": {"capabilities": {"lossless": true}}});
    let position =
        json!({"generation": 1, "uid": 9, "positionMs": 1000, "durationMs": 2000, "positionAt": 5});
    Mock::given(method("GET"))
        .and(path("/api/v1/events"))
        .respond_with(Bodies {
            bodies: vec![
                sse(1, "engine.snapshot", snapshot.clone())
                    + &sse(2, "ping", json!({"ts": 1}))
                    + &sse(3, "player.position", position),
                sse(10, "engine.snapshot", snapshot) + &sse(11, "replay.truncated", json!({})),
            ],
            n: AtomicUsize::new(0),
        })
        .mount(&e.mock)
        .await;
    let cancel = CancellationToken::new();
    let (tx, mut link) = events::spawn(e.client(), cancel.clone());
    let mut rx = tx.subscribe();
    let mut got = Vec::new();
    while got.len() < 4 {
        got.push(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(matches!(&got[0], EngineEvent::Snapshot(s) if s.capabilities.lossless));
    assert!(matches!(&got[1], EngineEvent::Player(PlayerEvent::Position(p)) if p.uid == 9));
    assert!(matches!(&got[2], EngineEvent::Snapshot(_)));
    assert_eq!(got[3], EngineEvent::ReplayTruncated);
    let requests = e.requests("/api/v1/events").await;
    assert!(
        requests[0].headers.get("last-event-id").is_none(),
        "a first connect asks for no replay"
    );
    assert_eq!(
        requests[1]
            .headers
            .get("last-event-id")
            .map(|v| v.to_str().unwrap()),
        Some("3"),
        "the reconnect names the last event seen, heartbeats included"
    );
    // The link drops between connections and is up while one is open.
    link.wait_for(|open| !open).await.unwrap();
    cancel.cancel();
}

// ── Supervisor ───────────────────────────────────────────────────────────────

/// A fake engine child: `sh -c 'exit 1'`, or a preparation that fails.
struct FakeChild {
    prepare_fails: bool,
}

impl EngineLauncher for FakeChild {
    async fn prepare(&self, _quit: &CancellationToken) -> Result<(), EngineError> {
        if self.prepare_fails {
            Err(EngineError::PortInUse(20025))
        } else {
            Ok(())
        }
    }

    fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "exit 1"]);
        cmd
    }
}

async fn until(
    rx: &mut watch::Receiver<Connection>,
    f: impl Fn(&Connection) -> bool,
) -> Connection {
    tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|c| f(c)))
        .await
        .expect("connection state")
        .unwrap()
        .clone()
}

#[tokio::test]
async fn the_supervisor_restarts_a_crashed_engine_and_reports_a_failed_start() {
    let timing = SupervisorTiming {
        restart_delay: Duration::from_millis(50),
        kill_delay: Duration::from_millis(200),
    };
    let cert = EngineIdentity::generate().unwrap();
    let client = || EngineClient::new(free_port(), cert.cert_der()).unwrap();

    let engine = Engine::start(
        FakeChild {
            prepare_fails: false,
        },
        client(),
        timing,
    );
    let mut conn = engine.connection();
    // A crash after a short run: restarted after twice the base delay.
    let c = until(&mut conn, |c| {
        matches!(c, Connection::Restarting { retry_in: Some(_) })
    })
    .await;
    assert_eq!(
        c,
        Connection::Restarting {
            retry_in: Some(Duration::from_millis(100))
        }
    );
    engine.stop().await;
    assert_eq!(*engine.connection().borrow(), Connection::Stopped);

    let engine = Engine::start(
        FakeChild {
            prepare_fails: true,
        },
        client(),
        timing,
    );
    let mut conn = engine.connection();
    let Connection::Failed { reason } =
        until(&mut conn, |c| matches!(c, Connection::Failed { .. })).await
    else {
        unreachable!()
    };
    assert!(reason.contains("Close the other player"), "{reason}");
    engine.stop().await;
}

// ── Web account ──────────────────────────────────────────────────────────────

/// `sh -c SCRIPT sh <profile> [storefront]`: logs its arguments, prints `line` (sign-in) or
/// nothing (`--sign-out`).
fn signin_program(log: &std::path::Path, line: &str) -> SignInProgram {
    let script = format!(
        "echo \"$@\" >> '{}'; [ \"$1\" = --sign-out ] && exit 0; echo '{line}'",
        log.display()
    );
    SignInProgram {
        program: "sh".into(),
        args: vec!["-c".into(), script.into(), "sh".into()],
    }
}

#[tokio::test]
async fn web_sign_in_stores_and_pushes_the_token_and_sign_out_removes_it() {
    let e = FakeEngine::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/library/token"))
        .and(body_json(json!({"musicUserToken": "tok-123", "developerToken": "", "storefront": "gb", "language": ""})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&e.mock)
        .await;
    for p in ["/api/v1/auth/token", "/api/v1/player/history"] {
        Mock::given(method("DELETE"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(204))
            .mount(&e.mock)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("args");
    let store = Store::open(dir.path(), StoreCipher::Plain).await;
    let profile = dir.path().join("web-signin");
    let line =
        r#"{"result":"token","musicUserToken":"tok-123","storefront":"gb","source":"cookie"}"#;
    let account = Account::load(
        store.clone(),
        e.client(),
        signin_program(&log, line),
        profile.clone(),
    );
    assert_eq!(*account.state().borrow(), WebAccount::SignedOut);

    let session = account.sign_in().await.unwrap();
    assert_eq!(
        (session.token.as_str(), session.storefront.as_str()),
        ("tok-123", "gb")
    );
    assert_eq!(
        *account.state().borrow(),
        WebAccount::SignedIn {
            storefront: "gb".into()
        }
    );
    assert!(
        !format!("{session:?}").contains("tok-123"),
        "Debug never shows the token"
    );
    assert_eq!(e.requests("/api/v1/library/token").await.len(), 1);
    // Kept across restarts.
    let again = Account::load(
        store.clone(),
        e.client(),
        signin_program(&log, line),
        profile.clone(),
    );
    assert_eq!(
        again
            .session()
            .borrow()
            .as_ref()
            .map(|s| s.token.as_str().to_owned()),
        Some("tok-123".into())
    );

    // auth.expired marks it expired; the token stays until a new sign-in.
    let (tx, rx) = broadcast::channel(8);
    let run = tokio::spawn(account.clone().run(rx));
    tx.send(EngineEvent::Player(PlayerEvent::AuthExpired))
        .unwrap();
    let mut state = account.state();
    tokio::time::timeout(
        Duration::from_secs(2),
        state.wait_for(|s| matches!(s, WebAccount::Expired { .. })),
    )
    .await
    .unwrap()
    .unwrap();
    drop(tx);
    run.await.unwrap();

    account.sign_out().await;
    assert_eq!(*account.state().borrow(), WebAccount::SignedOut);
    assert!(account.session().borrow().is_none());
    assert_eq!(store.get("web_music_user_token"), None);
    assert_eq!(e.requests("/api/v1/auth/token").await.len(), 1);
    assert_eq!(e.requests("/api/v1/player/history").await.len(), 1);
    let args = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let p = profile.display();
    // The first sign-in knows no storefront yet: the window starts on its default.
    assert_eq!(
        lines,
        [p.to_string().as_str(), format!("--sign-out {p}").as_str()]
    );
}

#[tokio::test]
async fn a_closed_or_failed_sign_in_window_leaves_the_account_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreCipher::Plain).await;
    let client =
        EngineClient::new(free_port(), EngineIdentity::generate().unwrap().cert_der()).unwrap();
    let log = dir.path().join("args");
    let program = |line: &str| signin_program(&log, line);
    let account = Account::load(
        store.clone(),
        client.clone(),
        program(r#"{"result":"cancelled","reason":"windowClosed"}"#),
        dir.path().join("p"),
    );
    assert!(matches!(
        account.sign_in().await,
        Err(WebSignInError::Cancelled)
    ));
    assert_eq!(*account.state().borrow(), WebAccount::SignedOut);
    let account = Account::load(
        store.clone(),
        client.clone(),
        program(r#"{"result":"error","message":"no display"}"#),
        dir.path().join("p"),
    );
    assert!(matches!(account.sign_in().await, Err(WebSignInError::Window(m)) if m == "no display"));
    let account = Account::load(store, client, program("garbage"), dir.path().join("p"));
    assert!(matches!(
        account.sign_in().await,
        Err(WebSignInError::NoAnswer)
    ));
}

// ── Lossless sheet ───────────────────────────────────────────────────────────

fn snap(v: Value) -> DrmStatus {
    serde_json::from_value(v).unwrap()
}

fn two_factor(id: u64) -> DrmStatus {
    snap(
        json!({"state": {"authentication": "challenging"}, "challenge": {"type": "two_factor", "id": id}}),
    )
}

#[test]
fn verification_codes_keep_six_digits() {
    assert_eq!(normalize_code("123 456"), "123456");
    assert_eq!(normalize_code("Your code is 987-654."), "987654");
    assert_eq!(normalize_code("1234567890"), "123456");
    assert_eq!(normalize_code(""), "");
}

#[test]
fn the_sheet_moves_from_sign_in_to_code_and_closes_when_fairplay_is_ready() {
    let mut s = Sheet::default();
    assert_eq!(s.on_snapshot(&two_factor(1)), None);
    assert!(
        matches!(s.view(), SheetView::Code { .. }),
        "a challenge opens the sheet by itself"
    );
    let mut s = Sheet::default();
    s.open("me@example.com");
    assert!(matches!(
        s.view(),
        SheetView::SignIn {
            known_account: true,
            ..
        }
    ));
    s.sign_in_started("me@example.com");
    s.on_snapshot(&two_factor(1));
    assert!(matches!(s.view(), SheetView::Code { busy: None, .. }));
    // Neither a credentials challenge nor a web session alone opens anything.
    let mut closed = Sheet::default();
    closed.on_snapshot(&snap(
        json!({"state": {"authentication": "challenging"}, "challenge": {"type": "credentials"}}),
    ));
    closed.on_snapshot(&snap(json!({"state": {"session": "valid"}})));
    assert!(!closed.is_open());
    assert_eq!(
        s.on_snapshot(&snap(json!({"capabilities": {"cbcs": true}}))),
        Some(SheetOutcome::SignedIn)
    );
    assert!(!s.is_open());
    assert_eq!(
        s.on_snapshot(&snap(json!({"state": {"fairplay": "ready"}}))),
        None,
        "closed: nothing to do"
    );
}

#[test]
fn the_sheet_tells_a_wrong_code_from_a_repeated_challenge() {
    let mut s = Sheet::default();
    s.on_snapshot(&two_factor(1));
    s.code_started(CodeWait::Verifying);
    s.on_snapshot(&two_factor(1));
    assert!(
        matches!(
            s.view(),
            SheetView::Code {
                busy: Some(CodeWait::Verifying),
                ..
            }
        ),
        "same challenge repeated"
    );
    s.on_snapshot(&two_factor(2));
    let SheetView::Code {
        wrong_code: true,
        error: Some(e),
        busy: None,
        ..
    } = s.view()
    else {
        panic!("{:?}", s.view())
    };
    assert!(e.message.starts_with("Incorrect code"));
    // Nothing submitted, a new challenge after a special reply: fresh entry, no error.
    s.code_started(CodeWait::Resending);
    s.on_snapshot(&two_factor(3));
    assert_eq!(
        s.view(),
        &SheetView::Code {
            busy: None,
            error: None,
            wrong_code: false,
            phone_options: false
        }
    );
    s.toggle_phone_options();
    assert!(matches!(
        s.view(),
        SheetView::Code {
            phone_options: true,
            ..
        }
    ));
}

#[test]
fn a_cancelled_challenge_is_not_reopened_but_a_new_one_is() {
    let mut s = Sheet::default();
    s.on_snapshot(&two_factor(3));
    assert!(s.cancel(), "the engine's sign-in must be stopped");
    s.on_snapshot(&two_factor(3));
    assert!(!s.is_open());
    s.on_snapshot(&two_factor(4));
    assert!(s.is_open());
    // Unnumbered challenges (id 0) never match a remembered one.
    let mut s = Sheet::default();
    s.on_snapshot(&two_factor(0));
    s.cancel();
    s.on_snapshot(&two_factor(0));
    assert!(s.is_open());
    // Cancelling the plain form stops nothing.
    let mut s = Sheet::default();
    s.open("");
    assert!(!s.cancel());
}

#[test]
fn failures_go_back_to_the_form_with_what_to_do() {
    let failed = snap(
        json!({"state": {"authentication": "failed"}, "message": "timed out",
        "failure": {"kind": "locked", "message": "Apple has blocked sign-ins for now.", "url": "https://iforgot.apple.com"}}),
    );
    let mut s = Sheet::default();
    s.on_snapshot(&two_factor(1));
    s.on_snapshot(&failed);
    let SheetView::SignIn {
        error: Some(e),
        busy: false,
        ..
    } = s.view()
    else {
        panic!()
    };
    assert_eq!(
        (e.message.as_str(), e.help_url.as_deref()),
        (
            "Apple has blocked sign-ins for now.",
            Some("https://iforgot.apple.com")
        )
    );

    // On the form: only a failure of the attempt in progress is shown.
    let plain = snap(json!({"state": {"authentication": "failed"}}));
    let mut s = Sheet::default();
    s.open("");
    s.on_snapshot(&plain);
    assert!(
        matches!(s.view(), SheetView::SignIn { error: None, .. }),
        "a stale failure"
    );
    s.sign_in_started("a@b.c");
    s.on_snapshot(&plain);
    let SheetView::SignIn { error: Some(e), .. } = s.view() else {
        panic!()
    };
    assert_eq!(
        e.message,
        "Could not sign in. Check your Apple Account and password."
    );
    // A cancelled sign-in (logged out, no failure) never reopens the sheet.
    let mut s = Sheet::default();
    s.on_snapshot(&snap(
        json!({"state": {"authentication": "logged_out"}, "message": "sign-in cancelled"}),
    ));
    assert!(!s.is_open());
}

#[test]
fn a_dialog_choice_lists_apples_options() {
    let mut s = Sheet::default();
    s.on_snapshot(&snap(json!({"state": {"authentication": "challenging"},
        "challenge": {"type": "dialog_choice", "id": 5, "title": "Verify", "metadata": {"buttons": "Text Message\nPhone Call\n"}}})));
    assert_eq!(
        s.view(),
        &SheetView::Choice {
            title: "Verify".into(),
            options: vec!["Text Message".into(), "Phone Call".into()],
            busy: false
        }
    );
    s.choice_started();
    // Apple then asks for the code it sent: the code page, not "wrong code".
    s.on_snapshot(&two_factor(6));
    assert!(matches!(
        s.view(),
        SheetView::Code {
            wrong_code: false,
            ..
        }
    ));
}

// ── Settings ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn settings_read_the_web_shells_keys_and_write_them_back_under_the_same_names() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("electron-prefs.json"),
        json!({
            "streaming-quality": "hi-res-lossless", "lossless-enabled": false,
            "crossfade-sec": 4, "xf-station": false, "dl-artwork-size": "3000", "dl-mv-quality": "720p",
            "retry-timeout": "60", "persistLimitMB": 800, "hideRadio": true, "artTheme": false,
            "explicit-enabled": false, "adm-enabled": true, "adm-marker": "{M}",
            "zoomFactor": 1.25, "themeMode": "accent", "somethingElse": {"kept": 1},
        })
        .to_string(),
    )
    .unwrap();
    let store = Store::open(dir.path(), StoreCipher::Plain).await;
    store.update(|m| {
        m.insert("resumeEnabled".into(), json!("false"));
        m.insert("soundCheckEnabled".into(), json!("true"));
        m.insert("discordEnabled".into(), json!(true));
        m.insert(
            "hotkeyBindings".into(),
            json!(r#"{"playPause":"k","bogus":"x","love":""}"#),
        );
    });
    let prefs = Arc::new(PrefsFile::new(dir.path()));
    let settings = SettingsStore::load(prefs.clone(), store.clone());
    let s = settings.get();
    let p = &s.playback;
    assert_eq!(p.quality, StreamingQuality::HiResLossless);
    assert!(!p.lossless_enabled);
    assert_eq!(
        s.player().quality,
        StreamingQuality::HighQuality,
        "lossless off forces AAC"
    );
    assert_eq!(
        (p.crossfade.mode, p.crossfade.manual_seconds),
        (CrossfadeMode::Manual, 4),
        "a legacy length means manual"
    );
    assert!(!p.crossfade.stations && p.crossfade.keep_albums_gapless);
    assert!(p.sound_check && !p.resume_on_launch && p.report_plays);
    assert_eq!(p.mv_max_height, 1080, "the default when none was chosen");
    let d = &s.downloads;
    assert_eq!(
        (
            d.artwork_size.pixels(),
            d.mv_quality.max_height(),
            d.retry_after_secs
        ),
        (3000, 720, 60)
    );
    assert_eq!(
        (
            d.explicit_marker.choice(),
            d.clean_marker.choice(),
            d.master_marker.choice()
        ),
        ("", "", "{M}")
    );
    assert_eq!(d.track_template, "{track_number:02d} - {title}");
    assert_eq!(s.storage.song_cache_limit_mb, Some(800));
    assert!(s.appearance.hide_radio && !s.appearance.album_art_colours);
    assert_eq!(s.appearance.zoom, 1.25);
    assert!(s.history.show_in_up_next && s.integrations.discord);
    assert_eq!(
        s.shortcuts.bindings[&ShortcutAction::PlayPause],
        Key("k".into())
    );
    assert_eq!(
        s.shortcuts.bindings[&ShortcutAction::Love],
        Key("l".into()),
        "an empty binding keeps the default"
    );

    let watcher = settings.subscribe();
    settings
        .update(|s| {
            s.playback.lossless_enabled = true;
            s.integrations.discord = false;
            s.playback.mv_max_height = 720;
        })
        .unwrap();
    assert!(watcher.has_changed().unwrap());
    store.flush_now().await;
    let file: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("electron-prefs.json")).unwrap())
            .unwrap();
    assert_eq!(file["lossless-enabled"], true);
    assert_eq!(file["rustet-mv-quality"], 720);
    assert_eq!(
        file["somethingElse"],
        json!({"kept": 1}),
        "unknown keys survive"
    );
    assert_eq!(file["themeMode"], "accent");
    assert!(
        file.get("dl-quality").is_none(),
        "unchanged settings are not written"
    );
    assert_eq!(
        store.get("discordEnabled"),
        Some(json!("false")),
        "store switches stay strings"
    );
    // A reload reads the same.
    assert_eq!(SettingsStore::load(prefs, store).get(), settings.get());

    assert!(matches!(
        settings.update(|s| s.playback.mv_max_height = 999),
        Err(SettingsError::MvHeight(999))
    ));
    assert!(matches!(
        settings.update(|s| {
            s.shortcuts
                .bindings
                .insert(ShortcutAction::Love, Key("K".into()));
        }),
        Err(SettingsError::DuplicateKey)
    ));
    assert_eq!(
        settings.get().playback.mv_max_height,
        720,
        "a refused change changes nothing"
    );
}

// ── Integrations ─────────────────────────────────────────────────────────────

fn item(uid: u64) -> QueueItem {
    QueueItem {
        uid,
        id: CatalogId("1440857781".into()),
        library_id: None,
        kind: ItemKind::Song,
        source: ItemSource::Context,
        meta: TrackMeta {
            name: "Song Name!".into(),
            artist_name: "Artist".into(),
            album_name: "Album".into(),
            artwork_url: "https://a/{w}x{h}bb.jpg".into(),
            duration_ms: 200_000,
            ..TrackMeta::default()
        },
        container_id: None,
    }
}

fn playing(uid: u64) -> PlayerState {
    PlayerState {
        generation: 4,
        status: Status::Playing,
        track: Some(item(uid)),
        position_ms: 10_000,
        duration_ms: 200_000,
        position_at: 1_000_000,
        volume: 60,
        repeat: Repeat::One,
        shuffle: true,
        ..PlayerState::default()
    }
}

#[test]
fn player_state_maps_to_mpris() {
    let u = integrations::mpris_update(&playing(17), 1_002_500);
    assert_eq!(u.status, Some(PlaybackStatus::Playing));
    assert_eq!(
        u.position_us,
        Some(12_500_000),
        "interpolated from positionAt while playing"
    );
    assert_eq!(
        (u.shuffle, u.loop_status, u.volume),
        (Some(true), Some(LoopStatus::Track), Some(0.6))
    );
    let m = u.metadata.unwrap();
    assert_eq!(
        m["mpris:trackid"], "/com/apple/music/track/17",
        "the queue uid"
    );
    assert_eq!(m["mpris:length"], 200_000_000);
    assert_eq!(m["mpris:artUrl"], "https://a/512x512bb.jpg");
    assert_eq!(m["xesam:artist"], json!(["Artist"]));

    let paused = PlayerState {
        status: Status::Paused,
        muted: true,
        ..playing(17)
    };
    let u = integrations::mpris_update(&paused, 9_999_999);
    assert_eq!(
        (u.status, u.position_us, u.volume),
        (Some(PlaybackStatus::Paused), Some(10_000_000), Some(0.0))
    );
    let idle = integrations::mpris_update(&PlayerState::default(), 0);
    assert_eq!(idle.status, Some(PlaybackStatus::Stopped));
    assert!(idle.metadata.unwrap().is_empty());

    let p = integrations::presence(&playing(17), "gb", 1_000_000).unwrap();
    assert_eq!(
        p.apple_url,
        "https://music.apple.com/gb/song/song-name/1440857781"
    );
    assert_eq!((p.started_at_ms, p.ends_at_ms), (990_000.0, 1_190_000.0));
}

#[test]
fn desktop_commands_become_player_commands() {
    let s = playing(17);
    let route = |c: PageCommand| integrations::route_command(c, &s, 1_000_000);
    assert_eq!(
        route(Transport::PlayPause.into()),
        Some(Routed::Player(Command::Toggle))
    );
    assert_eq!(
        route(
            Action::Seek {
                delta_ms: -15_000.0
            }
            .into()
        ),
        Some(Routed::Player(Command::Seek(Seek {
            position_ms: 0,
            expect_generation: Some(4)
        }))),
        "clamped at the start, for this track only"
    );
    assert_eq!(
        route(Action::SetPosition { ms: 999_999.0 }.into()),
        Some(Routed::Player(Command::Seek(Seek {
            position_ms: 200_000,
            expect_generation: Some(4)
        })))
    );
    assert_eq!(
        route(
            Action::SetLoopStatus {
                value: LoopStatus::Playlist
            }
            .into()
        ),
        Some(Routed::Player(Command::Mode(Mode {
            repeat: Some(Repeat::All),
            ..Mode::default()
        })))
    );
    assert_eq!(
        route(Action::Repeat.into()),
        Some(Routed::Player(Command::Mode(Mode {
            repeat: Some(Repeat::Off),
            ..Mode::default()
        }))),
        "one → off"
    );
    assert_eq!(
        route(Action::SetVolume { value: 140.0 }.into()),
        Some(Routed::Player(Command::Volume(Volume {
            level: 100,
            muted: None
        })))
    );
    assert_eq!(
        route(Action::Love.into()),
        Some(Routed::Ui(Intent::ToggleLove))
    );
    let idle = PlayerState::default();
    assert_eq!(
        integrations::route_command(Action::Seek { delta_ms: 5.0 }.into(), &idle, 0),
        None
    );
    assert_eq!(
        ShortcutAction::SeekForward.command(&s, 1_000_000),
        Some(Command::Seek(Seek {
            position_ms: 20_000,
            expect_generation: Some(4)
        }))
    );
    assert_eq!(
        ShortcutAction::VolumeDown.command(&s, 0),
        Some(Command::Volume(Volume {
            level: 55,
            muted: None
        }))
    );
}

#[derive(Default)]
struct Secrets(Mutex<std::collections::HashMap<String, String>>);

impl SecretStore for Secrets {
    fn get(&self, key: &str) -> Option<String> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn set(&self, key: &str, value: String) {
        self.0.lock().unwrap().insert(key.into(), value);
    }
    fn delete(&self, key: &str) {
        self.0.lock().unwrap().remove(key);
    }
}

#[tokio::test]
async fn scrobblers_get_now_playing_at_track_start_and_the_scrobble_when_it_is_due() {
    let lastfm = MockServer::start().await;
    let lb = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&lastfm)
        .await;
    Mock::given(method("POST"))
        .and(path("/1/submit-listens"))
        .and(header("authorization", "Token lb-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
        .mount(&lb)
        .await;
    let secrets = Arc::new(Secrets::default());
    for (k, v) in [
        ("lastfm_api_key", "key"),
        ("lastfm_secret", "secret"),
        ("lastfm_sk", "session"),
        ("lb_token", "lb-token"),
    ] {
        secrets.set(k, v.into());
    }
    let http = reqwest::Client::new();
    let scrobblers = Arc::new(Scrobblers {
        lastfm: Lastfm::with_root(http.clone(), secrets.clone(), lastfm.uri().parse().unwrap()),
        listenbrainz: ListenBrainz::with_root(http, secrets, lb.uri().parse().unwrap()),
    });
    let dir = tempfile::tempdir().unwrap();
    let settings = SettingsStore::load(
        Arc::new(PrefsFile::new(dir.path())),
        Store::open(dir.path(), StoreCipher::Plain).await,
    );
    let (tx, rx) = broadcast::channel(16);
    let inputs = Inputs {
        client: EngineClient::new(free_port(), EngineIdentity::generate().unwrap().cert_der())
            .unwrap(),
        events: rx,
        settings: settings.subscribe(),
        session: watch::channel(None).1,
        desktop: false,
    };
    let (services, _intents) = Integrations::start(inputs, scrobblers).await;

    let started = TrackStarted {
        generation: 4,
        item: item(17),
        duration_ms: 200_000,
        quality: None,
        started_at: 1_700_000_000_000,
    };
    tx.send(EngineEvent::Player(PlayerEvent::Track(Box::new(started))))
        .unwrap();
    wait_until("now playing", || async {
        lastfm.received_requests().await.unwrap().len() == 1
            && lb.received_requests().await.unwrap().len() == 1
    })
    .await;
    let body =
        String::from_utf8(lastfm.received_requests().await.unwrap()[0].body.clone()).unwrap();
    assert!(
        body.contains("method=track.updateNowPlaying") && !body.contains("timestamp"),
        "{body}"
    );
    assert!(
        body.contains("duration=200") && body.contains("album=Album"),
        "{body}"
    );
    let listen: Value = lb.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(listen["listen_type"], "playing_now");

    // Nothing more until the engine says the scrobble is due.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(lastfm.received_requests().await.unwrap().len(), 1);
    let due = ScrobbleDue {
        generation: 4,
        item: item(17),
        duration_ms: 200_000,
        timestamp: 1_700_000_000,
    };
    tx.send(EngineEvent::Player(PlayerEvent::ScrobbleDue(Box::new(due))))
        .unwrap();
    wait_until("scrobble", || async {
        lastfm.received_requests().await.unwrap().len() == 2
            && lb.received_requests().await.unwrap().len() == 2
    })
    .await;
    let body =
        String::from_utf8(lastfm.received_requests().await.unwrap()[1].body.clone()).unwrap();
    assert!(
        body.contains("method=track.scrobble") && body.contains("timestamp=1700000000"),
        "{body}"
    );
    let listen: Value = lb.received_requests().await.unwrap()[1]
        .body_json()
        .unwrap();
    assert_eq!(
        (
            listen["listen_type"].as_str(),
            listen["payload"][0]["listened_at"].as_u64()
        ),
        (Some("single"), Some(1_700_000_000))
    );
    services.stop().await;
}
