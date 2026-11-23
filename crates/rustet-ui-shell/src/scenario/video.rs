//! `--scenario video`: a music video in Now Playing from a frame ring this process writes, as
//! the engine would (spec §4.9), without an account. The writer draws a moving bar and a frame
//! counter; the player state is built.
//!
//! Steps: the loading state, 1080p, 720p and 2160p windowed, 720p and 1080p in full screen, a
//! pause, an ended ring, a new ring with two minutes for memory growth, audio only, and Now
//! Playing closed. Measured per step: frames the view presented per second, the CPU of this process without
//! the writer thread (the UI's cost: polling, image upload, drawing), the writer's own CPU, and
//! the resident memory.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use gpui::AppContext as _;
use rustet_framering::{Format, Writer, now_us};
use rustet_player::model::{
    CatalogId, Codec, ItemKind, ItemSource, PlayerState, Quality, QualityTier, QueueItem,
    QueueView, Status, TrackMeta, VideoSurface,
};
use serde_json::json;

use super::{Script, report, songs};

const FPS: f64 = 30.;
const RUN: u8 = 0;
const PAUSE: u8 = 1;
const CLOSE: u8 = 2;
const SUPERSEDE: u8 = 3;

/// A ring and the thread that writes the test pattern into it.
struct TestRing {
    surface: VideoSurface,
    control: Arc<AtomicU8>,
    /// `/proc/<pid>/task/<tid>/stat` of the writer thread, once it runs.
    stat: Arc<std::sync::Mutex<Option<PathBuf>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestRing {
    fn start(generation: u64, width: u32, height: u32) -> Option<Self> {
        let dir = rustet_framering::default_dir().ok()?;
        let mut writer = Writer::create(&dir, generation, Format::bgra(width, height)).ok()?;
        let surface = VideoSurface {
            ring: writer.path().to_string_lossy().into_owned(),
            generation,
            width,
            height,
        };
        let control = Arc::new(AtomicU8::new(RUN));
        let stat = Arc::new(std::sync::Mutex::new(None));
        let (ctl, tid) = (control.clone(), stat.clone());
        let thread = std::thread::Builder::new()
            .name("test-ring".into())
            .spawn(move || {
                if let Ok(me) = std::fs::read_link("/proc/thread-self") {
                    *tid.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(PathBuf::from("/proc").join(me).join("stat"));
                }
                let mut pattern = Pattern::new(width, height);
                let period = Duration::from_secs_f64(1. / FPS);
                let mut next = Instant::now();
                let mut n = 0u64;
                loop {
                    match ctl.load(Ordering::Relaxed) {
                        RUN => {
                            n += 1;
                            pattern.draw(n);
                            writer.write_frame(&pattern.pixels, now_us()).ok();
                        }
                        PAUSE => {}
                        SUPERSEDE => return writer.supersede(),
                        _ => return,
                    }
                    next += period;
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                }
            })
            .ok()?;
        Some(Self {
            surface,
            control,
            stat,
            thread: Some(thread),
        })
    }

    fn set(&self, mode: u8) {
        self.control.store(mode, Ordering::Relaxed);
    }

    /// Ends the ring: closed (the video stopped) or superseded (a ring of another size follows).
    fn end(mut self, mode: u8) {
        self.set(mode);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }

    fn cpu_ticks(&self) -> u64 {
        let path = self.stat.lock().unwrap_or_else(|e| e.into_inner()).clone();
        path.map_or(0, |p| ticks(&p))
    }
}

impl Drop for TestRing {
    fn drop(&mut self) {
        self.set(CLOSE);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

/// A gradient with a moving white bar and a seven-segment frame counter.
struct Pattern {
    width: usize,
    height: usize,
    background: Vec<u8>,
    pixels: Vec<u8>,
    /// The areas drawn over the background last frame (x, y, w, h).
    dirty: Vec<(usize, usize, usize, usize)>,
}

const SEGMENTS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

impl Pattern {
    fn new(width: u32, height: u32) -> Self {
        let (w, h) = (width as usize, height as usize);
        let mut background = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) * 4;
                background[p..p + 4].copy_from_slice(&[
                    (x * 255 / w) as u8,
                    (y * 255 / h) as u8,
                    96,
                    255,
                ]);
            }
        }
        Self {
            width: w,
            height: h,
            pixels: background.clone(),
            background,
            dirty: Vec::new(),
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, bgra: [u8; 4]) {
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.width) {
                let p = (row * self.width + col) * 4;
                self.pixels[p..p + 4].copy_from_slice(&bgra);
            }
        }
        self.dirty.push((x, y, w, h));
    }

    fn draw(&mut self, n: u64) {
        for (x, y, w, h) in std::mem::take(&mut self.dirty) {
            for row in y..(y + h).min(self.height) {
                let a = (row * self.width + x) * 4;
                let b = (row * self.width + (x + w).min(self.width)) * 4;
                self.pixels[a..b].copy_from_slice(&self.background[a..b]);
            }
        }
        let bar = self.width / 40;
        let x = (n as usize * bar / 3) % (self.width - bar);
        self.fill(x, 0, bar, self.height, [255, 255, 255, 255]);
        let dh = self.height / 8;
        let dw = dh * 3 / 5;
        let t = (dh / 9).max(2);
        let gap = dh / 4;
        let (ox, oy) = (gap * 2, gap * 2);
        self.fill(ox, oy, 6 * (dw + gap) + gap, dh + 2 * gap, [0, 0, 0, 255]);
        let digits = format!("{:06}", n % 1_000_000);
        let half = dh / 2;
        for (i, d) in digits.bytes().enumerate() {
            let s = SEGMENTS[usize::from(d - b'0')];
            let x0 = ox + gap + i * (dw + gap);
            let y0 = oy + gap;
            let segments = [
                (x0, y0, dw, t),
                (x0 + dw - t, y0, t, half),
                (x0 + dw - t, y0 + half, t, half),
                (x0, y0 + dh - t, dw, t),
                (x0, y0 + half, t, half),
                (x0, y0, t, half),
                (x0, y0 + half - t / 2, dw, t),
            ];
            for (bit, (x, y, w, h)) in segments.into_iter().enumerate() {
                if s & (1 << bit) != 0 {
                    self.fill(x, y, w, h, [0, 255, 0, 255]);
                }
            }
        }
    }
}

