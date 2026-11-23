//! A libvlc media player ("deck").

use std::ffi::{CString, c_int};
use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::callbacks::CallbackMedia;
use crate::events::{DeckEvent, DeckId, Events, LoadId, Sink};
use crate::ffi::{Api, Media, MediaPlayer, state};
use crate::source::{AbortHandle, Source};
use crate::video::{Video, VideoOutput, VideoRing};
use crate::{Error, Instance};

/// libvlc's playback state, reduced to what the renderer understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    Opening,
    Buffering,
    Playing,
    Paused,
    Ended,
    Error,
    Unknown,
}

impl State {
    fn from_raw(raw: c_int) -> Self {
        match raw {
            state::NOTHING_SPECIAL | state::STOPPED => Self::Stopped,
            state::OPENING => Self::Opening,
            state::BUFFERING => Self::Buffering,
            state::PLAYING => Self::Playing,
            state::PAUSED => Self::Paused,
            state::ENDED => Self::Ended,
            state::ERROR => Self::Error,
            _ => Self::Unknown,
        }
    }

    /// The `state` string of the engine's `/vlc/time` response.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Opening => "opening",
            Self::Buffering => "buffering",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Ended => "ended",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }

    fn is_seekable(self) -> bool {
        matches!(self, Self::Playing | Self::Paused)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Position, length and state of the current media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Current position in ms, as libvlc reports it (-1 without media).
    pub position_ms: i64,
    /// Total length in ms, as libvlc reports it (0 or -1 while unknown).
    pub length_ms: i64,
    pub state: State,
}

/// The live mixer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    /// 0–200, 100 = 100%.
    pub level: u32,
    pub muted: bool,
}

const DEFAULT_VOLUME: u32 = 100;
const MAX_VOLUME: u32 = 200;
/// How long a seek requested while the media is still opening waits for it to play.
/// Throttled CPUs take long to open a stream.
const DELAYED_SEEK_DEADLINE: Duration = Duration::from_secs(10);
const DELAYED_SEEK_POLL: Duration = Duration::from_millis(100);

/// How a deck starts a new media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Play at once.
    Playing,
    /// Pre-roll: open, demux and buffer, then hold at 0 without sound (libvlc's
    /// `start-paused`) until [`Deck::resume`]. For gapless, cue the next track on the idle deck
    /// this way, then resume it at the current deck's [`Event::EndReached`](crate::Event::EndReached):
    /// its first sample follows about 10 ms after the resume.
    ///
    /// The deck reports [`Event::Playing`](crate::Event::Playing) then
    /// [`Event::Paused`](crate::Event::Paused) at once, and `Buffering(100)` once buffered (a
    /// short media that libvlc reads to its end only reports it after the resume). libvlc does
    /// not decode while paused, so the audio output stream opens at the resume.
    Paused,
}

/// One libvlc media player: plays one track at a time; loading replaces the current media.
/// Safe for concurrent use.
///
/// Decks made with [`Instance::deck`] share one libvlc instance and report [`DeckEvent`]s on a
/// channel; each has its own volume and audio stream. A deck made
/// with [`Deck::new`] (the single-track [`Player`](crate::Player)) owns its instance and has no
/// events: poll [`Deck::timing`].
///
/// Methods call libvlc synchronously, and `load*`, `open*`, `stop` and drop wait for libvlc's
/// input thread. Call them from a blocking context (`spawn_blocking`), not from a runtime worker.
pub struct Deck {
    shared: Arc<Mutex<Inner>>,
    id: DeckId,
}

struct Inner {
    /// `None` once the deck is closed.
    vlc: Option<Vlc>,
    volume: u32,
    /// Bumped on every load, stop and close; a delayed seek compares it to detect staleness,
    /// and events carry it as their [`LoadId`].
    load_gen: u64,
    /// The callback source attached to the current media, if any.
    cur: Option<Attached>,
}

struct Vlc {
    /// Kept so the instance outlives the media player.
    inst: Instance,
    api: &'static Api,
    mp: NonNull<MediaPlayer>,
    /// Dropped (detached) before the media player is released.
    events: Option<Events>,
    /// How the deck halts playback. See [`Vlc::halt`].
    keep_output: bool,
    /// The video callbacks' state (instance decks only). Dropped after the media player.
    video: Option<Box<Video>>,
}

