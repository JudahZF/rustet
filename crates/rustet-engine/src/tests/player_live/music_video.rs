//! Music videos on real libvlc decks (ignored by default; nothing is heard or shown:
//! `--aout=adummy`, frames go to a ring). Run as the parent module says, with ffmpeg built with
//! libx264.
//!
//! The fixture is an ffmpeg test pattern (frame n draws n in binary as 13 white squares along its
//! top edge) and a 440 Hz tone, served as two fMP4 streams the way Apple's arrive: H.264 in 2 s
//! fragments at keyframes whose decode times run 10 s ahead (Apple's raw timeline), AAC in 3 s
//! fragments, both written at the play rate. The tests read the ring the way the UI does:
//! `PlayerState.video` names it, `rustet_framering::Reader` copies its newest frame, and the frame
//! number tells which picture is on screen.

use std::process::Command as Process;

use rustet_framering::{Read, Reader};
use rustet_player::model::{PlayerState, Status, VideoSurface};

use super::*;

const FPS: u64 = 30;
const BITS: u32 = 13;
/// Square side at 720 lines.
const SQUARE: u32 = 48;
/// The fixture streams arrive at this multiple of the play rate. Apple's CDN delivers a music
/// video faster than it plays; at exactly 1× a 2 s fragment arrives just as it is due, and
/// libvlc (with no input caching here) stalls at fragment edges.
const DOWNLOAD_SPEED: f64 = 3.0;

fn ffmpeg() -> Process {
    let mut cmd = Process::new(std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into()));
    cmd.args(["-v", "error", "-y"]);
    cmd
}

/// The video stream: `seconds` of the pattern at `width`×`height`, 30 fps, a keyframe every
/// 2 s and a fragment at each, decode times 10 s ahead of the playlist.
fn video_fixture(dir: &Path, seconds: u32, width: u32, height: u32) -> Fixture {
    let path = dir.join(format!("v{height}.mp4"));
    let s = SQUARE * height / 720;
    let mut filter = format!("color=c=0x202020:s={width}x{height}:r={FPS}:d={seconds}");
    for k in 0..BITS {
        filter += &format!(
            ",drawbox=x={}:y=0:w={s}:h={s}:color=white:t=fill:enable='eq(mod(floor(n/{}),2),1)'",
            k * s,
            1u64 << k
        );
    }
    let status = ffmpeg()
        .args([
            "-f", "lavfi", "-i", &filter, "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt",
            "yuv420p",
        ])
        .args([
            "-g",
            "60",
            "-keyint_min",
            "60",
            "-sc_threshold",
            "0",
            "-output_ts_offset",
            "10",
        ])
        .args([
            "-f",
            "mp4",
            "-movflags",
            "+empty_moov+default_base_moof+frag_keyframe",
            "-frag_duration",
            "2000000",
        ])
        .arg(&path)
        .status()
        .expect("ffmpeg");
    assert!(status.success(), "ffmpeg (with libx264) made no video");
    split(&std::fs::read(&path).unwrap(), 2000)
}

