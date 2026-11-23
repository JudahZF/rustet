//! The shared client, its error type and the request plumbing every resource uses.

use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use regex::bytes::Regex;
use reqwest::header::{AUTHORIZATION, ORIGIN, USER_AGENT};
use reqwest::{RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::lyrics::TtmlError;

/// The Apple Music API origin.
pub const AMP_BASE: &str = "https://amp-api.music.apple.com";
/// The web player origin, sent as `Origin` and scraped for the developer token.
pub const WEB_ORIGIN: &str = "https://music.apple.com";

/// Apple's API accepts the web player only from a desktop browser.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36";

/// Deadline for one catalog request, body included, so a slow CDN cannot hang a caller.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds the web player page and script scanned for the developer token.
const MAX_TOKEN_PAGE_BYTES: usize = 32 << 20;

/// Errors from the Apple Music API client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("invalid response: {0}")]
    Json(#[from] serde_json::Error),
    /// A catalog request answered with a status other than 200.
    #[error("{0}")]
    Status(StatusCode),
    /// A personalised feed answered with a status other than 200. 401 means the user
    /// token expired, 403 no subscription.
    #[error("apple music api: HTTP {}", status.as_u16())]
    Feed { status: StatusCode, body: Bytes },
    #[error("library playlist HTTP {}", .0.as_u16())]
    LibraryStatus(StatusCode),
    /// Apple returned no lyrics. Only the status is kept: Apple's body must not reach
    /// the engine's HTTP clients.
    #[error("failed to get lyrics: HTTP {}", .0.as_u16())]
    LyricsUnavailable(StatusCode),
    #[error(transparent)]
    Ttml(#[from] TtmlError),
    /// The resource has no `data` entry.
    #[error("{0}: empty response")]
    Empty(&'static str),
    #[error("library playlist {0}: no tracks")]
    LibraryPlaylistEmpty(String),
    /// The call needs the listener's Media-User-Token / Music-User-Token.
    #[error("a user token is required (user must be signed in)")]
    MissingUserToken,
    #[error("track pagination exceeded {} pages", crate::track::MAX_TRACK_PAGES)]
    TooManyPages,
    #[error("token: web player script not found on music.apple.com")]
    TokenScriptNotFound,
    #[error("token: developer token not found in the web player script")]
    TokenNotFound,
    #[error("cancelled")]
    Cancelled,
    /// Apple refused the music user token that was sent (401 or 403): the user is not
    /// signed in, the token expired, or the account has no subscription. Show "Sign in
    /// again"; browsing still works.
    #[error("apple music refused the user token: HTTP {}", .0.as_u16())]
    Unauthorized(StatusCode),
    /// Apple refused the developer token (401) even after a fresh scrape.
    #[error("apple music refused the developer token: HTTP {}", .0.as_u16())]
    DeveloperTokenRejected(StatusCode),
    /// The resource does not exist (404), or the ID is not a valid path segment.
    #[error("not found")]
    NotFound,
    /// Apple answered 429 and kept answering it after the retries.
    #[error("rate limited by apple music")]
    RateLimited {
        /// Apple's `Retry-After`, when it sent one.
        retry_after: Option<Duration>,
    },
}

/// The class of an [`Error`], for callers that react to the class and not the details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Sign in (again): the user token is missing, expired or refused.
    Unauthorized,
    NotFound,
    /// Wait and try again later.
    RateLimited,
    /// The request did not reach Apple or the answer did not arrive (DNS, TLS, timeout).
    Transport,
    /// Apple answered, but the body is not what the model expects.
    Decode,
    Cancelled,
    /// Anything else: other statuses, a rejected developer token, the token scrape.
    Other,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        let of_status = |status: StatusCode| match status.as_u16() {
            404 => ErrorKind::NotFound,
            429 => ErrorKind::RateLimited,
            _ => ErrorKind::Other,
        };
        match self {
            Self::Unauthorized(_) | Self::MissingUserToken => ErrorKind::Unauthorized,
            Self::Feed { status, .. } if matches!(status.as_u16(), 401 | 403) => {
                ErrorKind::Unauthorized
            }
            Self::NotFound | Self::Empty(_) => ErrorKind::NotFound,
            Self::RateLimited { .. } => ErrorKind::RateLimited,
            Self::Http(e) if e.is_decode() => ErrorKind::Decode,
            Self::Http(e) => e.status().map_or(ErrorKind::Transport, of_status),
            Self::Json(_) | Self::Ttml(_) => ErrorKind::Decode,
            Self::Cancelled => ErrorKind::Cancelled,
            Self::Status(s)
            | Self::Feed { status: s, .. }
            | Self::LibraryStatus(s)
            | Self::LyricsUnavailable(s) => of_status(*s),
            Self::Url(_)
            | Self::LibraryPlaylistEmpty(_)
            | Self::TooManyPages
            | Self::TokenScriptNotFound
            | Self::TokenNotFound
            | Self::DeveloperTokenRejected(_) => ErrorKind::Other,
        }
    }
}

/// Apple Music API client. Cheap to clone; clones share the connection pool.
#[derive(Debug, Clone)]
pub struct Client {
    pub(crate) http: reqwest::Client,
    pub(crate) amp_base: String,
    web_base: String,
}

impl Client {
    /// A client for Apple's servers over the caller's shared HTTP client.
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_base_urls(http, AMP_BASE, WEB_ORIGIN)
    }

    /// A client for other origins (tests, mock servers). `amp_base` replaces
    /// [`AMP_BASE`] and `web_base` replaces the token scrape origin [`WEB_ORIGIN`].
    pub fn with_base_urls(
        http: reqwest::Client,
        amp_base: impl Into<String>,
        web_base: impl Into<String>,
    ) -> Self {
        Self {
            http,
            amp_base: amp_base.into(),
            web_base: web_base.into(),
        }
    }

    /// A GET to an Apple Music API URL with the developer token and browser headers.
    pub(crate) fn api_get(&self, url: impl reqwest::IntoUrl, token: &str) -> RequestBuilder {
        self.http
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, BROWSER_USER_AGENT)
            .header(ORIGIN, WEB_ORIGIN)
    }

    /// Sends `request` and reads the body, at most `limit` bytes (the rest is dropped,
    /// not an error).
    pub(crate) async fn fetch(
        &self,
        cancel: &CancellationToken,
        request: RequestBuilder,
        limit: Option<usize>,
    ) -> Result<(StatusCode, Bytes), Error> {
        let read = async {
            let mut response = request.send().await?;
            let status = response.status();
            let Some(limit) = limit else {
                return Ok((status, response.bytes().await?));
            };
            let mut body = BytesMut::new();
            while body.len() < limit
                && let Some(chunk) = response.chunk().await?
            {
                body.extend_from_slice(&chunk);
            }
            body.truncate(limit);
            Ok((status, body.freeze()))
        };
        cancel
            .run_until_cancelled(read)
            .await
            .unwrap_or(Err(Error::Cancelled))
    }

    /// Sends `request` and decodes a 200 response.
    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        cancel: &CancellationToken,
        request: RequestBuilder,
    ) -> Result<T, Error> {
        let (status, body) = self.fetch(cancel, request, None).await?;
        if status != StatusCode::OK {
            return Err(Error::Status(status));
        }
        Ok(serde_json::from_slice(&body)?)
    }

    /// `token`, or a freshly scraped developer token when it is empty.
    pub(crate) async fn bearer<'a>(&self, token: &'a str) -> Result<Cow<'a, str>, Error> {
        if token.is_empty() {
            Ok(Cow::Owned(self.token().await?))
        } else {
            Ok(Cow::Borrowed(token))
        }
    }

    /// Scrapes the developer token (a JWT) from the web player's main script.
    pub async fn token(&self) -> Result<String, Error> {
        static SCRIPT: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"/assets/index~[^/]+\.js").expect("valid regex"));
        static JWT: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"eyJ[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+")
                .expect("valid regex")
        });

        let cancel = CancellationToken::new();
        let page = self.http.get(&self.web_base).timeout(REQUEST_TIMEOUT);
        let (_, body) = self
            .fetch(&cancel, page, Some(MAX_TOKEN_PAGE_BYTES))
            .await?;
        let script = SCRIPT.find(&body).ok_or(Error::TokenScriptNotFound)?;
        let script_url = format!(
            "{}{}",
            self.web_base,
            String::from_utf8_lossy(script.as_bytes())
        );

        let script = self.http.get(script_url).timeout(REQUEST_TIMEOUT);
        let (_, body) = self
            .fetch(&cancel, script, Some(MAX_TOKEN_PAGE_BYTES))
            .await?;
        let token = JWT.find(&body).ok_or(Error::TokenNotFound)?;
        // The pattern only matches ASCII.
        Ok(String::from_utf8_lossy(token.as_bytes()).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn token_is_scraped_from_the_index_script() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<script src="/assets/index~abc123.js"></script>"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/assets/index~abc123.js"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"const t="eyJhbGc.eyJpc3M.sig-_=";"#),
            )
            .mount(&server)
            .await;
        let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
        assert_eq!(client.token().await.unwrap(), "eyJhbGc.eyJpc3M.sig-_=");
    }

    #[tokio::test]
    #[ignore = "calls music.apple.com"]
    async fn live_token() {
        let token = Client::new(reqwest::Client::new()).token().await.unwrap();
        assert!(token.starts_with("eyJ"));
    }
}
