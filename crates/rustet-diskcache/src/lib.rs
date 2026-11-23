//! Disk cache for decrypted audio tracks (and music-video files), so that replaying a track
//! skips the HLS download and decryption pipeline.
//!
//! Cache layout (compatible with the Go engine):
//!
//! ```text
//! {dir}/{assetID}-{qualifier}.m4a       committed entry
//! {dir}/{assetID}-{qualifier}.m4a.tmp   write in progress (deleted on abort)
//! ```
//!
//! The qualifier is the codec name (`aac`, `alac`, `atmos`, …), optionally suffixed with `_raw`
//! when the caller requested the native container. Every character outside
//! `[A-Za-z0-9_-]` becomes `_`, so a key can never leave the cache directory and the directory
//! is `ls`-friendly.
//!
//! Eviction is size-based (LRU by mtime) and TTL-based. Both are lazy: size eviction runs after
//! each successful commit, TTL eviction on lookup and in [`Cache::evict_expired`]. Limits come
//! from [`Cache::set_config`]; zero means unlimited / no expiry.
//!
//! # Concurrency model
//!
//! The API is async. File-system work runs in `spawn_blocking`, never on a runtime worker.
//! A [`StreamingPut`] lets any number of [`StreamingReader`]s follow a file while it is
//! still being written: a reader waits when it catches up with the writer and resumes when
//! more bytes arrive. Dropping a pending read cancels it (this replaces Go's `Abort`).
//! Threads outside the runtime (libvlc callbacks) can drive a reader with
//! `tokio::runtime::Handle::block_on`.
#![forbid(unsafe_code)]

mod fs;
mod streaming;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::io::AsyncWriteExt;

pub use streaming::{StreamingEntry, StreamingPut, StreamingReader};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// Lookups refresh an entry's mtime at most this often; byte-range replays would otherwise
/// issue a `utimes` syscall on every range request.
const TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const TMP_SUFFIX: &str = ".tmp";

/// Errors from committing an entry or reading a streaming entry.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The writer was dropped without a commit (download failed or was abandoned).
    #[error("streaming cache download failed or was abandoned")]
    Abandoned,
    /// Renaming the temp file to its final name failed. Readers of the entry see the same error.
    #[error("cache commit failed: {0}")]
    CommitFailed(#[source] Arc<io::Error>),
}

impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::Io(err) => err,
            other => io::Error::other(other),
        }
    }
}

/// Size of the committed entries, from [`Cache::stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub total_bytes: u64,
    pub count: usize,
}

/// Handle to a cache directory. Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct Cache(Arc<Inner>);

struct Inner {
    dir: PathBuf,
    limit_bytes: AtomicU64,
    ttl_days: AtomicU64,
    /// Eviction is requested after every commit. Passes are coalesced so a burst of commits
    /// costs one directory scan, not one per commit running concurrently.
    evicting: AtomicBool,
    evict_again: AtomicBool,
    /// Keys with a write in progress, so concurrent requests for the same track do not both
    /// write the same temp file. `Some` for streaming puts, which readers can join.
    claims: Mutex<HashMap<String, Option<StreamingEntry>>>,
}

/// File name of an entry. Only `[A-Za-z0-9_-]` survive; every other character (one per
/// Unicode scalar, as Go's regexp does) becomes `_`.
fn filename(asset_id: &str, qualifier: &str) -> String {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("{}-{}.m4a", safe(asset_id), safe(qualifier))
}

/// Runs blocking file-system work off the runtime workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

fn is_committed_name(name: &std::ffi::OsStr) -> bool {
    !name.to_string_lossy().ends_with(TMP_SUFFIX)
}

/// Committed entries (not directories, not temp files) with their metadata.
fn committed_entries(dir: &Path) -> io::Result<impl Iterator<Item = (PathBuf, std::fs::Metadata)>> {
    Ok(std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            if !is_committed_name(&e.file_name()) {
                return None;
            }
            let meta = e.metadata().ok()?;
            (!meta.is_dir()).then(|| (e.path(), meta))
        }))
}

fn age(meta: &std::fs::Metadata) -> Duration {
    meta.modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .unwrap_or_default()
}

