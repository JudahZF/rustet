//! A ring of decoded video frames in shared memory (spec §4.9): one writer, the engine's libvlc
//! deck, and any number of readers, the UI. Both processes map the same file; the reader always
//! takes the newest complete frame and never sees a torn one.
//!
//! # Layout
//!
//! A ring file is a 4 KiB header followed by [`SLOTS`] frame slots, each `slot_bytes` long (a
//! multiple of 4 KiB). The header is a sequence of native-endian `u64` words:
//!
//! | word | field |
//! |---|---|
//! | 0 | magic `"RUSTETFR"` |
//! | 1 | layout version ([`VERSION`]) |
//! | 2, 3 | width, height in pixels |
//! | 4 | stride: bytes per row in a slot |
//! | 5 | pixel format ([`PixelFormat`]) |
//! | 6 | slot count |
//! | 7 | bytes per slot |
//! | 8 | generation: unique per ring in the writer's process |
//! | 9 | writer process ID |
//! | 10 | heartbeat: Unix ms, renewed every 500 ms while the writer lives |
//! | 11 | state: 0 live, 1 closed, 2 superseded (a ring with another size follows) |
//! | 12 | `latest`: number of the newest complete frame, 0 before the first |
//! | 16 + 2·i | slot i sequence: `2n` holds frame n complete, `2n + 1` while frame n is written |
//! | 17 + 2·i | slot i presentation time: Unix µs when the writer showed the frame |
//!
//! Frame n goes to slot `n % SLOTS`. Writer: mark the slot odd, write the pixels, mark it even,
//! publish `latest = n`. Reader: read `latest`, check the slot holds frame n, copy, check again
//! (a seqlock). A size change never resizes a file: the writer makes a new ring with a new
//! generation and marks the old one superseded.
//!
//! Every access to the shared bytes is an atomic word access (module `shm`, the only `unsafe`).
//!
//! # Files
//!
//! Rings live in [`default_dir`] (Linux: `$XDG_RUNTIME_DIR/rustet`, private to the user) as
//! `rustet-video-<pid>-<generation>.ring`. A ring is created under a temporary name and renamed
//! when its header is complete, so a reader never maps half a header. The writer deletes the file
//! when it closes; readers that still have it mapped keep their memory. [`sweep`] deletes the
//! rings of writers that died.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod shm;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shm::{ReadMap, WriteMap};

/// The layout version this crate reads and writes.
pub const VERSION: u64 = 1;
/// Frame slots per ring.
pub const SLOTS: u64 = 3;
/// The header length; slots start here.
pub const HEADER_BYTES: u64 = 4096;
/// A ring whose heartbeat is older than this belongs to a writer that died.
pub const STALE_AFTER: Duration = Duration::from_secs(3);

const MAGIC: u64 = u64::from_le_bytes(*b"RUSTETFR");
const HEARTBEAT_EVERY: Duration = Duration::from_millis(500);
const PAGE: u64 = 4096;
/// Attempts of one [`Reader::read`] before it gives up for this frame (the writer lapped it).
const READ_TRIES: usize = 4;

/// Header word indexes.
mod w {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 1;
    pub const WIDTH: usize = 2;
    pub const HEIGHT: usize = 3;
    pub const STRIDE: usize = 4;
    pub const FORMAT: usize = 5;
    pub const SLOTS: usize = 6;
    pub const SLOT_BYTES: usize = 7;
    pub const GENERATION: usize = 8;
    pub const PID: usize = 9;
    pub const HEARTBEAT: usize = 10;
    pub const STATE: usize = 11;
    pub const LATEST: usize = 12;
    pub const fn slot_seq(i: usize) -> usize {
        16 + 2 * i
    }
    pub const fn slot_time(i: usize) -> usize {
        17 + 2 * i
    }
}

const STATE_LIVE: u64 = 0;
const STATE_CLOSED: u64 = 1;
const STATE_SUPERSEDED: u64 = 2;

/// How the pixels of a slot are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Bytes B, G, R, A per pixel, A always 255: what gpui uploads as is.
    Bgra = 1,
}

/// The geometry of a ring's frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    /// Bytes per row in a slot, at least `width * 4` and a multiple of 8.
    pub stride: u32,
    pub pixel: PixelFormat,
}

