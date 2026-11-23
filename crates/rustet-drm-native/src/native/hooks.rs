//! The symbol hook libhybris consults for every import of an Android library, before its own
//! tables (`hybris_libc_shim_hook` in `drm_hybris.c`): the linker calls it once per relocation
//! with the symbol and the real path of the library being relocated.
//!
//! curl: the C hook also bound libmediaplatform's curl imports (and eight of
//! libCoreFoundation's nineteen) to the host libcurl that `libdrm_client.so` linked. Reading
//! the relocated GOT slots of a running C build confirmed it, and showed libCoreFoundation
//! mixing host easy handles with Android `curl_multi_*` calls: the libcurl ABI mixing commit
//! ce35d51 meant to avoid ("ALAC went silent after the first moof"). This crate links no
//! libcurl, so every curl import resolves to the Android libcurl. Only `curl_easy_setopt` is
//! bound to [`setopt_shim`], which applies the phone-OTP patch and forwards to the Android
//! libcurl's own `curl_easy_setopt`, on every architecture and without patching code.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing::{debug, info, trace, warn};

use super::guard;
use super::loader::{Core, path_cstring};

type HookCallback = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_void;
pub(crate) type SetHookCallback = unsafe extern "C" fn(HookCallback);

/// What the hooks need from [`install`].
struct Context {
    core: &'static Core,
    /// `ANDROID_ROOT` and `ANDROID_DATA` for the guest (none without a lib64 dir), see
    /// [`getenv`].
    android_root: Option<CString>,
    android_data: Option<CString>,
}

static CONTEXT: OnceLock<Context> = OnceLock::new();

/// Registers [`lookup`] with libhybris. Must run before the first Android library is loaded;
/// later calls keep the first context (the libraries are only loaded once).
pub(crate) fn install(core: &'static Core, set_hook: SetHookCallback, lib64: &Path) {
    let below_lib64 = |rel: &str| {
        (!lib64.as_os_str().is_empty())
            .then(|| path_cstring(&lib64.join(rel)))
            .flatten()
    };
    CONTEXT.get_or_init(|| Context {
        core,
        android_root: below_lib64(".."),
        android_data: below_lib64("../../data"),
    });
    // SAFETY: `lookup` has the C signature hybris expects and lives forever.
    unsafe { set_hook(lookup) };
}

/// Returns our function for `symbol`, or null to let libhybris resolve it as usual.
pub(super) unsafe extern "C" fn lookup(
    symbol: *const c_char,
    requester: *const c_char,
) -> *mut c_void {
    if symbol.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: libhybris passes the NUL-terminated name of the symbol being bound.
    match unsafe { CStr::from_ptr(symbol) }.to_bytes() {
        // Newer Apple Music builds (4.9.x/5.x) import two libc symbols bionic's libc.so here
        // does not define.
        b"__register_atfork" => register_atfork as *mut c_void,
        b"__memchr_chk" => memchr_chk as *mut c_void,
        // Every abort path of Apple's code and its C++ runtime ends in one of these (see
        // `guard`): std::terminate in libc++_shared calls abort, and bionic's
        // __stack_chk_fail, __assert2 and __fortify_chk_fail call abort through libc.so's PLT.
        b"abort" => guard::guest_abort as *mut c_void,
        b"__stack_chk_fail" => guard::guest_stack_chk_fail as *mut c_void,
        b"getenv" => getenv as *mut c_void,
        // libcurl's internal calls keep its own function.
        // SAFETY: libhybris passes the requester's path, or null.
        b"curl_easy_setopt" if !unsafe { is_libcurl(requester) } => setopt_shim as *mut c_void,
        _ => ptr::null_mut(),
    }
}

/// # Safety
/// `requester` must be null or a NUL-terminated string.
unsafe fn is_libcurl(requester: *const c_char) -> bool {
    if requester.is_null() {
        return false;
    }
    // SAFETY: per the caller.
    let path = unsafe { CStr::from_ptr(requester) }.to_bytes();
    path.rsplit(|&b| b == b'/').next() == Some(b"libcurl.so")
}

/// The guest's fork handlers never run here (nothing forks), so registering them is a no-op.
unsafe extern "C" fn register_atfork(
    _prepare: Option<unsafe extern "C" fn()>,
    _parent: Option<unsafe extern "C" fn()>,
    _child: Option<unsafe extern "C" fn()>,
    _dso: *mut c_void,
) -> c_int {
    0
}

/// `memchr` with a destination-size check; the check is dropped.
unsafe extern "C" fn memchr_chk(
    s: *const c_void,
    c: c_int,
    n: usize,
    _buf_size: usize,
) -> *mut c_void {
    // SAFETY: the guest passes the same arguments it would pass to memchr.
    unsafe { libc::memchr(s, c, n) }
}

