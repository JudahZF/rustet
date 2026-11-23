//! Hand-written libvlc 3 bindings, loaded at run time.
//!
//! Only the functions the player calls are bound. There is no build-time dependency on VLC: when
//! the library is missing, [`api`] fails and the engine runs without lossless playback.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::marker::{PhantomData, PhantomPinned};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libloading::Library;

/// Declares an opaque libvlc handle type: only ever used behind a pointer.
macro_rules! opaque {
    ($($name:ident),*) => {$(
        #[repr(C)]
        pub(crate) struct $name {
            _data: [u8; 0],
            _marker: PhantomData<(*mut u8, PhantomPinned)>,
        }
    )*};
}
opaque!(Instance, Media, MediaPlayer, EventManager);

/// `libvlc_event_t` (libvlc 3), only the union members the deck reads.
#[repr(C)]
pub(crate) struct Event {
    pub kind: c_int,
    pub _obj: *mut c_void,
    pub u: EventData,
}

/// The start of libvlc's event union. Reading through a pointer to the real event is sound
/// because every member read here starts at offset 0 of the C union, which has the same
/// alignment (it holds `int64_t` and pointers).
#[repr(C)]
pub(crate) union EventData {
    /// `media_player_buffering.new_cache`, 0–100.
    pub new_cache: f32,
    /// `media_player_time_changed.new_time` and `media_player_length_changed.new_length`, in ms.
    pub new_time: i64,
    _ptr: *mut c_void,
}

pub(crate) type EventCb = unsafe extern "C" fn(*const Event, *mut c_void);

/// `libvlc_event_e` values of the media player events the deck forwards (libvlc 3).
pub(crate) mod event {
    use std::ffi::c_int;
    pub const BUFFERING: c_int = 0x103;
    pub const PLAYING: c_int = 0x104;
    pub const PAUSED: c_int = 0x105;
    pub const END_REACHED: c_int = 0x109;
    pub const ERROR: c_int = 0x10a;
    pub const TIME_CHANGED: c_int = 0x10b;
    pub const LENGTH_CHANGED: c_int = 0x111;
}

pub(crate) type OpenCb = unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut u64) -> c_int;
pub(crate) type ReadCb = unsafe extern "C" fn(*mut c_void, *mut u8, usize) -> isize;
pub(crate) type SeekCb = unsafe extern "C" fn(*mut c_void, u64) -> c_int;
pub(crate) type CloseCb = unsafe extern "C" fn(*mut c_void);

/// `libvlc_video_format_cb`: chroma (4 bytes), width, height, pitches, lines → buffer count.
pub(crate) type VideoFormatCb = unsafe extern "C" fn(
    *mut *mut c_void,
    *mut c_char,
    *mut c_uint,
    *mut c_uint,
    *mut c_uint,
    *mut c_uint,
) -> c_uint;
pub(crate) type VideoCleanupCb = unsafe extern "C" fn(*mut c_void);
/// `libvlc_video_lock_cb`: fills the plane pointers, returns a picture ID.
pub(crate) type VideoLockCb = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> *mut c_void;
pub(crate) type VideoUnlockCb = unsafe extern "C" fn(*mut c_void, *mut c_void, *const *mut c_void);
pub(crate) type VideoDisplayCb = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// `libvlc_state_t` values (libvlc 3).
pub(crate) mod state {
    use std::ffi::c_int;
    pub const NOTHING_SPECIAL: c_int = 0;
    pub const OPENING: c_int = 1;
    pub const BUFFERING: c_int = 2;
    pub const PLAYING: c_int = 3;
    pub const PAUSED: c_int = 4;
    pub const STOPPED: c_int = 5;
    pub const ENDED: c_int = 6;
    pub const ERROR: c_int = 7;
}

/// Defines [`Api`]: one function pointer per bound libvlc symbol, resolved by name.
macro_rules! libvlc_api {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// The resolved libvlc functions. The libraries stay loaded for the life of the process.
        pub(crate) struct Api {
            $(pub(crate) $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
            _lib: Library,
            _core: Option<Library>,
        }

        impl Api {
            /// # Safety
            /// `lib` must be libvlc 3 (`libvlc.so.5` / `libvlc.dll`), so every symbol has the declared signature.
            unsafe fn resolve(lib: Library, core: Option<Library>) -> Result<Self, libloading::Error> {
                $(
                    // SAFETY: the caller guarantees `lib` is libvlc 3, whose `$name` has this C signature.
                    let $name = unsafe {
                        *lib.get::<unsafe extern "C" fn($($arg),*) $(-> $ret)?>(stringify!($name))?
                    };
                )*
                Ok(Self { $($name,)* _lib: lib, _core: core })
            }
        }
    };
}

