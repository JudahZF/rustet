//! The models a view holds, against a fake engine (`fake_engine`) and a fake Apple Music API
//! (`pages_common`): the account over both sign-ins, the player state through a reconnect,
//! and the downloads list.

mod fake_engine;
mod pages_common;

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use fake_engine::{Bodies, FakeEngine, sse, wait_until};
use rustet_ampapi::api::{ResourceRef, ResourceType};
use rustet_app::account::lossless::{LosslessAccount, SignOutResult};
use rustet_app::account::{Account, AccountModel, AccountState, Backend, SignInProgram, SigningIn};
use rustet_app::downloads::{DownloadTarget, Downloads};
use rustet_app::engine::events::{self, EngineEvent};
use rustet_app::library::{Library, SyncStatus, Syncer};
use rustet_app::settings::SettingsStore;
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::{Store, StoreCipher};
use rustet_player::model::{Event as PlayerEvent, PlayerState, Status};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const WAIT: Duration = Duration::from_secs(5);

async fn until<T: Clone>(rx: &mut watch::Receiver<T>, what: &str, f: impl Fn(&T) -> bool) -> T {
    tokio::time::timeout(WAIT, rx.wait_for(|v| f(v)))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap()
        .clone()
}

async fn answer(e: &FakeEngine, verb: &str, route: &str, status: u16, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&e.mock)
        .await;
}

fn snapshot() -> EngineEvent {
    EngineEvent::Snapshot(Box::default())
}

/// The account model over `engine` and a mock Apple Music API with a one-song library.
struct AccountRig {
    model: AccountModel,
    apple: pages_common::Env,
    library: Library,
    events: broadcast::Sender<EngineEvent>,
    _dir: tempfile::TempDir,
}

async fn account_rig(engine: &FakeEngine, store_values: Value, sign_in_line: &str) -> AccountRig {
    let apple = pages_common::env().await;
    let song = json!({"id": "i.1", "type": "library-songs", "attributes": {"name": "Song",
        "playParams": {"id": "i.1", "kind": "song", "isLibrary": true, "catalogId": "101"}}});
    apple
        .get("/v1/me/library/songs", json!({"data": [song]}).to_string())
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreCipher::Plain).await;
    if let Value::Object(values) = store_values {
        store.update(|m| m.extend(values));
    }
    let library = Library::open_in(dir.path().join("cache")).await.unwrap();
    let client = engine.client();
    let lossless =
        LosslessAccount::new(client.clone(), store.clone(), dir.path().join("drm"), || {});
    let script = format!("[ \"$1\" = --sign-out ] && exit 0; echo '{sign_in_line}'");
    let program = SignInProgram {
        program: "sh".into(),
        args: vec!["-c".into(), script.into(), "sh".into()],
    };
    let web = Account::load(
        store,
        client.clone(),
        program,
        dir.path().join("web-signin"),
    );
    let syncer = Syncer::new(library.clone(), apple.api.clone());
    let model = AccountModel::new(
        client,
        apple.api.clone(),
        syncer,
        "en-GB".into(),
        lossless,
        web,
    );
    let (events, _) = broadcast::channel(16);
    tokio::spawn(
        model
            .clone()
            .run(events.subscribe(), events.subscribe(), events.subscribe()),
    );
    AccountRig {
        model,
        apple,
        library,
        events,
        _dir: dir,
    }
}

async fn songs(library: &Library) -> u64 {
    library.status().await.unwrap().stats.songs
}

