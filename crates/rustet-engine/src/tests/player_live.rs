//! The player on real libvlc decks (ignored by default; nothing is heard: `--aout=adummy`).
//!
//! Run with `cargo test -p rustet-engine player_live -- --ignored --nocapture --test-threads=1`, with
//! `libvlc.so.5` on `LD_LIBRARY_PATH` (`nix build nixpkgs#libvlc`), its plugins in
//! `VLC_PLUGIN_PATH`, and ffmpeg in `FFMPEG` or on `PATH`. The provider serves fragmented MP4
//! made by ffmpeg (ALAC and AAC, 1 s fragments, the track length in mvhd as both real paths
//! write it) at a set multiple of the play rate, so the decks read files that are still growing.

use std::collections::HashMap;
use std::ffi::CStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use rustet_core::media::{Metadata, OpenRequest, OpenTrack, Provider, Session, SessionKind, Track};
use rustet_core::pipeline::{
    Codec, PipelineError, Seekable, Sink, Source, Stream, StreamContext, StreamKind,
};
use rustet_player::model::{Capabilities, Command};
use rustet_player::ports::{Deck, DeckError, DeckTiming, LoadRequest, deck_channel};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use super::{app, body_json, call};
use crate::player::deck::{VlcDeck, libvlc_decks};
use crate::state::{Engine, Playback, Vlc};

/// `adummy` has no clock: libvlc plays what arrives at once and reports the position of its
/// input clock, which runs behind the data by the input caching. No caching keeps the reported
/// position close to the data, so the end of a track shows before it is reached.
const ARGS: &[&CStr] = &[
    c"--aout=adummy",
    c"--vout=vdummy",
    c"--no-video-title-show",
    c"--file-caching=0",
    c"--network-caching=0",
];

/// An ffmpeg-made track: init segment and `frag_ms` fragments with absolute decode times.
struct Fixture {
    init: Vec<u8>,
    frags: Vec<Vec<u8>>,
    frag_ms: u64,
}

/// `seconds` of audio whose moov declares `declared` seconds.
fn ffmpeg_track(
    dir: &Path,
    name: &str,
    codec: &[&str],
    seconds: u32,
    declared: u32,
    frag_ms: u64,
) -> Fixture {
    let out = dir.join(format!("{name}.m4a"));
    let ffmpeg = std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let src = format!("sine=frequency=440:sample_rate=44100:duration={seconds}");
    let status = std::process::Command::new(ffmpeg)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", &src, "-ac", "2"])
        .args(codec)
        .args([
            "-f",
            "mp4",
            "-movflags",
            "+empty_moov+default_base_moof",
            "-frag_duration",
        ])
        .arg((frag_ms * 1000).to_string())
        .arg(&out)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
    let mut data = std::fs::read(&out).unwrap();
    // ffmpeg's empty moov says 0, as Apple's does; both engine paths write the length.
    let at = data.windows(4).position(|w| w == b"mvhd").unwrap() + 4;
    assert_eq!(data[at], 0, "mvhd version 0");
    let timescale = u32::from_be_bytes(data[at + 12..at + 16].try_into().unwrap());
    data[at + 16..at + 20].copy_from_slice(&(timescale * declared).to_be_bytes());
    split(&data, frag_ms)
}

/// Cuts an ffmpeg fMP4 file into its init segment and its fragments.
fn split(data: &[u8], frag_ms: u64) -> Fixture {
    let (mut init, mut frags, mut cur, mut i) = (Vec::new(), Vec::new(), Vec::new(), 0);
    while i + 8 <= data.len() {
        let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        let (kind, bytes) = (&data[i + 4..i + 8], &data[i..i + size]);
        match kind {
            b"ftyp" | b"moov" => init.extend_from_slice(bytes),
            b"moof" => cur = bytes.to_vec(),
            b"mdat" => {
                cur.extend_from_slice(bytes);
                frags.push(std::mem::take(&mut cur));
            }
            _ => {}
        }
        i += size;
    }
    Fixture {
        init,
        frags,
        frag_ms,
    }
}