// SAFETY: libvlc media players may be used from any thread; every use here is serialised by
// the deck's mutex.
unsafe impl Send for Vlc {}

impl Vlc {
    /// Stops the input and waits for libvlc's input thread.
    ///
    /// `libvlc_media_player_stop` also destroys the player's audio output, and libvlc drops a
    /// volume set while there is none, until the next track decodes its first audio. A deck that
    /// keeps its output detaches the media instead (`set_media(NULL)`, which joins the input
    /// thread the same way), so a level set on an idle or cued deck applies when its next track
    /// starts: the crossfade and pre-roll need that.
    fn halt(&self) {
        // SAFETY: the player is live; passing null detaches its media.
        unsafe {
            if self.keep_output {
                (self.api.libvlc_media_player_set_media)(self.mp.as_ptr(), ptr::null_mut());
            } else {
                (self.api.libvlc_media_player_stop)(self.mp.as_ptr());
            }
        }
    }
}

struct Attached {
    media: Arc<CallbackMedia>,
    abort: AbortHandle,
}

impl Deck {
    /// Loads libvlc and creates a deck on its own instance, without events.
    ///
    /// Fails with [`Error::Unavailable`] when libvlc is not installed or bundled: the engine then
    /// runs without lossless playback.
    pub fn new() -> Result<Self, Error> {
        Self::create(Instance::new()?, None, false)
    }

    #[cfg(test)]
    pub(crate) fn with_args(args: &[&std::ffi::CStr]) -> Result<Self, Error> {
        Self::create(Instance::with_args(args)?, None, false)
    }

    /// `keep_output`: see [`Vlc::halt`]. The single-track [`Player`](crate::Player) does not keep
    /// it, as before decks existed.
    pub(crate) fn create(
        inst: Instance,
        events: Option<UnboundedSender<DeckEvent>>,
        keep_output: bool,
    ) -> Result<Self, Error> {
        let id = inst.next_deck_id();
        let api = inst.shared().api;
        // SAFETY: the instance is live (held by `inst`).
        let mp = NonNull::new(unsafe { (api.libvlc_media_player_new)(inst.shared().raw.as_ptr()) })
            .ok_or(Error::PlayerFailed)?;
        // SAFETY: `mp` is live and released only after `events` is dropped (in `Drop`, or below).
        let events = match events
            .map(|tx| unsafe { Events::attach(api, mp, Sink::new(id, tx)) })
            .transpose()
        {
            Ok(events) => events,
            Err(e) => {
                // SAFETY: `mp` is owned here, has no media and no attached callbacks.
                unsafe { (api.libvlc_media_player_release)(mp.as_ptr()) };
                return Err(e);
            }
        };
        // Instance decks render video into frame rings; a load without a `VideoOutput` refuses
        // video, so no deck ever opens a window.
        let video = events.as_ref().map(|e| Video::new(id, e.sink().sender()));
        if let Some(v) = &video {
            // SAFETY: `mp` is live; the boxed state keeps its address and is dropped after the
            // player is released (field order in `Vlc` and the explicit release in `Drop`).
            unsafe { v.install(api, mp) };
        }
        let vlc = Vlc {
            inst,
            api,
            mp,
            events,
            keep_output,
            video,
        };
        let inner = Inner {
            vlc: Some(vlc),
            volume: DEFAULT_VOLUME,
            load_gen: 0,
            cur: None,
        };
        Ok(Self {
            shared: Arc::new(Mutex::new(inner)),
            id,
        })
    }