/// utime + stime of a `/proc/…/stat` file, in clock ticks.
fn ticks(path: &std::path::Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    // After the command name in parentheses, utime and stime are the 12th and 13th fields.
    let rest = text.rsplit_once(") ").map_or("", |(_, r)| r);
    let f: Vec<&str> = rest.split_whitespace().collect();
    let get = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    get(11) + get(12)
}

/// Resident memory of this process, in MiB.
fn rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        })
        .map_or(0., |kb| kb / 1024.)
}

/// CPU time in `/proc` is in ticks of 1/100 s (USER_HZ) on Linux.
const TICKS_PER_SECOND: f64 = 100.;

/// Milliseconds since this process started (its start time in `/proc/self/stat` against the
/// system's uptime).
pub(super) fn since_process_start_ms() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = stat.rsplit_once(") ")?.1;
    // The start time is field 22: the 20th after the command name.
    let start_ticks: f64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(((uptime - start_ticks / TICKS_PER_SECOND) * 1000.).round() as u64)
}

/// The idle window's CPU and memory over 30 s, after it settled.
pub(super) async fn idle_cost(s: &Script<'_>) {
    s.wait(6000).await;
    let me = PathBuf::from("/proc/self/stat");
    let (t0, start) = (ticks(&me), Instant::now());
    s.wait(30_000).await;
    let elapsed = start.elapsed().as_secs_f64();
    report(
        "idle",
        json!({
            "cpu_pct": ((ticks(&me) - t0) as f64 / TICKS_PER_SECOND / elapsed * 1000.).round() / 10.,
            "rss_mib": rss_mib().round(),
        }),
    );
}

