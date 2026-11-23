//! The HTTPS listener on 127.0.0.1: the accept loop, the per-user check, TLS, HTTP/1.1 and
//! HTTP/2, and the graceful shutdown.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use rustet_drm::SessionLock;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::peercred::PeerGuard;
use crate::state::AppState;
use crate::tls::TlsIdentity;

/// Bounds the TLS handshake and the request headers (Go's `ReadHeaderTimeout`).
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest request header block.
const MAX_HEADER_BYTES: usize = 64 << 10;
/// How long a shutdown waits for open requests.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// A running server. [`Server::stop`] shuts it down.
pub(crate) struct Server {
    engine: AppState,
    accept: JoinHandle<GracefulShutdown>,
    /// Held while the engine runs; released last.
    session_lock: Option<SessionLock>,
}

/// Takes the session lock, binds the listener and serves in the background. Fails when
/// another engine owns the session, so a second instance refuses to run.
pub(crate) async fn start(
    engine: AppState,
    router: Router,
    identity: &TlsIdentity,
) -> anyhow::Result<Server> {
    let session_lock = engine.acquire_session_lock()?;
    let port = engine.guard.port;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("listen on 127.0.0.1:{port}"))?;
    serve(
        engine,
        router,
        listener,
        PeerGuard::from_env(),
        session_lock,
        identity,
    )
}

/// Serves `router` on `listener`. Split from [`start`] for tests.
pub(crate) fn serve(
    engine: AppState,
    router: Router,
    listener: TcpListener,
    peers: PeerGuard,
    session_lock: Option<SessionLock>,
    identity: &TlsIdentity,
) -> anyhow::Result<Server> {
    // DRM work that needs the session lock: the snapshot watcher and the eager start.
    engine.start_drm();
    let tls = identity.server_config().context("TLS setup failed")?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let addr = listener.local_addr()?;
    info!(addr = %format!("https://{addr}"), "Apple Music API (TLS) ready");

    let cancel = engine.shutdown.clone();
    let accept = tokio::spawn(async move {
        let graceful = GracefulShutdown::new();
        loop {
            let (tcp, client) = tokio::select! {
                () = cancel.cancelled() => break,
                r = listener.accept() => match r {
                    Ok(c) => c,
                    Err(e) => {
                        // Out of file descriptors and the like: back off instead of spinning.
                        warn!("accept: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                },
            };
            let (watcher, acceptor, router, peers) = (
                graceful.watcher(),
                acceptor.clone(),
                router.clone(),
                peers.clone(),
            );
            tokio::spawn(async move {
                let Ok(server) = tcp.local_addr() else { return };
                let check = tokio::task::spawn_blocking(move || peers.admits(client, server));
                if !check.await.unwrap_or(false) {
                    return; // dropping the stream closes the refused connection
                }
                let tls = match tokio::time::timeout(HEADER_TIMEOUT, acceptor.accept(tcp)).await {
                    Ok(Ok(tls)) => tls,
                    // Local clients (reachability probes, aborted media requests) close
                    // mid-handshake all the time: not worth a warning.
                    Ok(Err(e)) => return debug!("TLS handshake with {client}: {e}"),
                    Err(_) => return debug!("TLS handshake with {client} timed out"),
                };
                let mut http = auto::Builder::new(TokioExecutor::new());
                http.http1()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_TIMEOUT)
                    .max_buf_size(MAX_HEADER_BYTES);
                http.http2()
                    .timer(TokioTimer::new())
                    .max_header_list_size(MAX_HEADER_BYTES as u32);
                let conn = http.serve_connection_with_upgrades(
                    TokioIo::new(tls),
                    TowerToHyperService::new(router),
                );
                if let Err(e) = watcher.watch(conn.into_owned()).await {
                    debug!("connection from {client}: {e}");
                }
            });
        }
        graceful
    });
    Ok(Server {
        engine,
        accept,
        session_lock,
    })
}

impl Server {
    /// Stops accepting, ends SSE streams, stops the scheduler and the DRM backend, waits up to
    /// five seconds for open requests, then releases the session lock.
    pub async fn stop(self) {
        self.engine.stop().await;
        match self.accept.await {
            Ok(graceful) => {
                if tokio::time::timeout(SHUTDOWN_WAIT, graceful.shutdown())
                    .await
                    .is_err()
                {
                    warn!("shutdown: requests still open after {SHUTDOWN_WAIT:?}");
                }
            }
            Err(e) => warn!("accept loop: {e}"),
        }
        drop(self.session_lock);
    }
}