/// Writes the init, then one fragment per `frag_ms / speed`, from fragment `from`.
#[derive(Clone)]
struct Paced {
    fixture: Arc<Fixture>,
    speed: f64,
    from: usize,
}

#[async_trait]
impl Source for Paced {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        w.write_all(&self.fixture.init).await?;
        for f in &self.fixture.frags[self.from..] {
            w.write_all(f).await?;
            tokio::select! {
                () = ctx.cancel.cancelled() => return Err(PipelineError::Cancelled),
                () = tokio::time::sleep(Duration::from_secs_f64(self.fixture.frag_ms as f64 / 1000.0 / self.speed)) => {}
            }
        }
        Ok(())
    }

    fn seekable(&self) -> Option<&dyn Seekable> {
        Some(self)
    }
}

impl Seekable for Paced {
    /// The fragment holding `start_sec` and the rest: decode times stay absolute.
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64) {
        let frag = self.fixture.frag_ms as f64 / 1000.0;
        let from =
            ((start_sec / frag).floor() as usize).min(self.fixture.frags.len().saturating_sub(1));
        (
            Arc::new(Self {
                from,
                ..self.clone()
            }),
            from as f64 * frag,
        )
    }
}

struct Ready(Stream);

#[async_trait]
impl OpenTrack for Ready {
    async fn open(&self, _: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(self.0.clone())
    }
}

/// Opens each asset ID as its fixture.
struct Tracks(HashMap<String, (Codec, Paced)>);

