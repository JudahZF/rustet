//! The cache key file and AES-256-GCM sealing of the dump.
//!
//! Key: 32 random bytes in `library.key`, generated on first use.
//! Sealed data: `nonce (12 bytes) || ciphertext || tag (16 bytes)`, no associated data.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

use crate::LibraryError;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

type Key = [u8; KEY_LEN];

/// The key file, read (or created) once and then cached.
pub(crate) struct KeyFile {
    path: PathBuf,
    cached: Mutex<Option<Key>>,
}

impl KeyFile {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            cached: Mutex::new(None),
        }
    }

    pub(crate) fn key(&self) -> Result<Key, LibraryError> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = *cached {
            return Ok(key);
        }
        let key = match self.read() {
            Ok(key) => key,
            // Only a missing file means "no key yet". Any other failure (permissions, I/O)
            // must not mint a new key: that would overwrite the real one and orphan the cache.
            Err(LibraryError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                self.create()?
            }
            Err(err) => return Err(err),
        };
        *cached = Some(key);
        Ok(key)
    }

    fn read(&self) -> Result<Key, LibraryError> {
        let data = fs::read(&self.path).map_err(|source| LibraryError::Io {
            context: "read key",
            source,
        })?;
        Key::try_from(data.as_slice()).map_err(|_| LibraryError::KeyLength {
            len: data.len(),
            path: self.path.clone(),
        })
    }

    /// Writes a new key to a temp file and hard-links it into place: the link fails if
    /// another process created the key first (then theirs is used), and a reader never
    /// sees a half-written key file. The temp file is created with mode 0600.
    fn create(&self) -> Result<Key, LibraryError> {
        let io_err = |source| LibraryError::Io {
            context: "write key",
            source,
        };
        let mut key = [0u8; KEY_LEN];
        getrandom::fill(&mut key).map_err(|err| io_err(io::Error::other(err)))?;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".library.key.")
            .tempfile_in(dir)
            .map_err(io_err)?;
        tmp.write_all(&key).map_err(io_err)?;
        match fs::hard_link(tmp.path(), &self.path) {
            Ok(()) => Ok(key),
            Err(link_err) => self.read().map_err(|_| io_err(link_err)),
        }
        // `tmp` is removed on drop.
    }

    pub(crate) fn encrypt(&self, plain: &[u8]) -> Result<Vec<u8>, LibraryError> {
        let cipher = Aes256Gcm::new(&self.key()?.into());
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|err| LibraryError::Io {
            context: "generate nonce",
            source: io::Error::other(err),
        })?;
        let sealed = cipher
            .encrypt(&Nonce::from(nonce), plain)
            .map_err(|_| LibraryError::Crypto)?;
        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    pub(crate) fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, LibraryError> {
        let cipher = Aes256Gcm::new(&self.key()?.into());
        let (nonce, sealed) = data
            .split_first_chunk::<NONCE_LEN>()
            .ok_or(LibraryError::CiphertextTooShort)?;
        cipher
            .decrypt(&Nonce::from(*nonce), sealed)
            .map_err(|_| LibraryError::Crypto)
    }
}
