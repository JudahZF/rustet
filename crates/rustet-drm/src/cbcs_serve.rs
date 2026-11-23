//! The FairPlay sample protocol a CBCS decryption session speaks.

use std::io::{self, Read, Write};

use tokio_util::sync::CancellationToken;

use crate::backend::BackendError;

/// The largest sample accepted. Real samples are well under a megabyte; the size comes off the
/// wire as a `u32` that would otherwise allocate up to 4 GiB.
pub const MAX_CBCS_SAMPLE: u32 = 64 << 20;

/// Why a CBCS session ended with an error.
#[derive(Debug, thiserror::Error)]
pub enum CbcsError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("CBCS session cancelled")]
    Cancelled,
    #[error("CBCS sample of {0} bytes exceeds the {MAX_CBCS_SAMPLE} byte limit")]
    SampleTooLarge(u32),
    #[error(transparent)]
    OpenKey(BackendError),
    #[error("decrypt CBCS sample: {0}")]
    Decrypt(BackendError),
}

/// Serves one CBCS session on `conn`: the protocol of `handle()` in the native wrapper's
/// `main.c`, with key opening and sample decryption injected.
///
/// ```text
/// repeat:  [u8 len][adamID] [u8 len][keyURI]            select a key
///          ([u32 LE size][sample] → [decrypted sample])*  decrypt samples
///          [u32 LE 0]                                    end of this key
/// ```
///
/// A zero size ends the current key, not the connection. The session ends cleanly when the
/// client closes the connection at a record boundary (where an adamID or a size would start;
/// EOF inside a record is an error), or when an adamID is empty. It ends with an error when a
/// key cannot be opened or a sample cannot be decrypted.
///
/// The calls block: run it on its own thread (for an async stream, through
/// [`tokio_util::io::SyncIoBridge`] in `spawn_blocking`). `cancel` is checked before each key
/// and each sample.
pub fn serve_cbcs<C, K, D>(
    cancel: &CancellationToken,
    conn: &mut C,
    mut open_key: K,
) -> Result<(), CbcsError>
where
    C: Read + Write,
    K: FnMut(&str, &str) -> Result<D, BackendError>,
    D: FnMut(&mut [u8]) -> Result<(), BackendError>,
{
    loop {
        let mut len = [0u8; 1];
        if !read_or_eof(conn, &mut len)? || len[0] == 0 {
            return Ok(()); // client finished, or an empty adamID (handle(): rustetSize <= 0)
        }
        let adam_id = read_bytes(conn, len[0].into())?;
        conn.read_exact(&mut len)?;
        let uri = read_bytes(conn, len[0].into())?;
        if cancel.is_cancelled() {
            return Err(CbcsError::Cancelled);
        }
        let mut decrypt = open_key(
            &String::from_utf8_lossy(&adam_id),
            &String::from_utf8_lossy(&uri),
        )
        .map_err(CbcsError::OpenKey)?;
        loop {
            if cancel.is_cancelled() {
                return Err(CbcsError::Cancelled);
            }
            let mut size = [0u8; 4];
            if !read_or_eof(conn, &mut size)? {
                return Ok(()); // client closed after its last sample
            }
            let size = u32::from_le_bytes(size);
            if size == 0 {
                break; // key switch: the next header selects a new key
            }
            if size > MAX_CBCS_SAMPLE {
                return Err(CbcsError::SampleTooLarge(size));
            }
            let mut sample = read_bytes(conn, size as usize)?;
            decrypt(&mut sample).map_err(CbcsError::Decrypt)?;
            conn.write_all(&sample)?;
            conn.flush()?;
        }
    }
}

