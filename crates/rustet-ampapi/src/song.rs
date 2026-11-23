//! Catalog songs.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::types::{
    Artwork, ExtendedAssetUrls, List, MediaRelationships, Page, PlayParams, Preview,
};
use crate::{Client, Error};

/// The response of the song endpoint.
pub type SongResp = Page<Song>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Song {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub href: String,
    pub attributes: SongAttributes,
    pub relationships: MediaRelationships,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SongAttributes {
    pub previews: List<Preview>,
    pub artwork: Artwork,
    pub artist_name: String,
    pub url: String,
    pub disc_number: u32,
    pub genre_names: List<String>,
    pub has_time_synced_lyrics: bool,
    pub is_mastered_for_itunes: bool,
    pub is_apple_digital_master: bool,
    pub content_rating: String,
    pub duration_in_millis: u64,
    pub release_date: String,
    pub name: String,
    pub extended_asset_urls: ExtendedAssetUrls,
    pub isrc: String,
    /// For example `"lossless"`, `"hi-res-lossless"`, `"atmos"`.
    pub audio_traits: List<String>,
    pub has_lyrics: bool,
    pub album_name: String,
    pub play_params: PlayParams,
    pub track_number: u32,
    pub audio_locale: String,
    pub composer_name: String,
}

impl Client {
    /// Fetches a song with its album and artists and the enhanced HLS URL. An empty
    /// `token` is replaced by a scraped developer token.
    pub async fn song(
        &self,
        cancel: &CancellationToken,
        storefront: &str,
        id: &str,
        language: &str,
        token: &str,
    ) -> Result<SongResp, Error> {
        let token = self.bearer(token).await?;
        let request = self
            .api_get(
                format!("{}/v1/catalog/{storefront}/songs/{id}", self.amp_base),
                &token,
            )
            .query(&[
                ("extend", "extendedAssetUrls"),
                ("include", "albums,artists"),
                ("l", language),
            ]);
        self.get_json(cancel, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn song_request_headers_and_decoding() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/catalog/gb/songs/42"))
            .and(query_param("extend", "extendedAssetUrls"))
            .and(query_param("include", "albums,artists"))
            .and(query_param("l", "en-GB"))
            .and(header("authorization", "Bearer tok"))
            .and(header("origin", "https://music.apple.com"))
            // `header()` splits values at commas, and the user agent has one.
            .and(|r: &wiremock::Request| {
                r.headers.get("user-agent").is_some_and(|v| v == crate::client::BROWSER_USER_AGENT)
            })
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":[{"id":"42","attributes":{"name":"S","durationInMillis":1000,"extendedAssetUrls":{"enhancedHls":"https://x/m.m3u8"}},"relationships":{"albums":{"data":[{"attributes":{"recordLabel":"L","trackCount":9}}]}}}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(path("/v1/catalog/gb/songs/404"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
        let cancel = CancellationToken::new();

        let song = client
            .song(&cancel, "gb", "42", "en-GB", "tok")
            .await
            .unwrap();
        let a = &song.data[0].attributes;
        assert_eq!((a.name.as_str(), a.duration_in_millis), ("S", 1000));
        assert_eq!(a.extended_asset_urls.enhanced_hls, "https://x/m.m3u8");
        let album = &song.data[0].relationships.albums.data[0].attributes;
        assert_eq!((album.record_label.as_str(), album.track_count), ("L", 9));

        let err = client
            .song(&cancel, "gb", "404", "en-GB", "tok")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "404 Not Found");
    }

    #[tokio::test]
    async fn cancelled_request_returns_cancelled() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/catalog/us/songs/1"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(10)))
            .mount(&server)
            .await;
        let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = client
            .song(&cancel, "us", "1", "", "tok")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }
}
