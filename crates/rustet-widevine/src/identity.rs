//! The Widevine device identity: an RSA private key and the client ID blob that names the device.
//!
//! On disk an identity is a directory with two files: `device_private_key` (PEM) and
//! `device_client_id_blob` (an encoded `ClientIdentification`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;
use rsa::PrivateKey;
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey, EncodeRsaPublicKey, LineEnding};
use rsa::pkcs8::DecodePrivateKey;
use sha2::Sha256;
use tracing::{debug, info, warn};

use crate::proto;

const KEY_FILE: &str = "device_private_key";
const CLIENT_ID_FILE: &str = "device_client_id_blob";

/// Overrides the identity directory. An explicit directory is never second-guessed: no legacy
/// identity is migrated into it.
const DIR_ENV: &str = "RUSTET_RUSTETPLAY_DIR";
/// `1` lets [`Identity::load`] generate a new identity when none is stored.
const GENERATE_ENV: &str = "RUSTET_GENERATE_IDENTITY";

/// The built-in identity of an Android emulator, used when no other identity exists.
const EMBEDDED_KEY: &str = include_str!("embedded/device_private_key");
const EMBEDDED_CLIENT_ID: &[u8] = include_bytes!("embedded/device_client_id_blob");

/// Concurrent callers must not each generate or migrate an identity.
static LOAD: Mutex<()> = Mutex::new(());

/// A Widevine device identity.
#[derive(Clone, PartialEq)]
pub struct Identity {
    pub(crate) private_key: PrivateKey,
    /// An encoded `ClientIdentification`, sent in every licence request as it is.
    pub(crate) client_id: Vec<u8>,
}

/// An identity as its two files hold it. The key is kept as text so a migrated identity is
/// copied byte for byte.
struct Stored {
    key_pem: String,
    client_id: Vec<u8>,
}

/// Where [`Identity::load`] looks, resolved from the environment.
struct Sources {
    dir: PathBuf,
    /// Directories where earlier versions kept an identity. Empty when `dir` was set explicitly.
    legacy: Vec<PathBuf>,
    generate: bool,
}

/// Where the per-device identity is stored: `$RUSTET_RUSTETPLAY_DIR`, else `<config dir>/rustetplay`.
pub fn identity_dir() -> PathBuf {
    explicit_dir().unwrap_or_else(|| rustet_core::appdir::config_dir().join("rustetplay"))
}

