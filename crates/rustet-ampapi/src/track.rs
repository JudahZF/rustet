//! Tracks of an album or playlist, and the `next`-link pagination that collects them.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::types::{
    Artist, Artwork, ExtendedAssetUrls, List, Page, PlayParams, Preview, Relationship, Resource,
};
use crate::{Client, Error};

/// Bounds `next`-link pagination so a looping or runaway API response cannot keep a
/// request going forever.
pub(crate) const MAX_TRACK_PAGES: usize = 200;

/// One page of an album's or playlist's tracks.
pub type TrackPage = Page<Track>;

/// An album or playlist track: `type` is `"songs"` or `"music-videos"`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Track {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub href: String,
    pub attributes: TrackAttributes,
    pub relationships: TrackRelationships,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackAttributes {
    pub previews: List<Preview>,
    pub artwork: Artwork,
    pub artist_name: String,
    pub url: String,
    pub disc_number: u32,
    pub genre_names: List<String>,
    pub extended_asset_urls: ExtendedAssetUrls,
    pub has_time_synced_lyrics: bool,
    pub is_mastered_for_itunes: bool,
    pub is_apple_digital_master: bool,
    pub content_rating: String,
    pub duration_in_millis: u64,
    pub release_date: String,
    pub name: String,
    pub isrc: String,
    pub audio_traits: List<String>,
    pub has_lyrics: bool,
    pub album_name: String,
    pub play_params: PlayParams,
    pub track_number: u32,
    pub audio_locale: String,
    pub composer_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackRelationships {
    pub artists: Relationship<Artist>,
    pub albums: Relationship<TrackAlbum>,
}

/// The album of a track. Unlike [`RelatedAlbumAttributes`](crate::RelatedAlbumAttributes)
/// it has no record label or copyright.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackAlbumAttributes {
    pub artist_name: String,
    pub artwork: Artwork,
    pub genre_names: List<String>,
    pub is_compilation: bool,
    pub is_complete: bool,
    pub is_mastered_for_itunes: bool,
    pub is_prerelease: bool,
    pub is_single: bool,
    pub name: String,
    pub play_params: PlayParams,
    pub release_date: String,
    pub track_count: u32,
    pub upc: String,
    pub url: String,
}

pub type TrackAlbum = Resource<TrackAlbumAttributes>;

impl Client {
    /// Follows Apple's `next` links from `next` and appends the tracks of every
    /// following page to `out`. Pages fetched before an error stay in `out`.
    ///
    /// `media_user_token` may be empty; user-shared playlists need it on every page.
    pub(crate) async fn append_track_pages(
        &self,
        cancel: &CancellationToken,
        mut next: String,
        token: &str,
        media_user_token: &str,
        out: &mut List<Track>,
    ) -> Result<(), Error> {
        // Set (replacing any value in the link), then sorted by key like Go's url.Values.
        const OVERRIDES: [(&str, &str); 3] = [
            ("extend", "editorialVideo,extendedAssetUrls"),
            ("include", "artists"),
            ("omit[resource]", "autos"),
        ];
        let mut pages = 0;
        while !next.is_empty() {
            if pages >= MAX_TRACK_PAGES {
                return Err(Error::TooManyPages);
            }
            pages += 1;

            let mut url = url::Url::parse(&format!("{}{next}", self.amp_base))?;
            let mut query: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| !OVERRIDES.iter().any(|(o, _)| o == k))
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .chain(OVERRIDES.map(|(k, v)| (k.to_owned(), v.to_owned())))
                .collect();
            query.sort_by(|a, b| a.0.cmp(&b.0));
            url.query_pairs_mut().clear().extend_pairs(&query);

            let mut request = self.api_get(url, token);
            if !media_user_token.is_empty() {
                request = request.header("Media-User-Token", media_user_token);
            }
            let page: TrackPage = self.get_json(cancel, request).await?;
            out.extend(page.data);
            next = page.next;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> Client {
        Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri())
    }

    #[tokio::test]
    async fn follows_next_and_sends_media_user_token() {
        let server = MockServer::start().await;
        Mock::given(path("/p2"))
            .and(header("media-user-token", "mut"))
            .and(header("authorization", "Bearer tok"))
            .and(query_param("offset", "100"))
            .and(query_param("include", "artists"))
            .and(query_param("omit[resource]", "autos"))
            .and(query_param("extend", "editorialVideo,extendedAssetUrls"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"next":"/p3","data":[{"id":"2"}]}"#),
            )
            .mount(&server)
            .await;
        Mock::given(path("/p3"))
            .and(header("media-user-token", "mut"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[{"id":"3"}]}"#))
            .mount(&server)
            .await;

        let mut out = List::default();
        client(&server)
            .append_track_pages(
                &CancellationToken::new(),
                "/p2?offset=100&include=x".into(),
                "tok",
                "mut",
                &mut out,
            )
            .await
            .unwrap();
        let ids: Vec<_> = out.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["2", "3"]);
    }

    #[tokio::test]
    async fn caps_runaway_pagination() {
        let server = MockServer::start().await;
        Mock::given(path("/again"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"next":"/again","data":[]}"#),
            )
            .expect(MAX_TRACK_PAGES as u64)
            .mount(&server)
            .await;
        let mut out = List::default();
        let err = client(&server)
            .append_track_pages(
                &CancellationToken::new(),
                "/again".into(),
                "tok",
                "",
                &mut out,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::TooManyPages), "{err}");
    }

    #[tokio::test]
    async fn failed_page_keeps_earlier_tracks() {
        let server = MockServer::start().await;
        Mock::given(path("/p1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"next":"/p2","data":[{"id":"1"}]}"#),
            )
            .mount(&server)
            .await;
        Mock::given(path("/p2"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let mut out = List::default();
        let err = client(&server)
            .append_track_pages(&CancellationToken::new(), "/p1".into(), "tok", "", &mut out)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Status(s) if s == 500));
        assert_eq!(out.len(), 1);
    }
}
