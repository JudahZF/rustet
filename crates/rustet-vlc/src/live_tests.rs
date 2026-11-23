//! Two-deck checks against a real libvlc: gapless join, crossfade, AAC from a growing file.
//!
//! Run with `cargo test -p rustet-vlc -- --ignored --nocapture`, with `libvlc.so.5` on
//! `LD_LIBRARY_PATH` and ffmpeg in `FFMPEG` or on `PATH`. Nothing is heard: audio goes to
//! libvlc's memory output (amem) and is recorded here. libvlc's `adummy` and `afile` outputs have
//! no clock and play faster than real time, so they cannot show a gap; the recorder below acts
//! as a sound card instead: it holds each block until its play time (`pts`), and drain waits
//! until the last block has played, so libvlc paces itself as with a real device.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use libloading::Library;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::callbacks::tests::MemSource;
use crate::ffi::MediaPlayer;
use crate::{AbortHandle, Deck, DeckEvent, Event, Instance, LoadId, Ramp, Source, Start};

const RATE: u32 = 44_100;
const CHANNELS: usize = 2;
const ARGS: &[&CStr] = &[c"--aout=adummy", c"--vout=vdummy", c"--no-video-title-show"];

type PlayCb = unsafe extern "C" fn(*mut c_void, *const c_void, c_uint, i64);
type PtsCb = unsafe extern "C" fn(*mut c_void, i64);
type DrainCb = unsafe extern "C" fn(*mut c_void);
type VolumeCb = unsafe extern "C" fn(*mut c_void, f32, bool) -> c_int;

/// The libvlc functions only these tests need.
struct Amem {
    set_callbacks: unsafe extern "C" fn(
        *mut MediaPlayer,
        Option<PlayCb>,
        Option<PtsCb>,
        Option<PtsCb>,
        Option<PtsCb>,
        Option<DrainCb>,
        *mut c_void,
    ),
    set_format: unsafe extern "C" fn(*mut MediaPlayer, *const c_char, c_uint, c_uint),
    set_volume_callback: unsafe extern "C" fn(*mut MediaPlayer, Option<VolumeCb>),
    clock: unsafe extern "C" fn() -> i64,
    _lib: Library,
}

fn amem() -> &'static Amem {
    static AMEM: OnceLock<Amem> = OnceLock::new();
    AMEM.get_or_init(|| {
        // SAFETY: libvlc 3 is already loaded by `Instance::with_args`; these symbols have the
        // declared C signatures in libvlc 3.
        unsafe {
            let lib = Library::new("libvlc.so.5").expect("libvlc.so.5");
            Amem {
                set_callbacks: *lib.get(b"libvlc_audio_set_callbacks").unwrap(),
                set_format: *lib.get(b"libvlc_audio_set_format").unwrap(),
                set_volume_callback: *lib.get(b"libvlc_audio_set_volume_callback").unwrap(),
                clock: *lib.get(b"libvlc_clock").unwrap(),
                _lib: lib,
            }
        }
    })
}

/// libvlc's clock in µs: the time base of `pts`.
fn now() -> i64 {
    // SAFETY: no preconditions.
    unsafe { (amem().clock)() }
}

fn ms(us: i64) -> f64 {
    us as f64 / 1000.0
}

/// One block as the emulated sound card played it.
#[derive(Clone, Copy)]
struct Block {
    /// When it started to play: its `pts`, or the arrival time when it came late.
    start: i64,
    frames: usize,
    /// Largest absolute sample, after the volume.
    peak: i16,
}

impl Block {
    fn end(&self) -> i64 {
        self.start + (self.frames as i64 * 1_000_000) / i64::from(RATE)
    }
}

/// Records what libvlc plays. The volume is applied here the way PulseAudio applies libvlc's:
/// a cubic gain, and a level set before the stream starts is kept. (libvlc's own software
/// volume forgets a level set before the output starts.)
#[derive(Default)]
struct Recorder {
    /// As decoded, before the volume.
    samples: Mutex<Vec<i16>>,
    blocks: Mutex<Vec<Block>>,
    gain: Mutex<f32>,
}