fn explicit_dir() -> Option<PathBuf> {
    std::env::var_os(DIR_ENV)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

impl Sources {
    fn from_env() -> Self {
        let generate = std::env::var_os(GENERATE_ENV).is_some_and(|v| v == "1");
        match explicit_dir() {
            Some(dir) => Self {
                dir,
                legacy: Vec::new(),
                generate,
            },
            None => Self::under(rustet_core::appdir::config_dir(), generate),
        }
    }

    fn under(config_dir: &Path, generate: bool) -> Self {
        let mut legacy = vec![config_dir.join("widevine")];
        if let Some(parent) = config_dir.parent() {
            legacy.push(parent.join("musickit-sdk-linux").join("widevine"));
        }
        Self {
            dir: config_dir.join("rustetplay"),
            legacy,
            generate,
        }
    }
}

impl Identity {
    /// Selects the device identity, in order of preference:
    ///  1. the identity in [`identity_dir`],
    ///  2. one left by an earlier version (copied into [`identity_dir`] so it is kept),
    ///  3. a freshly generated identity, only when `RUSTET_GENERATE_IDENTITY=1`,
    ///  4. the built-in default.
    ///
    /// Generation is opt-in because a self-made identity has no certificate chain and the
    /// licence server rejects it (code -42585). Real device identities are accepted.
    ///
    /// This reads (and may write) small files, so it blocks.
    pub fn load() -> Self {
        Self::load_from(&Sources::from_env())
    }

    fn load_from(sources: &Sources) -> Self {
        let _guard = LOAD.lock().unwrap_or_else(|e| e.into_inner());

        if let Some((identity, _)) = read(&sources.dir) {
            debug!(dir = %sources.dir.display(), "rustetplay: loaded existing device identity");
            return identity;
        }

        if let Some((old, identity, stored)) = sources
            .legacy
            .iter()
            .find_map(|old| read(old).map(|(identity, stored)| (old, identity, stored)))
        {
            match persist(&sources.dir, &stored) {
                Ok(()) => {
                    info!(from = %old.display(), to = %sources.dir.display(), "rustetplay: migrated device identity")
                }
                Err(err) => {
                    warn!(from = %old.display(), %err, "rustetplay: cannot copy legacy identity, using it in place")
                }
            }
            return identity;
        }

        if sources.generate {
            match generate()
                .map_err(io::Error::other)
                .and_then(|(identity, stored)| {
                    persist(&sources.dir, &stored)?;
                    Ok(identity)
                }) {
                Ok(identity) => {
                    info!(dir = %sources.dir.display(), "rustetplay: generated new per-device identity");
                    return identity;
                }
                Err(err) => {
                    warn!(%err, "rustetplay: identity generation failed, using built-in default")
                }
            }
        }
        Self::embedded()
    }

    /// The built-in default identity.
    pub fn embedded() -> Self {
        Self {
            private_key: parse_key(EMBEDDED_KEY)
                .expect("the embedded device key is a valid RSA key"),
            client_id: EMBEDDED_CLIENT_ID.to_vec(),
        }
    }
}

/// Parses a PEM RSA private key, PKCS#1 or PKCS#8.
fn parse_key(pem: &str) -> Option<PrivateKey> {
    PrivateKey::from_pkcs1_pem(pem)
        .ok()
        .or_else(|| PrivateKey::from_pkcs8_pem(pem).ok())
}

/// Reads the identity stored in `dir`. Both files must exist and the key must parse: a truncated
/// or corrupt file would otherwise be used as-is and fail every licence request, with the working
/// fallbacks never consulted.
fn read(dir: &Path) -> Option<(Identity, Stored)> {
    let key = std::fs::read(dir.join(KEY_FILE));
    let client_id = std::fs::read(dir.join(CLIENT_ID_FILE));
    let (Ok(key), Ok(client_id)) = (key, client_id) else {
        return None;
    };
    let private_key = String::from_utf8(key)
        .ok()
        .and_then(|pem| Some((parse_key(&pem)?, pem)))
        .filter(|_| !client_id.is_empty());
    let Some((private_key, key_pem)) = private_key else {
        warn!(dir = %dir.display(), "rustetplay: ignoring unreadable device identity");
        return None;
    };
    Some((
        Identity {
            private_key,
            client_id: client_id.clone(),
        },
        Stored { key_pem, client_id },
    ))
}

/// Stores an identity atomically. The client ID goes first and the key last, so the pair only
/// counts as present once the key exists.
fn persist(dir: &Path, stored: &Stored) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    write_file_atomic(&dir.join(CLIENT_ID_FILE), &stored.client_id)?;
    write_file_atomic(&dir.join(KEY_FILE), stored.key_pem.as_bytes())
}

/// Writes `data` to a temporary file next to `path` (mode 0600 on Unix), syncs it, then renames
/// it over `path`, so readers see the old or the new file, never half of one.
fn write_file_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!("{name}.tmp-"))
        .tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Creates an RSA-2048 key pair and a Widevine L3 `ClientIdentification` blob with OS-derived
