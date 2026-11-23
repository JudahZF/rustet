//! A mock Apple Music API for the request tests.
#![allow(dead_code)] // each test binary uses a different part

use rustet_ampapi::api::{ApiConfig, UserToken};
use rustet_ampapi::{Api, Client, DeveloperToken, Locale, WEB_ORIGIN};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

pub const EDGE: &str = "/edge";
pub const PLAY: &str = "/play";

pub struct Env {
    pub server: MockServer,
    pub api: Api,
}

/// An [`Api`] for the `us` storefront in `en-US` whose amp-api host is the mock server's
/// root, the edge host `/edge` and the play-activity URL `/play`. The developer token is
/// `dev`.
pub async fn env() -> Env {
    let server = MockServer::start().await;
    let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
    let config = ApiConfig {
        edge_base: format!("{}{EDGE}", server.uri()),
        play_activity_url: format!("{}{PLAY}", server.uri()),
        ..ApiConfig::default()
    };
    let api = Api::with_config(client, Locale::new("us", "en-US"), config);
    api.set_developer_token(DeveloperToken::new("dev")).await;
    Env { server, api }
}

/// [`env`] signed in with the user token `mut`.
pub async fn signed_in() -> Env {
    let env = env().await;
    env.api.set_user_token(Some(UserToken::new("mut")));
    env
}

pub fn fixture(name: &str) -> String {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The query, decoded, equals `pairs` in any order: nothing missing, nothing extra.
pub struct ExactQuery(Vec<(String, String)>);

pub fn query(pairs: &[(&str, &str)]) -> ExactQuery {
    let mut pairs: Vec<_> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    pairs.sort();
    ExactQuery(pairs)
}

impl Match for ExactQuery {
    fn matches(&self, request: &Request) -> bool {
        let mut got: Vec<_> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        got.sort();
        if got != self.0 {
            eprintln!("query mismatch:\n  got  {got:?}\n  want {:?}", self.0);
        }
        got == self.0
    }
}

/// The headers every call sends, and the user token exactly when `signed_in`.
pub struct Headers {
    pub user_token: bool,
}

impl Match for Headers {
    fn matches(&self, request: &Request) -> bool {
        let h = &request.headers;
        let is = |name: &str, value: &str| h.get(name).is_some_and(|v| v == value);
        let ok = is("authorization", "Bearer dev")
            && is("origin", WEB_ORIGIN)
            && h.get("user-agent")
                .is_some_and(|v| v.to_str().unwrap().starts_with("Mozilla/5.0"))
            && match self.user_token {
                true => is("media-user-token", "mut"),
                false => !h.contains_key("media-user-token"),
            };
        if !ok {
            eprintln!("header mismatch: {h:?}");
        }
        ok
    }
}

/// Expects exactly one `verb path?query` call with the standard headers.
pub fn expect(verb: &str, route: &str, q: ExactQuery, user_token: bool) -> wiremock::MockBuilder {
    Mock::given(method(verb))
        .and(path(route))
        .and(q)
        .and(Headers { user_token })
}

pub fn json(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .insert_header("cache-control", "max-age=60, public, no-transform")
        .set_body_string(body)
}
