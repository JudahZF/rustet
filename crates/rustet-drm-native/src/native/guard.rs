//! Barrier against `abort()` inside Apple's libraries (`native/guard.cpp`).
//!
//! SVError and friends are C++ exceptions thrown by bionic's libc++. The host cannot catch
//! them (separate runtimes). When one escapes, `__cxa_throw` finds no handler and calls
//! `std::terminate`, whose default handler (`abort_message` in libc++_shared) calls
//! `abort()`. bionic's `__stack_chk_fail`, `__assert2` and `__fortify_chk_fail` end in
//! `__libc_fatal`, which calls `abort()` through libc.so's PLT as well. Every one of those
//! calls is an import that libhybris binds through [`super::hooks`], which binds `abort`
//! (and `__stack_chk_fail`) to the functions here. No signal handler is involved.
//!
//! The C armed a thread-local `sigsetjmp` and `siglongjmp`ed out of a SIGABRT handler. Rust
//! cannot do that soundly (`setjmp` returns twice; jumping over Rust frames skips their
//! destructors). Instead a guarded call runs on its own thread. When Apple's code aborts
//! there, the hook reports the call as aborted to the waiting caller and parks that thread
//! forever: nothing is unwound or jumped over, and the abandoned frames simply never run
//! again. As with the C, anything Apple's code held at that point stays held; guarded code
//! must therefore never hold one of the crate's locks while it calls into Apple's libraries.
//! An abort on any other thread is the host's `abort()`, as libhybris would have bound it.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc;

use tracing::{error, warn};

use super::session::GUEST_STACK_SIZE;

/// The guarded call aborted inside an Android library.
#[derive(Debug)]
pub(crate) struct Aborted;

thread_local! {
    /// Reports the guarded call running on this thread as aborted; `None` off guarded threads.
    static ON_ABORT: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
}

/// Runs `f` on a fresh thread behind the abort barrier and waits for it. `Err(Aborted)` when
/// Apple's code aborted; that thread is then parked for good.
pub(crate) fn run<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<Result<T, Aborted>> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(format!("fairplay-{name}"))
        .stack_size(GUEST_STACK_SIZE)
        .spawn(move || {
            let aborted = tx.clone();
            ON_ABORT.set(Some(Box::new(move || {
                let _ = aborted.send(Err(Aborted));
            })));
            let out = catch_unwind(AssertUnwindSafe(f));
            ON_ABORT.take();
            let _ = tx.send(Ok(out));
        })?;
    match rx.recv() {
        Ok(Ok(Ok(v))) => Ok(Ok(v)),
        Ok(Ok(Err(panic))) => resume_unwind(panic),
        // Both senders gone without a word: the thread died; treat it like an abort.
        Ok(Err(Aborted)) | Err(_) => Ok(Err(Aborted)),
    }
}

/// The guest's `abort`.
pub(crate) extern "C" fn guest_abort() -> ! {
    on_abort("abort()")
}

/// The guest's `__stack_chk_fail`: bionic's would log and call `abort()`.
pub(crate) extern "C" fn guest_stack_chk_fail() -> ! {
    on_abort("stack corruption detected (__stack_chk_fail)")
}

fn on_abort(what: &str) -> ! {
    let Some(report) = ON_ABORT.take() else {
        error!("[guard] {what} in an Android library with no recovery point - aborting");
        // SAFETY: abort has no preconditions.
        unsafe { libc::abort() }
    };
    warn!("[guard] {what} in an Android library; parking the aborted thread");
    report();
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, c_void};

    use super::*;

    // The barrier must turn an abort on the guarded thread into an error for the caller and
    // keep the process alive; without it every SVError would take the engine down. The abort
    // is called through the hook libhybris would bind, as Apple's code calls it.
    #[test]
    fn abort_on_the_guarded_thread_is_reported_not_fatal() {
        let hooked = |symbol: &CStr| {
            // SAFETY: valid C strings; the hook only reads them.
            let p = unsafe {
                super::super::hooks::lookup(
                    symbol.as_ptr(),
                    c"/r/system/lib64/libc++_shared.so".as_ptr(),
                )
            };
            assert!(!p.is_null(), "{symbol:?} is not hooked");
            // SAFETY: the hook returns `extern "C" fn() -> !` for both symbols.
            unsafe { std::mem::transmute::<*mut c_void, extern "C" fn() -> !>(p) }
        };
        let abort = hooked(c"abort");
        let out = run("test-abort", move || -> i32 { abort() }).unwrap();
        assert!(matches!(out, Err(Aborted)));
        let stack_chk_fail = hooked(c"__stack_chk_fail");
        assert!(matches!(
            run("test-canary", move || -> i32 { stack_chk_fail() }).unwrap(),
            Err(Aborted)
        ));
        assert_eq!(run("test-ok", || 2).unwrap().unwrap(), 2);
    }
}
