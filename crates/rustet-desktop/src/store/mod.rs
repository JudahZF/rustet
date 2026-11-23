//! The encrypted store `rustet-store.enc` (`_storeLoad`, `_storeFlush*`, `store:*`).
//!
//! One JSON object holds the page's per-key data (history, play counts, resume state) and the
//! shell's secrets (saved Apple Account, Last.fm and ListenBrainz tokens). The file is the whole
//! object as JSON, encrypted with OSCrypt the way Electron's `safeStorage` did, or plain JSON
//! where the desktop has no keyring (Electron then reported encryption unavailable).
//!
//! A file that exists but cannot be read (a `v11` file without its keyring password, corrupt
//! data) is never overwritten: the store starts empty and, before its first write, the old file
//! is moved aside to `rustet-store.enc.unreadable` (`.1`, `.2`, … if that exists).

mod keyring;
#[cfg(target_os = "linux")]
mod kwallet;
pub mod oscrypt;
#[cfg(target_os = "linux")]
mod secret_service;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::task::JoinHandle;
use tracing::warn;

pub use keyring::{
    APP_NAMES, KWalletVersion, KeyringError, LinuxBackend, OsKeyring, PasswordSource,
};
use oscrypt::{DecryptError, LinuxKey, WindowsKey};

use crate::atomic::write_atomic;

/// The store file's name inside the config directory.
pub const STORE_FILE_NAME: &str = "rustet-store.enc";
/// Bytes of JSON (UTF-16 units, as JavaScript counted) a page may store per key.
pub const STORE_VALUE_MAX: usize = 256 * 1024;
/// Keys a page may create.
pub const STORE_KEYS_MAX: usize = 2000;
/// Changes are written this long after the last one, so bursts make one write.
const FLUSH_DELAY: Duration = Duration::from_millis(200);

/// How the file is encoded.
#[derive(Debug, Clone)]
pub enum StoreCipher {
    /// No keyring: plain JSON. `v10` data (the built-in key) still decrypts.
    Plain,
    /// Linux with a keyring: writes `v11`.
    Linux(LinuxKey),
    /// Windows: `v10` AES-256-GCM with the DPAPI-protected profile key.
    Windows(WindowsKey),
}

impl StoreCipher {
    /// The cipher for a password source: `v11` with the keyring password, or plain when there is
    /// no keyring or it fails (then a `v11` file stays unread and is kept, see the module docs).
    pub async fn from_keyring(source: &impl PasswordSource) -> Self {
        match source.password().await {
            Ok(Some(password)) => Self::Linux(LinuxKey::derive(password.as_bytes())),
            Ok(None) => Self::Plain,
            Err(e) => {
                warn!("keyring unavailable, the store is not encrypted: {e}");
                Self::Plain
            }
        }
    }

    fn decode(&self, raw: &[u8]) -> Result<Vec<u8>, DecryptError> {
        match self {
            Self::Plain => oscrypt::decrypt_linux(raw, None),
            Self::Linux(key) => oscrypt::decrypt_linux(raw, Some(key)),
            Self::Windows(key) => oscrypt::decrypt_windows(raw, key),
        }
    }

    fn encode(&self, json: Vec<u8>) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Plain => Ok(json),
            Self::Linux(key) => Ok(oscrypt::encrypt_linux_v11(key, &json)),
            Self::Windows(key) => {
                oscrypt::encrypt_windows(key, &json).map_err(std::io::Error::other)
            }
        }
    }
}

/// A key the page may use: `^[a-zA-Z][a-zA-Z0-9_:.-]{0,127}$`. Restricted so script running in
/// the Apple Music page cannot smuggle anything odd into the store.
fn page_key(key: &Value) -> Option<&str> {
    let key = key.as_str()?;
    let mut chars = key.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && key.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || "_:.-".contains(c));
    ok.then_some(key)
}

/// Whether a page value is small enough to store.
pub fn store_value_ok(value: &Value) -> bool {
    serde_json::to_string(value).is_ok_and(|s| s.encode_utf16().count() <= STORE_VALUE_MAX)
}