libvlc_api! {
    libvlc_new: fn(c_int, *const *const c_char) -> *mut Instance;
    libvlc_release: fn(*mut Instance);
    libvlc_set_user_agent: fn(*mut Instance, *const c_char, *const c_char);
    libvlc_set_app_id: fn(*mut Instance, *const c_char, *const c_char, *const c_char);
    libvlc_media_new_location: fn(*mut Instance, *const c_char) -> *mut Media;
    libvlc_media_new_callbacks: fn(
        *mut Instance, Option<OpenCb>, Option<ReadCb>, Option<SeekCb>, Option<CloseCb>, *mut c_void
    ) -> *mut Media;
    libvlc_media_release: fn(*mut Media);
    libvlc_media_add_option: fn(*mut Media, *const c_char);
    libvlc_media_player_new: fn(*mut Instance) -> *mut MediaPlayer;
    libvlc_media_player_release: fn(*mut MediaPlayer);
    libvlc_media_player_set_media: fn(*mut MediaPlayer, *mut Media);
    libvlc_media_player_play: fn(*mut MediaPlayer) -> c_int;
    libvlc_media_player_stop: fn(*mut MediaPlayer);
    libvlc_media_player_set_pause: fn(*mut MediaPlayer, c_int);
    libvlc_media_player_get_state: fn(*mut MediaPlayer) -> c_int;
    libvlc_media_player_get_time: fn(*mut MediaPlayer) -> i64;
    libvlc_media_player_get_length: fn(*mut MediaPlayer) -> i64;
    libvlc_media_player_set_time: fn(*mut MediaPlayer, i64);
    libvlc_audio_get_volume: fn(*mut MediaPlayer) -> c_int;
    libvlc_audio_set_volume: fn(*mut MediaPlayer, c_int) -> c_int;
    libvlc_audio_get_mute: fn(*mut MediaPlayer) -> c_int;
    libvlc_audio_set_mute: fn(*mut MediaPlayer, c_int);
    libvlc_media_player_event_manager: fn(*mut MediaPlayer) -> *mut EventManager;
    libvlc_event_attach: fn(*mut EventManager, c_int, EventCb, *mut c_void) -> c_int;
    libvlc_event_detach: fn(*mut EventManager, c_int, EventCb, *mut c_void);
    libvlc_video_set_format_callbacks: fn(*mut MediaPlayer, Option<VideoFormatCb>, Option<VideoCleanupCb>);
    libvlc_video_set_callbacks: fn(
        *mut MediaPlayer, Option<VideoLockCb>, Option<VideoUnlockCb>, Option<VideoDisplayCb>, *mut c_void
    );
}

#[cfg(unix)]
const LIBVLC: &str = "libvlc.so.5";
#[cfg(unix)]
const LIBVLCCORE: &str = "libvlccore.so.9";
#[cfg(windows)]
const LIBVLC: &str = "libvlc.dll";

/// The process-wide libvlc, loaded on first use. The error is the loader's message.
pub(crate) fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        let bundled = bundled_dir().filter(|dir| dir.join(LIBVLC).is_file());
        let api = match &bundled {
            Some(dir) => Api::load_dir(dir),
            None => Api::load_system(),
        };
        match &api {
            Ok(_) => tracing::debug!(dir = ?bundled, "[vlc] libvlc loaded"),
            Err(e) => tracing::info!("[vlc] libvlc unavailable: {e}"),
        }
        api
    })
    .as_ref()
    .map_err(String::as_str)
}

/// The bundled VLC subset: `vlc/` next to the executable, as `scripts/bundle-vlc.sh` and
/// `scripts/build-windows.ps1` lay out the resources directory.
fn bundled_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("vlc"))
}

/// The loader's message with its cause (`dlerror` / `GetLastError`), which `Display` leaves out.
fn describe(e: &libloading::Error) -> String {
    match std::error::Error::source(e) {
        Some(cause) => format!("{e}: {cause}"),
        None => e.to_string(),
    }
}

impl Api {
    /// Loads libvlc through the platform search path (`LD_LIBRARY_PATH` / `PATH`), like the
    /// dynamically linked Go engine did.
    fn load_system() -> Result<Self, String> {
        // SAFETY: loading libvlc runs its initialisers, which have no preconditions; the name is libvlc 3.
        unsafe {
            let lib = Library::new(LIBVLC)
                .map_err(|e| format!("{} (is {LIBVLC} on the library path?)", describe(&e)))?;
            Self::resolve(lib, None).map_err(|e| describe(&e))
        }
    }

    /// Loads libvlc from `dir`. libvlccore is loaded from the same directory first so the
    /// dynamic linker reuses it for libvlc and the plugins, without `LD_LIBRARY_PATH`.
    /// Plugins are still found through `VLC_PLUGIN_PATH`, which the shell sets.
    #[cfg(unix)]
    pub(crate) fn load_dir(dir: &Path) -> Result<Self, String> {
        use libloading::os::unix::{Library as UnixLibrary, RTLD_GLOBAL, RTLD_NOW};
        let open = |name: &str| {
            let path = dir.join(name);
            // SAFETY: loading libvlc / libvlccore runs their initialisers, which have no preconditions.
            unsafe { UnixLibrary::open(Some(&path), RTLD_NOW | RTLD_GLOBAL) }
                .map(Library::from)
                .map_err(|e| describe(&e))
        };
        let core = open(LIBVLCCORE)?;
        let lib = open(LIBVLC)?;
        // SAFETY: `lib` is the libvlc 3 shared object (`libvlc.so.5`).
        unsafe { Self::resolve(lib, Some(core)) }.map_err(|e| describe(&e))
    }

    /// Loads libvlc from `dir`. The altered search path makes Windows resolve libvlccore.dll from
    /// the same directory, and libvlc finds its `plugins` directory next to itself.
    #[cfg(windows)]
    pub(crate) fn load_dir(dir: &Path) -> Result<Self, String> {
        use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library as WinLibrary};
        let path = dir.join(LIBVLC);
        // SAFETY: loading libvlc runs its initialisers, which have no preconditions; the file is libvlc 3.
        unsafe {
            let lib = WinLibrary::load_with_flags(&path, LOAD_WITH_ALTERED_SEARCH_PATH)
                .map(Library::from)
                .map_err(|e| describe(&e))?;
            Self::resolve(lib, None).map_err(|e| describe(&e))
        }
    }
}
