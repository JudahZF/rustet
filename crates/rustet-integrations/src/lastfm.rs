//! Last.fm scrobbling: request signing, the desktop auth flow, now-playing and scrobbles.
//!
//! The API key and shared secret are set from Rustet's settings. Both, and the user's session key,
//! live only in the encrypted store; the secret is never returned to the page.
//!
//! Auth flow: `lastfm:auth` gets a request token and returns the authorize-page URL, which the
//! shell opens in its own window; once the user has approved, `lastfm:session` trades the token
//! for a session key.

use std::{collections::BTreeMap, sync::Arc, sync::Mutex};

use md5::{Digest, Md5};
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;
use url::Url;

use crate::{
    scrobble::ScrobbleTrack,
    store::{SecretStore, get_set, js_len},
};

/// Last.fm API root.
pub const API_ROOT: &str = "https://ws.audioscrobbler.com/2.0/";
/// The page the user approves the app on.
pub const AUTH_PAGE: &str = "https://www.last.fm/api/auth/";

const KEY_API_KEY: &str = "lastfm_api_key";
const KEY_SECRET: &str = "lastfm_secret";
const KEY_SESSION: &str = "lastfm_sk";
const KEY_USER: &str = "lastfm_user";

/// Longest API key or secret accepted from the page.
const CREDENTIAL_MAX: usize = 128;

/// Why a Last.fm call failed. Serialises to the page's `{ "error": "…" }`.
#[derive(Debug, thiserror::Error)]
pub enum LastfmError {
    #[error("not-configured")]
    NotConfigured,
    #[error("no-token")]
    NoToken,
    /// Last.fm answered without the expected field; the text is its `message` or a fallback.
    #[error("{0}")]
    Api(String),
    #[error("{0}")]
    Http(#[from] reqwest::Error),
}

impl Serialize for LastfmError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(1))?;
        m.serialize_entry("error", &self.to_string())?;
        m.end()
    }
}

/// `{ "ok": true }`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Ack {
    pub ok: bool,
}

/// Reply of `lastfm:status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastfmStatus {
    /// Both the API key and the secret are set.
    pub configured: bool,
    /// The API key is public and returned for the settings form; the secret never is.
    pub api_key: String,
    pub secret_set: bool,
    pub connected: bool,
    pub username: Option<String>,
}

/// A started auth flow. Serialises to the page's `{ "token": "…" }`; the shell opens `url`.
#[derive(Debug, Clone, Serialize)]
pub struct AuthStart {
    pub token: String,
    #[serde(skip)]
    pub url: Url,
}

/// Reply of a successful `lastfm:session`.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub username: Option<String>,
}

/// The Last.fm client. Cheap to share behind an `Arc`.
pub struct Lastfm {
    http: reqwest::Client,
    store: Arc<dyn SecretStore>,
    root: Url,
    /// Token of the auth flow in progress, used when `lastfm:session` comes without one.
    pending_token: Mutex<Option<String>>,
}

/// `api_sig`: MD5 over the parameters sorted by name, each as name + value, excluding `format`
/// and `callback`, followed by the shared secret.
pub fn sign(params: &BTreeMap<&str, String>, secret: &str) -> String {
    let mut h = Md5::new();
    for (k, v) in params
        .iter()
        .filter(|(k, _)| !matches!(**k, "format" | "callback"))
    {
        h.update(k.as_bytes());
        h.update(v.as_bytes());
    }
    h.update(secret.as_bytes());
    format!("{:x}", h.finalize())
}

impl Lastfm {
    pub fn new(http: reqwest::Client, store: Arc<dyn SecretStore>) -> Self {
        Self::with_root(http, store, Url::parse(API_ROOT).expect("valid URL"))
    }

    /// A client for another API root (tests).
    pub fn with_root(http: reqwest::Client, store: Arc<dyn SecretStore>, root: Url) -> Self {
        Self {
            http,
            store,
            root,
            pending_token: Mutex::new(None),
        }
    }

    fn credentials(&self) -> Option<(String, String)> {
        Some((
            get_set(&*self.store, KEY_API_KEY)?,
            get_set(&*self.store, KEY_SECRET)?,
        ))
    }

