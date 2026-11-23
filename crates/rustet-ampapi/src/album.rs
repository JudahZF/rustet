//! Catalog albums.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::track::TrackPage;
use crate::types::{Artwork, List, Page, PlayParams, Relationship, Resource};
use crate::{Client, Error};

/// The response of the album endpoint.
pub type AlbumResp = Page<Album>;

/// A catalog album. Playlists use the same shape (see [`Playlist`](crate::Playlist)).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Album {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub href: String,
    pub attributes: AlbumAttributes,
    pub relationships: AlbumRelationships,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AlbumAttributes {
    pub artwork: Artwork,
    pub artist_name: String,
    pub is_single: bool,
    pub url: String,
    pub is_complete: bool,
    pub genre_names: List<String>,
    pub track_count: u32,
    pub is_mastered_for_itunes: bool,
    pub is_apple_digital_master: bool,
    pub content_rating: String,
    pub release_date: String,
    pub name: String,
    pub record_label: String,
    pub upc: String,
    pub audio_traits: List<String>,
    pub copyright: String,
    pub play_params: PlayParams,
    pub is_compilation: bool,
    pub editorial_video: EditorialVideo,
}

/// Motion artwork.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorialVideo {
    #[serde(rename = "motionTallVideo3x4")]
    pub motion_tall: VideoAsset,
    #[serde(rename = "motionSquareVideo1x1")]
    pub motion_square: VideoAsset,
    #[serde(rename = "motionDetailTall")]
    pub motion_detail_tall: VideoAsset,
    #[serde(rename = "motionDetailSquare")]
    pub motion_detail_square: VideoAsset,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoAsset {
    /// HLS playlist URL.
    pub video: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumRelationships {
    /// Kept as raw JSON: no caller reads it, the renderer gets it as is.
    #[serde(rename = "record-labels")]
    pub record_labels: Relationship<Value>,
    pub artists: Relationship<AlbumArtist>,
    pub tracks: TrackPage,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumArtistAttributes {
    pub name: String,
    pub artwork: ArtworkUrl,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtworkUrl {
    pub url: String,
}

pub type AlbumArtist = Resource<AlbumArtistAttributes>;

impl Client {
    /// Fetches an album with every track page, record labels and motion artwork, for
    /// the renderer. An empty `token` is replaced by a scraped developer token.
    pub async fn catalog_album(
        &self,
        storefront: &str,
        id: &str,
        language: &str,
        token: &str,
    ) -> Result<AlbumResp, Error> {
        let cancel = CancellationToken::new();
        let token = self.bearer(token).await?;
        let request = self
            .api_get(
                format!("{}/v1/catalog/{storefront}/albums/{id}", self.amp_base),
                &token,
            )
            .query(&[
                ("extend", "editorialVideo,extendedAssetUrls"),
                ("include", "tracks,artists,record-labels"),
                ("include[songs]", "artists"),
                ("l", language),
                ("omit[resource]", "autos"),
            ]);
        let mut resp: AlbumResp = self.get_json(&cancel, request).await?;
        let album = resp.data.first_mut().ok_or(Error::Empty("album"))?;
        let next = album.relationships.tracks.next.clone();
        self.append_track_pages(
            &cancel,
            next,
            &token,
            "",
            &mut album.relationships.tracks.data,
        )
        .await?;
        Ok(resp)
    }

    /// Fetches an album's metadata and first page of tracks. `token` is used as is.
    pub async fn album(
        &self,
        cancel: &CancellationToken,
        storefront: &str,
        id: &str,
        language: &str,
        token: &str,
    ) -> Result<AlbumResp, Error> {
        let request = self
            .api_get(
                format!("{}/v1/catalog/{storefront}/albums/{id}", self.amp_base),
                token,
            )
            .query(&[
                ("include", "tracks,artists"),
                ("include[songs]", "artists"),
                ("l", language),
                ("omit[resource]", "autos"),
            ]);
        self.get_json(cancel, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn catalog_album_appends_track_pages() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/catalog/us/albums/1"))
            .and(query_param("include", "tracks,artists,record-labels"))
            .and(query_param("l", "en-US"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":[{"id":"1","relationships":{"tracks":{"next":"/v1/catalog/us/albums/1/tracks?offset=1","data":[{"id":"a"}]}}}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(path("/v1/catalog/us/albums/1/tracks"))
            .and(query_param("offset", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"data":[{"id":"b","type":"songs"}]}"#),
            )
            .mount(&server)
            .await;
        let client = Client::with_base_urls(reqwest::Client::new(), server.uri(), server.uri());
        let resp = client
            .catalog_album("us", "1", "en-US", "tok")
            .await
            .unwrap();
        let ids: Vec<_> = resp.data[0]
            .relationships
            .tracks
            .data
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    /// The album response is re-serialised to the renderer: Go field names, every field
    /// present, absent arrays as `null`.
    #[test]
    fn album_reserialises_with_go_field_names() {
        let resp: AlbumResp = serde_json::from_str(r#"{"data":[{"id":"1","attributes":{"name":"A","isAppleDigitalMaster":true,"unknown":1}}]}"#).unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        let album = &json["data"][0];
        assert_eq!(album["attributes"]["name"], "A");
        assert_eq!(album["attributes"]["isAppleDigitalMaster"], true);
        assert_eq!(album["attributes"]["genreNames"], Value::Null);
        assert!(album["attributes"].get("unknown").is_none());
        assert_eq!(
            album["attributes"]["editorialVideo"]["motionTallVideo3x4"]["video"],
            ""
        );
        assert_eq!(album["relationships"]["record-labels"]["data"], Value::Null);
        assert_eq!(album["relationships"]["tracks"]["next"], "");
        assert_eq!(json["next"], "");
    }

    #[tokio::test]
    #[ignore = "calls Apple's servers"]
    async fn live_catalog_album() {
        let client = Client::new(reqwest::Client::new());
        let resp = client
            .catalog_album("us", "1440857781", "en-US", "")
            .await
            .unwrap();
        assert!(!resp.data[0].relationships.tracks.data.is_empty());
    }
}
