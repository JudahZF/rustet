//! Playback, vseg, VLC, export, music-video cache and TLS tests through the router, with a fake
//! provider whose tracks stream fixture bytes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use rustet_core::media::{Metadata, OpenRequest, OpenTrack, Provider, Session, SessionKind, Track};
use rustet_core::pipeline::{
    Codec, DirectInfo, DirectSeekable, PipelineError, Seekable, Sink, Source, Stream,
    StreamContext, StreamKind,
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::{HOST, app, body_json, body_text, call, engine};
use crate::state::{Engine, Playback};

// ── Fakes ─────────────────────────────────────────────────────────────────────

/// What a fake audio source writes.
#[derive(Clone, Default)]
pub(super) struct Script {
    pub data: Vec<u8>,
    /// Write this many bytes, then wait until `release` turns true.
    pub pause_at: Option<(usize, watch::Receiver<bool>)>,
    /// Fail after writing everything.
    pub fail: Option<&'static str>,
}

#[async_trait]
impl Source for Script {
    async fn stream(&self, _: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        let split = self
            .pause_at
            .as_ref()
            .map_or(self.data.len(), |(at, _)| *at);
        w.write_all(&self.data[..split]).await?;
        if let Some((_, release)) = &self.pause_at {
            let _ = release.clone().wait_for(|r| *r).await;
        }
        w.write_all(&self.data[split..]).await?;
        match self.fail {
            Some(msg) => Err(PipelineError::Other(msg.into())),
            None => Ok(()),
        }
    }

    fn seekable(&self) -> Option<&dyn Seekable> {
        Some(self)
    }
}

impl Seekable for Script {
    /// The whole data again, from the "segment" at the whole second.
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64) {
        (Arc::new(self.clone()), start_sec.floor())
    }
}

/// A clear fMP4 video: an init segment and one-second fragments on a raw timeline from 10 s.
#[derive(Clone)]
struct Video {
    init: Vec<u8>,
    frags: Vec<Vec<u8>>,
    direct_ok: bool,
    /// Fragments start at this index.
    from: usize,
}

/// The fixture split into top-level moof+mdat pairs.
fn video_fixture() -> Video {
    let mut media = include_bytes!("../../tests/data/direct_seg.m4s").to_vec();
    media.extend_from_slice(include_bytes!("../../tests/data/direct_next.m4s"));
    let mut frags = Vec::new();
    let mut i = 0;
    while i < media.len() {
        let size = |at: usize| u32::from_be_bytes(media[at..at + 4].try_into().unwrap()) as usize;
        let (moof, mdat) = (size(i), size(i + size(i)));
        frags.push(media[i..i + moof + mdat].to_vec());
        i += moof + mdat;
    }
    assert_eq!(frags.len(), 7);
    Video {
        init: include_bytes!("../../tests/data/direct_init.mp4").to_vec(),
        frags,
        direct_ok: true,
        from: 0,
    }
}

impl Video {
    fn starting(&self, sec: f64) -> Arc<dyn Source> {
        Arc::new(Self {
            from: sec as usize,
            ..self.clone()
        })
    }
}

#[async_trait]
impl Source for Video {
    async fn stream(&self, _: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        w.write_all(&self.init).await?;
        for f in &self.frags[self.from..] {
            w.write_all(f).await?;
        }
        Ok(())
    }

    fn seekable(&self) -> Option<&dyn Seekable> {
        Some(self)
    }
}

impl Seekable for Video {
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64) {
        (self.starting(start_sec), start_sec.floor())
    }

    fn direct_seekable(&self) -> Option<&dyn DirectSeekable> {
        Some(self)
    }
}

#[async_trait]
impl DirectSeekable for Video {
    async fn direct_source_from(
        &self,
        _: &CancellationToken,
        start_sec: f64,
    ) -> Result<(Arc<dyn Source>, DirectInfo), PipelineError> {
        if !self.direct_ok {
            return Err(PipelineError::Io(std::io::ErrorKind::UnexpectedEof.into()));
        }
        Ok((
            self.starting(start_sec),
            DirectInfo {
                start_sec: start_sec.floor(),
                ts_offset: -10.0,
            },
        ))
    }
}

struct Ready(Stream);

#[async_trait]
impl OpenTrack for Ready {
    async fn open(&self, _: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(self.0.clone())
    }
}

