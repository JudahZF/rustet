//! Loading `libhybris-core.so` and Apple's Android libraries (`embedded_loader.c`,
//! `hybris_backend_init` in `drm_hybris.c`), and symbol lookup (`sym()`, `hybris_init_libs`
//! in `native/hybris_stubs.c`).
//!
//! The C build embedded `libhybris-core.so` in `libdrm_client.so` and loaded it from a memfd.
//! Here it is loaded from a file: the core is an architecture-specific Git LFS binary
//! (`packaging/hybris-<arch>/`), so the packaging step places the right one next to the
//! executable instead of the build baking in whichever copy the checkout has. Apple's
//! libraries were always loaded from disk: FairPlay locates files relative to its own path.

use std::ffi::{CStr, CString, OsStr, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing::{debug, info, trace, warn};

use super::session::{self, FairPlay};
use super::{Error, FairPlayConfig, hooks};

type AndroidDlopen = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;
type AndroidDlsym = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type AndroidDlerror = unsafe extern "C" fn() -> *const c_char;

/// `hybris_stubs.c` RTLD_NOW_GLOBAL; bionic uses the glibc values.
const RTLD_NOW_GLOBAL: c_int = libc::RTLD_NOW | libc::RTLD_GLOBAL;

/// Where the libhybris pieces and Apple's libraries are.
#[derive(Clone, Debug)]
pub struct HybrisPaths {
    /// `libhybris-core.so` for this architecture.
    pub core: PathBuf,
    /// The directory holding the Android linker shim `q.so`.
    pub linker_dir: PathBuf,
    /// `rootfs/system/lib64` with Apple's Android libraries.
    pub lib64_dir: PathBuf,
}

impl HybrisPaths {
    /// The installed layout: `libhybris-core.so` and `hybris-linker/q.so` next to the
    /// executable (the engine looked for `hybris-linker/` there).
    pub fn beside_executable(lib64_dir: PathBuf) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().unwrap_or(Path::new("."));
        Ok(Self {
            core: dir.join("libhybris-core.so"),
            linker_dir: dir.join("hybris-linker"),
            lib64_dir,
        })
    }
}

/// The loaded `libhybris-core.so` entry points (`g_el` in `embedded_loader.c`).
pub(crate) struct Core {
    dlopen: AndroidDlopen,
    dlsym: AndroidDlsym,
    dlerror: Option<AndroidDlerror>,
    set_hook_callback: Option<hooks::SetHookCallback>,
}

/// An Android library handle from `android_dlopen`.
#[derive(Clone, Copy)]
pub(crate) struct Handle(*mut c_void);

// SAFETY: a handle is an opaque token of the Android linker, valid for the process lifetime
// (nothing is ever unloaded); the linker's dlsym is thread-safe.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}

impl Core {
    /// `android_dlopen`; `None` on failure.
    pub(crate) fn open(&self, name: &CStr, flags: c_int) -> Option<Handle> {
        // SAFETY: `name` is a valid C string; android_dlopen has no other preconditions.
        let h = unsafe { (self.dlopen)(name.as_ptr(), flags) };
        (!h.is_null()).then_some(Handle(h))
    }

    /// `android_dlsym`; `handle` `None` searches every loaded library.
    pub(crate) fn sym(&self, handle: Option<Handle>, name: &CStr) -> *mut c_void {
        let h = handle.map_or(std::ptr::null_mut(), |h| h.0);
        // SAFETY: `h` is null or a live handle, `name` a valid C string.
        unsafe { (self.dlsym)(h, name.as_ptr()) }
    }

    /// The Android linker's last error.
    pub(crate) fn last_error(&self) -> String {
        let Some(dlerror) = self.dlerror else {
            return String::new();
        };
        // SAFETY: android_dlerror has no preconditions.
        let msg = unsafe { dlerror() };
        if msg.is_null() {
            return String::new();
        }
        // SAFETY: non-null android_dlerror results are NUL-terminated strings.
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }
}

/// The libraries loaded by [`Hybris::load`]. Process-wide: the Android linker cannot unload
/// them, and Apple's code keeps global state.
pub struct Hybris {
    pub(crate) core: &'static Core,
}

static CORE: OnceLock<Core> = OnceLock::new();
static HYBRIS: OnceLock<Hybris> = OnceLock::new();
static LOAD_LOCK: Mutex<()> = Mutex::new(());

impl Hybris {
    /// Loads `libhybris-core.so` and, through it, `libCoreFP.so`, `libstoreservicescore.so` and
    /// `libandroidappmusic.so` with their dependencies (`hybris_backend_init`). Safe to call
    /// again: after a success it returns the same libraries.
    pub fn load(paths: &HybrisPaths) -> Result<&'static Hybris, Error> {
        let _lock = LOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = HYBRIS.get() {
            return Ok(h);
        }
        #[cfg(target_arch = "aarch64")]
        super::tls::check()?;

