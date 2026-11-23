//! Opening FairPlay decryption connections.

use std::io;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

/// A byte stream to the FairPlay decryptor.
pub trait DrmIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> DrmIo for T {}

/// One decryption connection: in production one end of a [`tokio::io::duplex`] served by the
/// DRM backend, in tests a TCP stream.
pub type DrmStream = Box<dyn DrmIo>;

/// Opens FairPlay decryption connections for the CBCS source. In production the DRM manager
/// implements it. It lives here so this crate does not depend on the DRM crate.
#[async_trait]
pub trait CbcsDialer: Send + Sync {
    /// Opens one decryption session. The caller owns the connection, speaks the protocol of
    /// [`crate::alac`] on it and closes it after the last fragment.
    async fn dial_cbcs(&self, cancel: &CancellationToken) -> io::Result<DrmStream>;

    /// Whether connections are in-process pipes rather than sockets to another process. An
    /// in-process pipe cannot go stale from idleness, so it is not refreshed when idle.
    fn in_process(&self) -> bool {
        false
    }

    /// Asks the DRM backend for fresh keys, after a key decrypted to garbage (the backend
    /// gave an expired key as valid). The next dial then opens new key contexts.
    async fn refresh_keys(&self) -> io::Result<()> {
        Ok(())
    }
}

/// A [`CbcsDialer`] that connects to a TCP address (Go `DialerFromAddr`). Tests use it to
/// drive the stream against a fake decryption server.
#[derive(Debug, Clone)]
pub struct TcpDialer(pub String);

#[async_trait]
impl CbcsDialer for TcpDialer {
    async fn dial_cbcs(&self, cancel: &CancellationToken) -> io::Result<DrmStream> {
        let conn = tokio::select! {
            () = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
            r = TcpStream::connect(&self.0) => r,
        };
        match conn {
            Ok(c) => Ok(Box::new(c)),
            Err(e) => Err(io::Error::new(
                e.kind(),
                format!("cbcs dial {}: {e}", self.0),
            )),
        }
    }
}