#[tokio::test]
async fn fairplay_sign_in_reads_the_session_and_signs_the_api_in() {
    let e = FakeEngine::start().await;
    answer(
        &e,
        "GET",
        "/api/v1/drm/status",
        200,
        json!({"available": true}),
    )
    .await;
    answer(
        &e,
        "GET",
        "/api/v1/auth/session",
        200,
        json!({"musicUserToken": "mut-1", "storefront": "GB"}),
    )
    .await;
    answer(&e, "POST", "/api/v1/drm/logout", 200, json!({})).await;
    answer(&e, "DELETE", "/api/v1/player/history", 204, json!({})).await;
    let rig = account_rig(&e, json!({}), "").await;
    let mut state = rig.model.state();
    assert_eq!(*state.borrow(), AccountState::Starting);

    rig.events.send(snapshot()).unwrap();
    until(&mut state, "signed in", |s| {
        *s == AccountState::SignedIn {
            storefront: "gb".into(),
        }
    })
    .await;
    assert_eq!(*rig.model.backend().borrow(), Some(Backend::FairPlay));
    let api = &rig.apple.api;
    assert_eq!(
        api.user_token().map(|t| t.secret().to_owned()).as_deref(),
        Some("mut-1")
    );
    assert_eq!(api.locale().storefront, "gb");
    let session = rig.model.session().borrow().clone().expect("a session");
    assert!(
        !format!("{session:?}").contains("mut-1"),
        "Debug never shows the token"
    );
    // The library sync starts with the sign-in.
    let mut sync = rig.model.syncer().status();
    until(&mut sync, "the sync", |s| matches!(s, SyncStatus::Done(_))).await;
    assert_eq!(songs(&rig.library).await, 1);

    // "Sign in" opens the sheet; the account shows it.
    rig.model.sign_in().await;
    until(&mut state, "the sheet", |s| {
        matches!(s, AccountState::SigningIn(SigningIn::Sheet(_)))
    })
    .await;
    rig.model.lossless().cancel().await;
    until(&mut state, "the sheet closed", |s| {
        matches!(s, AccountState::SignedIn { .. })
    })
    .await;

    // Sign-out clears the `Api`, the library and the history.
    assert_eq!(rig.model.sign_out(false).await, SignOutResult::Done);
    assert_eq!(*rig.model.state().borrow(), AccountState::SignedOut);
    assert!(!api.is_signed_in());
    assert!(rig.model.session().borrow().is_none());
    assert_eq!(songs(&rig.library).await, 0);
    assert_eq!(e.requests("/api/v1/player/history").await.len(), 1);
}

#[tokio::test]
async fn an_expired_token_signs_out_with_a_message_and_is_not_used_again() {
    let e = FakeEngine::start().await;
    answer(
        &e,
        "GET",
        "/api/v1/drm/status",
        200,
        json!({"available": true}),
    )
    .await;
    answer(
        &e,
        "GET",
        "/api/v1/auth/session",
        200,
        json!({"musicUserToken": "mut-1", "storefront": "us"}),
    )
    .await;
    let rig = account_rig(&e, json!({}), "").await;
    let mut state = rig.model.state();
    rig.events.send(snapshot()).unwrap();
    until(&mut state, "signed in", |s| {
        matches!(s, AccountState::SignedIn { .. })
    })
    .await;
    let mut sync = rig.model.syncer().status();
    until(&mut sync, "the sync", |s| matches!(s, SyncStatus::Done(_))).await;

    rig.events
        .send(EngineEvent::Player(PlayerEvent::AuthExpired))
        .unwrap();
    let s = until(&mut state, "the expiry", |s| {
        matches!(s, AccountState::Error { .. })
    })
    .await;
    assert!(matches!(s, AccountState::Error { message } if message.contains("Sign in again")));
    assert!(!rig.apple.api.is_signed_in());
    assert_eq!(songs(&rig.library).await, 0, "the library cache is emptied");
    // The engine still hands out the refused token after a restart: it is not used.
    rig.events.send(snapshot()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(*state.borrow(), AccountState::Error { .. }));
    assert!(!rig.apple.api.is_signed_in());
}

#[tokio::test]
async fn auth_expired_without_a_token_is_signed_out_not_an_error() {
    let e = FakeEngine::start().await;
    answer(
        &e,
        "GET",
        "/api/v1/drm/status",
        200,
        json!({"available": true}),
    )
    .await;
    answer(
        &e,
        "GET",
        "/api/v1/auth/session",
        404,
        json!("not signed in"),
    )
    .await;
    let rig = account_rig(&e, json!({}), "").await;
    let mut state = rig.model.state();
    rig.events
        .send(EngineEvent::Player(PlayerEvent::AuthExpired))
        .unwrap();
    rig.events.send(snapshot()).unwrap();
    until(&mut state, "signed out", |s| *s == AccountState::SignedOut).await;
    rig.events
        .send(EngineEvent::Player(PlayerEvent::AuthExpired))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*state.borrow(), AccountState::SignedOut);
}

