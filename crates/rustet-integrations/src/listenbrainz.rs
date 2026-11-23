//! ListenBrainz scrobbling. Simpler than Last.fm: one user token (from
//! <https://listenbrainz.org/settings/>) in an `Authorization: Token …` header, no signing.
//! The token lives only in the encrypted store.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use url::Url;

use crate::{
    lastfm::Ack,
    scrobble::ScrobbleTrack,
    store::{SecretStore, get_set, js_len},
};

/// ListenBrainz API root.
pub const API_ROOT: &str = "https://api.listenbrainz.org";

const KEY_TOKEN: &str = "lb_token";
const TOKEN_MAX: usize = 256;

/// Reply of `lb:status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LbStatus {
    pub connected: bool,
    /// The user the token belongs to, when ListenBrainz said so.
    pub username: Option<String>,
}

/// The ListenBrainz client.
pub struct ListenBrainz {
    http: reqwest::Client,
    store: Arc<dyn SecretStore>,
    root: Url,
}

/// `track_metadata` of a listen: `release_name` only when there is an album.
fn track_metadata(t: &ScrobbleTrack) -> Value {
    let mut md = json!({ "artist_name": t.artist, "track_name": t.track });
    if let Some(album) = t.album() {
        md["release_name"] = album.into();
    }
    md
}

impl ListenBrainz {
    pub fn new(http: reqwest::Client, store: Arc<dyn SecretStore>) -> Self {
        Self::with_root(http, store, Url::parse(API_ROOT).expect("valid URL"))
    }

    /// A client for another API root (tests).
    pub fn with_root(http: reqwest::Client, store: Arc<dyn SecretStore>, root: Url) -> Self {
        Self { http, store, root }
    }

    fn endpoint(&self, path: &str) -> Url {
        self.root.join(path).expect("valid API path")
    }

    /// `lb:set-token`: a string of at most 256 characters, or null to clear.
    pub fn set_token(&self, token: &Value) -> Ack {
        let token = match token {
            Value::Null => "",
            Value::String(s) if js_len(s) <= TOKEN_MAX => s.trim(),
            _ => return Ack { ok: false },
        };
        self.store.set(KEY_TOKEN, token.to_owned());
        Ack { ok: true }
    }

    /// `lb:status`: validates the token to learn the user name. When ListenBrainz cannot be
    /// reached the token is assumed good.
    pub async fn status(&self) -> LbStatus {
        let Some(token) = get_set(&*self.store, KEY_TOKEN) else {
            return LbStatus {
                connected: false,
                username: None,
            };
        };
        let reply: Result<Value, reqwest::Error> = async {
            self.http
                .get(self.endpoint("/1/validate-token"))
                .header("Authorization", format!("Token {token}"))
                .send()
                .await?
                .json()
                .await
        }
        .await;
        match reply {
            Ok(j) => LbStatus {
                connected: j.get("valid").is_some_and(truthy),
                username: j
                    .get("user_name")
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                    .map(str::to_owned),
            },
            Err(e) => {
                tracing::debug!("ListenBrainz token check failed: {e}");
                LbStatus {
                    connected: true,
                    username: None,
                }
            }
        }
    }

    /// `lb:disconnect`.
    pub fn disconnect(&self) -> Ack {
        self.store.delete(KEY_TOKEN);
        Ack { ok: true }
    }

    /// `lb:nowplaying`. Failures are logged and dropped.
    pub async fn now_playing(&self, t: &ScrobbleTrack) {
        if t.is_complete() {
            self.submit(json!({ "listen_type": "playing_now", "payload": [{ "track_metadata": track_metadata(t) }] }))
                .await;
        }
    }

    /// `lb:scrobble`. Needs the start timestamp. Failures are logged and dropped.
    pub async fn scrobble(&self, t: &ScrobbleTrack) {
        let Some(ts) = t.timestamp().filter(|_| t.is_complete()) else {
            return;
        };
        self.submit(json!({
            "listen_type": "single",
            "payload": [{ "listened_at": ts, "track_metadata": track_metadata(t) }],
        }))
        .await;
    }

