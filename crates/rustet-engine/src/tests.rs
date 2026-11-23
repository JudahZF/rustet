//! Handler tests through the router, with fakes for Apple's API (wiremock), the DRM backend and
//! the media provider.

mod playback;
mod player;
mod player_live;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use rustet_core::media::{OpenRequest, Provider, Session};
use rustet_core::pipeline::PipelineError;
use rustet_core::ring;
use rustet_drm::{
    AccountInfo, BackendConfig, BackendError, BackendLink, DrmBackend, DrmEvent, DrmManager,
    DrmState, FairPlayState, ProgressiveMvUrl, RestartPolicy, SessionManager, StateUpdate,
};
use serde_json::{Value, json};
use tokio::io::DuplexStream;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::breaker::CircuitBreaker;
use crate::epoch::{Epoch, Lifecycle};
use crate::events::EventBus;
use crate::guard::GuardConfig;
use crate::state::{Drm, Engine, Playback, Tokens};

const HOST: &str = "127.0.0.1:20025";

/// An engine over temporary directories. Apple's API is `amp_base`; no DRM, no playback, no
/// libvlc.
async fn engine(dir: &Path, amp_base: &str) -> Engine {
    let http = reqwest::Client::new();
    let streams = Arc::new(rustet_streams::Streams::with_cache_root(
        http.clone(),
        &dir.join("streams"),
    ));
    let mv_prefs =
        crate::handlers::mvcache::MvPrefs::load(dir.join("mv-prefs.json"), &streams).await;
    let session = Arc::new(SessionManager::new(dir.join("session")));
    std::fs::create_dir_all(dir.join("library")).unwrap();
    Engine {
        guard: Arc::new(GuardConfig {
            port: 20025,
            allow_local_origins: false,
            api_token: None,
        }),
        epoch: Arc::new(Epoch::new()),
        lifecycle: Lifecycle::new(),
        events: Arc::new(EventBus::new()),
        tokens: Arc::new(Tokens::new(session)),
        drm: None,
        amp: rustet_ampapi::Client::with_base_urls(http.clone(), amp_base, amp_base),
        amp_base: amp_base.to_owned(),
        http,
        streams,
        mv_prefs,
        disk_cache: Some(rustet_diskcache::Cache::new(dir.join("playback")).unwrap()),
        library: Some(
            rustet_library::Store::open(dir.join("library"))
                .await
                .unwrap(),
        ),
        playback: None,
        export: None,
        alac: Arc::default(),
        vseg: Default::default(),
        vlc: None,
        open_latency: ring::Buffer::new(100),
        open_breaker: CircuitBreaker::new(3, Duration::from_secs(60)),
        shutdown: CancellationToken::new(),
        data_dir: dir.join("engine-data"),
        spool_dir: dir.join("spool"),
        video_dir: {
            let video = dir.join("video");
            rustet_framering::ensure_private_dir(&video).unwrap();
            Some(video)
        },
        apple: Default::default(),
        player: Default::default(),
        player_decks: Default::default(),
        player_caps: Default::default(),
        player_owns_libvlc: Default::default(),
    }
}

fn app(engine: &Arc<Engine>) -> Router {
    crate::routes::router(engine.clone())
}

async fn call(app: &Router, method: &str, uri: &str, body: &str) -> Response {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn body_text(resp: Response) -> String {
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

async fn body_json(resp: Response) -> Value {
    serde_json::from_str(&body_text(resp).await).unwrap()
}

// ── System ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_and_errors_keep_the_go_wire_format() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&Arc::new(engine(dir.path(), "http://127.0.0.1:9").await));
    let resp = call(&app, "GET", "/api/v1/status", "").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        resp.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://music.apple.com"
    );
    assert_eq!(body_text(resp).await, "{\"status\":\"ok\"}\n");

    let resp = call(&app, "GET", "/api/v1/nope", "").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
}

// The guard runs before routing: a preflight on a GET-only route, or on no route at all, is
// answered, and a forbidden origin never reaches a handler.
#[tokio::test]
async fn guard_wraps_every_route() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&Arc::new(engine(dir.path(), "http://127.0.0.1:9").await));
    for uri in ["/api/v1/status", "/api/v1/playback/x/audio", "/nowhere"] {
        let req = Request::builder()
            .method("OPTIONS")
            .uri(uri)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, "https://music.apple.com")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{uri}");
        assert_eq!(
            resp.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
            "GET, POST, PUT, DELETE, OPTIONS"
        );
    }
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/drm/logout")
        .header(header::HOST, HOST)
        .header(header::ORIGIN, "https://attacker.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_text(resp).await, "forbidden origin\n");
}