#[tokio::test]
async fn without_fairplay_the_web_window_signs_in_and_out() {
    let e = FakeEngine::start().await;
    answer(
        &e,
        "GET",
        "/api/v1/drm/status",
        200,
        json!({"available": false}),
    )
    .await;
    answer(
        &e,
        "POST",
        "/api/v1/library/token",
        200,
        json!({"ok": true}),
    )
    .await;
    answer(&e, "DELETE", "/api/v1/auth/token", 204, json!({})).await;
    answer(&e, "DELETE", "/api/v1/player/history", 204, json!({})).await;
    let line = r#"{"result":"token","musicUserToken":"tok-w","storefront":"de"}"#;
    let rig = account_rig(&e, json!({}), line).await;
    let mut state = rig.model.state();
    rig.events.send(snapshot()).unwrap();
    until(&mut state, "signed out", |s| *s == AccountState::SignedOut).await;
    assert_eq!(*rig.model.backend().borrow(), Some(Backend::Web));

    rig.model.sign_in().await;
    until(&mut state, "signed in", |s| {
        *s == AccountState::SignedIn {
            storefront: "de".into(),
        }
    })
    .await;
    let api = &rig.apple.api;
    assert_eq!(
        api.user_token().map(|t| t.secret().to_owned()).as_deref(),
        Some("tok-w")
    );
    assert_eq!(api.locale().storefront, "de");
    let mut sync = rig.model.syncer().status();
    until(&mut sync, "the sync", |s| matches!(s, SyncStatus::Done(_))).await;
    assert_eq!(songs(&rig.library).await, 1);

    assert_eq!(rig.model.sign_out(false).await, SignOutResult::Done);
    assert_eq!(*state.borrow(), AccountState::SignedOut);
    assert!(!api.is_signed_in());
    assert_eq!(songs(&rig.library).await, 0);
    assert_eq!(e.requests("/api/v1/auth/token").await.len(), 1);
}

#[tokio::test]
async fn a_stored_web_token_signs_in_at_the_first_snapshot() {
    let e = FakeEngine::start().await;
    answer(
        &e,
        "GET",
        "/api/v1/drm/status",
        200,
        json!({"available": false}),
    )
    .await;
    answer(
        &e,
        "POST",
        "/api/v1/library/token",
        200,
        json!({"ok": true}),
    )
    .await;
    let stored = json!({"web_music_user_token": "tok-s", "web_storefront": "jp"});
    let rig = account_rig(&e, stored, "").await;
    let mut state = rig.model.state();
    rig.events.send(snapshot()).unwrap();
    until(&mut state, "signed in", |s| {
        *s == AccountState::SignedIn {
            storefront: "jp".into(),
        }
    })
    .await;
    assert!(rig.apple.api.is_signed_in());
    // The web account pushes its token to every new engine.
    wait_until("the token push", || async {
        !e.requests("/api/v1/library/token").await.is_empty()
    })
    .await;
}

// ── Player ───────────────────────────────────────────────────────────────────

fn state_json(generation: u64, status: Status, position_ms: u64) -> Value {
    let mut v = serde_json::to_value(PlayerState::default()).unwrap();
    v["generation"] = json!(generation);
    v["status"] = serde_json::to_value(status).unwrap();
    v["positionMs"] = json!(position_ms);
    v["durationMs"] = json!(200_000);
    v
}

#[tokio::test]
async fn the_player_state_follows_the_stream_through_a_reconnect() {
    let e = FakeEngine::start().await;
    let snap = |state: Value| json!({"version": 1, "snapshot": {"player": state}});
    Mock::given(method("GET"))
        .and(path("/api/v1/events"))
        .respond_with(Bodies {
            bodies: vec![
                sse(1, "engine.snapshot", snap(state_json(3, Status::Playing, 1000)))
                    + &sse(2, "player.position", json!({"generation": 3, "uid": 1, "positionMs": 5000,
                        "durationMs": 200000, "positionAt": 42}))
                    // A position of an older track is not applied.
                    + &sse(3, "player.position", json!({"generation": 2, "uid": 1, "positionMs": 9,
                        "durationMs": 9, "positionAt": 9})),
                // The engine restarted: its snapshot replaces the state.
                sse(1, "engine.snapshot", snap(state_json(1, Status::Paused, 7000))),
            ],
            n: AtomicUsize::new(0),
        })
        .mount(&e.mock)
        .await;
    answer(&e, "GET", "/api/v1/player/history", 200, json!([])).await;
    let cancel = CancellationToken::new();
    let (tx, _link) = events::spawn(e.client(), cancel.clone());
    let player = rustet_app::player::Player::start(e.client(), tx.subscribe());
    let mut state = player.state();
    let s = until(&mut state, "the position", |s| {
        s.as_ref().is_some_and(|s| s.position_ms == 5000)
    })
    .await
    .unwrap();
    assert_eq!((s.generation, s.position_at), (3, 42));
    let s = until(&mut state, "the new engine's state", |s| {
        s.as_ref().is_some_and(|s| s.generation == 1)
    })
    .await
    .unwrap();
    assert_eq!((s.status, s.position_ms), (Status::Paused, 7000));
    assert_eq!(player.position_ms(), 7000, "paused: no interpolation");
    cancel.cancel();
}

