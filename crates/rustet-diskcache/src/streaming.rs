//! Streaming puts: a temp file that readers follow while it is written.

use std::fs::File;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use tokio::sync::watch;

use crate::{Cache, Claim, Error, blocking, filename, fs};

/// Chunk size of [`StreamingReader::into_stream`]; the same as Go's `io.Copy` buffer.
const STREAM_CHUNK: usize = 32 * 1024;

/// Why a streaming entry ended without a committed file.
#[derive(Debug, Clone)]
pub(crate) enum Failure {
    Abandoned,
    Commit(Arc<io::Error>),
}

impl From<Failure> for Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Abandoned => Error::Abandoned,
            Failure::Commit(err) => Error::CommitFailed(err),
        }
    }
}

#[derive(Debug, Clone)]
enum Phase {
    Writing,
    Committed,
    Failed(Failure),
}

#[derive(Debug)]
struct Progress {
    written: u64,
    phase: Phase,
}

struct Shared {
    /// Opened read-write. All I/O is positional, so readers and the writer never share a cursor.
    /// The descriptor stays open across the commit rename until the last handle is dropped.
    file: File,
    progress: watch::Sender<Progress>,
}

/// What a reader at some position may do next.
enum Ready {
    /// Bytes `pos..written` are on disk.
    Data { written: u64 },
    /// The entry is complete and the reader is at or past its end.
    End,
}

/// Shared view of a streaming entry: an in-progress write or a committed file. Cheap to clone.
/// Get one from [`StreamingPut::entry`], [`Cache::get_streaming`] or
/// [`Cache::open_committed`].
#[derive(Clone)]
pub struct StreamingEntry(Arc<Shared>);

impl StreamingEntry {
    fn new(file: File, written: u64, phase: Phase) -> Self {
        let (progress, _) = watch::channel(Progress { written, phase });
        Self(Arc::new(Shared { file, progress }))
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.0.progress.borrow().written
    }

    /// A reader that starts at `offset`. Reads wait until the writer reaches it, so this can
    /// serve an HTTP Range request from an in-progress download.
    pub fn reader_at(&self, offset: u64) -> StreamingReader {
        StreamingReader {
            entry: self.clone(),
            pos: offset,
        }
    }

    /// A reader from byte 0, or `None` once the writer has committed or been dropped; the
    /// caller should then look up the committed file instead.
    pub fn reader_if_active(&self) -> Option<StreamingReader> {
        matches!(self.0.progress.borrow().phase, Phase::Writing).then(|| self.reader_at(0))
    }

    /// Waits until more than `from` bytes are written or the entry is complete.
    /// Fails if the writer was dropped or the commit failed before reaching `from`.
    /// Cancel it by dropping the future (for example in `tokio::select!`).
    pub async fn wait_for_growth(&self, from: u64) -> Result<(), Error> {
        self.wait_past(from).await.map(drop)
    }

    async fn wait_past(&self, pos: u64) -> Result<Ready, Error> {
        let mut rx = self.0.progress.subscribe();
        // The sender lives in `Shared`, which `self` keeps alive, so this cannot fail.
        let progress = rx
            .wait_for(|p| p.written > pos || !matches!(p.phase, Phase::Writing))
            .await
            .map_err(|_| Error::Abandoned)?;
        if progress.written > pos {
            return Ok(Ready::Data {
                written: progress.written,
            });
        }
        match &progress.phase {
            Phase::Failed(failure) => Err(failure.clone().into()),
            Phase::Committed | Phase::Writing => Ok(Ready::End),
        }
    }

    pub(crate) fn finish(&self, result: Result<(), Failure>) {
        self.0.progress.send_modify(|p| {
            p.phase = match result {
                Ok(()) => Phase::Committed,
                Err(failure) => Phase::Failed(failure),
            }
        });
    }
}

/// Owner side of a streaming write, from [`Cache::begin_streaming_put`].
///
/// ```ignore
/// let put = cache.begin_streaming_put(asset_id, "alac").await?.expect("key free");
/// let reader = put.entry().reader_at(0);
/// tokio::spawn(async move {
///     // fill with put.write_all(..); then:
///     put.commit().await
/// });
/// let body = reader.into_stream();
/// ```
///
/// Dropping it without [`commit`](Self::commit) deletes the temp file and fails readers
/// with [`Error::Abandoned`] (Go's `Discard`).
pub struct StreamingPut {
    entry: StreamingEntry,
    claim: Claim,
}

impl StreamingPut {
    /// The shared view that readers use.
    pub fn entry(&self) -> &StreamingEntry {
        &self.entry
    }

