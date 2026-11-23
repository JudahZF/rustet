//! HLS master and media playlist parsing for Apple Music streams.
//!
//! This crate fetches and parses playlists into typed structs. It downloads no segments, acquires no keys and
//! decrypts nothing. The playlist decoder is a focused one that keeps the Apple attributes the engine needs
//! (`HDCP-LEVEL`, `ALLOWED-CPC`, `skd://` key URIs, per-segment keys and maps) and never panics on any input.
#![forbid(unsafe_code)]

mod m3u8;
mod master;
mod media;

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use url::Url;

pub use m3u8::ParseError;
pub use master::{Alternative, Master, Variant, cpc_summary};
pub use media::{CbcsMedia, CbcsSegment, EncryptionInfo, Media, Segment};

/// Playlists are small text files on the critical path of every session open, so they must fail fast rather than
/// hang: a stalled CDN connection would otherwise outlast the caller's own deadline. Apple's Android client is far
/// more aggressive (connect 4 s, read 2 s, then retry); 15 s still bounds the hang.
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(15);

/// Bodies are cut at 1 MiB; a real playlist is a few kilobytes.
const PLAYLIST_LIMIT: usize = 1 << 20;

/// Errors from fetching, parsing or selecting in HLS playlists.
#[derive(Debug, thiserror::Error)]
pub enum HlsError {
    #[error("invalid playlist URL {url:?}: {source}")]
    InvalidUrl {
        url: String,
        source: url::ParseError,
    },
    #[error("playlist request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP {status} from {url}")]
    Status {
        status: reqwest::StatusCode,
        url: String,
    },
    #[error("playlist request cancelled")]
    Cancelled,
    #[error("m3u8 decode: {0}")]
    Parse(#[from] ParseError),
    #[error("expected media playlist at {url}")]
    NotMedia { url: String },
    #[error("cbcs: no segments in playlist {url}")]
    CbcsNoSegments { url: String },
    #[error("cbcs: non-byterange playlist not supported at {url}")]
    CbcsNotByteRange { url: String },
    #[error("cbcs: resolve file URL: {0}")]
    CbcsFileUrl(url::ParseError),
    #[error("no audio alternative in master playlist")]
    NoAudioAlternative,
    #[error("no video variant at or below {max_height}p")]
    NoVideoVariant { max_height: u32 },
}

/// Apple Music credentials for playlists at play.itunes.apple.com.
#[derive(Debug, Clone, Copy)]
pub struct Auth<'a> {
    /// Developer token, sent as `Authorization: Bearer`.
    pub developer_token: &'a str,
    /// Music user token, sent as `x-apple-music-user-token`.
    pub media_user_token: &'a str,
}

/// GETs a playlist and returns its parsed URL (the base for relative URIs) and its body.
async fn fetch(
    client: &reqwest::Client,
    raw_url: &str,
    auth: Option<Auth<'_>>,
    cancel: &CancellationToken,
) -> Result<(Url, String), HlsError> {
    let url = Url::parse(raw_url).map_err(|source| HlsError::InvalidUrl {
        url: raw_url.to_owned(),
        source,
    })?;
    let mut request = client.get(url.clone()).timeout(PLAYLIST_TIMEOUT);
    if let Some(auth) = auth {
        request = request
            .bearer_auth(auth.developer_token)
            .header("x-apple-music-user-token", auth.media_user_token)
            .header("Origin", "https://music.apple.com");
    }
    let download = async {
        let mut response = request.send().await?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(HlsError::Status {
                status: response.status(),
                url: raw_url.to_owned(),
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            let room = PLAYLIST_LIMIT - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if body.len() == PLAYLIST_LIMIT {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(HlsError::Cancelled),
        body = download => Ok((url, body?)),
    }
}
