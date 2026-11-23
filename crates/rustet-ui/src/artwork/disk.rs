//! The artwork disk cache: one file per sized URL, named by its SHA-256, evicted oldest-first
//! (by modification time, refreshed on every hit) when the directory grows past its cap.
//!
//! Blocking: call it from tokio's blocking pool.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct DiskCache {
    dir: PathBuf,
    cap_bytes: u64,
    /// Bytes in the directory, as last counted plus what was written since.
    used: Mutex<u64>,
}

impl DiskCache {
    pub fn open(dir: impl Into<PathBuf>, cap_bytes: u64) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let used = entries(&dir)?.iter().map(|e| e.1).sum();
        Ok(Self {
            dir,
            cap_bytes,
            used: Mutex::new(used),
        })
    }

    fn path(&self, url: &str) -> PathBuf {
        let hash = Sha256::digest(url.as_bytes());
        let name: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(name)
    }

    /// The cached bytes for `url`, marking them recently used.
    pub fn get(&self, url: &str) -> Option<Vec<u8>> {
        let path = self.path(url);
        let bytes = fs::read(&path).ok()?;
        if let Ok(f) = fs::File::options().append(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    /// Stores `bytes` for `url` (write to a temporary name, then rename, so a reader never sees
    /// half a file), then evicts when over the cap.
    pub fn put(&self, url: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(url);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        *used += bytes.len() as u64;
        if *used > self.cap_bytes {
            *used = evict(&self.dir, self.cap_bytes * 9 / 10)?;
        }
        Ok(())
    }

    pub fn used_bytes(&self) -> u64 {
        *self.used.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn entries(dir: &Path) -> io::Result<Vec<(PathBuf, u64, SystemTime)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            out.push((
                entry.path(),
                meta.len(),
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            ));
        }
    }
    Ok(out)
}

/// Deletes the oldest files until at most `target` bytes remain. Returns the bytes left.
fn evict(dir: &Path, target: u64) -> io::Result<u64> {
    let mut files = entries(dir)?;
    files.sort_by_key(|f| f.2);
    let mut used: u64 = files.iter().map(|f| f.1).sum();
    for (path, len, _) in files {
        if used <= target {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            used -= len;
        }
    }
    Ok(used)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(dir.path(), 2500).unwrap();
        let kb = vec![0u8; 1000];
        cache.put("a", &kb).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.put("b", &kb).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // A hit makes "a" the most recent.
        assert!(cache.get("a").is_some());
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.put("c", &kb).unwrap();
        assert!(cache.get("b").is_none(), "least recently used goes first");
        assert!(cache.get("a").is_some() && cache.get("c").is_some());
        assert_eq!(cache.used_bytes(), 2000);
        // A reopened cache counts what is there.
        assert_eq!(
            DiskCache::open(dir.path(), 2500).unwrap().used_bytes(),
            2000
        );
    }
}
