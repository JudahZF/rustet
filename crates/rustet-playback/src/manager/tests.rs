use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use rustet_core::media::{Metadata, OpenTrack, SessionKind};
use rustet_core::pipeline::{BoxError, Codec, Seekable};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

use super::*;

// ── Fakes ───────────────────────────────────────────────────────────────────

fn err(msg: &'static str) -> PipelineError {
    PipelineError::Other(BoxError::from(msg))
}

/// Writes a fixed payload, after an optional gate.
struct Payload {
    bytes: &'static [u8],
    gate: Option<Gate>,
    seekable: bool,
}

/// Lets a test hold a stream mid-flight: the source reports `entered`, then waits for
/// `release`.
#[derive(Clone, Default)]
struct Gate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Source for Payload {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        ctx.check()?;
        if let Some(g) = &self.gate {
            g.entered.notify_one();
            g.release.notified().await;
        }
        w.set_header("Content-Length", &self.bytes.len().to_string());
        w.write_all(self.bytes).await?;
        Ok(())
    }

    fn seekable(&self) -> Option<&dyn Seekable> {
        self.seekable.then_some(self as &dyn Seekable)
    }
}

impl Seekable for Payload {
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64) {
        let from: &'static [u8] = b"SEEK";
        (
            Arc::new(Payload {
                bytes: from,
                gate: None,
                seekable: false,
            }),
            start_sec.floor(),
        )
    }

    fn segment_timings(&self) -> Option<Vec<f64>> {
        Some(vec![0.0, 6.0, 12.0])
    }
}

fn stream(kind: StreamKind, bytes: &'static [u8]) -> Stream {
    Stream {
        source: Arc::new(Payload {
            bytes,
            gate: None,
            seekable: false,
        }),
        stages: Vec::new(),
        kind,
        codec: Codec::Aac,
        duration_ms: 0,
        codec_string: None,
    }
}

/// Opens to a clone of a fixed stream.
struct Ready(Stream);

#[async_trait]
impl OpenTrack for Ready {
    async fn open(&self, _: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(self.0.clone())
    }
}

struct FailsToOpen(&'static str);

#[async_trait]
impl OpenTrack for FailsToOpen {
    async fn open(&self, _: &CancellationToken) -> Result<Stream, PipelineError> {
        Err(err(self.0))
    }
}

fn track(kind: StreamKind, codec: Codec, open: impl OpenTrack + 'static) -> Track {
    Track {
        kind,
        codec,
        codec_string: String::new(),
        sample_rate: 0,
        bit_depth: 0,
        bit_rate: 0,
        channel_count: 0,
        codec_mime_type: String::new(),
        spatial_audio: None,
        open: Arc::new(open),
    }
}

fn audio_track(bytes: &'static [u8]) -> Track {
    track(
        StreamKind::Audio,
        Codec::Aac,
        Ready(stream(StreamKind::Audio, bytes)),
    )
}

fn media_session(kind: SessionKind, tracks: Vec<Track>) -> media::Session {
    media::Session {
        kind,
        metadata: Metadata::default(),
        tracks,
        video_heights: Vec::new(),
        mv_progressive_url: None,
        mv_download_key: None,
    }
}

fn song() -> media::Session {
    let mut t = audio_track(b"AUDIO");
    t.codec = Codec::Alac;
    t.sample_rate = 96_000;
    t.bit_depth = 24;
    media::Session {
        metadata: Metadata {
            title: "Song Title".into(),
            artist_name: "Artist".into(),
            album_name: "Album".into(),
            duration_ms: 210_000,
            artwork_url: "http://art/500.jpg".into(),
            has_lyrics: true,
        },
        ..media_session(SessionKind::Song, vec![t])
    }
}

fn music_video() -> media::Session {
    media_session(
        SessionKind::MusicVideo,
        vec![
            track(
                StreamKind::Video,
                Codec::H264,
                Ready(stream(StreamKind::Video, b"VIDEO")),
            ),
            audio_track(b"AUDIO"),
        ],
    )
}

/// Returns a fixed media session (or error), optionally after a delay or a gate, and counts
/// calls. Honours its token.
#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    sess: Option<media::Session>,
    fail: Option<&'static str>,
    delay: Option<Duration>,
    /// Holds every open until notified.
    hold: Option<Arc<Notify>>,
    panic_once: AtomicBool,
}

