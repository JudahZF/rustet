//! Memory layouts of the NDK libc++ types Apple's API takes and returns (`native/import.h`,
//! `native/hybris_types.h`), and the guest-heap helpers.
//!
//! Return values: a C++ function that returns a `shared_ptr`, `std::string` or `std::vector`
//! by value builds it at a hidden address the caller passes (x86-64: the first integer
//! argument, AArch64: `x8`). The C stubs spelled that address as an explicit first parameter,
//! which only matches x86-64 (and needed `guest_abi.h` on AArch64). Here those functions are
//! declared with their real by-value return type, a `#[repr(C)]` struct larger than 16 bytes,
//! for which the C ABI on both architectures passes exactly that hidden address.

use std::alloc::{Layout, handle_alloc_error};
use std::ffi::{CStr, c_void};
use std::mem::MaybeUninit;
use std::ptr;

/// `std::__ndk1::shared_ptr<T>`: the object and its control block.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SharedPtr {
    pub obj: *mut c_void,
    pub ctrl: *mut c_void,
}

// SAFETY: a SharedPtr is two addresses of objects owned by Apple's libraries, which are
// thread-safe for the calls the session makes; moving the addresses between threads is sound.
unsafe impl Send for SharedPtr {}
// SAFETY: as above; the struct itself is never mutated through a shared reference.
unsafe impl Sync for SharedPtr {}

impl SharedPtr {
    pub(crate) const NULL: Self = Self {
        obj: ptr::null_mut(),
        ctrl: ptr::null_mut(),
    };
}

/// A `shared_ptr` returned by value. The padding makes it a memory-class aggregate, so the
/// hidden result address is passed as libc++ expects for this non-trivially-copyable type
/// (the callee writes only the first 16 bytes).
#[repr(C, align(16))]
pub(crate) struct SharedPtrRet {
    pub ptr: SharedPtr,
    _memory: MaybeUninit<[u8; 16]>,
}

/// A returned object the wrapper never reads (`DeviceGUID::configure`, `RequestContext::init`;
/// the C passed a static 88-byte buffer). 16-byte aligned like that static array, in case the
/// callee stores with aligned vector moves.
#[repr(C, align(16))]
pub(crate) struct Ignored88(MaybeUninit<[u8; 88]>);

/// `std::__ndk1::basic_string<char>`, 24 bytes. Short form: byte 0 is `len << 1`, the
/// characters follow inline. Long form: word 0 is `capacity | 1`, then size and data pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct StdString {
    words: [usize; 3],
}

impl StdString {
    /// Long form pointing at `s` with capacity 0 (word 0 = 1), as `new_std_string()` built
    /// strings on Linux. Only for `const&` parameters, which Apple's code reads or copies
    /// during the call; `s` must outlive it. A string passed by value or by non-const
    /// reference may be moved from and later freed by Apple's code: use
    /// [`guest_owned`](Self::guest_owned) there.
    pub(crate) fn borrow(s: &CStr) -> Self {
        Self {
            words: [1, s.to_bytes().len(), s.as_ptr() as usize],
        }
    }

    /// Short (inline) form; `None` when `s` is longer than 22 bytes.
    fn short(s: &[u8]) -> Option<Self> {
        if s.len() > 22 {
            return None;
        }
        let mut bytes = [0u8; 24];
        bytes[0] = (s.len() << 1) as u8;
        bytes[1..=s.len()].copy_from_slice(s);
        Some(Self::from_bytes(bytes))
    }

    /// A well-formed string whose heap buffer (if any) Apple's code owns and frees
    /// (`rcc_ret_string`): short form up to 22 bytes, else a `malloc`ed long form.
    pub(crate) fn guest_owned(s: &[u8]) -> Self {
        if let Some(short) = Self::short(s) {
            return short;
        }
        let n = s.len();
        let cap = ((n + 1) | 1) + 16;
        let data = guest_alloc(cap);
        // SAFETY: `data` has `cap > n + 1` writable bytes; `s` is `n` readable bytes.
        unsafe {
            ptr::copy_nonoverlapping(s.as_ptr(), data, n);
            *data.add(n) = 0;
        }
        Self {
            words: [cap | 1, n, data as usize],
        }
    }

    /// Runs the string's destructor (frees a long form's buffer), for a by-value argument
    /// whose copy the caller owns after the call. A string the callee moved from is short and
    /// empty again, so nothing is freed twice.
    ///
    /// # Safety
    /// `self` must be a [`guest_owned`](Self::guest_owned) string or one Apple's code left in
    /// it, and must not be used afterwards.
    pub(crate) unsafe fn destroy_guest(&mut self) {
        if self.words[0] & 1 == 1 && self.words[2] != 0 {
            // SAFETY: per the caller, a long form's buffer came from the guest heap (host malloc).
            unsafe { libc::free(self.words[2] as *mut c_void) };
        }
        *self = Self { words: [0; 3] };
    }

