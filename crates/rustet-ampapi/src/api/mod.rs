//! The typed Apple Music API client of the native UI and the player: [`Api`].
//!
//! Every request carries `Authorization: Bearer <developer token>`, the browser user agent
//! and `Origin: https://music.apple.com` (Apple answers 401 without it). Requests under
//! `/v1/me/` add `Media-User-Token`.
//!
//! - The developer token is scraped on first use and cached; it is replaced at 80 % of its
//!   life and after a 401 (risk R1 of the native-UI spec).
//! - Catalog GETs go through an in-memory response cache that follows Apple's
//!   `Cache-Control` and `ETag`. Library reads and writes are never cached.
//! - A GET answered 429 or 503 is retried after `Retry-After`, or after 1 s, 2 s and 4 s.
//!
//! Requests under `/v1/me/` and the play-activity report are written from the web
//! player's and MusicKit's code and Apple's documentation; they were not called while
//! writing this crate (no sign-in was allowed). Each says so in its documentation.

mod activity;
mod cache;
mod catalog;
mod me;
mod token;

use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use futures::Stream;
use reqwest::header::{AUTHORIZATION, HeaderMap, IF_NONE_MATCH, ORIGIN, RETRY_AFTER, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::client::BROWSER_USER_AGENT;
use crate::model::{Cursor, Page, Storefront};
use crate::{Client, Error, WEB_ORIGIN};

pub use activity::{
    EndReason, ItemId, MediaKind, PlayContainer, PlayEvent, PlayMode, PlayPhase, PlayedItem, Repeat,
};
pub use catalog::{ArtistView, BrowsePage};
pub use me::{
    History, LibraryContainer, LibraryKind, NewPlaylist, PlaylistEdit, Rating, ResourceRef,
    ResourceType,
};
pub use token::{DeveloperToken, UserToken};

use cache::{Lookup, ResponseCache};
use token::DeveloperTokenCache;

/// The edge host the web player uses for search, suggestions and editorial pages.
pub const AMP_EDGE_BASE: &str = "https://amp-api-edge.music.apple.com";
/// Where MusicKit reports plays.
pub const PLAY_ACTIVITY_URL: &str = "https://universal-activity-service.itunes.apple.com/play";

/// Deadline for one request, body included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds a response body (the largest seen, a curator page, is about 1 MB).
const MAX_BODY_BYTES: usize = 32 << 20;
/// Default response-cache size.
const DEFAULT_CACHE_BYTES: usize = 32 << 20;
/// Retries of a GET answered 429 or 503.
const MAX_RETRIES: u32 = 3;
/// Caps a `Retry-After` the client is willing to wait for.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Bounds [`Api::pages`].
pub const MAX_PAGES: usize = 200;

/// Hosts and sizes of an [`Api`]. The amp-api and web origins come from the [`Client`].
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Replaces [`AMP_EDGE_BASE`] (tests, mock servers).
    pub edge_base: String,
    /// Replaces [`PLAY_ACTIVITY_URL`].
    pub play_activity_url: String,
    /// Upper bound of the response cache, in body bytes.
    pub cache_bytes: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            edge_base: AMP_EDGE_BASE.to_owned(),
            play_activity_url: PLAY_ACTIVITY_URL.to_owned(),
            cache_bytes: DEFAULT_CACHE_BYTES,
        }
    }
}

/// The storefront and language every request is made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locale {
    /// Two-letter storefront ID, for example `us`.
    pub storefront: String,
    /// BCP-47 tag sent as `l`, for example `en-US`.
    pub language: String,
}

impl Locale {
    pub fn new(storefront: impl Into<String>, language: impl Into<String>) -> Self {
        Self {
            storefront: storefront.into(),
            language: language.into(),
        }
    }

    /// The `preferred` language if the storefront supports it, else the storefront's
    /// default language.
    pub fn for_storefront(storefront: &Storefront, preferred: &str) -> Self {
        let attributes = storefront.attributes.as_ref();
        let supported = attributes.is_some_and(|a| {
            a.supported_language_tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(preferred))
        });
        let language = match attributes {
            Some(a) if !supported && !a.default_language_tag.is_empty() => {
                a.default_language_tag.clone()
            }
            _ => preferred.to_owned(),
        };
        Self::new(&storefront.id, language)
    }
}

