//! The request guard: the outermost layer of every request.
//!
//! The API is unauthenticated on a fixed loopback port, so it rejects any request a foreign web
//! page could forge: a non-loopback `Host` (DNS rebinding), or a browser `Origin` outside the
//! allowlist (cross-site "simple" POSTs skip the CORS preflight entirely). Media elements and
//! the shell's main process send no `Origin` and pass. Other local users are kept out at the
//! socket ([`crate::peercred`]); `RUSTET_API_TOKEN` adds a shared secret for embedders.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::http::text_error;

/// The origin of the Apple Music page the renderer scripts run in.
const APPLE_ORIGIN: &str = "https://music.apple.com";

/// What the guard needs to know. Read from the environment once, at startup.
#[derive(Clone, Debug)]
pub(crate) struct GuardConfig {
    /// The engine's own port: its loopback origin is allowed.
    pub port: u16,
    /// `RUSTET_ALLOW_LOCAL_ORIGINS=1`: allow loopback origins on any port (local web tooling).
    pub allow_local_origins: bool,
    /// `RUSTET_API_TOKEN`: when set, every request except a preflight must present it.
    pub api_token: Option<String>,
}

impl GuardConfig {
    pub fn from_env(port: u16) -> Self {
        Self {
            port,
            allow_local_origins: std::env::var("RUSTET_ALLOW_LOCAL_ORIGINS")
                .is_ok_and(|v| v == "1"),
            api_token: std::env::var("RUSTET_API_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
        }
    }

    /// Exact match by scheme and host: a prefix test would also accept
    /// `http://localhost.attacker.example`. Apart from music.apple.com only loopback origins on
    /// the engine's own port pass, or any loopback port with `allow_local_origins`: otherwise any
    /// other web app on this machine could drive the API.
    pub fn is_allowed_origin(&self, origin: &str) -> bool {
        if origin == APPLE_ORIGIN {
            return true;
        }
        let Some(authority) = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
        else {
            return false;
        };
        // An origin is scheme://host[:port], nothing else. Browsers never send a path, user
        // info, query or fragment in it.
        if authority.contains(['/', '@', '?', '#']) {
            return false;
        }
        let (host, port) = split_host_port(authority).unwrap_or((authority, ""));
        if !is_loopback_name(host) || !port.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        self.allow_local_origins || port == self.port.to_string()
    }
}

/// Go's `net.SplitHostPort`: `host:port` or `[host]:port`. `None` when there is no port
/// separator, or when an unbracketed host has a colon.
fn split_host_port(hostport: &str) -> Option<(&str, &str)> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        return Some((host, after.strip_prefix(':')?));
    }
    let (host, port) = hostport.rsplit_once(':')?;
    (!host.contains(':')).then_some((host, port))
}

fn is_loopback_name(host: &str) -> bool {
    matches!(
        host.trim_matches(|c| c == '[' || c == ']'),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Whether a `Host` value (with or without port) names the loopback interface.
pub(crate) fn is_loopback_host(hostport: &str) -> bool {
    is_loopback_name(split_host_port(hostport).map_or(hostport, |(h, _)| h))
}

/// The request's host: the URI authority (HTTP/2 `:authority`, HTTP/1 absolute form) or the
/// `Host` header.
fn request_host(req: &Request) -> &str {
    req.uri()
        .authority()
        .map(|a| a.as_str())
        .or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
        })
        .unwrap_or("")
}

fn header_str(headers: &HeaderMap, name: impl header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The CORS response headers for a request with `origin`.
fn cors_headers(cfg: &GuardConfig, origin: Option<&str>, private_network: bool) -> HeaderMap {
    let mut h = HeaderMap::new();
    let allow = origin
        .filter(|o| cfg.is_allowed_origin(o))
        .and_then(|o| HeaderValue::from_str(o).ok())
        .unwrap_or(HeaderValue::from_static(APPLE_ORIGIN));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Range, Last-Event-ID, Cache-Control"),
    );
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Content-Type, Content-Length"),
    );
    // Chrome's Private Network Access: fetches across localhost ports need this on the preflight.
    if private_network {
        h.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
    // Responses carry catalog text (lyrics as XML or VTT, metadata): never let a browser sniff
    // them into an executable type.
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h
}

