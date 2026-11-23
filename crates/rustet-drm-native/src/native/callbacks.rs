//! `std::__ndk1::function` objects handed to Apple's code (`native/hybris_stubs.c`).
//!
//! NDK libc++ lays a `std::function` out as a 32-byte inline buffer followed by the callable
//! pointer `__f_` at +0x20 (48 bytes, 16-aligned). A small callable lives in the buffer and
//! `__f_` points at it; a large one is on the heap. Apple's `SVPlaybackLeaseManager`
//! constructor reads `__f_` at +0x20 (disassembly of the AArch64 build), as the
//! `RequestContextConfig` setters of the newer libraries do.
//!
//! The C built the lease callbacks (and the `RequestContextConfig` ones) without that layout:
//! the lease callbacks as 32-byte arrays with `__f_` first, so Apple's code read each
//! callback's `__f_` from whatever the linker placed after the array. What that called depended
//! on how the C was linked (with `endLeaseCallback` directly before `pbErrCallback`, the
//! lease-end callback called the playback-error callable). Here every function object has the
//! real layout and reaches its own handler.
//!
//! The vtable functions are entered from the guest through the `__base` vtable
//! (`{~D1, ~D0, clone(), clone(dest), destroy, destroy_deallocate, operator(), target,
//! target_type}`; the object's vptr points at `~D1`), with the C ABI. Heap copies use the host
//! `malloc`/`free`, as the C did.

use std::ffi::{c_char, c_int, c_long, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use tracing::debug;

use super::cxx::{StdString, StdVector};
use super::loader::Resolver;
use super::recovery;

/// A vtable as Apple's code reads it: offset-to-top, RTTI, then the virtual functions.
#[repr(C)]
struct Vtable([*const c_void; 9]);

// SAFETY: the table only holds function addresses and is never written.
unsafe impl Sync for Vtable {}

// ── lease callbacks ─────────────────────────────────────────────────────────

/// The `__func` object in a lease callback's inline buffer: vptr, the callable, allocator.
#[repr(C)]
#[derive(Clone, Copy)]
struct LeaseFunc {
    vptr: *const *const c_void,
    callable: unsafe extern "C" fn(*const c_void),
    allocator: usize,
}

/// `std::function<void(int const&)>` / `std::function<void(shared_ptr<StoreErrorCondition>
/// const&)>` holding a [`LeaseFunc`] in its inline buffer (see the module docs).
#[repr(C, align(16))]
pub(crate) struct LeaseFunction {
    buf: LeaseFunc,
    _buf_rest: usize,
    f: *mut LeaseFunc,
}

unsafe extern "C" fn lease_destroy(_self: *mut LeaseFunc) {}

unsafe extern "C" fn lease_destroy_deallocate(this: *mut LeaseFunc) {
    // SAFETY: only heap clones made by `lease_clone` are destroyed through this entry.
    unsafe { libc::free(this.cast()) };
}

unsafe extern "C" fn lease_clone(this: *const LeaseFunc) -> *mut LeaseFunc {
    // SAFETY: malloc has no preconditions; the copy is written only when it succeeded.
    unsafe {
        let copy = libc::malloc(size_of::<LeaseFunc>()).cast::<LeaseFunc>();
        if !copy.is_null() {
            ptr::copy_nonoverlapping(this, copy, 1);
        }
        copy
    }
}

unsafe extern "C" fn lease_clone_into(this: *const LeaseFunc, dest: *mut LeaseFunc) {
    // SAFETY: `dest` is the destination function's inline buffer, large enough for a LeaseFunc.
    unsafe { ptr::copy_nonoverlapping(this, dest, 1) };
}

unsafe extern "C" fn lease_invoke(this: *const LeaseFunc, arg: *const c_void) {
    // SAFETY: `this` is one of our `__func` objects; the callable takes the argument reference.
    unsafe { ((*this).callable)(arg) };
}

/// One vtable serves both callbacks: `operator()` calls the stored callable, whose argument is
/// a reference either way.
static LEASE_VTABLE: Vtable = Vtable([
    ptr::null(),
    ptr::null(),
    lease_destroy as *const c_void,
    lease_destroy_deallocate as *const c_void,
    lease_clone as *const c_void,
    lease_clone_into as *const c_void,
    lease_destroy as *const c_void,
    lease_destroy_deallocate as *const c_void,
    lease_invoke as *const c_void,
]);

/// The lease-end and playback-error callbacks for `SVPlaybackLeaseManager`'s constructor.
pub(crate) struct LeaseCallbacks {
    pub end_lease: *const LeaseFunction,
    pub playback_error: *const LeaseFunction,
}

// SAFETY: the objects are leaked, never written after construction, and only read by Apple's
// code (which copies them).
unsafe impl Send for LeaseCallbacks {}
// SAFETY: as above.
unsafe impl Sync for LeaseCallbacks {}

/// The two callback objects (`hybris_init_callbacks`), built once: `__f_` points into the
/// object itself, so they need a fixed address. The lease manager copies them (through
/// `clone(dest)`, as the callable is in the inline buffer).
pub(crate) fn lease_callbacks() -> &'static LeaseCallbacks {
    static CALLBACKS: OnceLock<LeaseCallbacks> = OnceLock::new();
    CALLBACKS.get_or_init(|| {
        let build = |callable: unsafe extern "C" fn(*const c_void)| -> *const LeaseFunction {
            let vptr = ptr::from_ref(&LEASE_VTABLE.0[2]);
            let f = Box::leak(Box::new(LeaseFunction {
                buf: LeaseFunc {
                    vptr,
                    callable,
                    allocator: 0,
                },
                _buf_rest: 0,
                f: ptr::null_mut(),
            }));
            f.f = ptr::from_mut(&mut f.buf);
            f
        };
        LeaseCallbacks {
            end_lease: build(recovery::on_lease_end),
            playback_error: build(recovery::on_playback_error),
        }
    })
}