/// The guest's `getenv`: the host environment, plus the variables the C set in it for bionic
/// (`drm_hybris.c`, `native/main.c`), which are answered here instead so the process
/// environment is never written while other threads may read it. Bionic logs "ANDROID_DATA/
/// ANDROID_ROOT not set!" on every timezone lookup without the first two (there is no tzdata
/// in either, so behaviour is unchanged); host values win, as with the C's `setenv(.., 0)`.
unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the guest passes a NUL-terminated name, as to getenv.
    let host = unsafe { libc::getenv(name) };
    let Some(cx) = CONTEXT.get() else { return host };
    // SAFETY: as above.
    let ours: Option<&CStr> = match unsafe { CStr::from_ptr(name) }.to_bytes() {
        b"ANDROID_DNS_MODE" => Some(c"local"),
        b"ANDROID_ROOT" if host.is_null() => cx.android_root.as_deref(),
        b"ANDROID_DATA" if host.is_null() => cx.android_data.as_deref(),
        _ => None,
    };
    let Some(ours) = ours else { return host };
    // getenv's result is `char *` but callers never write through it.
    ours.as_ptr().cast_mut()
}

// ── the phone-OTP patch (commit ce35d51) ────────────────────────────────────

const CURLOPT_WRITEFUNCTION: c_int = 20011;
const CURLOPT_WRITEDATA: c_int = 10001;
/// `CURLE_FAILED_INIT`, for the case the original cannot be found.
const CURLE_FAILED_INIT: c_int = 2;
const MAX_WATCHERS: usize = 256;

type WriteFn = unsafe extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;
type SetoptFn = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;

/// Calls of [`setopt_shim`] and checks by [`write_shim`], for the debug log.
static SETOPT_CALLS: AtomicUsize = AtomicUsize::new(0);
static BODY_CHECKS: AtomicUsize = AtomicUsize::new(0);

/// The Android libcurl's own `curl_easy_setopt`. Resolved on the first call: android_dlsym
/// takes the linker's recursive lock, so this is safe even from a library constructor.
fn original_setopt() -> Option<SetoptFn> {
    static ORIGINAL: OnceLock<Option<SetoptFn>> = OnceLock::new();
    *ORIGINAL.get_or_init(|| {
        let core = CONTEXT.get()?.core;
        let curl = core.open(c"libcurl.so", libc::RTLD_NOW)?;
        let f = core.sym(Some(curl), c"curl_easy_setopt");
        info!("[phone-patch] curl_easy_setopt shim forwards to Android libcurl at {f:p}");
        // SAFETY: a non-null result is libcurl's `CURLcode curl_easy_setopt(CURL *, CURLoption, ...)`.
        (!f.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, SetoptFn>(f) })
    })
}

#[derive(Clone, Copy)]
struct Watcher {
    handle: usize,
    orig_fn: Option<WriteFn>,
    orig_ud: usize,
    shimmed: bool,
}

const EMPTY: Watcher = Watcher {
    handle: 0,
    orig_fn: None,
    orig_ud: 0,
    shimmed: false,
};

/// One slot per curl handle whose write callback we own. A slot's address is handed to
/// libcurl as WRITEDATA, so the table is static. Slots are recycled round-robin once it is
/// full, so a long session of short-lived handles never exhausts it.
struct Watchers {
    slots: [Watcher; MAX_WATCHERS],
    next: usize,
}

static WATCHERS: Mutex<Watchers> = Mutex::new(Watchers {
    slots: [EMPTY; MAX_WATCHERS],
    next: 0,
});

impl Watchers {
    fn find_or_create(&mut self, handle: usize) -> usize {
        if let Some(i) = self.slots.iter().position(|w| w.handle == handle) {
            return i;
        }
        let i = self.next % MAX_WATCHERS;
        self.next = self.next.wrapping_add(1);
        self.slots[i] = Watcher { handle, ..EMPTY };
        i
    }
}

/// Flips `is-phone-number-supported</key><false/>` to `<true/> ` in a GrandSlam lookup
/// response, in place, so the library offers the phone-OTP flow behind "Can't get to your
/// devices?". Both value tags are 8 bytes (the space keeps the length). Returns whether it
/// patched.
fn patch_phone(data: &mut [u8]) -> bool {
    const KEY: &[u8] = b"is-phone-number-supported</key>";
    const FROM: &[u8] = b"<false/>";
    const TO: &[u8] = b"<true/> ";
    let len = data.len();
    if len < KEY.len() + FROM.len() {
        return false;
    }
    for p in 0..=len - KEY.len() {
        if &data[p..p + KEY.len()] != KEY {
            continue;
        }
        let mut q = p + KEY.len();
        while q < len - FROM.len() && matches!(data[q], b' ' | b'\t' | b'\n' | b'\r') {
            q += 1;
        }
        if q + FROM.len() <= len && &data[q..q + FROM.len()] == FROM {
            info!("[phone-patch] is-phone-number-supported false->true");
            data[q..q + FROM.len()].copy_from_slice(TO);
            return true;
        }
    }
    false
}

