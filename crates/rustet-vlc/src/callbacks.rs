//! libvlc media callbacks (`libvlc_media_new_callbacks`) over a [`Source`].

use std::ffi::{c_int, c_void};
use std::io::{ErrorKind, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::ffi::{Api, Instance, Media};
use crate::source::Source;

/// Owns a [`Source`] for the lifetime of one libvlc media. libvlc gets `Arc::as_ptr` as its opaque
/// pointer, so the player must keep its `Arc` until libvlc has stopped using the media.
pub(crate) struct CallbackMedia {
    // Only libvlc's input thread reads and seeks; the lock is uncontended.
    src: Mutex<Box<dyn Source>>,
    /// Set when the source reported a clean end of data (all bytes delivered, no error) and
    /// cleared when libvlc seeks. A read that failed or was aborted never sets it, so it tells the
    /// real end of the data from a failure.
    eof: AtomicBool,
}

impl CallbackMedia {
    pub(crate) fn new(src: Box<dyn Source>) -> Arc<Self> {
        Arc::new(Self {
            src: Mutex::new(src),
            eof: AtomicBool::new(false),
        })
    }

    pub(crate) fn eof(&self) -> bool {
        self.eof.load(Ordering::Acquire)
    }

    /// Creates the libvlc media; null on failure.
    ///
    /// # Safety
    /// `inst` must be a live libvlc instance, and `this` must outlive libvlc's use of the media
    /// (until the player has been stopped and detached from it).
    pub(crate) unsafe fn new_media(this: &Arc<Self>, api: &Api, inst: *mut Instance) -> *mut Media {
        let opaque = Arc::as_ptr(this).cast_mut().cast::<c_void>();
        // SAFETY: the callbacks match libvlc's prototypes and only use `opaque` as a
        // `&CallbackMedia`, which the caller keeps alive.
        unsafe {
            (api.libvlc_media_new_callbacks)(
                inst,
                Some(open_cb),
                Some(read_cb),
                Some(seek_cb),
                Some(close_cb),
                opaque,
            )
        }
    }

    fn source(&self) -> std::sync::MutexGuard<'_, Box<dyn Source>> {
        self.src.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fills `buf` for libvlc: the byte count, 0 at the end of the data, -1 on a read error.
    pub(crate) fn read(&self, buf: &mut [u8]) -> isize {
        if buf.is_empty() {
            return 0;
        }
        let mut src = self.source();
        loop {
            match src.read(buf) {
                // A slice never exceeds isize::MAX bytes.
                Ok(n) if n > 0 => return n as isize,
                Ok(_) => {
                    self.eof.store(true, Ordering::Release);
                    return 0;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::debug!("[vlc] source read failed: {e}");
                    return -1;
                }
            }
        }
    }

    /// Repositions the source; 0 on success, -1 on failure. Any seek means libvlc is not at the
    /// end any more.
    pub(crate) fn seek_to(&self, offset: u64) -> c_int {
        self.eof.store(false, Ordering::Release);
        match self.source().seek(SeekFrom::Start(offset)) {
            Ok(_) => 0,
            Err(e) => {
                tracing::debug!("[vlc] source seek to {offset} failed: {e}");
                -1
            }
        }
    }
}

/// # Safety
/// `opaque` is the pointer given to `libvlc_media_new_callbacks`, still alive.
unsafe fn media<'a>(opaque: *mut c_void) -> &'a CallbackMedia {
    // SAFETY: guaranteed by the caller; the player keeps the `Arc` alive while libvlc uses it.
    unsafe { &*opaque.cast::<CallbackMedia>() }
}

unsafe extern "C" fn open_cb(
    opaque: *mut c_void,
    datap: *mut *mut c_void,
    sizep: *mut u64,
) -> c_int {
    // SAFETY: libvlc passes the opaque pointer we registered, and valid out-pointers.
    unsafe {
        *datap = opaque;
        let cm = media(opaque);
        if cm.seek_to(0) != 0 {
            return -1;
        }
        *sizep = cm.source().size().unwrap_or(u64::MAX);
    }
    0
}

unsafe extern "C" fn read_cb(opaque: *mut c_void, buf: *mut u8, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    // SAFETY: `opaque` is our data pointer (set in open_cb); libvlc hands a writable buffer of `len` bytes.
    unsafe { media(opaque).read(std::slice::from_raw_parts_mut(buf, len)) }
}

unsafe extern "C" fn seek_cb(opaque: *mut c_void, offset: u64) -> c_int {
    // SAFETY: `opaque` is our data pointer (set in open_cb).
    unsafe { media(opaque).seek_to(offset) }
}

// The player owns the source and drops it after detaching the media.
unsafe extern "C" fn close_cb(_opaque: *mut c_void) {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::source::AbortHandle;
    use std::io::{self, Cursor, Read, Seek};

    pub(crate) struct MemSource(pub(crate) Cursor<Vec<u8>>);

    impl Read for MemSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Seek for MemSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }
    impl Source for MemSource {
        fn size(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
        fn abort_handle(&self) -> AbortHandle {
            AbortHandle::none()
        }
    }

    /// Fails every read, like a download that broke or a reader that was aborted.
    struct ErrSource;

    impl Read for ErrSource {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("download failed"))
        }
    }
    impl Seek for ErrSource {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }
    impl Source for ErrSource {
        fn size(&self) -> Option<u64> {
            None
        }
        fn abort_handle(&self) -> AbortHandle {
            AbortHandle::none()
        }
    }

    // libvlc reaching its end only means the track's data ended when the source said so with a
    // clean EOF. A failed read must not look like an end, and a seek must clear it.
    #[test]
    fn tells_data_end_from_read_failure() {
        let cm = CallbackMedia::new(Box::new(MemSource(Cursor::new(b"abcd".to_vec()))));
        let mut buf = [0; 8];
        assert_eq!(cm.read(&mut buf), 4);
        assert!(!cm.eof(), "eof set before the data was exhausted");
        assert_eq!(cm.read(&mut buf), 0);
        assert!(cm.eof());
        assert_eq!(cm.seek_to(0), 0);
        assert!(!cm.eof(), "a seek must clear eof");
        assert_eq!(cm.read(&mut buf), 4);

        let bad = CallbackMedia::new(Box::new(ErrSource));
        assert_eq!(bad.read(&mut buf), -1);
        assert!(!bad.eof());
    }
}