    /// One signed API call. Last.fm's JSON is returned whatever the HTTP status: errors come
    /// as `{ "error": n, "message": "…" }`.
    async fn call(
        &self,
        mut params: BTreeMap<&str, String>,
        post: bool,
    ) -> Result<Value, LastfmError> {
        let (key, secret) = self.credentials().unwrap_or_default();
        params.insert("api_key", key);
        let sig = sign(&params, &secret);
        params.insert("api_sig", sig);
        params.insert("format", "json".to_owned());
        let req = if post {
            self.http.post(self.root.clone()).form(&params)
        } else {
            self.http.get(self.root.clone()).query(&params)
        };
        Ok(req.send().await?.json().await?)
    }

    /// `lastfm:set-credentials`: `{ apiKey?, secret? }`. A blank secret keeps the saved one.
    pub fn set_credentials(&self, c: &Value) -> Ack {
        if let Some(key) = c.get("apiKey").and_then(Value::as_str)
            && js_len(key) <= CREDENTIAL_MAX
        {
            self.store.set(KEY_API_KEY, key.trim().to_owned());
        }
        if let Some(secret) = c.get("secret").and_then(Value::as_str)
            && !secret.trim().is_empty()
            && js_len(secret) <= CREDENTIAL_MAX
        {
            self.store.set(KEY_SECRET, secret.trim().to_owned());
        }
        Ack { ok: true }
    }

    /// `lastfm:status`.
    pub fn status(&self) -> LastfmStatus {
        let api_key = get_set(&*self.store, KEY_API_KEY);
        let secret_set = get_set(&*self.store, KEY_SECRET).is_some();
        LastfmStatus {
            configured: api_key.is_some() && secret_set,
            api_key: api_key.unwrap_or_default(),
            secret_set,
            connected: get_set(&*self.store, KEY_SESSION).is_some(),
            username: get_set(&*self.store, KEY_USER),
        }
    }

    /// `lastfm:auth`: request a token and build the authorize-page URL for the shell to open.
    pub async fn auth(&self) -> Result<AuthStart, LastfmError> {
        let (key, _) = self.credentials().ok_or(LastfmError::NotConfigured)?;
        let r = self
            .call(
                BTreeMap::from([("method", "auth.getToken".to_owned())]),
                false,
            )
            .await?;
        let Some(token) = r
            .get("token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        else {
            return Err(api_error(&r, "token request failed"));
        };
        *self.pending_token.lock().unwrap() = Some(token.to_owned());
        let mut url = Url::parse(AUTH_PAGE).expect("valid URL");
        url.query_pairs_mut()
            .append_pair("api_key", &key)
            .append_pair("token", token);
        Ok(AuthStart {
            token: token.to_owned(),
            url,
        })
    }

    /// `lastfm:session`: trade the approved token (the page's, or the pending one) for a
    /// session key and keep it.
    pub async fn session(&self, token: &Value) -> Result<Session, LastfmError> {
        self.credentials().ok_or(LastfmError::NotConfigured)?;
        let token = match token.as_str().filter(|t| !t.is_empty()) {
            Some(t) => t.to_owned(),
            None => self
                .pending_token
                .lock()
                .unwrap()
                .clone()
                .ok_or(LastfmError::NoToken)?,
        };
        let params = BTreeMap::from([("method", "auth.getSession".to_owned()), ("token", token)]);
        let r = self.call(params, false).await?;
        let session = r.get("session");
        let Some(sk) = session
            .and_then(|s| s.get("key"))
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
        else {
            return Err(api_error(
                &r,
                "not authorized yet — approve in the browser first",
            ));
        };
        let username = session
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.store.set(KEY_SESSION, sk.to_owned());
        match &username {
            Some(name) => self.store.set(KEY_USER, name.clone()),
            None => self.store.delete(KEY_USER),
        }
        *self.pending_token.lock().unwrap() = None;
        Ok(Session { username })
    }

    /// `lastfm:disconnect`: forget the session (the API credentials stay).
    pub fn disconnect(&self) -> Ack {
        self.store.delete(KEY_SESSION);
        self.store.delete(KEY_USER);
        Ack { ok: true }
    }

    /// `lastfm:nowplaying`. Failures are logged and dropped.
    pub async fn now_playing(&self, t: &ScrobbleTrack) {
        self.submit("track.updateNowPlaying", t, None).await;
    }

    /// `lastfm:scrobble`. Needs the start timestamp. Failures are logged and dropped.
    pub async fn scrobble(&self, t: &ScrobbleTrack) {
        if let Some(ts) = t.timestamp() {
            self.submit("track.scrobble", t, Some(ts.to_string())).await;
        }
    }

