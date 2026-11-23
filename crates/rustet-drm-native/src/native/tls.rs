//! AArch64: keeps Rust's thread-locals out of the TLS slots bionic code reads.
//!
//! Apple's code is built for bionic, whose AArch64 thread pointer (`TPIDR_EL0`) points at
//! eight reserved slots. Every protected function reads its stack canary from slot 5
//! (`TPIDR_EL0 + 0x28`) on entry and compares it on exit. libhybris shares the host thread
//! pointer, so in a glibc process those 64 bytes are glibc's 16-byte TCB followed by the
//! executable's own TLS block, which starts at `TP + align_up(16, PT_TLS alignment)`. With
//! Rust's usual 8- or 16-byte alignment, slot 5 is offset 0x18 of that block: a Rust
//! thread-local that changes while an Apple frame is on the stack then fires
//! `__stack_chk_fail`.
//!
//! bionic's own rule for AArch64 executables fixes it: a `PT_TLS` aligned to at least 64
//! bytes, so the block starts at `TP + 64` and the slots are padding. The thread-local below
//! forces that alignment on whatever executable links this crate, LTO and `strip` included,
//! because [`check`] (run by `Hybris::load`) takes its address. A crate built into a shared
//! library instead would leave its thread-locals in that library, not the executable, so the
//! check reads the executable's own program headers.

use std::ptr;

use super::Error;

/// Bionic's reserved slots on AArch64: `TPIDR_EL0 + 0 .. TPIDR_EL0 + 64`.
const BIONIC_SLOTS: usize = 64;

#[repr(C, align(64))]
struct Align64(u8);

thread_local! {
    /// Only its alignment matters: it raises the executable's `PT_TLS` alignment to 64.
    static ALIGN_TLS: Align64 = const { Align64(0) };
}

fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: reading TPIDR_EL0 has no side effects.
    unsafe { std::arch::asm!("mrs {}, TPIDR_EL0", out(reg) tp, options(nomem, nostack)) };
    tp
}

/// The alignment of the executable's `PT_TLS` segment, if it has one.
fn executable_tls_align() -> Option<u64> {
    // SAFETY: getauxval has no preconditions; AT_PHDR / AT_PHNUM describe the executable's
    // program headers, mapped for the life of the process.
    let phdrs = unsafe {
        let base = libc::getauxval(libc::AT_PHDR) as *const libc::Elf64_Phdr;
        let n = libc::getauxval(libc::AT_PHNUM) as usize;
        if base.is_null() {
            return None;
        }
        std::slice::from_raw_parts(base, n)
    };
    phdrs
        .iter()
        .find(|p| p.p_type == libc::PT_TLS)
        .map(|p| p.p_align)
}

/// Fails unless bionic's slots on this thread are padding: the executable's `PT_TLS` is
/// aligned to 64 or more, and this crate's thread-locals start at or after `TP + 64`.
pub(crate) fn check() -> Result<(), Error> {
    let ours = ALIGN_TLS.with(|a| ptr::from_ref(a) as usize);
    let offset = ours.wrapping_sub(thread_pointer());
    match executable_tls_align() {
        Some(align) if align >= BIONIC_SLOTS as u64 && offset >= BIONIC_SLOTS => Ok(()),
        align => Err(Error::Backend(format!(
            "thread-local storage overlaps bionic's TLS slots (executable PT_TLS alignment {align:?}, \
             crate thread-locals at TPIDR_EL0+{offset:#x}; need alignment >= 64 and offset >= 0x40): \
             Apple's stack canary at TPIDR_EL0+0x28 would not be stable"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// Slots 2-7 (`TP+0x10 .. TP+0x40`); slots 0-1 are glibc's TCB.
    fn slots() -> [usize; 6] {
        let tp = thread_pointer();
        // SAFETY: the bytes after glibc's 16-byte TCB are TLS padding, mapped for the life of
        // the thread.
        std::array::from_fn(|i| unsafe { ptr::read_volatile((tp + 16 + 8 * i) as *const usize) })
    }

    // The canary slot (TP+0x28) must not move while Rust code runs between Apple frames:
    // write Rust thread-locals of every kind on a fresh thread and compare bionic's slots.
    #[test]
    fn rust_thread_locals_leave_bionic_slots_alone() {
        thread_local! {
            static COUNTER: Cell<u64> = const { Cell::new(0) };
            // Lazily initialised, with a destructor: the other thread-local kind.
            static TEXT: RefCell<String> = RefCell::new(String::with_capacity(8));
        }
        std::thread::spawn(|| {
            check().expect("TLS layout");
            let before = slots();
            for i in 0..100 {
                COUNTER.set(COUNTER.get().wrapping_add(0x0101_0101_0101_0101));
                TEXT.with_borrow_mut(|t| t.push_str(&i.to_string()));
                let _ = std::thread::current().id();
            }
            // Unwinding updates std's thread-local panic count.
            let _ = std::panic::catch_unwind(|| std::panic::resume_unwind(Box::new(())));
            assert_eq!(slots(), before, "a Rust thread-local wrote bionic's slots");
        })
        .join()
        .unwrap();
    }
}