struct State {
    map: Map<String, Value>,
    dirty: bool,
    /// The file on disk could not be read; move it aside before the first write.
    keep_old_file: bool,
}

struct Inner {
    path: PathBuf,
    cipher: StoreCipher,
    /// Never held across an `.await`, so the store can be read and changed from sync code
    /// (the scrobblers' secret store in the binary).
    state: Mutex<State>,
    /// Serialises writes: two overlapping flushes must not interleave.
    write: tokio::sync::Mutex<()>,
    timer: Mutex<Option<JoinHandle<()>>>,
    /// The runtime the store was opened on. Writes run there, so the store can be changed
    /// from any thread (the UI thread is not a runtime thread).
    runtime: tokio::runtime::Handle,
}

/// The store. Cheap to clone; clones share one in-memory copy.
#[derive(Clone)]
pub struct Store(Arc<Inner>);

impl Store {
    /// Loads `rustet-store.enc` from `config_dir`. A missing file is an empty store.
    pub async fn open(config_dir: &Path, cipher: StoreCipher) -> Self {
        let path = config_dir.join(STORE_FILE_NAME);
        let (map, keep_old_file) = match tokio::fs::read(&path).await {
            Err(_) => (Map::new(), false),
            Ok(raw) if raw.is_empty() => (Map::new(), false),
            Ok(raw) => match cipher
                .decode(&raw)
                .map(|json| serde_json::from_slice::<Map<String, Value>>(&json))
            {
                Ok(Ok(map)) => (map, false),
                Ok(Err(_)) => {
                    warn!(
                        "{} is not valid JSON; starting empty and keeping the old file",
                        path.display()
                    );
                    (Map::new(), true)
                }
                Err(e) => {
                    warn!(
                        "{} cannot be read ({e}); starting empty and keeping the old file",
                        path.display()
                    );
                    (Map::new(), true)
                }
            },
        };
        Self(Arc::new(Inner {
            path,
            cipher,
            state: Mutex::new(State {
                map,
                dirty: false,
                keep_old_file,
            }),
            write: tokio::sync::Mutex::new(()),
            timer: Mutex::new(None),
            runtime: tokio::runtime::Handle::current(),
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `store:read`: the value, or `null` for an invalid key or a missing value.
    pub fn read(&self, key: &Value) -> Value {
        match page_key(key) {
            Some(key) => self.get(key).unwrap_or(Value::Null),
            None => Value::Null,
        }
    }

    /// `store:write`. `value` is `None` when the page passed none (`undefined`), which is refused.
    /// Ignores invalid keys, oversized values, and new keys past [`STORE_KEYS_MAX`].
    pub fn write(&self, key: &Value, value: Option<Value>) {
        let (Some(key), Some(value)) = (page_key(key), value) else {
            return;
        };
        if !store_value_ok(&value) {
            return;
        }
        let mut state = self.state();
        if !state.map.contains_key(key) && state.map.len() >= STORE_KEYS_MAX {
            return;
        }
        state.map.insert(key.to_owned(), value);
        state.dirty = true;
        drop(state);
        self.schedule_flush();
    }

    /// `store:delete`.
    pub fn delete(&self, key: &Value) {
        if let Some(key) = page_key(key) {
            self.update(|m| m.remove(key));
        }
    }

    /// A value by key, for the shell's own entries (no page-key rules).
    pub fn get(&self, key: &str) -> Option<Value> {
        self.state().map.get(key).cloned()
    }

    /// Changes the store and schedules a write (`_storeDirty = true; _storeFlush()`). Works
    /// from any thread: the write is a task on the runtime the store was opened on.
    pub fn update<R>(&self, f: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
        let out = {
            let mut state = self.state();
            let out = f(&mut state.map);
            state.dirty = true;
            out
        };
        self.schedule_flush();
        out
    }

    fn schedule_flush(&self) {
        let mut timer = self.0.timer.lock().unwrap_or_else(|e| e.into_inner());
        if timer.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let store = self.clone();
        *timer = Some(self.0.runtime.spawn(async move {
            tokio::time::sleep(FLUSH_DELAY).await;
            store
                .0
                .timer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            store.flush(false).await;
        }));
    }

    /// Writes the store now, cancelling a pending delayed write. Returns whether the write
    /// succeeded (`_storeFlushNow`). Call it before quitting.
    pub async fn flush_now(&self) -> bool {
        if let Some(t) = self
            .0
            .timer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            t.abort();
        }
        self.flush(true).await
    }

    async fn flush(&self, always: bool) -> bool {
        let _write = self.0.write.lock().await;
        // The latest state when this write runs.
        let (json, keep_old_file) = {
            let mut state = self.state();
            if !always && !state.dirty {
                return true;
            }
            state.dirty = false;
            (serde_json::to_vec(&state.map), state.keep_old_file)
        };
        let result = match json {
            Ok(json) => self.write_file(json, keep_old_file).await,
            Err(e) => Err(std::io::Error::other(e)),
        };
        let mut state = self.state();
        match result {
            Ok(()) => {
                state.keep_old_file = false;
                true
            }
            Err(e) => {
                // Keep the change; the next flush retries.
                state.dirty = true;
                warn!("store flush failed: {e}");
                false
            }
        }
    }

    async fn write_file(&self, json: Vec<u8>, keep_old_file: bool) -> std::io::Result<()> {
        let data = self.0.cipher.encode(json)?;
        let path = self.0.path.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if keep_old_file && path.exists() {
                std::fs::rename(&path, unreadable_name(&path))?;
            }
            write_atomic(&path, &data)
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

/// `<file>.unreadable`, or the first free `<file>.unreadable.<n>`.
fn unreadable_name(path: &Path) -> PathBuf {
    let base = format!("{}.unreadable", path.display());
    std::iter::once(PathBuf::from(&base))
        .chain((1..).map(|n| PathBuf::from(format!("{base}.{n}"))))
        .find(|p| !p.exists())
        .unwrap_or_else(|| PathBuf::from(base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn open(dir: &Path, cipher: StoreCipher) -> Store {
        Store::open(dir, cipher).await
    }

    struct FakeKeyring(Option<&'static str>);

    impl PasswordSource for FakeKeyring {
        async fn password(&self) -> Result<Option<String>, KeyringError> {
            match self.0 {
                Some("fail") => Err(KeyringError::Refused("locked")),
                p => Ok(p.map(str::to_owned)),
            }
        }
    }

    #[tokio::test]
    async fn page_rules_for_keys_values_and_counts() {
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path(), StoreCipher::Plain).await;
        s.write(&json!("rustet_play_counts"), Some(json!({ "a": 1 })));
        assert_eq!(s.read(&json!("rustet_play_counts")), json!({ "a": 1 }));
        for bad in [
            json!("_x"),
            json!("1x"),
            json!("a/b"),
            json!("a".repeat(129)),
            json!(5),
        ] {
            s.write(&bad, Some(json!(1)));
            assert_eq!(s.read(&bad), Value::Null, "{bad}");
        }
        s.write(&json!("big"), Some(json!("x".repeat(300 * 1024))));
        assert_eq!(s.read(&json!("big")), Value::Null);
        s.write(&json!("undef"), None);
        assert_eq!(s.get("undef"), None);
        s.write(&json!("nul"), Some(Value::Null));
        assert_eq!(
            s.get("nul"),
            Some(Value::Null),
            "null is a value, undefined is not"
        );
        s.update(|m| {
            for i in 0..STORE_KEYS_MAX {
                m.insert(format!("k{i}"), json!(i));
            }
        });
        s.write(&json!("onemore"), Some(json!(1)));
        assert_eq!(s.get("onemore"), None, "no new keys past the limit");
        s.write(&json!("k1"), Some(json!("changed")));
        assert_eq!(
            s.get("k1"),
            Some(json!("changed")),
            "existing keys can still change"
        );
        s.delete(&json!("k1"));
        assert_eq!(s.get("k1"), None);
    }

    #[test]
    fn value_limit() {
        assert!(store_value_ok(&json!({ "plays": [1, 2, 3] })));
        assert!(!store_value_ok(&json!("x".repeat(300 * 1024))));
    }

    #[tokio::test]
    async fn round_trips_encrypted() {
        let d = tempfile::tempdir().unwrap();
        let cipher = StoreCipher::from_keyring(&FakeKeyring(Some("pw"))).await;
        assert!(matches!(cipher, StoreCipher::Linux(_)));
        let s = open(d.path(), cipher.clone()).await;
        s.update(|m| m.insert("rustet_resume_state".into(), json!({ "t": 1 })));
        assert!(s.flush_now().await);
        let raw = std::fs::read(d.path().join(STORE_FILE_NAME)).unwrap();
        assert!(raw.starts_with(b"v11"));
        let s = open(d.path(), cipher).await;
        assert_eq!(s.get("rustet_resume_state"), Some(json!({ "t": 1 })));
    }

    #[tokio::test]
    async fn electron_files_load_plain_and_v10() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(STORE_FILE_NAME), br#"{"drm_email":"a@b.c"}"#).unwrap();
        let s = open(
            d.path(),
            StoreCipher::from_keyring(&FakeKeyring(None)).await,
        )
        .await;
        assert_eq!(s.get("drm_email"), Some(json!("a@b.c")));
        // A plain file still loads once a keyring exists (Chromium reads unprefixed data as clear text).
        let s = open(d.path(), StoreCipher::Linux(LinuxKey::derive(b"pw"))).await;
        assert_eq!(s.get("drm_email"), Some(json!("a@b.c")));
        // Electron's v10 bytes for {"a":1}.
        let v10 = [
            0x76, 0x31, 0x30, 0x1b, 0x30, 0x44, 0xba, 0xde, 0x8c, 0xe0, 0xc5, 0x18, 0x43, 0xbc,
            0xe9, 0x11, 0x13, 0xab, 0xfb,
        ];
        std::fs::write(d.path().join(STORE_FILE_NAME), v10).unwrap();
        assert_eq!(
            open(d.path(), StoreCipher::Plain).await.get("a"),
            Some(json!(1))
        );
    }

    #[tokio::test]
    async fn plain_store_writes_plain_json() {
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path(), StoreCipher::Plain).await;
        s.update(|m| m.insert("x".into(), json!(1)));
        assert!(s.flush_now().await);
        assert_eq!(
            std::fs::read(d.path().join(STORE_FILE_NAME)).unwrap(),
            br#"{"x":1}"#
        );
    }

    #[tokio::test]
    async fn an_unreadable_file_is_kept_and_the_store_still_works() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join(STORE_FILE_NAME);
        let locked = oscrypt::encrypt_linux_v11(&LinuxKey::derive(b"lost"), br#"{"keep":"me"}"#);
        std::fs::write(&file, &locked).unwrap();
        // The keyring fails: no key for the v11 file.
        let s = open(
            d.path(),
            StoreCipher::from_keyring(&FakeKeyring(Some("fail"))).await,
        )
        .await;
        assert_eq!(s.get("keep"), None);
        assert_eq!(
            std::fs::read(&file).unwrap(),
            locked,
            "loading never touches the file"
        );
        s.update(|m| m.insert("new".into(), json!(true)));
        assert!(s.flush_now().await);
        assert_eq!(
            std::fs::read(d.path().join("rustet-store.enc.unreadable")).unwrap(),
            locked
        );
        assert_eq!(std::fs::read(&file).unwrap(), br#"{"new":true}"#);
        // Only the first write moves a file aside.
        s.update(|m| m.insert("again".into(), json!(true)));
        assert!(s.flush_now().await);
        assert_eq!(
            std::fs::read(d.path().join("rustet-store.enc.unreadable")).unwrap(),
            locked
        );
        assert!(!d.path().join("rustet-store.enc.unreadable.1").exists());
        // The right key reads the kept file.
        std::fs::rename(d.path().join("rustet-store.enc.unreadable"), &file).unwrap();
        assert_eq!(
            open(d.path(), StoreCipher::Linux(LinuxKey::derive(b"lost")))
                .await
                .get("keep"),
            Some(json!("me"))
        );
    }

    #[tokio::test]
    async fn bursts_of_changes_make_one_delayed_write() {
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path(), StoreCipher::Plain).await;
        let file = d.path().join(STORE_FILE_NAME);
        for i in 0..5 {
            s.write(&json!("k"), Some(json!(i)));
        }
        assert!(!file.exists(), "nothing written before the delay");
        tokio::time::sleep(FLUSH_DELAY * 3).await;
        assert_eq!(std::fs::read(&file).unwrap(), br#"{"k":4}"#);
    }

    #[tokio::test]
    async fn a_failed_write_reports_false_and_keeps_the_change() {
        let d = tempfile::tempdir().unwrap();
        let blocker = d.path().join("file-not-dir");
        std::fs::write(&blocker, b"").unwrap();
        let s = open(&blocker, StoreCipher::Plain).await;
        s.update(|m| m.insert("x".into(), json!(1)));
        assert!(!s.flush_now().await);
        assert_eq!(s.get("x"), Some(json!(1)));
    }
}

/// Against a real Secret Service. Never run these on a user session: they create keyring items.
/// Run them inside a throwaway bus with its own gnome-keyring (`dbus-run-session`, temporary
/// `HOME`), with `RUSTET_TEST_PRIVATE_BUS=1` set to confirm it.
#[cfg(all(test, target_os = "linux"))]
mod keyring_tests {
    use super::*;

    fn private_bus() -> bool {
        std::env::var_os("RUSTET_TEST_PRIVATE_BUS").is_some()
    }

    #[tokio::test]
    #[ignore = "needs a private D-Bus session with a Secret Service"]
    async fn secret_service_creates_then_finds_the_password() {
        assert!(
            private_bus(),
            "refusing to touch a keyring outside a private bus"
        );
        let keyring = OsKeyring {
            backend: LinuxBackend::SecretService,
        };
        let first = keyring.password().await.unwrap().unwrap();
        assert_eq!(
            keyring.password().await.unwrap().unwrap(),
            first,
            "found again, not created twice"
        );
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path(), StoreCipher::from_keyring(&keyring).await).await;
        store.update(|m| m.insert("k".into(), Value::from("v")));
        assert!(store.flush_now().await);
        let reopened = Store::open(d.path(), StoreCipher::from_keyring(&keyring).await).await;
        assert_eq!(reopened.get("k"), Some(Value::from("v")));
        println!("PASSWORD {first}");
    }

    /// `RUSTET_TEST_ELECTRON_V11`: a file Electron's `safeStorage.encryptString` wrote with
    /// `--password-store=gnome-libsecret` on this bus; `RUSTET_TEST_ELECTRON_PLAIN`: its plaintext.
    #[tokio::test]
    #[ignore = "needs a private D-Bus session and a blob from Electron"]
    async fn reads_what_electron_wrote_with_the_keyring() {
        assert!(
            private_bus(),
            "refusing to touch a keyring outside a private bus"
        );
        let blob = std::fs::read(std::env::var("RUSTET_TEST_ELECTRON_V11").unwrap()).unwrap();
        assert!(blob.starts_with(b"v11"));
        let StoreCipher::Linux(key) = StoreCipher::from_keyring(&OsKeyring {
            backend: LinuxBackend::SecretService,
        })
        .await
        else {
            panic!("no keyring")
        };
        let plain = oscrypt::decrypt_linux(&blob, Some(&key)).unwrap();
        assert_eq!(
            plain,
            std::env::var("RUSTET_TEST_ELECTRON_PLAIN")
                .unwrap()
                .as_bytes()
        );
    }
}
