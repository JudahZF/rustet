//! One running Rustet per user (`requestSingleInstanceLock` and `second-instance`): a second
//! `rustet <link>` hands its arguments to the running app and exits.
//!
//! Unix: the first process holds an exclusive `flock` on `rustet.lock` for its whole life and
//! listens on the socket `rustet.sock`, both in the runtime directory. A later process that cannot
//! take the lock connects, sends its arguments as one JSON array, waits for `ok`, and exits.

#![cfg(unix)]

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{FlockOperation, flock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::platform::{Env, create_private_dir};

const LOCK_NAME: &str = "rustet.lock";
const SOCKET_NAME: &str = "rustet.sock";
/// Largest argument list accepted from another process.
const MAX_MESSAGE: u64 = 64 * 1024;
const ACK: &[u8] = b"ok\n";

/// Where the lock and the socket live: `$XDG_RUNTIME_DIR` (private to the user), else a private
/// `rustet-<uid>` folder in the temp directory.
pub fn runtime_dir(env: &Env) -> io::Result<PathBuf> {
    if let Some(dir) = env.get("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dir = std::env::temp_dir().join(format!("rustet-{}", rustix::process::getuid().as_raw()));
    create_private_dir(&dir)?;
    Ok(dir)
}

/// The outcome of [`acquire`].
pub enum Instance {
    /// This is the running app. Argument lists from later `rustet` processes arrive here.
    Primary(PrimaryInstance),
    /// Another Rustet runs and has this process's arguments: exit now.
    Forwarded,
}

/// The running app's side. Dropping it stops listening (the lock goes with the process).
pub struct PrimaryInstance {
    /// Each later process's command-line arguments (without the program name).
    pub args: mpsc::UnboundedReceiver<Vec<String>>,
    socket: PathBuf,
    task: JoinHandle<()>,
    _lock: File,
}

impl Drop for PrimaryInstance {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Becomes the running app, or forwards `args` to it.
pub async fn acquire(dir: &Path, args: &[String]) -> io::Result<Instance> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_NAME))?;
    let socket = dir.join(SOCKET_NAME);
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            // Left behind by a process that died: nobody listens on it.
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket)?;
            let (tx, rx) = mpsc::unbounded_channel();
            let task = tokio::spawn(serve(listener, tx));
            Ok(Instance::Primary(PrimaryInstance {
                args: rx,
                socket,
                task,
                _lock: lock,
            }))
        }
        Err(rustix::io::Errno::WOULDBLOCK) => {
            forward(&socket, args).await?;
            Ok(Instance::Forwarded)
        }
        Err(e) => Err(e.into()),
    }
}

async fn serve(listener: UnixListener, tx: mpsc::UnboundedSender<Vec<String>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = receive(stream, &tx).await {
                warn!("second instance: {e}");
            }
        });
    }
}

async fn receive(
    mut stream: UnixStream,
    tx: &mpsc::UnboundedSender<Vec<String>>,
) -> io::Result<()> {
    let mut raw = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        (&mut stream).take(MAX_MESSAGE + 1).read_to_end(&mut raw),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no arguments"))??;
    if raw.len() as u64 > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "argument list too large",
        ));
    }
    let args: Vec<String> =
        serde_json::from_slice(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let _ = tx.send(args);
    stream.write_all(ACK).await
}

/// Sends `args` to the running app. It may still be binding its socket: retry for a while.
async fn forward(socket: &Path, args: &[String]) -> io::Result<()> {
    let mut stream = None;
    for _ in 0..50 {
        match UnixStream::connect(socket).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let mut stream = stream.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotConnected,
            "the running Rustet does not answer",
        )
    })?;
    stream.write_all(&serde_json::to_vec(args)?).await?;
    stream.shutdown().await?;
    let mut ack = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        (&mut stream).take(16).read_to_end(&mut ack),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no answer from the running Rustet"))??;
    if ack != ACK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected answer from the running Rustet",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_instance_forwards_its_arguments() {
        let d = tempfile::tempdir().unwrap();
        let Instance::Primary(mut primary) = acquire(d.path(), &[]).await.unwrap() else {
            panic!("first must be primary")
        };
        let link = vec!["music://music.apple.com/us/album/x/1".to_owned()];
        assert!(matches!(
            acquire(d.path(), &link).await.unwrap(),
            Instance::Forwarded
        ));
        assert_eq!(primary.args.recv().await.unwrap(), link);
        let empty: Vec<String> = Vec::new();
        assert!(matches!(
            acquire(d.path(), &empty).await.unwrap(),
            Instance::Forwarded
        ));
        assert_eq!(primary.args.recv().await.unwrap(), empty);
        drop(primary);
        assert!(!d.path().join(SOCKET_NAME).exists());
        // With the first gone, the next one becomes primary even over a stale socket file.
        std::fs::write(d.path().join(SOCKET_NAME), b"").unwrap();
        assert!(matches!(
            acquire(d.path(), &[]).await.unwrap(),
            Instance::Primary(_)
        ));
    }

    #[tokio::test]
    async fn garbage_from_a_local_process_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let Instance::Primary(mut primary) = acquire(d.path(), &[]).await.unwrap() else {
            panic!()
        };
        let mut s = UnixStream::connect(d.path().join(SOCKET_NAME))
            .await
            .unwrap();
        s.write_all(b"{not json").await.unwrap();
        s.shutdown().await.unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest).await;
        assert!(rest.is_empty(), "no ack for garbage");
        assert!(matches!(
            acquire(d.path(), &["x".to_owned()]).await.unwrap(),
            Instance::Forwarded
        ));
        assert_eq!(primary.args.recv().await.unwrap(), ["x"]);
    }
}