impl Format {
    /// Tightly packed BGRA rows of an even `width`.
    pub fn bgra(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            stride: width * 4,
            pixel: PixelFormat::Bgra,
        }
    }

    /// Bytes of one frame in a slot.
    pub fn frame_bytes(&self) -> usize {
        self.stride as usize * self.height as usize
    }

    /// Bytes of one frame with packed rows, as [`Reader::read`] returns it.
    pub fn packed_bytes(&self) -> usize {
        self.width as usize * 4 * self.height as usize
    }

    fn slot_bytes(&self) -> u64 {
        (self.frame_bytes() as u64).div_ceil(PAGE) * PAGE
    }

    fn check(&self) -> Result<(), Error> {
        let ok = self.width > 0
            && self.height > 0
            && self.stride >= self.width.saturating_mul(4)
            && self.stride.is_multiple_of(8)
            && (self.width as usize * 4).is_multiple_of(8)
            && self.frame_bytes() <= 1 << 30;
        if ok {
            Ok(())
        } else {
            Err(Error::BadFormat(*self))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("frame ring: {0}")]
    Io(#[from] io::Error),
    /// The file is not a frame ring, or its header is not complete.
    #[error("frame ring: not a ring file")]
    NotARing,
    #[error("frame ring: layout version {0}, this build reads {VERSION}")]
    Version(u64),
    /// The header contradicts itself or the file length.
    #[error("frame ring: corrupt header ({0})")]
    Corrupt(&'static str),
    #[error("frame ring: unusable format {0:?}")]
    BadFormat(Format),
    /// [`Writer::write_frame`] got fewer bytes than a frame.
    #[error("frame ring: frame of {got} bytes, want {want}")]
    ShortFrame { got: usize, want: usize },
    /// The ring directory is shared with other users.
    #[error("frame ring: {0} is not private to this user")]
    NotPrivate(PathBuf),
}

/// Why a ring has no more frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The writer closed it: the video ended or stopped.
    Closed,
    /// The writer replaced it with a ring of another size: open the new one.
    Superseded,
    /// The writer died: its heartbeat stopped.
    Stale,
}

/// One frame [`Reader::read`] copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// The frame's number in the ring, from 1, increasing.
    pub number: u64,
    /// Unix µs when the writer presented it.
    pub presented_at_us: u64,
}

/// What [`Reader::read`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// A newer frame than the last one read, now in the buffer.
    Frame(Frame),
    /// Nothing new (also before the first frame); the buffer is untouched.
    Unchanged,
    /// The ring will have no more frames; the buffer is untouched.
    Ended(End),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Unix µs now, the clock of [`Frame::presented_at_us`].
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// The words of slot `i` of a ring with this header.
fn slot(words: &[AtomicU64], slot_bytes: u64, i: u64) -> &[AtomicU64] {
    let start = ((HEADER_BYTES + i * slot_bytes) / 8) as usize;
    &words[start..start + (slot_bytes / 8) as usize]
}

/// The ring file name of `generation` in the writer process `pid`.
pub fn file_name(pid: u32, generation: u64) -> String {
    format!("rustet-video-{pid}-{generation}.ring")
}

// ── Writer ──────────────────────────────────────────────────────────────────

struct Shared {
    map: WriteMap,
    stop: Mutex<bool>,
    wake: Condvar,
}

/// The writing end of one ring. Dropping it closes the ring and deletes its file.
pub struct Writer {
    shared: Arc<Shared>,
    path: PathBuf,
    format: Format,
    generation: u64,
    next: u64,
    end: End,
    heartbeat: Option<JoinHandle<()>>,
}

