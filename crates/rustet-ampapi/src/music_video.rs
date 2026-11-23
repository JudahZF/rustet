//! Catalog music videos.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::types::{Artwork, List, MediaRelationships, Page, PlayParams, Preview};
use crate::{Client, Error};

/// The response of the music-video endpoint.
pub type MusicVideoResp = Page<MusicVideo>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MusicVideo {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub href: String,
    pub attributes: MusicVideoAttributes,
    pub relationships: MediaRelationships,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MusicVideoAttributes {
    pub previews: List<Preview>,
    pub artwork: Artwork,
    pub album_name: String,
    pub artist_name: String,
    pub url: String,
    pub genre_names: List<String>,
    pub duration_in_millis: u64,
    pub isrc: String,
    pub track_number: u32,
    pub disc_number: u32,
    pub content_rating: String,
    pub release_date: String,
    pub name: String,
    #[serde(rename = "has4K")]
    pub has_4k: bool,
    #[serde(rename = "hasHDR")]
    pub has_hdr: bool,
    pub play_params: PlayParams,
}

impl Client {
    /// Fetches a music video with its album and artists. An empty `token` is replaced by
    /// a scraped developer token.
    pub async fn music_video(
        &self,
        cancel: &CancellationToken,
        storefront: &str,
        id: &str,
        language: &str,
        token: &str,
    ) -> Result<MusicVideoResp, Error> {
        let token = self.bearer(token).await?;
        let request = self
            .api_get(
                format!(
                    "{}/v1/catalog/{storefront}/music-videos/{id}",
                    self.amp_base
                ),
                &token,
            )
            .query(&[("include", "albums,artists"), ("l", language)]);
        self.get_json(cancel, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_video_flags_use_apple_names() {
        let mv: MusicVideoResp =
            serde_json::from_str(r#"{"data":[{"attributes":{"has4K":true,"hasHDR":true}}]}"#)
                .unwrap();
        assert!(mv.data[0].attributes.has_4k && mv.data[0].attributes.has_hdr);
    }
}