#[async_trait]
impl Provider for Tracks {
    async fn open(
        &self,
        _: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<Session, PipelineError> {
        let (codec, paced) = self
            .0
            .get(&req.asset_id)
            .cloned()
            .ok_or_else(|| PipelineError::Other("unknown asset".into()))?;
        let stream = Stream {
            source: Arc::new(paced),
            stages: Vec::new(),
            kind: StreamKind::Audio,
            codec,
            duration_ms: 0,
            codec_string: None,
        };
        Ok(Session {
            kind: SessionKind::Song,
            metadata: Metadata {
                title: req.asset_id.clone(),
                ..Metadata::default()
            },
            tracks: vec![Track {
                kind: StreamKind::Audio,
                codec,
                codec_string: String::new(),
                sample_rate: 44_100,
                bit_depth: if codec == Codec::Alac { 16 } else { 0 },
                bit_rate: 0,
                channel_count: 2,
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

/// Logs what the player asks of a deck, to tell a gapless hand-over from a fresh load.
struct Logged {
    name: &'static str,
    inner: Arc<VlcDeck>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Logged {
    fn note(&self, s: String) {
        self.log.lock().unwrap().push(format!("{} {s}", self.name));
    }
}

#[async_trait]
impl Deck for Logged {
    async fn load(&self, req: LoadRequest) -> Result<(), DeckError> {
        self.note(format!("load paused={} at={}", req.paused, req.start_ms));
        self.inner.load(req).await
    }
    async fn play(&self) -> Result<(), DeckError> {
        self.note("play".into());
        self.inner.play().await
    }
    async fn pause(&self) -> Result<(), DeckError> {
        self.inner.pause().await
    }
    async fn seek(&self, position_ms: u64) -> Result<(), DeckError> {
        let r = self.inner.seek(position_ms).await;
        self.note(format!("seek {position_ms}: {r:?}"));
        r
    }
    async fn stop(&self) -> Result<(), DeckError> {
        self.inner.stop().await
    }
    async fn set_volume(&self, level: u8, muted: bool) -> Result<(), DeckError> {
        self.inner.set_volume(level, muted).await
    }
    async fn timing(&self) -> DeckTiming {
        self.inner.timing().await
    }
}

/// An engine over `dir` (or with its data directory at `data_dir`) with the fixtures, signed in,
/// with the web shell's libvlc player too.
async fn live_engine(
    dir: &Path,
    data_dir: Option<&Path>,
    tracks: Arc<dyn Provider>,
) -> Arc<Engine> {
    let mut e = super::engine(dir, "http://127.0.0.1:9").await;
    if let Some(d) = data_dir {
        e.data_dir = d.to_owned();
    }
    let manager = rustet_playback::Manager::new(tracks);
    e.playback = Some(Playback::new(
        manager,
        &e.tokens,
        e.epoch.clone(),
        e.events.clone(),
    ));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let legacy = tokio::task::spawn_blocking(move || {
        rustet_vlc::Instance::with_args(ARGS).and_then(|i| i.deck(tx))
    })
    .await
    .unwrap()
    .expect("libvlc");
    e.vlc = Some(Vlc {
        player: Arc::new(legacy),
        gate: Default::default(),
    });
    let e = Arc::new(e);
    e.install_release_hook();
    e.tokens.set_developer("dev");
    e.set_music_user_token("mut");
    e
}

/// Starts the player on two real decks; returns the decks and the hand-over log.
async fn start_live_player(e: &Arc<Engine>) -> ([Arc<VlcDeck>; 2], Arc<Mutex<Vec<String>>>) {
    let (tx, rx) = deck_channel();
    let decks = libvlc_decks(e, ARGS, tx).await.expect("libvlc decks");
    let log = Arc::new(Mutex::new(Vec::new()));
    let wrap = |name, d: &Arc<VlcDeck>| -> Arc<dyn Deck> {
        Arc::new(Logged {
            name,
            inner: d.clone(),
            log: log.clone(),
        })
    };
    e.start_player([wrap("A", &decks[0]), wrap("B", &decks[1])], rx);
    // The fixtures' lossless tracks are ALAC: let the player ask for lossless.
    let caps = Capabilities {
        lossless_ready: true,
        signed_in: true,
    };
    *e.player_caps.lock().unwrap() = Some(caps);
    e.player
        .get()
        .unwrap()
        .send(Command::Capabilities(caps))
        .await
        .unwrap();
    (decks, log)
}

async fn state(app: &Router) -> Value {
    body_json(call(app, "GET", "/api/v1/player", "").await).await
}

async fn until(app: &Router, what: &str, secs: u64, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = state(app).await;
        if ok(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "{what}: last state {s:#}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn post(app: &Router, uri: &str, body: Value) {
    let resp = call(app, "POST", uri, &body.to_string()).await;
    assert_eq!(resp.status().as_u16(), 202, "{uri}");
}

/// The deck's position once it lands in `[from, to)`, and how long that took.
async fn lands(deck: &Arc<VlcDeck>, from: u64, to: u64, limit: Duration) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if deck
            .timing()
            .await
            .position_ms
            .is_some_and(|p| (from..to).contains(&p))
        {
            return Some(t0.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

fn meta(lossless: bool, seconds: u64) -> Value {
    let traits = if lossless {
        json!(["lossless", "lossy-stereo"])
    } else {
        json!(["lossy-stereo"])
    };
    json!({"name": "t", "durationMs": seconds * 1000, "audioTraits": traits})
}

/// Three short tracks (lossless, AAC, lossless) played to the end on real decks: starts,
/// gapless hand-overs, advancing positions, history, pause and resume, a seek while the first
/// track still downloads, the ownership rule with the web shell's routes, and a restart that
/// restores the queue paused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs libvlc and ffmpeg"]
async fn three_tracks_on_real_decks() {
    let dir = tempfile::tempdir().unwrap();
    let alac = ["-c:a", "alac"];
    let aac = ["-c:a", "aac", "-b:a", "256k"];
    // `adummy` has no clock: libvlc plays the data as it arrives, its position about 800 ms
    // behind, and at the end of the data the buffered rest plays at once. The 300 ms pre-roll
    // window before the end would never show, so each track has 1 s of audio past its declared
    // 6 s (the end then comes as with a clocked output, after the position reached the length).
    let fixture = |name, codec: &[&str]| Arc::new(ffmpeg_track(dir.path(), name, codec, 7, 6, 250));
    let tracks = Arc::new(Tracks(HashMap::from([
        (
            "101".to_owned(),
            (
                Codec::Alac,
                Paced {
                    fixture: fixture("l1", &alac),
                    speed: 1.0,
                    from: 0,
                },
            ),
        ),
        (
            "102".to_owned(),
            (
                Codec::Aac,
                Paced {
                    fixture: fixture("a2", &aac),
                    speed: 1.0,
                    from: 0,
                },
            ),
        ),
        (
            "103".to_owned(),
            (
                Codec::Alac,
                Paced {
                    fixture: fixture("l3", &alac),
                    speed: 1.0,
                    from: 0,
                },
            ),
        ),
    ])));
    let e = live_engine(dir.path(), None, tracks.clone()).await;
    let (decks, log) = start_live_player(&e).await;
    let app = app(&e);
    let mut sub = e.events.subscribe(None);
    let events = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let collector = {
        let events = events.clone();
        tokio::spawn(async move {
            while let Some(ev) = sub.events.recv().await {
                events.lock().unwrap().push((
                    ev.kind.to_owned(),
                    serde_json::from_str(&ev.payload).unwrap(),
                ));
            }
        })
    };

    let items = json!([
        {"id": "101", "kind": "song", "meta": meta(true, 6)},
        {"id": "102", "kind": "song", "meta": meta(false, 6)},
        {"id": "103", "kind": "song", "meta": meta(true, 6)},
    ]);
    let t0 = Instant::now();
    post(
        &app,
        "/api/v1/player/queue",
        json!({"items": items, "startIndex": 0, "context": {"kind": "album", "id": "100"}}),
    )
    .await;
    let s = until(&app, "track 1 plays", 10, |s| s["status"] == "playing").await;
    eprintln!(
        "track 1 (ALAC, growing at 1x): queue → playing in {:?}, quality {}",
        t0.elapsed(),
        s["quality"]
    );
    assert_eq!(s["quality"]["codec"], "alac");
    lands(&decks[0], 800, 6000, Duration::from_secs(5))
        .await
        .expect("track 1 position advances");

    // Pause and resume.
    post(&app, "/api/v1/player/pause", json!({})).await;
    let paused = until(&app, "paused", 5, |s| s["status"] == "paused").await["positionMs"]
        .as_u64()
        .unwrap();
    let at = decks[0].timing().await.position_ms.unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let later = decks[0].timing().await.position_ms.unwrap_or(0);
    assert!(
        later <= at + 100,
        "the deck moved while paused: {at} → {later}"
    );
    post(&app, "/api/v1/player/play", json!({})).await;
    until(&app, "resumed", 5, |s| s["status"] == "playing").await;
    eprintln!("pause at {paused} ms held ({at} → {later}), resumed");

    // A seek beyond the downloaded part of the growing file. Done once it played 0.5 s past.
    let generation = state(&app).await["generation"].clone();
    let (asked, downloaded) = (Instant::now(), t0.elapsed().as_secs_f64());
    post(
        &app,
        "/api/v1/player/seek",
        json!({"positionMs": 4500, "expectGeneration": generation}),
    )
    .await;
    if lands(&decks[0], 4800, u64::MAX, Duration::from_secs(10))
        .await
        .is_none()
    {
        let state = state(&app).await;
        panic!(
            "the seek never played past its target; deck calls {:?}; state {state:#}",
            log.lock().unwrap()
        );
    }
    let took = asked.elapsed();
    eprintln!(
        "seek to 4.5 s with ~{downloaded:.1} s downloaded at 1x: played past the target after {took:?}"
    );
    assert!(took < Duration::from_secs(4), "seek took {took:?}");

    // To the end of the queue.
    until(&app, "the queue ends", 40, |s| s["status"] == "stopped").await;
    let log = log.lock().unwrap().clone();
    eprintln!("deck calls: {log:?}");
    let started: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == "player.track")
        .map(|(_, p)| p["item"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(started, ["101", "102", "103"]);
    let cues = log
        .iter()
        .filter(|l| l.contains("load paused=true"))
        .count();
    let plays = log.iter().filter(|l| l.ends_with(" play")).count();
    assert!(
        cues >= 2 && plays >= 2,
        "tracks 2 and 3 start from a pre-roll on the idle deck: {log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l.contains("seek 4500: Err(NeedsReload)")),
        "the growing file seeks by reload: {log:?}"
    );
    let positions: Vec<(u64, u64)> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == "player.position")
        .map(|(_, p)| {
            (
                p["uid"].as_u64().unwrap(),
                p["positionMs"].as_u64().unwrap(),
            )
        })
        .collect();
    for uid in positions
        .iter()
        .map(|p| p.0)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let of: Vec<u64> = positions
            .iter()
            .filter(|p| p.0 == uid)
            .map(|p| p.1)
            .collect();
        assert!(
            of.len() >= 2 && of.last() > of.first(),
            "uid {uid} positions {of:?}"
        );
    }
    let history = body_json(call(&app, "GET", "/api/v1/player/history", "").await).await;
    assert_eq!(history.as_array().unwrap().len(), 3, "{history}");
    assert!(e.data_dir.join("player-history.json").exists());
    eprintln!(
        "queue of 3 played in {:?}; history {} entries",
        t0.elapsed(),
        history.as_array().unwrap().len()
    );

    // Track 1 queued paused at 2 s, then a restart. The restarted engine has an empty cache, so
    // its track streams at the play rate again (`adummy` would play a cached file in a burst).
    post(
        &app,
        "/api/v1/player/queue",
        json!({"items": items, "startIndex": 0, "startMs": 2000, "play": false}),
    )
    .await;
    until(&app, "queued paused", 5, |s| {
        s["status"] == "paused" && s["track"]["id"] == "101"
    })
    .await;
    e.player.get().unwrap().shutdown().await;
    crate::player::stop_decks(&e).await;
    e.shutdown.cancel();
    collector.abort();

    let dir2 = tempfile::tempdir().unwrap();
    let next = live_engine(dir2.path(), Some(&e.data_dir), tracks.clone()).await;
    let (decks2, log2) = start_live_player(&next).await;
    let app2 = super::app(&next);
    let s = until(&app2, "restored paused", 10, |s| s["restored"] == true).await;
    eprintln!(
        "after restart: status {}, track {}, position {}",
        s["status"], s["track"]["id"], s["positionMs"]
    );
    assert_eq!(
        (
            s["status"].as_str(),
            s["track"]["id"].as_str(),
            s["positionMs"].as_u64()
        ),
        (Some("paused"), Some("101"), Some(2000))
    );
    post(&app2, "/api/v1/player/play", json!({})).await;
    until(&app2, "plays after the restart", 10, |s| {
        s["status"] == "playing"
    })
    .await;
    lands(&decks2[0], 2000, 7000, Duration::from_secs(5))
        .await
        .expect("the restored track resumes at its position");
    assert!(
        log2.lock()
            .unwrap()
            .iter()
            .any(|l| l == "A load paused=false at=2000"),
        "{:?}",
        log2.lock().unwrap()
    );

    // Ownership: the web shell's /vlc/load pauses the player; the player's play stops the shell's player.
    let req = OpenRequest {
        asset_id: "102".into(),
        ..Default::default()
    };
    let session = next
        .playback
        .as_ref()
        .unwrap()
        .manager
        .open(&CancellationToken::new(), req)
        .await
        .unwrap()
        .id
        .clone();
    let resp = call(
        &app2,
        "POST",
        "/api/v1/vlc/load",
        &json!({"sessionId": session}).to_string(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    until(&app2, "the routes pause the player", 5, |s| {
        s["status"] == "paused"
    })
    .await;
    post(&app2, "/api/v1/player/play", json!({})).await;
    until(&app2, "the player plays again", 10, |s| {
        s["status"] == "playing"
    })
    .await;
    let legacy = next.vlc.as_ref().unwrap().player.clone();
    let legacy_state = tokio::task::spawn_blocking(move || legacy.timing().state)
        .await
        .unwrap();
    assert_ne!(
        legacy_state,
        rustet_vlc::State::Playing,
        "the shell's player was stopped"
    );
    eprintln!(
        "ownership: /vlc/load paused the player; player play stopped the shell's player ({legacy_state})"
    );
    next.player.get().unwrap().shutdown().await;
    crate::player::stop_decks(&next).await;
}

/// Seek times on a growing file, before (libvlc's own seek in the cache download) and after
/// (the deck's reload from a seek stream), for ALAC and AAC downloading at 1× and 4× the play
/// rate. A seek counts as done once the deck has played 0.5 s past the target: libvlc reports
/// the target position at once, also while its demuxer still waits for the file to complete.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs libvlc and ffmpeg"]
async fn seek_on_a_growing_file_before_and_after() {
    let dir = tempfile::tempdir().unwrap();
    let mut report = Vec::new();
    for (name, codec, args) in [
        ("alac", Codec::Alac, &["-c:a", "alac"][..]),
        ("aac", Codec::Aac, &["-c:a", "aac", "-b:a", "256k"][..]),
    ] {
        let fixture = Arc::new(ffmpeg_track(dir.path(), name, args, 12, 12, 1000));
        for speed in [1.0, 4.0] {
            let paced = Paced {
                fixture: fixture.clone(),
                speed,
                from: 0,
            };
            let tracks = Arc::new(Tracks(HashMap::from([
                ("1".to_owned(), (codec, paced.clone())),
                ("2".to_owned(), (codec, paced)),
            ])));
            let sub = tempfile::tempdir().unwrap();
            let e = live_engine(sub.path(), None, tracks).await;
            let (tx, _rx) = deck_channel();
            let [deck, _] = libvlc_decks(&e, ARGS, tx).await.unwrap();
            let p = e.playback.as_ref().unwrap();
            let mut times = Vec::new();
            for (asset, reload) in [("1", false), ("2", true)] {
                let req = OpenRequest {
                    asset_id: asset.into(),
                    ..Default::default()
                };
                let sess = p
                    .manager
                    .open(&CancellationToken::new(), req)
                    .await
                    .unwrap();
                let load = |n, start_ms| LoadRequest {
                    load: rustet_player::ports::LoadId(n),
                    session: rustet_player::ports::SessionId(sess.id.clone()),
                    start_ms,
                    paused: false,
                    volume: 100,
                    muted: false,
                    video: false,
                };
                deck.load(load(1, 0)).await.unwrap();
                lands(&deck, 500, 12_000, Duration::from_secs(10))
                    .await
                    .expect("plays");
                let t0 = Instant::now();
                if reload {
                    assert_eq!(
                        deck.seek(8000).await,
                        Err(DeckError::NeedsReload),
                        "a growing file seeks by reload"
                    );
                    deck.load(load(2, 8000)).await.unwrap();
                } else {
                    let raw = deck.raw().clone();
                    tokio::task::spawn_blocking(move || raw.set_time(8000))
                        .await
                        .unwrap();
                }
                times.push(
                    lands(&deck, 8500, 11_000, Duration::from_secs(20))
                        .await
                        .map(|_| t0.elapsed()),
                );
                deck.stop().await.unwrap();
            }
            report.push(format!(
                "{name} at {speed}x: seek to 8 s of a 12 s track while it downloads: libvlc seek in the growing file {:?}, reload from a seek stream {:?}",
                times[0], times[1]
            ));
            assert!(
                times[1].is_some_and(|t| t < Duration::from_secs(4)),
                "{}",
                report.last().unwrap()
            );
            e.shutdown.cancel();
        }
    }
    for r in &report {
        eprintln!("{r}");
    }
}

mod music_video;