    /// Appends `buf` and wakes waiting readers. Do not cancel this future and then keep
    /// writing: the cancelled write may still land. Drop the put instead.
    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let shared = self.entry.0.clone();
        let data = buf.to_vec();
        blocking(move || {
            let mut rest = data.as_slice();
            while !rest.is_empty() {
                let offset = shared.progress.borrow().written;
                match fs::write_at(&shared.file, rest, offset) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        shared.progress.send_modify(|p| p.written += n as u64);
                        rest = &rest[n..];
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(())
        })
        .await
    }

    /// Moves the temp file to its final name and tells readers the entry is complete.
    /// Readers keep working across the rename: they hold the open descriptor.
    pub async fn commit(self) -> Result<(), Error> {
        self.claim.commit().await
    }
}

/// Reads a streaming entry. A read waits when it has consumed everything written so far and
/// resumes when the writer adds more; it ends after the writer commits and all bytes are read,
/// and fails if the writer is dropped without a commit.
pub struct StreamingReader {
    entry: StreamingEntry,
    pos: u64,
}

impl StreamingReader {
    /// Reads up to `max` bytes at the current position. `Ok(None)` is the end of a committed
    /// entry. Cancel-safe: a dropped read does not move the position.
    pub async fn read(&mut self, max: usize) -> Result<Option<Bytes>, Error> {
        let written = match self.entry.wait_past(self.pos).await? {
            Ready::Data { written } => written,
            Ready::End => return Ok(None),
        };
        let len = usize::try_from(written - self.pos).map_or(max, |avail| avail.min(max));
        let shared = self.entry.0.clone();
        let pos = self.pos;
        let buf = blocking(move || {
            let mut buf = vec![0; len];
            let n = fs::read_at(&shared.file, &mut buf, pos)?;
            if n == 0 {
                // The bytes were counted as written, so the file cannot be shorter.
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            buf.truncate(n);
            Ok(buf)
        })
        .await?;
        self.pos += buf.len() as u64;
        Ok(Some(Bytes::from(buf)))
    }

    /// Current read position.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Moves the read position. Seeking past the written length is allowed; the next read
    /// waits until the writer reaches that offset.
    pub fn seek(&mut self, offset: u64) {
        self.pos = offset;
    }

    /// The final length once the writer has committed, else `None`.
    pub fn size(&self) -> Option<u64> {
        let progress = self.entry.0.progress.borrow();
        matches!(progress.phase, Phase::Committed).then_some(progress.written)
    }

    /// The rest of the entry as a byte stream, for an HTTP response body. Wrap it in
    /// `tokio_util::io::StreamReader` for an `AsyncRead`.
    pub fn into_stream(self) -> impl Stream<Item = Result<Bytes, Error>> + Send + 'static {
        futures::stream::try_unfold(self, |mut reader| async move {
            Ok(reader
                .read(STREAM_CHUNK)
                .await?
                .map(|chunk| (chunk, reader)))
        })
    }
}

impl Cache {
    /// Starts a streaming write that readers can follow. Returns `Ok(None)` when another task
    /// is already writing this key.
    pub async fn begin_streaming_put(
        &self,
        asset_id: &str,
        qualifier: &str,
    ) -> io::Result<Option<StreamingPut>> {
        let Some(mut claim) = self.claim(asset_id, qualifier) else {
            return Ok(None);
        };
        let tmp = claim.tmp_path();
        let file = blocking(move || {
            fs::shared_options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(tmp)
        })
        .await?;
        let entry = StreamingEntry::new(file, 0, Phase::Writing);
        claim.stream = Some(entry.clone());
        self.0
            .claims()
            .insert(claim.key.clone(), Some(entry.clone()));
        Ok(Some(StreamingPut { entry, claim }))
    }

    /// The in-progress streaming write for this key, if any, so concurrent Range requests can
    /// be served from it instead of downloading from byte 0.
    pub fn get_streaming(&self, asset_id: &str, qualifier: &str) -> Option<StreamingEntry> {
        self.0
            .claims()
            .get(&filename(asset_id, qualifier))
            .cloned()
            .flatten()
    }

    /// Wraps a committed entry as a complete [`StreamingEntry`], so it can be served through
    /// the same read path as an in-progress write. The file stays open while any handle lives.
    pub async fn open_committed(
        &self,
        asset_id: &str,
        qualifier: &str,
    ) -> io::Result<StreamingEntry> {
        let path = self.0.dir.join(filename(asset_id, qualifier));
        let (file, len) = blocking(move || {
            let file = fs::shared_options().read(true).open(path)?;
            let len = file.metadata()?.len();
            Ok((file, len))
        })
        .await?;
        Ok(StreamingEntry::new(file, len, Phase::Committed))
    }
}
