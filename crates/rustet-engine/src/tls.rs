//! The loopback TLS identity.
//!
//! The engine serves HTTPS on 127.0.0.1: Chromium's `<video src>` refuses plain `http://` on the
//! `https://` Apple Music page (mixed content), and a real HTTPS origin gives it native,
//! reliable byte-range seeking.
//!
//! The identity is an ECDSA P-256 key and a self-signed certificate, valid from an hour ago
//! (clock skew) for one year, and never written to disk. Either the engine generates it on
//! start, or the shell generates it ([`TlsIdentity::generate`]), passes
//! [`TlsIdentity::spki_sha256_base64`] to Chromium (`--ignore-certificate-errors-spki-list`:
//! CEF cannot accept a self-signed certificate at run time) and writes
//! [`TlsIdentity::to_pem`] to the engine's standard input.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

/// Why a TLS identity or configuration could not be built.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("tls config: {0}")]
    Config(#[from] rustls::Error),
    #[error("pem: {0}")]
    Pem(#[from] rustls::pki_types::pem::Error),
    /// The private key has no public key to compare with the certificate's.
    #[error("tls identity: the key's public key is unknown")]
    NoPublicKey,
}

/// A certificate and its private key: what the loopback server presents.
pub struct TlsIdentity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    /// DER `SubjectPublicKeyInfo` of the certificate (checked equal to the key's).
    spki: Vec<u8>,
}

impl Clone for TlsIdentity {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            key: self.key.clone_key(),
            spki: self.spki.clone(),
        }
    }
}

// Holds a private key: no `Debug` output of its bytes.
impl std::fmt::Debug for TlsIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIdentity")
            .field("spki_sha256", &self.spki_sha256_base64())
            .finish_non_exhaustive()
    }
}

fn provider() -> CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

impl TlsIdentity {
    /// A new ECDSA P-256 key and a self-signed certificate for `localhost`, `127.0.0.1` and
    /// `::1`.
    pub fn generate() -> Result<Self, TlsError> {
        let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::default();
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "rustet engine (loopback)");
        params.distinguished_name = name;
        let mut serial = [0u8; 16];
        rand_fill(&mut serial);
        serial[0] &= 0x7f; // a positive 128-bit serial, as Go's rand.Int below 2^128
        params.serial_number = Some(SerialNumber::from_slice(&serial));
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::hours(1);
        // Regenerated on every start: no reason for it to outlive a year.
        params.not_after = now
            .replace_year(now.year() + 1)
            .unwrap_or(now + Duration::days(365));
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into()?),
            SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ];
        let cert = params.self_signed(&key)?;
        Self::new(
            cert.der().clone(),
            PrivatePkcs8KeyDer::from(key.serialize_der()),
        )
    }

    /// Checks that `key` belongs to `cert`.
    fn new(
        cert: CertificateDer<'static>,
        key: PrivatePkcs8KeyDer<'static>,
    ) -> Result<Self, TlsError> {
        let certified = CertifiedKey::from_der(
            vec![cert.clone()],
            PrivateKeyDer::Pkcs8(key.clone_key()),
            &provider(),
        )?;
        let spki = certified
            .key
            .public_key()
            .ok_or(TlsError::NoPublicKey)?
            .to_vec();
        Ok(Self { cert, key, spki })
    }

    /// Reads a `CERTIFICATE` block and a `PRIVATE KEY` (PKCS#8) block, in any order; other
    /// blocks are ignored. Fails when either is missing or the key does not match the
    /// certificate.
    pub fn from_pem(pem: &[u8]) -> Result<Self, TlsError> {
        Self::new(
            CertificateDer::from_pem_slice(pem)?,
            PrivatePkcs8KeyDer::from_pem_slice(pem)?,
        )
    }

    /// The certificate block, then the PKCS#8 key block, each with 64-character lines and a
    /// trailing newline: the layout [`Self::from_pem`] reads.
    pub fn to_pem(&self) -> String {
        let mut out = String::new();
        pem_block(&mut out, "CERTIFICATE", &self.cert);
        pem_block(&mut out, "PRIVATE KEY", self.key.secret_pkcs8_der());
        out
    }

    /// Base64 of the SHA-256 of the certificate's DER `SubjectPublicKeyInfo`: the value of
    /// Chromium's `--ignore-certificate-errors-spki-list`.
    pub fn spki_sha256_base64(&self) -> String {
        BASE64.encode(Sha256::digest(&self.spki))
    }

    /// A server configuration presenting this identity. Offers HTTP/2 and HTTP/1.1, TLS 1.3
    /// and 1.2, and session resumption (stateful cache and tickets).
    pub(crate) fn server_config(&self) -> Result<ServerConfig, TlsError> {
        let mut cfg = ServerConfig::builder_with_provider(Arc::new(provider()))
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.cert.clone()],
                PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )?;
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        cfg.ticketer = rustls::crypto::aws_lc_rs::Ticketer::new()?;
        Ok(cfg)
    }

    /// The certificate as DER.
    #[cfg(test)]
    pub(crate) fn cert_der(&self) -> &[u8] {
        &self.cert
    }
}

