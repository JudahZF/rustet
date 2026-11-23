//! Sign-out on disk: the requests, the files each scope removes, and crash-safe completion.
//!
//! A sign-out is first recorded (`logout-pending`), then the files go, then the request. A
//! request left behind (a crash, a failed delete, or a backstop the Electron main process wrote
//! because it could not reach the engine) is completed before any session starts.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use tracing::info;

use crate::deviceid::DEVICE_ID_FILE;
use crate::session::{SessionError, SessionManager, create_private_dir, open_private};

/// How much a sign-out removes. An account sign-out keeps the device identity, as Apple's own
/// Android and Windows apps do; "Forget This Device" removes it too, so Apple treats the next
/// sign-in as a new device and asks for a verification code. Ordered: a stronger scope wins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LogoutScope {
    #[default]
    None,
    Account,
    Device,
}

impl LogoutScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Account => "account",
            Self::Device => "device",
        }
    }

    /// Reads a request file. The file exists, so it asks for at least an account sign-out:
    /// anything but "device" (the old code's "1", empty, unreadable) is one, so recovery never
    /// removes device files nobody asked to remove.
    fn parse(contents: &[u8]) -> Self {
        if std::str::from_utf8(contents).is_ok_and(|s| s.trim() == "device") {
            Self::Device
        } else {
            Self::Account
        }
    }
}

const LOGOUT_MARKER_NAME: &str = "logout-pending";
const BACKSTOP_PREFIX: &str = "logout-pending.backstop-";

/// How old an uncommitted backstop `.tmp` must be before a completed cleanup removes it: a
/// younger one may belong to a write still in progress.
const STALE_TMP_AGE: Duration = Duration::from_secs(5 * 60);

/// `logout-pending.backstop-<32 lower-case hex>`: one file per main-process request, never
/// replaced (see `electron/drm-signout.mjs`).
fn is_backstop(name: &str) -> bool {
    name.strip_prefix(BACKSTOP_PREFIX)
        .is_some_and(|nonce| is_hex(nonce, 32, false))
}

/// A backstop's uncommitted write; not a request.
fn is_backstop_tmp(name: &str) -> bool {
    name.strip_suffix(".tmp").is_some_and(is_backstop)
}

pub(crate) fn is_hex(s: &str, len: usize, upper: bool) -> bool {
    s.len() == len
        && s.bytes().all(|c| {
            c.is_ascii_digit()
                || if upper {
                    (b'A'..=b'F').contains(&c)
                } else {
                    (b'a'..=b'f').contains(&c)
                }
        })
}

/// Every sign-out request on disk, merged into one scope.
#[derive(Default)]
struct PendingLogout {
    scope: LogoutScope,
    /// Exactly the files this pass consumes.
    files: Vec<PathBuf>,
}

type FileOp = Box<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
type WriteOp = Box<dyn Fn(&Path, &[u8]) -> io::Result<()> + Send + Sync>;

/// The filesystem operations sign-out depends on. Tests replace them to force failures or to
/// block at a chosen point.
pub(crate) struct FileHooks {
    pub write_marker: WriteOp,
    pub remove_all: FileOp,
    pub sync_dir: FileOp,
}

impl Default for FileHooks {
    fn default() -> Self {
        Self {
            write_marker: Box::new(write_durable),
            remove_all: Box::new(remove_all),
            sync_dir: Box::new(sync_directory),
        }
    }
}

impl SessionManager {
    fn hook(&self) -> Arc<FileHooks> {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn set_hooks(&self, hooks: FileHooks) {
        *self.hooks.lock().unwrap() = Arc::new(hooks);
    }

    /// Lists the requests on disk. It reads the directory each time, so a request another
    /// process wrote is seen by the next caller.
    fn pending(&self) -> io::Result<PendingLogout> {
        let mut p = PendingLogout::default();
        let entries = match fs::read_dir(self.base_dir()) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(p),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if entry.file_type().is_ok_and(|t| t.is_dir())
                || (name != LOGOUT_MARKER_NAME && !is_backstop(name))
            {
                continue;
            }
            let path = entry.path();
            // Unreadable: still a request, at least an account sign-out.
            let contents = fs::read(&path).unwrap_or_default();
            p.scope = p.scope.max(LogoutScope::parse(&contents));
            p.files.push(path);
        }
        Ok(p)
    }