impl Writer {
    /// Creates the ring `generation` in `dir` (which must exist) and starts its heartbeat.
    pub fn create(dir: &Path, generation: u64, format: Format) -> Result<Self, Error> {
        format.check()?;
        let pid = std::process::id();
        let path = dir.join(file_name(pid, generation));
        let tmp = dir.join(format!(".{}.tmp", file_name(pid, generation)));
        let slot_bytes = format.slot_bytes();
        let len = HEADER_BYTES + SLOTS * slot_bytes;
        let file = private_file(&tmp)?;
        let made = (|| {
            file.set_len(len)?;
            let map = WriteMap::new(&file)?;
            let h = map.words();
            let fields = [
                (w::VERSION, VERSION),
                (w::WIDTH, u64::from(format.width)),
                (w::HEIGHT, u64::from(format.height)),
                (w::STRIDE, u64::from(format.stride)),
                (w::FORMAT, format.pixel as u64),
                (w::SLOTS, SLOTS),
                (w::SLOT_BYTES, slot_bytes),
                (w::GENERATION, generation),
                (w::PID, u64::from(pid)),
                (w::HEARTBEAT, now_ms()),
                (w::STATE, STATE_LIVE),
            ];
            for (i, v) in fields {
                h[i].store(v, Ordering::Relaxed);
            }
            h[w::MAGIC].store(MAGIC, Ordering::Release);
            fs::rename(&tmp, &path)?;
            io::Result::Ok(map)
        })();
        let map = match made {
            Ok(map) => map,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e.into());
            }
        };
        let shared = Arc::new(Shared {
            map,
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let beat = shared.clone();
        let heartbeat = std::thread::Builder::new()
            .name("rustet-framering".into())
            .spawn(move || {
                let mut stop = beat.stop.lock().unwrap_or_else(PoisonError::into_inner);
                while !*stop {
                    beat.map.words()[w::HEARTBEAT].store(now_ms(), Ordering::Relaxed);
                    stop = beat
                        .wake
                        .wait_timeout(stop, HEARTBEAT_EVERY)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            })?;
        Ok(Self {
            shared,
            path,
            format,
            generation,
            next: 1,
            end: End::Closed,
            heartbeat: Some(heartbeat),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.next - 1
    }

    /// Publishes one frame: `pixels` holds `format.frame_bytes()` bytes with rows `stride` apart.
    /// The alpha bytes are written as 255 (libvlc's RV32 leaves them undefined).
    pub fn write_frame(&mut self, pixels: &[u8], presented_at_us: u64) -> Result<(), Error> {
        let want = self.format.frame_bytes();
        let pixels = pixels.get(..want).ok_or(Error::ShortFrame {
            got: pixels.len(),
            want,
        })?;
        let words = self.shared.map.words();
        let n = self.next;
        let i = n % SLOTS;
        let seq = &words[w::slot_seq(i as usize)];
        seq.store(2 * n + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        // Pixels are B, G, R, A in memory: the alpha bytes are bytes 3 and 7 of each word.
        let opaque = u64::from_ne_bytes([0, 0, 0, 0xff, 0, 0, 0, 0xff]);
        for (dst, src) in slot(words, self.format.slot_bytes(), i)
            .iter()
            .zip(pixels.as_chunks::<8>().0)
        {
            dst.store(u64::from_ne_bytes(*src) | opaque, Ordering::Relaxed);
        }
        words[w::slot_time(i as usize)].store(presented_at_us, Ordering::Relaxed);
        seq.store(2 * n, Ordering::Release);
        words[w::LATEST].store(n, Ordering::Release);
        self.next += 1;
        Ok(())
    }

    /// Marks the ring as replaced by one of another size, then closes it.
    pub fn supersede(mut self) {
        self.end = End::Superseded;
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        *self
            .shared
            .stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.heartbeat.take() {
            let _ = t.join();
        }
        let state = if self.end == End::Superseded {
            STATE_SUPERSEDED
        } else {
            STATE_CLOSED
        };
        self.shared.map.words()[w::STATE].store(state, Ordering::Release);
        if let Err(e) = fs::remove_file(&self.path) {
            tracing::debug!(path = %self.path.display(), "frame ring: remove: {e}");
        }
    }
}

/// Creates a new file only this user can read.
fn private_file(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

// ── Reader ──────────────────────────────────────────────────────────────────

/// The reading end of a ring: maps the file read-only. Cheap to poll every UI frame.
pub struct Reader {
    map: ReadMap,
    format: Format,
    generation: u64,
    pid: u32,
    slot_bytes: u64,
    last: u64,
}

impl Reader {
    /// Opens the ring at `path`. A ring that has no frame yet opens fine.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < HEADER_BYTES || len % 8 != 0 {
            return Err(Error::NotARing);
        }
        let map = ReadMap::new(&file)?;
        let h = map.words();
        if h[w::MAGIC].load(Ordering::Relaxed) != MAGIC {
            return Err(Error::NotARing);
        }
        fence(Ordering::Acquire);
        let get = |i: usize| h[i].load(Ordering::Relaxed);
        match get(w::VERSION) {
            VERSION => {}
            v => return Err(Error::Version(v)),
        }
        let narrow = |v: u64, what| u32::try_from(v).map_err(|_| Error::Corrupt(what));
        let pixel = match get(w::FORMAT) {
            1 => PixelFormat::Bgra,
            _ => return Err(Error::Corrupt("pixel format")),
        };
        let format = Format {
            width: narrow(get(w::WIDTH), "width")?,
            height: narrow(get(w::HEIGHT), "height")?,
            stride: narrow(get(w::STRIDE), "stride")?,
            pixel,
        };
        format.check().map_err(|_| Error::Corrupt("geometry"))?;
        let slot_bytes = get(w::SLOT_BYTES);
        if get(w::SLOTS) != SLOTS
            || slot_bytes != format.slot_bytes()
            || len != HEADER_BYTES + SLOTS * slot_bytes
        {
            return Err(Error::Corrupt("slot layout"));
        }
        let pid = narrow(get(w::PID), "pid")?;
        Ok(Self {
            generation: get(w::GENERATION),
            map,
            format,
            pid,
            slot_bytes,
            last: 0,
        })
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The writer's process ID.
    pub fn writer_pid(&self) -> u32 {
        self.pid
    }

    /// Why the ring has no more frames, if it has none.
    pub fn ended(&self) -> Option<End> {
        let h = self.map.words();
        match h[w::STATE].load(Ordering::Acquire) {
            STATE_LIVE => {}
            STATE_SUPERSEDED => return Some(End::Superseded),
            _ => return Some(End::Closed),
        }
        let beat = h[w::HEARTBEAT].load(Ordering::Relaxed);
        (now_ms().saturating_sub(beat) > STALE_AFTER.as_millis() as u64).then_some(End::Stale)
    }

    /// Copies the newest frame into `buf` with packed rows (`width * 4` bytes each) when it is
    /// newer than the last one this reader returned. A frame the writer overwrote during the copy
    /// is never returned. Cheap when nothing changed: one atomic load.
    pub fn read(&mut self, buf: &mut Vec<u8>) -> Read {
        let h = self.map.words();
        let mut n = h[w::LATEST].load(Ordering::Acquire);
        if n == self.last {
            // The last frames before an end are still shown; the end is reported once they are read.
            return self.ended().map_or(Read::Unchanged, Read::Ended);
        }
        buf.resize(self.format.packed_bytes(), 0);
        for _ in 0..READ_TRIES {
            let i = n % SLOTS;
            let seq = &h[w::slot_seq(i as usize)];
            if seq.load(Ordering::Acquire) == 2 * n {
                self.copy(slot(h, self.slot_bytes, i), buf);
                let at = h[w::slot_time(i as usize)].load(Ordering::Relaxed);
                fence(Ordering::Acquire);
                if seq.load(Ordering::Relaxed) == 2 * n {
                    self.last = n;
                    return Read::Frame(Frame {
                        number: n,
                        presented_at_us: at,
                    });
                }
            }
            // The writer moved on: try its newest frame.
            n = h[w::LATEST].load(Ordering::Acquire);
        }
        Read::Unchanged
    }

    fn copy(&self, slot: &[AtomicU64], buf: &mut [u8]) {
        let row = self.format.width as usize * 4;
        let stride_words = self.format.stride as usize / 8;
        for (dst, src) in buf.chunks_exact_mut(row).zip(slot.chunks(stride_words)) {
            for (d, s) in dst.as_chunks_mut::<8>().0.iter_mut().zip(src) {
                *d = s.load(Ordering::Relaxed).to_ne_bytes();
            }
        }
    }
}

// ── Directory ───────────────────────────────────────────────────────────────

/// Where rings live. Linux: `$XDG_RUNTIME_DIR/rustet` (a tmpfs private to the user), else
/// `/dev/shm/rustet-<uid>`; other platforms: `rustet-video` in the temp directory. Created with mode
/// 0700 when missing; an existing directory must be the user's and private.
pub fn default_dir() -> Result<PathBuf, Error> {
    let dir = platform_dir();
    ensure_private_dir(&dir)?;
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn platform_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(run) => run.join("rustet"),
        None => PathBuf::from("/dev/shm").join(format!("rustet-{}", current_uid())),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn platform_dir() -> PathBuf {
    std::env::temp_dir().join(format!("rustet-video-{}", current_uid()))
}

#[cfg(not(unix))]
fn platform_dir() -> PathBuf {
    std::env::temp_dir().join("rustet-video")
}

/// The real user ID, from the owner of the process's own `/proc` entry or a file it creates.
#[cfg(unix)]
fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    if let Ok(m) = fs::metadata("/proc/self") {
        return m.uid();
    }
    let probe = std::env::temp_dir().join(format!(".rustet-uid-{}", std::process::id()));
    let uid = File::create(&probe)
        .and_then(|f| f.metadata())
        .map_or(u32::MAX, |m| m.uid());
    let _ = fs::remove_file(probe);
    uid
}

/// Creates `dir` (mode 0700) or checks that it belongs to this user and is private.
pub fn ensure_private_dir(dir: &Path) -> Result<(), Error> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = fs::symlink_metadata(dir)?;
        if !m.is_dir() || m.uid() != current_uid() || m.permissions().mode() & 0o077 != 0 {
            return Err(Error::NotPrivate(dir.to_owned()));
        }
    }
    Ok(())
}

/// Deletes the rings in `dir` whose writer is gone (closed, superseded or stale), files that are
/// not rings, and temporary files left by a writer that died while creating one. Blocking.
pub fn sweep(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let dead = if name.starts_with(".rustet-video-") && name.ends_with(".tmp") {
            e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|d| d > STALE_AFTER))
        } else if name.starts_with("rustet-video-") && name.ends_with(".ring") {
            Reader::open(&path).map_or(true, |r| r.ended().is_some())
        } else {
            false
        };
        if dead && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests;
