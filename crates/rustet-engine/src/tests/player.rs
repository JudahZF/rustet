//! Player, auth and guard tests through the router: fake decks that "play" in real time, the
//! fake provider whose tracks stream fixture bytes.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rustet_core::pipeline::Codec;
use rustet_player::ports::{
    Deck, DeckError, DeckEvent, DeckEvents, DeckId, DeckSignal, DeckTiming, LoadId, LoadRequest,
    deck_channel,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::playback::{Script, data, song, with_provider};
use super::{HOST, app, body_json, body_text, call};
use crate::state::Engine;

/// Every fake media is this long.
const LENGTH_MS: u64 = 1500;

#[derive(Default)]
struct St {
    load: Option<LoadId>,
    base_ms: u64,
    since: Option<Instant>,
    /// Bumped at every load, seek, pause and stop: an end timer of an older one does nothing.
    epoch: u64,
}

/// A deck whose media plays in real time for [`LENGTH_MS`] and then ends cleanly.
struct FakeDeck {
    id: DeckId,
    tx: DeckEvents,
    st: Arc<Mutex<St>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl FakeDeck {
    fn name(&self) -> &'static str {
        if self.id == DeckId::A { "A" } else { "B" }
    }

    fn note(&self, what: String) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{} {what}", self.name()));
    }

    fn start(&self) {
        let (load, epoch, remaining) = {
            let mut st = self.st.lock().unwrap();
            if st.since.is_some() {
                return;
            }
            st.since = Some(Instant::now());
            st.epoch += 1;
            (st.load, st.epoch, LENGTH_MS.saturating_sub(st.base_ms))
        };
        let Some(load) = load else { return };
        let _ = self.tx.send(DeckSignal {
            deck: self.id,
            load,
            event: DeckEvent::Playing,
        });
        let (st, tx, deck) = (self.st.clone(), self.tx.clone(), self.id);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(remaining)).await;
            if st.lock().unwrap().epoch == epoch {
                let _ = tx.send(DeckSignal {
                    deck,
                    load,
                    event: DeckEvent::Ended { source_eof: true },
                });
            }
        });
    }

    fn position(st: &St) -> u64 {
        (st.base_ms + st.since.map_or(0, |s| s.elapsed().as_millis() as u64)).min(LENGTH_MS)
    }
}

#[async_trait]
impl Deck for FakeDeck {
    async fn load(&self, req: LoadRequest) -> Result<(), DeckError> {
        self.note(format!(
            "load {} paused={} at={}",
            req.session.0, req.paused, req.start_ms
        ));
        {
            let mut st = self.st.lock().unwrap();
            *st = St {
                load: Some(req.load),
                base_ms: req.start_ms,
                since: None,
                epoch: st.epoch + 1,
            };
        }
        if !req.paused {
            self.start();
        }
        Ok(())
    }
    async fn play(&self) -> Result<(), DeckError> {
        self.note("play".into());
        self.start();
        Ok(())
    }
    async fn pause(&self) -> Result<(), DeckError> {
        self.note("pause".into());
        let mut st = self.st.lock().unwrap();
        st.base_ms = Self::position(&st);
        st.since = None;
        st.epoch += 1;
        Ok(())
    }
    async fn seek(&self, position_ms: u64) -> Result<(), DeckError> {
        self.note(format!("seek {position_ms}"));
        let playing = {
            let mut st = self.st.lock().unwrap();
            let playing = st.since.take().is_some();
            st.base_ms = position_ms;
            st.epoch += 1;
            playing
        };
        if playing {
            self.start();
        }
        Ok(())
    }
    async fn stop(&self) -> Result<(), DeckError> {
        let mut st = self.st.lock().unwrap();
        *st = St {
            epoch: st.epoch + 1,
            ..St::default()
        };
        Ok(())
    }
    async fn set_volume(&self, _: u8, _: bool) -> Result<(), DeckError> {
        Ok(())
    }
    async fn timing(&self) -> DeckTiming {
        let st = self.st.lock().unwrap();
        DeckTiming {
            position_ms: Some(Self::position(&st)),
            length_ms: Some(LENGTH_MS),
            source_ended: false,
            volume: 100,
            muted: false,
        }
    }
}

/// An engine with the fake provider (AAC tracks of fixture bytes), signed in, and the player on
/// fake decks. Returns the deck log.
async fn player_engine(dir: &std::path::Path) -> (Arc<Engine>, Arc<Mutex<Vec<String>>>) {
    let e = with_provider(
        dir,
        song(
            Codec::Aac,
            Script {
                data: data(64 << 10),
                ..Script::default()
            },
        ),
    )
    .await;
    e.tokens.set_developer("dev");
    e.set_music_user_token("mut");
    let (e, log) = start_fake_player(e);
    (e, log)
}

