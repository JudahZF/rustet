//! The engine child process and the UI's link to it.
//!
//! The engine is this executable started as `rustet engine --api <port> --tls-identity-stdin`
//! (`rustet_desktop::engine::RustetEngine`). The UI makes the TLS identity ([`EngineIdentity`]),
//! hands it to every engine start on standard input, and its [`EngineClient`] trusts only that
//! certificate. [`Engine`] owns the supervised child (restart after a crash, orphan clean-up,
//! both from `rustet_desktop`), the typed client, the `/api/v1/events` stream
//! ([`events::EngineEvent`]) and the [`Connection`] state the offline view draws.

pub mod client;
pub mod events;
mod link;
pub mod types;

use std::io;
use std::path::{Path, PathBuf};

use rustet_desktop::engine::{DrmLayout, ENGINE_PORT, EngineProgram, RustetEngine, bundled_libs};
use rustet_desktop::platform::Env;

pub use client::{ClientError, EngineClient};
pub use events::EngineEvent;
pub use link::{Connection, Engine};

/// The engine's TLS identity: a P-256 key and a self-signed certificate for `localhost`,
/// `127.0.0.1` and `::1`, made fresh at every UI start and never written to disk.
#[derive(Clone)]
pub struct EngineIdentity {
    pem: String,
    cert_der: Vec<u8>,
}

// Holds a private key.
impl std::fmt::Debug for EngineIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineIdentity").finish_non_exhaustive()
    }
}

/// Why the identity could not be made.
#[derive(Debug, thiserror::Error)]
#[error("engine TLS identity: {0}")]
pub struct IdentityError(#[from] rcgen::Error);

impl EngineIdentity {
    pub fn generate() -> Result<Self, IdentityError> {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::default();
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "rustet engine (loopback)");
        params.distinguished_name = name;
        params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into()?),
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ];
        let cert = params.self_signed(&key)?;
        // The layout `rustet_engine::TlsIdentity::from_pem` reads: the certificate, then the
        // PKCS#8 key.
        Ok(Self {
            pem: format!("{}{}", cert.pem(), key.serialize_pem()),
            cert_der: cert.der().to_vec(),
        })
    }

    /// Certificate and key as PEM, for the engine's standard input.
    pub fn pem(&self) -> &str {
        &self.pem
    }

    /// The certificate (DER), the one the client trusts.
    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }
}

/// `RUSTET_ENGINE_PORT` (development and tests), else 20025.
pub fn engine_port(env: &Env) -> u16 {
    match env.get("RUSTET_ENGINE_PORT").map(str::parse::<u16>) {
        Some(Ok(port)) if port != 0 => port,
        Some(_) => {
            tracing::warn!("RUSTET_ENGINE_PORT is not a port number; using {ENGINE_PORT}");
            ENGINE_PORT
        }
        None => ENGINE_PORT,
    }
}

/// The real engine launcher, set up as the web shell did: this executable with `engine`, the
/// DRM session in `<config>/drm/files` (copied once from an install's own `drm/files`), the
/// bundled VLC and FFmpeg beside the executable, and `identity` on standard input.
pub fn launcher(
    config_dir: &Path,
    port: u16,
    identity: &EngineIdentity,
    env: Env,
) -> io::Result<RustetEngine> {
    let program = EngineProgram::current_exe()?;
    let exe_dir = program
        .program
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Ok(RustetEngine {
        program,
        port,
        config_dir: config_dir.to_path_buf(),
        drm: DrmLayout::Installed {
            legacy: Some(exe_dir.join("drm/files")),
        },
        libs: bundled_libs(&exe_dir),
        env,
        proc_root: PathBuf::from("/proc"),
        tls_identity: Some(identity.pem.clone()),
    })
}