impl Cache {
    /// Opens a cache rooted at `dir`, creating it (mode 0700) if needed.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir(&dir)?;
        Ok(Self(Arc::new(Inner {
            dir,
            limit_bytes: AtomicU64::new(0),
            ttl_days: AtomicU64::new(0),
            evicting: AtomicBool::new(false),
            evict_again: AtomicBool::new(false),
            claims: Mutex::new(HashMap::new()),
        })))
    }

    /// Updates the limits. Zero means unlimited / no expiry.
    pub fn set_config(&self, limit_mb: u64, ttl_days: u64) {
        self.0
            .limit_bytes
            .store(limit_mb.saturating_mul(1024 * 1024), Ordering::Relaxed);
        self.0.ttl_days.store(ttl_days, Ordering::Relaxed);
    }

    /// Path of a committed entry, or `None` on a miss or TTL expiry (the entry is then
    /// deleted). A hit counts as a use for LRU: callers that open the file themselves
    /// (VLC, range serving) never go through [`Cache::get`].
    pub async fn path(&self, asset_id: &str, qualifier: &str) -> Option<PathBuf> {
        let inner = self.0.clone();
        let name = filename(asset_id, qualifier);
        blocking(move || {
            let hit = inner.lookup(&name);
            if let Some((path, meta)) = &hit {
                touch_if_stale(path, meta);
            }
            Ok(hit.map(|(path, _)| path))
        })
        .await
        .ok()
        .flatten()
    }

    /// Opens a committed entry for reading, or `None` on a miss or TTL expiry.
    pub async fn get(&self, asset_id: &str, qualifier: &str) -> Option<tokio::fs::File> {
        let inner = self.0.clone();
        let name = filename(asset_id, qualifier);
        blocking(move || {
            let Some((path, meta)) = inner.lookup(&name) else {
                return Ok(None);
            };
            let file = fs::shared_options().read(true).open(&path)?;
            touch_if_stale(&path, &meta);
            Ok(Some(tokio::fs::File::from_std(file)))
        })
        .await
        .ok()
        .flatten()
    }

    /// Starts a plain (non-streaming) write. Returns `Ok(None)` when another task is already
    /// writing this key; the caller should then skip caching.
    pub async fn begin_put(
        &self,
        asset_id: &str,
        qualifier: &str,
    ) -> io::Result<Option<PutWriter>> {
        let Some(claim) = self.claim(asset_id, qualifier) else {
            return Ok(None);
        };
        let file = tokio::fs::OpenOptions::from(
            fs::shared_options()
                .write(true)
                .create(true)
                .truncate(true)
                .to_owned(),
        )
        .open(claim.tmp_path())
        .await?;
        Ok(Some(PutWriter { file, claim }))
    }

    /// Deletes one committed entry; no-op if absent. Used for ephemeral entries that must
    /// not persist (MV faststart files when caching is disabled).
    pub async fn remove(&self, asset_id: &str, qualifier: &str) {
        let path = self.0.dir.join(filename(asset_id, qualifier));
        let _ = tokio::fs::remove_file(path).await;
    }

    /// Total size and count of committed entries.
    pub async fn stats(&self) -> CacheStats {
        let inner = self.0.clone();
        blocking(move || {
            Ok(
                committed_entries(&inner.dir)?.fold(CacheStats::default(), |s, (_, meta)| {
                    CacheStats {
                        total_bytes: s.total_bytes + meta.len(),
                        count: s.count + 1,
                    }
                }),
            )
        })
        .await
        .unwrap_or_default()
    }

    /// Deletes all committed entries (not temp files). Fails only if the directory cannot
    /// be listed.
    pub async fn clear(&self) -> io::Result<()> {
        let inner = self.0.clone();
        blocking(move || {
            for (path, _) in committed_entries(&inner.dir)? {
                let _ = std::fs::remove_file(path);
            }
            Ok(())
        })
        .await
    }

    /// Deletes every entry older than the TTL. No-op when the TTL is zero.
    pub async fn evict_expired(&self) {
        let inner = self.0.clone();
        let _ = blocking(move || {
            let ttl = inner.ttl();
            if ttl.is_zero() {
                return Ok(());
            }
            for (path, meta) in committed_entries(&inner.dir)? {
                if age(&meta) > ttl {
                    let _ = std::fs::remove_file(path);
                }
            }
            Ok(())
        })
        .await;
    }

    /// Claims the key for a write, or `None` if it is already claimed.
    fn claim(&self, asset_id: &str, qualifier: &str) -> Option<Claim> {
        let key = filename(asset_id, qualifier);
        let mut claims = self.0.claims();
        if claims.contains_key(&key) {
            return None;
        }
        claims.insert(key.clone(), None);
        Some(Claim {
            cache: self.clone(),
            key,
            stream: None,
            committed: false,
        })
    }
}

