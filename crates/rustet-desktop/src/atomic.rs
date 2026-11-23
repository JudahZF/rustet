//! Atomic file replacement (`atomic-write.mjs`).
//!
//! A crash or a concurrent reader sees the old file or the new one, never a truncated mix. A
//! torn prefs or store file otherwise parses as corrupt and the loaders fall back to empty,
//! discarding everything saved in it.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<path>.<pid>.<n>.tmp`: unique per process and per write, so concurrent writers never share
/// a temp file.
fn tmp_name(path: &Path) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    PathBuf::from(name)
}

/// Replaces `path` with `data` (file mode 0600 on Unix): temp file, fsync, rename.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = tmp_name(path);
    let result = write_tmp(&tmp, data).and_then(|()| rename_retry(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_tmp(tmp: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(tmp)?;
    file.write_all(data)?;
    file.sync_all()
}

/// Windows refuses to rename over a file another rename or a reader has open for an instant
/// (sharing violation, access denied); the same call succeeds a few ms later.
fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    const RETRIES: u32 = 40;
    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if cfg!(windows) && attempt < RETRIES && is_busy(&e) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_busy(e: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33).
    e.kind() == io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn replaces_content_applies_mode_leaves_no_temp_files() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("store");
        write_atomic(&f, b"one").unwrap();
        write_atomic(&f, b"two").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(names(d.path()), ["store"]);
    }

    #[test]
    fn concurrent_writes_never_tear() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("store");
        std::thread::scope(|s| {
            for c in [b"a", b"b", b"c"] {
                let f = &f;
                s.spawn(move || write_atomic(f, c).unwrap());
            }
        });
        let got = fs::read(&f).unwrap();
        assert!([b"a", b"b", b"c"].iter().any(|c| got == *c));
        assert_eq!(names(d.path()), ["store"]);
    }

    #[test]
    fn failure_leaves_existing_file_and_cleans_up() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("store");
        write_atomic(&f, b"keep").unwrap();
        assert!(write_atomic(&d.path().join("no-such-dir").join("x"), b"x").is_err());
        assert_eq!(fs::read(&f).unwrap(), b"keep");
        assert_eq!(names(d.path()), ["store"]);
    }
}