    /// Whether any sign-out request is on disk. A read error counts as pending: no session may
    /// be used while that is unknown.
    pub(crate) fn has_pending(&self) -> bool {
        self.unsynced.load(Ordering::SeqCst) || self.pending().map_or(true, |p| !p.files.is_empty())
    }

    /// Durably records a sign-out request before anything is stopped or deleted, merged with an
    /// engine marker already there (device wins). An error means nothing was recorded.
    pub(crate) fn mark_logout_pending(&self, scope: LogoutScope) -> io::Result<()> {
        create_private_dir(self.base_dir())?;
        let path = self.base_dir().join(LOGOUT_MARKER_NAME);
        let scope = match fs::read(&path) {
            Ok(b) => scope.max(LogoutScope::parse(&b)),
            Err(_) => scope,
        };
        (self.hook().write_marker)(&path, scope.as_str().as_bytes())
    }

    /// Signs the account out on disk in one complete pass: records the request, removes the
    /// account files (and the device files when a pending request asks for that), then the
    /// request. The device identity stays. On error the request stays and the next pass
    /// finishes it. Stop the backend first, or the native library writes the files back.
    pub fn clear_session(&self) -> Result<(), SessionError> {
        self.mark_logout_pending(LogoutScope::Account)?;
        self.run_pending_pass()
    }

    /// Finishes every sign-out request left on disk with the scope it asked for. While an
    /// error is returned a request remains, and no session may be started or reused.
    pub fn complete_pending_logout(&self) -> Result<(), SessionError> {
        if !self.has_pending() {
            return Ok(());
        }
        info!("drm: completing a pending sign-out");
        self.run_pending_pass()
    }

    /// The Apple account: the login database, tokens, the session marker and FairPlay's
    /// per-account key files.
    fn account_files(&self) -> Vec<PathBuf> {
        let dir = self.base_dir();
        let mut paths: Vec<PathBuf> = [
            "mpl_db",
            "STOREFRONT_ID",
            "MUSIC_TOKEN",
            "drm-state",
            "session-ready",
            "2fa.txt",
        ]
        .into_iter()
        .map(|n| dir.join(n))
        .collect();
        paths.extend(fair_play_key_files(dir));
        paths
    }