impl Inner {
    fn claims(&self) -> MutexGuard<'_, HashMap<String, Option<StreamingEntry>>> {
        self.claims.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ttl(&self) -> Duration {
        let days = self.ttl_days.load(Ordering::Relaxed);
        DAY.saturating_mul(u32::try_from(days).unwrap_or(u32::MAX))
    }

    /// Stats a committed entry and applies the TTL (an expired entry is deleted).
    fn lookup(&self, name: &str) -> Option<(PathBuf, std::fs::Metadata)> {
        let path = self.dir.join(name);
        let meta = std::fs::metadata(&path).ok()?;
        let ttl = self.ttl();
        if !ttl.is_zero() && age(&meta) > ttl {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some((path, meta))
    }

    /// Removes the oldest entries (by mtime) until the total is within the limit.
    /// Concurrent calls coalesce: while a pass runs, a new request makes it go round once
    /// more instead of starting a second scan.
    fn evict(&self) {
        if self
            .evicting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.evict_again.store(true, Ordering::Release);
            return;
        }
        loop {
            self.evict_once();
            if !self.evict_again.swap(false, Ordering::AcqRel) {
                break;
            }
        }
        self.evicting.store(false, Ordering::Release);
    }

    fn evict_once(&self) {
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        if limit == 0 {
            return;
        }
        let Ok(entries) = committed_entries(&self.dir) else {
            return;
        };
        let mut files: Vec<(PathBuf, u64, SystemTime)> = entries
            .map(|(path, meta)| {
                let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                (path, meta.len(), mtime)
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
        if total <= limit {
            return;
        }
        files.sort_by_key(|(_, _, mtime)| *mtime); // oldest first
        for (path, size, _) in files {
            if total <= limit {
                break;
            }
            let _ = std::fs::remove_file(path);
            total -= size;
        }
    }
}

/// Refreshes the mtime so LRU eviction keeps recently used entries, at most once per
/// [`TOUCH_INTERVAL`].
fn touch_if_stale(path: &Path, meta: &std::fs::Metadata) {
    if age(meta) > TOUCH_INTERVAL {
        let now = SystemTime::now();
        let _ = std::fs::set_times(
            path,
            std::fs::FileTimes::new()
                .set_accessed(now)
                .set_modified(now),
        );
    }
}

/// Ownership of a key while it is being written. Dropping it without a commit deletes the
/// temp file, releases the key and fails any streaming readers.
struct Claim {
    cache: Cache,
    key: String,
    stream: Option<StreamingEntry>,
    committed: bool,
}

impl Claim {
    fn tmp_path(&self) -> PathBuf {
        self.cache.0.dir.join(format!("{}{TMP_SUFFIX}", self.key))
    }

    /// Renames the temp file to its final name, then releases the key and schedules
    /// eviction. The rename happens BEFORE the key is released: once it is free, a new put
    /// may truncate the same temp path, which would truncate this file just before it
    /// becomes the committed entry.
    async fn commit(mut self) -> Result<(), Error> {
        let tmp = self.tmp_path();
        let final_path = self.cache.0.dir.join(&self.key);
        let result = blocking(move || fs::commit_rename(&tmp, &final_path)).await;
        let stream = self.stream.take();
        let cache = self.cache.clone();
        self.committed = result.is_ok();
        drop(self); // releases the key; on failure also deletes the temp file
        match result {
            Ok(()) => {
                if let Some(stream) = stream {
                    stream.finish(Ok(()));
                }
                // Detached so it does not hold up the response that triggered the commit.
                tokio::task::spawn_blocking(move || cache.0.evict());
                Ok(())
            }
            Err(err) => {
                let err = Arc::new(err);
                if let Some(stream) = stream {
                    stream.finish(Err(streaming::Failure::Commit(err.clone())));
                }
                Err(Error::CommitFailed(err))
            }
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(self.tmp_path());
        }
        self.cache.0.claims().remove(&self.key);
        if let Some(stream) = self.stream.take() {
            stream.finish(Err(streaming::Failure::Abandoned));
        }
    }
}

/// A plain cache write from [`Cache::begin_put`]. Write the bytes, then [`commit`](Self::commit).
/// Dropping it without a commit discards the temp file.
pub struct PutWriter {
    file: tokio::fs::File,
    claim: Claim,
}

impl PutWriter {
    /// Appends `buf` to the temp file.
    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.write_all(buf).await
    }

    /// Closes the temp file and moves it to its final name, then schedules size eviction.
    pub async fn commit(self) -> Result<(), Error> {
        let Self { mut file, claim } = self;
        // tokio completes writes in the background; flush surfaces their errors.
        file.flush().await?;
        drop(file);
        claim.commit().await
    }
}