/// Opens every asset as one session of `kind` with one track.
pub(super) struct Fake {
    kind: SessionKind,
    track: StreamKind,
    codec: Codec,
    source: Arc<dyn Source>,
}

#[async_trait]
impl Provider for Fake {
    async fn open(&self, _: &CancellationToken, _: &OpenRequest) -> Result<Session, PipelineError> {
        let stream = Stream {
            source: self.source.clone(),
            stages: Vec::new(),
            kind: self.track,
            codec: self.codec,
            duration_ms: 0,
            codec_string: None,
        };
        Ok(Session {
            kind: self.kind,
            metadata: Metadata {
                title: "T".into(),
                ..Metadata::default()
            },
            tracks: vec![Track {
                kind: self.track,
                codec: self.codec,
                codec_string: if self.track == StreamKind::Video {
                    "avc1.640028".into()
                } else {
                    String::new()
                },
                sample_rate: 0,
                bit_depth: 0,
                bit_rate: 0,
                channel_count: 0,
                codec_mime_type: String::new(),
                spatial_audio: None,
                open: Arc::new(Ready(stream)),
            }],
            video_heights: Vec::new(),
            mv_progressive_url: None,
            mv_download_key: None,
        })
    }
}

pub(super) fn song(codec: Codec, script: Script) -> Fake {
    Fake {
        kind: SessionKind::Song,
        track: StreamKind::Audio,
        codec,
        source: Arc::new(script),
    }
}

fn music_video_audio(script: Script) -> Fake {
    Fake {
        kind: SessionKind::MusicVideo,
        track: StreamKind::Audio,
        codec: Codec::Aac,
        source: Arc::new(script),
    }
}

fn music_video(video: Video) -> Fake {
    Fake {
        kind: SessionKind::MusicVideo,
        track: StreamKind::Video,
        codec: Codec::H264,
        source: Arc::new(video),
    }
}

struct Failing;

#[async_trait]
impl Provider for Failing {
    async fn open(&self, _: &CancellationToken, _: &OpenRequest) -> Result<Session, PipelineError> {
        Err(PipelineError::Io(std::io::Error::other(
            "connection reset by peer",
        )))
    }
}

pub(super) async fn with_provider(dir: &Path, provider: impl Provider + 'static) -> Arc<Engine> {
    let mut e = engine(dir, "http://127.0.0.1:9").await;
    let manager = rustet_playback::Manager::new(Arc::new(provider));
    e.playback = Some(Playback::new(
        manager.clone(),
        &e.tokens,
        e.epoch.clone(),
        e.events.clone(),
    ));
    let opts = rustet_export::Options {
        output_roots: vec![dir.to_owned()],
        ..rustet_export::Options::default()
    };
    e.export = Some(rustet_export::Manager::new(manager, e.http.clone(), opts));
    let e = Arc::new(e);
    e.install_release_hook();
    e
}

