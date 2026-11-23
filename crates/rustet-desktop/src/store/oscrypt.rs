//! Chromium OSCrypt, the format Electron's `safeStorage` writes.
//!
//! Linux (`components/os_crypt/sync/os_crypt_linux.cc`): `v10` or `v11` followed by
//! AES-128-CBC (PKCS#7) ciphertext. The key is PBKDF2-HMAC-SHA1 of a password, salt
//! `saltysalt`, one iteration, 16 bytes; the IV is 16 spaces. `v10` uses the built-in password
//! `peanuts`, `v11` the password kept in the OS keyring. Empty plaintext encrypts to nothing, and
//! data with neither prefix is clear text from before encryption was available.
//!
//! Windows (`os_crypt_win.cc`): `v10` followed by a 12-byte nonce and AES-256-GCM ciphertext
//! with its tag. The 32-byte key is DPAPI-protected in the profile's `Local State`.

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

const PREFIX_V10: &[u8] = b"v10";
const PREFIX_V11: &[u8] = b"v11";
const SALT: &[u8] = b"saltysalt";
const IV: [u8; 16] = [b' '; 16];
/// The `v10` password on Linux when no keyring is used.
const V10_PASSWORD: &[u8] = b"peanuts";
const GCM_NONCE_LEN: usize = 12;

/// Why data could not be decrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecryptError {
    /// `v11` data, and the keyring password is not available.
    #[error("the data needs the keyring password, which is not available")]
    KeyringUnavailable,
    /// Wrong key or corrupt data.
    #[error("the data does not decrypt")]
    Corrupt,
}

/// A Linux OSCrypt key (AES-128).
#[derive(Clone, PartialEq, Eq)]
pub struct LinuxKey([u8; 16]);

impl std::fmt::Debug for LinuxKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinuxKey(..)")
    }
}

impl LinuxKey {
    /// The key for `password`.
    pub fn derive(password: &[u8]) -> Self {
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, SALT, 1, &mut key);
        Self(key)
    }

    /// The `v10` key (`peanuts`).
    pub fn v10() -> Self {
        Self::derive(V10_PASSWORD)
    }

    fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        cbc::Encryptor::<aes::Aes128>::new(&self.0.into(), &IV.into())
            .encrypt_padded_vec::<Pkcs7>(plaintext)
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, DecryptError> {
        cbc::Decryptor::<aes::Aes128>::new(&self.0.into(), &IV.into())
            .decrypt_padded_vec::<Pkcs7>(ciphertext)
            .map_err(|_| DecryptError::Corrupt)
    }
}

/// A Windows OSCrypt key (AES-256-GCM), as unprotected from `Local State`.
#[derive(Clone, PartialEq, Eq)]
pub struct WindowsKey(pub [u8; 32]);

impl std::fmt::Debug for WindowsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WindowsKey(..)")
    }
}

/// `v11` encryption with the keyring key (what `safeStorage.encryptString` writes on Linux).
pub fn encrypt_linux_v11(key: &LinuxKey, plaintext: &[u8]) -> Vec<u8> {
    if plaintext.is_empty() {
        return Vec::new();
    }
    [PREFIX_V11, &key.encrypt(plaintext)].concat()
}

/// Linux decryption: `v11` with the keyring key, `v10` with the built-in one, anything else is
/// returned as clear text.
pub fn decrypt_linux(data: &[u8], keyring: Option<&LinuxKey>) -> Result<Vec<u8>, DecryptError> {
    if let Some(body) = data.strip_prefix(PREFIX_V11) {
        keyring
            .ok_or(DecryptError::KeyringUnavailable)?
            .decrypt(body)
    } else if let Some(body) = data.strip_prefix(PREFIX_V10) {
        LinuxKey::v10().decrypt(body)
    } else {
        Ok(data.to_vec())
    }
}

/// Windows encryption: `v10`, a fresh nonce, AES-256-GCM.
pub fn encrypt_windows(key: &WindowsKey, plaintext: &[u8]) -> Result<Vec<u8>, getrandom::Error> {
    let mut nonce = [0u8; GCM_NONCE_LEN];
    getrandom::fill(&mut nonce)?;
    let sealed = Aes256Gcm::new(&key.0.into())
        .encrypt(&Nonce::from(nonce), plaintext)
        .expect("AES-GCM encryption of an in-memory buffer cannot fail");
    Ok([PREFIX_V10, &nonce, &sealed].concat())
}