fn presented(s: &Script<'_>) -> u64 {
    s.update(|sh, cx| {
        sh.now_playing
            .read(cx)
            .video_stats(cx)
            .map_or(0, |(_, stats, _)| stats.presented)
    })
}

/// Frame rate, CPU and memory over `secs` while `ring` plays.
async fn measure(s: &Script<'_>, name: &str, ring: &TestRing, secs: u64) -> serde_json::Value {
    let me = PathBuf::from("/proc/self/stat");
    let (p0, w0, f0, t0) = (ticks(&me), ring.cpu_ticks(), presented(s), Instant::now());
    s.wait(secs * 1000).await;
    let (p1, w1, f1) = (ticks(&me), ring.cpu_ticks(), presented(s));
    let elapsed = t0.elapsed().as_secs_f64();
    let pct = |ticks: u64| (ticks as f64 / TICKS_PER_SECOND / elapsed * 100.).round();
    let size = s.update(|sh, cx| {
        sh.now_playing
            .read(cx)
            .video_stats(cx)
            .and_then(|(_, _, size)| size)
    });
    let value = json!({
        "ring": format!("{}x{}", ring.surface.width, ring.surface.height),
        "shown": size.map(|(w, h)| format!("{w}x{h}")),
        "fps_presented": ((f1 - f0) as f64 / elapsed * 10.).round() / 10.,
        "ui_cpu_pct": pct((p1 - p0).saturating_sub(w1 - w0)),
        "writer_cpu_pct": pct(w1 - w0),
        "rss_mib": rss_mib().round(),
    });
    report(name, value.clone());
    value
}

fn video_state(
    item: &QueueItem,
    video: Option<VideoSurface>,
    status: Status,
    audio_only: bool,
) -> PlayerState {
    let height = video.as_ref().map(|v| v.height);
    PlayerState {
        generation: 5,
        status,
        track: Some(item.clone()),
        position_ms: 30_000,
        position_at: rustet_app::player::unix_ms(),
        duration_ms: item.meta.duration_ms,
        quality: Some(Quality {
            codec: Codec::H264,
            tier: QualityTier::Video,
            bit_depth: None,
            sample_rate: None,
            bitrate: None,
            channels: Some(2),
            spatial: None,
            video_height: height,
            fell_back: false,
        }),
        volume: 70,
        queue: QueueView {
            items: vec![item.clone()],
            current_index: Some(0),
        },
        video,
        mv_audio_only: audio_only,
        ..PlayerState::default()
    }
}

