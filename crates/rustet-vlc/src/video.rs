//! Video of a deck: libvlc's video callbacks (`vmem`) write each displayed frame into a frame
//! ring (`rustet_framering`) that the UI maps.
//!
//! libvlc calls the format, lock, unlock, display and cleanup callbacks on the video output
//! thread, one at a time. The format callback picks BGRA (`RV32`) at the source size, scaled
//! down to the load's height limit, and gives libvlc a private buffer to render into; the display
//! callback, which libvlc calls at the frame's presentation time, copies it into the ring. So the
//! ring shows the frame libvlc's clock wants now, in sync with the audio of the same media.
//!
//! The ring is created at the first displayed frame of a load, and again when the size changes
//! (a new generation); the deck closes it when the load ends (a new load, a stop) and libvlc's
//! cleanup closes it when the video output goes. A deck installs the callbacks once; a load
//! without a [`VideoOutput`] refuses any video, so an audio load never opens a window.

use std::ffi::{c_char, c_uint, c_void};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rustet_framering::{Format, Writer};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::events::{DeckEvent, DeckId, Event, LoadId};
use crate::ffi::{Api, MediaPlayer};

/// Where and how a load shows its video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoOutput {
    /// The directory of the frame rings (`rustet_framering::default_dir`).
    pub dir: PathBuf,
    /// Frames taller than this are scaled down to it (aspect kept); 0 for no limit.
    pub max_height: u32,
    /// The video's visible size (from its sample entry), when the caller knows it. libvlc's
    /// format callback gets the decoder's padded size (1280×738 for 720p) and stretches the
    /// picture into whatever size it is given, so without this the aspect is off by the padding.
    pub source_size: Option<(u32, u32)>,
}

/// The ring a deck writes now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRing {
    pub path: PathBuf,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
}

/// Chromas of hardware decoder surfaces (VA-API, VDPAU, D3D, VideoToolbox, NVDEC, MediaCodec,
/// MMAL). libvlc's H.264 decoder first offers these, one by one, and builds a video output for
/// each; with frames that go back to memory anyway, that is only a delay (about a second per
/// load, during which libvlc's clock already runs and the first frames come too late). The
/// format callback refuses them at once, so libvlc goes straight to software decoding.
const HARDWARE_CHROMAS: [&[u8]; 9] = [
    b"VAO", b"VDV", b"VDO", b"DX", b"D3D", b"CVP", b"NVD", b"ANOP", b"MMAL",
];

fn is_hardware(chroma: &[u8; 4]) -> bool {
    HARDWARE_CHROMAS.iter().any(|p| chroma.starts_with(p))
}

/// Ring generations, unique in the process.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The size libvlc renders at: the source size, scaled to `max_height` when taller. The width is
/// rounded to a multiple of 16 (libvlc's converters and the ring's whole-word rows), the height
/// to an even number.
pub(crate) fn render_size(width: u32, height: u32, max_height: u32) -> (u32, u32) {
    let (w, h) = if max_height > 0 && height > max_height {
        let w =
            (u64::from(width) * u64::from(max_height) + u64::from(height) / 2) / u64::from(height);
        (u32::try_from(w).unwrap_or(u32::MAX), max_height)
    } else {
        (width, height)
    };
    (((w + 8) & !15).max(16), (h & !1).max(2))
}

/// The picture libvlc renders into, from the format callback to the cleanup.
struct Picture {
    format: Format,
    pixels: Box<[u8]>,
    /// [`fingerprint`] of the last frame written to the ring.
    written: Option<u64>,
}