    /// The raw media player, for test harnesses that install libvlc audio callbacks.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> NonNull<MediaPlayer> {
        self.lock().vlc.as_ref().expect("deck open").mp
    }

    /// The deck's identity in its [`DeckEvent`]s.
    pub fn id(&self) -> DeckId {
        self.id
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.shared)
    }

    /// Stops any current playback and starts playing `url` from the beginning.
    pub fn load(&self, url: &str) -> Result<(), Error> {
        self.open_url(url, Start::Playing).map(drop)
    }

    /// Stops any current playback and plays `src` through libvlc's read and seek callbacks, with
    /// no loopback HTTP. The deck owns `src` and drops it when the media is replaced, stopped
    /// or the deck is dropped.
    pub fn load_source(&self, src: impl Source) -> Result<(), Error> {
        self.open_source(src, Start::Playing).map(drop)
    }

    /// [`Deck::load`] with a choice of [`Start`]; returns the load's identity in events.
    pub fn open_url(&self, url: &str, start: Start) -> Result<LoadId, Error> {
        let url_c = CString::new(url).map_err(|_| Error::InvalidUrl)?;
        let mut inner = self.lock();
        let vlc = inner.vlc.as_ref().ok_or(Error::Closed)?;
        let (api, mp) = (vlc.api, vlc.mp);
        // SAFETY: the instance is live and `url_c` is NUL-terminated.
        let media = unsafe {
            (api.libvlc_media_new_location)(vlc.inst.shared().raw.as_ptr(), url_c.as_ptr())
        };
        if media.is_null() {
            return Err(Error::MediaFailed {
                url: url.to_owned(),
            });
        }
        inner.detach();
        // SAFETY: player and media are live; the player keeps its own reference to the media.
        unsafe {
            set_start(api, media, start);
            (api.libvlc_media_player_set_media)(mp.as_ptr(), media);
            (api.libvlc_media_release)(media);
        }
        inner.play(None)
    }

    /// [`Deck::load_source`] with a choice of [`Start`]; returns the load's identity in events.
    pub fn open_source(&self, src: impl Source, start: Start) -> Result<LoadId, Error> {
        self.open(src, start, None)
    }

    /// [`Deck::open_source`] for a media with video: its audio plays as any other, and its frames
    /// go to a frame ring in `output.dir`, reported with [`Event::VideoStarted`](crate::Event::VideoStarted)
    /// and [`Deck::video_ring`]. Only decks made with [`Instance::deck`] show video; on others
    /// the video is dropped.
    ///
    /// `at_ms` > 0 starts at that time (libvlc's `start-time`): libvlc seeks before it plays and
    /// shows no frame and plays no sample before it. For a fragmented MP4 that is only quick with
    /// an index (`sidx`); without one libvlc reads the whole file first.
    pub fn open_video(
        &self,
        src: impl Source,
        start: Start,
        at_ms: u64,
        output: VideoOutput,
    ) -> Result<LoadId, Error> {
        self.open(src, start, Some((output, at_ms)))
    }

    /// The frame ring the current load writes, once its first frame was shown.
    pub fn video_ring(&self) -> Option<VideoRing> {
        self.lock().vlc.as_ref()?.video.as_ref()?.ring()
    }

    fn open(
        &self,
        src: impl Source,
        start: Start,
        video: Option<(VideoOutput, u64)>,
    ) -> Result<LoadId, Error> {
        let abort = src.abort_handle();
        let mut inner = self.lock();
        let Some(vlc) = inner.vlc.as_ref() else {
            abort.abort();
            return Err(Error::Closed);
        };
        let (api, inst, mp) = (vlc.api, vlc.inst.shared().raw, vlc.mp);
        inner.detach();
        let media = CallbackMedia::new(Box::new(src));
        // SAFETY: the instance is live; `media` is kept in `inner.cur` until `detach` has stopped
        // the player and detached it from the libvlc media.
        let vlc_media = unsafe { CallbackMedia::new_media(&media, api, inst.as_ptr()) };
        if vlc_media.is_null() {
            return Err(Error::CallbacksFailed);
        }
        // SAFETY: player and media are live; the player keeps its own reference to the media.
        unsafe {
            set_start(api, vlc_media, start);
            if let Some((_, at_ms)) = &video {
                // libvlc would draw the media's name over the first seconds of the frames; and
                // its hardware decoders (VA-API, VDPAU) are tried and dropped one by one, as the
                // frames go back to memory anyway.
                (api.libvlc_media_add_option)(vlc_media, c":no-video-title-show".as_ptr());
                (api.libvlc_media_add_option)(vlc_media, c":avcodec-hw=none".as_ptr());
                if *at_ms > 0 {
                    let at =
                        CString::new(format!(":start-time={}.{:03}", at_ms / 1000, at_ms % 1000))
                            .expect("no NUL");
                    (api.libvlc_media_add_option)(vlc_media, at.as_ptr());
                }
            }
            (api.libvlc_media_player_set_media)(mp.as_ptr(), vlc_media);
            (api.libvlc_media_release)(vlc_media);
        }
        inner.cur = Some(Attached { media, abort });
        inner.play(video.map(|(output, _)| output))
    }

    /// The live mixer state rather than the last requested volume. Together with not re-applying
    /// the volume on every load, this is from PR #1 (https://github.com/silentone12725/apple-music-linux/pull/1).
    pub fn volume(&self) -> Volume {
        let inner = self.lock();
        let Some(vlc) = &inner.vlc else {
            return Volume {
                level: inner.volume,
                muted: false,
            };
        };
        // SAFETY: the player is live.
        let (level, muted) = unsafe {
            (
                (vlc.api.libvlc_audio_get_volume)(vlc.mp.as_ptr()),
                (vlc.api.libvlc_audio_get_mute)(vlc.mp.as_ptr()),
            )
        };
        // A negative level means no audio output yet.
        Volume {
            level: u32::try_from(level).unwrap_or(inner.volume),
            muted: muted == 1,
        }
    }

    /// Sets this deck's volume: `level` is clamped to 0–200 (100 = 100%) and unmutes. Each deck
    /// has its own audio stream, so this is also the crossfade primitive (see [`Ramp`](crate::Ramp)).
    pub fn set_volume(&self, level: i64) {
        let level = level.clamp(0, i64::from(MAX_VOLUME)) as u32;
        let mut inner = self.lock();
        inner.volume = level;
        if let Some(vlc) = &inner.vlc {
            // SAFETY: the player is live; `level` is at most 200.
            unsafe {
                (vlc.api.libvlc_audio_set_mute)(vlc.mp.as_ptr(), 0);
                (vlc.api.libvlc_audio_set_volume)(vlc.mp.as_ptr(), level as c_int);
            }
        }
    }

    /// Seeks the current media to `position_ms`. A negative position is ignored.
    ///
    /// When libvlc is playing or paused the seek is applied at once. While the media is still
    /// opening (right after a load), a background thread polls for up to 10 s and applies it once
    /// libvlc plays, so this is safe to call right after `load`. A load, stop or close in between
    /// drops the delayed seek.
    pub fn set_time(&self, position_ms: i64) {
        if position_ms < 0 {
            return;
        }
        let inner = self.lock();
        if inner.seek_if_ready(position_ms) {
            return;
        }
        if inner.vlc.is_none() {
            return;
        }
        let generation = inner.load_gen;
        drop(inner);
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("vlc-delayed-seek".into())
            .spawn(move || {
                let deadline = Instant::now() + DELAYED_SEEK_DEADLINE;
                while Instant::now() < deadline {
                    std::thread::sleep(DELAYED_SEEK_POLL);
                    let inner = lock(&shared);
                    // Track changed or deck closed: drop the seek.
                    if inner.load_gen != generation || inner.seek_if_ready(position_ms) {
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("[vlc] delayed seek not started: {e}");
        }
    }

    /// Current position, length and state.
    pub fn timing(&self) -> Timing {
        let inner = self.lock();
        let Some(vlc) = &inner.vlc else {
            return Timing {
                position_ms: 0,
                length_ms: 0,
                state: State::Stopped,
            };
        };
        let mp = vlc.mp.as_ptr();
        // SAFETY: the player is live.
        unsafe {
            Timing {
                position_ms: (vlc.api.libvlc_media_player_get_time)(mp),
                length_ms: (vlc.api.libvlc_media_player_get_length)(mp),
                state: State::from_raw((vlc.api.libvlc_media_player_get_state)(mp)),
            }
        }
    }

    /// Whether the attached source delivered all of its data and ended cleanly, as opposed to
    /// failing or being aborted. libvlc reaching its end while this is false means a read failed,
    /// not that the track ended.
    pub fn source_ended(&self) -> bool {
        self.lock().cur.as_ref().is_some_and(|cur| cur.media.eof())
    }

    /// Pauses playback.
    pub fn pause(&self) {
        self.lock().set_pause(true);
    }

    /// Resumes playback, or starts a media opened with [`Start::Paused`].
    pub fn resume(&self) {
        self.lock().set_pause(false);
    }

    /// Halts playback at once. The deck stays usable for a later load.
    pub fn stop(&self) {
        let mut inner = self.lock();
        if inner.vlc.is_none() {
            return;
        }
        inner.detach();
        inner.stop_player();
        inner.load_gen += 1; // cancel delayed seeks even if no new track follows
        if let Some(v) = inner.vlc.as_ref().and_then(|v| v.video.as_ref()) {
            v.end();
        }
    }
}

impl Drop for Deck {
    /// Releases all libvlc resources.
    fn drop(&mut self) {
        let mut inner = self.lock();
        inner.load_gen += 1; // delayed seeks see a stale generation and never touch the released player
        inner.detach();
        inner.stop_player();
        if let Some(mut vlc) = inner.vlc.take() {
            vlc.events = None; // detach the event callbacks first
            // SAFETY: the player is stopped, detached from callback media and has no event
            // callbacks; it is owned here and released once. `vlc.inst` drops after it.
            unsafe { (vlc.api.libvlc_media_player_release)(vlc.mp.as_ptr()) };
        }
    }
}

/// Marks `media` to start paused.
///
/// # Safety
/// `media` must be a live libvlc media that no player has started yet.
unsafe fn set_start(api: &Api, media: *mut Media, start: Start) {
    if start == Start::Paused {
        // SAFETY: guaranteed by the caller; libvlc copies the option.
        unsafe { (api.libvlc_media_add_option)(media, c":start-paused".as_ptr()) };
    }
}

fn lock(shared: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    /// Stops playback and drops the current callback source, if any. The abort runs first so a
    /// read blocked on an in-progress download returns and `libvlc_media_player_stop` can join
    /// the input thread. The media is detached so nothing can re-invoke the open callback on a
    /// dropped source.
    fn detach(&mut self) {
        let (Some(vlc), Some(cur)) = (&self.vlc, self.cur.take()) else {
            return;
        };
        cur.abort.abort();
        vlc.halt();
        // SAFETY: the player is live; passing null detaches its media.
        unsafe { (vlc.api.libvlc_media_player_set_media)(vlc.mp.as_ptr(), ptr::null_mut()) };
        // `cur.media` drops here, after libvlc has let go of it.
    }

    fn stop_player(&self) {
        if let Some(vlc) = &self.vlc {
            vlc.halt();
        }
    }

    fn play(&mut self, video: Option<VideoOutput>) -> Result<LoadId, Error> {
        self.load_gen += 1;
        let load = LoadId(self.load_gen);
        let vlc = self.vlc.as_ref().ok_or(Error::Closed)?;
        if let Some(events) = &vlc.events {
            events.sink().begin(load);
        }
        if let Some(v) = &vlc.video {
            v.begin(load, video);
        }
        // SAFETY: the player is live and has media.
        let ret = unsafe { (vlc.api.libvlc_media_player_play)(vlc.mp.as_ptr()) };
        // Volume and mute belong to the existing player and the desktop mixer. Re-applying them
        // here overwrote external mixer changes on every new track.
        if ret == 0 {
            Ok(load)
        } else {
            Err(Error::PlayFailed(ret))
        }
    }

    /// Applies a seek when libvlc is playing or paused; returns whether it did.
    fn seek_if_ready(&self, position_ms: i64) -> bool {
        let Some(vlc) = &self.vlc else { return false };
        let mp = vlc.mp.as_ptr();
        // SAFETY: the player is live.
        unsafe {
            if !State::from_raw((vlc.api.libvlc_media_player_get_state)(mp)).is_seekable() {
                return false;
            }
            (vlc.api.libvlc_media_player_set_time)(mp, position_ms);
        }
        true
    }

    fn set_pause(&self, pause: bool) {
        if let Some(vlc) = &self.vlc {
            // SAFETY: the player is live.
            unsafe { (vlc.api.libvlc_media_player_set_pause)(vlc.mp.as_ptr(), c_int::from(pause)) };
        }
    }
}

/// These need libvlc: run with `cargo test -p rustet-vlc -- --ignored`, with `libvlc.so.5` on
/// `LD_LIBRARY_PATH`. Audio and video go to libvlc's dummy outputs, so nothing is heard or shown.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::Player;
    use crate::callbacks::tests::MemSource;
    use std::ffi::CStr;
    use std::io::{self, Cursor, Read, Seek, SeekFrom};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    const SILENT_OUTPUTS: &[&CStr] =
        &[c"--aout=adummy", c"--vout=vdummy", c"--no-video-title-show"];

    fn test_player() -> Player {
        let player = Player::with_args(SILENT_OUTPUTS).expect("libvlc unavailable");
        player.set_volume(0);
        player
    }

    fn mem(data: Vec<u8>) -> MemSource {
        MemSource(Cursor::new(data))
    }

    pub(crate) fn silent_wav(seconds: u32) -> Vec<u8> {
        const RATE: u32 = 48_000;
        const CHANNELS: u16 = 2;
        const BITS: u16 = 16;
        let block = CHANNELS * BITS / 8;
        let data = RATE * u32::from(block) * seconds;
        let mut wav = Vec::with_capacity(44 + data as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&CHANNELS.to_le_bytes());
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * u32::from(block)).to_le_bytes());
        wav.extend_from_slice(&block.to_le_bytes());
        wav.extend_from_slice(&BITS.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data.to_le_bytes());
        wav.resize(44 + data as usize, 0);
        wav
    }

    /// Never yields data until aborted: libvlc reading ahead of an in-progress download.
    struct BlockingSource {
        abort: mpsc::Receiver<()>,
        abort_tx: Mutex<Option<mpsc::Sender<()>>>,
        closed: Arc<AtomicBool>,
    }

    impl BlockingSource {
        fn new() -> (Self, Arc<AtomicBool>) {
            let (tx, rx) = mpsc::channel();
            let closed = Arc::new(AtomicBool::new(false));
            let src = Self {
                abort: rx,
                abort_tx: Mutex::new(Some(tx)),
                closed: Arc::clone(&closed),
            };
            (src, closed)
        }
    }

    impl Read for BlockingSource {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            let _ = self.abort.recv();
            Err(io::ErrorKind::UnexpectedEof.into())
        }
    }
    impl Seek for BlockingSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            Ok(match pos {
                SeekFrom::Start(offset) => offset,
                _ => 0,
            })
        }
    }
    impl Source for BlockingSource {
        fn size(&self) -> Option<u64> {
            None
        }
        fn abort_handle(&self) -> AbortHandle {
            let tx = self.abort_tx.lock().unwrap().take();
            AbortHandle::new(move || drop(tx))
        }
    }
    impl Drop for BlockingSource {
        fn drop(&mut self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    /// Runs `f` on its own thread and fails if it does not finish in 5 s (a libvlc deadlock).
    fn within_5s<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(f()));
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{what} deadlocked"))
    }

    fn wait_for(player: &Player, timeout: Duration, done: impl Fn(Timing) -> bool) -> Timing {
        let deadline = Instant::now() + timeout;
        loop {
            let timing = player.timing();
            if done(timing) || Instant::now() > deadline {
                return timing;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    #[ignore = "needs libvlc"]
    fn load_source_plays_from_callbacks() {
        let player = test_player();
        player.load_source(mem(silent_wav(3))).unwrap();
        let t = wait_for(&player, Duration::from_secs(5), |t| {
            t.state == State::Playing && t.length_ms > 0
        });
        assert_eq!(t.state, State::Playing, "never reached playing: {t:?}");
        assert!(
            (2500..=3500).contains(&t.length_ms),
            "length = {}ms, want ~3000ms",
            t.length_ms
        );
    }

    #[test]
    #[ignore = "needs libvlc"]
    fn stop_unblocks_pending_read() {
        let player = Arc::new(test_player());
        let (src, closed) = BlockingSource::new();
        player.load_source(src).unwrap();
        std::thread::sleep(Duration::from_millis(300)); // let the input thread block in read
        let p = Arc::clone(&player);
        within_5s("stop on a blocked read callback", move || p.stop());
        assert!(
            closed.load(Ordering::SeqCst),
            "source not dropped after stop"
        );
    }

    // Instance decks halt with `set_media(NULL)` instead of `stop`: that must unblock a read too.
    #[test]
    #[ignore = "needs libvlc"]
    fn deck_stop_unblocks_pending_read() {
        let inst = Instance::with_args(SILENT_OUTPUTS).expect("libvlc unavailable");
        let deck = Arc::new(inst.deck(tokio::sync::mpsc::unbounded_channel().0).unwrap());
        let (src, closed) = BlockingSource::new();
        deck.open_source(src, Start::Playing).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let d = Arc::clone(&deck);
        within_5s("deck stop on a blocked read callback", move || d.stop());
        assert!(
            closed.load(Ordering::SeqCst),
            "source not dropped after stop"
        );
    }

    #[test]
    #[ignore = "needs libvlc"]
    fn load_source_replaces_previous() {
        let player = Arc::new(test_player());
        let (first, closed) = BlockingSource::new();
        player.load_source(first).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let p = Arc::clone(&player);
        within_5s("load_source replacing a blocked source", move || {
            p.load_source(mem(silent_wav(1)))
        })
        .unwrap();
        assert!(closed.load(Ordering::SeqCst), "previous source not dropped");
    }

    /// Plays a short ALAC file made by ffmpeg (`FFMPEG`, else `ffmpeg` on `PATH`) through the
    /// callbacks: a seek queued while opening lands, and the track ends with a clean eof. The dummy
    /// output has no clock, so libvlc plays faster than real time and pacing is not checked.
    #[test]
    #[ignore = "needs libvlc and ffmpeg"]
    fn plays_alac_to_the_end() {
        let dir = std::env::temp_dir().join(format!("rustet-vlc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tone.m4a");
        let ffmpeg = std::env::var_os("FFMPEG").unwrap_or_else(|| "ffmpeg".into());
        let status = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("sine=frequency=440:sample_rate=44100:duration=3")
            .args(["-c:a", "alac"])
            .arg(&file)
            .status()
            .expect("ffmpeg not found");
        assert!(status.success());
        let data = std::fs::read(&file).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let player = test_player();
        player.load_source(mem(data)).unwrap();
        player.set_time(1500); // queued: libvlc is still opening
        let t = wait_for(&player, Duration::from_secs(5), |t| {
            t.state == State::Playing && t.position_ms >= 1500
        });
        assert!(t.position_ms >= 1500, "delayed seek did not land: {t:?}");
        assert!(
            (2900..=3100).contains(&t.length_ms),
            "length = {}ms, want ~3000ms",
            t.length_ms
        );

        let t = wait_for(&player, Duration::from_secs(5), |t| t.state == State::Ended);
        assert_eq!(t.state, State::Ended, "never ended: {t:?}");
        assert!(t.position_ms >= 1500, "position went back: {t:?}");
        assert!(player.source_ended(), "ended without a clean end of data");
        assert!(player.volume().level <= MAX_VOLUME);
    }

    /// Port of the Go stutter probe: a 100 ms `set_time` drift-correction loop for 15 s on a local
    /// video (`TEST_VLC_FILE`), reporting call latency and landing error. A measurement, not a check.
    #[test]
    #[ignore = "needs libvlc and TEST_VLC_FILE"]
    fn set_time_stutter_probe() {
        let Some(file) = std::env::var_os("TEST_VLC_FILE") else {
            eprintln!("TEST_VLC_FILE not set, skipping");
            return;
        };
        let player = test_player();
        player
            .load(&format!("file://{}", file.to_string_lossy()))
            .unwrap();
        wait_for(&player, Duration::from_secs(8), |t| {
            t.state == State::Playing && t.position_ms > 0
        });
        std::thread::sleep(Duration::from_secs(2)); // settle

        const CORRECTION_MS: i64 = 30;
        const WARN: Duration = Duration::from_millis(15);
        let mut calls = Vec::new();
        let mut errors = Vec::new();
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            std::thread::sleep(Duration::from_millis(100));
            let t = player.timing();
            if t.state != State::Playing {
                continue;
            }
            let target = t.position_ms + CORRECTION_MS;
            let start = Instant::now();
            player.set_time(target);
            calls.push(start.elapsed());
            std::thread::sleep(Duration::from_millis(5));
            errors.push((player.timing().position_ms - target).abs());
        }
        assert!(!calls.is_empty(), "libvlc never entered the playing state");
        calls.sort();
        errors.sort();
        let pct = |n: usize, p: f64| ((n as f64 * p) as usize).min(n - 1);
        let warns = calls.iter().filter(|d| **d > WARN).count();
        eprintln!(
            "set_time latency p50={:?} p95={:?} max={:?}; landing error p50={}ms p95={}ms; >{WARN:?}: {warns}/{}",
            calls[pct(calls.len(), 0.5)],
            calls[pct(calls.len(), 0.95)],
            calls[calls.len() - 1],
            errors[pct(errors.len(), 0.5)],
            errors[pct(errors.len(), 0.95)],
            calls.len(),
        );
    }
}
