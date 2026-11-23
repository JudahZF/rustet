//! Reuse of the bytes playback already fetched, so exporting a track that is cached (or playing
//! right now) does not download it a second time.

use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use futures::TryStreamExt;
use rustet_core::pipeline::Codec;
use tokio::io::AsyncRead;
use tokio_util::io::StreamReader;

use crate::job::ExportCapabilities;

/// A reader over an in-progress playback download.
pub type TailReader = Pin<Box<dyn AsyncRead + Send>>;

/// The part of the playback disk cache that exports read from.
#[async_trait]
pub trait AudioCache: Send + Sync {
    /// The committed cache file of the track, if any.
    async fn path(&self, asset_id: &str, qualifier: &str) -> Option<PathBuf>;

    /// A reader over an in-progress playback download of exactly this asset and qualifier, never
    /// merely because a session exists. Reads wait for more bytes, end once the producer
    /// commits, and fail if it gives up.
    fn tail_reader(&self, asset_id: &str, qualifier: &str) -> Option<TailReader>;
}

#[async_trait]
impl AudioCache for rustet_diskcache::Cache {
    async fn path(&self, asset_id: &str, qualifier: &str) -> Option<PathBuf> {
        rustet_diskcache::Cache::path(self, asset_id, qualifier).await
    }

    fn tail_reader(&self, asset_id: &str, qualifier: &str) -> Option<TailReader> {
        // The writer may commit between the lookup and here; then there is no tail and the
        // export reads the committed file or downloads normally.
        let reader = self
            .get_streaming(asset_id, qualifier)?
            .reader_if_active()?;
        let stream = reader.into_stream().map_err(std::io::Error::other);
        Some(Box::pin(StreamReader::new(stream)))
    }
}

/// The disk-cache qualifier playback would use for this request. Playback keys its cache by the
/// session codec, which the provider derives from the request flags and the track's traits (a
/// lossless request for a track without lossless audio yields AAC). This request-only mapping
/// can miss a reusable entry, but never match wrong content: an `alac` or `atmos` entry exists
/// only for tracks that have it, so a miss just falls back to the network.
pub(crate) fn audio_qualifier(c: ExportCapabilities) -> &'static str {
    if c.atmos {
        Codec::Atmos.as_str()
    } else if c.lossless {
        Codec::Alac.as_str()
    } else {
        Codec::Aac.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn qualifiers() {
        let caps = |lossless, atmos| ExportCapabilities {
            lossless,
            atmos,
            ..ExportCapabilities::default()
        };
        assert_eq!(audio_qualifier(caps(false, false)), "aac");
        assert_eq!(audio_qualifier(caps(true, false)), "alac");
        assert_eq!(audio_qualifier(caps(false, true)), "atmos");
        // Atmos wins over lossless, as in the provider's selection order.
        assert_eq!(audio_qualifier(caps(true, true)), "atmos");
    }

    #[tokio::test]
    async fn disk_cache_tail_only_while_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let dc = rustet_diskcache::Cache::new(dir.path()).unwrap();
        assert!(
            dc.tail_reader("1", "aac").is_none(),
            "tail with no download"
        );

        let mut put = dc.begin_streaming_put("1", "aac").await.unwrap().unwrap();
        put.write_all(b"hello ").await.unwrap();
        let mut tail = dc
            .tail_reader("1", "aac")
            .expect("no tail while downloading");
        assert!(
            dc.tail_reader("1", "alac").is_none(),
            "tail for another qualifier"
        );

        let read = tokio::spawn(async move {
            let mut got = Vec::new();
            tail.read_to_end(&mut got).await.map(|_| got)
        });
        put.write_all(b"world").await.unwrap();
        put.commit().await.unwrap();
        assert_eq!(read.await.unwrap().unwrap(), b"hello world");
        assert!(AudioCache::path(&dc, "1", "aac").await.is_some());
        assert!(dc.tail_reader("1", "aac").is_none(), "tail after commit");
    }

    #[tokio::test]
    async fn disk_cache_tail_fails_when_the_producer_gives_up() {
        let dir = tempfile::tempdir().unwrap();
        let dc = rustet_diskcache::Cache::new(dir.path()).unwrap();
        let mut put = dc.begin_streaming_put("1", "aac").await.unwrap().unwrap();
        put.write_all(b"partial").await.unwrap();
        let mut tail = dc.tail_reader("1", "aac").unwrap();
        drop(put);
        let mut got = Vec::new();
        assert!(tail.read_to_end(&mut got).await.is_err());
    }
}