/// The audio stream: `seconds` of a 440 Hz tone, AAC, 3 s fragments.
fn audio_fixture(dir: &Path, seconds: u32) -> Fixture {
    let path = dir.join("a.m4a");
    let tone = format!("sine=frequency=440:sample_rate=44100:duration={seconds}");
    let status = ffmpeg()
        .args([
            "-f", "lavfi", "-i", &tone, "-ac", "2", "-c:a", "aac", "-b:a", "128k",
        ])
        .args([
            "-f",
            "mp4",
            "-movflags",
            "+empty_moov+default_base_moof",
            "-frag_duration",
            "3000000",
        ])
        .arg(&path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
    split(&std::fs::read(&path).unwrap(), 3000)
}

/// A music video of the provider: its two streams, its height and length.
#[derive(Clone)]
struct Video {
    video: Paced,
    audio: Paced,
    height: u32,
    seconds: u32,
}

/// Songs as [`Tracks`] has them, and music videos.
struct Videos {
    songs: Tracks,
    videos: HashMap<String, Video>,
}

#[async_trait]
impl Provider for Videos {
    async fn open(
        &self,
        cancel: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<Session, PipelineError> {
        let Some(v) = self.videos.get(&req.asset_id).cloned() else {
            return self.songs.open(cancel, req).await;
        };
        let track = |kind, codec, source: Paced| Track {
            kind,
            codec,
            codec_string: if kind == StreamKind::Video {
                "avc1.64001f".into()
            } else {
                "mp4a.40.2".into()
            },
            sample_rate: if kind == StreamKind::Audio { 44_100 } else { 0 },
            bit_depth: 0,
            bit_rate: 0,
            channel_count: if kind == StreamKind::Audio { 2 } else { 0 },
            codec_mime_type: String::new(),
            spatial_audio: None,
            open: Arc::new(Ready(Stream {
                source: Arc::new(source),
                stages: Vec::new(),
                kind,
                codec,
                duration_ms: 0,
                codec_string: None,
            })),
        };
        Ok(Session {
            kind: SessionKind::MusicVideo,
            metadata: Metadata {
                title: req.asset_id.clone(),
                duration_ms: v.seconds * 1000,
                ..Metadata::default()
            },
            tracks: vec![
                track(StreamKind::Video, Codec::H264, v.video),
                track(StreamKind::Audio, Codec::Aac, v.audio),
            ],
            video_heights: vec![v.height],
            mv_progressive_url: None,
            mv_download_key: None,
        })
    }
}

/// The frame number drawn in a ring frame of a 720-line (or scaled) picture.
fn frame_code(buf: &[u8], width: u32, height: u32) -> u64 {
    let s = SQUARE * height / 720;
    (0..BITS)
        .filter(|k| {
            let (x, y) = (k * s + s / 2, s / 2);
            buf[((y * width + x) * 4 + 1) as usize] > 128
        })
        .map(|k| 1 << k)
        .sum()
}

/// One frame the UI would have shown.
#[derive(Debug, Clone, Copy)]
struct Shown {
    /// The picture's frame number.
    code: u64,
    /// Unix ms when the deck presented it.
    at_ms: f64,
    /// The position the UI shows at that moment (`positionMs` + time since `positionAt`).
    ui_ms: f64,
}

impl Shown {
    /// Video time on screen minus the position the UI shows: the A/V sync error as the UI sees it.
    fn error_ms(&self) -> f64 {
        self.code as f64 * 1000.0 / FPS as f64 - self.ui_ms
    }
}

/// What the UI does: follows `PlayerState.video` and reads the ring's newest frame.
struct Screen {
    surface: Option<VideoSurface>,
    reader: Option<Reader>,
    buf: Vec<u8>,
    /// Rings opened.
    opened: usize,
}

impl Screen {
    fn new() -> Self {
        Self {
            surface: None,
            reader: None,
            buf: Vec::new(),
            opened: 0,
        }
    }

    /// Opens the ring the state names; drops it when the state has none.
    fn follow(&mut self, s: &PlayerState) {
        if s.video == self.surface {
            return;
        }
        self.surface = s.video.clone();
        self.reader = self
            .surface
            .as_ref()
            .map(|v| Reader::open(Path::new(&v.ring)).expect("the ring in the state opens"));
        self.opened += usize::from(self.reader.is_some());
    }

    fn poll(&mut self, s: &PlayerState) -> Option<Shown> {
        self.follow(s);
        let r = self.reader.as_mut()?;
        let Read::Frame(f) = r.read(&mut self.buf) else {
            return None;
        };
        let fmt = r.format();
        let at_ms = f.presented_at_us as f64 / 1000.0;
        let ui_ms = if s.status == Status::Playing {
            s.position_ms as f64 + (at_ms - s.position_at as f64)
        } else {
            s.position_ms as f64
        };
        Some(Shown {
            code: frame_code(&self.buf, fmt.width, fmt.height),
            at_ms,
            ui_ms,
        })
    }

    /// Frames shown during `d`.
    async fn watch(&mut self, player: &rustet_player::PlayerHandle, d: Duration) -> Vec<Shown> {
        let end = Instant::now() + d;
        let mut shown = Vec::new();
        while Instant::now() < end {
            shown.extend(self.poll(&player.state()));
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        shown
    }

    /// After a seek: waits for the first frame of a new ring (every seek makes one) and returns
    /// how long that took and where it landed (its video time minus `target_ms`).
    async fn landing(
        &mut self,
        player: &rustet_player::PlayerHandle,
        target_ms: u64,
        limit: Duration,
    ) -> Option<(Duration, f64)> {
        let before = self.surface.as_ref().map(|v| v.generation);
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            let s = player.state();
            if let Some(f) = self.poll(&s)
                && self.surface.as_ref().map(|v| v.generation) != before
            {
                return Some((
                    t0.elapsed(),
                    f.code as f64 * 1000.0 / FPS as f64 - target_ms as f64,
                ));
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        None
    }
}

struct Stats {
    fps: f64,
    p5: f64,
    median: f64,
    p95: f64,
    forward: bool,
}

fn stats(shown: &[Shown]) -> Stats {
    let span = (shown.last().unwrap().at_ms - shown[0].at_ms) / 1000.0;
    let mut errors: Vec<f64> = shown.iter().map(Shown::error_ms).collect();
    errors.sort_by(f64::total_cmp);
    let pct = |p: f64| errors[((errors.len() - 1) as f64 * p) as usize];
    Stats {
        fps: (shown.len() - 1) as f64 / span,
        p5: pct(0.05),
        median: pct(0.5),
        p95: pct(0.95),
        forward: shown.windows(2).all(|w| w[1].code > w[0].code),
    }
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.1} fps, sync (video − UI position) p5 {:.0} / median {:.0} / p95 {:.0} ms",
            self.fps, self.p5, self.median, self.p95
        )
    }
}

async fn engine_with_videos(
    dir: &Path,
    videos: Videos,
) -> (Arc<Engine>, Router, Arc<Mutex<Vec<String>>>) {
    let e = live_engine(dir, None, Arc::new(videos)).await;
    let (_, log) = start_live_player(&e).await;
    let app = app(&e);
    (e, app, log)
}

fn aac_song(dir: &Path, name: &str, seconds: u32) -> (String, (Codec, Paced)) {
    let fixture = Arc::new(ffmpeg_track(
        dir,
        name,
        &["-c:a", "aac", "-b:a", "256k"],
        seconds + 1,
        seconds,
        250,
    ));
    (
        name.to_owned(),
        (
            Codec::Aac,
            Paced {
                fixture,
                speed: 1.0,
                from: 0,
            },
        ),
    )
}

/// Audio → music video → audio on real decks: the video's frames reach the ring at the source
/// rate and match the position the UI shows (the sync check); pause holds the frame; a seek in
/// the downloaded part and one beyond it show the right frame; the hand-overs in and out of the
/// video work and its ring goes at the end.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs libvlc and ffmpeg with libx264"]
async fn a_music_video_between_two_songs() {
    let dir = tempfile::tempdir().unwrap();
    const SECONDS: u32 = 60;
    let video = Video {
        video: Paced {
            fixture: Arc::new(video_fixture(dir.path(), SECONDS, 1280, 720)),
            speed: DOWNLOAD_SPEED,
            from: 0,
        },
        audio: Paced {
            fixture: Arc::new(audio_fixture(dir.path(), SECONDS)),
            speed: DOWNLOAD_SPEED,
            from: 0,
        },
        height: 720,
        seconds: SECONDS,
    };
    let songs = Tracks(HashMap::from([
        aac_song(dir.path(), "a1", 6),
        aac_song(dir.path(), "a2", 6),
    ]));
    let (e, app, log) = engine_with_videos(
        dir.path(),
        Videos {
            songs,
            videos: HashMap::from([("mv".to_owned(), video)]),
        },
    )
    .await;
    let player = e.player.get().unwrap().clone();
    let item = |id: &str, kind: &str, seconds: u64| json!({"id": id, "kind": kind, "meta": {"name": id, "durationMs": seconds * 1000, "audioTraits": ["lossy-stereo"]}});
    let items = json!([
        item("a1", "song", 6),
        item("mv", "musicVideo", SECONDS as u64),
        item("a2", "song", 6)
    ]);
    post(
        &app,
        "/api/v1/player/queue",
        json!({"items": items, "startIndex": 0}),
    )
    .await;
    until(&app, "a1 plays", 10, |s| {
        s["status"] == "playing" && s["track"]["id"] == "a1"
    })
    .await;
    assert!(
        state(&app).await["video"].is_null(),
        "a song shows no video"
    );

    // Into the video by the gapless hand-over.
    let s = until(&app, "the video plays with a ring", 20, |s| {
        s["track"]["id"] == "mv" && !s["video"].is_null()
    })
    .await;
    let t_video = Instant::now();
    eprintln!(
        "music video: state video {} quality {}",
        s["video"], s["quality"]
    );
    assert_eq!(
        (s["video"]["width"].as_u64(), s["video"]["height"].as_u64()),
        (Some(1280), Some(720))
    );
    let first_ring = s["video"]["ring"].as_str().unwrap().to_owned();
    let mut screen = Screen::new();
    let shown = screen.watch(&player, Duration::from_secs(6)).await;
    let run = stats(&shown);
    eprintln!("music video, first 6 s: {} frames, {run}", shown.len());
    assert!(run.forward, "frame numbers go forward");
    assert!((27.0..=33.0).contains(&run.fps), "{run}");
    assert!(
        run.median.abs() < 100.0 && run.p5 > -250.0 && run.p95 < 250.0,
        "out of sync with the position the UI shows: {run}"
    );

    // Pause holds the frame.
    post(&app, "/api/v1/player/pause", json!({})).await;
    until(&app, "paused", 5, |s| s["status"] == "paused").await;
    screen.watch(&player, Duration::from_millis(300)).await;
    let held = screen.watch(&player, Duration::from_secs(1)).await;
    eprintln!("paused: {} new frames in 1 s", held.len());
    assert!(held.is_empty(), "the paused picture stays: {held:?}");
    assert!(
        !state(&app).await["video"].is_null(),
        "the ring stays while paused"
    );
    post(&app, "/api/v1/player/play", json!({})).await;
    until(&app, "playing again", 5, |s| s["status"] == "playing").await;

    // A seek back, inside the downloaded part.
    let downloaded = t_video.elapsed().as_secs_f64();
    for target in [2500, 5000, 1000] {
        post(&app, "/api/v1/player/seek", json!({"positionMs": target})).await;
        let back = screen
            .landing(&player, target, Duration::from_secs(8))
            .await;
        eprintln!(
            "seek to {target} ms with ~{downloaded:.0} s downloaded: first frame after, landing error (ms) {back:?}"
        );
        assert!(
            back.is_some_and(|(_, e)| e.abs() < 150.0),
            "seek to {target}: deck calls {:?}",
            log.lock().unwrap()
        );
    }
    let after = stats(&screen.watch(&player, Duration::from_secs(3)).await);
    eprintln!("after the seek back: {after}");
    assert!(after.forward && after.median.abs() < 150.0, "{after}");

    // A seek beyond the downloaded part.
    let written_s = 3.0 * t_video.elapsed().as_secs_f64();
    post(&app, "/api/v1/player/seek", json!({"positionMs": 55_000})).await;
    let ahead = screen
        .landing(&player, 55_000, Duration::from_secs(10))
        .await;
    eprintln!(
        "seek ahead to 55 s with ~{written_s:.0} s downloaded: first frame after, landing error (ms) {ahead:?}"
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|l| l == "B seek 55000: Err(NeedsReload)"),
        "beyond the spool: a reload"
    );
    assert!(
        ahead.is_some_and(|(_, e)| e.abs() < 150.0),
        "seek ahead: deck calls {:?}",
        log.lock().unwrap()
    );
    let after = stats(&screen.watch(&player, Duration::from_secs(3)).await);
    eprintln!("after the seek ahead: {after}");
    assert!(after.forward && after.median.abs() < 150.0, "{after}");

    // To the end and out into a2: the ring goes.
    let s = until(&app, "a2 plays", 20, |s| {
        s["status"] == "playing" && s["track"]["id"] == "a2"
    })
    .await;
    assert!(s["video"].is_null(), "no video after the music video");
    let last_ring = screen
        .surface
        .as_ref()
        .map(|v| v.ring.clone())
        .unwrap_or(first_ring);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!Path::new(&last_ring).exists(), "the ring file is gone");
    assert_eq!(
        std::fs::read_dir(e.video_dir.as_ref().unwrap())
            .unwrap()
            .count(),
        0,
        "no ring left"
    );
    until(&app, "the queue ends", 20, |s| s["status"] == "stopped").await;
    eprintln!(
        "rings opened by the screen: {}; deck calls {:?}",
        screen.opened,
        log.lock().unwrap()
    );
    e.player.get().unwrap().shutdown().await;
    crate::player::stop_decks(&e).await;
}