/// The write callback we install: patch the body, then forward to the library's own
/// callback with its own user data.
unsafe extern "C" fn write_shim(
    ptr: *mut c_char,
    size: usize,
    nmemb: usize,
    ud: *mut c_void,
) -> usize {
    let (orig_fn, orig_ud) = {
        let table = WATCHERS.lock().unwrap_or_else(|e| e.into_inner());
        let base = table.slots.as_ptr() as usize;
        let off = (ud as usize).wrapping_sub(base);
        if (ud as usize) < base
            || off >= size_of_val(&table.slots)
            || !off.is_multiple_of(size_of::<Watcher>())
        {
            return 0; // not one of ours: signal a write error rather than drop data
        }
        let w = table.slots[off / size_of::<Watcher>()];
        (w.orig_fn, w.orig_ud)
    };
    let total = size.wrapping_mul(nmemb);
    if !ptr.is_null() && total > 0 {
        // SAFETY: libcurl hands its callback `size * nmemb` bytes at `ptr`, writable for the
        // duration of the call.
        let patched =
            patch_phone(unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), total) });
        let n = BODY_CHECKS.fetch_add(1, Ordering::Relaxed) + 1;
        trace!("[phone-patch] body check {n}: {total} bytes, patched={patched}");
    }
    match orig_fn {
        // SAFETY: the callback and user data the library registered for this handle.
        Some(f) => unsafe { f(ptr, size, nmemb, orig_ud as *mut c_void) },
        None => total,
    }
}

/// `curl_easy_setopt` as Apple's libraries see it. The third argument is read as a pointer:
/// every curl option is pointer- or long-sized on LP64, and it is forwarded unchanged.
/// WRITEDATA is only withheld from libcurl once our callback owns the handle's writes; until
/// then it is forwarded and also remembered, so nothing is lost.
unsafe extern "C" fn setopt_shim(handle: *mut c_void, option: c_int, mut args: ...) -> c_int {
    // SAFETY: curl_easy_setopt always has one pointer-sized argument after the option.
    let param: *mut c_void = unsafe { args.next_arg() };
    let calls = SETOPT_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let Some(fwd) = original_setopt() else {
        warn!("[phone-patch] Android curl_easy_setopt not found; option {option} refused");
        return CURLE_FAILED_INIT;
    };
    if option == CURLOPT_WRITEFUNCTION {
        let (slot, restore_ud) = {
            let mut table = WATCHERS.lock().unwrap_or_else(|e| e.into_inner());
            let i = table.find_or_create(handle as usize);
            let w = &mut table.slots[i];
            if param.is_null() {
                // Back to libcurl's default writer: give it its WRITEDATA back.
                w.shimmed = false;
                w.orig_fn = None;
                (None, w.orig_ud)
            } else {
                // SAFETY: a non-null WRITEFUNCTION argument is a curl write callback.
                w.orig_fn = Some(unsafe { std::mem::transmute::<*mut c_void, WriteFn>(param) });
                w.shimmed = true;
                (Some(ptr::from_mut(w) as usize), 0)
            }
        };
        debug!(
            "[phone-patch] setopt call {calls}: write callback of handle {handle:p} {}",
            if slot.is_some() { "wrapped" } else { "reset" }
        );
        // SAFETY: forwarding to the original setopt with valid option/argument pairs.
        return unsafe {
            match slot {
                None => {
                    fwd(handle, CURLOPT_WRITEDATA, restore_ud as *mut c_void);
                    fwd(handle, option, ptr::null_mut::<c_void>())
                }
                Some(slot) => {
                    fwd(handle, CURLOPT_WRITEDATA, slot as *mut c_void);
                    fwd(handle, option, write_shim as WriteFn)
                }
            }
        };
    }
    if option == CURLOPT_WRITEDATA {
        let shimmed = {
            let mut table = WATCHERS.lock().unwrap_or_else(|e| e.into_inner());
            let i = table.find_or_create(handle as usize);
            table.slots[i].orig_ud = param as usize;
            table.slots[i].shimmed
        };
        if shimmed {
            return 0;
        }
    }
    // SAFETY: forwarding the caller's own option and argument.
    unsafe { fwd(handle, option, param) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_support_flag_is_flipped_in_place() {
        let mut body = b"<key>is-phone-number-supported</key>\n\t<false/><key>x</key>".to_vec();
        let len = body.len();
        assert!(patch_phone(&mut body));
        assert_eq!(body.len(), len);
        assert_eq!(
            body,
            b"<key>is-phone-number-supported</key>\n\t<true/> <key>x</key>"
        );
        // Already true, or another key's false, is left alone.
        assert!(!patch_phone(&mut body));
        let mut other = b"<key>is-other</key><false/>".to_vec();
        assert!(!patch_phone(&mut other));
    }

    // Apple's libraries must reach the shim. libcurl.so binds its own exported
    // curl_easy_setopt through its PLT too (for handles it makes internally): those calls
    // keep the real function.
    #[test]
    fn setopt_is_shimmed_for_apple_libraries_only() {
        let lookup = |requester: &CStr| {
            // SAFETY: both arguments are valid C strings.
            unsafe { lookup(c"curl_easy_setopt".as_ptr(), requester.as_ptr()) }
        };
        assert_eq!(
            lookup(c"/r/system/lib64/libmediaplatform.so"),
            setopt_shim as *mut c_void
        );
        assert!(lookup(c"/r/system/lib64/libcurl.so").is_null());
    }
}