// Without a DRM layer the session can never become ready, which the rest of the status cannot
// tell apart from "not ready yet"; the page uses the flag to stop offering lossless.
#[tokio::test]
async fn drm_availability_is_reported_for_a_build_without_drm() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&Arc::new(engine(dir.path(), "http://127.0.0.1:9").await));
    let status = body_json(call(&app, "GET", "/api/v1/drm/status", "").await).await;
    assert_eq!(status["available"], false);
    assert_eq!(status["backend"], json!({"selected": ""}));
    assert_eq!(status["state"]["process"], "unknown");
    assert!(status.get("readySinceMs").is_none());

    let caps = body_json(call(&app, "GET", "/api/v1/capabilities", "").await).await;
    assert_eq!(caps["lossless"], false);
    assert_eq!(caps["drm"]["available"], false);
    assert_eq!(caps["musicVideo"], true);

    let resp = call(&app, "POST", "/api/v1/drm/logout", "").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let resp = call(
        &app,
        "POST",
        "/api/v1/drm/authenticate",
        r#"{"email":"a","password":"b"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn metrics_shape() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(engine(dir.path(), "http://127.0.0.1:9").await);
    for ms in [10, 20, 30] {
        engine.open_latency.record(ms);
    }
    let m = body_json(call(&app(&engine), "GET", "/api/v1/metrics", "").await).await;
    assert_eq!(m["sessionOpen"]["count"], 3);
    assert_eq!(m["sessionOpen"]["avgMs"], 20.0);
    assert_eq!(m["sessionOpen"]["p95Ms"], 20.0);
    assert_eq!(m["sessionOpen"]["samples"], json!([10, 20, 30]));
    assert_eq!(m["circuitBreaker"]["state"], "closed");
}

// ── SSE ───────────────────────────────────────────────────────────────────────

/// Reads SSE frames from a streaming body until `n` frames arrived.
async fn frames(body: &mut Body, n: usize) -> Vec<(String, Value)> {
    let mut text = String::new();
    while text.matches("\n\n").count() < n {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("frame in time");
        let data = frame.expect("stream open").unwrap().into_data().unwrap();
        text.push_str(std::str::from_utf8(&data).unwrap());
    }
    text.split("\n\n")
        .filter(|f| !f.is_empty())
        .map(|f| {
            let lines: Vec<&str> = f.lines().collect();
            let id = lines[0].strip_prefix("id: ").unwrap();
            let kind = lines[1].strip_prefix("event: ").unwrap().to_owned();
            let data: Value =
                serde_json::from_str(lines[2].strip_prefix("data: ").unwrap()).unwrap();
            assert_eq!(
                data["meta"]["id"].to_string(),
                id,
                "the frame id is the meta id"
            );
            (kind, data)
        })
        .collect()
}

