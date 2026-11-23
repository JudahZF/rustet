//! The typed client of the engine's HTTPS API on the loopback address.
//!
//! It trusts exactly one certificate, the [`super::EngineIdentity`] the UI handed the engine,
//! and no proxy. Errors are typed: a view matches [`ClientError`] variants, never message text.

use std::time::Duration;

use reqwest::{Method, StatusCode};
use rustet_player::model::{Command, HistoryEntry, PlayerState, VideoMode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::types::{
    AuthSession, AuthStatus, CacheScope, CacheStats, DrmStatus, EngineCapabilities, ExportJob,
    ExportRequest, LyricsFormat, LyricsKind, MvCache, MvCacheUpdate, TokenPush, Tools,
    TrackMetadata,
};

/// The time a request may take. The DRM sign-out may take 45 s in the engine
/// ([`EngineClient::drm_sign_out`] waits longer).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Above the engine's bounded sign-out (45 s route + 2 s exit), as the renderer's
/// `SIGN_OUT_TIMEOUT_MS`.
const SIGN_OUT_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No answer: the engine is not running, restarting, or too slow.
    #[error("the engine is not reachable: {0}")]
    Unreachable(#[source] reqwest::Error),
    /// A player command named a generation that is no longer current (409): the track
    /// changed meanwhile, so the command no longer applies.
    #[error("the player moved on to another track")]
    StaleGeneration,
    /// 400: the request was refused as invalid.
    #[error("{0}")]
    BadRequest(String),
    /// 401: the engine has no Apple token for this.
    #[error("{0}")]
    NotAuthenticated(String),
    /// 404.
    #[error("{0}")]
    NotFound(String),
    /// 409 outside the player: a sign-in already runs, no challenge is pending, a job runs.
    #[error("{0}")]
    Conflict(String),
    /// 429 from the DRM sign-in: Apple's attempt limit. The message says for how long.
    #[error("{message}")]
    SignInPaused {
        message: String,
        retry_after: Option<Duration>,
    },
    /// 503: the feature is not in this engine (no DRM layer, no player, no exports).
    #[error("{0}")]
    Unavailable(String),
    /// Any other status.
    #[error("engine answered {status}: {message}")]
    Status { status: u16, message: String },
    /// The answer was not the expected JSON.
    #[error("unexpected engine answer: {0}")]
    Decode(#[source] reqwest::Error),
    /// The engine-internal [`Command::Capabilities`] has no route.
    #[error("this command has no route")]
    NoRoute,
}

/// Why the client could not be built.
#[derive(Debug, thiserror::Error)]
#[error("engine client: {0}")]
pub struct BuildError(#[from] reqwest::Error);

/// The engine API at `https://127.0.0.1:<port>`. Cheap to clone.
#[derive(Debug, Clone)]
pub struct EngineClient {
    http: reqwest::Client,
    base: String,
}

impl EngineClient {
    /// A client for the engine on `port` that trusts only `cert_der`.
    pub fn new(port: u16, cert_der: &[u8]) -> Result<Self, BuildError> {
        let cert = reqwest::Certificate::from_der(cert_der)?;
        let http = reqwest::Client::builder()
            .tls_certs_only([cert])
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .build()?;
        Ok(Self {
            http,
            base: format!("https://127.0.0.1:{port}"),
        })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
        timeout: Duration,
    ) -> Result<reqwest::Response, ClientError> {
        let mut req = self.http.request(method, self.url(path)).timeout(timeout);
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req.send().await.map_err(ClientError::Unreachable)?;
        if res.status().is_success() {
            return Ok(res);
        }
        let status = res.status();
        let retry_after = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        // The engine answers errors as one line of plain text.
        let message = res.text().await.unwrap_or_default().trim().to_owned();
        Err(match status {
            StatusCode::CONFLICT if path.starts_with("/api/v1/player/") => {
                ClientError::StaleGeneration
            }
            StatusCode::CONFLICT => ClientError::Conflict(message),
            StatusCode::BAD_REQUEST => ClientError::BadRequest(message),
            StatusCode::UNAUTHORIZED => ClientError::NotAuthenticated(message),
            StatusCode::NOT_FOUND => ClientError::NotFound(message),
            StatusCode::TOO_MANY_REQUESTS => ClientError::SignInPaused {
                message,
                retry_after,
            },
            StatusCode::SERVICE_UNAVAILABLE => ClientError::Unavailable(message),
            s => ClientError::Status {
                status: s.as_u16(),
                message,
            },
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send(Method::GET, path, None::<&()>, REQUEST_TIMEOUT)
            .await?
            .json()
            .await
            .map_err(ClientError::Decode)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<reqwest::Response, ClientError> {
        self.send(method, path, body, REQUEST_TIMEOUT).await
    }

    async fn call_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&(impl Serialize + ?Sized)>,
    ) -> Result<T, ClientError> {
        self.call(method, path, body)
            .await?
            .json()
            .await
            .map_err(ClientError::Decode)
    }

    // ── Player ───────────────────────────────────────────────────────────────

    /// Sends a player command to its route (spec §4.8). `Ok` once the player executed it (202).
    pub async fn command(&self, command: &Command) -> Result<(), ClientError> {
        let (path, body): (&str, Option<Value>) = match command {
            Command::SetQueue(b) => ("queue", Some(json!(b))),
            Command::Insert(b) => ("queue/insert", Some(json!(b))),
            Command::Remove(b) => ("queue/remove", Some(json!(b))),
            Command::Move(b) => ("queue/move", Some(json!(b))),
            Command::Jump(b) => ("queue/jump", Some(json!(b))),
            Command::ClearUpcoming => ("queue/clear", None),
            Command::Play => ("play", None),
            Command::Pause => ("pause", None),
            Command::Toggle => ("toggle", None),
            Command::Next => ("next", None),
            Command::Previous => ("previous", None),
            Command::Stop => ("stop", None),
            Command::Seek(b) => ("seek", Some(json!(b))),
            Command::Volume(b) => ("volume", Some(json!(b))),
            Command::Mode(b) => ("mode", Some(json!(b))),
            Command::Settings(b) => ("settings", Some(json!(b))),
            Command::Hover(b) => ("hover", Some(json!(b))),
            Command::ClearHistory => return self.clear_history().await,
            Command::Capabilities(_) => return Err(ClientError::NoRoute),
        };
        self.call(
            Method::POST,
            &format!("/api/v1/player/{path}"),
            body.as_ref(),
        )
        .await
        .map(drop)
    }

    /// `POST /api/v1/player/video`: music videos play as audio only, or with their video. A
    /// playing music video reloads at its position; the choice is kept by the engine.
    pub async fn set_video_mode(&self, mode: VideoMode) -> Result<(), ClientError> {
        self.call(Method::POST, "/api/v1/player/video", Some(&mode))
            .await
            .map(drop)
    }

    /// `GET /api/v1/player`.
    pub async fn player_state(&self) -> Result<PlayerState, ClientError> {
        self.get("/api/v1/player").await
    }

    /// `GET /api/v1/player/history`: newest first.
    pub async fn history(&self) -> Result<Vec<HistoryEntry>, ClientError> {
        self.get("/api/v1/player/history").await
    }

    /// `DELETE /api/v1/player/history`.
    pub async fn clear_history(&self) -> Result<(), ClientError> {
        self.call(Method::DELETE, "/api/v1/player/history", None::<&()>)
            .await
            .map(drop)
    }

    // ── Web account ──────────────────────────────────────────────────────────

    /// `GET /api/v1/auth`.
    pub async fn auth(&self) -> Result<AuthStatus, ClientError> {
        self.get("/api/v1/auth").await
    }

    /// `GET /api/v1/auth/session`: the account's music user token and storefront, `None`
    /// signed out (404). The engine refuses it to any request with an `Origin` header; this
    /// client sends none. The token is never logged: the error variants carry only the
    /// engine's message.
    pub async fn session(&self) -> Result<Option<AuthSession>, ClientError> {
        match self.get("/api/v1/auth/session").await {
            Ok(session) => Ok(Some(session)),
            Err(ClientError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `POST /api/v1/library/token`: the web session's music user token and storefront.
    pub async fn push_token(&self, push: &TokenPush<'_>) -> Result<(), ClientError> {
        self.call(Method::POST, "/api/v1/library/token", Some(push))
            .await
            .map(drop)
    }

    /// `DELETE /api/v1/auth/token`: the engine forgets the web token.
    pub async fn delete_token(&self) -> Result<(), ClientError> {
        self.call(Method::DELETE, "/api/v1/auth/token", None::<&()>)
            .await
            .map(drop)
    }

    // ── Lossless account (FairPlay) ──────────────────────────────────────────

    /// `GET /api/v1/drm/status`.
    pub async fn drm_status(&self) -> Result<DrmStatus, ClientError> {
        self.get("/api/v1/drm/status").await
    }

    /// `POST /api/v1/drm/authenticate`: starts a sign-in (202). Progress, codes and the outcome
    /// arrive as `drm` events.
    pub async fn drm_sign_in(&self, email: &str, password: &str) -> Result<DrmStatus, ClientError> {
        let body = json!({ "email": email, "password": password });
        self.call_json(Method::POST, "/api/v1/drm/authenticate", Some(&body))
            .await
    }

    /// `POST /api/v1/drm/challenge`: the answer to a challenge: the six digits, a dialog
    /// button index, or one of the special replies ([`SPECIAL_RESEND`], [`SPECIAL_SMS`]).
    pub async fn drm_reply(&self, reply: &str) -> Result<DrmStatus, ClientError> {
        let body = json!({ "reply": reply });
        self.call_json(Method::POST, "/api/v1/drm/challenge", Some(&body))
            .await
    }

    /// `POST /api/v1/drm/logout` (`device: false`) or `/forget-device`. The engine may exit
    /// afterwards (code 3) to finish in a fresh process; the supervisor restarts it.
    pub async fn drm_sign_out(&self, device: bool) -> Result<(), ClientError> {
        let path = if device {
            "/api/v1/drm/forget-device"
        } else {
            "/api/v1/drm/logout"
        };
        self.send(Method::POST, path, None::<&()>, SIGN_OUT_TIMEOUT)
            .await
            .map(drop)
    }

    // ── Metadata, lyrics ─────────────────────────────────────────────────────

    /// `GET /api/v1/metadata/{id}`: track info and the available qualities (the quality popup).
    pub async fn metadata(&self, id: &str, storefront: &str) -> Result<TrackMetadata, ClientError> {
        let url = with_query(
            &format!("/api/v1/metadata/{}", segment(id)),
            &[("sf", storefront)],
        );
        self.get(&url).await
    }

    /// `GET /api/v1/lyrics/{id}`: the lyrics text in `format`.
    pub async fn lyrics(
        &self,
        id: &str,
        storefront: &str,
        format: LyricsFormat,
        kind: LyricsKind,
    ) -> Result<String, ClientError> {
        let url = with_query(
            &format!("/api/v1/lyrics/{}", segment(id)),
            &[
                ("sf", storefront),
                ("format", format.as_str()),
                ("type", kind.as_str()),
            ],
        );
        self.call(Method::GET, &url, None::<&()>)
            .await?
            .text()
            .await
            .map_err(ClientError::Decode)
    }

    // ── Exports ──────────────────────────────────────────────────────────────

    /// `POST /api/v1/export`: queues an export; progress arrives as `export` events.
    pub async fn export(&self, request: &ExportRequest) -> Result<ExportJob, ClientError> {
        self.call_json(Method::POST, "/api/v1/export", Some(request))
            .await
    }

    /// `GET /api/v1/export`: every job.
    pub async fn exports(&self) -> Result<Vec<ExportJob>, ClientError> {
        self.get("/api/v1/export").await
    }

    /// `GET /api/v1/export/{id}`.
    pub async fn export_job(&self, id: &str) -> Result<ExportJob, ClientError> {
        self.get(&format!("/api/v1/export/{}", segment(id))).await
    }

    /// `DELETE /api/v1/export/{id}`: cancels a queued or running job, removes a finished one.
    pub async fn cancel_export(&self, id: &str) -> Result<(), ClientError> {
        let path = format!("/api/v1/export/{}", segment(id));
        self.call(Method::DELETE, &path, None::<&()>)
            .await
            .map(drop)
    }

    /// `POST /api/v1/export/{id}/retry`: a failed or cancelled job again, as a new job.
    pub async fn retry_export(&self, id: &str) -> Result<ExportJob, ClientError> {
        let path = format!("/api/v1/export/{}/retry", segment(id));
        self.call_json(Method::POST, &path, None::<&()>).await
    }

    /// `POST /api/v1/export/{id}/priority`: higher runs sooner. Only queued jobs.
    pub async fn prioritize_export(
        &self,
        id: &str,
        priority: i32,
    ) -> Result<ExportJob, ClientError> {
        let path = format!("/api/v1/export/{}/priority", segment(id));
        self.call_json(Method::POST, &path, Some(&json!({ "priority": priority })))
            .await
    }

    // ── Cache and storage ────────────────────────────────────────────────────

    /// `PUT /api/v1/cache/config` with the limits that were set
    /// ([`crate::settings::Storage::cache_config`]).
    pub async fn set_cache_config(&self, config: &Value) -> Result<(), ClientError> {
        self.call(Method::PUT, "/api/v1/cache/config", Some(config))
            .await
            .map(drop)
    }

    /// `GET /api/v1/cache/stats`.
    pub async fn cache_stats(&self) -> Result<CacheStats, ClientError> {
        self.get("/api/v1/cache/stats").await
    }

    /// `DELETE /api/v1/cache/playback[?what=…]`.
    pub async fn clear_cache(&self, scope: CacheScope) -> Result<(), ClientError> {
        let path = format!("/api/v1/cache/playback{}", scope.query());
        self.call(Method::DELETE, &path, None::<&()>)
            .await
            .map(drop)
    }

    /// `GET /api/v1/cache/mv`.
    pub async fn mv_cache(&self) -> Result<MvCache, ClientError> {
        self.get("/api/v1/cache/mv").await
    }

    /// `PUT /api/v1/cache/mv`.
    pub async fn set_mv_cache(&self, update: MvCacheUpdate) -> Result<MvCache, ClientError> {
        self.call_json(Method::PUT, "/api/v1/cache/mv", Some(&update))
            .await
    }

    /// `DELETE /api/v1/cache/mv`.
    pub async fn clear_mv_cache(&self) -> Result<(), ClientError> {
        self.call(Method::DELETE, "/api/v1/cache/mv", None::<&()>)
            .await
            .map(drop)
    }

    // ── Status ───────────────────────────────────────────────────────────────

    /// `GET /api/v1/status`: the engine answers.
    pub async fn ping(&self) -> Result<(), ClientError> {
        self.call(Method::GET, "/api/v1/status", None::<&()>)
            .await
            .map(drop)
    }

    /// `GET /api/v1/capabilities` (Settings → Engine Status).
    pub async fn capabilities(&self) -> Result<EngineCapabilities, ClientError> {
        self.get("/api/v1/capabilities").await
    }

    /// `GET /api/v1/tools[?ffmpegPath=]`: whether FFmpeg (the custom path, else `PATH`) works.
    pub async fn tools(&self, ffmpeg_path: &str) -> Result<Tools, ClientError> {
        self.get(&with_query("/api/v1/tools", &[("ffmpegPath", ffmpeg_path)]))
            .await
    }
}

/// Reply to a code challenge: send a new code to the trusted devices.
pub const SPECIAL_RESEND: &str = "__resend__";
/// Reply to a code challenge: the trusted devices cannot be reached, text a code instead.
pub const SPECIAL_SMS: &str = "__cant_devices__";

/// A path segment, percent-encoded.
fn segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// `path?k=v&…` with the non-empty values.
fn with_query(path: &str, params: &[(&str, &str)]) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params.iter().filter(|(_, v)| !v.is_empty()) {
        q.append_pair(k, v);
    }
    let q = q.finish();
    if q.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{q}")
    }
}

/// The untyped interface of `rustet_desktop`'s DRM helpers (sign-in again on launch), so they run
/// over this client.
impl rustet_desktop::engine::EngineApi for EngineClient {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Option<rustet_desktop::engine::EngineResponse> {
        let mut req = self
            .http
            .request(method, self.url(path))
            .timeout(REQUEST_TIMEOUT);
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
        Some(rustet_desktop::engine::EngineResponse { status, json })
    }
}