fn read_bytes(r: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Fills `buf`, or returns `false` when the stream ended before its first byte. An end after
/// some bytes is an `UnexpectedEof` error.
fn read_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Serves a fixed input and records what is written back.
    struct Script {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            Read::read(&mut self.input, buf)
        }
    }

    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Write::write(&mut self.output, buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn record(rustet: &str, uri: &str, samples: &[&[u8]]) -> Vec<u8> {
        let mut b = vec![rustet.len() as u8];
        b.extend_from_slice(rustet.as_bytes());
        b.push(uri.len() as u8);
        b.extend_from_slice(uri.as_bytes());
        for s in samples {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s);
        }
        b.extend_from_slice(&[0; 4]); // size 0: end of this key
        b
    }

    type Decrypt = Box<dyn FnMut(&mut [u8]) -> Result<(), BackendError>>;

    fn xor_key(_: &str, _: &str) -> Result<Decrypt, BackendError> {
        Ok(Box::new(|s: &mut [u8]| {
            s.iter_mut().for_each(|b| *b ^= 0xFF);
            Ok(())
        }))
    }

    fn run(
        input: Vec<u8>,
        open_key: impl FnMut(&str, &str) -> Result<Decrypt, BackendError>,
    ) -> (Vec<u8>, Result<(), CbcsError>) {
        let mut conn = Script {
            input: Cursor::new(input),
            output: Vec::new(),
        };
        let result = serve_cbcs(&CancellationToken::new(), &mut conn, open_key);
        (conn.output, result)
    }

    #[test]
    fn a_key_switch_keeps_the_connection_and_uses_each_key() {
        let mut opened = Vec::new();
        let mut input = record("111", "uri-a", &[&[1, 2, 3]]);
        input.extend(record("222", "uri-b", &[&[9]]));
        let (out, result) = run(input, |rustet, uri| {
            opened.push(rustet.to_owned());
            xor_key(rustet, uri)
        });
        result.unwrap();
        assert_eq!(opened, ["111", "222"]);
        assert_eq!(out, [0xFE, 0xFD, 0xFC, 0xF6]);
    }

    #[test]
    fn an_empty_adam_id_ends_the_session() {
        let (out, result) = run(vec![0], xor_key);
        assert!(result.is_ok() && out.is_empty());
    }

    // EOF is a clean end only at a record boundary: where the next adamID or the next sample's
    // size would start. Anywhere else the record is truncated.
    #[test]
    fn truncated_input_is_an_error_not_a_clean_end() {
        let good = record("111", "uri", &[&[1, 2, 3, 4]]);
        let (after_header, after_sample) = (8, 16);
        for cut in 1..good.len() {
            let (_, result) = run(good[..cut].to_vec(), xor_key);
            let legal = cut == after_header || cut == after_sample;
            assert_eq!(
                result.is_ok(),
                legal,
                "prefix {cut}/{}: {result:?}",
                good.len()
            );
        }
        assert!(
            run(Vec::new(), xor_key).1.is_ok(),
            "an empty stream is a clean end"
        );
        assert!(run(good, xor_key).1.is_ok());
    }

    #[test]
    fn a_key_open_failure_ends_the_session() {
        let (_, result) = run(record("1", "u", &[&[1]]), |_, _| {
            Err(BackendError::Failed("no key".into()))
        });
        assert!(
            matches!(result, Err(CbcsError::OpenKey(BackendError::Failed(m))) if m == "no key")
        );
    }

    #[test]
    fn an_oversized_sample_is_refused_before_allocating() {
        let mut input = vec![1, b'a', 1, b'u'];
        input.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            run(input, xor_key).1,
            Err(CbcsError::SampleTooLarge(u32::MAX))
        ));
    }

    #[test]
    fn a_cancelled_session_stops() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut conn = Script {
            input: Cursor::new(record("1", "u", &[&[1]])),
            output: Vec::new(),
        };
        assert!(matches!(
            serve_cbcs(&cancel, &mut conn, xor_key),
            Err(CbcsError::Cancelled)
        ));
    }

    // Whatever bytes arrive, the session halts without panicking.
    #[test]
    fn random_bytes_never_panic_or_hang() {
        let mut seed: u64 = 7;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for i in 0..3000 {
            let mut input: Vec<u8> = (0..next() % 300).map(|_| next() as u8).collect();
            if i % 3 == 0 && input.len() > 8 {
                input[0] = 1 + (next() % 3) as u8; // plausible headers reach the deeper states
            }
            let _ = run(input, xor_key);
        }
    }

    // How a backend serves an in-process stream: the blocking protocol on a blocking thread,
    // bridged to the async pipe the client holds.
    #[tokio::test]
    async fn serves_an_async_pipe_through_a_bridge() {
        let (mut client, server) = tokio::io::duplex(64);
        let server = tokio::task::spawn_blocking(move || {
            let mut conn = tokio_util::io::SyncIoBridge::new(server);
            let mut opened = 0u8;
            let result = serve_cbcs(&CancellationToken::new(), &mut conn, |_, _| {
                opened += 1;
                let k = opened; // key 1 XORs with 1, key 2 with 2
                Ok(move |s: &mut [u8]| {
                    s.iter_mut().for_each(|b| *b ^= k);
                    Ok(())
                })
            });
            (result, opened)
        });

        async fn round_trip(c: &mut tokio::io::DuplexStream, sample: &[u8]) -> Vec<u8> {
            c.write_all(&(sample.len() as u32).to_le_bytes())
                .await
                .unwrap();
            c.write_all(sample).await.unwrap();
            let mut out = vec![0; sample.len()];
            c.read_exact(&mut out).await.unwrap();
            out
        }
        client.write_all(b"\x010\x0eskd://prefetch").await.unwrap();
        assert_eq!(round_trip(&mut client, &[0, 0]).await, [1, 1]);
        client.write_all(&[0; 4]).await.unwrap(); // switch keys
        client
            .write_all(b"\x0a1440833098\x0askd://real")
            .await
            .unwrap();
        assert_eq!(
            round_trip(&mut client, &[0, 0]).await,
            [2, 2],
            "second key not applied after the switch"
        );
        drop(client);

        let (result, opened) = server.await.unwrap();
        result.unwrap();
        assert_eq!(opened, 2);
    }
}
