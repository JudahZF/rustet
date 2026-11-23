//! The engine's exclusive claim on the session directory.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::session::create_private_dir;

const LOCK_FILE_NAME: &str = "engine-session.lock";

/// An exclusive advisory lock over the session directory, held for the engine's lifetime so
/// that no two engines (and no two backends) own the single-user Apple session at once. It
/// guards against SQLite races, concurrent token refreshes and session corruption.
///
/// The lock is on a dedicated file, `engine-session.lock`, not on a database, and holds the
/// owner's PID so a second engine can name it. It is released when dropped, and by the OS if
/// the process dies.
#[derive(Debug)]
pub struct SessionLock {
    file: File,
    path: PathBuf,
}

/// Why the lock could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum SessionLockError {
    #[error("session lock: mkdir {}: {source}", dir.display())]
    CreateDir { dir: PathBuf, source: io::Error },
    #[error("session lock: open {}: {source}", path.display())]
    Open { path: PathBuf, source: io::Error },
    #[error(
        "session already owned by another engine instance{} ({}): {source}",
        owner.as_ref().map(|pid| format!(" (held by pid {pid})")).unwrap_or_default(),
        path.display()
    )]
    Held {
        owner: Option<String>,
        path: PathBuf,
        source: io::Error,
    },
}

impl SessionLock {
    /// Takes the lock without blocking. Fails at once when another process holds it.
    ///
    /// The lock is `flock(2)` on Unix and `LockFileEx` over the whole file on Windows.
    pub fn acquire(dir: &Path) -> Result<Self, SessionLockError> {
        create_private_dir(dir).map_err(|source| SessionLockError::CreateDir {
            dir: dir.to_owned(),
            source,
        })?;
        let path = dir.join(LOCK_FILE_NAME);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let file = opts.open(&path).map_err(|source| SessionLockError::Open {
            path: path.clone(),
            source,
        })?;
        if let Err(e) = file.try_lock() {
            let source = match e {
                TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
                TryLockError::Error(e) => e,
            };
            let owner = fs::read_to_string(&path)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            return Err(SessionLockError::Held {
                owner,
                path,
                source,
            });
        }
        // Record the owner, for the error a second engine reports.
        let _ = file
            .set_len(0)
            .and_then(|()| writeln!(&file, "{}", std::process::id()));
        Ok(Self { file, path })
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Closing the file releases the lock too; unlocking first makes it immediate.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let first = SessionLock::acquire(dir.path()).unwrap();
        let err = SessionLock::acquire(dir.path())
            .expect_err("a second acquire must fail while the first is held");
        let pid = std::process::id().to_string();
        assert!(
            matches!(&err, SessionLockError::Held { owner: Some(o), .. } if *o == pid),
            "{err}"
        );
        assert!(
            err.to_string().contains(&format!("(held by pid {pid})")),
            "{err}"
        );
        drop(first);
        SessionLock::acquire(dir.path()).expect("acquire after release");
    }
}