/// device information. No provisioning server is involved.
fn generate() -> Result<(Identity, Stored), rsa::Error> {
    let mut rng = rand::rng();
    let private_key = PrivateKey::new(&mut rng, 2048)?;
    let key_pem = private_key
        .to_pkcs1_pem(LineEnding::LF)
        .map_err(rsa::Error::from)?
        .to_string();
    // PKCS#1 DER: what DeviceCertificate.PublicKey holds.
    let public_key = private_key
        .to_public_key()
        .to_pkcs1_der()
        .map_err(rsa::Error::from)?;

    let mut serial = vec![0; 16];
    rand::fill(serial.as_mut_slice());
    let certificate = proto::DeviceCertificate {
        r#type: Some(proto::CERTIFICATE_TYPE_USER_DEVICE),
        serial_number: Some(serial),
        creation_time_seconds: Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as u32),
        ),
        public_key: Some(public_key.as_bytes().to_vec()),
        system_id: Some(0x1000),
    };

    // Self-signed (PSS-SHA256). Apple's licence server does not check the chain of the emulator
    // identity it accepts, so this was expected to suffice for L3; see the note on generation in
    // [`Identity::load`].
    let signature = private_key.pss_sign::<Sha256>(&certificate.encode_to_vec())?;
    let client_id = proto::ClientIdentification {
        r#type: Some(proto::TOKEN_TYPE_DEVICE_CERTIFICATE),
        token: Some(proto::SignedDeviceCertificate {
            device_certificate: Some(certificate),
            signature: Some(signature),
        }),
        client_info: client_info(),
    }
    .encode_to_vec();

    Ok((
        Identity {
            private_key,
            client_id: client_id.clone(),
        },
        Stored { key_pem, client_id },
    ))
}

