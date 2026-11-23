//! A mock Apple Music API for the page and library tests (`mod pages_common;`).
#![allow(dead_code)] // each test binary uses a different part

use rustet_ampapi::api::{ApiConfig, UserToken};
use rustet_ampapi::{Api, Client, DeveloperToken, Locale};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The edge host's path prefix on the mock server.
pub const EDGE: &str = "/edge";

pub struct Env {
    pub server: MockServer,
    pub api: Api,
}

/// An [`Api`] for `us`/`en-US` on a mock server (edge host under `/edge`), developer token
/// `dev`, signed out.
pub async fn env() -> Env {
    let server = MockServer::start().await;
    let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
    let config = ApiConfig {
        edge_base: format!("{}{EDGE}", server.uri()),
        play_activity_url: format!("{}/play", server.uri()),
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

pub fn json(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_string(body.into())
}

impl Env {
    /// Answers `GET route` with `body`.
    pub async fn get(&self, route: &str, body: impl Into<String>) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(json(body))
            .mount(&self.server)
            .await;
    }
}