fn start_fake_player(e: Arc<Engine>) -> (Arc<Engine>, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = deck_channel();
    let deck = |id| -> Arc<dyn Deck> {
        Arc::new(FakeDeck {
            id,
            tx: tx.clone(),
            st: Arc::default(),
            log: log.clone(),
        })
    };
    e.start_player([deck(DeckId::A), deck(DeckId::B)], rx);
    (e, log)
}

fn items(ids: &[&str]) -> Value {
    json!(ids.iter().map(|id| json!({"id": id, "kind": "song", "meta": {"name": format!("T{id}"), "durationMs": LENGTH_MS, "audioTraits": ["lossy-stereo"]}})).collect::<Vec<_>>())
}

async fn state(app: &Router) -> Value {
    let resp = call(app, "GET", "/api/v1/player", "").await;
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

/// Polls the state until `ok` holds.
async fn until(app: &Router, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = state(app).await;
        if ok(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "{what}: last state {s:#}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn status_of(app: &Router, method: &str, uri: &str, body: &str) -> (u16, String) {
    let resp = call(app, method, uri, body).await;
    (resp.status().as_u16(), body_text(resp).await)
}

// Every route answers with the status the spec gives, and each command error maps to its code.
#[tokio::test(flavor = "multi_thread")]
async fn every_player_route_and_its_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (e, log) = player_engine(dir.path()).await;
    let app = app(&e);
    // The capabilities reach the player as a command, a moment after it starts.
    let s = until(&app, "the player knows the sign-in", |s| {
        s["capabilities"]["signedIn"] == true
    })
    .await;
    assert_eq!(s["status"], "idle");

    let q = json!({"items": items(&["1", "2", "3"]), "startIndex": 0}).to_string();
    assert_eq!(
        status_of(&app, "POST", "/api/v1/player/queue", &q).await.0,
        202
    );
    let s = until(&app, "first track plays", |s| s["status"] == "playing").await;
    let uids: Vec<u64> = s["queue"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uid"].as_u64().unwrap())
        .collect();
    let generation = s["generation"].as_u64().unwrap();

    for (method, uri, body, want) in [
        ("POST", "/api/v1/player/queue", "not json", 400),
        (
            "POST",
            "/api/v1/player/queue",
            &json!({"items": items(&["1"]), "startIndex": 5}).to_string(),
            400,
        ),
        (
            "POST",
            "/api/v1/player/queue",
            &json!({"items": items(&["1"]), "context": {"kind": "liveStation", "id": "ra.1"}})
                .to_string(),
            400,
        ),
        (
            "POST",
            "/api/v1/player/queue/insert",
            &json!({"items": items(&["4"]), "position": "next"}).to_string(),
            202,
        ),
        (
            "POST",
            "/api/v1/player/queue/insert",
            &json!({"items": items(&["5"]), "position": "later"}).to_string(),
            202,
        ),
        (
            "POST",
            "/api/v1/player/queue/move",
            &json!({"uid": uids[2], "toIndex": 1}).to_string(),
            202,
        ),
        (
            "POST",
            "/api/v1/player/queue/move",
            r#"{"uid": 999999, "toIndex": 0}"#,
            404,
        ),
        (
            "POST",
            "/api/v1/player/queue/jump",
            r#"{"uid": 999999}"#,
            404,
        ),
        (
            "POST",
            "/api/v1/player/queue/remove",
            &json!({"uids": [uids[2]]}).to_string(),
            202,
        ),
        ("POST", "/api/v1/player/volume", r#"{"level": 150}"#, 400),
        (
            "POST",
            "/api/v1/player/volume",
            r#"{"level": 40, "muted": false}"#,
            202,
        ),
        (
            "POST",
            "/api/v1/player/settings",
            r#"{"mvMaxHeight": 333}"#,
            400,
        ),
        (
            "POST",
            "/api/v1/player/settings",
            r#"{"quality": "highQuality", "historyEnabled": true}"#,
            202,
        ),
        (
            "POST",
            "/api/v1/player/mode",
            r#"{"repeat": "all", "shuffle": false, "autoplay": false}"#,
            202,
        ),
        (
            "POST",
            "/api/v1/player/hover",
            r#"{"id": "9", "kind": "song"}"#,
            202,
        ),
        (
            "POST",
            "/api/v1/player/video",
            r#"{"audioOnly": "yes"}"#,
            400,
        ),
        (
            "POST",
            "/api/v1/player/video",
            r#"{"audioOnly": true}"#,
            202,
        ),
        (
            "POST",
            "/api/v1/player/seek",
            &json!({"positionMs": 200, "expectGeneration": generation + 7}).to_string(),
            409,
        ),
        (
            "POST",
            "/api/v1/player/seek",
            &json!({"positionMs": 200, "expectGeneration": generation}).to_string(),
            202,
        ),
        ("POST", "/api/v1/player/pause", "", 202),
        ("POST", "/api/v1/player/play", "", 202),
        ("POST", "/api/v1/player/toggle", "", 202),
        ("POST", "/api/v1/player/toggle", "", 202),
        ("POST", "/api/v1/player/next", "", 202),
        ("POST", "/api/v1/player/previous", "", 202),
        ("POST", "/api/v1/player/queue/clear", "{}", 202),
        ("POST", "/api/v1/player/stop", "", 202),
        ("GET", "/api/v1/player/history", "", 200),
        ("DELETE", "/api/v1/player/history", "", 204),
    ] {
        let (got, text) = status_of(&app, method, uri, body).await;
        assert_eq!(got, want, "{method} {uri} {body:.60}: {text}");
    }
    let s = state(&app).await;
    assert_eq!(
        (s["volume"].as_u64(), s["repeat"].as_str()),
        (Some(40), Some("all"))
    );
    assert_eq!(s["settings"]["quality"], "highQuality");
    assert_eq!(s["mvAudioOnly"], true);
    let log = log.lock().unwrap().join("\n");
    assert!(log.contains("seek 200"), "{log}");
    assert!(log.contains("pause"), "{log}");
}

/// Reads bus events until `done` holds for one; returns them all as (name, payload).
async fn collect(
    sub: &mut crate::events::Subscription,
    done: impl Fn(&str, &Value) -> bool,
) -> Vec<(String, Value)> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.events.recv())
            .await
            .expect("events in time")
            .expect("bus open");
        let payload: Value = serde_json::from_str(&ev.payload).unwrap();
        let stop = done(ev.kind, &payload);
        seen.push((ev.kind.to_owned(), payload));
        if stop {
            return seen;
        }
    }
}

