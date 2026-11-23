//! The only `unsafe` of the crate: mapping the ring file and viewing it as atomic words.
//!
//! Both processes touch the shared bytes only through `AtomicU64` loads and stores, never through
//! `&[u8]`, so a reader that copies while the writer overwrites a slot is a race on atomics (which
//! the seqlock then detects), not undefined behaviour. Readers map the file read-only and only
//! load with `Relaxed` ordering plus fences: `std::sync::atomic` documents such loads as sound on
//! read-only memory.

use std::fs::File;
use std::io;
use std::sync::atomic::AtomicU64;

use memmap2::{Mmap, MmapMut};

/// A writable mapping of a whole ring file.
pub(crate) struct WriteMap(MmapMut);

/// A read-only mapping of a whole ring file.
pub(crate) struct ReadMap(Mmap);

/// The mapping that `owner` holds, as words.
///
/// # Safety
/// `ptr..ptr + len` must be a live mapping owned by `owner` (it lives as long as `owner`), page
/// aligned, with `len` a multiple of 8.
unsafe fn words<T>(ptr: *const u8, len: usize, _owner: &T) -> &[AtomicU64] {
    debug_assert!(
        len.is_multiple_of(8) && (ptr as usize).is_multiple_of(std::mem::align_of::<AtomicU64>())
    );
    // SAFETY: guaranteed by the caller: valid, aligned and alive as long as the borrow of
    // `owner`. `AtomicU64` has the size and alignment of `u64` and allows shared mutation, and
    // every access to these bytes in this crate and in the other process is atomic.
    unsafe { std::slice::from_raw_parts(ptr.cast::<AtomicU64>(), len / 8) }
}

impl WriteMap {
    /// Maps `file`, which the caller created at its final length (a multiple of 8).
    pub(crate) fn new(file: &File) -> io::Result<Self> {
        // SAFETY: the file is the writer's own, freshly created and not yet visible to readers
        // under its final name; other processes only ever access it through atomic words (see
        // the module comment), and its length does not change while mapped.
        let map = unsafe { MmapMut::map_mut(file)? };
        if map.len() % 8 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ring length is not a multiple of 8",
            ));
        }
        Ok(Self(map))
    }

    pub(crate) fn words(&self) -> &[AtomicU64] {
        // SAFETY: `self.0` is the live, page-aligned mapping, its length checked in `new`.
        unsafe { words(self.0.as_ptr(), self.0.len(), self) }
    }
}

impl ReadMap {
    /// Maps `file` read-only. Its length must be a multiple of 8.
    pub(crate) fn new(file: &File) -> io::Result<Self> {
        // SAFETY: the writer never truncates a ring file (a size change makes a new file), so the
        // mapping stays backed for its whole life; concurrent writes by the writer are atomic word
        // stores, and this side only loads atomically (see the module comment).
        let map = unsafe { Mmap::map(file)? };
        if map.len() % 8 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ring length is not a multiple of 8",
            ));
        }
        Ok(Self(map))
    }

    pub(crate) fn words(&self) -> &[AtomicU64] {
        // SAFETY: `self.0` is the live, page-aligned mapping, its length checked in `new`.
        unsafe { words(self.0.as_ptr(), self.0.len(), self) }
    }
}