impl Recorder {
    /// Routes `deck`'s audio here as 16-bit stereo at 44.1 kHz. Leaked: libvlc keeps the
    /// pointer for the deck's lifetime.
    ///
    /// Installing the callbacks destroys the deck's audio output, and libvlc drops a volume set
    /// while there is none. A deck has one from creation on, so a short warm-up track recreates
    /// it here; an instance deck then keeps it (`Vlc::halt`).
    fn install(deck: &Deck) -> &'static Self {
        let rec: &'static Self = Box::leak(Box::new(Self {
            gain: Mutex::new(1.0),
            ..Self::default()
        }));
        let mp: NonNull<MediaPlayer> = deck.raw();
        let a = amem();
        // SAFETY: `mp` is live; `rec` lives forever; the callbacks match libvlc's prototypes.
        unsafe {
            (a.set_callbacks)(
                mp.as_ptr(),
                Some(play_cb),
                None,
                None,
                None,
                Some(drain_cb),
                std::ptr::from_ref(rec).cast_mut().cast(),
            );
            (a.set_format)(mp.as_ptr(), c"S16N".as_ptr(), RATE, CHANNELS as c_uint);
            (a.set_volume_callback)(mp.as_ptr(), Some(volume_cb));
        }
        deck.load_source(MemSource(Cursor::new(crate::deck::tests::silent_wav(1))))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while deck.timing().state != crate::State::Ended {
            assert!(
                std::time::Instant::now() < deadline,
                "warm-up track never ended"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        deck.stop();
        rec.samples.lock().unwrap().clear();
        rec.blocks.lock().unwrap().clear();
        rec
    }

    fn blocks(&self) -> Vec<Block> {
        self.blocks.lock().unwrap().clone()
    }

    fn samples(&self) -> Vec<i16> {
        self.samples.lock().unwrap().clone()
    }

    /// The loudest sample of the blocks that start in `[from, to)`.
    fn peak(&self, from: i64, to: i64) -> i16 {
        self.blocks()
            .iter()
            .filter(|b| (from..to).contains(&b.start))
            .map(|b| b.peak)
            .max()
            .unwrap_or(0)
    }
}

unsafe extern "C" fn play_cb(data: *mut c_void, samples: *const c_void, count: c_uint, pts: i64) {
    // SAFETY: `data` is the leaked recorder; libvlc passes `count` frames of S16N stereo.
    let (rec, pcm) = unsafe {
        (
            &*data.cast::<Recorder>(),
            std::slice::from_raw_parts(samples.cast::<i16>(), count as usize * CHANNELS),
        )
    };
    let arrived = now();
    let gain = *rec.gain.lock().unwrap();
    let peak = (f32::from(pcm.iter().map(|s| s.saturating_abs()).max().unwrap_or(0)) * gain) as i16;
    rec.samples.lock().unwrap().extend_from_slice(pcm);
    rec.blocks.lock().unwrap().push(Block {
        start: pts.max(arrived),
        frames: count as usize,
        peak,
    });
    if pts > arrived {
        std::thread::sleep(Duration::from_micros((pts - arrived) as u64));
    }
}

unsafe extern "C" fn volume_cb(data: *mut c_void, volume: f32, mute: bool) -> c_int {
    // SAFETY: `data` is the leaked recorder.
    let rec = unsafe { &*data.cast::<Recorder>() };
    *rec.gain.lock().unwrap() = if mute { 0.0 } else { volume.powi(3) };
    0
}

unsafe extern "C" fn drain_cb(data: *mut c_void) {
    // SAFETY: `data` is the leaked recorder.
    let rec = unsafe { &*data.cast::<Recorder>() };
    let end = rec.blocks.lock().unwrap().last().map(Block::end);
    if let Some(wait) = end.map(|end| end - now()).filter(|w| *w > 0) {
        std::thread::sleep(Duration::from_micros(wait as u64));
    }
}

/// Deck events stamped with libvlc's clock on arrival.
struct Inbox(mpsc::Receiver<(i64, DeckEvent)>);

impl Inbox {
    fn new(mut rx: UnboundedReceiver<DeckEvent>) -> Self {
        let (tx, inbox) = mpsc::channel();
        std::thread::spawn(move || {
            while let Some(ev) = rx.blocking_recv() {
                if tx.send((now(), ev)).is_err() {
                    return;
                }
            }
        });
        Self(inbox)
    }

