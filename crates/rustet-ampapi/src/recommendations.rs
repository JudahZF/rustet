//! Personalised "me" feeds.

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::{Client, Error};

/// Bounds a feed response body.
const MAX_FEED_BYTES: usize = 8 << 20;

/// One of the personalised feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecommendationKind {
    /// The "Listen Now" feed.
    Recommendations,
    /// The user's most-played content.
    HeavyRotation,
    /// Recently played containers.
    RecentlyPlayed,
}

impl RecommendationKind {
    fn path(self) -> &'static str {
        match self {
            Self::Recommendations => "/v1/me/recommendations",
            Self::HeavyRotation => "/v1/me/history/heavy-rotation",
            Self::RecentlyPlayed => "/v1/me/recent/played",
        }
    }
}

/// Tunes a feed request. `None` leaves Apple's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecommendationsOptions {
    /// BCP-47 tag, for example `en-US`.
    pub language: Option<String>,
    /// 1..=50.
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    /// Comma-separated resource types filter.
    pub types: Option<String>,
}

impl Client {
    /// Fetches a personalised feed. The developer token authorises the app (an empty
    /// one is replaced by a scraped token); the Music-User-Token identifies the listener.
    ///
    /// The response body is returned verbatim, so callers see every field Apple adds.
    /// A non-200 answer is [`Error::Feed`] with Apple's status.
    pub async fn recommendations(
        &self,
        cancel: &CancellationToken,
        kind: RecommendationKind,
        token: &str,
        music_user_token: &str,
        options: &RecommendationsOptions,
    ) -> Result<Bytes, Error> {
        let token = self.bearer(token).await?;
        if music_user_token.is_empty() {
            return Err(Error::MissingUserToken);
        }
        let query = [
            ("l", options.language.clone()),
            ("limit", options.limit.map(|n| n.to_string())),
            ("offset", options.offset.map(|n| n.to_string())),
            ("types", options.types.clone()),
        ];
        let query: Vec<_> = query
            .iter()
            .filter_map(|(k, v)| Some((*k, v.as_deref()?)))
            .collect();
        let request = self
            .api_get(format!("{}{}", self.amp_base, kind.path()), &token)
            .header("Music-User-Token", music_user_token)
            .query(&query);
        let (status, body) = self.fetch(cancel, request, Some(MAX_FEED_BYTES)).await?;
        if status != reqwest::StatusCode::OK {
            return Err(Error::Feed { status, body });
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn feed_is_returned_verbatim_and_status_is_kept() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/me/history/heavy-rotation"))
            .and(header("music-user-token", "mut"))
            .and(query_param("limit", "5"))
            .and(query_param("l", "en-US"))
            .and(query_param_is_missing("offset"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"data":[{"id":"x","new":1}]}"#),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v1/me/recommendations"))
            .respond_with(ResponseTemplate::new(403).set_body_string("no subscription"))
            .mount(&server)
            .await;
        let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
        let cancel = CancellationToken::new();
        let options = RecommendationsOptions {
            language: Some("en-US".into()),
            limit: Some(5),
            ..Default::default()
        };

        let body = client
            .recommendations(
                &cancel,
                RecommendationKind::HeavyRotation,
                "tok",
                "mut",
                &options,
            )
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"data":[{"id":"x","new":1}]}"#);

        let err = client
            .recommendations(
                &cancel,
                RecommendationKind::Recommendations,
                "tok",
                "mut",
                &options,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Feed { status, body } if *status == 403 && &body[..] == b"no subscription")
        );
        assert_eq!(err.to_string(), "apple music api: HTTP 403");
    }
}
