//! The Rustet engine: the local HTTPS API (127.0.0.1, self-signed TLS) that the renderer
//! scripts inside the Apple Music page call.
//!
//! [`run`] is the whole engine role: `rustet engine --api <port>`.
#![cfg_attr(not(windows), forbid(unsafe_code))]
// The Windows peer check calls Win32 (`peercred::windows`); nothing else is unsafe.
#![cfg_attr(windows, deny(unsafe_code))]

mod alacgate;
mod breaker;
mod epoch;
mod events;
mod guard;
mod handlers;
mod http;
mod peercred;
mod player;
mod routes;
mod server;
mod state;
mod stream;
mod tls;

#[cfg(test)]
mod tests;

use anyhow::Context;
use rustet_core::config::Config;
use tracing::{info, warn};

pub use tls::{TlsError, TlsIdentity};

/// How to run the engine.
#[derive(Debug, Clone)]
pub struct Options {
    /// The loopback port to serve on (the shell uses 20025).
    pub port: u16,
    /// Where the TLS identity comes from.
    pub tls: TlsSource,
}

/// Where the engine's TLS identity comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsSource {
    /// A new identity generated at start (the Electron shell trusts any loopback certificate).
    #[default]
    Generate,
    /// PEM read from standard input until it closes: a `CERTIFICATE` block and a PKCS#8
    /// `PRIVATE KEY` block, as [`TlsIdentity::to_pem`] writes them. The CEF shell generates the
    /// identity, so it can pin its key hash in Chromium before Chromium starts; the key never
    /// touches the disk, the environment or the command line.
    Stdin,
}

impl TlsSource {
    /// The identity: generated, or read from standard input.
    pub async fn identity(self) -> anyhow::Result<TlsIdentity> {
        match self {
            Self::Generate => TlsIdentity::generate().context("generate TLS identity"),
            Self::Stdin => {
                use tokio::io::AsyncReadExt;
                let mut pem = Vec::new();
                tokio::io::stdin()
                    .take(64 << 10)
                    .read_to_end(&mut pem)
                    .await
                    .context("read TLS identity from stdin")?;
                TlsIdentity::from_pem(&pem).context("TLS identity on stdin")
            }
        }
    }
}

/// Prepares the process for [`run`]. Call it first thing in `main`, before the tokio runtime
/// or any other thread exists: the FairPlay loader needs two environment variables, and
/// writing the environment is only sound in a single-threaded process.
pub fn prepare_process() {
    #[cfg(target_os = "linux")]
    {
        let cfg = Config::load(&Config::default_path()).unwrap_or_default();
        let dirs = state::DrmDirs::from_config(&cfg);
        if let Err(e) = rustet_drm::set_native_env_before_threads(&dirs.session_dir) {
            // Not fatal: the first FairPlay start then sets the variables itself.
            eprintln!("rustet engine: FairPlay environment not set early: {e}");
        }
    }
}

/// Runs the engine until SIGINT or SIGTERM (Ctrl+C, Ctrl+Break, console close, logoff or
/// shutdown on Windows), then shuts it down gracefully.
///
/// Fails when the server cannot start: another engine owns the session, the port is taken, or
/// TLS cannot be set up. The binary exits with code 1 then.
pub async fn run(options: Options) -> anyhow::Result<()> {
    init_logging();
    let path = Config::default_path();
    let cfg = Config::load(&path).unwrap_or_else(|e| {
        warn!("load config failed ({e}); using defaults");
        Config::default()
    });

    // Before anything else: a shell that writes the identity waits for nothing else.
    let identity = options.tls.identity().await?;
    let engine = state::Engine::build(options.port, &cfg).await?;
    let server = server::start(engine.clone(), routes::router(engine.clone()), &identity).await?;
    // After the session lock: a second engine never touches the snapshot or the decks.
    player::start_with_libvlc(&engine).await;
    shutdown_signal().await;
    info!("Shutting down API server");
    server.stop().await;
    Ok(())
}

/// Logs to stderr: info by default, debug with `RUSTET_ENGINE_DEBUG=1`. `RUST_LOG`, when set,
/// overrides both. Colours only on a terminal. Does nothing when a subscriber is already
/// installed.
pub fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let level = if std::env::var("RUSTET_ENGINE_DEBUG").is_ok_and(|v| v == "1") {
        "debug"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init();
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) else {
        warn!("cannot install signal handlers; stopping only on Ctrl+C");
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = int.recv() => {}
        _ = term.recv() => {}
    }
}

#[cfg(windows)]
async fn shutdown_signal() {
    use tokio::signal::windows;
    // Go maps Ctrl+C and Ctrl+Break to SIGINT, and console close, logoff and shutdown to SIGTERM.
    let (Ok(mut c), Ok(mut brk), Ok(mut close), Ok(mut logoff), Ok(mut shut)) = (
        windows::ctrl_c(),
        windows::ctrl_break(),
        windows::ctrl_close(),
        windows::ctrl_logoff(),
        windows::ctrl_shutdown(),
    ) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = c.recv() => {}
        _ = brk.recv() => {}
        _ = close.recv() => {}
        _ = logoff.recv() => {}
        _ = shut.recv() => {}
    }
}
