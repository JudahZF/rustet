//! JSON requests to the local engine (`engineRequest`): loopback HTTPS with the engine's own
//! self-signed certificate.

use std::future::Future;
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

/// An engine answer: the status and the body when it is JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineResponse {
    pub status: u16,
    pub json: Option<Value>,
}

/// Something that answers engine requests; `None` when the engine is unreachable. Tests use a
/// fake engine.
pub trait EngineApi: Send + Sync {
    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> impl Future<Output = Option<EngineResponse>> + Send;
}

/// The real engine at `https://127.0.0.1:<port>`.
#[derive(Debug, Clone)]
pub struct EngineClient {
    http: reqwest::Client,
    base: String,
}

impl EngineClient {
    /// A client for the engine on `port`. It has its own HTTP client: certificate checks are off,
    /// which is acceptable only because every request goes to the fixed loopback address. No
    /// proxy is used.
    pub fn new(port: u16) -> reqwest::Result<Self> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base: format!("https://127.0.0.1:{port}"),
        })
    }
}

impl EngineApi for EngineClient {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Option<EngineResponse> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(body) = &body {
            req = req.json(body);
        }
        let res = req.send().await.ok()?;
        let status = res.status().as_u16();
        let json = res
            .bytes()
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        Some(EngineResponse { status, json })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-request HTTPS server with a self-signed certificate, like the engine's. Answers with
    /// the request line and body it got.
    async fn engine_like_server() -> u16 {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = tokio_rustls::rustls::pki_types::PrivateKeyDer::try_from(
            cert.signing_key.serialize_der(),
        )
        .unwrap();
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = tls.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len = text[..head_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len || n == 0 {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            let (head, body) = text.split_once("\r\n\r\n").unwrap();
            let reply = serde_json::json!({ "line": head.lines().next().unwrap(), "body": body })
                .to_string();
            let res = format!(
                "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            tls.write_all(res.as_bytes()).await.unwrap();
            tls.shutdown().await.ok();
        });
        port
    }

    #[tokio::test]
    async fn talks_json_to_a_self_signed_loopback_engine() {
        let port = engine_like_server().await;
        let client = EngineClient::new(port).unwrap();
        let res = client
            .request(
                Method::POST,
                "/api/v1/drm/authenticate",
                Some(serde_json::json!({ "email": "a@b.c" })),
            )
            .await
            .unwrap();
        assert_eq!(res.status, 202);
        let json = res.json.unwrap();
        assert_eq!(json["line"], "POST /api/v1/drm/authenticate HTTP/1.1");
        assert_eq!(json["body"], r#"{"email":"a@b.c"}"#);
    }

    #[tokio::test]
    async fn an_unreachable_engine_is_none() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(
            EngineClient::new(port)
                .unwrap()
                .request(Method::GET, "/api/v1/drm/status", None)
                .await,
            None
        );
    }
}