    async fn submit(&self, body: Value) {
        let Some(token) = get_set(&*self.store, KEY_TOKEN) else {
            return;
        };
        let sent = self
            .http
            .post(self.endpoint("/1/submit-listens"))
            .header("Authorization", format!("Token {token}"))
            .json(&body)
            .send()
            .await;
        if let Err(e) = sent {
            tracing::debug!("ListenBrainz submit failed: {e}");
        }
    }
}

/// JavaScript truthiness of a JSON value (`!!j.valid`).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    fn track(v: Value) -> ScrobbleTrack {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn track_metadata_has_release_only_with_an_album() {
        assert_eq!(
            track_metadata(&track(
                json!({ "artist": "A", "track": "T", "album": "Al" })
            )),
            json!({ "artist_name": "A", "track_name": "T", "release_name": "Al" })
        );
        assert_eq!(
            track_metadata(&track(json!({ "artist": "A", "track": "T" }))),
            json!({ "artist_name": "A", "track_name": "T" })
        );
    }

    #[test]
    fn token_rules() {
        let store = Arc::new(MemoryStore::default());
        let lb = ListenBrainz::new(reqwest::Client::new(), store.clone());
        assert!(lb.set_token(&json!("  tok ")).ok);
        assert_eq!(store.get(KEY_TOKEN).as_deref(), Some("tok"));
        assert!(!lb.set_token(&json!("x".repeat(257))).ok);
        assert!(!lb.set_token(&json!(12)).ok);
        assert_eq!(store.get(KEY_TOKEN).as_deref(), Some("tok"));
        assert!(lb.set_token(&Value::Null).ok);
        assert_eq!(store.get(KEY_TOKEN).as_deref(), Some(""));
        lb.disconnect();
        assert_eq!(store.get(KEY_TOKEN), None);
    }

    #[tokio::test]
    async fn status_validates_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/1/validate-token"))
            .and(header("authorization", "Token good"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "valid": true, "user_name": "ann" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/1/validate-token"))
            .and(header("authorization", "Token bad"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "valid": false })))
            .mount(&server)
            .await;
        let store = Arc::new(MemoryStore::default());
        let lb = ListenBrainz::with_root(
            reqwest::Client::new(),
            store.clone(),
            Url::parse(&server.uri()).unwrap(),
        );
        assert_eq!(
            lb.status().await,
            LbStatus {
                connected: false,
                username: None
            }
        );
        store.set(KEY_TOKEN, "good".into());
        assert_eq!(
            lb.status().await,
            LbStatus {
                connected: true,
                username: Some("ann".into())
            }
        );
        store.set(KEY_TOKEN, "bad".into());
        assert_eq!(
            lb.status().await,
            LbStatus {
                connected: false,
                username: None
            }
        );

        // Unreachable: the token is assumed good.
        let offline = ListenBrainz::with_root(
            reqwest::Client::new(),
            store,
            Url::parse("http://127.0.0.1:9").unwrap(),
        );
        assert_eq!(
            offline.status().await,
            LbStatus {
                connected: true,
                username: None
            }
        );
    }

    #[tokio::test]
    async fn listens_are_submitted_with_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/1/submit-listens"))
            .and(header("authorization", "Token tok"))
            .and(body_json(json!({
                "listen_type": "playing_now",
                "payload": [{ "track_metadata": { "artist_name": "A", "track_name": "T", "release_name": "Al" } }],
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/1/submit-listens"))
            .and(body_json(json!({
                "listen_type": "single",
                "payload": [{ "listened_at": 1700000000, "track_metadata": { "artist_name": "A", "track_name": "T", "release_name": "Al" } }],
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let store = Arc::new(MemoryStore::default());
        store.set(KEY_TOKEN, "tok".into());
        let lb = ListenBrainz::with_root(
            reqwest::Client::new(),
            store.clone(),
            Url::parse(&server.uri()).unwrap(),
        );
        let t =
            track(json!({ "artist": "A", "track": "T", "album": "Al", "timestamp": 1700000000 }));
        lb.now_playing(&t).await;
        lb.scrobble(&t).await;
        lb.scrobble(&track(json!({ "artist": "A", "track": "T" })))
            .await; // no timestamp
        lb.now_playing(&track(json!({ "artist": "", "track": "T" })))
            .await; // no artist
        store.delete(KEY_TOKEN);
        lb.now_playing(&t).await; // no token
        server.verify().await;
    }
}
