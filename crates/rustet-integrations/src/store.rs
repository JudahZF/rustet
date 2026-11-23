//! The storage the scrobblers keep their credentials in.

/// String values by key in the shell's encrypted store (owned by `rustet-desktop`).
///
/// The scrobblers use the key names of the Electron shell, so sessions made there keep working:
/// `lastfm_api_key`, `lastfm_secret`, `lastfm_sk`, `lastfm_user`, `lb_token`.
/// Writes are best effort, as in the Electron shell: a failed flush is the store's to report.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: String);
    fn delete(&self, key: &str);
}

/// Length of a string as JavaScript counts it (UTF-16 code units), for the Electron limits.
pub(crate) fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `store.get(key)` treated the JavaScript way: a missing or empty value is "not set".
pub(crate) fn get_set(store: &dyn SecretStore, key: &str) -> Option<String> {
    store.get(key).filter(|v| !v.is_empty())
}

#[cfg(test)]
pub(crate) mod memory {
    use std::{collections::HashMap, sync::Mutex};

    /// In-memory store for tests.
    #[derive(Default)]
    pub struct MemoryStore(pub Mutex<HashMap<String, String>>);

    impl super::SecretStore for MemoryStore {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().unwrap().get(key).cloned()
        }
        fn set(&self, key: &str, value: String) {
            self.0.lock().unwrap().insert(key.to_owned(), value);
        }
        fn delete(&self, key: &str) {
            self.0.lock().unwrap().remove(key);
        }
    }
}
