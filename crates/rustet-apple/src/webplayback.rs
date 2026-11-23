//! Apple's webplayback API: the music-video master playlist and the song assets by DRM flavour.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::{Cause, Error};

/// The webplayback endpoint.
pub(crate) const WEBPLAYBACK_URL: &str =
    "https://play.music.apple.com/WebObjects/MZPlay.woa/wa/webPlayback";

/// Guards against a hung Apple server blocking a session open indefinitely. The cancellation token still stops the
/// request earlier.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Cap for the API JSON.
const BODY_LIMIT: usize = 1 << 20;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36";

/// The DRM family and bitrate of a webplayback asset, named `<id>:<enc><bitrate>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetFlavor {
    /// 256 kbps AAC-LC, CTR / Widevine: `EXT-X-KEY URI="data:;base64,<kid>"`, the format the HLS parser and the
    /// Widevine licence provider understand.
    Ctr256,
    /// 64 kbps AAC-LC, CTR / Widevine.
    Ctr64,
    /// 256 kbps AAC-LC, CBCS / FairPlay: `EXT-X-KEY URI="skd://itunes.apple.com/..."`.
    Cbcs256,
    /// 64 kbps AAC-LC, CBCS / FairPlay.
    Cbcs64,
}

impl AssetFlavor {
    /// The `flavor` value in the webplayback response.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ctr256 => "28:ctrp256",
            Self::Ctr64 => "32:ctrp64",
            Self::Cbcs256 => "30:cbcp256",
            Self::Cbcs64 => "34:cbcp64",
        }
    }
}

/// One webplayback lookup.
pub(crate) struct Request<'a> {
    pub adam_id: &'a str,
    pub storefront: &'a str,
    pub token: &'a str,
    pub mut_token: &'a str,
}

/// The POST body. Field order matches Go's `json.Marshal` of a map (sorted keys).
#[derive(Serialize)]
struct Body<'a> {
    /// Without it a catalog ID from a non-US storefront matches no full licensed stream, and the endpoint serves a
    /// 30-second preview instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<String>,
    #[serde(rename = "salableAdamId")]
    salable_adam_id: &'a str,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Response {
    #[serde(rename = "songList")]
    song_list: Vec<Song>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Song {
    #[serde(rename = "hls-playlist-url")]
    hls_playlist_url: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Asset {
    flavor: String,
    #[serde(rename = "URL")]
    url: String,
}

/// Posts `req` to the webplayback endpoint at `url` and parses the answer.
pub(crate) async fn fetch(
    http: &reqwest::Client,
    url: &str,
    req: &Request<'_>,
    cancel: &CancellationToken,
) -> Result<Response, Error> {
    let body = Body {
        country: (!req.storefront.is_empty() && req.storefront != "us")
            .then(|| req.storefront.to_uppercase()),
        salable_adam_id: req.adam_id,
    };
    let request = http
        .post(url)
        .timeout(TIMEOUT)
        .json(&body)
        .bearer_auth(req.token)
        .header("x-apple-music-user-token", req.mut_token)
        .header("Origin", "https://music.apple.com")
        .header("User-Agent", USER_AGENT);
    let read = async {
        let mut response = request
            .send()
            .await
            .map_err(|e| Error::caused("webplayback", Cause::Http(e)))?;
        // Without this, a 401 (expired token) or 5xx outage surfaced as "no song list", hiding the cause from users
        // and from the open circuit breaker.
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::caused(
                "webplayback",
                Cause::Status(response.status()),
            ));
        }
        let mut raw = Vec::new();
        while raw.len() < BODY_LIMIT
            && let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| Error::caused("webplayback", Cause::Http(e)))?
        {
            raw.extend_from_slice(&chunk);
        }
        raw.truncate(BODY_LIMIT);
        serde_json::from_slice(&raw)
            .map_err(|e| Error::caused("parse webplayback response", Cause::Json(e)))
    };
    cancel
        .run_until_cancelled(read)
        .await
        .unwrap_or(Err(Error::Cancelled))
}