/// The `ClientInfo` name/value pairs of a generated identity.
fn client_info() -> Vec<proto::NameValue> {
    // Keep only ASCII letters, digits and hyphens.
    let hostname: String = gethostname::gethostname()
        .to_string_lossy()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let device_name = if hostname.is_empty() {
        "device".to_owned()
    } else {
        hostname
    };

    // Go's GOARCH names, mapped to Widevine naming as the original did.
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    let platform = std::env::consts::OS;
    let build_info =
        format!("rustet/{platform}/{arch}/{device_name}:1/RUSTET.1/1:release/release-keys");

    [
        ("company_name", "Rustet".to_owned()),
        ("model_name", format!("{platform} {arch}")),
        ("architecture_name", arch.to_owned()),
        ("device_name", device_name),
        ("product_name", format!("rustet-{platform}")),
        ("build_info", build_info),
        ("widevine_cdm_version", "14.0.0".to_owned()),
        ("oem_crypto_security_patch_level", "0".to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| proto::NameValue {
        name: Some(name.to_owned()),
        value: Some(value),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::OnceLock;

    use super::*;

    /// One generated identity for the whole test run: generation is slow.
    fn test_identity() -> &'static (Identity, String) {
        static ID: OnceLock<(Identity, String)> = OnceLock::new();
        ID.get_or_init(|| {
            let (identity, stored) = generate().unwrap();
            (identity, stored.key_pem)
        })
    }

    fn write_identity(dir: &Path, key: &str, client_id: &[u8]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(KEY_FILE), key).unwrap();
        fs::write(dir.join(CLIENT_ID_FILE), client_id).unwrap();
    }

    fn explicit(dir: &Path, generate: bool) -> Sources {
        Sources {
            dir: dir.to_path_buf(),
            legacy: Vec::new(),
            generate,
        }
    }

    #[test]
    fn prefers_the_configured_identity() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Identity::load_from(&explicit(tmp.path(), false)) == Identity::embedded());

        let (identity, pem) = test_identity();
        write_identity(tmp.path(), pem, &[1, 2]);
        let loaded = Identity::load_from(&explicit(tmp.path(), false));
        assert!(loaded.private_key == identity.private_key);
        assert_eq!(loaded.client_id, [1, 2]);
    }

    #[test]
    fn generated_identity_is_well_formed() {
        let (identity, pem) = test_identity();
        assert!(pem.starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        let key = PrivateKey::from_pkcs1_pem(pem).unwrap();
        assert_eq!(rsa::traits::PublicKeyParts::size(&key.to_public_key()), 256);

        let cid = proto::ClientIdentification::decode(identity.client_id.as_slice()).unwrap();
        assert_eq!(cid.r#type, Some(proto::TOKEN_TYPE_DEVICE_CERTIFICATE));
        let cert = cid
            .token
            .and_then(|t| t.device_certificate)
            .expect("a device certificate");
        assert_eq!(cert.r#type, Some(proto::CERTIFICATE_TYPE_USER_DEVICE));
        assert_eq!(
            cert.public_key.unwrap(),
            key.to_public_key().to_pkcs1_der().unwrap().as_bytes()
        );

        let names: Vec<_> = cid
            .client_info
            .iter()
            .filter_map(|kv| kv.name.as_deref())
            .collect();
        for required in [
            "company_name",
            "model_name",
            "architecture_name",
            "device_name",
            "product_name",
            "build_info",
            "widevine_cdm_version",
        ] {
            assert!(names.contains(&required), "ClientInfo misses {required}");
        }
    }

    #[test]
    fn generates_distinct_keys() {
        let (_, other) = generate().unwrap();
        assert_ne!(other.key_pem, test_identity().1);
    }

    #[test]
    fn generates_and_persists_on_first_run_then_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Identity::load_from(&explicit(tmp.path(), true));
        assert!(first != Identity::embedded());

        let key = fs::read_to_string(tmp.path().join(KEY_FILE)).unwrap();
        let cid = fs::read(tmp.path().join(CLIENT_ID_FILE)).unwrap();
        assert!(key.contains("RSA PRIVATE KEY"));
        assert!(cid.len() >= 32);
        assert_eq!(first.client_id, cid);

        let second = Identity::load_from(&explicit(tmp.path(), false));
        assert!(
            first == second,
            "second load must reload the persisted identity"
        );
    }

    #[test]
    fn write_file_atomic_replaces_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("k");
        write_file_atomic(&path, b"one").unwrap();
        write_file_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "temp files left behind"
        );
        assert!(write_file_atomic(&tmp.path().join("missing/k"), b"").is_err());
    }

    #[test]
    fn migrates_a_legacy_identity_and_does_not_generate() {
        let root = tempfile::tempdir().unwrap();
        let (identity, pem) = test_identity();
        write_identity(
            &root.path().join("musickit-sdk-linux/widevine"),
            pem,
            &[9, 9, 9],
        );

        let sources = Sources::under(&root.path().join("rustet"), false);
        let loaded = Identity::load_from(&sources);
        assert!(loaded.private_key == identity.private_key);
        assert_eq!(loaded.client_id, [9, 9, 9]);
        let (copied, stored) =
            read(&sources.dir).expect("legacy identity copied into the identity dir");
        assert!(copied == loaded);
        assert_eq!(&stored.key_pem, pem);

        // With nothing anywhere, the built-in default is used and nothing is written.
        let empty = tempfile::tempdir().unwrap();
        let sources = Sources::under(&empty.path().join("rustet"), false);
        assert!(Identity::load_from(&sources) == Identity::embedded());
        assert!(
            read(&sources.dir).is_none(),
            "an identity must not be generated unless enabled"
        );
    }

    #[test]
    fn concurrent_callers_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded: Vec<Identity> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| Identity::load_from(&explicit(tmp.path(), true))))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let (stored, _) = read(tmp.path()).unwrap();
        assert!(loaded.iter().all(|id| *id == stored), "callers raced");
    }

    #[test]
    fn a_corrupt_identity_falls_through_to_the_next_source() {
        let root = tempfile::tempdir().unwrap();
        let sources = Sources::under(&root.path().join("rustet"), false);
        // A truncated key in the live dir, a good one in a legacy dir.
        write_identity(
            &sources.dir,
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE",
            &[1, 2, 3],
        );
        let (identity, pem) = test_identity();
        write_identity(
            &root.path().join("musickit-sdk-linux/widevine"),
            pem,
            &[4, 5],
        );

        let loaded = Identity::load_from(&sources);
        assert!(loaded.private_key == identity.private_key);
        assert_eq!(loaded.client_id, [4, 5]);
        let (_, stored) = read(&sources.dir).expect("the good identity replaces the corrupt one");
        assert_eq!(&stored.key_pem, pem);
    }

    #[test]
    fn key_validation() {
        assert!(parse_key(&test_identity().1).is_some());
        assert!(parse_key(EMBEDDED_KEY).is_some());
        for bad in [
            "",
            "KEY",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----",
        ] {
            assert!(parse_key(bad).is_none(), "{bad:?} must not validate");
        }
    }
}
