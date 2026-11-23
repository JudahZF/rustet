//! Dolby Atmos binaural rendering through CavernPipeServer.
//!
//! CavernPipeServer (<https://github.com/VoidXH/Cavern>, installed by
//! `scripts/install-cavern.sh`) reconstructs the JOC objects of an EC-3 + JOC bitstream and
//! renders them with its HeadphoneVirtualizer, so every Atmos object is rendered binaurally,
//! not only the 7.1 bed.
//!
//! Protocol, over the Unix socket `/tmp/CoreFxPipe_CavernPipe`:
//!
//! ```text
//! client → server: 8-byte header, then [i32 LE length][EC-3 bytes] chunks, then i32 -1 (EOF)
//! server → client: [i32 LE length][PCM bytes] chunks, then i32 -1 (EOS)
//! ```
//!
//! The PCM is raw interleaved float32 LE, 48 kHz, 2 channels. This stage writes a streaming
//! WAV header before it, so the output is a valid `audio/wav` stream that VLC can play.
//!
//! Cavern is Unix-only: on Windows [`is_available`] is false and binaural rendering uses the
//! FFmpeg path.
// The protocol code is only reached through the Unix socket.
#![cfg_attr(not(unix), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rustet_core::pipeline::{BoxError, PipelineError, Sink, Stage, StreamContext};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// The largest PCM chunk accepted. The length comes off the socket.
const MAX_CHUNK: usize = 64 << 20;

/// The end-of-stream marker in both directions: i32 -1.
const END_MARKER: [u8; 4] = (-1i32).to_le_bytes();

/// Input chunk size towards the server.
const FEED_CHUNK: usize = 4096;

/// The minimal Cavern `Save.dat` that turns on the HeadphoneVirtualizer with a stereo layout.
/// The deserializer reads one field per line: channel count, then (X°, Y°, LFE) per channel,
/// four environment fields, and the HeadphoneVirtualizer flag last.
const MINIMAL_SAVE_DAT: &str = "2\n0\n-30\nFalse\n0\n30\nFalse\n0\n10\n7\n10\nTrue\n";

/// An error of the Cavern stage.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cavern: CavernPipeServer not found")]
    NotFound,
    #[error("cavern: home dir not found")]
    NoHome,
    #[error("cavern: {}: {source}", path.display())]
    SaveDat { path: PathBuf, source: io::Error },
    #[error("cavern: start CavernPipeServer: {0}")]
    Start(io::Error),
    #[error("cavern: socket {SOCKET_PATH} did not appear within 5s")]
    NoSocket,
    #[error("cavern: send header: {0}")]
    SendHeader(io::Error),
    #[error("cavern: write WAV header: {0}")]
    WriteWavHeader(io::Error),
    #[error("cavern: send input: {0}")]
    Send(io::Error),
    #[error("cavern: inner source: {0}")]
    Input(io::Error),
    #[error("cavern: read chunk length: {0}")]
    ReadLength(io::Error),
    #[error("cavern: read chunk data: {0}")]
    ReadData(io::Error),
    #[error("cavern: chunk of {0} bytes exceeds the {MAX_CHUNK} byte limit")]
    ChunkTooLarge(usize),
    #[error("cavern: write output: {0}")]
    Output(io::Error),
    #[error("cavern: cancelled")]
    Cancelled,
}

impl From<Error> for PipelineError {
    fn from(e: Error) -> Self {
        match e {
            Error::Cancelled => Self::Cancelled,
            e => Self::Other(BoxError::from(e)),
        }
    }
}

const SOCKET_PATH: &str = "/tmp/CoreFxPipe_CavernPipe";

/// Whether CavernPipeServer is available: `$RUSTET_CAVERN`, then `CavernPipeServer` on `PATH`.
pub fn is_available() -> bool {
    binary().is_some()
}

#[cfg(unix)]
fn binary() -> Option<PathBuf> {
    binary_from(std::env::var_os("RUSTET_CAVERN"), std::env::var_os("PATH"))
}