impl Response {
    /// The HLS master playlist of a music video.
    pub(crate) fn playlist_url(self) -> Result<String, Error> {
        self.song_list
            .into_iter()
            .next()
            .map(|s| s.hls_playlist_url)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| Error::unavailable("webplayback response contained no HLS playlist"))
    }

    /// The media playlist of the song's asset of `flavor`.
    pub(crate) fn asset_url(self, flavor: AssetFlavor) -> Result<String, Error> {
        let song = self
            .song_list
            .into_iter()
            .next()
            .ok_or_else(|| Error::unavailable("webplayback response contained no song list"))?;
        song.assets
            .into_iter()
            .find(|a| a.flavor == flavor.as_str())
            .map(|a| a.url)
            .ok_or_else(|| {
                Error::unavailable(format!(
                    "webplayback response has no asset with flavor {:?}",
                    flavor.as_str()
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_json, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn request(storefront: &str) -> Request<'_> {
        Request {
            adam_id: "1440833098",
            storefront,
            token: "dev-token",
            mut_token: "user-token",
        }
    }

    async fn serve(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn sends_the_apple_headers_and_the_country_outside_the_us() {
        let server = MockServer::start().await;
        for (storefront, json) in [
            (
                "in",
                serde_json::json!({"country": "IN", "salableAdamId": "1440833098"}),
            ),
            ("us", serde_json::json!({"salableAdamId": "1440833098"})),
            ("", serde_json::json!({"salableAdamId": "1440833098"})),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .and(header("authorization", "Bearer dev-token"))
                .and(header("x-apple-music-user-token", "user-token"))
                .and(header("origin", "https://music.apple.com"))
                .and(header("user-agent", USER_AGENT))
                .and(header("content-type", "application/json"))
                .and(body_json(json))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(include_str!("../tests/data/webplayback_response.json")),
                )
                .expect(1)
                .mount(&server)
                .await;
            let resp = fetch(
                &reqwest::Client::new(),
                &server.uri(),
                &request(storefront),
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("storefront {storefront:?}: {e}"));
            assert_eq!(
                resp.playlist_url().unwrap(),
                "https://test.example.com/master.m3u8"
            );
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn picks_the_asset_of_the_requested_flavor() {
        let body = r#"{"songList":[{"assets":[
            {"flavor":"30:cbcp256","URL":"https://cdn/cbcs.m3u8"},
            {"flavor":"28:ctrp256","URL":"https://cdn/ctr.m3u8"}]}]}"#;
        let server = serve(200, body).await;
        let http = reqwest::Client::new();
        let resp = fetch(
            &http,
            &server.uri(),
            &request("us"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            resp.asset_url(AssetFlavor::Ctr256).unwrap(),
            "https://cdn/ctr.m3u8"
        );

        let resp = fetch(
            &http,
            &server.uri(),
            &request("us"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let err = resp.asset_url(AssetFlavor::Ctr64).unwrap_err();
        assert!(matches!(err, Error::Unavailable(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            r#"webplayback response has no asset with flavor "32:ctrp64""#
        );
    }

    #[tokio::test]
    async fn an_empty_answer_is_unavailable() {
        let server = serve(200, r#"{"songList":[]}"#).await;
        let http = reqwest::Client::new();
        let resp = fetch(
            &http,
            &server.uri(),
            &request("us"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(resp.playlist_url(), Err(Error::Unavailable(_))));
        let resp = fetch(
            &http,
            &server.uri(),
            &request("us"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            resp.asset_url(AssetFlavor::Ctr256),
            Err(Error::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn the_status_is_reported_and_classified() {
        for (status, check) in [
            (
                401,
                (|e| matches!(e, Error::Authentication(_))) as fn(&Error) -> bool,
            ),
            (503, |e| matches!(e, Error::Transport(_))),
            (404, |e| matches!(e, Error::NotFound(_))),
        ] {
            let server = serve(status, "").await;
            let err = fetch(
                &reqwest::Client::new(),
                &server.uri(),
                &request("us"),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert!(check(&err), "HTTP {status}: {err:?}");
            assert_eq!(err.to_string(), format!("webplayback: HTTP {status}"));
        }
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_transport_failure() {
        // Port 9 (discard) on loopback: nothing listens, so the connect is refused.
        let err = fetch(
            &reqwest::Client::new(),
            "http://127.0.0.1:9",
            &request("us"),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Transport(_)), "{err:?}");
    }

    #[tokio::test]
    async fn cancellation_stops_the_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = fetch(
            &reqwest::Client::new(),
            &server.uri(),
            &request("us"),
            &cancel,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }
}