/// The token presented by a request: `X-Api-Token`, `Authorization: Bearer`, or
/// `?access_token=` (for media elements, which cannot set headers).
fn presented_token(req: &Request) -> String {
    let headers = req.headers();
    if let Some(t) = header_str(headers, "x-api-token").filter(|t| !t.is_empty()) {
        return t.to_owned();
    }
    if let Some(t) = header_str(headers, header::AUTHORIZATION)
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
    {
        return t.to_owned();
    }
    req.uri()
        .query()
        .and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "access_token"))
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// The middleware. Order: host check, origin check, CORS headers on everything that passes,
/// preflights answered here (before routing, so every path answers them), then the token check.
pub(crate) async fn guard(
    State(cfg): State<Arc<GuardConfig>>,
    req: Request,
    next: Next,
) -> Response {
    if !is_loopback_host(request_host(&req)) {
        return text_error(StatusCode::FORBIDDEN, "forbidden host");
    }
    let origin = header_str(req.headers(), header::ORIGIN).filter(|o| !o.is_empty());
    if origin.is_some_and(|o| !cfg.is_allowed_origin(o)) {
        return text_error(StatusCode::FORBIDDEN, "forbidden origin");
    }
    let private_network =
        header_str(req.headers(), "access-control-request-private-network") == Some("true");
    let cors = cors_headers(&cfg, origin, private_network);

    let mut resp = if req.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else if let Some(want) = &cfg.api_token
        && !bool::from(presented_token(&req).as_bytes().ct_eq(want.as_bytes()))
    {
        let mut r = text_error(StatusCode::UNAUTHORIZED, "unauthorized");
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Bearer realm="musickit-engine""#),
        );
        r
    } else {
        next.run(req).await
    };
    resp.headers_mut().extend(cors);
    resp
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::routing::any;
    use tower::ServiceExt;

    use super::*;

    fn cfg() -> GuardConfig {
        GuardConfig {
            port: 20025,
            allow_local_origins: false,
            api_token: None,
        }
    }

    fn app(cfg: GuardConfig) -> Router {
        Router::new()
            .route("/{*rest}", any(|| async { StatusCode::IM_A_TEAPOT }))
            .layer(axum::middleware::from_fn_with_state(Arc::new(cfg), guard))
    }

    async fn call(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> Response {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn cors_guard() {
        let app = app(cfg());
        let cases = [
            (
                "apple page",
                "POST",
                "127.0.0.1:20025",
                "https://music.apple.com",
                418,
            ),
            (
                "main process, no origin",
                "POST",
                "127.0.0.1:20025",
                "",
                418,
            ),
            ("localhost host", "GET", "localhost:20025", "", 418),
            ("ipv6 loopback", "GET", "[::1]:20025", "", 418),
            (
                "own loopback origin",
                "GET",
                "127.0.0.1:20025",
                "http://127.0.0.1:20025",
                418,
            ),
            (
                "other local web app",
                "GET",
                "127.0.0.1:20025",
                "http://localhost:5173",
                403,
            ),
            (
                "preflight allowed",
                "OPTIONS",
                "127.0.0.1:20025",
                "https://music.apple.com",
                204,
            ),
            (
                "cross-site simple POST",
                "POST",
                "127.0.0.1:20025",
                "https://attacker.example",
                403,
            ),
            (
                "localhost prefix spoof",
                "POST",
                "127.0.0.1:20025",
                "http://localhost.attacker.example",
                403,
            ),
            (
                "127 prefix spoof",
                "POST",
                "127.0.0.1:20025",
                "http://127.0.0.1.attacker.example",
                403,
            ),
            (
                "apple lookalike",
                "POST",
                "127.0.0.1:20025",
                "https://music.apple.com.attacker.example",
                403,
            ),
            (
                "file page (null origin)",
                "POST",
                "127.0.0.1:20025",
                "null",
                403,
            ),
            (
                "dns rebinding host",
                "GET",
                "rebind.attacker.example:20025",
                "",
                403,
            ),
            (
                "foreign preflight",
                "OPTIONS",
                "127.0.0.1:20025",
                "https://attacker.example",
                403,
            ),
        ];
        for (name, method, host, origin, want) in cases {
            let mut headers = vec![("host", host)];
            if !origin.is_empty() {
                headers.push(("origin", origin));
            }
            let resp = call(&app, method, "/api/v1/drm/logout", &headers).await;
            assert_eq!(resp.status().as_u16(), want, "{name}");
            if want != 403 && !origin.is_empty() {
                assert_eq!(
                    resp.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                    origin,
                    "{name}"
                );
            }
        }
    }

    #[tokio::test]
    async fn responses_are_nosniff_and_carry_cors() {
        let app = app(cfg());
        let resp = call(
            &app,
            "GET",
            "/api/v1/lyrics/1",
            &[("host", "127.0.0.1:20025")],
        )
        .await;
        assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(
            resp.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            APPLE_ORIGIN
        );
        assert!(
            resp.headers()
                .get("access-control-allow-private-network")
                .is_none()
        );
        let resp = call(
            &app,
            "OPTIONS",
            "/anything",
            &[
                ("host", "127.0.0.1:20025"),
                ("access-control-request-private-network", "true"),
            ],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers()["access-control-allow-private-network"],
            "true"
        );
    }

    #[test]
    fn allow_local_origins_opt_in() {
        let cfg = GuardConfig {
            allow_local_origins: true,
            ..cfg()
        };
        assert!(
            cfg.is_allowed_origin("http://localhost:5173"),
            "opt-in allows any loopback port"
        );
        assert!(
            !cfg.is_allowed_origin("http://localhost.attacker.example"),
            "still rejects lookalikes"
        );
    }

    #[test]
    fn origin_edge_cases() {
        let cfg = cfg();
        for (origin, want) in [
            ("https://127.0.0.1:20025", true),
            ("http://[::1]:20025", true),
            ("http://127.0.0.1:20025/", false),
            ("http://127.0.0.1", false),
            ("http://127.0.0.1:020025", false),
            ("http://user@127.0.0.1:20025", false),
            ("ftp://127.0.0.1:20025", false),
            ("https://music.apple.com/", false),
        ] {
            assert_eq!(cfg.is_allowed_origin(origin), want, "{origin}");
        }
    }

    #[test]
    fn loopback_host_forms() {
        for (host, want) in [
            ("127.0.0.1", true),
            ("localhost", true),
            ("::1", true),
            ("[::1]", true),
            ("[::1]:1", true),
            ("127.0.0.2:1", false),
            ("", false),
        ] {
            assert_eq!(is_loopback_host(host), want, "{host:?}");
        }
    }

    #[tokio::test]
    async fn require_token() {
        let guarded = app(GuardConfig {
            api_token: Some("s3cret".into()),
            ..cfg()
        });
        let host = ("host", "127.0.0.1:20025");
        for (name, method, uri, header, want) in [
            ("no token", "GET", "/x", None, 401),
            (
                "wrong token",
                "GET",
                "/x",
                Some(("authorization", "Bearer nope")),
                401,
            ),
            (
                "bearer",
                "GET",
                "/x",
                Some(("authorization", "Bearer s3cret")),
                418,
            ),
            (
                "x-api-token",
                "GET",
                "/x",
                Some(("x-api-token", "s3cret")),
                418,
            ),
            (
                "query (for media)",
                "GET",
                "/x?access_token=s3cret",
                None,
                418,
            ),
            ("preflight", "OPTIONS", "/x", None, 204),
            (
                "basic auth",
                "GET",
                "/x",
                Some(("authorization", "Basic s3cret")),
                401,
            ),
        ] {
            let headers: Vec<_> = [host].into_iter().chain(header).collect();
            let resp = call(&guarded, method, uri, &headers).await;
            assert_eq!(resp.status().as_u16(), want, "{name}");
            if want == 401 {
                assert_eq!(
                    resp.headers()[header::WWW_AUTHENTICATE],
                    r#"Bearer realm="musickit-engine""#
                );
            }
        }
        // No token configured: the check is off, not a lockout.
        let resp = call(&app(cfg()), "GET", "/x", &[host]).await;
        assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);
    }
}