// A queue played to the end: each track starts, positions advance, the hand-over is the
// gapless pre-roll (the next track loads paused on the other deck before the end), history
// is written, and the player stops after the last track. The SSE stream carries the events
// under the crate's names.
#[tokio::test(flavor = "multi_thread")]
async fn a_queue_plays_to_the_end_with_its_events() {
    let dir = tempfile::tempdir().unwrap();
    let (e, log) = player_engine(dir.path()).await;
    let app = app(&e);
    let mut sub = e.events.subscribe(None);
    let q = json!({"items": items(&["11", "12", "13"]), "startIndex": 0, "context": {"kind": "album", "id": "100"}});
    assert_eq!(
        status_of(&app, "POST", "/api/v1/player/queue", &q.to_string())
            .await
            .0,
        202
    );
    let seen = collect(&mut sub, |name, p| {
        name == "player.state" && p["status"] == "stopped"
    })
    .await;

    let started: Vec<&str> = seen
        .iter()
        .filter(|(n, _)| n == "player.track")
        .map(|(_, p)| p["item"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(started, ["11", "12", "13"]);
    for name in [
        "player.queue",
        "player.position",
        "player.history",
        "playback.created",
    ] {
        assert!(seen.iter().any(|(n, _)| n == name), "no {name} event");
    }
    let mut last = (0, 0);
    for (_, p) in seen.iter().filter(|(n, _)| n == "player.position") {
        let at = (
            p["uid"].as_u64().unwrap(),
            p["positionMs"].as_u64().unwrap(),
        );
        assert!(
            at.0 > last.0 || at.1 >= last.1,
            "position went back within a track: {last:?} → {at:?}"
        );
        last = at;
    }
    let log = log.lock().unwrap().clone();
    let cues = log.iter().filter(|l| l.contains("paused=true")).count();
    assert_eq!(cues, 2, "tracks 2 and 3 are pre-rolled paused: {log:#?}");
    assert_eq!(
        log.iter().filter(|l| l.contains("paused=false")).count(),
        1,
        "only track 1 loads playing: {log:#?}"
    );

    let history = body_json(call(&app, "GET", "/api/v1/player/history", "").await).await;
    let ids: Vec<&str> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["13", "12", "11"], "newest first");
    assert!(e.data_dir.join("player-history.json").exists());

    // The SSE route itself names player events as the crate does.
    assert_eq!(
        status_of(&app, "POST", "/api/v1/player/volume", r#"{"level": 30}"#)
            .await
            .0,
        202
    );
    let req = Request::builder()
        .uri("/api/v1/events")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    let mut body = app.clone().oneshot(req).await.unwrap().into_body();
    let frames = super::frames(&mut body, 1).await;
    assert_eq!(frames[0].0, "engine.snapshot");
    assert_eq!(
        frames[0].1["payload"]["snapshot"]["player"]["status"],
        "stopped"
    );
    assert_eq!(frames[0].1["payload"]["snapshot"]["player"]["volume"], 30);
}

// Without a music user token an open fails as authentication: the player shows the error,
// and `auth.expired` reaches the stream.
#[tokio::test(flavor = "multi_thread")]
async fn no_token_is_an_authentication_error() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(
        dir.path(),
        song(
            Codec::Aac,
            Script {
                data: data(1024),
                ..Script::default()
            },
        ),
    )
    .await;
    let (e, _) = start_fake_player(e);
    let app = app(&e);
    let mut sub = e.events.subscribe(None);
    let q = json!({"items": items(&["1440857781"]), "startIndex": 0}).to_string();
    assert_eq!(
        status_of(&app, "POST", "/api/v1/player/queue", &q).await.0,
        202
    );
    let seen = collect(&mut sub, |name, _| name == "player.error").await;
    assert!(
        seen.iter().any(|(n, _)| n == "auth.expired"),
        "no auth.expired before the error: {seen:#?}"
    );
    let (_, err) = seen.last().unwrap();
    assert_eq!(err["code"], "openFailed");
    assert!(
        err["message"].as_str().unwrap().contains("not signed in"),
        "{err}"
    );
    let s = until(&app, "error in the state", |s| !s["error"].is_null()).await;
    assert_eq!(
        (
            s["error"]["code"].as_str(),
            s["capabilities"]["signedIn"].as_bool()
        ),
        (Some("openFailed"), Some(false))
    );
}

// GET /api/v1/auth reports which tokens exist (never the tokens); DELETE signs the web
// session out and the player learns it.
#[tokio::test(flavor = "multi_thread")]
async fn auth_routes() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(dir.path(), song(Codec::Aac, Script::default())).await;
    let (e, _) = start_fake_player(e);
    let app = app(&e);
    let auth = body_json(call(&app, "GET", "/api/v1/auth", "").await).await;
    assert_eq!(
        auth,
        json!({"webToken": false, "fairplayToken": false, "storefront": "", "language": "en-US"})
    );

    let push = json!({"musicUserToken": "secret-mut", "storefront": "gb", "language": "en-GB"})
        .to_string();
    assert_eq!(
        status_of(&app, "POST", "/api/v1/library/token", &push)
            .await
            .0,
        200
    );
    let text = body_text(call(&app, "GET", "/api/v1/auth", "").await).await;
    assert!(!text.contains("secret-mut"), "a token leaked: {text}");
    let auth: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        auth,
        json!({"webToken": true, "fairplayToken": false, "storefront": "gb", "language": "en-GB"})
    );
    until(&app, "signed in", |s| s["capabilities"]["signedIn"] == true).await;

    // The session route is the one that returns the token: for the native UI only, never for a
    // web page, not even the Apple Music page the guard lets through elsewhere.
    let session = body_json(call(&app, "GET", "/api/v1/auth/session", "").await).await;
    assert_eq!(
        session,
        json!({"musicUserToken": "secret-mut", "storefront": "gb"})
    );
    let from_page = Request::builder()
        .uri("/api/v1/auth/session")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "https://music.apple.com")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(from_page).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        status_of(&app, "DELETE", "/api/v1/auth/token", "").await.0,
        204
    );
    assert_eq!(
        status_of(&app, "GET", "/api/v1/auth/session", "").await.0,
        404
    );
    assert_eq!(
        body_json(call(&app, "GET", "/api/v1/auth", "").await).await["webToken"],
        false
    );
    until(&app, "signed out", |s| {
        s["capabilities"]["signedIn"] == false
    })
    .await;
    assert_eq!(
        status_of(&app, "DELETE", "/api/v1/auth/token", "").await.0,
        204,
        "signing out twice is fine"
    );
}