pub(super) async fn run(s: &Script<'_>) {
    let artwork = songs(s)
        .await
        .first()
        .map(|t| t.meta.artwork_url.clone())
        .unwrap_or_default();
    let item = QueueItem {
        uid: 77,
        id: CatalogId("1440833098".into()),
        library_id: None,
        kind: ItemKind::MusicVideo,
        source: ItemSource::User,
        meta: TrackMeta {
            name: "Test pattern".into(),
            artist_name: "Rustet".into(),
            album_name: String::new(),
            artwork_url: artwork,
            duration_ms: 240_000,
            audio_traits: Vec::new(),
            content_rating: None,
        },
        container_id: None,
    };
    let show = |ring: Option<&TestRing>, status: Status, audio_only: bool| {
        s.player(video_state(
            &item,
            ring.map(|r| r.surface.clone()),
            status,
            audio_only,
        ));
    };
    s.update(|sh, cx| {
        sh.now_playing_open = true;
        cx.notify();
    });
    let main = s.update(|sh, _| sh.main);
    let gpu = main.and_then(|m| {
        let mut cx = s.cx.clone();
        cx.update_window(m.into(), |_, window, _| {
            window.gpu_specs().map(|g| {
                json!({ "device": g.device_name, "driver": g.driver_name,
                        "info": g.driver_info, "software": g.is_software_emulated })
            })
        })
        .ok()
        .flatten()
    });
    report("gpu", json!(gpu));

    // Before the engine publishes the ring: the loading state.
    show(None, Status::Opening, false);
    s.wait(600).await;
    s.shot("video-loading").await;

    let Some(ring) = TestRing::start(9001, 1920, 1080) else {
        report("error", json!("the test ring could not be made"));
        return;
    };
    show(Some(&ring), Status::Playing, false);
    s.wait(1500).await;
    measure(s, "video.windowed_1080p", &ring, 20).await;
    s.shot("video-1080p-windowed").await;

    // A size change: a new ring of the same video.
    let Some(small) = TestRing::start(9002, 1280, 720) else {
        return;
    };
    ring.end(SUPERSEDE);
    show(Some(&small), Status::Playing, false);
    s.wait(1500).await;
    measure(s, "video.windowed_720p", &small, 20).await;
    s.shot("video-720p-windowed").await;

    // 4K, the engine's largest variant.
    let Some(big) = TestRing::start(9010, 3840, 2160) else {
        return;
    };
    show(Some(&big), Status::Playing, false);
    s.wait(1500).await;
    measure(s, "video.windowed_2160p", &big, 20).await;
    show(Some(&small), Status::Playing, false);
    big.end(SUPERSEDE);
    s.wait(1500).await;

    s.update(|sh, cx| sh.toggle_video_fullscreen(cx));
    s.wait(2000).await;
    report(
        "video.fullscreen",
        json!(s.update(|sh, _| sh.video_fullscreen)),
    );
    measure(s, "video.fullscreen_720p", &small, 20).await;
    s.shot("video-720p-fullscreen").await;

    let Some(ring) = TestRing::start(9003, 1920, 1080) else {
        return;
    };
    small.end(SUPERSEDE);
    show(Some(&ring), Status::Playing, false);
    s.wait(1500).await;
    measure(s, "video.fullscreen_1080p", &ring, 20).await;
    s.shot("video-1080p-fullscreen").await;
    s.update(|sh, cx| sh.toggle_video_fullscreen(cx));
    s.wait(1500).await;
    report(
        "video.fullscreen_left",
        json!(!s.update(|sh, _| sh.video_fullscreen)),
    );

    // Paused: no new frames, nothing to draw.
    ring.set(PAUSE);
    show(Some(&ring), Status::Paused, false);
    s.wait(1000).await;
    measure(s, "video.paused", &ring, 10).await;
    s.shot("video-paused").await;

    // The ring ends: the last frame stays.
    ring.end(CLOSE);
    s.wait(1000).await;
    let phase = s.update(|sh, cx| {
        sh.now_playing
            .read(cx)
            .video_stats(cx)
            .map(|(p, _, _)| format!("{p:?}"))
    });
    report("video.ended_phase", json!(phase));
    s.shot("video-ended").await;

    // A new ring (a seek), then two minutes for memory growth.
    let Some(ring) = TestRing::start(9004, 1920, 1080) else {
        return;
    };
    show(Some(&ring), Status::Playing, false);
    s.wait(1500).await;
    let mut rss = Vec::new();
    for i in 0..12 {
        let v = measure(s, &format!("video.soak_{i}"), &ring, 10).await;
        rss.extend(v["rss_mib"].as_f64());
    }
    report(
        "video.soak",
        json!({ "rss_first_mib": rss.first(), "rss_last_mib": rss.last(),
                "rss_max_mib": rss.iter().copied().fold(0., f64::max) }),
    );

    // Audio only: the engine stops the ring and the state says so.
    drop(ring);
    show(None, Status::Playing, true);
    s.wait(800).await;
    s.shot("video-audio-only").await;

    // Leaving Now Playing: the reader stops; the deck still says what plays.
    let Some(ring) = TestRing::start(9005, 1920, 1080) else {
        return;
    };
    show(Some(&ring), Status::Playing, false);
    s.wait(1000).await;
    s.update(|sh, cx| {
        sh.now_playing_open = false;
        cx.notify();
    });
    s.wait(1000).await;
    measure(s, "video.left_now_playing", &ring, 10).await;
    s.shot("video-left-now-playing").await;
}