// ── RequestContextConfig callbacks (4.9.x / 5.x) ────────────────────────────

/// The heap callable: our vtable and the callback's name, for the log.
#[repr(C)]
#[derive(Clone, Copy)]
struct RccCallable {
    vptr: *const *const c_void,
    name: *const c_char,
}

/// A `std::function` holding a heap [`RccCallable`]: inline buffer unused, `__f_` at +0x20,
/// 48 bytes and 16-aligned like the real one (see the module docs).
#[repr(C, align(16))]
struct RccFunction {
    buf: [usize; 4],
    f: *mut RccCallable,
    _padding: usize,
}

fn rcc_name(this: *const RccCallable) -> String {
    // SAFETY: `this` is one of our callables, whose name is a static C string.
    unsafe { std::ffi::CStr::from_ptr((*this).name) }
        .to_string_lossy()
        .into_owned()
}

fn rcc_log(this: *const RccCallable, extra: &str) {
    debug!("[rcc] {} {extra}", rcc_name(this));
}

/// The key/value store Apple's code keeps through `Get/SetValueForKeyCallback`, in memory,
/// with at most 128 keys (`rcc_kv`).
static RCC_KV: Mutex<Vec<(Vec<u8>, Vec<u8>)>> = Mutex::new(Vec::new());
const RCC_KV_MAX: usize = 128;

/// # Safety
/// `s` must be a valid libc++ string.
unsafe fn rcc_bytes(s: *const StdString) -> Vec<u8> {
    // SAFETY: per the caller.
    unsafe { (*s).as_bytes() }.to_vec()
}

// One operator() per signature. Arguments arrive by reference; object results are returned by
// value (the hidden result address of the platform ABI, see `cxx`).