/// A hash of every 61st word of a frame. libvlc shows a paused picture again about every 70 ms;
/// a frame whose fingerprint matches the last one written is that picture again and is not
/// written, so the UI uploads nothing while paused. Two different video frames differ in some
/// sampled word in practice (each row is sampled many times).
fn fingerprint(pixels: &[u8]) -> u64 {
    pixels
        .as_chunks::<8>()
        .0
        .iter()
        .step_by(61)
        .fold(0xcbf2_9ce4_8422_2325, |h, w| {
            (h ^ u64::from_ne_bytes(*w)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

struct Inner {
    /// The current load's output; `None` refuses video.
    output: Option<VideoOutput>,
    load: LoadId,
    picture: Option<Picture>,
    writer: Option<Writer>,
}

/// The callbacks' state. libvlc holds a pointer to it from install until the media player is
/// released, so the deck keeps it at a fixed address until then.
pub(crate) struct Video {
    deck: DeckId,
    events: UnboundedSender<DeckEvent>,
    inner: Mutex<Inner>,
}

impl Video {
    pub(crate) fn new(deck: DeckId, events: UnboundedSender<DeckEvent>) -> Box<Self> {
        let inner = Inner {
            output: None,
            load: LoadId(0),
            picture: None,
            writer: None,
        };
        Box::new(Self {
            deck,
            events,
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Installs the callbacks on `mp`.
    ///
    /// # Safety
    /// `mp` must be live, and `self` must stay at its address until `mp` is released.
    pub(crate) unsafe fn install(&self, api: &Api, mp: NonNull<MediaPlayer>) {
        let opaque = std::ptr::from_ref(self).cast_mut().cast::<c_void>();
        // SAFETY: guaranteed by the caller; the callbacks match libvlc 3's prototypes and only
        // use `opaque` as a `&Video`.
        unsafe {
            (api.libvlc_video_set_callbacks)(
                mp.as_ptr(),
                Some(lock_cb),
                Some(unlock_cb),
                Some(display_cb),
                opaque,
            );
            (api.libvlc_video_set_format_callbacks)(mp.as_ptr(), Some(format_cb), Some(cleanup_cb));
        }
    }

    /// A new load starts: its video goes to `output` (or nowhere). Closes the previous ring.
    pub(crate) fn begin(&self, load: LoadId, output: Option<VideoOutput>) {
        let mut inner = self.lock();
        self.close(&mut inner);
        inner.output = output;
        inner.load = load;
    }

    /// The load ended (stop, close): closes its ring.
    pub(crate) fn end(&self) {
        let mut inner = self.lock();
        self.close(&mut inner);
        inner.output = None;
    }

    pub(crate) fn ring(&self) -> Option<VideoRing> {
        self.lock().writer.as_ref().map(|w| VideoRing {
            path: w.path().to_owned(),
            generation: w.generation(),
            width: w.format().width,
            height: w.format().height,
        })
    }

    fn send(&self, load: LoadId, event: Event) {
        let _ = self.events.send(DeckEvent {
            deck: self.deck,
            load,
            event,
        });
    }

    fn close(&self, inner: &mut Inner) {
        if let Some(w) = inner.writer.take() {
            debug!(
                generation = w.generation(),
                frames = w.frames(),
                "[vlc] video ring closed"
            );
            drop(w);
            self.send(inner.load, Event::VideoStopped);
        }
    }

    /// The format callback: the size and chroma libvlc renders in. 0 refuses the video.
    fn configure(&self, width: u32, height: u32) -> Option<(Format, *mut u8)> {
        let mut inner = self.lock();
        let output = inner.output.as_ref()?;
        let (src_w, src_h) = output.source_size.unwrap_or((width, height));
        let (w, h) = render_size(src_w, src_h, output.max_height);
        let format = Format::bgra(w, h);
        let mut pixels = vec![0; format.frame_bytes()].into_boxed_slice();
        let ptr = pixels.as_mut_ptr();
        debug!(
            decoded = format!("{width}x{height}"),
            render = format!("{w}x{h}"),
            "[vlc] video format"
        );
        inner.picture = Some(Picture {
            format,
            pixels,
            written: None,
        });
        Some((format, ptr))
    }

    /// The display callback: the picture is due now.
    fn display(&self) {
        let mut inner = self.lock();
        let inner = &mut *inner;
        let (Some(picture), Some(output)) = (&mut inner.picture, &inner.output) else {
            return;
        };
        if inner
            .writer
            .as_ref()
            .is_some_and(|w| w.format() != picture.format)
            && let Some(old) = inner.writer.take()
        {
            old.supersede();
        }
        let print = fingerprint(&picture.pixels);
        if inner.writer.is_some() && picture.written == Some(print) {
            return;
        }
        if inner.writer.is_none() {
            let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
            match Writer::create(&output.dir, generation, picture.format) {
                Ok(w) => {
                    debug!(path = %w.path().display(), "[vlc] video ring created");
                    inner.writer = Some(w);
                    let f = picture.format;
                    self.send(
                        inner.load,
                        Event::VideoStarted {
                            generation,
                            width: f.width,
                            height: f.height,
                        },
                    );
                }
                Err(e) => {
                    warn!("[vlc] video ring: {e}; no frames for this load");
                    inner.output = None;
                    return;
                }
            }
        }
        if let Some(w) = inner.writer.as_mut() {
            match w.write_frame(&picture.pixels, rustet_framering::now_us()) {
                Ok(()) => picture.written = Some(print),
                Err(e) => warn!("[vlc] video frame: {e}"),
            }
        }
    }

    /// The cleanup callback: the video output goes.
    fn cleanup(&self) {
        let mut inner = self.lock();
        self.close(&mut inner);
        inner.picture = None;
    }

    fn pixels(&self) -> *mut u8 {
        self.lock()
            .picture
            .as_mut()
            .map_or(std::ptr::null_mut(), |p| p.pixels.as_mut_ptr())
    }
}

/// # Safety
/// `opaque` is the pointer given at install, still alive.
unsafe fn video<'a>(opaque: *mut c_void) -> &'a Video {
    // SAFETY: guaranteed by the caller; the deck keeps the `Video` until the player is released.
    unsafe { &*opaque.cast::<Video>() }
}

unsafe extern "C" fn format_cb(
    opaque: *mut *mut c_void,
    chroma: *mut c_char,
    width: *mut c_uint,
    height: *mut c_uint,
    pitches: *mut c_uint,
    lines: *mut c_uint,
) -> c_uint {
    // SAFETY: libvlc passes valid pointers: `*opaque` is the pointer set at install (left as is),
    // `chroma` has 4 writable bytes, the others point to plane 0 of writable arrays.
    unsafe {
        let offered: [u8; 4] = std::ptr::read(chroma.cast());
        if is_hardware(&offered) {
            return 0;
        }
        let v = video(*opaque);
        let Some((format, _)) = v.configure(*width, *height) else {
            return 0;
        };
        std::ptr::copy_nonoverlapping(c"RV32".as_ptr(), chroma, 4);
        *width = format.width;
        *height = format.height;
        *pitches = format.stride;
        *lines = format.height;
    }
    1
}

unsafe extern "C" fn cleanup_cb(opaque: *mut c_void) {
    // SAFETY: `opaque` is the pointer set at install.
    unsafe { video(opaque) }.cleanup();
}

unsafe extern "C" fn lock_cb(opaque: *mut c_void, planes: *mut *mut c_void) -> *mut c_void {
    // SAFETY: `opaque` is the pointer set at install; `planes` has room for plane 0. The buffer
    // stays allocated until the cleanup callback, which libvlc calls on this same thread after it
    // stopped rendering into it.
    unsafe { *planes = video(opaque).pixels().cast() };
    std::ptr::null_mut()
}

unsafe extern "C" fn unlock_cb(
    _opaque: *mut c_void,
    _picture: *mut c_void,
    _planes: *const *mut c_void,
) {
}

unsafe extern "C" fn display_cb(opaque: *mut c_void, _picture: *mut c_void) {
    // SAFETY: `opaque` is the pointer set at install. libvlc finished rendering into the buffer
    // (unlock came first) and renders the next picture only after this returns.
    unsafe { video(opaque) }.display();
}

#[cfg(test)]
mod tests {
    use super::*;

    // The UI gets what the user's height setting allows, never an upscale, and the ring needs
    // even sizes.
    #[test]
    fn hardware_surfaces_are_refused() {
        for c in [
            b"VAOP", b"VAO0", b"VDV0", b"DXA9", b"D3D9", b"CVPN", b"NVD8",
        ] {
            assert!(is_hardware(c), "{}", String::from_utf8_lossy(c));
        }
        for c in [b"I420", b"J420", b"NV12", b"I0AL", b"P010", b"RV32"] {
            assert!(!is_hardware(c), "{}", String::from_utf8_lossy(c));
        }
    }

    #[test]
    fn render_size_caps_the_height_and_keeps_the_aspect() {
        assert_eq!(render_size(1920, 1080, 2160), (1920, 1080));
        assert_eq!(render_size(1920, 1080, 720), (1280, 720));
        assert_eq!(render_size(1920, 1080, 480), (848, 480)); // 853.3 → 853 → nearest 16
        assert_eq!(render_size(3840, 2160, 1080), (1920, 1080));
        assert_eq!(render_size(1440, 1080, 0), (1440, 1080), "0 means no limit");
        assert_eq!(
            render_size(726, 405, 0),
            (720, 404),
            "the width to a multiple of 16, the height even"
        );
    }
}