async fn open_session(app: &Router, body: Value) -> String {
    let mut body = body;
    body["token"] = json!("dev");
    body["mediaUserToken"] = json!("mut");
    let resp = call(app, "POST", "/api/v1/playback", &body.to_string()).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn get(app: &Router, uri: &str, range: Option<&str>) -> Response {
    let mut req = Request::builder().uri(uri).header(header::HOST, HOST);
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn bytes(resp: Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

pub(super) fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

/// Waits until the disk cache has committed the entry.
async fn committed(e: &Engine, asset: &str, q: &str) {
    for _ in 0..200 {
        if e.disk_cache
            .as_ref()
            .unwrap()
            .path(asset, q)
            .await
            .is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{asset}-{q} never committed");
}

// ── Create ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_validates_the_request_and_needs_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&with_provider(dir.path(), song(Codec::Aac, Script::default())).await);
    for (body, status, text) in [
        ("not json", 400, "invalid request body\n"),
        (
            &format!(r#"{{"assetId":"{}"}}"#, "x".repeat(70 << 10)),
            400,
            "invalid request body\n",
        ),
        ("{}", 400, "assetId is required\n"),
        (
            r#"{"assetId":"1"}"#,
            401,
            "not authenticated — provide token+mediaUserToken in request body or configure them\n",
        ),
    ] {
        let resp = call(&app, "POST", "/api/v1/playback", body).await;
        assert_eq!(resp.status().as_u16(), status, "{body:.30}");
        assert_eq!(body_text(resp).await, text);
    }
}

#[tokio::test]
async fn create_returns_the_session_and_announces_it() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(
        dir.path(),
        song(
            Codec::Aac,
            Script {
                data: data(10),
                ..Script::default()
            },
        ),
    )
    .await;
    let app = app(&e);
    let mut sub = e.events.subscribe(None);
    let body =
        json!({"assetId": "1440", "storefront": "us", "token": "dev", "mediaUserToken": "mut"});
    let resp = call(&app, "POST", "/api/v1/playback", &body.to_string()).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
    let sess = body_json(resp).await;
    let id = sess["sessionId"].as_str().unwrap();
    assert_eq!(
        (sess["type"].clone(), sess["codec"].clone()),
        (json!("song"), json!("aac"))
    );
    assert_eq!(
        sess["streams"]["audio"],
        format!("/api/v1/playback/{id}/audio")
    );
    assert_eq!(sess["capabilities"]["seekable"], true);

    let ev = sub.events.recv().await.unwrap();
    assert_eq!(ev.kind, "playback.created");
    assert_eq!(
        serde_json::from_str::<Value>(&ev.payload).unwrap(),
        json!({"sessionId": id, "assetId": "1440", "codec": "aac"})
    );
    // The request's tokens are cached for the routes that have none.
    assert_eq!(
        (e.tokens.developer(), e.tokens.music_user_token()),
        ("dev".into(), "mut".into())
    );
    assert_eq!(e.open_latency.snapshot().len(), 1);
}

#[tokio::test]
async fn failed_opens_are_500_and_an_open_breaker_is_503() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(dir.path(), Failing).await;
    let app = app(&e);
    let body = r#"{"assetId":"1","token":"dev","mediaUserToken":"mut"}"#;
    let resp = call(&app, "POST", "/api/v1/playback", body).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_text(resp).await,
        "playback resolution failed: connection reset by peer\n"
    );
    assert_eq!(
        e.open_breaker.state(),
        crate::breaker::BreakerState::Closed,
        "an unclassified failure is not an outage"
    );

    for _ in 0..3 {
        e.open_breaker.record_failure();
    }
    let resp = call(&app, "POST", "/api/v1/playback", body).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_text(resp).await,
        "playback resolution failed: Apple servers appear unreachable (circuit open)\n"
    );
}

// ── Audio ────────────────────────────────────────────────────────────────────

// Headers wait for the first byte, so a pipeline that fails before it is a useful 502 instead
// of an empty 200.
#[tokio::test]
async fn music_video_audio_defers_headers_until_the_first_byte() {
    for (name, script, status, ctype, want) in [
        (
            "failure before bytes",
            Script {
                fail: Some("upstream failed"),
                ..Script::default()
            },
            502,
            "application/json",
            "{\"error\":\"upstream failed\"}\n".as_bytes().to_vec(),
        ),
        (
            "successful stream",
            Script {
                data: b"audio".to_vec(),
                ..Script::default()
            },
            200,
            "audio/mp4",
            b"audio".to_vec(),
        ),
        (
            "failure after bytes",
            Script {
                data: b"partial".to_vec(),
                fail: Some("upstream failed"),
                ..Script::default()
            },
            200,
            "audio/mp4",
            b"partial".to_vec(),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let app = app(&with_provider(dir.path(), music_video_audio(script)).await);
        let id = open_session(
            &app,
            json!({"assetId": "mv", "capabilities": {"video": true}}),
        )
        .await;
        let resp = get(&app, &format!("/api/v1/playback/{id}/audio"), None).await;
        assert_eq!(resp.status().as_u16(), status, "{name}");
        assert_eq!(resp.headers()[header::CONTENT_TYPE], ctype, "{name}");
        if status == 200 {
            assert_eq!(resp.headers()[header::ACCEPT_RANGES], "none", "{name}");
        }
        assert_eq!(bytes(resp).await, want, "{name}");
    }
}

#[tokio::test]
async fn audio_of_an_unknown_session_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&with_provider(dir.path(), song(Codec::Aac, Script::default())).await);
    let resp = get(&app, "/api/v1/playback/nope/audio", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_text(resp).await, "session not found or expired\n");
}