        let core = match CORE.get() {
            Some(core) => core,
            None => {
                set_hybris_env(paths);
                let core = load_core(&paths.core)?;
                CORE.get_or_init(|| core)
            }
        };

        // Consulted for every symbol the Android libraries import, before libhybris's own
        // tables: libc gaps, the abort barrier, the guest environment and the phone-OTP patch.
        match core.set_hook_callback {
            Some(set_hook) => hooks::install(core, set_hook, &paths.lib64_dir),
            None => warn!("libhybris-core.so has no hybris_set_hook_callback: no symbol hooks"),
        }

        // libandroidappmusic.so depends on libCoreFP.so; load it first.
        info!("loading libCoreFP.so");
        let corefp = core.open(c"libCoreFP.so", libc::RTLD_NOW);
        info!(
            "libCoreFP.so handle={:p}",
            corefp.map_or(std::ptr::null_mut(), |h| h.0)
        );
        if corefp.is_none() {
            warn!("libCoreFP.so: load failed");
        }

        info!("loading libstoreservicescore.so");
        let ssc = core.open(c"libstoreservicescore.so", RTLD_NOW_GLOBAL);
        info!(
            "libstoreservicescore.so handle={:p}",
            ssc.map_or(std::ptr::null_mut(), |h| h.0)
        );
        if ssc.is_none() {
            warn!("libstoreservicescore.so: load failed (may still work)");
        }

        info!("loading libandroidappmusic.so");
        let Some(appmusic) = core.open(c"libandroidappmusic.so", libc::RTLD_NOW) else {
            warn!("libandroidappmusic.so: load failed: {}", core.last_error());
            return Err(Error::Backend("libandroidappmusic.so did not load".into()));
        };
        info!("libandroidappmusic.so handle={:p}", appmusic.0);
        let nfc = core.sym(Some(appmusic), c"NfcRKVnxuKZy04KWbdFu71Ou");
        debug!(
            "libandroidappmusic.so: NfcRK={}",
            if nfc.is_null() { "missing" } else { "resolved" }
        );
        if nfc.is_null() {
            warn!("no usable backend — giving up");
            return Err(Error::Backend(
                "libandroidappmusic.so has no NfcRKVnxuKZy04KWbdFu71Ou".into(),
            ));
        }

        debug!("hybris initialized");
        Ok(HYBRIS.get_or_init(|| Hybris { core }))
    }

    /// Starts the FairPlay session (`hybris_fairplay_init`): request context, sign-in when
    /// `cfg.credentials` is set, playback lease, account tokens. Blocks for seconds, longer
    /// while Apple waits for the user. Once a start has succeeded, later calls sign in with new
    /// credentials, or return the running session when there are none.
    pub fn start_fair_play(&'static self, cfg: FairPlayConfig) -> Result<FairPlay, Error> {
        session::start(self, cfg)
    }

    /// The handles `hybris_init_libs` opens for symbol lookup: the libraries by full path (the
    /// linker returns the already loaded ones), after preloading bionic's implicit libraries so
    /// version-needed checks pass.
    pub(crate) fn open_for_lookup(&self, lib64: &Path) -> Result<Resolver, Error> {
        for lib in ["libdl.so", "libc.so", "libm.so"] {
            if let Some(path) = path_cstring(&lib64.join(lib)) {
                let _ = self.core.open(&path, RTLD_NOW_GLOBAL);
            }
        }
        let open = |name: &str| -> Result<Handle, Error> {
            let path = path_cstring(&lib64.join(name))
                .ok_or_else(|| Error::Backend(format!("bad path for {name}")))?;
            self.core.open(&path, RTLD_NOW_GLOBAL).ok_or_else(|| {
                let why = self.core.last_error();
                warn!("cannot load {name}: {why}");
                Error::Backend(format!("cannot load {name}: {why}"))
            })
        };
        let ssc = open("libstoreservicescore.so")?;
        let apm = open("libandroidappmusic.so")?;
        debug!("hybris libs loaded");
        Ok(Resolver {
            core: self.core,
            ssc,
            apm,
        })
    }
}

/// Symbol lookup across `libstoreservicescore.so` then `libandroidappmusic.so` (and their
/// dependency trees), the order of `sym()` in `hybris_stubs.c`. (libCoreFP.so was only added
/// to that search on Windows.)
pub(crate) struct Resolver {
    core: &'static Core,
    ssc: Handle,
    apm: Handle,
}

impl Resolver {
    pub(crate) fn find(&self, name: &str) -> Option<*mut c_void> {
        let cname = CString::new(name).ok()?;
        let mut p = self.core.sym(Some(self.ssc), &cname);
        if p.is_null() {
            p = self.core.sym(Some(self.apm), &cname);
        }
        trace!("resolve {name} -> {p:p}");
        (!p.is_null()).then_some(p)
    }

    /// Like [`find`](Self::find), but a missing symbol is an error (the C aborted).
    pub(crate) fn require(&self, name: &str) -> Result<*mut c_void, Error> {
        self.find(name).ok_or_else(|| {
            warn!("FATAL: {name}");
            Error::MissingSymbol(name.to_owned())
        })
    }