    /// The device identity Apple recognises the machine by. Only "Forget This Device" removes it.
    fn device_files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        [DEVICE_ID_FILE, "adi.pb", "IC-Info.sids", "fsi.pdat"]
            .into_iter()
            .map(|n| self.base_dir().join(n))
    }

    /// Removes the files of a scope. Every file is tried; the first error is returned. Removal
    /// is idempotent, so a repeated pass is harmless.
    fn delete_scope(&self, scope: LogoutScope) -> io::Result<()> {
        let mut paths = self.account_files();
        if scope == LogoutScope::Device {
            paths.extend(self.device_files());
        }
        let remove = &self.hook().remove_all;
        let mut first = None;
        for p in &paths {
            if let Err(e) = remove(p)
                && e.kind() != io::ErrorKind::NotFound
            {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Removes exactly the request files a completed pass consumed, then syncs the directory:
    /// only then may a new session start, or a removed but not yet durable request could
    /// reappear after a crash and wipe it.
    fn finish_pending(&self, files: &[PathBuf]) -> Result<(), SessionError> {
        let hooks = self.hook();
        for f in files {
            match (hooks.remove_all)(f) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        self.sync_removals()
    }

    /// Makes the removals durable. Until that succeeds the sign-out counts as pending.
    fn sync_removals(&self) -> Result<(), SessionError> {
        if let Err(source) = (self.hook().sync_dir)(self.base_dir()) {
            self.unsynced.store(true, Ordering::SeqCst);
            return Err(SessionError::Sync {
                dir: self.base_dir().to_owned(),
                source,
            });
        }
        self.unsynced.store(false, Ordering::SeqCst);
        self.remove_stale_tmp();
        Ok(())
    }

    /// Drops uncommitted backstop writes old enough to be abandoned.
    fn remove_stale_tmp(&self) {
        let Ok(entries) = fs::read_dir(self.base_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let stale = entry.file_name().to_str().is_some_and(is_backstop_tmp)
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_TMP_AGE));
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Completes every request on disk: delete the merged scope, remove the consumed request
    /// files, sync. A request written while a pass runs has a new name and is picked up by the
    /// next round.
    pub(crate) fn run_pending_pass(&self) -> Result<(), SessionError> {
        for _ in 0..8 {
            let p = self.pending()?;
            if p.files.is_empty() {
                if self.unsynced.load(Ordering::SeqCst) {
                    return self.sync_removals();
                }
                return Ok(());
            }
            self.delete_scope(p.scope)?;
            self.finish_pending(&p.files)?;
        }
        Err(SessionError::RequestsKeepArriving)
    }
}

/// FairPlay's key store files in `dir`, which it names by a 40-digit upper-case hex hash
/// (e.g. `33F633BF6AC7D676168B84C3594A7941AF2BEC09`).
fn fair_play_key_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            !e.file_type().is_ok_and(|t| t.is_dir())
                && e.file_name().to_str().is_some_and(|n| is_hex(n, 40, true))
        })
        .map(|e| e.path())
        .collect()
}

