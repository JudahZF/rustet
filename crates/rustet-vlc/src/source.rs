//! What libvlc reads from: the [`Source`] trait and the bridge from the engine's async readers.

use std::io::{self, Read, Seek, SeekFrom};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

/// Feeds libvlc directly from engine memory or disk, instead of a loopback HTTP fetch.
///
/// libvlc calls `read` and `seek` from its own input thread, and `read` may block until data is
/// available. `Ok(0)` from `read` means a clean end of the data; an error means the read failed or
/// was aborted. Dropping the source closes it.
pub trait Source: Read + Seek + Send + 'static {
    /// Total size in bytes, `None` while not yet known.
    fn size(&self) -> Option<u64>;

    /// A handle that makes a blocked `read` return from another thread, so
    /// `libvlc_media_player_stop` can join the input thread.
    fn abort_handle(&self) -> AbortHandle;
}

/// Unblocks a pending [`Source`] read. Taken once per source, before libvlc gets it.
pub struct AbortHandle(Option<Box<dyn FnOnce() + Send>>);

impl AbortHandle {
    /// For sources whose reads never block, such as a committed cache file.
    pub fn none() -> Self {
        Self(None)
    }

    /// Runs `abort` when the player stops or replaces the source.
    pub fn new(abort: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(abort)))
    }

    pub(crate) fn abort(self) {
        if let Some(abort) = self.0 {
            abort();
        }
    }
}

impl From<CancellationToken> for AbortHandle {
    fn from(token: CancellationToken) -> Self {
        Self::new(move || token.cancel())
    }
}

/// Bridges an async reader (for example a reader on an in-progress download) to [`Source`].
///
/// Each read and seek runs the async operation to completion with [`Handle::block_on`] on the
/// calling thread. libvlc's input thread is not a runtime thread, so this is allowed and keeps
/// runtime workers free. Do not call `read` or `seek` from inside the runtime: it panics.
/// The abort handle cancels a pending operation and every later one. They fail with an error
/// (not `Interrupted`, which the read callback retries), so an abort never reads as a clean end.
pub struct AsyncSource<R> {
    reader: R,
    size: Option<u64>,
    runtime: Handle,
    abort: CancellationToken,
}

impl<R> AsyncSource<R>
where
    R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
{
    /// Wraps `reader`; `runtime` drives its futures, `size` is the total length if known.
    pub fn new(reader: R, size: Option<u64>, runtime: Handle) -> Self {
        Self {
            reader,
            size,
            runtime,
            abort: CancellationToken::new(),
        }
    }

    fn run<T>(&mut self, op: impl AsyncFnOnce(&mut R) -> io::Result<T>) -> io::Result<T> {
        let Self {
            reader,
            runtime,
            abort,
            ..
        } = self;
        runtime.block_on(async {
            tokio::select! {
                biased;
                () = abort.cancelled() => Err(io::Error::other("source aborted")),
                result = op(reader) => result,
            }
        })
    }
}

impl<R> Read for AsyncSource<R>
where
    R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.run(async |reader| reader.read(buf).await)
    }
}

impl<R> Seek for AsyncSource<R>
where
    R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
{
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.run(async |reader| reader.seek(pos).await)
    }
}

impl<R> Source for AsyncSource<R>
where
    R: AsyncRead + AsyncSeek + Unpin + Send + 'static,
{
    fn size(&self) -> Option<u64> {
        self.size
    }

    fn abort_handle(&self) -> AbortHandle {
        self.abort.clone().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callbacks::CallbackMedia;
    use std::pin::Pin;
    use std::sync::{Arc, mpsc};
    use std::task::{Context, Poll};
    use std::time::Duration;

    /// Never yields data, like a reader waiting on a download that does not progress.
    struct Stalled;

    impl AsyncRead for Stalled {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }
    impl AsyncSeek for Stalled {
        fn start_seek(self: Pin<&mut Self>, _: SeekFrom) -> io::Result<()> {
            Ok(())
        }
        fn poll_complete(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<u64>> {
            Poll::Ready(Ok(0))
        }
    }

    // The bridge keeps the callback contract: data, then a clean end that sets eof.
    #[test]
    fn async_source_reads_to_clean_end() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let src = AsyncSource::new(
            io::Cursor::new(b"abcd".to_vec()),
            Some(4),
            rt.handle().clone(),
        );
        let cm = CallbackMedia::new(Box::new(src));
        let mut buf = [0; 8];
        assert_eq!(cm.seek_to(2), 0);
        assert_eq!(cm.read(&mut buf), 2);
        assert_eq!(&buf[..2], b"cd");
        assert_eq!(cm.read(&mut buf), 0);
        assert!(cm.eof());
    }

    // Stopping the player must unblock libvlc's input thread, and an abort is a failure, not an end.
    #[test]
    fn abort_unblocks_a_pending_async_read() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let src = AsyncSource::new(Stalled, None, rt.handle().clone());
        let abort = src.abort_handle();
        let cm = CallbackMedia::new(Box::new(src));
        let (done, result) = mpsc::channel();
        let reader = Arc::clone(&cm);
        std::thread::spawn(move || done.send(reader.read(&mut [0; 8])).unwrap());
        assert!(
            result.recv_timeout(Duration::from_millis(200)).is_err(),
            "read returned before abort"
        );
        abort.abort();
        assert_eq!(result.recv_timeout(Duration::from_secs(5)), Ok(-1));
        assert!(!cm.eof());
        assert_eq!(
            cm.read(&mut [0; 8]),
            -1,
            "reads after an abort fail at once"
        );
    }
}
