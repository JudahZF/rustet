//! Persistent on-disk segment caches with LRU eviction.
//!
//! A segment lives at `<dir>/<key[..2]>/<key>`, where the key is the hex SHA-256 of the segment's stable URL (see
//! [`stable_cache_key`](crate::stable_cache_key)). The file is `[32-byte SHA-256 of the content][content]`: every read
//! verifies the hash, so a corrupt or half-written entry is dropped instead of served.
//!
//! Eviction runs on insertion: while the total size is above the limit, the least recently used files are deleted.
//! All methods do blocking file I/O; async callers run them in `spawn_blocking`.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use tracing::debug;

const HASH_LEN: usize = 32;

/// The LRU index of the files on disk. The highest sequence number is the most recently used.
#[derive(Debug, Default)]
struct Index {
    entries: HashMap<String, Entry>,
    order: BTreeMap<u64, String>,
    next_seq: u64,
    total: i64,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    size: i64,
    seq: u64,
}

impl Index {
    /// Inserts `key` as the most recently used entry, or moves it there.
    fn touch(&mut self, key: &str, size: Option<i64>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        match self.entries.get_mut(key) {
            Some(e) => {
                self.order.remove(&e.seq);
                e.seq = seq;
                if let Some(size) = size {
                    self.total += size - e.size;
                    e.size = size;
                }
            }
            None => {
                let Some(size) = size else { return };
                self.entries.insert(key.to_owned(), Entry { size, seq });
                self.total += size;
            }
        }
        self.order.insert(seq, key.to_owned());
    }

    fn remove(&mut self, key: &str) {
        if let Some(e) = self.entries.remove(key) {
            self.order.remove(&e.seq);
            self.total -= e.size;
        }
    }

    fn pop_oldest(&mut self) -> Option<String> {
        let (_, key) = self.order.pop_first()?;
        if let Some(e) = self.entries.remove(&key) {
            self.total -= e.size;
        }
        Some(key)
    }
}

/// A bounded on-disk cache of segment bytes.
#[derive(Debug)]
pub(crate) struct SegmentCache {
    dir: PathBuf,
    /// Where earlier versions kept this cache (directly under the OS cache root). Moved into `dir` on first use.
    ready: OnceLock<()>,
    /// 0 disables the cache.
    max_bytes: AtomicI64,
    index: Mutex<Index>,
}

impl SegmentCache {
    pub(crate) fn new(dir: PathBuf, max_bytes: i64) -> Self {
        Self {
            dir,
            ready: OnceLock::new(),
            max_bytes: AtomicI64::new(max_bytes),
            index: Mutex::new(Index::default()),
        }
    }

    /// The cache directory, created on first use rather than at startup.
    fn directory(&self) -> &Path {
        self.ready.get_or_init(|| {
            let _ = create_private_dir(&self.dir);
        });
        &self.dir
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.directory().join(&key[..2]).join(key)
    }