#[tokio::test]
async fn events_start_with_a_snapshot_then_stream_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(engine(dir.path(), "http://127.0.0.1:9").await);
    let app = app(&engine);

    let resp = call(&app, "GET", "/api/v1/events", "").await;
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache");
    let mut body = resp.into_body();
    let snap = frames(&mut body, 1).await;
    assert_eq!(snap[0].0, "engine.snapshot");
    let meta = &snap[0].1["meta"];
    assert_eq!(
        (
            meta["generation"].clone(),
            meta["reason"].clone(),
            meta["snapshot"].clone()
        ),
        (json!(1), json!("engine-start"), json!(true))
    );
    let payload = &snap[0].1["payload"];
    assert_eq!(payload["version"], 1);
    assert_eq!(
        payload["snapshot"]["capabilities"],
        json!({"lossless": false, "hiRes": false, "atmos": false, "musicVideo": true})
    );
    assert_eq!(payload["snapshot"]["drm"]["session"], "unknown");
    assert!(
        payload["snapshot"].get("prefetch").is_none(),
        "no scheduler, no prefetch stats"
    );

    engine.emit("export", &json!({"id": "e1"}));
    let live = frames(&mut body, 1).await;
    assert_eq!(live[0].0, "export");
    assert_eq!(live[0].1["payload"], json!({"id": "e1"}));
    assert!(live[0].1["meta"].get("snapshot").is_none());
    let export_id = live[0].1["meta"]["id"].as_i64().unwrap();
    drop(body);

    // A reconnect replays what it missed after its Last-Event-ID.
    engine.emit("export", &json!({"id": "e2"}));
    let req = Request::builder()
        .uri("/api/v1/events")
        .header(header::HOST, HOST)
        .header("last-event-id", export_id.to_string())
        .body(Body::empty())
        .unwrap();
    let mut body = app.clone().oneshot(req).await.unwrap().into_body();
    let got = frames(&mut body, 2).await;
    assert_eq!(got[0].0, "engine.snapshot");
    assert_eq!(
        (got[1].0.as_str(), &got[1].1["payload"]),
        ("export", &json!({"id": "e2"}))
    );

    // An ID from before a restart: resync.
    let req = Request::builder()
        .uri("/api/v1/events")
        .header(header::HOST, HOST)
        .header("last-event-id", "999999")
        .body(Body::empty())
        .unwrap();
    let mut body = app.clone().oneshot(req).await.unwrap().into_body();
    let got = frames(&mut body, 2).await;
    assert_eq!(got[1].0, "replay.truncated");
    assert_eq!(got[1].1["payload"]["requestedAfter"], 999999);
    assert_eq!(got[1].1["payload"]["reason"], "history_evicted");

    // Shutdown ends the stream.
    engine.shutdown.cancel();
    let end = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(f) = body.frame().await {
            f.unwrap();
        }
    })
    .await;
    assert!(end.is_ok(), "the SSE stream must end on shutdown");
}

// ── DRM with a fake backend ──────────────────────────────────────────────────

/// A backend that never starts; tests push state through the link it is given.
#[derive(Default)]
struct FakeBackend {
    link: Mutex<Option<BackendLink>>,
}