impl Default for Locale {
    fn default() -> Self {
        Self::new("us", "en-US")
    }
}

/// Apple Music API client for the native UI and the player. Cheap to clone; clones share
/// the tokens, the locale and the response cache.
#[derive(Clone)]
pub struct Api {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    edge_base: String,
    play_activity_url: String,
    developer_token: DeveloperTokenCache,
    user_token: RwLock<Option<UserToken>>,
    locale: RwLock<Locale>,
    cache: ResponseCache,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("locale", &self.locale())
            .field("signed_in", &self.is_signed_in())
            .finish_non_exhaustive()
    }
}

/// Which origin a request goes to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Host {
    Amp,
    Edge,
}

/// One request before the headers are added.
pub(crate) struct Call {
    method: Method,
    url: Url,
    /// Sends the music user token; never cached.
    user: bool,
    body: Option<Value>,
}

impl Api {
    pub fn new(client: Client, locale: Locale) -> Self {
        Self::with_config(client, locale, ApiConfig::default())
    }

    pub fn with_config(client: Client, locale: Locale, config: ApiConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                edge_base: config.edge_base,
                play_activity_url: config.play_activity_url,
                developer_token: DeveloperTokenCache::default(),
                user_token: RwLock::new(None),
                locale: RwLock::new(locale),
                cache: ResponseCache::new(config.cache_bytes),
            }),
        }
    }

    /// The underlying client (lyrics, the engine's catalog calls).
    pub fn client(&self) -> &Client {
        &self.inner.client
    }

    pub fn locale(&self) -> Locale {
        self.inner
            .locale
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_locale(&self, locale: Locale) {
        *self
            .inner
            .locale
            .write()
            .unwrap_or_else(PoisonError::into_inner) = locale;
    }

    /// Sets or clears (sign-out) the music user token.
    pub fn set_user_token(&self, token: Option<UserToken>) {
        *self
            .inner
            .user_token
            .write()
            .unwrap_or_else(PoisonError::into_inner) = token;
    }

    pub fn is_signed_in(&self) -> bool {
        self.user_token().is_some()
    }

    /// The music user token, when signed in.
    pub fn user_token(&self) -> Option<UserToken> {
        self.inner
            .user_token
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The cached developer token, scraped from the web player when there is none or it
    /// has aged.
    pub async fn developer_token(
        &self,
        cancel: &CancellationToken,
    ) -> Result<DeveloperToken, Error> {
        self.inner
            .developer_token
            .get(&self.inner.client, cancel)
            .await
    }

    /// Uses `token` instead of scraping one (it is still replaced when it ages or Apple
    /// refuses it).
    pub async fn set_developer_token(&self, token: DeveloperToken) {
        self.inner.developer_token.set(token).await;
    }

    /// Drops every cached response ("refresh").
    pub fn clear_cache(&self) {
        self.inner.cache.clear();
    }

    /// The next page of a list.
    pub async fn next_page<T: DeserializeOwned>(
        &self,
        cancel: &CancellationToken,
        cursor: &Cursor,
    ) -> Result<Page<T>, Error> {
        let call = self.cursor_call(Host::Amp, cursor)?;
        self.fetch(cancel, call).await
    }

    /// `first`'s items, then every following page's, one page per stream item. Stops after
    /// [`MAX_PAGES`] pages with [`Error::TooManyPages`], and at the first error.
    pub fn pages<T>(
        &self,
        cancel: &CancellationToken,
        first: Page<T>,
    ) -> impl Stream<Item = Result<Vec<T>, Error>> + Send + 'static
    where
        T: DeserializeOwned + Send + 'static,
    {
        let api = self.clone();
        let cancel = cancel.clone();
        let state = (Some(first.data), first.next, 0usize);
        futures::stream::try_unfold(state, move |(data, next, fetched)| {
            let api = api.clone();
            let cancel = cancel.clone();
            async move {
                if let Some(data) = data {
                    return Ok(Some((data, (None, next, fetched))));
                }
                let Some(cursor) = next else {
                    return Ok(None);
                };
                if fetched + 1 >= MAX_PAGES {
                    return Err(Error::TooManyPages);
                }
                let page: Page<T> = api.next_page(&cancel, &cursor).await?;
                Ok(Some((page.data, (None, page.next, fetched + 1))))
            }
        })
    }

    fn base(&self, host: Host) -> &str {
        match host {
            Host::Amp => &self.inner.client.amp_base,
            Host::Edge => &self.inner.edge_base,
        }
    }

    /// A URL on `host` from path segments (escaped) and query pairs.
    pub(crate) fn url(
        &self,
        host: Host,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Url, Error> {
        let mut url = Url::parse(self.base(host))?;
        if segments.iter().any(|s| !valid_segment(s)) {
            return Err(Error::NotFound);
        }
        url.path_segments_mut()
            .map_err(|()| Error::Url(url::ParseError::RelativeUrlWithCannotBeABaseBase))?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    pub(crate) fn call(
        &self,
        method: Method,
        host: Host,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<Call, Error> {
        let url = self.url(host, segments, query)?;
        let user = segments.get(1) == Some(&"me");
        Ok(Call {
            method,
            url,
            user,
            body: None,
        })
    }

    /// A GET of a `next` link Apple returned.
    pub(crate) fn cursor_call(&self, host: Host, cursor: &Cursor) -> Result<Call, Error> {
        let path = cursor.as_str();
        if !path.starts_with("/v1/") {
            return Err(Error::NotFound);
        }
        let base = self.base(host).trim_end_matches('/');
        let url = Url::parse(&format!("{base}{path}"))?;
        Ok(Call {
            method: Method::GET,
            user: path.starts_with("/v1/me/"),
            url,
            body: None,
        })
    }

    pub(crate) async fn fetch<T: DeserializeOwned>(
        &self,
        cancel: &CancellationToken,
        call: Call,
    ) -> Result<T, Error> {
        let body = self.execute(cancel, call).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// Sends `call` and returns the body of a 2xx answer. See the module documentation for
    /// tokens, caching and retries.
    pub(crate) async fn execute(
        &self,
        cancel: &CancellationToken,
        call: Call,
    ) -> Result<Bytes, Error> {
        let cache = &self.inner.cache;
        let cacheable = call.method == Method::GET && !call.user;
        let key = call.url.as_str().to_owned();
        let mut etag = None;
        if cacheable {
            match cache.lookup(&key) {
                Lookup::Fresh(body) => return Ok(body),
                Lookup::Stale(tag) => etag = Some(tag),
                Lookup::Miss => {}
            }
        }
        let user_token = match call.user {
            true => Some(self.user_token().ok_or(Error::MissingUserToken)?),
            false => None,
        };

        let mut refreshed_token = false;
        let mut retries = 0;
        loop {
            let token = self.developer_token(cancel).await?;
            let mut request = self
                .inner
                .client
                .http
                .request(call.method.clone(), call.url.clone())
                .timeout(REQUEST_TIMEOUT)
                .header(AUTHORIZATION, format!("Bearer {}", token.secret()))
                .header(USER_AGENT, BROWSER_USER_AGENT)
                .header(ORIGIN, WEB_ORIGIN);
            if let Some(user_token) = &user_token {
                request = request.header("Media-User-Token", user_token.secret());
            }
            if let Some(etag) = &etag {
                request = request.header(IF_NONE_MATCH, etag.clone());
            }
            if let Some(body) = &call.body {
                request = request.json(body);
            }
            let response = cancel
                .run_until_cancelled(request.send())
                .await
                .ok_or(Error::Cancelled)??;
            let status = response.status();
            tracing::debug!(
                method = %call.method,
                path = call.url.path(),
                status = status.as_u16(),
                "apple music api"
            );

            match status {
                StatusCode::NOT_MODIFIED if etag.is_some() => {
                    if let Some(body) = cache.revalidated(&key, response.headers()) {
                        return Ok(body);
                    }
                    // Evicted meanwhile: ask again without the condition.
                    etag = None;
                }
                status if status.is_success() => {
                    let headers = response.headers().clone();
                    let body = read_body(cancel, response).await?;
                    if cacheable {
                        cache.store(key, &headers, body.clone());
                    }
                    return Ok(body);
                }
                StatusCode::UNAUTHORIZED
                    if !refreshed_token && self.inner.developer_token.refused(&token).await =>
                {
                    refreshed_token = true;
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if user_token.is_some() => {
                    return Err(Error::Unauthorized(status));
                }
                StatusCode::UNAUTHORIZED => return Err(Error::DeveloperTokenRejected(status)),
                StatusCode::NOT_FOUND => return Err(Error::NotFound),
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                    let retry_after = retry_after(response.headers(), SystemTime::now());
                    if call.method != Method::GET || retries == MAX_RETRIES {
                        return Err(match status {
                            StatusCode::TOO_MANY_REQUESTS => Error::RateLimited { retry_after },
                            _ => Error::Status(status),
                        });
                    }
                    let delay = retry_after
                        .unwrap_or(Duration::from_secs(1 << retries))
                        .min(MAX_RETRY_DELAY);
                    retries += 1;
                    tracing::debug!(?delay, "apple music api: backing off");
                    cancel
                        .run_until_cancelled(tokio::time::sleep(delay))
                        .await
                        .ok_or(Error::Cancelled)?;
                }
                _ => return Err(Error::Status(status)),
            }
        }
    }
}

/// Apple's IDs (`1440857781`, `pl.u-…`, `ra.978194965`, `i.AbC`) and fixed path words.
/// Anything else (empty, `.`, `..`, other characters) could change the path.
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && !segment.starts_with('.')
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Reads a body of at most [`MAX_BODY_BYTES`]; the rest is dropped, so an oversized body
/// fails to decode instead of exhausting memory.
async fn read_body(
    cancel: &CancellationToken,
    mut response: reqwest::Response,
) -> Result<Bytes, Error> {
    let read = async {
        let mut body = BytesMut::new();
        while body.len() < MAX_BODY_BYTES
            && let Some(chunk) = response.chunk().await?
        {
            body.extend_from_slice(&chunk);
        }
        body.truncate(MAX_BODY_BYTES);
        Ok(body.freeze())
    };
    cancel
        .run_until_cancelled(read)
        .await
        .unwrap_or(Err(Error::Cancelled))
}

/// `Retry-After` as seconds or an HTTP date.
fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use wiremock::matchers::{header, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn a_401_on_an_aged_token_rescrapes_and_retries_once() {
        let server = MockServer::start().await;
        Mock::given(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<script src="/assets/index~new.js"></script>"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/assets/index~new.js"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"t="eyJnew.eyJx.sig""#))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/catalog/us/genres"))
            .and(header("authorization", "Bearer old"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/catalog/us/genres"))
            .and(header("authorization", "Bearer eyJnew.eyJx.sig"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
        let api = Api::new(client, Locale::default());
        api.set_developer_token(DeveloperToken::new("old")).await;
        api.inner
            .developer_token
            .age(Duration::from_secs(3600))
            .await;

        let genres = api.genres(&CancellationToken::new()).await.unwrap();
        assert!(genres.is_empty());
        let token = api
            .developer_token(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(token.secret(), "eyJnew.eyJx.sig");
    }

    #[test]
    fn ids_cannot_change_the_path() {
        for ok in [
            "1440857781",
            "pl.u-8aAVZAGsor",
            "ra.978194965",
            "i.AbC_1",
            "us",
        ] {
            assert!(valid_segment(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a?b", "a#b", "%2e", "a b"] {
            assert!(!valid_segment(bad), "{bad}");
        }
    }

    #[test]
    fn retry_after_reads_seconds_and_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let h = |v: &str| {
            let mut map = HeaderMap::new();
            map.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
            map
        };
        assert_eq!(retry_after(&h("7"), now), Some(Duration::from_secs(7)));
        let date = httpdate::fmt_http_date(now + Duration::from_secs(30));
        assert_eq!(retry_after(&h(&date), now), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(&h("soon"), now), None);
        assert_eq!(retry_after(&HeaderMap::new(), now), None);
    }

    #[test]
    fn locale_falls_back_to_the_storefront_default() {
        let sf: Storefront = serde_json::from_str(
            r#"{"id":"jp","attributes":{"defaultLanguageTag":"ja","supportedLanguageTags":["ja","en-US"]}}"#,
        )
        .unwrap();
        assert_eq!(
            Locale::for_storefront(&sf, "en-us"),
            Locale::new("jp", "en-us")
        );
        assert_eq!(
            Locale::for_storefront(&sf, "de-DE"),
            Locale::new("jp", "ja")
        );
    }
}