    async fn submit(&self, method: &'static str, t: &ScrobbleTrack, timestamp: Option<String>) {
        if !t.is_complete() {
            return;
        }
        let Some(sk) = get_set(&*self.store, KEY_SESSION) else {
            return;
        };
        if get_set(&*self.store, KEY_API_KEY).is_none() {
            return;
        }
        let mut p = BTreeMap::from([
            ("method", method.to_owned()),
            ("artist", t.artist.clone()),
            ("track", t.track.clone()),
            ("sk", sk),
        ]);
        if let Some(ts) = timestamp {
            p.insert("timestamp", ts);
        }
        if let Some(album) = t.album() {
            p.insert("album", album.to_owned());
        }
        if let Some(aa) = t.album_artist() {
            p.insert("albumArtist", aa.to_owned());
        }
        if let Some(d) = t.duration_secs() {
            p.insert("duration", d.to_string());
        }
        if let Err(e) = self.call(p, true).await {
            tracing::debug!("Last.fm {method} failed: {e}");
        }
    }
}

fn api_error(r: &Value, fallback: &str) -> LastfmError {
    let msg = r
        .get("message")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty());
    LastfmError::Api(msg.unwrap_or(fallback).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{method, query_param},
    };

    fn params(p: &[(&'static str, &str)]) -> BTreeMap<&'static str, String> {
        p.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn signature_is_sorted_secret_suffixed_md5_without_format() {
        // Reference from the Electron algorithm (node crypto), and `md5sum` of
        // "api_keyKmethodauth.getTokenSEC".
        let p = params(&[("method", "auth.getToken"), ("api_key", "K")]);
        assert_eq!(sign(&p, "SEC"), "a96c8c01128caae9ef2d431e9986249d");
        let mut with_format = p.clone();
        with_format.insert("format", "json".into());
        with_format.insert("callback", "cb".into());
        assert_eq!(sign(&with_format, "SEC"), sign(&p, "SEC"));
        assert_ne!(sign(&p, "SEC"), sign(&p, "OTHER"));
    }

    fn configured() -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::default());
        store.set(KEY_API_KEY, "KEY".into());
        store.set(KEY_SECRET, "SECRET".into());
        store
    }

    fn client(server: &MockServer, store: Arc<MemoryStore>) -> Lastfm {
        Lastfm::with_root(
            reqwest::Client::new(),
            store,
            Url::parse(&server.uri()).unwrap(),
        )
    }

    /// The `api_sig` of a request's parameters must match a fresh signature over the rest.
    fn signature_ok(pairs: &[(String, String)]) -> bool {
        let sig = pairs
            .iter()
            .find(|(k, _)| k == "api_sig")
            .map(|(_, v)| v.clone());
        let rest: BTreeMap<&str, String> = pairs
            .iter()
            .filter(|(k, _)| k != "api_sig")
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        sig == Some(sign(&rest, "SECRET"))
    }

    #[test]
    fn credentials_limits_and_blank_secret() {
        let store = Arc::new(MemoryStore::default());
        let lf = Lastfm::new(reqwest::Client::new(), store.clone());
        assert!(!lf.status().configured);
        lf.set_credentials(&json!({ "apiKey": "  K  ", "secret": " S " }));
        assert_eq!(store.get(KEY_API_KEY).as_deref(), Some("K"));
        assert_eq!(store.get(KEY_SECRET).as_deref(), Some("S"));
        // A blank secret keeps the saved one; over-long values are ignored.
        lf.set_credentials(&json!({ "apiKey": "x".repeat(129), "secret": "   " }));
        assert_eq!(store.get(KEY_API_KEY).as_deref(), Some("K"));
        assert_eq!(store.get(KEY_SECRET).as_deref(), Some("S"));
        lf.set_credentials(&json!({ "apiKey": 5, "secret": "s".repeat(129) }));
        assert_eq!(store.get(KEY_SECRET).as_deref(), Some("S"));
        let st = lf.status();
        assert_eq!(
            st,
            LastfmStatus {
                configured: true,
                api_key: "K".into(),
                secret_set: true,
                connected: false,
                username: None
            }
        );
        let wire = serde_json::to_value(&st).unwrap();
        assert_eq!(
            wire,
            json!({ "configured": true, "apiKey": "K", "secretSet": true, "connected": false, "username": null })
        );
    }

    #[tokio::test]
    async fn auth_flow_signs_and_stores_the_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("method", "auth.getToken"))
            .and(query_param("api_key", "KEY"))
            .and(query_param("format", "json"))
            .and(|r: &Request| signature_ok(&r.url.query_pairs().into_owned().collect::<Vec<_>>()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "token": "TOK" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("method", "auth.getSession"))
            .and(query_param("token", "TOK"))
            .and(|r: &Request| signature_ok(&r.url.query_pairs().into_owned().collect::<Vec<_>>()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "session": { "key": "SK", "name": "bob" } })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let store = configured();
        let lf = client(&server, store.clone());
        let start = lf.auth().await.unwrap();
        assert_eq!(start.token, "TOK");
        assert_eq!(
            start.url.as_str(),
            "https://www.last.fm/api/auth/?api_key=KEY&token=TOK"
        );
        assert_eq!(
            serde_json::to_value(&start).unwrap(),
            json!({ "token": "TOK" })
        );

        // No token from the page: the pending one is used.
        let s = lf.session(&Value::Null).await.unwrap();
        assert_eq!(s.username.as_deref(), Some("bob"));
        assert_eq!(store.get(KEY_SESSION).as_deref(), Some("SK"));
        let st = lf.status();
        assert!(st.connected);
        assert_eq!(st.username.as_deref(), Some("bob"));
        // The pending token is spent.
        assert!(matches!(
            lf.session(&Value::Null).await,
            Err(LastfmError::NoToken)
        ));

        lf.disconnect();
        assert!(!lf.status().connected);
        assert!(lf.status().configured, "credentials survive a disconnect");
    }

    #[tokio::test]
    async fn auth_errors_carry_lastfm_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("method", "auth.getSession"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({ "error": 14, "message": "Unauthorized Token" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("method", "auth.getToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let lf = client(&server, configured());
        let e = lf.session(&json!("T")).await.unwrap_err();
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({ "error": "Unauthorized Token" })
        );
        let e = lf.auth().await.unwrap_err();
        assert_eq!(e.to_string(), "token request failed");

        let unconfigured = client(&server, Arc::new(MemoryStore::default()));
        assert!(matches!(
            unconfigured.auth().await,
            Err(LastfmError::NotConfigured)
        ));
        assert!(matches!(
            unconfigured.session(&json!("T")).await,
            Err(LastfmError::NotConfigured)
        ));
    }

    #[tokio::test]
    async fn now_playing_and_scrobble_post_signed_forms() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(|r: &Request| {
                let pairs: Vec<(String, String)> =
                    url::form_urlencoded::parse(&r.body).into_owned().collect();
                let has = |k: &str, v: &str| pairs.iter().any(|(a, b)| a == k && b == v);
                r.headers
                    .get("content-type")
                    .is_some_and(|v| v == "application/x-www-form-urlencoded")
                    && signature_ok(&pairs)
                    && has("sk", "SK")
                    && has("artist", "Ärtist & Co")
                    && has("track", "T")
                    && has("duration", "181")
                    && has("format", "json")
                    && !pairs.iter().any(|(k, _)| k == "album")
            })
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(2)
            .mount(&server)
            .await;

        let store = configured();
        store.set(KEY_SESSION, "SK".into());
        let lf = client(&server, store);
        let t: ScrobbleTrack = serde_json::from_value(
            json!({ "artist": "Ärtist & Co", "track": "T", "album": "", "duration": 180.5, "timestamp": 1700000000 }),
        )
        .unwrap();
        lf.now_playing(&t).await;
        lf.scrobble(&t).await;
        // Not sent: no timestamp for a scrobble, no title, no session.
        let mut no_ts = t.clone();
        no_ts.timestamp = None;
        lf.scrobble(&no_ts).await;
        let mut no_title = t.clone();
        no_title.track.clear();
        lf.now_playing(&no_title).await;
        lf.disconnect();
        lf.now_playing(&t).await;
        server.verify().await;
        let reqs = server.received_requests().await.unwrap();
        let scrobble = reqs
            .iter()
            .find(|r| String::from_utf8_lossy(&r.body).contains("track.scrobble"))
            .unwrap();
        assert!(String::from_utf8_lossy(&scrobble.body).contains("timestamp=1700000000"));
    }
}