    /// A symbol only in `libandroidappmusic.so` (FootHillConfig's two spellings).
    pub(crate) fn find_in_appmusic(&self, name: &CStr) -> Option<*mut c_void> {
        let p = self.core.sym(Some(self.apm), name);
        (!p.is_null()).then_some(p)
    }
}

/// The environment libhybris reads when it loads (the top of `hybris_backend_init`): the
/// variables the C also set for bionic are answered by the guest's `getenv` hook instead
/// (`hooks`), and `HYBRIS_ANDROID_LIB64` had no reader.
///
/// - `HYBRIS_LINKER_DIR`: libhybris-core.so dlopens its Android linker shim (q.so) from there.
/// - `HYBRIS_LD_LIBRARY_PATH`: bionic system libs (libc/libm/libdl/libz/libcurl/...) resolve
///   from the lib64 dir.
fn hybris_env(paths: &HybrisPaths) -> impl Iterator<Item = (&'static str, &Path)> {
    [
        ("HYBRIS_LINKER_DIR", &*paths.linker_dir),
        ("HYBRIS_LD_LIBRARY_PATH", &*paths.lib64_dir),
    ]
    .into_iter()
    .filter(|(name, value)| {
        !value.as_os_str().is_empty()
            && std::env::var_os(name).as_deref() != Some(value.as_os_str())
    })
}

/// Sets the environment libhybris needs, while the process still has one thread. The engine
/// calls this first thing in `main`; [`Hybris::load`] then finds the variables in place and
/// writes nothing. Refuses when other threads exist, because writing the environment is only
/// sound without them.
pub fn set_env_before_threads(paths: &HybrisPaths) -> std::io::Result<()> {
    let threads = std::fs::read_dir("/proc/self/task")?.count();
    if threads != 1 {
        return Err(std::io::Error::other(format!(
            "the process already has {threads} threads"
        )));
    }
    for (name, value) in hybris_env(paths) {
        // SAFETY: this is the only thread (checked above), so nothing reads or writes the
        // environment at the same time.
        unsafe { std::env::set_var(name, value) };
    }
    Ok(())
}

/// Sets what [`set_env_before_threads`] did not, on the first [`Hybris::load`].
fn set_hybris_env(paths: &HybrisPaths) {
    for (name, value) in hybris_env(paths) {
        // SAFETY: set_var is unsound while another thread reads or writes the environment
        // without std's lock, which C code (getenv in any library) does not take. This runs
        // once per process, before libhybris (the reader) exists, and only when the process
        // did not call set_env_before_threads (tests and tools): it then carries the same
        // risk the C build had.
        unsafe { std::env::set_var(name, value) };
    }
}

/// `dlopen`s the core with the host linker and resolves its entry points
/// (`embedded_loader_init_path`).
fn load_core(path: &Path) -> Result<Core, Error> {
    let cpath =
        path_cstring(path).ok_or_else(|| Error::Core(format!("bad path {}", path.display())))?;
    // SAFETY: `cpath` is a valid C string. Loading runs the core's constructors, which only
    // read the environment set by set_hybris_env.
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(Error::Core(format!(
            "dlopen({}): {}",
            path.display(),
            host_dlerror()
        )));
    }
    let entry = |name: &CStr| {
        // SAFETY: `handle` is a live host-linker handle, `name` a valid C string.
        let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
        (!p.is_null()).then_some(p)
    };
    let (Some(dlopen), Some(dlsym)) = (entry(c"android_dlopen"), entry(c"android_dlsym")) else {
        // SAFETY: `handle` came from dlopen above and nothing from it is in use.
        unsafe { libc::dlclose(handle) };
        return Err(Error::Core(
            "android_dlopen/dlsym not found in libhybris-core.so".into(),
        ));
    };
    debug!("libhybris-core.so loaded from {}", path.display());
    // SAFETY: the exports have these C signatures (libhybris's public API).
    unsafe {
        Ok(Core {
            dlopen: std::mem::transmute::<*mut c_void, AndroidDlopen>(dlopen),
            dlsym: std::mem::transmute::<*mut c_void, AndroidDlsym>(dlsym),
            dlerror: entry(c"android_dlerror")
                .map(|p| std::mem::transmute::<*mut c_void, AndroidDlerror>(p)),
            set_hook_callback: entry(c"hybris_set_hook_callback")
                .map(|p| std::mem::transmute::<*mut c_void, hooks::SetHookCallback>(p)),
        })
    }
}

fn host_dlerror() -> String {
    // SAFETY: dlerror has no preconditions.
    let msg = unsafe { libc::dlerror() };
    if msg.is_null() {
        return String::new();
    }
    // SAFETY: a non-null dlerror result is a NUL-terminated string.
    unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn path_cstring(path: &Path) -> Option<CString> {
    CString::new(OsStr::as_bytes(path.as_os_str())).ok()
}