/// Removes a file, or a directory with its contents. A missing path is fine.
fn remove_all(path: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Writes `data` to `path` so it survives a crash: a temp file, fsync, rename, then a
/// directory fsync. Nothing is visible at `path` until the rename.
pub(crate) fn write_durable(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let written = open_private(&tmp, true).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_directory(path.parent().unwrap_or(Path::new(".")))
}

/// Makes renames and removals in `dir` durable. Windows cannot fsync a directory, so there
/// directory-entry durability is not guaranteed (Rustet has no DRM sign-in on Windows).
pub(crate) fn sync_directory(dir: &Path) -> io::Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deviceid::device_info;
    use crate::test_support::{
        ACCOUNT_NAMES, DEVICE_NAMES, assert_gone, assert_kept, no_requests, write, write_signed_in,
    };

    const LEGACY: &str = crate::deviceid::LEGACY_ANDROID_ID;

    #[test]
    fn completing_a_pending_sign_out_honours_its_scope() {
        for (marker, keep_device) in [
            ("account", true),
            ("device", false),
            ("1", true),
            ("", true),
            ("junk", true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let dir = dir.path();
            write_signed_in(dir, LEGACY);
            write(dir, LOGOUT_MARKER_NAME, marker);
            let s = SessionManager::new(dir);
            assert!(
                !s.has_session(),
                "marker {marker:?}: a pending sign-out must hide the session"
            );
            s.complete_pending_logout().unwrap();
            assert_gone(dir, ACCOUNT_NAMES);
            assert_gone(dir, &["mpl_db"]);
            if keep_device {
                assert_kept(dir, DEVICE_NAMES);
            } else {
                assert_gone(dir, DEVICE_NAMES);
            }
            no_requests(dir);
        }
    }

    #[test]
    fn completing_leaves_a_live_session_alone() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "session-ready", "1");
        let s = SessionManager::new(dir.path());
        s.complete_pending_logout().unwrap();
        assert!(
            s.has_session(),
            "without a request the session must be left alone"
        );
    }

    #[test]
    fn a_failed_delete_keeps_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY);
        let s = SessionManager::new(dir);
        s.set_hooks(FileHooks {
            remove_all: Box::new(|p| {
                if p.ends_with("mpl_db") {
                    Err(io::Error::other("busy"))
                } else {
                    remove_all(p)
                }
            }),
            ..FileHooks::default()
        });
        assert!(
            s.clear_session().is_err(),
            "a failed delete must be reported"
        );
        assert_kept(dir, &[LOGOUT_MARKER_NAME]);
        s.set_hooks(FileHooks::default());
        s.complete_pending_logout().unwrap();
        assert_gone(dir, &["mpl_db", LOGOUT_MARKER_NAME]);
    }

    #[test]
    fn several_requests_merge_into_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY);
        let backstop = |c: &str| format!("{BACKSTOP_PREFIX}{}", c.repeat(32));
        write(dir, LOGOUT_MARKER_NAME, "account");
        write(dir, &backstop("a"), "device");
        write(dir, &backstop("b"), "account");
        let tmp = format!("{}.tmp", backstop("c"));
        write(dir, &tmp, "device"); // uncommitted: not a request
        SessionManager::new(dir).complete_pending_logout().unwrap();
        assert_gone(dir, DEVICE_NAMES);
        assert_gone(dir, &[LOGOUT_MARKER_NAME, &backstop("a"), &backstop("b")]);
        assert_kept(dir, &[&tmp]); // too young to be abandoned
    }

    #[test]
    fn write_durable_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        write_durable(&p, b"device").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"device");
        assert!(!dir.path().join("f.tmp").exists());
    }

    #[test]
    fn clear_session_leaves_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "session-ready", "1");
        write(dir.path(), "MUSIC_TOKEN", "tok");
        SessionManager::new(dir.path()).clear_session().unwrap();
        assert_gone(
            dir.path(),
            &["logout-pending", "session-ready", "MUSIC_TOKEN"],
        );
    }

    // A sign-out removes the account and keeps the device identity, as Apple's own apps do.
    #[test]
    fn clear_session_keeps_the_device_identity() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut account = vec![
            "33F633BF6AC7D676168B84C3594A7941AF2BEC09",
            "2fa.txt",
            "session-ready",
            "MUSIC_TOKEN",
        ];
        let mut keep = vec![
            "adi.pb",
            "IC-Info.sids",
            "fsi.pdat",
            "device-id",
            "engine-session.lock",
            "notes.txt",
        ];
        if cfg!(windows) {
            account.remove(0); // case-insensitive names: the upper- and lower-case hash are one file
        } else {
            keep.push("33f633bf6ac7d676168b84c3594a7941af2bec09");
        }
        for n in account.iter().chain(&keep) {
            write(dir, n, "x");
        }
        SessionManager::new(dir).clear_session().unwrap();
        assert_gone(dir, &account);
        assert_kept(dir, &keep);
    }

    #[test]
    fn interrupted_sign_out_is_finished() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write(dir, "logout-pending", "1");
        write(dir, "session-ready", "1");
        fs::create_dir_all(dir.join("mpl_db")).unwrap();
        let s = SessionManager::new(dir);
        s.complete_pending_logout().unwrap();
        assert_gone(dir, &["logout-pending", "session-ready", "mpl_db"]);
        assert!(!s.has_session());
    }

    // Sign-out keeps the device ID (a legacy install keeps the legacy ID Apple already trusts);
    // only Forget This Device gives a fresh one.
    #[test]
    fn sign_out_keeps_the_device_id_and_forget_device_renews_it() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write(dir, "session-ready", "1");
        assert!(device_info(dir).unwrap().ends_with(LEGACY));
        SessionManager::new(dir).clear_session().unwrap();
        assert!(
            device_info(dir).unwrap().ends_with(LEGACY),
            "sign-out changed the device ID"
        );
        let s = SessionManager::new(dir);
        s.mark_logout_pending(LogoutScope::Device).unwrap();
        s.complete_pending_logout().unwrap();
        assert!(
            !device_info(dir).unwrap().ends_with(LEGACY),
            "forgetting the device must give a fresh ID"
        );
    }
}