// The first play streams while the cache fills; replays come from the file with byte ranges.
#[tokio::test]
async fn song_audio_streams_while_caching_then_serves_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let payload = data(100_000);
    let e = with_provider(
        dir.path(),
        song(
            Codec::Aac,
            Script {
                data: payload.clone(),
                ..Script::default()
            },
        ),
    )
    .await;
    let app = app(&e);
    let id = open_session(&app, json!({"assetId": "42"})).await;
    let uri = format!("/api/v1/playback/{id}/audio");

    let resp = get(&app, &uri, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "audio/mp4");
    assert_eq!(bytes(resp).await, payload);
    committed(&e, "42", "aac").await;

    let resp = get(&app, &uri, None).await;
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "100000");
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(bytes(resp).await, payload);

    let resp = get(&app, &uri, Some("bytes=10-19")).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 10-19/100000");
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "10");
    assert_eq!(bytes(resp).await, payload[10..20]);

    let resp = get(&app, &uri, Some("bytes=200000-")).await;
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes */100000");
}

#[tokio::test]
async fn a_download_that_fails_before_any_byte_is_502() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&with_provider(
        dir.path(),
        song(
            Codec::Aac,
            Script {
                fail: Some("cdn"),
                ..Script::default()
            },
        ),
    )
    .await);
    let id = open_session(&app, json!({"assetId": "7"})).await;
    let resp = get(&app, &format!("/api/v1/playback/{id}/audio"), None).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        body_text(resp).await,
        "{\"error\":\"streaming cache download failed or was abandoned\"}\n"
    );
}

// A VLC seek during the first lossless play: 206 from the in-progress file when the offset is
// written, 416 at once when it is not.
#[tokio::test]
async fn lossless_ranges_during_the_download() {
    let dir = tempfile::tempdir().unwrap();
    let payload = data(5000);
    let (release, rx) = watch::channel(false);
    let script = Script {
        data: payload.clone(),
        pause_at: Some((1000, rx)),
        ..Script::default()
    };
    let e = with_provider(dir.path(), song(Codec::Alac, script)).await;
    let app = app(&e);
    let id = open_session(
        &app,
        json!({"assetId": "9", "capabilities": {"lossless": true}}),
    )
    .await;
    let uri = format!("/api/v1/playback/{id}/audio");

    let first = get(&app, &uri, None).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()[header::ACCEPT_RANGES], "bytes");
    let entry = e
        .disk_cache
        .as_ref()
        .unwrap()
        .get_streaming("9", "alac")
        .unwrap();
    for _ in 0..200 {
        if entry.written() == 1000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(entry.written(), 1000);

    let resp = get(&app, &uri, Some("bytes=3000-")).await;
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes */1000");

    let resp = get(&app, &uri, Some("bytes=500-")).await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers()[header::CONTENT_RANGE],
        "bytes 500-1073741823/1073741824"
    );
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "audio/mp4");
    release.send_replace(true);
    assert_eq!(bytes(resp).await, payload[500..]);
    assert_eq!(bytes(first).await, payload);
}

#[tokio::test]
async fn seeks_report_the_actual_start() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&with_provider(
        dir.path(),
        song(
            Codec::Aac,
            Script {
                data: data(64),
                ..Script::default()
            },
        ),
    )
    .await);
    let id = open_session(&app, json!({"assetId": "5"})).await;
    let resp = get(&app, &format!("/api/v1/playback/{id}/audio?t=2.5"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-actual-start"], "2.000");
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "none");
    assert_eq!(bytes(resp).await, data(64));
}

// ── Session upkeep ───────────────────────────────────────────────────────────