    fn from_bytes(bytes: [u8; 24]) -> Self {
        let word =
            |i: usize| usize::from_ne_bytes(bytes[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
        Self {
            words: [word(0), word(1), word(2)],
        }
    }

    /// The characters (`std_string_data`, bounded by the size instead of a NUL).
    ///
    /// # Safety
    /// `self` must be a valid libc++ string; a long string's buffer must stay alive and
    /// unchanged while the slice is used.
    pub(crate) unsafe fn as_bytes(&self) -> &[u8] {
        let mark = self.words[0] as u8;
        if mark & 1 == 0 {
            // SAFETY: short form: `mark >> 1 <= 22` characters start at byte 1 of `self`.
            unsafe {
                std::slice::from_raw_parts(
                    ptr::from_ref(self).cast::<u8>().add(1),
                    usize::from(mark >> 1),
                )
            }
        } else if self.words[2] == 0 {
            &[]
        } else {
            // SAFETY: long form: the caller guarantees the data pointer and size are valid.
            unsafe { std::slice::from_raw_parts(self.words[2] as *const u8, self.words[1]) }
        }
    }
}

/// The text of a string Apple returned by reference; empty for a null reference.
///
/// # Safety
/// `s` must be null or point at a valid libc++ string.
pub(crate) unsafe fn string_lossy(s: *const StdString) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: non-null and valid per the caller.
    String::from_utf8_lossy(unsafe { (*s).as_bytes() }).into_owned()
}

/// `std::__ndk1::vector<T>`: begin, end, end of capacity. 24 bytes, so a by-value return uses
/// the hidden result address on both architectures.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct StdVector {
    pub begin: *mut c_void,
    pub end: *mut c_void,
    pub end_capacity: *mut c_void,
}

impl StdVector {
    pub(crate) const EMPTY: Self = Self {
        begin: ptr::null_mut(),
        end: ptr::null_mut(),
        end_capacity: ptr::null_mut(),
    };

    /// The elements, for a vector of `T`.
    ///
    /// # Safety
    /// The vector must hold `T`s and stay unchanged while the slice is used.
    pub(crate) unsafe fn as_slice<T>(&self) -> &[T] {
        if self.begin.is_null() || self.end <= self.begin {
            return &[];
        }
        let len = (self.end as usize - self.begin as usize) / size_of::<T>();
        // SAFETY: per the caller, [begin, end) holds `len` initialised `T`s.
        unsafe { std::slice::from_raw_parts(self.begin.cast::<T>(), len) }
    }
}

/// What `StoreErrorCondition::errorCode()` returns a reference to.
#[repr(C)]
pub(crate) struct NativeErrorCode {
    pub value: i32,
    _padding: i32,
    _category: *const c_void,
}

/// Zeroed memory from the host `calloc`, for objects Apple's code later releases (control
/// blocks, strings). Apple's libraries free through the same allocator, as with the C build.
pub(crate) fn guest_alloc(size: usize) -> *mut u8 {
    // SAFETY: calloc has no preconditions; the result is checked below.
    let p = unsafe { libc::calloc(1, size) }.cast::<u8>();
    if p.is_null() {
        handle_alloc_error(Layout::from_size_align(size, 16).expect("valid layout"));
    }
    p
}

/// A `make_shared<T>` the newer libraries inline instead of exporting (`make_shared_manual`):
/// `[control block: vptr, shared_owners, weak_owners, pad][object at +0x20]`. `vtable` is the
/// `__shared_ptr_emplace<T>` vtable symbol; `esft_offset` is the byte offset of the object's
/// `enable_shared_from_this` weak pointer, if it has one.
///
/// # Safety
/// `vtable` must be that vtable, `total_size` the emplace size, and `construct` must run T's
/// constructor on the object address.
pub(crate) unsafe fn make_shared_manual(
    vtable: *mut c_void,
    total_size: usize,
    esft_offset: Option<usize>,
    construct: impl FnOnce(*mut c_void),
) -> SharedPtr {
    let cb = guest_alloc(total_size).cast::<usize>();
    // SAFETY: `cb` has `total_size >= 0x20 + size_of::<T>()` zeroed bytes; the vptr points two
    // slots into the vtable (past offset-to-top and RTTI), as a C++ object's does.
    unsafe {
        *cb = vtable as usize + 0x10;
        let obj = cb.cast::<u8>().add(0x20).cast::<c_void>();
        construct(obj);
        if let Some(off) = esft_offset {
            let weak_this = obj.cast::<u8>().add(off).cast::<usize>();
            *weak_this = obj as usize;
            *weak_this.add(1) = cb as usize;
            *cb.add(2) += 1; // the weak reference held by weak_this
        }
        SharedPtr {
            obj,
            ctrl: cb.cast(),
        }
    }
}

/// A control block plus object for a `shared_ptr` the wrapper builds itself (the dialog and
/// credential responses, the request-context config, the HTTP messages): `size` zeroed bytes,
/// vptr from `vtable` (the `__shared_ptr_emplace<T>` vtable symbol), object at `obj_offset`.
/// The caller then runs T's constructor on `obj`. Never freed here: Apple's library keeps
/// shared references.
pub(crate) fn emplace_block(vtable: *mut c_void, size: usize, obj_offset: usize) -> SharedPtr {
    let cb = guest_alloc(size);
    // SAFETY: `cb` has `size > obj_offset` zeroed bytes, aligned for a pointer.
    unsafe {
        *cb.cast::<usize>() = vtable as usize + 0x10;
        SharedPtr {
            obj: cb.add(obj_offset).cast(),
            ctrl: cb.cast(),
        }
    }
}
