//! Test servers: a minimal HTTP file server and a fake FairPlay decryptor.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serves `body` with `status` to every request after `delay`. Returns the URL and a
/// request counter.
pub async fn http_server(
    status: u16,
    body: Vec<u8>,
    delay: Duration,
) -> (String, Arc<AtomicUsize>) {
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file.mp4", ln.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let body = Arc::new(body);
    tokio::spawn(async move {
        while let Ok((mut c, _)) = ln.accept().await {
            let body = body.clone();
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                tokio::time::sleep(delay).await;
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(head.as_bytes()).await;
                let _ = c.write_all(&body).await;
                let _ = c.shutdown().await;
            });
        }
    });
    (url, calls)
}

/// A key the fake decryptor can open: AES-128 key and constant IV.
#[derive(Clone, Copy)]
pub struct Key {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

/// Speaks the wrapper's `handle()` protocol and decrypts each packet the way FairPlay does:
/// AES-128-CBC from the constant IV. A key it does not know, or a wrong adamID for it, drops
/// the connection.
pub struct FakeFairPlay {
    pub addr: String,
    opened: Arc<Mutex<Vec<String>>>,
    pub connections: Arc<AtomicUsize>,
}

impl FakeFairPlay {
    /// `drop_first_after`: the first connection is dropped after that many packets.
    pub async fn start(
        adam_id: &'static str,
        prefetch_key: &'static str,
        keys: HashMap<String, Key>,
        drop_first_after: Option<usize>,
    ) -> Self {
        let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap().to_string();
        let opened = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let (log, conns) = (opened.clone(), connections.clone());
        let keys = Arc::new(keys);
        tokio::spawn(async move {
            while let Ok((c, _)) = ln.accept().await {
                let n = conns.fetch_add(1, Ordering::SeqCst);
                let limit = if n == 0 { drop_first_after } else { None };
                tokio::spawn(handle(
                    c,
                    adam_id,
                    prefetch_key,
                    keys.clone(),
                    log.clone(),
                    limit,
                ));
            }
        });
        Self {
            addr,
            opened,
            connections,
        }
    }

    /// `"adamID uri"` for every key opened, in order.
    pub fn opened(&self) -> Vec<String> {
        self.opened.lock().unwrap().clone()
    }
}

async fn read_short<R: AsyncRead + Unpin>(r: &mut R) -> Option<String> {
    let n = r.read_u8().await.ok()?;
    let mut b = vec![0; usize::from(n)];
    r.read_exact(&mut b).await.ok()?;
    String::from_utf8(b).ok()
}

async fn handle(
    mut c: TcpStream,
    adam_id: &str,
    prefetch_key: &str,
    keys: Arc<HashMap<String, Key>>,
    log: Arc<Mutex<Vec<String>>>,
    limit: Option<usize>,
) {
    let mut packets = 0;
    loop {
        let Some(rustet) = read_short(&mut c).await.filter(|a| !a.is_empty()) else {
            return;
        };
        let Some(uri) = read_short(&mut c).await else {
            return;
        };
        log.lock().unwrap().push(format!("{rustet} {uri}"));
        let want = if uri == prefetch_key { "0" } else { adam_id };
        let Some(key) = keys.get(&uri).filter(|_| rustet == want) else {
            return; // handle(): no key context, drop the connection
        };
        loop {
            let Ok(n) = c.read_u32_le().await else { return };
            if n == 0 {
                break;
            }
            let mut buf = vec![0; n as usize];
            if c.read_exact(&mut buf).await.is_err() {
                return;
            }
            if limit.is_some_and(|l| packets >= l) {
                return;
            }
            packets += 1;
            let mut dec = cbc::Decryptor::<aes::Aes128>::new(&key.key.into(), &key.iv.into());
            for block in buf.as_chunks_mut::<16>().0 {
                dec.decrypt_block(block.into());
            }
            if c.write_all(&buf).await.is_err() {
                return;
            }
        }
    }
}