    /// Waits for the event `want` of `deck`'s load `load`; returns its arrival time.
    fn wait(
        &self,
        deck: &Deck,
        load: LoadId,
        timeout: Duration,
        want: impl Fn(Event) -> bool,
    ) -> i64 {
        let mut log = Vec::new();
        self.find(deck, load, timeout, want, &mut log)
            .unwrap_or_else(|| panic!("timed out; events: {log:?}"))
    }

    /// Like `wait`, but `None` on timeout, keeping every event of that load in `log`.
    fn find(
        &self,
        deck: &Deck,
        load: LoadId,
        timeout: Duration,
        want: impl Fn(Event) -> bool,
        log: &mut Vec<(i64, Event)>,
    ) -> Option<i64> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let (at, ev) = self.0.recv_timeout(left).ok()?;
            if ev.deck == deck.id() && ev.load == load {
                log.push((at, ev.event));
                if want(ev.event) {
                    return Some(at);
                }
            }
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rustet-vlc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ffmpeg() -> Command {
    let mut cmd = Command::new(std::env::var_os("FFMPEG").unwrap_or_else(|| "ffmpeg".into()));
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    cmd
}

fn run(cmd: &mut Command) {
    assert!(
        cmd.status().expect("ffmpeg not found").success(),
        "ffmpeg failed: {cmd:?}"
    );
}

fn mem(path: &Path) -> MemSource {
    MemSource(Cursor::new(std::fs::read(path).unwrap()))
}

fn pcm_s16(path: &Path) -> Vec<i16> {
    std::fs::read(path)
        .unwrap()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

/// A sine sweep cut into two fragmented-MP4 ALAC files (the shape of the engine's lossless cache)
/// at an odd sample is played as two tracks on two decks:
/// the second is pre-rolled paused and resumed at the first's `EndReached`. Every sample must
/// arrive once, in order; the time gap between the last sample of A and the first of B must be
/// inaudible (≤ 50 ms, the spec's target).
#[test]
#[ignore = "needs libvlc and ffmpeg"]
fn gapless_join_on_two_decks() {
    let dir = TempDir::new("gapless");
    let (pcm, a_file, b_file) = (
        dir.0.join("sweep.raw"),
        dir.0.join("a.m4a"),
        dir.0.join("b.m4a"),
    );
    const SPLIT: usize = 3 * RATE as usize + 1234; // not on an ALAC packet boundary
    let sweep =
        "aevalsrc=exprs=0.5*sin(2*PI*(110*t+300*t*t))|0.5*sin(2*PI*(110*t+300*t*t)):s=44100:d=12";
    run(ffmpeg()
        .args([
            "-f",
            "lavfi",
            "-i",
            sweep,
            "-f",
            "s16le",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&pcm));
    let raw = ["-f", "s16le", "-ar", "44100", "-ac", "2", "-i"];
    let cut_a = format!("atrim=end_sample={SPLIT}");
    let cut_b = format!("atrim=start_sample={SPLIT},asetpts=PTS-STARTPTS");
    let fmp4 = [
        "-c:a",
        "alac",
        "-f",
        "mp4",
        "-movflags",
        "+empty_moov+default_base_moof",
        "-frag_duration",
        "2000000",
    ];
    run(ffmpeg()
        .args(raw)
        .arg(&pcm)
        .args(["-af", &cut_a])
        .args(fmp4)
        .arg(&a_file));
    run(ffmpeg()
        .args(raw)
        .arg(&pcm)
        .args(["-af", &cut_b])
        .args(fmp4)
        .arg(&b_file));
    let want = pcm_s16(&pcm);

    let inst = Instance::with_args(ARGS).expect("libvlc unavailable");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (a, b) = (inst.deck(tx.clone()).unwrap(), inst.deck(tx).unwrap());
    let (rec_a, rec_b) = (Recorder::install(&a), Recorder::install(&b));
    a.set_volume(100);
    b.set_volume(100);
    let inbox = Inbox::new(rx);

    let la = a.open_source(mem(&a_file), Start::Playing).unwrap();
    inbox.wait(&a, la, Duration::from_secs(5), |e| e == Event::Playing);
    std::thread::sleep(Duration::from_secs(1));

    let cued = now();
    let lb = b.open_source(mem(&b_file), Start::Paused).unwrap();
    let mut log = Vec::new();
    // A media that libvlc reads to its end while paused (a short one) reports full buffering
    // only once resumed, so a missing 100 % is not an error.
    let ready = inbox.find(
        &b,
        lb,
        Duration::from_secs(1),
        |e| e == Event::Buffering(100),
        &mut log,
    );
    assert!(
        log.iter().any(|(_, e)| *e == Event::Paused),
        "pre-roll not paused: {log:?}"
    );
    assert!(rec_b.blocks().is_empty(), "the pre-rolled deck made sound");

    let ended = inbox.wait(&a, la, Duration::from_secs(10), |e| e == Event::EndReached);
    b.resume();
    let resumed = now();
    inbox.wait(&b, lb, Duration::from_secs(10), |e| e == Event::EndReached);

    let (blocks_a, blocks_b) = (rec_a.blocks(), rec_b.blocks());
    let a_end = blocks_a.last().unwrap().end();
    let b_start = blocks_b.first().unwrap().start;
    let gap = b_start - a_end;
    eprintln!(
        "gapless: cue→buffered {:?} ms; A last sample → EndReached {:.1} ms; EndReached → resume {:.2} ms; \
         resume → B first sample {:.1} ms; gap {:.1} ms; pre-roll events {:?}",
        ready.map(|r| ms(r - cued)),
        ms(ended - a_end),
        ms(resumed - ended),
        ms(b_start - resumed),
        ms(gap),
        log.iter()
            .map(|(at, e)| (format!("{:.0}ms", ms(at - cued)), *e))
            .collect::<Vec<_>>(),
    );

    let mut got = rec_a.samples();
    assert_eq!(got.len(), SPLIT * CHANNELS, "deck A lost or added samples");
    got.extend(rec_b.samples());
    assert!(
        got == want,
        "joined samples differ from the sweep ({} vs {} values)",
        got.len(),
        want.len()
    );
    assert!(gap <= 50_000, "audible gap: {:.1} ms", ms(gap));
}

/// Crossfade: both decks play at once, each with its own volume ramp driven by the caller.
#[test]
#[ignore = "needs libvlc and ffmpeg"]
fn crossfade_ramps_each_deck() {
    let dir = TempDir::new("xfade");
    let (a_file, b_file) = (dir.0.join("a.wav"), dir.0.join("b.wav"));
    for (file, freq) in [(&a_file, 440), (&b_file, 660)] {
        let src = format!("sine=frequency={freq}:sample_rate=44100:duration=4");
        run(ffmpeg()
            .args(["-f", "lavfi", "-i", &src, "-ac", "2"])
            .arg(file));
    }
    let inst = Instance::with_args(ARGS).expect("libvlc unavailable");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (a, b) = (inst.deck(tx.clone()).unwrap(), inst.deck(tx).unwrap());
    let (rec_a, rec_b) = (Recorder::install(&a), Recorder::install(&b));
    let inbox = Inbox::new(rx);
    a.set_volume(100);
    let la = a.open_source(mem(&a_file), Start::Playing).unwrap();
    inbox.wait(&a, la, Duration::from_secs(5), |e| e == Event::Playing);
    // The incoming deck: cued at volume 0, then started.
    b.set_volume(0);
    let lb = b.open_source(mem(&b_file), Start::Paused).unwrap();
    inbox.wait(&b, lb, Duration::from_secs(5), |e| {
        e == Event::Buffering(100)
    });
    b.resume();
    std::thread::sleep(Duration::from_millis(800));

    let out = Ramp {
        from: 100,
        to: 0,
        duration: Duration::from_secs(1),
    };
    let fade_in = Ramp {
        from: 0,
        to: 100,
        duration: Duration::from_secs(1),
    };
    let start = std::time::Instant::now();
    let t0 = now();
    loop {
        let elapsed = start.elapsed();
        a.set_volume(i64::from(out.level_at(elapsed)));
        b.set_volume(i64::from(fade_in.level_at(elapsed)));
        if elapsed >= out.duration {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let t1 = now();
    std::thread::sleep(Duration::from_millis(600));
    a.stop();
    b.stop();

    let s = 1_000_000 / 10; // 100 ms windows
    let mid = (t0 + t1) / 2;
    let full_a = rec_a.peak(t0 - 3 * s, t0);
    let (before_b, mid_a, mid_b) = (
        rec_b.peak(t0 - 3 * s, t0),
        rec_a.peak(mid - s / 2, mid + s / 2),
        rec_b.peak(mid - s / 2, mid + s / 2),
    );
    let (after_a, full_b) = (
        rec_a.peak(t1 + s, t1 + 4 * s),
        rec_b.peak(t1 + s, t1 + 4 * s),
    );
    eprintln!(
        "crossfade peaks: A before {full_a}, mid {mid_a}, after {after_a}; B before {before_b}, mid {mid_b}, after {full_b}"
    );
    assert!(
        full_a > 2000 && full_b > 2000,
        "a deck at volume 100 is too quiet"
    ); // the sine peaks at 1/8
    assert_eq!((before_b, after_a), (0, 0), "a deck at volume 0 made sound");
    // Level 50 is a gain of 0.5³ = 0.125 (libvlc's cubic volume curve).
    for (mid, full) in [(mid_a, full_a), (mid_b, full_b)] {
        let ratio = f64::from(mid) / f64::from(full);
        assert!(
            (0.05..0.3).contains(&ratio),
            "mid-ramp gain {ratio:.3}, want about 0.125"
        );
    }
}

/// A file that is still being written, like the engine's cache entry during a
/// download: reads at the written end wait for more until the writer exits.
struct Growing {
    file: File,
    writer_done: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let done = self.writer_done.load(Ordering::Acquire);
            let n = self.file.read(buf)?;
            if n > 0 || done {
                return Ok(n);
            }
            if self.abort.load(Ordering::Acquire) {
                return Err(io::Error::other("aborted"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Seek for Growing {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl Source for Growing {
    fn size(&self) -> Option<u64> {
        None // like the engine's streaming reader before the entry commits
    }

    fn abort_handle(&self) -> AbortHandle {
        let abort = Arc::clone(&self.abort);
        AbortHandle::new(move || abort.store(true, Ordering::Release))
    }
}

const AAC_SECONDS: u64 = 20;

/// 20 s of fragmented MP4 made by ffmpeg: AAC-LC 256 kbit/s, 44.1 kHz stereo, 2 s fragments, no
/// sidx or mfra, and a moov whose duration is 0. That is the shape of the engine's decrypted AAC
/// (Apple's CMAF moov says 0). With `duration`, mvhd gets the track length, as the lossless path
/// writes it (`patch_moov_duration` in rustet-fairplay).
fn aac_fmp4(dir: &Path, duration: bool) -> Vec<u8> {
    let path = dir.join("track.m4a");
    let src = format!("sine=frequency=440:sample_rate=44100:duration={AAC_SECONDS}");
    run(ffmpeg()
        .args([
            "-f",
            "lavfi",
            "-i",
            &src,
            "-ac",
            "2",
            "-c:a",
            "aac",
            "-profile:a",
            "aac_low",
            "-b:a",
            "256k",
        ])
        .args([
            "-f",
            "mp4",
            "-movflags",
            "+empty_moov+default_base_moof",
            "-frag_duration",
            "2000000",
        ])
        .arg(&path));
    let mut data = std::fs::read(&path).unwrap();
    let at = data.windows(4).position(|w| w == b"mvhd").expect("mvhd") + 4;
    // Version 0: flags, creation, modification, timescale, duration (u32 each).
    assert_eq!(data[at], 0, "mvhd version");
    let duration_at = at + 16;
    assert_eq!(
        &data[duration_at..duration_at + 4],
        [0; 4],
        "ffmpeg's empty_moov duration is 0"
    );
    if duration {
        let timescale = u32::from_be_bytes(data[at + 12..at + 16].try_into().unwrap());
        let length = timescale * AAC_SECONDS as u32;
        data[duration_at..duration_at + 4].copy_from_slice(&length.to_be_bytes());
    }
    data
}

/// Writes `data` to `path` in 4 KiB pieces at `speed` × the 256 kbit/s play rate, like the
/// engine's cache download; returns the writer and the flag it sets when done.
fn grow(path: &Path, data: Vec<u8>, speed: u64) -> (std::thread::JoinHandle<()>, Arc<AtomicBool>) {
    let mut file = File::create(path).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let pause = Duration::from_secs(AAC_SECONDS) / (data.len() as u32 / 4096 + 1) / speed as u32;
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        for piece in data.chunks(4096) {
            file.write_all(piece).unwrap();
            std::thread::sleep(pause);
        }
        flag.store(true, Ordering::Release);
    });
    (writer, done)
}

struct AacRun {
    to_playing: i64,
    to_sound: i64,
    write_time: i64,
    deck: Deck,
    load: LoadId,
    inbox: Inbox,
    rec: &'static Recorder,
    writer: std::thread::JoinHandle<()>,
    _dir: TempDir,
    _inst: Instance,
}

/// Starts writing the AAC file at `speed` and plays it through the `Source` path at once.
fn play_growing_aac(name: &str, duration: bool, speed: u64) -> AacRun {
    let dir = TempDir::new(name);
    let data = aac_fmp4(&dir.0, duration);
    let path = dir.0.join("growing.m4a");
    let inst = Instance::with_args(ARGS).expect("libvlc unavailable");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let deck = inst.deck(tx).unwrap();
    let rec = Recorder::install(&deck);
    deck.set_volume(100);
    let inbox = Inbox::new(rx);

    let started = now();
    let (writer, writer_done) = grow(&path, data, speed);
    let src = Growing {
        file: File::open(&path).unwrap(),
        writer_done: Arc::clone(&writer_done),
        abort: Arc::default(),
    };
    let load = deck.open_source(src, Start::Playing).unwrap();
    let playing = inbox.wait(&deck, load, Duration::from_secs(AAC_SECONDS + 10), |e| {
        e == Event::Playing
    });
    inbox.wait(
        &deck,
        load,
        Duration::from_secs(5),
        |e| matches!(e, Event::TimeChanged(t) if t > 0),
    );
    let to_sound = rec.blocks().first().map(|b| b.start).expect("no audio") - started;
    let write_time = AAC_SECONDS as i64 * 1_000_000 / speed as i64;
    AacRun {
        to_playing: playing - started,
        to_sound,
        write_time,
        deck,
        load,
        inbox,
        rec,
        writer,
        _dir: dir,
        _inst: inst,
    }
}

/// Seeks to `target` and reports how long until the position lands within 2 s of it.
fn seek_report(run: &AacRun, target: i64) -> String {
    let asked = now();
    run.deck.set_time(target);
    let mut log = Vec::new();
    let landed = |e| matches!(e, Event::TimeChanged(t) if (target..target + 2000).contains(&t));
    match run.inbox.find(
        &run.deck,
        run.load,
        Duration::from_secs(AAC_SECONDS + 5),
        landed,
        &mut log,
    ) {
        Some(at) => format!("landed after {:.0} ms", ms(at - asked)),
        None => format!("never landed; then {:?}", run.deck.timing()),
    }
}

/// AAC fMP4 with its length in mvhd plays from a file still being written at real-time rate.
/// Seeks are reported, not checked: see `aac_seek_needs_the_whole_file`.
#[test]
#[ignore = "needs libvlc and ffmpeg"]
fn aac_plays_from_a_growing_file() {
    let run = play_growing_aac("aac-grow", true, 1);
    run.inbox
        .wait(&run.deck, run.load, Duration::from_secs(10), |e| {
            matches!(e, Event::TimeChanged(4000..))
        });
    let back = seek_report(&run, 1000);
    let peak = run.rec.blocks().iter().map(|b| b.peak).max().unwrap_or(0);
    eprintln!(
        "aac growing at 1x: open→Playing {:.0} ms, open→first sound {:.0} ms; length {} ms; seek back to 1 s at 4 s: {back}; peak {peak}",
        ms(run.to_playing),
        ms(run.to_sound),
        run.deck.timing().length_ms,
    );
    run.deck.stop();
    run.writer.join().unwrap();
    assert!(peak > 1000, "no signal in the decoded AAC");
    assert!(
        run.to_playing < 3_000_000,
        "start-up took {:.0} ms",
        ms(run.to_playing)
    );
}

/// Seeks in AAC fMP4 (no sidx or mfra) once the file is complete: back, and forward.
#[test]
#[ignore = "needs libvlc and ffmpeg"]
fn aac_seek_needs_the_whole_file() {
    let run = play_growing_aac("aac-seek", true, 100);
    run.inbox
        .wait(&run.deck, run.load, Duration::from_secs(10), |e| {
            matches!(e, Event::TimeChanged(4000..))
        });
    let back = seek_report(&run, 1000);
    std::thread::sleep(Duration::from_millis(300));
    let fwd = seek_report(&run, 15_000);
    eprintln!("aac complete: seek back to 1 s: {back}; seek forward to 15 s: {fwd}");
    run.deck.stop();
    run.writer.join().unwrap();
}

/// Pins the libvlc behaviour behind the mvhd duration: with duration 0 (and no mehd or sidx),
/// libvlc's MP4 demuxer reads every fragment at open to build an index, so a growing file only
/// starts once it is complete.
#[test]
#[ignore = "needs libvlc and ffmpeg"]
fn aac_without_duration_waits_for_the_whole_file() {
    let run = play_growing_aac("aac-nodur", false, 4);
    eprintln!(
        "aac growing at 4x, moov duration 0: open→Playing {:.0} ms, open→first sound {:.0} ms, write time {:.0} ms",
        ms(run.to_playing),
        ms(run.to_sound),
        ms(run.write_time),
    );
    run.deck.stop();
    run.writer.join().unwrap();
    assert!(
        run.to_playing > run.write_time - 500_000,
        "started before the file was complete"
    );
}

// ── Video ───────────────────────────────────────────────────────────────────

/// Frame rate, size and the frame-number code of the video fixtures.
const MV_FPS: u64 = 30;
const MV_SQUARE: u32 = 48;
const MV_BITS: u32 = 13;

/// `seconds` of 1280×720 H.264 at 30 fps with a 440 Hz tone. Frame n shows n in binary as 13
/// white squares of 48 px along the top edge (bit k at x = 48k), on dark grey.
fn mv_fixture(dir: &Path, seconds: u32) -> PathBuf {
    let path = dir.join("mv.mp4");
    let mut filter = format!("color=c=0x202020:s=1280x720:r={MV_FPS}:d={seconds}");
    for k in 0..MV_BITS {
        filter += &format!(
            ",drawbox=x={}:y=0:w={MV_SQUARE}:h={MV_SQUARE}:color=white:t=fill:enable='eq(mod(floor(n/{}),2),1)'",
            k * MV_SQUARE,
            1u64 << k
        );
    }
    let tone = format!("sine=frequency=440:sample_rate=44100:duration={seconds}");
    run(ffmpeg()
        .args(["-f", "lavfi", "-i", &filter, "-f", "lavfi", "-i", &tone])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-g", "30", "-pix_fmt", "yuv420p",
        ])
        .args([
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-ac",
            "2",
            "-movflags",
            "+faststart",
        ])
        .arg(&path));
    path
}

/// The frame number coded in a ring frame (packed BGRA rows) of a 720-line source.
fn frame_code(buf: &[u8], width: u32, height: u32) -> u64 {
    let s = MV_SQUARE * height / 720;
    (0..MV_BITS)
        .filter(|k| {
            let (x, y) = (k * s + s / 2, s / 2);
            buf[((y * width + x) * 4 + 1) as usize] > 128
        })
        .map(|k| 1 << k)
        .sum()
}

/// The audio played at `t` (libvlc clock), as media time in ms: the samples of the blocks that
/// started before `t`, plus the part of the block playing at `t`.
fn audio_ms_at(blocks: &[Block], t: i64) -> Option<f64> {
    let mut frames = 0usize;
    for b in blocks {
        if b.start > t {
            break;
        }
        if t < b.end() {
            return Some(
                (frames as f64 + (t - b.start) as f64 * f64::from(RATE) / 1e6) * 1000.0
                    / f64::from(RATE),
            );
        }
        frames += b.frames;
    }
    None
}

/// A music video on a deck with a clocked audio output: frames reach the ring at the source rate,
/// scaled to the height limit, in sync with the audio that plays; pause holds the frame; stop
/// closes the ring.
#[test]
#[ignore = "needs libvlc and ffmpeg"]
fn video_frames_follow_the_audio_clock() {
    let dir = TempDir::new("video");
    let file = mv_fixture(&dir.0, 8);
    let rings = dir.0.join("rings");
    rustet_framering::ensure_private_dir(&rings).unwrap();

    let inst = Instance::with_args(ARGS).expect("libvlc unavailable");
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let deck = inst.deck(tx).unwrap();
    let rec = Recorder::install(&deck);
    deck.set_volume(100);
    let inbox = Inbox::new(rx);
    let load = deck
        .open_video(
            mem(&file),
            Start::Playing,
            0,
            crate::VideoOutput {
                dir: rings.clone(),
                max_height: 480,
                source_size: Some((1280, 720)),
            },
        )
        .unwrap();
    inbox.wait(&deck, load, Duration::from_secs(5), |e| {
        matches!(e, Event::VideoStarted { .. })
    });
    let ring = deck.video_ring().expect("a ring");
    assert_eq!(
        (ring.width, ring.height),
        (848, 480),
        "720p capped to 480 lines"
    );
    let mut reader = rustet_framering::Reader::open(&ring.path).unwrap();

    // libvlc's clock against the ring's Unix clock.
    let offset = rustet_framering::now_us() as i64 - now();
    let mut buf = Vec::new();
    let mut frames = Vec::new();
    let t0 = std::time::Instant::now();
    while t0.elapsed() < Duration::from_secs(4) {
        if let rustet_framering::Read::Frame(f) = reader.read(&mut buf) {
            frames.push((
                frame_code(&buf, ring.width, ring.height),
                f.presented_at_us as i64 - offset,
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let blocks = rec.blocks();
    let span = (frames.last().unwrap().1 - frames[0].1) as f64 / 1e6;
    let fps = (frames.len() - 1) as f64 / span;
    let mut errors: Vec<f64> = frames
        .iter()
        .filter_map(|&(n, at)| {
            audio_ms_at(&blocks, at).map(|a| n as f64 * 1000.0 / MV_FPS as f64 - a)
        })
        .collect();
    errors.sort_by(f64::total_cmp);
    let pct = |p: f64| errors[((errors.len() - 1) as f64 * p) as usize];
    let codes_ok = frames.windows(2).all(|w| w[1].0 > w[0].0);

    // Pause holds the frame.
    deck.pause();
    std::thread::sleep(Duration::from_millis(300));
    while let rustet_framering::Read::Frame(_) = reader.read(&mut buf) {}
    let held = frame_code(&buf, ring.width, ring.height);
    std::thread::sleep(Duration::from_millis(700));
    let after_pause = reader.read(&mut buf);

    deck.stop();
    inbox.wait(&deck, load, Duration::from_secs(5), |e| {
        e == Event::VideoStopped
    });
    let ended = std::iter::repeat_with(|| reader.read(&mut buf))
        .find(|r| !matches!(r, rustet_framering::Read::Frame(_)));
    eprintln!(
        "video: {} frames in {span:.2} s = {fps:.1} fps; A/V error (video − audio) p5 {:.1} ms, median {:.1} ms, p95 {:.1} ms \
         over {} frames; paused at frame {held}, then {after_pause:?}; after stop {ended:?}",
        frames.len(),
        pct(0.05),
        pct(0.5),
        pct(0.95),
        errors.len(),
    );
    assert!(
        codes_ok,
        "frame numbers go forward: {:?}",
        &frames[..10.min(frames.len())]
    );
    assert!((27.0..=33.0).contains(&fps), "{fps} fps");
    assert!(
        pct(0.5).abs() < 40.0 && pct(0.05) > -80.0 && pct(0.95) < 80.0,
        "out of sync"
    );
    assert_eq!(
        after_pause,
        rustet_framering::Read::Unchanged,
        "pause holds the frame"
    );
    assert_eq!(
        ended,
        Some(rustet_framering::Read::Ended(rustet_framering::End::Closed))
    );
    assert!(!ring.path.exists(), "stop deletes the ring");
}
