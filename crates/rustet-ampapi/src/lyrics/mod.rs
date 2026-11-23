//! Apple Music lyrics: the TTML fetch and its conversion to LRC, SRT and WebVTT.

mod dom;
mod ttml;

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, COOKIE, ORIGIN, REFERER};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

pub use ttml::{ttml_to_lrc, ttml_to_srt, ttml_to_vtt};

use crate::{Client, Error};

/// The lyrics request has a shorter deadline than the catalog calls.
const LYRICS_TIMEOUT: Duration = Duration::from_secs(15);
/// Bounds the lyrics response body.
const MAX_LYRICS_BYTES: usize = 8 << 20;

/// Why a TTML document could not be converted.
#[derive(Debug, thiserror::Error)]
pub enum TtmlError {
    #[error("invalid TTML: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("invalid TTML: {0}")]
    Attribute(#[from] quick_xml::events::attributes::AttrError),
    #[error("invalid TTML: {0}")]
    Encoding(#[from] quick_xml::encoding::EncodingError),
    #[error("invalid TTML: {0}")]
    Escape(#[from] quick_xml::escape::EscapeError),
    #[error("invalid TTML: unknown entity &{0};")]
    UnknownEntity(String),
    #[error("invalid TTML: unbalanced elements")]
    Malformed,
    #[error("no <tt> element")]
    NoTt,
    #[error("no <body> in TTML")]
    NoBody,
    #[error("no synchronised lyrics")]
    NotSynced,
    #[error("invalid TTML time {0:?}")]
    InvalidTime(String),
}

/// The output format of [`Client::lyrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LyricsFormat {
    /// Apple's TTML, unchanged.
    Ttml,
    /// WebVTT subtitles.
    Vtt,
    /// SubRip subtitles.
    Srt,
    /// LRC; enhanced (syllable) LRC for word-timed lyrics.
    #[default]
    Lrc,
}

impl From<&str> for LyricsFormat {
    /// `"ttml"`, `"vtt"` and `"srt"`; anything else is LRC.
    fn from(s: &str) -> Self {
        match s {
            "ttml" => Self::Ttml,
            "vtt" => Self::Vtt,
            "srt" => Self::Srt,
            _ => Self::Lrc,
        }
    }
}

/// One lyrics request.
#[derive(Debug, Clone, Copy)]
pub struct LyricsRequest<'a> {
    pub storefront: &'a str,
    pub song_id: &'a str,
    /// The catalog relationship: `"lyrics"` or `"syllable-lyrics"`.
    pub kind: &'a str,
    pub language: &'a str,
    pub format: LyricsFormat,
    /// Developer token (sent as is, not fetched when empty).
    pub token: &'a str,
    pub media_user_token: &'a str,
}

#[derive(Deserialize)]
struct SongLyrics {
    #[serde(default)]
    data: Vec<SongLyricsData>,
}

#[derive(Deserialize)]
struct SongLyricsData {
    #[serde(default)]
    attributes: SongLyricsAttributes,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct SongLyricsAttributes {
    ttml: String,
    ttml_localizations: String,
}

impl Client {
    /// Fetches a song's lyrics as TTML and converts them to `req.format`.
    ///
    /// Needs the listener's media-user-token: lyrics are not available anonymously.
    pub async fn lyrics(
        &self,
        cancel: &CancellationToken,
        req: &LyricsRequest<'_>,
    ) -> Result<String, Error> {
        // Anything shorter is not a real token.
        if req.media_user_token.len() < 50 {
            return Err(Error::MissingUserToken);
        }
        let ttml = self.lyrics_ttml(cancel, req).await?;
        Ok(match req.format {
            LyricsFormat::Ttml => ttml,
            LyricsFormat::Vtt => ttml_to_vtt(&ttml)?,
            LyricsFormat::Srt => ttml_to_srt(&ttml)?,
            LyricsFormat::Lrc => ttml_to_lrc(&ttml)?,
        })
    }

    async fn lyrics_ttml(
        &self,
        cancel: &CancellationToken,
        req: &LyricsRequest<'_>,
    ) -> Result<String, Error> {
        let mut url = url::Url::parse(&self.amp_base)?;
        url.path_segments_mut()
            .map_err(|()| Error::Url(url::ParseError::RelativeUrlWithCannotBeABaseBase))?
            .pop_if_empty()
            .extend([
                "v1",
                "catalog",
                req.storefront,
                "songs",
                req.song_id,
                req.kind,
            ]);
        url.query_pairs_mut()
            .append_pair("l", req.language)
            .append_pair("extend", "ttmlLocalizations");

        let request = self
            .http
            .get(url)
            .timeout(LYRICS_TIMEOUT)
            .header(ORIGIN, crate::client::WEB_ORIGIN)
            .header(REFERER, "https://music.apple.com/")
            .header(AUTHORIZATION, format!("Bearer {}", req.token))
            .header(COOKIE, format!("media-user-token={}", req.media_user_token));
        let (status, body) = self.fetch(cancel, request, Some(MAX_LYRICS_BYTES)).await?;

        // Any body without lyrics data is reported by status only: the message reaches
        // the HTTP client, Apple's body must not.
        let lyrics: Option<SongLyrics> = serde_json::from_slice(&body).ok();
        let attrs = lyrics
            .and_then(|l| l.data.into_iter().next())
            .map(|d| d.attributes)
            .ok_or(Error::LyricsUnavailable(status))?;
        Ok(if attrs.ttml.is_empty() {
            attrs.ttml_localizations
        } else {
            attrs.ttml
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MUT: &str = "m0123456789012345678901234567890123456789012345678901234567890";

    fn request(server: &MockServer) -> (Client, LyricsRequest<'static>) {
        let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
        let req = LyricsRequest {
            storefront: "us",
            song_id: "1 2",
            kind: "syllable-lyrics",
            language: "en-US",
            format: LyricsFormat::Lrc,
            token: "tok",
            media_user_token: MUT,
        };
        (client, req)
    }

    #[tokio::test]
    async fn fetches_ttml_with_cookie_and_converts() {
        let server = MockServer::start().await;
        let ttml = r#"<tt itunes:timing="None"><body><div><p>Line</p></div></body></tt>"#;
        Mock::given(method("GET"))
            .and(path("/v1/catalog/us/songs/1%202/syllable-lyrics"))
            .and(query_param("l", "en-US"))
            .and(query_param("extend", "ttmlLocalizations"))
            .and(header("authorization", "Bearer tok"))
            .and(header("cookie", format!("media-user-token={MUT}").as_str()))
            .and(header("referer", "https://music.apple.com/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"attributes": {"ttml": ttml}}]
            })))
            .mount(&server)
            .await;
        let (client, req) = request(&server);
        let cancel = CancellationToken::new();
        assert_eq!(client.lyrics(&cancel, &req).await.unwrap(), "Line");
    }

    #[tokio::test]
    async fn missing_lyrics_report_status_not_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_string("secret apple body"))
            .mount(&server)
            .await;
        let (client, req) = request(&server);
        let err = client
            .lyrics(&CancellationToken::new(), &req)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LyricsUnavailable(s) if s == 404));
        assert_eq!(err.to_string(), "failed to get lyrics: HTTP 404");
    }

    #[tokio::test]
    async fn short_media_user_token_is_rejected() {
        let server = MockServer::start().await;
        let (client, mut req) = request(&server);
        req.media_user_token = "short";
        let err = client
            .lyrics(&CancellationToken::new(), &req)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::MissingUserToken));
    }
}