unsafe extern "C" fn op_get_cookie(
    this: *const RccCallable,
    _url: *const StdString,
    _dsid: *const c_long,
) -> StdString {
    rcc_log(this, "");
    StdString::guest_owned(b"")
}
unsafe extern "C" fn op_void_str_long(
    this: *const RccCallable,
    _s: *const StdString,
    _l: *const c_long,
) {
    rcc_log(this, "");
}
unsafe extern "C" fn op_void_str(this: *const RccCallable, _s: *const StdString) {
    rcc_log(this, "");
}
unsafe extern "C" fn op_void(this: *const RccCallable) {
    rcc_log(this, "");
}
unsafe extern "C" fn op_cookie_list(
    this: *const RccCallable,
    _s: *const StdString,
    _l: *const c_long,
    _v: *mut StdVector,
) {
    rcc_log(this, "");
}
unsafe extern "C" fn op_get_value(this: *const RccCallable, key: *const StdString) -> StdString {
    // SAFETY: Apple passes a valid string reference.
    let key = unsafe { rcc_bytes(key) };
    let value = {
        let kv = RCC_KV.lock().unwrap_or_else(|e| e.into_inner());
        StdString::guest_owned(
            kv.iter()
                .find(|(k, _)| *k == key)
                .map_or(&[][..], |(_, v)| v),
        )
    };
    rcc_log(this, &String::from_utf8_lossy(&key));
    value
}
unsafe extern "C" fn op_set_value(
    this: *const RccCallable,
    key: *const StdString,
    value: *const StdString,
) {
    // SAFETY: Apple passes valid string references.
    let (key, value) = unsafe { (rcc_bytes(key), rcc_bytes(value)) };
    let stored = {
        let mut kv = RCC_KV.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = kv.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
            true
        } else if kv.len() < RCC_KV_MAX {
            kv.push((key.clone(), value));
            true
        } else {
            false
        }
    };
    rcc_log(
        this,
        &if stored {
            "(stored)".into()
        } else {
            String::from_utf8_lossy(&key).into_owned()
        },
    );
}
unsafe extern "C" fn op_bool(this: *const RccCallable) -> bool {
    rcc_log(this, "");
    false
}
unsafe extern "C" fn op_account_value(
    this: *const RccCallable,
    _dsid: *const c_long,
    _key: *const StdString,
) -> StdString {
    rcc_log(this, "");
    StdString::guest_owned(b"")
}
unsafe extern "C" fn op_all_cookies(this: *const RccCallable, _dsid: *const u64) -> StdVector {
    rcc_log(this, "");
    StdVector::EMPTY
}
unsafe extern "C" fn op_status(
    this: *const RccCallable,
    _a: *const c_long,
    _b: *const c_int,
    _c: *const c_int,
    _d: *const c_int,
) {
    rcc_log(this, "");
}
unsafe extern "C" fn op_xtoken(
    this: *const RccCallable,
    _dsid: *const c_long,
    _token: *const StdString,
) {
    rcc_log(this, "(token received)");
}
unsafe extern "C" fn op_dsid(this: *const RccCallable) -> c_long {
    rcc_log(this, "");
    0
}
unsafe extern "C" fn op_play_integrity(
    this: *const RccCallable,
    _s: *const StdString,
) -> StdString {
    rcc_log(this, "(empty token)");
    StdString::guest_owned(b"")
}
unsafe extern "C" fn op_proto_action(
    this: *const RccCallable,
    _s: *const StdString,
    _b: *const c_int,
) {
    rcc_log(this, "");
}

const _: () = assert!(size_of::<RccFunction>() == 48 && align_of::<RccFunction>() == 16);
const _: () = assert!(size_of::<LeaseFunction>() == 48 && align_of::<LeaseFunction>() == 16);

unsafe extern "C" fn rcc_destroy(_this: *mut RccCallable) {}

unsafe extern "C" fn rcc_destroy_deallocate(this: *mut RccCallable) {
    // SAFETY: callables are malloc'd by `install` or `rcc_clone`.
    unsafe { libc::free(this.cast()) };
}

unsafe extern "C" fn rcc_clone(this: *const RccCallable) -> *mut RccCallable {
    // SAFETY: malloc has no preconditions; the copy is written only when it succeeded.
    unsafe {
        let copy = libc::malloc(size_of::<RccCallable>()).cast::<RccCallable>();
        if !copy.is_null() {
            ptr::copy_nonoverlapping(this, copy, 1);
        }
        copy
    }
}

unsafe extern "C" fn rcc_clone_into(this: *const RccCallable, dest: *mut RccCallable) {
    // SAFETY: `dest` is a destination buffer large enough for a callable.
    unsafe { ptr::copy_nonoverlapping(this, dest, 1) };
}

unsafe extern "C" fn rcc_target(
    _this: *const RccCallable,
    _type_info: *const c_void,
) -> *const c_void {
    ptr::null()
}

/// Installs one callback through its setter, if the libraries export it (`rcc_install`).
fn install(
    r: &Resolver,
    cfg: *mut c_void,
    setter: &str,
    name: &'static std::ffi::CStr,
    op: *const c_void,
) {
    let Some(set) = r.find(setter) else { return };
    // The vtable stays allocated: the setter's copies of the callable keep pointing at it.
    let vt: &'static Vtable = Box::leak(Box::new(Vtable([
        rcc_destroy as *const c_void,
        rcc_destroy_deallocate as *const c_void,
        rcc_clone as *const c_void,
        rcc_clone_into as *const c_void,
        rcc_destroy as *const c_void,
        rcc_destroy_deallocate as *const c_void,
        op,
        rcc_target as *const c_void,
        rcc_target as *const c_void,
    ])));
    // SAFETY: `set` is `RequestContextConfig::setXCallback(std::function<...>)`: the function
    // is passed by value, i.e. as a pointer to our temporary, which the setter copies through
    // clone(). Afterwards the temporary's callable is ours to release.
    unsafe {
        let callable = libc::malloc(size_of::<RccCallable>()).cast::<RccCallable>();
        if callable.is_null() {
            return;
        }
        callable.write(RccCallable {
            vptr: vt.0.as_ptr(),
            name: name.as_ptr(),
        });
        let mut function = RccFunction {
            buf: [0; 4],
            f: callable,
            _padding: 0,
        };
        let set = std::mem::transmute::<
            *mut c_void,
            unsafe extern "C" fn(*mut c_void, *mut RccFunction),
        >(set);
        set(cfg, &mut function);
        rcc_destroy_deallocate(callable);
    }
}