#[tokio::test]
async fn touch_precache_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(
        dir.path(),
        song(
            Codec::Alac,
            Script {
                data: data(3000),
                ..Script::default()
            },
        ),
    )
    .await;
    let app = app(&e);
    let id = open_session(
        &app,
        json!({"assetId": "11", "capabilities": {"lossless": true}}),
    )
    .await;

    assert_eq!(
        call(&app, "POST", &format!("/api/v1/playback/{id}/touch"), "")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, "POST", "/api/v1/playback/nope/touch", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "POST", "/api/v1/playback/nope/precache", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    let resp = call(&app, "POST", &format!("/api/v1/playback/{id}/precache"), "").await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    committed(&e, "11", "alac").await;
    let resp = call(&app, "POST", &format!("/api/v1/playback/{id}/precache"), "").await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "already cached");

    let mut sub = e.events.subscribe(None);
    assert_eq!(
        call(&app, "DELETE", &format!("/api/v1/playback/{id}"), "")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let ev = sub.events.recv().await.unwrap();
    assert_eq!(
        (ev.kind, &*ev.payload),
        (
            "playback.deleted",
            format!(r#"{{"sessionId":"{id}"}}"#).as_str()
        )
    );
    // A released session is gone: the renderer reopens the track.
    assert_eq!(
        call(&app, "POST", &format!("/api/v1/playback/{id}/touch"), "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn context_submits_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&with_provider(dir.path(), song(Codec::Aac, Script::default())).await);
    assert_eq!(
        call(&app, "PUT", "/api/v1/playback/context", "[")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let resp = call(&app, "PUT", "/api/v1/playback/context", r#"{"tracks":[]}"#).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let job = body_json(resp).await;
    assert_eq!(job["jobId"].as_str().unwrap().len(), 16);
}

// ── Segmented video ──────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct SeekAnswer {
    n: usize,
    t: f64,
    #[serde(default)]
    direct: bool,
    #[serde(default)]
    ts_offset: f64,
    reinit: bool,
}

async fn vseek(app: &Router, id: &str, t: &str) -> SeekAnswer {
    let resp = get(app, &format!("/api/v1/playback/{id}/vseg/seek?t={t}"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    serde_json::from_slice(&bytes(resp).await).unwrap()
}

#[tokio::test]
async fn direct_seek_serves_fragments_without_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    let fx = video_fixture();
    let app = app(&with_provider(dir.path(), music_video(fx.clone())).await);
    let id = open_session(
        &app,
        json!({"assetId": "mv-test", "capabilities": {"video": true}}),
    )
    .await;

    let ans = vseek(&app, &id, "2.4").await;
    assert_eq!(
        ans,
        SeekAnswer {
            n: 0,
            t: 2.0,
            direct: true,
            ts_offset: -10.0,
            reinit: true
        }
    );

    let resp = get(&app, &format!("/api/v1/playback/{id}/vseg/init"), None).await;
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(
        resp.headers()[header::CONTENT_LENGTH],
        fx.init.len().to_string().as_str()
    );
    assert_eq!(bytes(resp).await, fx.init, "the decrypted init");
    for i in 0..5 {
        let resp = get(&app, &format!("/api/v1/playback/{id}/vseg/seg/{i}"), None).await;
        assert_eq!(resp.status(), StatusCode::OK, "seg/{i}");
        assert_eq!(
            bytes(resp).await,
            fx.frags[2 + i],
            "seg/{i} is fixture fragment {}",
            2 + i
        );
    }
    // Past the end: 404, the player's end-of-stream signal.
    let resp = get(&app, &format!("/api/v1/playback/{id}/vseg/seg/5"), None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let manifest =
        body_json(get(&app, &format!("/api/v1/playback/{id}/vseg/manifest"), None).await).await;
    assert_eq!(manifest["timescale"], 24);
    assert_eq!(manifest["done"], true);
    assert_eq!(manifest["frags"].as_array().unwrap().len(), 5);
    assert_eq!(manifest["frags"][1], json!({"t": 1.0, "n": 1}));

    // A second direct seek keeps the init kind: no re-append.
    let ans = vseek(&app, &id, "4.1").await;
    assert_eq!(
        ans,
        SeekAnswer {
            n: 0,
            t: 4.0,
            direct: true,
            ts_offset: -10.0,
            reinit: false
        }
    );
    let resp = get(&app, &format!("/api/v1/playback/{id}/vseg/seg/0"), None).await;
    assert_eq!(
        bytes(resp).await,
        fx.frags[4],
        "the second direct session starts at fragment 4"
    );
}

#[tokio::test]
async fn vseg_routes_validate() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(dir.path(), music_video(video_fixture())).await;
    let app = app(&e);
    let id = open_session(
        &app,
        json!({"assetId": "mv", "capabilities": {"video": true}}),
    )
    .await;

    // Before any producer: an empty but valid manifest.
    let resp = get(&app, &format!("/api/v1/playback/{id}/vseg/manifest"), None).await;
    // Compared as JSON: the key order depends on a serde_json feature other crates turn on.
    let index: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(
        index,
        json!({"codecs": "avc1.640028", "done": false, "err": false, "frags": [], "timescale": 0})
    );
    for t in ["", "-1", "NaN", "Inf"] {
        let resp = get(
            &app,
            &format!("/api/v1/playback/{id}/vseg/seek?t={t}"),
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "t={t}");
        assert_eq!(body_text(resp).await, "invalid t parameter\n");
    }
    let resp = get(&app, &format!("/api/v1/playback/{id}/vseg/seg/-1"), None).await;
    assert_eq!(
        (resp.status(), body_text(resp).await.as_str()),
        (StatusCode::BAD_REQUEST, "invalid segment index\n")
    );
    let resp = get(&app, "/api/v1/playback/nope/vseg/init", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = call(&app, "DELETE", &format!("/api/v1/playback/{id}/vseg"), "").await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // A song has no video.
    let dir2 = tempfile::tempdir().unwrap();
    let app2 = super::app(&with_provider(dir2.path(), song(Codec::Aac, Script::default())).await);
    let sid = open_session(&app2, json!({"assetId": "1"})).await;
    let resp = get(&app2, &format!("/api/v1/playback/{sid}/vseg/init"), None).await;
    assert_eq!(
        (resp.status(), body_text(resp).await.as_str()),
        (StatusCode::NOT_FOUND, "no video stream in this session\n")
    );
}

// A session that goes away without a DELETE (expiry, reaper) must still stop its producers:
// they otherwise run on and pin the cache file until the process exits.
#[tokio::test]
async fn session_release_stops_vseg_producers() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(dir.path(), music_video(video_fixture())).await;
    let app = app(&e);
    let id = open_session(
        &app,
        json!({"assetId": "mv", "capabilities": {"video": true}}),
    )
    .await;
    vseek(&app, &id, "1").await;
    let manifest =
        body_json(get(&app, &format!("/api/v1/playback/{id}/vseg/manifest"), None).await).await;
    assert!(manifest["timescale"] != 0, "a producer runs after the seek");
    e.playback.as_ref().unwrap().manager.release(&id); // what the reaper does for an expired session
    // The session is gone, and so are its producers.
    assert!(
        e.vseg.active(&id).is_none(),
        "vseg state survived the session"
    );
}

#[tokio::test]
async fn a_failed_direct_plan_is_reported_for_the_ffmpeg_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(
        dir.path(),
        music_video(Video {
            direct_ok: false,
            ..video_fixture()
        }),
    )
    .await;
    let app = app(&e);
    let id = open_session(
        &app,
        json!({"assetId": "mv", "capabilities": {"video": true}}),
    )
    .await;
    assert!(
        crate::handlers::vseg::start_direct_session(&e, &id, "mv", 1.0)
            .await
            .is_err()
    );
}

// ── VLC, exports, music-video cache, tools ───────────────────────────────────

// Without libvlc every VLC route answers 503, as the Go build without cgo does.
#[tokio::test]
async fn vlc_routes_without_libvlc_are_503() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&with_provider(dir.path(), song(Codec::Aac, Script::default())).await);
    for (m, route) in [
        ("POST", "load"),
        ("POST", "pause"),
        ("POST", "resume"),
        ("GET", "time"),
        ("POST", "seek"),
        ("POST", "stop"),
        ("POST", "volume"),
    ] {
        let resp = call(&app, m, &format!("/api/v1/vlc/{route}"), r#"{"volume":50}"#).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{route}");
        assert_eq!(body_text(resp).await, "libvlc not available\n");
    }
}

#[tokio::test]
async fn export_routes_keep_the_http_contract() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(dir.path(), song(Codec::Aac, Script::default())).await;
    let app = app(&e);
    for body in ["", "{}", r#"{"priority":"high"}"#, "not json"] {
        let resp = call(&app, "POST", "/api/v1/export/x/priority", body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            body_text(resp).await,
            "invalid request body: want {\"priority\": N}\n"
        );
    }
    let resp = call(
        &app,
        "POST",
        "/api/v1/export/nope/priority",
        r#"{"priority":3}"#,
    )
    .await;
    assert_eq!(
        (resp.status(), body_text(resp).await.as_str()),
        (StatusCode::NOT_FOUND, "job not found\n")
    );

    let resp = call(&app, "POST", "/api/v1/export", r#"{"assetId":"1"}"#).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_text(resp).await, "not signed in\n");
    let resp = call(&app, "POST", "/api/v1/export/x/retry", "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(&app, "POST", "/api/v1/export", "[").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        body_text(call(&app, "GET", "/api/v1/export", "").await).await,
        "[]\n"
    );
    assert_eq!(
        call(&app, "GET", "/api/v1/export/nope", "").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", "/api/v1/export/nope", "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // A folder outside the roots is refused with the manager's reason.
    let outside = json!({"assetId": "1", "token": "dev", "mut": "mut", "outputDir": "/"});
    let resp = call(&app, "POST", "/api/v1/export", &outside.to_string()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Queued with tokens: 202 with the job, then listed, then cancelled.
    let ok = json!({"assetId": "1", "token": "dev", "mut": "mut", "outputDir": dir.path()});
    let resp = call(&app, "POST", "/api/v1/export", &ok.to_string()).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let job = body_json(resp).await;
    let jid = job["jobId"].as_str().unwrap();
    assert_eq!(
        call(&app, "GET", &format!("/api/v1/export/{jid}"), "")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "DELETE", &format!("/api/v1/export/{jid}"), "")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    e.export.as_ref().unwrap().stop().await;
}

#[tokio::test]
async fn mv_cache_settings_persist() {
    let dir = tempfile::tempdir().unwrap();
    let e = with_provider(dir.path(), song(Codec::Aac, Script::default())).await;
    let app = app(&e);
    let info = body_json(call(&app, "GET", "/api/v1/cache/mv", "").await).await;
    assert_eq!(
        (info["enabled"].clone(), info["maxBytes"].clone()),
        (json!(true), json!(2_i64 << 30))
    );
    assert!(info["sizeBytes"].is_i64() && info["quality"].is_string());

    let prefs = dir.path().join("mv-prefs.json");
    let info = body_json(call(&app, "PUT", "/api/v1/cache/mv", r#"{"maxBytes":1000}"#).await).await;
    assert_eq!(
        (info["enabled"].clone(), info["maxBytes"].clone()),
        (json!(true), json!(1000))
    );
    assert_eq!(
        std::fs::read_to_string(&prefs).unwrap(),
        r#"{"enabled":true,"maxBytes":1000}"#
    );
    let info = body_json(call(&app, "PUT", "/api/v1/cache/mv", r#"{"enabled":false}"#).await).await;
    assert_eq!(
        (info["enabled"].clone(), info["maxBytes"].clone()),
        (json!(false), json!(0))
    );
    assert_eq!(
        std::fs::read_to_string(&prefs).unwrap(),
        r#"{"enabled":false,"maxBytes":1000}"#
    );

    // The next start applies the saved choice.
    let streams =
        rustet_streams::Streams::with_cache_root(reqwest::Client::new(), &dir.path().join("s2"));
    let reloaded = crate::handlers::mvcache::MvPrefs::load(prefs.clone(), &streams).await;
    assert!(!streams.mv_cache_enabled());
    reloaded.set_enabled(&streams, true).await;
    assert_eq!(
        streams.mv_cache_max_bytes(),
        1000,
        "re-enabling restores the remembered capacity"
    );

    assert_eq!(
        call(&app, "PUT", "/api/v1/cache/mv", "x").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, "DELETE", "/api/v1/cache/mv", "").await.status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn tools_reports_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&with_provider(dir.path(), song(Codec::Aac, Script::default())).await);
    let resp = call(
        &app,
        "GET",
        "/api/v1/tools?ffmpegPath=/nonexistent/ffmpeg",
        "",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_json(resp).await["ffmpeg"]["available"].is_boolean());
}

// ── TLS identity ─────────────────────────────────────────────────────────────

// The shell pins the key of the identity it hands the engine: the server must present exactly
// that certificate.
#[tokio::test]
async fn the_server_presents_the_given_identity() {
    let dir = tempfile::tempdir().unwrap();
    let pem = include_str!("../../tests/data/identity.pem");
    let identity = crate::tls::TlsIdentity::from_pem(pem.as_bytes()).unwrap();
    assert_eq!(
        identity.spki_sha256_base64(),
        include_str!("../../tests/data/identity.spki-sha256").trim()
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut e = engine(dir.path(), "http://127.0.0.1:9").await;
    e.guard = Arc::new(crate::guard::GuardConfig {
        port,
        allow_local_origins: false,
        api_token: None,
    });
    let e = Arc::new(e);
    let server = crate::server::serve(
        e.clone(),
        app(&e),
        listener,
        crate::peercred::PeerGuard::Off,
        None,
        &identity,
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .tls_info(true)
        .build()
        .unwrap();
    let resp = client
        .get(format!("https://127.0.0.1:{port}/api/v1/status"))
        .send()
        .await
        .unwrap();
    let info = resp.extensions().get::<reqwest::tls::TlsInfo>().unwrap();
    assert_eq!(info.peer_certificate().unwrap(), identity.cert_der());
    server.stop().await;
}
