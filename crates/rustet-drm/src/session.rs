//! The persisted session: which files make an Apple session, and reading its tokens.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use crate::logout_files::FileHooks;

/// The web player's account, as it handed it to the engine.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct WebSession {
    pub music_user_token: String,
    /// Empty when the web player did not say.
    pub storefront: String,
}

impl std::fmt::Debug for WebSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSession")
            .field("storefront", &self.storefront)
            .finish_non_exhaustive()
    }
}

/// Returns the web player's current account. Must be cheap and must not call back into the
/// [`SessionManager`].
pub type WebSessionSource = Arc<dyn Fn() -> WebSession + Send + Sync>;

/// A failure removing or recording session files.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("sync {}: {source}", dir.display())]
    Sync { dir: PathBuf, source: io::Error },
    #[error("sign-out requests keep arriving")]
    RequestsKeepArriving,
    #[error("the sign-out cleanup was aborted")]
    Aborted,
}

/// The engine's authority on the persisted DRM session. It owns, below `base_dir`:
///
/// - `mpl_db/` (the Android library's account databases), `STOREFRONT_ID`, `MUSIC_TOKEN`,
///   `drm-state`, `session-ready`, `2fa.txt` and FairPlay's per-account key files;
/// - the device identity (`device-id`, `adi.pb`, `IC-Info.sids`, `fsi.pdat`);
/// - the sign-out requests (`logout-pending`, and `logout-pending.backstop-<nonce>` files the
///   Electron main process writes when it cannot reach the engine);
/// - the sign-in limiter (`signin-pause`).
///
/// Nothing else reads or deletes these files, except the native backend, which writes them.
/// All methods do blocking file I/O, but only small amounts, except a sign-out pass.
pub struct SessionManager {
    base_dir: PathBuf,
    web_session: Mutex<Option<WebSessionSource>>,
    /// Sign-out files were removed but the directory sync failed: until a sync succeeds the
    /// removals could come back after a crash, so the sign-out still counts as pending.
    pub(crate) unsynced: AtomicBool,
    pub(crate) hooks: Mutex<Arc<FileHooks>>,
}

impl SessionManager {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            web_session: Mutex::default(),
            unsynced: AtomicBool::new(false),
            hooks: Mutex::new(Arc::new(FileHooks::default())),
        }
    }

    /// The session directory.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Registers where the web player's token comes from. It stands in for the token files, so
    /// a profile that only signed in on the web still has a token to send.
    pub fn set_web_session(&self, source: WebSessionSource) {
        *self
            .web_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(source);
    }

    fn web(&self) -> WebSession {
        let source = self
            .web_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        source.map(|f| f()).unwrap_or_default()
    }

    /// Whether an Apple account is signed in: the `session-ready` marker the backend writes
    /// after a successful FairPlay sign-in exists, and no sign-out is pending.
    ///
    /// A web-player token alone is not a session: lossless needs the in-app sign-in, and
    /// counting the token made a signed-out engine report "signed in" again as soon as the page
    /// sent it. A pending sign-out means no session: it is about to be removed. This does not
    /// say the session still works.
    pub fn has_session(&self) -> bool {
        fs::metadata(self.base_dir.join("session-ready")).is_ok() && !self.has_pending()
    }

    /// `STOREFRONT_ID`, or the web player's storefront when the file is missing or empty.
    pub fn read_storefront_id(&self) -> String {
        read_trimmed(&self.base_dir.join("STOREFRONT_ID")).unwrap_or_else(|| self.web().storefront)
    }

    /// `MUSIC_TOKEN`, or the web player's token when the file is missing or empty.
    ///
    /// Read fresh on every call: the backend may refresh the token while the engine runs.
    pub fn read_music_token(&self) -> String {
        read_trimmed(&self.base_dir.join("MUSIC_TOKEN"))
            .unwrap_or_else(|| self.web().music_user_token)
    }
}

/// Strips the platform and content-class suffix the native library appends to `STOREFRONT_ID`
/// (`"143467-2,31"` → `"143467"`): the catalog API wants the bare ID. A value without `-` is
/// returned unchanged (already bare, or an ISO code such as `"us"`).
pub fn normalize_storefront_id(sf: &str) -> &str {
    sf.split_once('-').map_or(sf, |(id, _)| id)
}

/// The file's content without surrounding whitespace (these values go into HTTP headers, where
/// a trailing newline is invalid). `None` when it is missing or blank.
fn read_trimmed(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// `mkdir -p` with mode 0700 on Unix: the session directory holds the Apple account.
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// Opens `path` for writing, created with mode 0600 on Unix.
pub(crate) fn open_private(path: &Path, truncate: bool) -> io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(truncate);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)
}

/// Writes `data` to `path` (mode 0600 when created), like Go's `os.WriteFile(path, data, 0o600)`.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    open_private(path, true)?.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_storefront() {
        let cases = [
            ("143467-2,31", "143467"),
            ("143441-28,29", "143441"),
            ("143441-2,29", "143441"),
            ("143467", "143467"),
            ("us", "us"),
            ("in", "in"),
            ("gb", "gb"),
            ("", ""),
            ("-", ""),
            ("-2,31", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_storefront_id(input), want, "{input:?}");
        }
    }

    // Guards against "optimising" the token into a cached value: the backend rotates it.
    #[test]
    fn music_token_is_read_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionManager::new(dir.path());
        fs::write(dir.path().join("MUSIC_TOKEN"), "token-v1").unwrap();
        assert_eq!(s.read_music_token(), "token-v1");
        fs::write(dir.path().join("MUSIC_TOKEN"), "token-v2").unwrap();
        assert_eq!(s.read_music_token(), "token-v2");
    }

    #[test]
    fn web_token_is_read_but_is_not_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionManager::new(dir.path());
        assert!(!s.has_session() && s.read_music_token().is_empty());

        s.set_web_session(Arc::new(|| WebSession {
            music_user_token: "web-token".into(),
            storefront: "us".into(),
        }));
        assert!(
            !s.has_session(),
            "a web-player token alone must not count as a sign-in"
        );
        assert_eq!(
            (
                s.read_music_token().as_str(),
                s.read_storefront_id().as_str()
            ),
            ("web-token", "us")
        );

        // Token files, when present, win over the web token.
        fs::write(dir.path().join("MUSIC_TOKEN"), "file-token\n").unwrap();
        fs::write(dir.path().join("STOREFRONT_ID"), "143467-2,31\n").unwrap();
        assert_eq!(
            (
                s.read_music_token().as_str(),
                s.read_storefront_id().as_str()
            ),
            ("file-token", "143467-2,31")
        );
    }

    // The Android library creates 4 KB SQLite stubs even when nobody is signed in.
    #[test]
    fn login_database_alone_is_not_a_session() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("mpl_db")).unwrap();
        fs::write(dir.path().join("mpl_db/accounts.sqlitedb"), vec![0; 4096]).unwrap();
        let s = SessionManager::new(dir.path());
        assert!(!s.has_session());

        fs::write(dir.path().join("session-ready"), "1").unwrap();
        assert!(s.has_session(), "the session-ready marker makes a session");
        s.clear_session().unwrap();
        assert!(!s.has_session());
        assert!(
            !dir.path().join("session-ready").exists(),
            "clear_session must remove the marker"
        );
    }
}