    fn index(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn enabled(&self) -> bool {
        self.max_bytes.load(Ordering::Relaxed) > 0
    }

    pub(crate) fn max_bytes(&self) -> i64 {
        self.max_bytes.load(Ordering::Relaxed)
    }

    /// Sets the capacity. 0 disables the cache; a negative value counts as 0 (it would half-enable it).
    pub(crate) fn set_max_bytes(&self, n: i64) {
        self.max_bytes.store(n.max(0), Ordering::Relaxed);
    }

    /// Bytes of content the index accounts for.
    pub(crate) fn total_bytes(&self) -> i64 {
        self.index().total
    }

    /// The cached bytes for `url`, verified against their stored hash. A corrupt entry is deleted and is a miss.
    pub(crate) fn get(&self, url: &str) -> Option<Vec<u8>> {
        if !self.enabled() {
            return None;
        }
        let key = cache_key(url);
        let path = self.path_for(&key);
        let mut raw = fs::read(&path).ok()?;
        if raw.len() < HASH_LEN || Sha256::digest(&raw[HASH_LEN..])[..] != raw[..HASH_LEN] {
            // Corrupt or stale entry: evict and miss.
            let _ = fs::remove_file(&path);
            self.index().remove(&key);
            return None;
        }
        let now = SystemTime::now();
        if let Ok(f) = fs::File::options().write(true).open(&path) {
            let _ = f.set_times(fs::FileTimes::new().set_accessed(now).set_modified(now));
        }
        self.index().touch(&key, None);
        raw.drain(..HASH_LEN);
        Some(raw)
    }

    /// Stores `data` for `url` with its SHA-256 prefix, then evicts down to the limit.
    pub(crate) fn put(&self, url: &str, data: &[u8]) {
        if !self.enabled() || data.is_empty() {
            return;
        }
        let key = cache_key(url);
        let path = self.path_for(&key);
        if let Some(parent) = path.parent()
            && create_private_dir(parent).is_err()
        {
            return;
        }
        let tmp = path.with_extension("tmp");
        let written = write_private(&tmp, &Sha256::digest(data), data);
        if written.and_then(|()| fs::rename(&tmp, &path)).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        self.index().touch(&key, Some(data.len() as i64));
        self.evict();
    }

    /// Removes least recently used entries until the total is within the limit.
    fn evict(&self) {
        let limit = self.max_bytes();
        if limit <= 0 {
            return;
        }
        let doomed: Vec<String> = {
            let mut index = self.index();
            std::iter::from_fn(|| (index.total > limit).then(|| index.pop_oldest()).flatten())
                .collect()
        };
        for key in doomed {
            let _ = fs::remove_file(self.path_for(&key));
        }
    }

    /// Loads the index from the files already on disk, so eviction accounts for segments cached before a restart.
    /// A key that a concurrent [`put`](Self::put) already indexed is skipped.
    pub(crate) fn warm_from_disk(&self) {
        if !self.enabled() {
            return;
        }
        let mut files = Vec::new();
        walk(self.directory(), &mut |path, meta| {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                return;
            };
            if name.len() != 64 {
                return;
            }
            // The index tracks content bytes, without the hash prefix.
            let size = (meta.len() as i64 - HASH_LEN as i64).max(0);
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((name.to_owned(), size, mtime));
        });
        // Oldest first, so the most recently used file ends up most recent in the index.
        files.sort_by_key(|&(_, _, mtime)| mtime);
        let mut index = self.index();
        for (key, size, _) in files {
            if !index.entries.contains_key(&key) {
                index.touch(&key, Some(size));
            }
        }
        debug!(dir = %self.dir.display(), entries = index.entries.len(), "segment cache warmed");
    }

    /// Deletes every cached file and resets the index.
    pub(crate) fn clear(&self) -> io::Result<()> {
        *self.index() = Index::default();
        let dir = self.directory();
        match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        create_private_dir(dir)
    }
}

/// The decrypted-MV disk cache (`<cache>/engine/mv-dec/*.enc`). Nothing writes it any more; its size is still
/// reported and clearing still removes what earlier versions left on disk.
#[derive(Debug)]
pub(crate) struct MvDecCache {
    dir: PathBuf,
    /// Bytes of `.enc` files, counted on first use.
    total: OnceLock<AtomicI64>,
    clearing: Mutex<()>,
}

impl MvDecCache {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            total: OnceLock::new(),
            clearing: Mutex::new(()),
        }
    }

    /// Creates the directory and counts what is already in it, once. It holds at most a handful of large files.
    fn total(&self) -> &AtomicI64 {
        self.total.get_or_init(|| {
            let _ = create_private_dir(&self.dir);
            let mut total = 0;
            walk(&self.dir, &mut |path, meta| {
                if path.extension().is_some_and(|e| e == "enc") {
                    total += meta.len() as i64;
                }
            });
            AtomicI64::new(total)
        })
    }

    pub(crate) fn total_bytes(&self) -> i64 {
        self.total().load(Ordering::Relaxed)
    }

    /// Deletes every cached file. The directory is renamed aside first, so a writer with an open handle keeps
    /// writing into the old directory instead of failing; the old directory is removed in the background.
    pub(crate) fn clear(&self) -> io::Result<()> {
        let _guard = self.clearing.lock().unwrap_or_else(|e| e.into_inner());
        let total = self.total();
        let mut old = self.dir.clone().into_os_string();
        old.push(".clearing");
        let old = PathBuf::from(old);
        let _ = fs::remove_dir_all(&old); // a stale .clearing directory from a crash
        match fs::rename(&self.dir, &old) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        total.store(0, Ordering::Relaxed);
        create_private_dir(&self.dir)?;
        std::thread::spawn(move || {
            let _ = fs::remove_dir_all(old);
        });
        Ok(())
    }
}