/// The process's CPU time (s) and resident memory (MiB).
fn usage() -> (f64, f64) {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat.rsplit_once(") ").unwrap().1.split(' ').collect();
    let ticks: f64 = fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap();
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let rss_kb: f64 = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (ticks / 100.0, rss_kb / 1024.0)
}

/// A 1080p30 music video played for two minutes while the test reads every frame as the UI
/// would: frame rate, sync, CPU and memory of the process (engine, libvlc and the reader).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs libvlc and ffmpeg with libx264; takes about three minutes"]
async fn a_music_video_for_two_minutes() {
    let dir = tempfile::tempdir().unwrap();
    const SECONDS: u32 = 130;
    let video = Video {
        video: Paced {
            fixture: Arc::new(video_fixture(dir.path(), SECONDS, 1920, 1080)),
            speed: DOWNLOAD_SPEED,
            from: 0,
        },
        audio: Paced {
            fixture: Arc::new(audio_fixture(dir.path(), SECONDS)),
            speed: DOWNLOAD_SPEED,
            from: 0,
        },
        height: 1080,
        seconds: SECONDS,
    };
    let videos = Videos {
        songs: Tracks(HashMap::new()),
        videos: HashMap::from([("mv".to_owned(), video)]),
    };
    let (e, app, _) = engine_with_videos(dir.path(), videos).await;
    let player = e.player.get().unwrap().clone();
    let items = json!([{"id": "mv", "kind": "musicVideo", "meta": {"name": "mv", "durationMs": SECONDS * 1000}}]);
    post(
        &app,
        "/api/v1/player/queue",
        json!({"items": items, "startIndex": 0}),
    )
    .await;
    until(&app, "the video plays", 20, |s| {
        s["status"] == "playing" && !s["video"].is_null()
    })
    .await;
    let mut screen = Screen::new();
    let mut all = Vec::new();
    let mut samples = Vec::new();
    let (cpu0, _) = usage();
    let t0 = Instant::now();
    for i in 0..12 {
        let shown = screen.watch(&player, Duration::from_secs(10)).await;
        let (cpu, rss) = usage();
        let s = stats(&shown);
        eprintln!(
            "{:>3} s: {s}; CPU {:.0} % of one core since start; RSS {rss:.0} MiB",
            (i + 1) * 10,
            (cpu - cpu0) / t0.elapsed().as_secs_f64() * 100.0
        );
        samples.push((rss, s.fps));
        all.extend(shown);
    }
    let run = stats(&all);
    let (cpu, _) = usage();
    let cpu_pct = (cpu - cpu0) / t0.elapsed().as_secs_f64() * 100.0;
    let (rss_20s, rss_end) = (samples[1].0, samples.last().unwrap().0);
    eprintln!(
        "two minutes of 1080p30: {} frames, {run}; CPU {cpu_pct:.0} % of one core; RSS {rss_20s:.0} MiB at 20 s, {rss_end:.0} MiB at 120 s",
        all.len()
    );
    assert!(run.forward && (28.0..=32.0).contains(&run.fps), "{run}");
    assert!(
        rss_end - rss_20s < 40.0,
        "memory grows: {rss_20s:.0} → {rss_end:.0} MiB"
    );
    e.player.get().unwrap().shutdown().await;
    crate::player::stop_decks(&e).await;
}