/// Windows decryption of `v10` data; anything without the prefix is clear text.
pub fn decrypt_windows(data: &[u8], key: &WindowsKey) -> Result<Vec<u8>, DecryptError> {
    let Some(body) = data.strip_prefix(PREFIX_V10) else {
        return Ok(data.to_vec());
    };
    if body.len() < GCM_NONCE_LEN {
        return Err(DecryptError::Corrupt);
    }
    let (nonce, sealed) = body.split_at(GCM_NONCE_LEN);
    let nonce: [u8; GCM_NONCE_LEN] = nonce.try_into().map_err(|_| DecryptError::Corrupt)?;
    Aes256Gcm::new(&key.0.into())
        .decrypt(&Nonce::from(nonce), sealed)
        .map_err(|_| DecryptError::Corrupt)
}

/// The DPAPI blob of the Windows OSCrypt key: `os_crypt.encrypted_key` of `Local State`,
/// base64-decoded, without its `DPAPI` prefix. Unprotecting it (`CryptUnprotectData`) is
/// Windows-only and not part of this crate yet.
pub fn windows_key_blob(local_state: &[u8]) -> Option<Vec<u8>> {
    use base64::Engine;
    let state: serde_json::Value = serde_json::from_slice(local_state).ok()?;
    let b64 = state.get("os_crypt")?.get("encrypted_key")?.as_str()?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    raw.strip_prefix(b"DPAPI").map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Produced by Electron 43's `safeStorage.encryptString` (`--password-store=basic` with
    /// plain-text encryption allowed, which is the `v10` path).
    const ELECTRON_V10: [(&str, &str); 4] = [
        ("{\"a\":1}", "7631301b3044bade8ce0c51843bce91113abfb"),
        ("", ""),
        (
            "xxxxxxxxxxxxxxxx",
            "7631304eaeebcc9931e80c65fdfedf3c32e007af9f5b8b6c30d929a49fcf25fb4fe017",
        ),
        (
            "{\"adam_resume_state\":{\"t\":1},\"drm_email\":\"ä@b.c\"}",
            "763130fde6a157ce63d8500b397573c573f6acb2741c2fd8fbcc4e60ba82147fe434f27daa2b00ccf0b7e0633405943b7f7999a20a36771198e3b025b25f9898448213",
        ),
    ];

    #[test]
    fn decrypts_what_electron_wrote() {
        for (plain, blob) in ELECTRON_V10 {
            assert_eq!(
                decrypt_linux(&hex(blob), None).unwrap(),
                plain.as_bytes(),
                "{plain}"
            );
        }
    }

    #[test]
    fn v10_encryption_matches_electron_byte_for_byte() {
        // The IV is fixed, so the same key and plaintext give Electron's exact bytes.
        for (plain, blob) in ELECTRON_V10.iter().filter(|(p, _)| !p.is_empty()) {
            let ours = [PREFIX_V10, &LinuxKey::v10().encrypt(plain.as_bytes())].concat();
            assert_eq!(ours, hex(blob));
        }
    }

    #[test]
    fn v11_round_trip_needs_the_keyring_key() {
        let key = LinuxKey::derive(b"keyring-secret");
        let blob = encrypt_linux_v11(&key, b"{\"x\":1}");
        assert!(blob.starts_with(b"v11"));
        assert_eq!(decrypt_linux(&blob, Some(&key)).unwrap(), b"{\"x\":1}");
        assert_eq!(
            decrypt_linux(&blob, None),
            Err(DecryptError::KeyringUnavailable)
        );
        assert_eq!(
            decrypt_linux(&blob, Some(&LinuxKey::derive(b"other"))),
            Err(DecryptError::Corrupt)
        );
        assert!(encrypt_linux_v11(&key, b"").is_empty());
    }

    #[test]
    fn data_without_a_prefix_is_clear_text() {
        assert_eq!(
            decrypt_linux(b"{\"plain\":true}", None).unwrap(),
            b"{\"plain\":true}"
        );
        assert_eq!(decrypt_windows(b"{}", &WindowsKey([7; 32])).unwrap(), b"{}");
    }

    #[test]
    fn windows_format_round_trip_and_key_blob() {
        use base64::Engine;
        let key = WindowsKey([7; 32]);
        let blob = encrypt_windows(&key, b"secret").unwrap();
        assert_eq!(&blob[..3], b"v10");
        assert_eq!(blob.len(), 3 + 12 + 6 + 16);
        assert_eq!(decrypt_windows(&blob, &key).unwrap(), b"secret");
        assert_eq!(
            decrypt_windows(&blob, &WindowsKey([8; 32])),
            Err(DecryptError::Corrupt)
        );
        assert_eq!(
            decrypt_windows(b"v10short", &key),
            Err(DecryptError::Corrupt)
        );
        let enc = base64::engine::general_purpose::STANDARD.encode(b"DPAPIblob");
        let state = format!(r#"{{"os_crypt":{{"encrypted_key":"{enc}"}}}}"#);
        assert_eq!(windows_key_blob(state.as_bytes()).unwrap(), b"blob");
        assert_eq!(windows_key_blob(b"{}"), None);
    }
}