#[async_trait]
impl DrmBackend for FakeBackend {
    fn attach(&self, link: BackendLink) {
        *self.link.lock().unwrap() = Some(link);
    }
    async fn start(
        &self,
        _cfg: BackendConfig,
        _cancel: CancellationToken,
    ) -> Result<(), BackendError> {
        Err(BackendError::Setup("test backend".into()))
    }
    async fn stop(&self) -> Result<(), BackendError> {
        Ok(())
    }
    fn running(&self) -> bool {
        false
    }
    async fn get_m3u8(&self, _: u64) -> Result<String, BackendError> {
        Err(BackendError::NotRunning)
    }
    async fn get_account(&self) -> Result<AccountInfo, BackendError> {
        Err(BackendError::NotRunning)
    }
    async fn get_progressive_mv_url(&self, _: u64) -> Result<ProgressiveMvUrl, BackendError> {
        Err(BackendError::NotRunning)
    }
    async fn decrypt_itun_samples(
        &self,
        _: u64,
        _: Vec<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>, BackendError> {
        Err(BackendError::NotRunning)
    }
    async fn dial_cbcs(&self, _: CancellationToken) -> Result<DuplexStream, BackendError> {
        Err(BackendError::NotRunning)
    }
}

async fn with_drm(dir: &Path) -> (Arc<Engine>, Arc<FakeBackend>) {
    let mut e = engine(dir, "http://127.0.0.1:9").await;
    let backend = Arc::new(FakeBackend::default());
    let session_dir = dir.join("session");
    let manager = DrmManager::new(
        backend.clone(),
        e.tokens.session.clone(),
        BackendConfig {
            base_dir: session_dir.clone(),
            ..BackendConfig::default()
        },
        RestartPolicy::default(),
    )
    .await;
    e.drm = Some(Drm {
        manager,
        backend_name: "rustetplay",
        session_dir,
        eager_start: false,
    });
    let e = Arc::new(e);
    e.start_drm();
    (e, backend)
}

#[tokio::test]
async fn drm_routes_validate_and_report_the_backend() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _backend) = with_drm(dir.path()).await;
    let app = app(&engine);

    let status = body_json(call(&app, "GET", "/api/v1/drm/status", "").await).await;
    assert_eq!(status["available"], true);
    assert_eq!(status["backend"]["selected"], "rustetplay");
    assert_eq!(status["state"]["session"], "empty");

    for (body, want) in [
        ("not json", "invalid request body\n"),
        (r#"{"email":"a"}"#, "email and password required\n"),
        (
            &format!(r#"{{"email":"{}","password":"p"}}"#, "x".repeat(5000)),
            "invalid request body\n",
        ),
    ] {
        let resp = call(&app, "POST", "/api/v1/drm/authenticate", body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body:.40}");
        assert_eq!(body_text(resp).await, want);
    }
    let resp = call(&app, "POST", "/api/v1/drm/challenge", r#"{"reply":""}"#).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // No sign-in waits for a code.
    let resp = call(
        &app,
        "POST",
        "/api/v1/drm/challenge",
        r#"{"reply":"123456"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A sign-in is accepted at once; its outcome arrives through the snapshot.
    let resp = call(
        &app,
        "POST",
        "/api/v1/drm/authenticate",
        r#"{"email":"a@b.c","password":"p"}"#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(body_json(resp).await.get("state").is_some());
}

// A FairPlay-ready snapshot becomes a `drm` SSE event and starts the session-age clock.
#[tokio::test]
async fn drm_snapshots_become_events() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, backend) = with_drm(dir.path()).await;
    let app = app(&engine);
    let mut body = call(&app, "GET", "/api/v1/events", "").await.into_body();
    frames(&mut body, 1).await;

    let events = backend
        .link
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .events
        .clone();
    let state = DrmState {
        fair_play: FairPlayState::Ready,
        ..DrmState::default()
    };
    events
        .send(DrmEvent {
            update: StateUpdate {
                state,
                ..StateUpdate::default()
            },
            intentional: false,
        })
        .await
        .unwrap();

    let got = frames(&mut body, 1).await;
    assert_eq!(got[0].0, "drm");
    assert_eq!(got[0].1["payload"]["state"]["fairplay"], "ready");
    assert_eq!(got[0].1["payload"]["capabilities"]["alac"], true);
    let status = body_json(call(&app, "GET", "/api/v1/drm/status", "").await).await;
    assert!(status["readySinceMs"].as_i64().unwrap() > 0);
    assert_eq!(status["sessionTTLMs"], 24 * 60 * 60 * 1000);
    let caps = body_json(call(&app, "GET", "/api/v1/capabilities", "").await).await;
    assert_eq!(
        (caps["lossless"].clone(), caps["drm"]["fairplay"].clone()),
        (json!(true), json!("ready"))
    );
}

// ── Library ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn library_routes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(engine(dir.path(), "http://127.0.0.1:9").await);
    let app = app(&engine);

    let status = body_json(call(&app, "GET", "/api/v1/library/status", "").await).await;
    assert_eq!(
        status,
        json!({"songs": 0, "albums": 0, "playlists": 0, "syncedAt": "0001-01-01T00:00:00Z", "needsSync": true})
    );

    let ingest = json!({
        "songs": [{"id": "i.1", "attributes": {"name": "S", "playParams": {"catalogId": "100"}},
                   "relationships": {"albums": {"data": [{"id": "l.A"}]}}}],
        "albums": [{"id": "l.A", "attributes": {"name": "A"}}],
        "playlists": [{"id": "p.1", "attributes": {"name": "P"}}],
        "playlistTracks": {"p.1": [{"id": "i.1", "attributes": {"playParams": {"catalogId": "100"}}}]},
    });
    let resp = call(&app, "POST", "/api/v1/library/ingest", &ingest.to_string()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let stats = body_json(resp).await;
    assert_eq!(
        (stats["songs"].clone(), stats["playlists"].clone()),
        (json!(1), json!(1))
    );

    // A body that does not decode is refused and wipes nothing.
    let resp = call(&app, "POST", "/api/v1/library/ingest", r#"{"songs": 5}"#).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(resp).await.starts_with("bad request: "));
    assert_eq!(
        body_json(call(&app, "GET", "/api/v1/library/status", "").await).await["songs"],
        1
    );

    let pl = body_json(call(&app, "GET", "/api/v1/library/playlists/p.1/tracks", "").await).await;
    assert_eq!(
        pl,
        json!({"tracks": [{"lid": "i.1", "cid": "100"}], "cached": true})
    );
    let resp = call(&app, "GET", "/api/v1/library/playlists/..%2Fx/tracks", "").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Not cached and no user token: an empty answer, no fetch.
    let pl = body_json(call(&app, "GET", "/api/v1/library/playlists/p.2/tracks", "").await).await;
    assert_eq!(pl, json!({"tracks": [], "cached": false}));

    // The `l.` prefix is added when missing.
    let a = body_json(call(&app, "GET", "/api/v1/library/albums/A/tracks", "").await).await;
    let b = body_json(call(&app, "GET", "/api/v1/library/albums/l.A/tracks", "").await).await;
    assert_eq!(a, b);
    assert_eq!(a["cached"], true);

    let playlists = body_json(call(&app, "GET", "/api/v1/library/playlists", "").await).await;
    assert_eq!(playlists["playlists"][0]["lid"], "p.1");

    assert_eq!(
        call(&app, "POST", "/api/v1/library/sync", "")
            .await
            .status(),
        StatusCode::GONE
    );

    let resp = call(
        &app,
        "POST",
        "/api/v1/library/token",
        r#"{"musicUserToken":"mut","developerToken":"dev"}"#,
    )
    .await;
    assert_eq!(body_json(resp).await, json!({"ok": true}));
    assert_eq!(
        (engine.tokens.developer(), engine.tokens.music_user_token()),
        ("dev".into(), "mut".into())
    );
    assert_eq!(
        call(&app, "POST", "/api/v1/library/token", "[")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn library_routes_without_a_store_are_503() {
    let dir = tempfile::tempdir().unwrap();
    let mut e = engine(dir.path(), "http://127.0.0.1:9").await;
    e.library = None;
    let app = app(&Arc::new(e));
    for (m, uri) in [
        ("GET", "/api/v1/library/status"),
        ("POST", "/api/v1/library/ingest"),
        ("GET", "/api/v1/library/albums/1/tracks"),
    ] {
        assert_eq!(
            call(&app, m, uri, "{}").await.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{uri}"
        );
    }
}

// ── Catalog and metadata validation ──────────────────────────────────────────

#[tokio::test]
async fn catalog_params_validation() {
    let apple = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&apple)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = app(&Arc::new(engine(dir.path(), &apple.uri()).await));
    for (id, sf, ok) in [
        ("1440833098", "us", true),
        ("pl.u-abc_1", "gb", true),
        ("l.AbC.1", "", true),
        ("..", "us", false),
        ("1", "us%2F..%2F..%2Fv1%2Fme", false),
        ("a%3Fb", "us", false),
        ("1", "..", false),
        ("1", ".", false),
    ] {
        let uri = format!("/api/v1/catalog/artists/{id}?sf={sf}");
        let resp = call(&app, "GET", &uri, "").await;
        assert_eq!(
            resp.status() != StatusCode::BAD_REQUEST,
            ok,
            "id={id} sf={sf}: {}",
            resp.status()
        );
    }
}

#[tokio::test]
async fn lyrics_type_validation() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&Arc::new(engine(dir.path(), "http://127.0.0.1:9").await));
    for t in ["..%2F..%2Fme%2Flibrary", "a%2Fb"] {
        let resp = call(
            &app,
            "GET",
            &format!("/api/v1/lyrics/1440833098?sf=us&type={t}"),
            "",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{t}");
    }
    // A valid request reaches Apple's API (unreachable here) and fails with 404, not 400.
    let resp = call(
        &app,
        "GET",
        "/api/v1/lyrics/1440833098?sf=us&type=syllable-lyrics",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metadata_tries_song_then_video_then_album() {
    let apple = MockServer::start().await;
    Mock::given(path("/v1/catalog/us/music-videos/7"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":[{"attributes":{"name":"V","artistName":"A","durationInMillis":1000,"has4K":true,"artwork":{"url":"https://x/{w}x{h}.jpg"}}}]}"#,
        ))
        .mount(&apple)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&apple)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(engine(dir.path(), &apple.uri()).await);
    engine.tokens.set_developer("dev");
    let app = app(&engine);
    let meta = body_json(call(&app, "GET", "/api/v1/metadata/7?sf=us", "").await).await;
    assert_eq!(
        meta,
        json!({"id": "7", "type": "mv", "title": "V", "artistName": "A", "durationMs": 1000,
               "artworkUrl": "https://x/500x500.jpg", "has4k": true,
               "availableStreams": [{"codec": "H.264"}, {"codec": "AAC"}]})
    );
    assert_eq!(
        call(&app, "GET", "/api/v1/metadata/8?sf=us", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

// ── Recommendations ───────────────────────────────────────────────────────────

async fn feed_engine(apple: &MockServer, dir: &Path) -> Router {
    let e = Arc::new(engine(dir, &apple.uri()).await);
    e.tokens.set_developer("dev-token");
    e.tokens.set_music_kit("user-token");
    app(&e)
}

#[tokio::test]
async fn recommendations_proxy_and_send_tokens() {
    let apple = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[{"id":"x"}]}"#))
        .mount(&apple)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let app = feed_engine(&apple, dir.path()).await;

    let resp = call(
        &app,
        "GET",
        "/api/v1/recommendations?limit=5&offset=2&types=playlists,albums",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_text(resp).await, r#"{"data":[{"id":"x"}]}"#);
    let reqs = apple.received_requests().await.unwrap();
    let r = &reqs[0];
    assert_eq!(r.headers["authorization"], "Bearer dev-token");
    assert_eq!(r.headers["music-user-token"], "user-token");
    assert_eq!(
        format!("{}?{}", r.url.path(), r.url.query().unwrap()),
        "/v1/me/recommendations?l=en-US&limit=5&offset=2&types=playlists%2Calbums"
    );

    for (uri, want) in [
        (
            "/api/v1/recommendations/heavy-rotation",
            "/v1/me/history/heavy-rotation?l=en-US",
        ),
        (
            "/api/v1/recommendations/recently-played",
            "/v1/me/recent/played?l=en-US",
        ),
    ] {
        apple.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&apple)
            .await;
        assert_eq!(call(&app, "GET", uri, "").await.status(), StatusCode::OK);
        let r = &apple.received_requests().await.unwrap()[0];
        assert_eq!(format!("{}?{}", r.url.path(), r.url.query().unwrap()), want);
    }
}

#[tokio::test]
async fn recommendations_validation_auth_and_upstream_errors() {
    let apple = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let app = feed_engine(&apple, dir.path()).await;
    for q in [
        "limit=0",
        "limit=51",
        "limit=abc",
        "offset=-1",
        "types=../x",
        "types=a,b,c,d,e,f,g,h,i,j,k",
    ] {
        let resp = call(&app, "GET", &format!("/api/v1/recommendations?{q}"), "").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{q}");
    }

    let no_user = Arc::new(engine(dir.path(), &apple.uri()).await);
    no_user.tokens.set_developer("dev-token");
    assert_eq!(
        call(&self::app(&no_user), "GET", "/api/v1/recommendations", "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    for (upstream, want) in [(401, 401), (403, 403), (500, 502), (429, 502)] {
        apple.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(upstream).set_body_string(r#"{"errors":[]}"#))
            .mount(&apple)
            .await;
        let resp = call(&app, "GET", "/api/v1/recommendations", "").await;
        assert_eq!(resp.status().as_u16(), want, "upstream {upstream}");
    }
}

// ── Cache and jobs ────────────────────────────────────────────────────────────

/// A provider that never opens anything.
struct NoProvider;

#[async_trait]
impl Provider for NoProvider {
    async fn open(&self, _: &CancellationToken, _: &OpenRequest) -> Result<Session, PipelineError> {
        Err(PipelineError::Io(std::io::Error::other(
            "no provider in tests",
        )))
    }
}

#[tokio::test]
async fn cache_and_job_routes() {
    let dir = tempfile::tempdir().unwrap();
    let app_without = app(&Arc::new(engine(dir.path(), "http://127.0.0.1:9").await));
    assert_eq!(
        call(&app_without, "GET", "/api/v1/cache/stats", "")
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );

    let mut e = engine(dir.path(), "http://127.0.0.1:9").await;
    let manager = rustet_playback::Manager::new(Arc::new(NoProvider));
    e.playback = Some(Playback::new(
        manager,
        &e.tokens,
        e.epoch.clone(),
        e.events.clone(),
    ));
    let engine = Arc::new(e);
    let app = app(&engine);

    let stats = body_json(call(&app, "GET", "/api/v1/cache/stats", "").await).await;
    assert_eq!(
        stats["prewarm"],
        json!({"entries": 0, "sizeBytes": 0, "limitBytes": 1024 * 1024 * 1024})
    );
    assert_eq!(stats["persistent"]["available"], true);
    assert_eq!(stats["persistent"]["limitBytes"], 500 * 1024 * 1024);

    for body in ["", r#"{"prewarmLimitMB":-1}"#, r#"{"persistTTLDays":1.5}"#] {
        assert_eq!(
            call(&app, "PUT", "/api/v1/cache/config", body)
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    let cfg = body_json(
        call(
            &app,
            "PUT",
            "/api/v1/cache/config",
            r#"{"persistLimitMB":100,"persistTTLDays":7}"#,
        )
        .await,
    )
    .await;
    assert_eq!(cfg, json!({"persistLimitMB": 100, "persistTTLDays": 7}));
    let stats = body_json(call(&app, "GET", "/api/v1/cache/stats", "").await).await;
    assert_eq!(
        (
            stats["persistent"]["limitBytes"].clone(),
            stats["persistent"]["ttlDays"].clone()
        ),
        (json!(100 * 1024 * 1024), json!(7))
    );

    assert_eq!(
        call(&app, "GET", "/api/v1/jobs/nope", "").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", "/api/v1/jobs/nope", "").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", "/api/v1/cache/playback?what=persistent", "")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "DELETE", "/api/v1/cache/playback", "")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );

    let rt = body_json(call(&app, "GET", "/api/v1/debug/runtime", "").await).await;
    assert!(rt["tasks"].is_u64() && rt["prefetch"]["queueDepth"].is_u64());
    engine.playback.as_ref().unwrap().scheduler.stop().await;
}

// ── The real server ───────────────────────────────────────────────────────────

fn identity() -> crate::tls::TlsIdentity {
    crate::tls::TlsIdentity::generate().unwrap()
}

fn tls_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}

#[tokio::test]
async fn serves_https_with_h2_and_http1_and_stops_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut e = engine(dir.path(), "http://127.0.0.1:9").await;
    e.guard = Arc::new(GuardConfig {
        port,
        allow_local_origins: false,
        api_token: None,
    });
    let engine = Arc::new(e);
    let server = crate::server::serve(
        engine.clone(),
        app(&engine),
        listener,
        crate::peercred::PeerGuard::Off,
        None,
        &identity(),
    )
    .unwrap();

    let url = format!("https://127.0.0.1:{port}/api/v1/status");
    let resp = tls_client().get(&url).send().await.unwrap();
    assert_eq!(resp.version(), reqwest::Version::HTTP_2, "ALPN offers h2");
    assert_eq!(resp.text().await.unwrap(), "{\"status\":\"ok\"}\n");
    let h1 = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http1_only()
        .build()
        .unwrap();
    let resp = h1.get(&url).send().await.unwrap();
    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // An open SSE stream must not hold up the shutdown.
    let mut sse = tls_client()
        .get(format!("https://127.0.0.1:{port}/api/v1/events"))
        .send()
        .await
        .unwrap();
    assert!(sse.chunk().await.unwrap().is_some());
    tokio::time::timeout(Duration::from_secs(3), server.stop())
        .await
        .expect("stops within the limit");
    assert!(
        tls_client().get(&url).send().await.is_err(),
        "no longer accepting"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_connections_from_other_users() {
    if rustix::process::geteuid().is_root() {
        return; // root is always admitted
    }
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let engine = Arc::new(engine(dir.path(), "http://127.0.0.1:9").await);
    let other = crate::peercred::PeerGuard::ProcNet {
        euid: rustix::process::geteuid().as_raw() + 1,
    };
    let server = crate::server::serve(
        engine.clone(),
        app(&engine),
        listener,
        other,
        None,
        &identity(),
    )
    .unwrap();
    let err = tls_client()
        .get(format!("https://127.0.0.1:{port}/api/v1/status"))
        .send()
        .await;
    assert!(err.is_err(), "a refused connection is closed before TLS");
    server.stop().await;
}