// The native UI is not a web page and sends no Origin. The rule: no Origin passes the guard
// (as the shell's main process always did); a browser always sends one on a cross-site POST, so
// a foreign page is still refused. Other local users are kept out by the peer check at the
// socket, which stays the boundary.
#[tokio::test(flavor = "multi_thread")]
async fn native_ui_requests_without_origin_pass_the_guard() {
    let dir = tempfile::tempdir().unwrap();
    let (e, _) = player_engine(dir.path()).await;
    let app = app(&e);
    let send = |method: &str, uri: &str, origin: Option<&str>| {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, HOST);
        if let Some(o) = origin {
            req = req.header(header::ORIGIN, o);
        }
        app.clone().oneshot(
            req.header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"level":20}"#))
                .unwrap(),
        )
    };
    for (method, uri) in [
        ("GET", "/api/v1/player"),
        ("POST", "/api/v1/player/volume"),
        ("GET", "/api/v1/auth"),
    ] {
        let ok = send(method, uri, None).await.unwrap().status();
        assert!(ok.is_success(), "{method} {uri} without Origin: {ok}");
        let foreign = send(method, uri, Some("https://attacker.example"))
            .await
            .unwrap()
            .status();
        assert_eq!(
            foreign,
            StatusCode::FORBIDDEN,
            "{method} {uri} from a foreign page"
        );
        let null = send(method, uri, Some("null")).await.unwrap().status();
        assert_eq!(
            null,
            StatusCode::FORBIDDEN,
            "{method} {uri} from a sandboxed or file page"
        );
    }
    // DNS rebinding: a foreign host name never passes, Origin or not.
    let req = Request::builder()
        .uri("/api/v1/player")
        .header(header::HOST, "rebind.example:20025")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