fn pem_block(out: &mut String, label: &str, der: &[u8]) {
    let b64 = BASE64.encode(der);
    let _ = writeln!(out, "-----BEGIN {label}-----");
    for line in b64.as_bytes().chunks(64) {
        // Base64 output is ASCII.
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    let _ = writeln!(out, "-----END {label}-----");
}

/// Fills `buf` from the OS random source through the TLS provider.
fn rand_fill(buf: &mut [u8]) {
    // Without the OS random source no key could be generated either; the key generation
    // reports that failure.
    let _ = provider().secure_random.fill(buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed identity made once with OpenSSL 3.5: `openssl req -x509 -newkey ec -pkeyopt
    /// ec_paramgen_curve:P-256 -nodes …`, key converted with `openssl pkcs8 -topk8 -nocrypt`.
    pub(crate) const FIXED_PEM: &str = include_str!("../tests/data/identity.pem");
    /// `openssl x509 -in identity.pem -pubkey -noout | openssl pkey -pubin -outform der |
    /// openssl dgst -sha256 -binary | base64` (OpenSSL 3, `nix shell nixpkgs#openssl`).
    pub(crate) const FIXED_SPKI_SHA256: &str = include_str!("../tests/data/identity.spki-sha256");

    #[test]
    fn builds_a_config_offering_h2_and_http1() {
        let cfg = TlsIdentity::generate().unwrap().server_config().unwrap();
        assert_eq!(cfg.alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
        assert!(cfg.ticketer.enabled());
    }

    #[test]
    fn pem_round_trips() {
        let id = TlsIdentity::generate().unwrap();
        let pem = id.to_pem();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.contains("-----END CERTIFICATE-----\n-----BEGIN PRIVATE KEY-----\n"));
        assert!(pem.lines().all(|l| l.len() <= 64));
        let back = TlsIdentity::from_pem(pem.as_bytes()).unwrap();
        assert_eq!(back.cert_der(), id.cert_der());
        assert_eq!(back.spki_sha256_base64(), id.spki_sha256_base64());
        assert_ne!(
            TlsIdentity::generate().unwrap().spki_sha256_base64(),
            id.spki_sha256_base64()
        );
    }

    // The hash is what Chromium pins: it must match OpenSSL's for the same key.
    #[test]
    fn spki_hash_matches_openssl() {
        let id = TlsIdentity::from_pem(FIXED_PEM.as_bytes()).unwrap();
        assert_eq!(id.spki_sha256_base64(), FIXED_SPKI_SHA256.trim());
    }

    #[test]
    fn from_pem_refuses_incomplete_or_mismatched_input() {
        let a = TlsIdentity::generate().unwrap().to_pem();
        let b = TlsIdentity::generate().unwrap().to_pem();
        let (a_cert, _) = a.split_at(a.find("-----BEGIN PRIVATE KEY").unwrap());
        let b_key = &b[b.find("-----BEGIN PRIVATE KEY").unwrap()..];
        assert!(matches!(
            TlsIdentity::from_pem(a_cert.as_bytes()),
            Err(TlsError::Pem(_))
        ));
        assert!(matches!(
            TlsIdentity::from_pem(b_key.as_bytes()),
            Err(TlsError::Pem(_))
        ));
        assert!(
            TlsIdentity::from_pem(format!("{a_cert}{b_key}").as_bytes()).is_err(),
            "key of another certificate"
        );
        assert!(TlsIdentity::from_pem(b"").is_err());
        // Key first is fine.
        let a_key = &a[a.find("-----BEGIN PRIVATE KEY").unwrap()..];
        assert!(TlsIdentity::from_pem(format!("{a_key}{a_cert}").as_bytes()).is_ok());
    }
}