// ── Downloads ────────────────────────────────────────────────────────────────

fn job(id: &str, phase: &str, queue_pos: i64) -> Value {
    json!({"jobId": id, "assetId": "1", "phase": phase, "queuePos": queue_pos, "title": id})
}

#[tokio::test]
async fn the_download_list_follows_export_events_and_actions() {
    let e = FakeEngine::start().await;
    answer(
        &e,
        "GET",
        "/api/v1/export",
        200,
        json!([job("b", "queued", 2), job("a", "downloading", 1)]),
    )
    .await;
    answer(&e, "POST", "/api/v1/export", 202, job("c", "queued", 3)).await;
    answer(
        &e,
        "POST",
        "/api/v1/export/a/retry",
        202,
        job("d", "queued", 4),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreCipher::Plain).await;
    let settings = SettingsStore::load(std::sync::Arc::new(PrefsFile::new(dir.path())), store);
    settings
        .update(|s| s.downloads.retry_on_fail = false)
        .unwrap();
    let apple = pages_common::env().await;
    let (tx, _) = broadcast::channel(16);
    let downloads = Downloads::start(
        e.client(),
        apple.api.clone(),
        settings.subscribe(),
        tx.subscribe(),
    );
    let mut list = downloads.list();

    tx.send(snapshot()).unwrap();
    let l = until(&mut list, "the list", |l| l.jobs.len() == 2).await;
    let ids: Vec<&str> = l.jobs.iter().map(|j| j.job_id.as_str()).collect();
    assert_eq!(ids, ["a", "b"], "in run order");
    assert_eq!((l.active(), l.finished()), (2, 0));

    let progress = |id: &str, phase: &str, percent: u32| {
        let p = json!({"jobId": id, "phase": phase, "percent": percent, "error": ""});
        EngineEvent::Export(serde_json::from_value(p).unwrap())
    };
    tx.send(progress("a", "downloading", 40)).unwrap();
    until(&mut list, "the progress", |l| l.jobs[0].percent == 40).await;
    tx.send(progress("a", "failed", 40)).unwrap();
    let l = until(&mut list, "the failure", |l| l.finished() == 1).await;
    assert_eq!(l.active(), 1);

    // "Download" sends the export with the settings and the storefront; the job is listed.
    let item = ResourceRef::new(ResourceType::Albums, "1440857781");
    let queued = downloads
        .download(DownloadTarget::Item(&item))
        .await
        .unwrap();
    assert_eq!(queued.job_id, "c");
    let sent: Value = e
        .requests("/api/v1/export")
        .await
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .unwrap()
        .body_json()
        .unwrap();
    assert_eq!(sent["assetId"], "1440857781");
    assert_eq!(sent["storefront"], "us");
    assert_eq!(
        sent["capabilities"]["lossless"], false,
        "the engine reported no lossless"
    );
    assert!(
        sent.get("token").is_none() && sent.get("mutToken").is_none(),
        "no tokens in the request"
    );
    assert!(
        downloads
            .list()
            .borrow()
            .jobs
            .iter()
            .any(|j| j.job_id == "c")
    );

    let retried = downloads.retry("a").await.unwrap();
    assert_eq!(retried.job_id, "d");
    let catalog_only = ResourceRef::new(ResourceType::Stations, "ra.1");
    assert!(
        downloads
            .download(DownloadTarget::Item(&catalog_only))
            .await
            .is_err()
    );
}