impl Fake {
    fn with(sess: media::Session) -> Arc<Self> {
        Arc::new(Self {
            sess: Some(sess),
            ..Self::default()
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for Fake {
    async fn open(
        &self,
        cancel: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<media::Session, PipelineError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.panic_once.swap(false, Ordering::SeqCst) {
            panic!("malformed manifest");
        }
        if let Some(d) = self.delay {
            tokio::select! {
                () = cancel.cancelled() => return Err(PipelineError::Cancelled),
                () = tokio::time::sleep(d) => {}
            }
        }
        if let Some(hold) = &self.hold {
            tokio::select! {
                () = cancel.cancelled() => return Err(PipelineError::Cancelled),
                () = hold.notified() => hold.notify_one(), // pass it on to the next open
            }
        }
        if let Some(msg) = self.fail {
            return Err(err(msg));
        }
        let mut s = self.sess.clone().unwrap_or_else(song);
        s.metadata.title.clone_from(&req.asset_id);
        Ok(s)
    }
}

fn manager(p: Arc<Fake>) -> Manager {
    Manager::unreaped(p)
}

fn req(asset: &str) -> OpenRequest {
    OpenRequest {
        asset_id: asset.into(),
        storefront: "us".into(),
        ..OpenRequest::default()
    }
}

async fn open(m: &Manager, asset: &str) -> Arc<Session> {
    m.open(&CancellationToken::new(), req(asset)).await.unwrap()
}

async fn stream_to_vec(m: &Manager, id: &str, kind: StreamKind) -> Result<Vec<u8>, StreamError> {
    let mut out = Vec::new();
    m.stream(
        &StreamContext::default(),
        StreamClass::Foreground,
        id,
        kind,
        &mut out,
    )
    .await
    .map(|()| out)
}

fn set_expiry(m: &Manager, id: &str, expiry: Instant) {
    lock(&m.table).entries.get_mut(id).unwrap().expiry = expiry;
}

fn expiry(m: &Manager, id: &str) -> Instant {
    lock(&m.table).entries[id].expiry
}

// ── Open ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn open_song_fills_the_descriptor() {
    let m = manager(Fake::with(song()));
    let s = open(&m, "1").await;
    assert_eq!(s.kind, SessionKind::Song);
    assert_eq!(s.title, "1");
    assert_eq!(s.artist_name, "Artist");
    assert_eq!(
        (s.codec, s.sample_rate, s.bit_depth),
        (Some(Codec::Alac), 96_000, 24)
    );
    assert!(s.capabilities.audio && s.capabilities.lyrics);
    assert!(!s.capabilities.video && !s.capabilities.seekable);
    assert_eq!(
        s.streams.audio.as_deref(),
        Some(format!("/api/v1/playback/{}/audio", s.id).as_str())
    );
    assert_eq!(s.streams.video, None);
    assert_eq!(s.expires_in, 14_400);
    assert_eq!(s.id.len(), 16);
}

#[tokio::test]
async fn open_music_video_routes_each_kind() {
    let m = manager(Fake::with(music_video()));
    let s = open(&m, "1").await;
    assert!(s.capabilities.video);
    assert!(s.streams.video.is_some());
    assert_eq!(
        stream_to_vec(&m, &s.id, StreamKind::Audio).await.unwrap(),
        b"AUDIO"
    );
    assert_eq!(
        stream_to_vec(&m, &s.id, StreamKind::Video).await.unwrap(),
        b"VIDEO"
    );
}

#[tokio::test]
async fn format_fields_come_from_the_first_audio_track() {
    let mut aac = audio_track(b"A");
    aac.sample_rate = 44_100;
    aac.bit_rate = 256_000;
    aac.channel_count = 2;
    aac.codec_mime_type = r#"audio/mp4; codecs="mp4a.40.2""#.into();
    let mut alac = audio_track(b"B");
    alac.codec = Codec::Alac;
    let m = manager(Fake::with(media_session(
        SessionKind::Song,
        vec![aac, alac],
    )));
    let s = open(&m, "fmt").await;
    assert_eq!(
        s.codec,
        Some(Codec::Aac),
        "a second track must not overwrite"
    );
    assert_eq!(
        (s.sample_rate, s.bit_rate, s.channel_count),
        (44_100, 256_000, 2)
    );
    assert_eq!(s.codec_mime_type, r#"audio/mp4; codecs="mp4a.40.2""#);
}

#[tokio::test]
async fn seekable_capability_and_seek_queries() {
    let mut st = stream(StreamKind::Audio, b"AUDIO");
    st.source = Arc::new(Payload {
        bytes: b"AUDIO",
        gate: None,
        seekable: true,
    });
    let t = track(StreamKind::Audio, Codec::Aac, Ready(st));
    let m = manager(Fake::with(media_session(SessionKind::Song, vec![t])));
    let s = open(&m, "1").await;
    assert!(s.capabilities.seekable);
    assert_eq!(m.seek_start(&s.id, StreamKind::Audio, 7.5), Some(7.0));
    assert_eq!(m.seek_start(&s.id, StreamKind::Video, 7.5), None);
    assert_eq!(
        m.segment_timings(&s.id, StreamKind::Audio),
        Some(vec![0.0, 6.0, 12.0])
    );

    let mut out = Vec::new();
    let actual = m
        .stream_from(
            &StreamContext::default(),
            StreamClass::Foreground,
            &s.id,
            StreamKind::Audio,
            7.5,
            &mut out,
        )
        .await
        .unwrap();
    assert_eq!((actual, out.as_slice()), (7.0, b"SEEK".as_slice()));

    let err = m
        .prepare_direct_seek(&CancellationToken::new(), &s.id, StreamKind::Audio, 1.0)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, StreamError::NoDirectSeek(_)), "{err:?}");
}

#[tokio::test]
async fn stream_from_rejects_an_unseekable_stream() {
    let m = manager(Fake::with(song()));
    let s = open(&m, "1").await;
    let err = m
        .stream_from(
            &StreamContext::default(),
            StreamClass::Foreground,
            &s.id,
            StreamKind::Audio,
            3.0,
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StreamError::NotSeekable(_)), "{err:?}");
}

#[tokio::test]
async fn provider_and_track_errors_pass_through() {
    let m = manager(Arc::new(Fake {
        fail: Some("provider down"),
        ..Fake::default()
    }));
    let e = m
        .open(&CancellationToken::new(), req("1"))
        .await
        .unwrap_err();
    assert!(matches!(e, OpenError::Provider(_)), "{e:?}");
    assert_eq!(e.to_string(), "provider down");

    let bad = track(
        StreamKind::Audio,
        Codec::Aac,
        FailsToOpen("track open failed"),
    );
    let m = manager(Fake::with(media_session(SessionKind::Song, vec![bad])));
    let e = m
        .open(&CancellationToken::new(), req("1"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            OpenError::Track {
                kind: StreamKind::Audio,
                ..
            }
        ),
        "{e:?}"
    );
    // The handlers read the provider's words to tell a missing track from an outage.
    assert_eq!(e.to_string(), "open audio stream: track open failed");
}

/// A session reports the length its stream declares when that differs from the catalog's by
/// more than the tolerance; with several streams, the longest is what the player runs to.
#[tokio::test]
async fn reports_the_stream_duration() {
    for (name, catalog, streams, want) in [
        ("shorter stream", 200_000, vec![187_000], 187_000),
        ("longer stream", 200_000, vec![214_500], 214_500),
        ("within tolerance", 200_000, vec![200_600], 200_000),
        ("unknown stream length", 200_000, vec![0], 200_000),
        ("music video", 214_000, vec![205_000, 205_200], 205_200),
    ] {
        let tracks = streams
            .into_iter()
            .map(|ms| {
                let mut st = stream(StreamKind::Audio, b"");
                st.duration_ms = ms;
                track(StreamKind::Audio, Codec::Aac, Ready(st))
            })
            .collect();
        let mut sess = media_session(SessionKind::Song, tracks);
        sess.metadata.duration_ms = catalog;
        let m = manager(Fake::with(sess));
        assert_eq!(open(&m, name).await.duration_ms, want, "{name}");
    }
}

/// A video that fell back to a lower variant advertises that variant's codec string.
#[tokio::test]
async fn uses_the_opened_video_codec_string() {
    for (name, planned, opened, want) in [
        ("fell back", "hvc1.2.4", Some("avc1.640028"), "avc1.640028"),
        ("no override", "avc1.640028", None, "avc1.640028"),
    ] {
        let mut st = stream(StreamKind::Video, b"");
        st.codec_string = opened.map(str::to_owned);
        let mut t = track(StreamKind::Video, Codec::H264, Ready(st));
        t.codec_string = planned.into();
        let m = manager(Fake::with(media_session(SessionKind::MusicVideo, vec![t])));
        assert_eq!(
            open(&m, name).await.capabilities.video_codec,
            want,
            "{name}"
        );
    }
}

// ── Reuse and deduplication ─────────────────────────────────────────────────

#[tokio::test]
async fn reuses_a_live_session() {
    let p = Fake::with(song());
    let m = manager(Arc::clone(&p));
    let a = open(&m, "t").await;
    let b = open(&m, "t").await;
    assert_eq!(a.id, b.id);
    assert_eq!(p.calls(), 1);
}

#[tokio::test]
async fn different_capabilities_get_separate_sessions() {
    let p = Fake::with(song());
    let m = manager(Arc::clone(&p));
    let aac = open(&m, "t").await;
    let alac = m
        .open(
            &CancellationToken::new(),
            OpenRequest {
                lossless: true,
                ..req("t")
            },
        )
        .await
        .unwrap();
    assert_ne!(aac.id, alac.id);
    assert_eq!(p.calls(), 2);
}

#[tokio::test]
async fn an_expired_session_is_not_reused() {
    let p = Fake::with(song());
    let m = manager(Arc::clone(&p));
    let a = open(&m, "t").await;
    set_expiry(&m, &a.id, Instant::now() - Duration::from_secs(1));
    let b = open(&m, "t").await;
    assert_ne!(a.id, b.id);
    assert_eq!(p.calls(), 2);
}

#[tokio::test]
async fn release_clears_the_reuse_index() {
    let p = Fake::with(song());
    let m = manager(Arc::clone(&p));
    let a = open(&m, "t").await;
    m.release(&a.id);
    open(&m, "t").await;
    assert_eq!(p.calls(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_share_one_provider_call() {
    let p = Arc::new(Fake {
        delay: Some(Duration::from_millis(30)),
        ..Fake::default()
    });
    let m = Arc::new(manager(Arc::clone(&p)));
    let opens: Vec<_> = (0..10)
        .map(|_| {
            let m = Arc::clone(&m);
            tokio::spawn(async move { open(&m, "dup").await.id.clone() })
        })
        .collect();
    let mut ids = Vec::new();
    for o in opens {
        ids.push(o.await.unwrap());
    }
    assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    assert_eq!(p.calls(), 1);
}

/// The user skipped: the first caller's request is gone. A caller that joined its open must
/// not inherit that cancellation.
#[tokio::test]
async fn waiter_survives_a_cancelled_leader() {
    let hold = Arc::new(Notify::new());
    let p = Arc::new(Fake {
        hold: Some(Arc::clone(&hold)),
        ..Fake::default()
    });
    let m = Arc::new(manager(Arc::clone(&p)));
    let leader_token = CancellationToken::new();
    let leader = tokio::spawn({
        let (m, t) = (Arc::clone(&m), leader_token.clone());
        async move { m.open(&t, req("skip-me")).await }
    });
    while p.calls() == 0 {
        tokio::task::yield_now().await;
    }
    let waiter = tokio::spawn({
        let m = Arc::clone(&m);
        async move { m.open(&CancellationToken::new(), req("skip-me")).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await; // the waiter joins the flight

    leader_token.cancel();
    let e = leader.await.unwrap().unwrap_err();
    assert!(e.is_cancelled(), "{e:?}");
    hold.notify_one();
    let got = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter never completed")
        .unwrap();
    assert!(
        got.is_ok(),
        "waiter inherited the leader's cancellation: {got:?}"
    );
}

/// Dropping an open (a client disconnect drops the handler future) must not leave the flight
/// behind for later opens to join forever.
#[tokio::test]
async fn a_dropped_leader_does_not_wedge_the_asset() {
    let hold = Arc::new(Notify::new());
    let p = Arc::new(Fake {
        hold: Some(Arc::clone(&hold)),
        ..Fake::default()
    });
    let m = Arc::new(manager(Arc::clone(&p)));
    let leader = tokio::spawn({
        let m = Arc::clone(&m);
        async move { open(&m, "a").await }
    });
    while p.calls() == 0 {
        tokio::task::yield_now().await;
    }
    leader.abort();
    let _ = leader.await;
    assert!(lock(&m.inflight).is_empty(), "flight left behind");
    hold.notify_one();
    tokio::time::timeout(Duration::from_secs(2), open(&m, "a"))
        .await
        .expect("the next open hung on a dead flight");
}

/// A provider panic inside the leader must surface as an error and not leave the flight
/// behind: every later open of the asset would join a flight that never ends.
#[tokio::test]
async fn a_leader_panic_does_not_wedge_the_asset() {
    let p = Arc::new(Fake {
        panic_once: AtomicBool::new(true),
        ..Fake::default()
    });
    let m = manager(p);
    let e = m
        .open(&CancellationToken::new(), req("123"))
        .await
        .unwrap_err();
    assert!(matches!(e, OpenError::Panicked { .. }), "{e:?}");
    tokio::time::timeout(Duration::from_secs(2), open(&m, "123"))
        .await
        .expect("second open hung: the failed flight was never cleaned up");
}

// ── Private sessions ────────────────────────────────────────────────────────
//
// Private sessions back exports and the prefetcher: their release must never delete a session
// that playback is still serving.

async fn private(m: &Manager, asset: &str) -> Arc<Session> {
    m.open_private(&CancellationToken::new(), req(asset))
        .await
        .unwrap()
}

#[tokio::test]
async fn private_open_does_not_reuse_the_playback_session() {
    let p = Fake::with(song());
    let m = manager(Arc::clone(&p));
    let play = open(&m, "a").await;
    let exp = private(&m, "a").await;
    assert_ne!(play.id, exp.id);
    assert_eq!(p.calls(), 2);
}

#[tokio::test]
async fn private_session_is_never_indexed() {
    let m = manager(Fake::with(song()));
    let s = private(&m, "a").await;
    assert!(lock(&m.table).by_key.is_empty());
    assert_ne!(
        open(&m, "a").await.id,
        s.id,
        "a shared open reused a private session"
    );
}

#[tokio::test]
async fn releasing_a_private_session_keeps_the_playback_session() {
    let m = manager(Fake::with(song()));
    let play = open(&m, "a").await;
    let exp = private(&m, "a").await;
    m.release(&exp.id);
    assert!(m.session(&play.id).is_some());
    assert_eq!(
        open(&m, "a").await.id,
        play.id,
        "playback session no longer reusable"
    );
}

#[tokio::test]
async fn two_private_opens_are_independent() {
    let m = manager(Fake::with(song()));
    let a = private(&m, "a").await;
    let b = private(&m, "a").await;
    assert_ne!(a.id, b.id);
    m.release(&a.id);
    assert!(m.session(&b.id).is_some());
}

// ── Lookup, release, expiry ─────────────────────────────────────────────────

#[tokio::test]
async fn release_removes_the_session_and_is_idempotent() {
    let m = manager(Fake::with(song()));
    let s = open(&m, "1").await;
    assert!(m.session(&s.id).is_some());
    m.release(&s.id);
    assert!(m.session(&s.id).is_none());
    let e = stream_to_vec(&m, &s.id, StreamKind::Audio)
        .await
        .unwrap_err();
    assert!(matches!(e, StreamError::SessionNotFound(_)), "{e:?}");
    m.release(&s.id);
}

#[tokio::test]
async fn stream_errors_name_the_problem() {
    let m = manager(Fake::with(song()));
    let e = stream_to_vec(&m, "ghost", StreamKind::Audio)
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "session ghost not found or expired");
    let s = open(&m, "1").await;
    let e = stream_to_vec(&m, &s.id, StreamKind::Video)
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            StreamError::NoStream {
                kind: StreamKind::Video,
                ..
            }
        ),
        "{e:?}"
    );
}

#[tokio::test]
async fn expiry_on_lookup_and_sweep_clean_every_index() {
    let m = manager(Fake::with(song()));
    let a = open(&m, "a").await;
    open(&m, "b").await;
    open(&m, "c").await;
    set_expiry(&m, &a.id, Instant::now() - Duration::from_secs(1));
    assert!(m.session(&a.id).is_none(), "expired session still returned");
    assert_eq!(lock(&m.table).by_key.len(), 2);

    m.sweep_expired(Instant::now() + SESSION_TTL + Duration::from_secs(60));
    let t = lock(&m.table);
    assert!(
        t.entries.is_empty() && t.by_key.is_empty(),
        "leftover entries after expiry"
    );
}

/// A session in use must not expire at a fixed deadline in the middle of a listen, and a
/// fresh session is not touched on every lookup.
#[tokio::test(start_paused = true)]
async fn lookup_slides_expiry() {
    let m = manager(Fake::with(song()));
    let s = open(&m, "x").await;
    set_expiry(&m, &s.id, Instant::now() + Duration::from_secs(10 * 60));
    assert!(m.session(&s.id).is_some());
    assert_eq!(expiry(&m, &s.id), Instant::now() + SESSION_TTL);

    let fresh = Instant::now() + SESSION_TTL;
    tokio::time::advance(Duration::from_secs(60)).await;
    m.session(&s.id);
    assert_eq!(
        expiry(&m, &s.id),
        fresh,
        "a freshly refreshed session was touched again"
    );
}

/// The reaper removes sessions that nobody looks up.
#[tokio::test(start_paused = true)]
async fn reaper_sweeps_unused_sessions() {
    let m = Manager::new(Fake::with(song()));
    let s = open(&m, "x").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    m.set_release_hook(move |id| tx.send(id.to_owned()).unwrap());
    tokio::time::sleep(SESSION_TTL + 2 * REAP_EVERY).await;
    assert_eq!(rx.try_recv().ok(), Some(s.id.clone()));
    assert!(lock(&m.table).entries.is_empty());
}

/// Whoever owns per-session resources (producers, scratch files) must hear about a session
/// going away however it goes, exactly once.
#[tokio::test]
async fn release_hook_sees_every_removal_path() {
    let m = manager(Fake::with(song()));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    m.set_release_hook({
        let seen = Arc::clone(&seen);
        move |id| seen.lock().unwrap().push(id.to_owned())
    });
    let released = open(&m, "a").await.id.clone();
    let expired = open(&m, "b").await.id.clone();
    let swept = open(&m, "c").await.id.clone();

    m.release(&released);
    set_expiry(&m, &expired, Instant::now() - Duration::from_secs(1));
    assert!(m.session(&expired).is_none());
    m.sweep_expired(Instant::now() + SESSION_TTL + Duration::from_secs(60));
    m.release(&released); // already gone: not reported again

    let mut got = seen.lock().unwrap().clone();
    got.sort();
    let mut want = vec![released, expired, swept];
    want.sort();
    assert_eq!(got, want);
}

/// The hook may call back into the manager: it must not run under the manager's lock.
#[tokio::test]
async fn release_hook_may_call_back_into_the_manager() {
    let m = Arc::new(manager(Fake::with(song())));
    let s = open(&m, "x").await;
    let called = Arc::new(AtomicBool::new(false));
    m.set_release_hook({
        let (weak, called) = (Arc::downgrade(&m), Arc::clone(&called));
        move |id| {
            if let Some(m) = weak.upgrade() {
                m.session(id); // would deadlock if the lock were still held
            }
            called.store(true, Ordering::SeqCst);
        }
    });
    m.release(&s.id);
    assert!(called.load(Ordering::SeqCst));
}

// ── open_tracks ─────────────────────────────────────────────────────────────

/// Each open returns only once the other has started: impossible if they ran one by one.
struct Rendezvous {
    me: Arc<Notify>,
    other: Arc<Notify>,
    kind: StreamKind,
}

#[async_trait]
impl OpenTrack for Rendezvous {
    async fn open(&self, _: &CancellationToken) -> Result<Stream, PipelineError> {
        self.me.notify_one();
        self.other.notified().await;
        Ok(stream(self.kind, b""))
    }
}

#[tokio::test]
async fn open_tracks_runs_concurrently_in_order() {
    let (a, v) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let tracks = [
        track(
            StreamKind::Audio,
            Codec::Aac,
            Rendezvous {
                me: Arc::clone(&a),
                other: Arc::clone(&v),
                kind: StreamKind::Audio,
            },
        ),
        track(
            StreamKind::Video,
            Codec::H264,
            Rendezvous {
                me: v,
                other: a,
                kind: StreamKind::Video,
            },
        ),
    ];
    let got = tokio::time::timeout(
        Duration::from_secs(2),
        open_tracks(&CancellationToken::new(), "x", &tracks),
    )
    .await
    .expect("opens did not overlap")
    .unwrap();
    let kinds: Vec<_> = got.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [StreamKind::Audio, StreamKind::Video]);
}

/// Waits for cancellation and reports it.
struct UntilCancelled(Arc<AtomicBool>);

#[async_trait]
impl OpenTrack for UntilCancelled {
    async fn open(&self, cancel: &CancellationToken) -> Result<Stream, PipelineError> {
        cancel.cancelled().await;
        self.0.store(true, Ordering::SeqCst);
        Err(PipelineError::Cancelled)
    }
}

#[tokio::test]
async fn open_tracks_failure_cancels_the_other() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let tracks = [
        track(
            StreamKind::Audio,
            Codec::Aac,
            UntilCancelled(Arc::clone(&cancelled)),
        ),
        track(StreamKind::Video, Codec::H264, FailsToOpen("boom")),
    ];
    let Err(e) = open_tracks(&CancellationToken::new(), "x", &tracks).await else {
        panic!("open_tracks succeeded with a failing track");
    };
    // The video failure, not the cancellation it induced.
    assert_eq!(e.to_string(), "open video stream: boom");
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the other open was not cancelled"
    );
}

#[tokio::test]
async fn open_tracks_single_and_none() {
    let token = CancellationToken::new();
    assert_eq!(open_tracks(&token, "x", &[]).await.unwrap().len(), 0);
    assert_eq!(
        open_tracks(&token, "x", &[audio_track(b"")])
            .await
            .unwrap()
            .len(),
        1
    );
}

// ── Stream classes and the streaming signal ─────────────────────────────────
//
// `is_streaming` gates the prefetcher and `foreground_stats` drives the export throttle. Both
// failure modes are silent: stuck true defers warming forever, stuck false lets warming
// compete with playback.

fn gated_song(gate: &Gate, bit_rate: u32) -> media::Session {
    let st = Stream {
        source: Arc::new(Payload {
            bytes: b"AUDIO",
            gate: Some(gate.clone()),
            seekable: false,
        }),
        ..stream(StreamKind::Audio, b"")
    };
    let mut t = track(StreamKind::Audio, Codec::Aac, Ready(st));
    t.bit_rate = bit_rate;
    media_session(SessionKind::Song, vec![t])
}

/// Starts a stream that stays mid-flight until `gate.release` is notified.
async fn start_gated(
    m: &Arc<Manager>,
    gate: &Gate,
    class: StreamClass,
    kind: StreamKind,
) -> tokio::task::JoinHandle<Result<Vec<u8>, StreamError>> {
    let s = open(m, "1").await;
    let task = tokio::spawn({
        let m = Arc::clone(m);
        async move {
            let mut out = Vec::new();
            m.stream(&StreamContext::default(), class, &s.id, kind, &mut out)
                .await
                .map(|()| out)
        }
    });
    gate.entered.notified().await;
    task
}

#[tokio::test]
async fn foreground_stream_counts_while_running() {
    let gate = Gate::default();
    let m = Arc::new(manager(Fake::with(gated_song(&gate, 0))));
    assert!(!m.is_streaming() && m.active_streams() == 0);

    let task = start_gated(&m, &gate, StreamClass::Foreground, StreamKind::Audio).await;
    assert!(m.is_streaming());
    assert_eq!(m.active_streams(), 1);
    let stats = m.foreground_stats();
    assert_eq!((stats.active, stats.need_bps), (1, DEFAULT_AUDIO_NEED_BPS));

    gate.release.notify_one();
    assert_eq!(task.await.unwrap().unwrap(), b"AUDIO");
    assert!(!m.is_streaming(), "stuck true after the stream finished");
    assert_eq!(
        m.foreground_stats(),
        ForegroundStats {
            active: 0,
            bytes: 5,
            need_bps: 0
        }
    );
}

#[tokio::test]
async fn foreground_need_follows_the_bit_rate() {
    let gate = Gate::default();
    let m = Arc::new(manager(Fake::with(gated_song(&gate, 256_000))));
    let task = start_gated(&m, &gate, StreamClass::Foreground, StreamKind::Audio).await;
    assert_eq!(m.foreground_stats().need_bps, 32_000);
    gate.release.notify_one();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn background_stream_never_looks_like_playback() {
    let gate = Gate::default();
    let m = Arc::new(manager(Fake::with(gated_song(&gate, 0))));
    let task = start_gated(&m, &gate, StreamClass::Background, StreamKind::Audio).await;
    assert!(
        !m.is_streaming(),
        "a background stream made is_streaming true"
    );
    assert_eq!(m.active_streams(), 1, "metrics count every class");
    assert_eq!(m.foreground_stats(), ForegroundStats::default());
    gate.release.notify_one();
    task.await.unwrap().unwrap();
    assert_eq!(m.active_streams(), 0);
    assert_eq!(
        m.foreground_stats().bytes,
        0,
        "background bytes are not counted"
    );
}

#[tokio::test]
async fn video_need_uses_the_video_default() {
    let gate = Gate::default();
    let st = Stream {
        source: Arc::new(Payload {
            bytes: b"VIDEO",
            gate: Some(gate.clone()),
            seekable: false,
        }),
        ..stream(StreamKind::Video, b"")
    };
    let sess = media_session(
        SessionKind::MusicVideo,
        vec![track(StreamKind::Video, Codec::H264, Ready(st))],
    );
    let m = Arc::new(manager(Fake::with(sess)));
    let task = start_gated(&m, &gate, StreamClass::Foreground, StreamKind::Video).await;
    assert_eq!(m.foreground_stats().need_bps, DEFAULT_VIDEO_NEED_BPS);
    gate.release.notify_one();
    task.await.unwrap().unwrap();
}

/// Fails every write.
struct BrokenSink;

impl AsyncWrite for BrokenSink {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("boom")))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Sink for BrokenSink {}

/// The counters unwind when a stream fails, when its future is dropped (the client went
/// away), and are never touched for a session that does not exist.
#[tokio::test]
async fn counters_unwind_on_every_exit() {
    let m = manager(Fake::with(song()));
    let s = open(&m, "1").await;
    let ctx = StreamContext::default();
    let e = m
        .stream(
            &ctx,
            StreamClass::Foreground,
            &s.id,
            StreamKind::Audio,
            &mut BrokenSink,
        )
        .await;
    assert!(e.is_err());
    assert!(stream_to_vec(&m, "ghost", StreamKind::Audio).await.is_err());
    assert_eq!(m.foreground_stats().active, 0);

    let gate = Gate::default();
    let m = Arc::new(manager(Fake::with(gated_song(&gate, 0))));
    let task = start_gated(&m, &gate, StreamClass::Foreground, StreamKind::Audio).await;
    task.abort();
    let _ = task.await;
    assert!(!m.is_streaming(), "a dropped stream left the counter up");
    assert_eq!(m.active_streams(), 0);
    assert_eq!(m.foreground_stats().need_bps, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_streams_balance() {
    let m = Arc::new(manager(Fake::with(song())));
    let s = open(&m, "1").await;
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let (m, id) = (Arc::clone(&m), s.id.clone());
            tokio::spawn(async move { stream_to_vec(&m, &id, StreamKind::Audio).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(m.active_streams(), 0);
    assert_eq!(m.foreground_stats().bytes, 8 * 5);
}

#[derive(Default)]
struct HeaderSink {
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl AsyncWrite for HeaderSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.body).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Sink for HeaderSink {
    fn set_header(&mut self, name: &str, value: &str) {
        self.headers.push((name.to_owned(), value.to_owned()));
    }
}

/// The byte counter must not hide the sink's headers from the source (Content-Length).
#[tokio::test]
async fn counting_keeps_headers_visible() {
    let m = manager(Fake::with(song()));
    let s = open(&m, "1").await;
    let mut sink = HeaderSink::default();
    m.stream(
        &StreamContext::default(),
        StreamClass::Foreground,
        &s.id,
        StreamKind::Audio,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(
        sink.headers,
        [("Content-Length".to_owned(), "5".to_owned())]
    );
    assert_eq!(sink.body, b"AUDIO");
}