/// Builds that expect host callbacks (4.9.x/5.x) get them here; the older libraries export
/// only `setBagInvalidateCallback`, work without any, and get none.
pub(crate) fn install_request_context_callbacks(r: &Resolver, cfg: *mut c_void) {
    const GET_COOKIE: &str = "_ZN17storeservicescore20RequestContextConfig20setGetCookieCallbackENSt6__ndk18functionIFNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEERS8_lEEE";
    if r.find(GET_COOKIE).is_none() {
        return;
    }
    let all: [(&str, &'static std::ffi::CStr, *const c_void); 17] = [
        (
            GET_COOKIE,
            c"GetCookieCallback",
            op_get_cookie as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig20setSetCookieCallbackENSt6__ndk18functionIFvRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEElEEE",
            c"SetCookieCallback",
            op_void_str_long as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig21setAddProfileCallbackENSt6__ndk18functionIFvRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEEEEE",
            c"AddProfileCallback",
            op_void_str as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig24setBagInvalidateCallbackENSt6__ndk18functionIFvvEEE",
            c"BagInvalidateCallback",
            op_void as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig24setGetCookieListCallbackENSt6__ndk18functionIFvRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEElRNS1_6vectorIS8_NS6_IS8_EEEEEEE",
            c"GetCookieListCallback",
            op_cookie_list as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig25setGetValueForKeyCallbackENSt6__ndk18functionIFNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEERS8_EEE",
            c"GetValueForKeyCallback",
            op_get_value as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig25setSetValueForKeyCallbackENSt6__ndk18functionIFvRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEES9_EEE",
            c"SetValueForKeyCallback",
            op_set_value as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig25setUseLegacyStoreCallbackENSt6__ndk18functionIFbvEEE",
            c"UseLegacyStoreCallback",
            op_bool as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig26setGetAccountValueCallbackENSt6__ndk18functionIFNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEElRKS8_EEE",
            c"GetAccountValueCallback",
            op_account_value as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig28setAllCookiesForUserCallbackENSt6__ndk18functionIFNS1_6vectorINS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEENS7_IS9_EEEEmEEE",
            c"AllCookiesForUserCallback",
            op_all_cookies as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig29setUpdateAccountFlagsCallbackENSt6__ndk18functionIFvRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEEEEE",
            c"UpdateAccountFlagsCallback",
            op_void_str as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig30setUpdateAccountStatusCallbackENSt6__ndk18functionIFvlbbbEEE",
            c"UpdateAccountStatusCallback",
            op_status as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig30setUpdateAccountXTokenCallbackENSt6__ndk18functionIFvlRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEEEEE",
            c"UpdateAccountXTokenCallback",
            op_xtoken as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig31setGetActiveAccountDSIDCallbackENSt6__ndk18functionIFlvEEE",
            c"GetActiveAccountDSIDCallback",
            op_dsid as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig31setGetPlayIntegrityHashCallbackENSt6__ndk18functionIFNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEERS8_EEE",
            c"GetPlayIntegrityHashCallback",
            op_play_integrity as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig34setUpdateAccountStoreFrontCallbackENSt6__ndk18functionIFvRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEEEEE",
            c"UpdateAccountStoreFrontCallback",
            op_void_str as *const c_void,
        ),
        (
            "_ZN17storeservicescore20RequestContextConfig35setCreateAccountProtoActionCallbackENSt6__ndk18functionIFvRNS1_12basic_stringIcNS1_11char_traitsIcEENS1_9allocatorIcEEEEbEEE",
            c"CreateAccountProtoActionCallback",
            op_proto_action as *const c_void,
        ),
    ];
    for (setter, name, op) in all {
        install(r, cfg, setter, name, op);
    }
}
