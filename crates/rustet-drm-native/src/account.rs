//! The account files the FairPlay session leaves in its base directory (`drm_linux.c`,
//! `drm_get_account`).
//!
//! `STOREFRONT_ID` and `MUSIC_TOKEN` are written after every successful start or sign-in. The
//! developer token is not stored; the engine fetches it itself.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Files larger than this are refused instead of read: the credential files are a few hundred
/// bytes, and a replaced file must not make us allocate whatever it claims.
const MAX_FILE_LEN: u64 = 65536;

/// The account as the session files describe it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct StoredAccount {
    /// `STOREFRONT_ID`, empty when that file is missing.
    pub storefront_id: String,
    /// `MUSIC_TOKEN`, never empty.
    pub music_token: String,
}

impl std::fmt::Debug for StoredAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredAccount")
            .field("storefront_id", &self.storefront_id)
            .field("music_token", &"<redacted>")
            .finish()
    }
}

/// Reads the stored account below `base_dir`. `None` when there is no usable `MUSIC_TOKEN`.
pub fn stored_account(base_dir: &Path) -> Option<StoredAccount> {
    let music_token = read_trimmed(&base_dir.join("MUSIC_TOKEN")).filter(|t| !t.is_empty())?;
    let storefront_id = read_trimmed(&base_dir.join("STOREFRONT_ID")).unwrap_or_default();
    Some(StoredAccount {
        storefront_id,
        music_token,
    })
}

/// The file's text without trailing newlines and spaces, cut at the first NUL as the C string
/// was. `None` for a missing, empty or oversized file.
fn read_trimmed(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 || len > MAX_FILE_LEN {
        return None;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    while buf
        .last()
        .is_some_and(|b| matches!(b, b'\n' | b'\r' | b' '))
    {
        buf.pop();
    }
    if let Some(nul) = buf.iter().position(|&b| b == 0) {
        buf.truncate(nul);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from drm/tests/test_drm_linux.c. The C built JSON by hand, so quotes and
    // backslashes had to be escaped; here the value must simply come back unchanged, with only
    // the trailing newline removed.
    #[test]
    fn token_with_quotes_and_backslashes_is_returned_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MUSIC_TOKEN"), "ab\"c\\d\n").unwrap();
        std::fs::write(dir.path().join("STOREFRONT_ID"), "us\n").unwrap();
        let account = stored_account(dir.path()).unwrap();
        assert_eq!(account.music_token, "ab\"c\\d");
        assert_eq!(account.storefront_id, "us");
    }

    #[test]
    fn oversized_token_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MUSIC_TOKEN"), vec![b'a'; 70000]).unwrap();
        assert!(stored_account(dir.path()).is_none());
    }
}