/// Hex SHA-256 of `url`.
fn cache_key(url: &str) -> String {
    Sha256::digest(url.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Calls `f` for every regular file under `dir`, recursively. Unreadable entries are skipped.
fn walk(dir: &Path, f: &mut impl FnMut(&Path, &fs::Metadata)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk(&path, f);
        } else {
            f(&path, &meta);
        }
    }
}

/// `mkdir -p` with mode 0700 on Unix.
fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// Writes `head` then `body` to a new file with mode 0600 on Unix.
fn write_private(path: &Path, head: &[u8], body: &[u8]) -> io::Result<()> {
    let mut options = fs::File::options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut f = options.open(path)?;
    f.write_all(head)?;
    f.write_all(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(dir: &Path, max: i64) -> SegmentCache {
        SegmentCache::new(dir.join("segments"), max)
    }

    #[test]
    fn round_trip_uses_the_go_file_format() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 1 << 20);
        c.put("https://cdn/a.mp4", b"segment");
        assert_eq!(c.get("https://cdn/a.mp4").as_deref(), Some(&b"segment"[..]));

        // <dir>/<key[..2]>/<key>, content prefixed by its SHA-256.
        let key = cache_key("https://cdn/a.mp4");
        assert_eq!(
            key,
            "65a8dcd28e8fbeb6f0a555331e5c58629c5e4f999963927942c02c8c69ef034d"
        );
        let raw = fs::read(tmp.path().join("segments").join(&key[..2]).join(&key)).unwrap();
        assert_eq!(&raw[..32], &Sha256::digest(b"segment")[..]);
        assert_eq!(&raw[32..], b"segment");
        assert_eq!(c.total_bytes(), 7);
    }

    #[test]
    fn corrupt_entries_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 1 << 20);
        c.put("u", b"data");
        let key = cache_key("u");
        let path = tmp.path().join("segments").join(&key[..2]).join(&key);
        let mut raw = fs::read(&path).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        fs::write(&path, raw).unwrap();
        assert_eq!(c.get("u"), None);
        assert!(!path.exists());
        assert_eq!(c.total_bytes(), 0);
    }

    #[test]
    fn evicts_least_recently_used() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 10);
        c.put("a", b"aaaa");
        c.put("b", b"bbbb");
        assert!(c.get("a").is_some()); // a is now more recent than b
        c.put("c", b"cccc");
        assert!(c.get("b").is_none(), "least recently used entry survived");
        assert!(c.get("a").is_some() && c.get("c").is_some());
        assert_eq!(c.total_bytes(), 8);
    }

    #[test]
    fn disabled_cache_neither_reads_nor_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 1 << 20);
        c.put("a", b"x");
        c.set_max_bytes(0);
        assert_eq!(c.get("a"), None);
        c.put("b", b"y");
        c.set_max_bytes(1 << 20);
        assert_eq!(c.get("b"), None);
    }

    #[test]
    fn negative_max_disables() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 1 << 20);
        c.set_max_bytes(-5);
        assert_eq!(c.max_bytes(), 0);
        assert!(!c.enabled());
        c.set_max_bytes(1 << 20);
        assert!(c.enabled());
    }

    #[test]
    fn warm_from_disk_indexes_once() {
        let tmp = tempfile::tempdir().unwrap();
        cache(tmp.path(), 1 << 20).put("a", b"12345");
        let c = cache(tmp.path(), 1 << 20);
        c.put("a", b"12345"); // indexed before the warm-up runs
        c.warm_from_disk();
        assert_eq!(
            c.total_bytes(),
            5,
            "a file indexed by put was counted twice"
        );
        c.clear().unwrap();
        assert_eq!(c.total_bytes(), 0);
        assert!(c.get("a").is_none());
    }

    #[test]
    fn mv_dec_cache_counts_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mv-dec");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.enc"), [0; 10]).unwrap();
        fs::write(dir.join("b.tmp"), [0; 99]).unwrap();
        let c = MvDecCache::new(dir.clone());
        assert_eq!(c.total_bytes(), 10);
        c.clear().unwrap();
        assert_eq!(c.total_bytes(), 0);
        assert!(dir.is_dir() && fs::read_dir(&dir).unwrap().next().is_none());
    }
}