#[cfg(not(unix))]
fn binary() -> Option<PathBuf> {
    None
}

#[cfg(unix)]
fn binary_from(
    env: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(env) = env.filter(|e| !e.is_empty()) {
        return Some(env.into());
    }
    std::env::split_paths(&path?)
        .map(|dir| dir.join("CavernPipeServer"))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Writes the minimal `~/.config/Cavern/Save.dat` when it is absent or its last line does not
/// turn the HeadphoneVirtualizer on. A user's file with the virtualizer on is left alone.
pub async fn ensure_hrtf() -> Result<(), Error> {
    ensure_hrtf_in(&dirs::home_dir().ok_or(Error::NoHome)?).await
}

async fn ensure_hrtf_in(home: &Path) -> Result<(), Error> {
    let dir = home.join(".config").join("Cavern");
    let path = dir.join("Save.dat");
    if let Ok(raw) = tokio::fs::read_to_string(&path).await
        && raw.trim().lines().last().map(str::trim) == Some("True")
    {
        return Ok(());
    }
    let save_dat_err = |source| Error::SaveDat {
        path: path.clone(),
        source,
    };
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(save_dat_err)?;
    tokio::fs::write(&path, MINIMAL_SAVE_DAT)
        .await
        .map_err(save_dat_err)?;
    tracing::info!(path = %path.display(), "cavern: HeadphoneVirtualizer enabled");
    Ok(())
}

/// Renders an EC-3 + JOC input to binaural float32 stereo WAV through CavernPipeServer. Each
/// stream starts its own server, which is killed when the stream is cancelled or the stage
/// returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct CavernStage;

#[async_trait]
impl Stage for CavernStage {
    async fn process(
        &self,
        ctx: &StreamContext,
        r: &mut (dyn AsyncRead + Send + Unpin),
        w: &mut dyn Sink,
    ) -> Result<(), PipelineError> {
        Ok(render(&ctx.cancel, r, w).await?)
    }
}

#[cfg(not(unix))]
async fn render(
    _: &CancellationToken,
    _: &mut (dyn AsyncRead + Send + Unpin),
    _: &mut dyn Sink,
) -> Result<(), Error> {
    Err(Error::NotFound)
}

#[cfg(unix)]
async fn render(
    cancel: &CancellationToken,
    r: &mut (dyn AsyncRead + Send + Unpin),
    w: &mut dyn Sink,
) -> Result<(), Error> {
    use std::process::Stdio;
    use std::time::Duration;

    if let Err(e) = ensure_hrtf().await {
        tracing::warn!(error = %e, "cavern: Save.dat setup failed, continuing anyway");
    }
    let bin = binary().ok_or(Error::NotFound)?;
    let _server = tokio::process::Command::new(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(Error::Start)?;

    // The server opens the socket asynchronously.
    let connect = async {
        loop {
            if let Ok(conn) = tokio::net::UnixStream::connect(SOCKET_PATH).await {
                return conn;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let conn = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::Cancelled),
        c = tokio::time::timeout(Duration::from_secs(5), connect) => c.map_err(|_| Error::NoSocket)?,
    };
    let (mut rd, mut wr) = conn.into_split();

    // BitDepth 32, mandatoryFrames 1, OutputChannels 2 (u16 LE), UpdateRate 1024 (i32 LE).
    let mut header = [32u8, 1, 0, 0, 0, 0, 0, 0];
    header[2..4].copy_from_slice(&2u16.to_le_bytes());
    header[4..8].copy_from_slice(&1024u32.to_le_bytes());
    wr.write_all(&header).await.map_err(Error::SendHeader)?;

    w.write_all(&streaming_wav_header())
        .await
        .map_err(Error::WriteWavHeader)?;

    let feed = feed_socket(r, &mut wr);
    tokio::pin!(feed);
    let read = read_socket(cancel, &mut rd, w);
    tokio::pin!(read);
    // A failed read ends the stream at once; the feed may be blocked on a server that
    // stopped reading.
    let mut fed = None;
    loop {
        tokio::select! {
            res = &mut read => {
                res?;
                break;
            }
            res = &mut feed, if fed.is_none() => fed = Some(res),
        }
    }
    match fed {
        Some(res) => res,
        None => feed.await,
    }
}

/// A 44-byte WAV header for 48 kHz stereo float32 with open-ended sizes (`0xFFFFFFFF`), so the
/// player treats the stream as unbounded.
pub(crate) fn streaming_wav_header() -> [u8; 44] {
    const SAMPLE_RATE: u32 = 48000;
    const CHANNELS: u16 = 2;
    const BITS: u16 = 32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE float
    h[22..24].copy_from_slice(&CHANNELS.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * u32::from(CHANNELS * BITS / 8)).to_le_bytes());
    h[32..34].copy_from_slice(&(CHANNELS * BITS / 8).to_le_bytes());
    h[34..36].copy_from_slice(&BITS.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    h
}

/// Forwards `r` to the server as length-prefixed chunks and ends with the EOF marker. The
/// marker is sent after an input error too, so the server can finish; the error is returned.
async fn feed_socket(
    r: &mut (dyn AsyncRead + Send + Unpin),
    conn: &mut (impl AsyncWrite + Unpin),
) -> Result<(), Error> {
    let mut buf = [0u8; FEED_CHUNK];
    let input = loop {
        match r.read(&mut buf).await {
            Ok(0) => break Ok(()),
            Ok(n) => {
                let len = u32::try_from(n).expect("chunk fits in u32").to_le_bytes();
                conn.write_all(&len).await.map_err(Error::Send)?;
                conn.write_all(&buf[..n]).await.map_err(Error::Send)?;
            }
            Err(e) => break Err(Error::Input(e)),
        }
    };
    let _ = conn.write_all(&END_MARKER).await;
    input
}

/// Copies the server's length-prefixed PCM chunks to `w` until the EOS marker.
async fn read_socket(
    cancel: &CancellationToken,
    conn: &mut (impl AsyncRead + Unpin),
    w: &mut (impl AsyncWrite + Unpin + ?Sized),
) -> Result<(), Error> {
    let copy = async {
        let mut data = Vec::new();
        loop {
            let mut len = [0u8; 4];
            conn.read_exact(&mut len).await.map_err(Error::ReadLength)?;
            // Any negative length is the end marker.
            let Ok(len) = usize::try_from(i32::from_le_bytes(len)) else {
                return Ok(());
            };
            if len > MAX_CHUNK {
                return Err(Error::ChunkTooLarge(len));
            }
            data.resize(len, 0);
            conn.read_exact(&mut data).await.map_err(Error::ReadData)?;
            w.write_all(&data).await.map_err(Error::Output)?;
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        res = copy => res,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn chunk(b: &[u8]) -> Vec<u8> {
        [&(b.len() as u32).to_le_bytes(), b].concat()
    }

    #[test]
    fn streaming_wav_header_fields() {
        let h = streaming_wav_header();
        let u16_at = |o: usize| u16::from_le_bytes([h[o], h[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        assert_eq!(
            (&h[0..4], &h[8..12], &h[36..40]),
            (&b"RIFF"[..], &b"WAVE"[..], &b"data"[..])
        );
        assert_eq!(
            (u16_at(20), u16_at(22), u32_at(24), u16_at(34)),
            (3, 2, 48000, 32)
        );
        assert_eq!((u32_at(28), u16_at(32)), (48000 * 2 * 4, 8));
        assert_eq!(
            (u32_at(4), u32_at(40)),
            (u32::MAX, u32::MAX),
            "open-ended for a live stream"
        );
    }

    #[tokio::test]
    async fn read_socket_copies_chunks_until_eos() {
        let wire = [
            chunk(b"abc"),
            chunk(b"defg"),
            END_MARKER.to_vec(),
            b"ignored".to_vec(),
        ]
        .concat();
        let mut out = Vec::new();
        read_socket(&CancellationToken::new(), &mut wire.as_slice(), &mut out)
            .await
            .unwrap();
        assert_eq!(out, b"abcdefg");
    }

    #[tokio::test]
    async fn read_socket_errors() {
        let cancel = CancellationToken::new();
        let cases: [(&str, Vec<u8>); 3] = [
            ("truncated length", vec![1, 0]),
            ("truncated data", [&10u32.to_le_bytes()[..], b"x"].concat()),
            (
                "oversized chunk",
                ((MAX_CHUNK + 1) as u32).to_le_bytes().to_vec(),
            ),
        ];
        for (name, wire) in cases {
            let res = read_socket(&cancel, &mut wire.as_slice(), &mut tokio::io::sink()).await;
            assert!(res.is_err(), "{name}: want an error");
        }

        cancel.cancel();
        let (mut idle, _keep) = tokio::io::duplex(8);
        let res = read_socket(&cancel, &mut idle, &mut tokio::io::sink()).await;
        assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    }

    #[tokio::test]
    async fn feed_socket_frames_input_and_ends_with_eof() {
        let payload = vec![7u8; 10_000]; // more than one chunk
        let mut wire = Vec::new();
        feed_socket(&mut payload.as_slice(), &mut wire)
            .await
            .unwrap();
        let mut got = Vec::new();
        let mut rest = wire.as_slice();
        loop {
            let n = i32::from_le_bytes(rest[..4].try_into().unwrap());
            rest = &rest[4..];
            let Ok(n) = usize::try_from(n) else { break };
            got.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
        }
        assert_eq!(got, payload);
        assert!(rest.is_empty(), "{} trailing bytes", rest.len());
    }

    /// Yields some bytes, then fails, like a pipeline input whose source failed.
    struct Failing(bool);

    impl AsyncRead for Failing {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if std::mem::replace(&mut self.0, true) {
                Poll::Ready(Err(io::Error::other("cdn failed")))
            } else {
                buf.put_slice(b"x");
                Poll::Ready(Ok(()))
            }
        }
    }

    #[tokio::test]
    async fn feed_socket_reports_input_error_after_eof_marker() {
        let mut wire = Vec::new();
        let err = feed_socket(&mut Failing(false), &mut wire)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Input(e) if e.to_string() == "cdn failed"),
            "{err:?}"
        );
        assert_eq!(wire, [chunk(b"x"), END_MARKER.to_vec()].concat());
    }

    #[tokio::test]
    async fn ensure_hrtf_enables_headphone_virtualizer() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".config/Cavern/Save.dat");

        ensure_hrtf_in(home.path()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            MINIMAL_SAVE_DAT,
            "absent file"
        );

        std::fs::write(&path, "2\n0\nFalse\n").unwrap();
        ensure_hrtf_in(home.path()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            MINIMAL_SAVE_DAT,
            "a Save.dat with the virtualizer off must be rewritten"
        );

        let custom = "6\n1\n2\nTrue\n";
        std::fs::write(&path, custom).unwrap();
        ensure_hrtf_in(home.path()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            custom,
            "a user's Save.dat with the virtualizer on must be left alone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn binary_lookup() {
        let cavern = "/opt/cavern/CavernPipeServer";
        let empty = tempfile::tempdir().unwrap();
        let empty_path = Some(empty.path().as_os_str().to_owned());
        assert_eq!(
            binary_from(Some(cavern.into()), empty_path.clone()),
            Some(PathBuf::from(cavern)),
            "$RUSTET_CAVERN takes precedence"
        );
        assert_eq!(binary_from(Some("".into()), empty_path.clone()), None);

        let bin = empty.path().join("CavernPipeServer");
        std::fs::write(&bin, "").unwrap();
        assert_eq!(
            binary_from(None, empty_path.clone()),
            None,
            "not executable"
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(binary_from(None, empty_path), Some(bin));
    }
}