// Without the player (no `start_player`) its routes answer 503, like the other optional parts.
#[tokio::test]
async fn player_routes_without_a_player_are_503() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&Arc::new(
        super::engine(dir.path(), "http://127.0.0.1:9").await,
    ));
    for (m, uri) in [
        ("GET", "/api/v1/player"),
        ("POST", "/api/v1/player/play"),
        ("POST", "/api/v1/player/queue"),
    ] {
        assert_eq!(
            status_of(&app, m, uri, "{}").await,
            (503, "player not available\n".into()),
            "{m} {uri}"
        );
    }
}

// A restart restores the queue paused at its position, from the snapshot in the data directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_restores_the_queue_paused() {
    let dir = tempfile::tempdir().unwrap();
    let (e, _) = player_engine(dir.path()).await;
    let app1 = app(&e);
    let q = json!({"items": items(&["21", "22"]), "startIndex": 1, "startMs": 700, "play": false})
        .to_string();
    assert_eq!(
        status_of(&app1, "POST", "/api/v1/player/queue", &q).await.0,
        202
    );
    e.player.get().unwrap().shutdown().await;
    assert!(e.data_dir.join("player-state.json").exists());

    let next = with_provider(dir.path(), song(Codec::Aac, Script::default())).await;
    let (next, _) = start_fake_player(next);
    let s = until(&app(&next), "restored", |s| s["restored"] == true).await;
    assert_eq!(
        (s["status"].as_str(), s["track"]["id"].as_str()),
        (Some("paused"), Some("22"))
    );
    assert_eq!(s["positionMs"], 700);
}

// The spool feeds libvlc a stream outside the cache: reads follow the writer, the end is
// clean, a seek stream starts where the provider's seek source starts, and an abort unblocks.
#[tokio::test(flavor = "multi_thread")]
async fn spool_follows_the_stream() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let (release, released) = tokio::sync::watch::channel(false);
    let bytes = data(100_000);
    let script = Script {
        data: bytes.clone(),
        pause_at: Some((40_000, released)),
        ..Script::default()
    };
    let e = with_provider(dir.path(), song(Codec::Aac, script)).await;
    let sess = e
        .playback
        .as_ref()
        .unwrap()
        .manager
        .open(
            &tokio_util::sync::CancellationToken::new(),
            rustet_core::media::OpenRequest {
                asset_id: "1".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut src = crate::player::spool_start(&e, &sess.id, 0).await.unwrap();
    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        src.read_to_end(&mut got).map(|_| got)
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!reader.is_finished(), "the reader waits at the written end");
    release.send(true).unwrap();
    assert_eq!(
        tokio::task::spawn_blocking(move || reader.join().unwrap())
            .await
            .unwrap()
            .unwrap(),
        bytes
    );

    let mut seek = crate::player::spool_start(&e, &sess.id, 2_000)
        .await
        .unwrap();
    let got = tokio::task::spawn_blocking(move || {
        let mut got = Vec::new();
        seek.read_to_end(&mut got).map(|_| got)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(got, bytes, "the fake seek source restarts the data");
    let leftovers = std::fs::read_dir(&e.spool_dir).map_or(0, |d| d.count());
    assert_eq!(leftovers, 0, "spool files go with their readers");
}
