//! A fake HTTPS engine for the tests of the engine link and the models (`mod fake_engine;`):
//! wiremock behind a TLS terminator that serves the UI-made identity.
#![allow(dead_code)] // each test binary uses a different part

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rustet_app::engine::{EngineClient, EngineIdentity};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio_rustls::rustls;
use wiremock::{MockServer, Request, Respond, ResponseTemplate};

// ── A fake HTTPS engine ──────────────────────────────────────────────────────

/// wiremock behind TLS: the client sees an engine on `port` with `identity`.
pub struct FakeEngine {
    pub mock: MockServer,
    pub port: u16,
    pub identity: EngineIdentity,
}

impl FakeEngine {
    pub async fn start() -> Self {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let mock = MockServer::start().await;
        let identity = EngineIdentity::generate().unwrap();
        let pem = identity.pem().as_bytes();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(pem).unwrap()],
            PrivateKeyDer::from_pem_slice(pem).unwrap(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream = *mock.address();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let Ok(mut plain) = tokio::net::TcpStream::connect(upstream).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        Self {
            mock,
            port,
            identity,
        }
    }

    pub fn client(&self) -> EngineClient {
        EngineClient::new(self.port, self.identity.cert_der()).unwrap()
    }

    pub async fn requests(&self, p: &str) -> Vec<Request> {
        self.mock
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path() == p)
            .collect()
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub async fn wait_until<F: std::future::Future<Output = bool>>(
    what: &str,
    mut check: impl FnMut() -> F,
) {
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

pub fn sse(id: i64, event: &str, payload: Value) -> String {
    let data = json!({"meta": {"id": id, "generation": 1}, "payload": payload});
    format!("id: {id}\nevent: {event}\ndata: {data}\n\n")
}

/// Answers each connection with the next body; the stream then ends and the client
/// reconnects.
pub struct Bodies {
    pub bodies: Vec<String>,
    pub n: AtomicUsize,
}

impl Respond for Bodies {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let i = self.n.fetch_add(1, Ordering::SeqCst);
        let body = self.bodies.get(i).cloned().unwrap_or_default();
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }
}
