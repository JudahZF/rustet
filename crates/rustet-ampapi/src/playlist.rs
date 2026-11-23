//! Catalog playlists and user library playlists.

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::album::Album;
use crate::types::Page;
use crate::{Client, Error};

/// A catalog playlist. The Go engine decoded playlists into the album shape, and the
/// renderer receives that shape, so it is kept.
pub type Playlist = Album;
/// The response of the playlist endpoint.
pub type PlaylistResp = Page<Playlist>;

/// One track of a user library playlist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LibraryPlaylistTrack {
    pub id: String,
    /// `"library-songs"` or `"library-music-videos"`.
    #[serde(rename = "type")]
    pub r#type: String,
    pub attributes: LibraryTrackAttributes,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryTrackAttributes {
    pub name: String,
    pub artist_name: String,
    pub play_params: LibraryPlayParams,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LibraryPlayParams {
    pub catalog_id: String,
    pub reporting_id: String,
    /// `"song"` or `"musicVideo"`.
    pub kind: String,
}

impl LibraryPlayParams {
    /// The catalog ID to download; see [`crate::model::PlayParams::stream_catalog_id`].
    pub fn stream_catalog_id(&self) -> Option<&str> {
        crate::model::stream_catalog_id(Some(&self.reporting_id), Some(&self.catalog_id))
    }
}

#[derive(Deserialize)]
struct LibraryTrackPage {
    #[serde(default)]
    data: Vec<LibraryPlaylistTrack>,
    #[serde(default)]
    next: String,
}

impl Client {
    /// Fetches a catalog playlist (Apple-curated or user-shared) with every track page.
    ///
    /// A failed later page keeps the tracks fetched so far: a partial playlist rather
    /// than none. An empty `token` is replaced by a scraped developer token;
    /// `media_user_token` may be empty.
    pub async fn playlist(
        &self,
        cancel: &CancellationToken,
        storefront: &str,
        id: &str,
        language: &str,
        token: &str,
        media_user_token: &str,
    ) -> Result<PlaylistResp, Error> {
        let token = self.bearer(token).await?;
        let mut request = self
            .api_get(
                format!("{}/v1/catalog/{storefront}/playlists/{id}", self.amp_base),
                &token,
            )
            .query(&[
                ("include", "tracks,artists"),
                ("include[songs]", "artists"),
                ("l", language),
            ]);
        if !media_user_token.is_empty() {
            request = request.header("Media-User-Token", media_user_token);
        }
        let mut resp: PlaylistResp = self.get_json(cancel, request).await?;
        let playlist = resp.data.first_mut().ok_or(Error::Empty("playlist"))?;
        let next = playlist.relationships.tracks.next.clone();
        let tracks = &mut playlist.relationships.tracks.data;
        if let Err(err) = self
            .append_track_pages(cancel, next, &token, media_user_token, tracks)
            .await
        {
            tracing::warn!("playlist {id}: pagination stopped: {err}");
        }
        Ok(resp)
    }

    /// Fetches a playlist with every track page, record labels and motion artwork, for
    /// the renderer. Any failed page fails the call. An empty `token` is replaced by a
    /// scraped developer token.
    pub async fn catalog_playlist(
        &self,
        storefront: &str,
        id: &str,
        language: &str,
        token: &str,
    ) -> Result<PlaylistResp, Error> {
        let cancel = CancellationToken::new();
        let token = self.bearer(token).await?;
        let request = self
            .api_get(
                format!("{}/v1/catalog/{storefront}/playlists/{id}", self.amp_base),
                &token,
            )
            .query(&[
                ("extend", "editorialVideo,extendedAssetUrls"),
                ("include", "tracks,artists,record-labels"),
                ("include[songs]", "artists"),
                ("l", language),
                ("omit[resource]", "autos"),
            ]);
        let mut resp: PlaylistResp = self.get_json(&cancel, request).await?;
        let playlist = resp.data.first_mut().ok_or(Error::Empty("playlist"))?;
        let next = playlist.relationships.tracks.next.clone();
        self.append_track_pages(
            &cancel,
            next,
            &token,
            "",
            &mut playlist.relationships.tracks.data,
        )
        .await?;
        Ok(resp)
    }

    /// Fetches every track of a user library playlist (IDs starting with `p.`). Needs
    /// the listener's media-user-token. Never returns an empty list.
    pub async fn library_playlist_tracks(
        &self,
        cancel: &CancellationToken,
        id: &str,
        language: &str,
        token: &str,
        media_user_token: &str,
    ) -> Result<Vec<LibraryPlaylistTrack>, Error> {
        if media_user_token.is_empty() {
            return Err(Error::MissingUserToken);
        }
        let token = self.bearer(token).await?;
        let mut next = url::Url::parse_with_params(
            &format!("{}/v1/me/library/playlists/{id}/tracks", self.amp_base),
            [
                ("include[library-music-videos]", "catalog"),
                ("include[library-songs]", "catalog"),
                ("l", language),
                ("limit", "100"),
            ],
        )?
        .to_string();

        let mut tracks = Vec::new();
        while !next.is_empty() {
            let request = self
                .api_get(&next, &token)
                .header("Media-User-Token", media_user_token);
            let (status, body) = self.fetch(cancel, request, None).await?;
            // The body is decoded first: an undecodable error page reports the JSON error.
            let page: LibraryTrackPage = serde_json::from_slice(&body)?;
            if status != reqwest::StatusCode::OK {
                return Err(Error::LibraryStatus(status));
            }
            tracks.extend(page.data);
            next = if page.next.starts_with('/') {
                format!("{}{}", self.amp_base, page.next)
            } else {
                page.next
            };
        }
        if tracks.is_empty() {
            return Err(Error::LibraryPlaylistEmpty(id.to_owned()));
        }
        Ok(tracks)
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
    async fn library_playlist_follows_relative_next_links() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/me/library/playlists/p.1/tracks"))
            .and(query_param("limit", "100"))
            .and(header("media-user-token", "mut"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"next":"/v1/me/library/playlists/p.1/tracks?offset=100","data":[{"id":"i.1","type":"library-songs","attributes":{"playParams":{"catalogId":"11"}}}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(path("/v1/me/library/playlists/p.1/tracks"))
            .and(query_param("offset", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":[{"id":"i.2","type":"library-music-videos","attributes":{"playParams":{"catalogId":"22"}}}]}"#,
            ))
            .mount(&server)
            .await;
        let tracks = client(&server)
            .library_playlist_tracks(&CancellationToken::new(), "p.1", "en-US", "tok", "mut")
            .await
            .unwrap();
        let ids: Vec<_> = tracks
            .iter()
            .map(|t| t.attributes.play_params.catalog_id.as_str())
            .collect();
        assert_eq!(ids, ["11", "22"]);
    }

    #[tokio::test]
    async fn library_playlist_errors() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/me/library/playlists/p.401/tracks"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"errors":[]}"#))
            .mount(&server)
            .await;
        Mock::given(path("/v1/me/library/playlists/p.empty/tracks"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .mount(&server)
            .await;
        let client = client(&server);
        let cancel = CancellationToken::new();
        let err = client
            .library_playlist_tracks(&cancel, "p.401", "", "tok", "mut")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LibraryStatus(s) if s == 401), "{err}");
        let err = client
            .library_playlist_tracks(&cancel, "p.empty", "", "tok", "mut")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LibraryPlaylistEmpty(_)), "{err}");
        let err = client
            .library_playlist_tracks(&cancel, "p.1", "", "tok", "")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::MissingUserToken), "{err}");
    }

    #[tokio::test]
    async fn playlist_keeps_tracks_when_a_later_page_fails() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/catalog/us/playlists/pl.1"))
            .and(header("media-user-token", "mut"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":[{"id":"pl.1","relationships":{"tracks":{"next":"/broken","data":[{"id":"a"}]}}}]}"#,
            ))
            .mount(&server)
            .await;
        let resp = client(&server)
            .playlist(
                &CancellationToken::new(),
                "us",
                "pl.1",
                "en-US",
                "tok",
                "mut",
            )
            .await
            .unwrap();
        assert_eq!(resp.data[0].relationships.tracks.data.len(), 1);
    }
}
