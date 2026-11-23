//! Media playlists: segment lists for CTR (Widevine) streams and FairPlay CBCS streams.

use tracing::{debug, info};
use url::Url;

use crate::m3u8::{self, Playlist};
use crate::{Auth, HlsError, fetch};

/// The `EXT-X-KEY` attributes needed to acquire a decryption key. The key material itself never passes through here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncryptionInfo {
    /// Licence endpoint prefix: the key URI before its first comma.
    pub uri_prefix: String,
    /// Base64 key ID: the key URI after its first comma.
    pub kid_base64: String,
    /// METHOD and KEYFORMAT, kept for diagnostics: they show which key system the playlist declares.
    pub method: String,
    pub key_format: String,
    /// Distinct key URIs in the playlist. More than one means the key changes between segments, which a single
    /// licence request does not cover.
    pub key_count: usize,
}

/// One media segment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Segment {
    /// Absolute URL. A byte-range segment ends in `#bytes=<first>-<last>`; the downloader turns that into a Range
    /// request.
    pub url: String,
    /// Declared duration in seconds.
    pub duration: f64,
}

/// A parsed HLS media playlist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Media {
    /// CMAF initialization segment (EXT-X-MAP).
    pub init_url: Option<String>,
    /// Media segments in presentation order.
    pub segments: Vec<Segment>,
    /// `None` for unencrypted tracks.
    pub encryption: Option<EncryptionInfo>,
}

impl Media {
    /// Fetches and parses a media playlist. `auth` adds the Apple Music headers that play.itunes.apple.com needs.
    pub async fn open(
        client: &reqwest::Client,
        url: &str,
        auth: Option<Auth<'_>>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self, HlsError> {
        let (base, body) = fetch(client, url, auth, cancel).await?;
        Self::parse(&base, &body)
    }

    /// Parses a media playlist body; relative URIs resolve against `base`.
    pub fn parse(base: &Url, body: &str) -> Result<Self, HlsError> {
        let Playlist::Media(pl) = m3u8::parse(body, |_| true)? else {
            return Err(HlsError::NotMedia {
                url: base.to_string(),
            });
        };

        let mut init_url = pl
            .first_map
            .as_ref()
            .filter(|m| !m.uri.is_empty())
            .and_then(|m| Some(byte_range_url(base.join(&m.uri).ok()?, m.offset, m.limit)));

        let encryption = pl.first_key.as_ref().and_then(|key| {
            let (prefix, kid) = key.uri.split_once(',')?;
            let mut uris: Vec<&str> = std::iter::once(key.uri.as_str())
                .chain(
                    pl.segments
                        .iter()
                        .filter_map(|s| s.key.as_ref())
                        .map(|k| k.uri.as_str()),
                )
                .filter(|u| !u.is_empty())
                .collect();
            uris.sort_unstable();
            uris.dedup();
            Some(EncryptionInfo {
                uri_prefix: prefix.to_owned(),
                kid_base64: kid.to_owned(),
                method: key.method.clone(),
                key_format: key.key_format.clone(),
                key_count: uris.len(),
            })
        });

        let mut segments = Vec::with_capacity(pl.segments.len());
        for seg in &pl.segments {
            let Ok(url) = base.join(&seg.uri) else {
                continue;
            };
            segments.push(Segment {
                url: byte_range_url(url, seg.offset, seg.limit),
                duration: seg.duration,
            });
            // A segment with its own EXT-X-MAP means the playlist switched init segments mid-stream (clear-leader
            // CBCS: the first segments use a clear mp4a init, later encrypted ones an enca init). Keep the LAST map
            // so init decryption can read the CBCS sinf and decrypt the encrypted segments.
            if let Some(map) = seg.map.as_ref().filter(|m| !m.uri.is_empty())
                && let Ok(url) = base.join(&map.uri)
            {
                init_url = Some(byte_range_url(url, map.offset, map.limit));
            }
        }

        let media = Self {
            init_url,
            segments,
            encryption,
        };
        debug!(
            url = %base,
            segments = media.segments.len(),
            total_duration = media.segments.iter().map(|s| s.duration).sum::<f64>(),
            encryption = media.encryption.as_ref().map_or("none".to_owned(), |e| format!("{},<kid>", e.uri_prefix)),
            method = pl.first_key.as_ref().map_or("", |k| k.method.as_str()),
            init_url = media.init_url.as_deref().unwrap_or(""),
            first = media.segments.first().map_or("(none)", |s| s.url.as_str()),
            last = media.segments.get(1..).and_then(<[_]>::last).map_or("(none)", |s| s.url.as_str()),
            "hls: parsed media playlist"
        );
        Ok(media)
    }

    /// `[init, seg0, seg1, ...]`: every URL the pipeline downloads and joins into one fMP4 stream.
    pub fn all_urls(&self) -> Vec<String> {
        self.urls_from_index(0)
    }

    /// `[init, seg_N, ...]` for a seek to `start_sec`, and the start time of segment N. N is one segment before the
    /// one holding `start_sec`: the TFDT offset (segment boundary versus HLS timestamp) must not push the first
    /// decoded frame past the target, and the overlap gives the decoder keyframe context. A start at or past the end
    /// counts as the last segment. A start at or before 0 returns [`Media::all_urls`] at 0.
    pub fn urls_from(&self, start_sec: f64) -> (Vec<String>, f64) {
        if self.segments.is_empty() || start_sec <= 0.0 {
            info!(
                start_sec,
                segments = self.segments.len(),
                "hls: urls_from falls back to all URLs"
            );
            return (self.all_urls(), 0.0);
        }
        let (mut idx, mut start) = self.locate(start_sec);
        if idx > 0 {
            idx -= 1;
            start -= self.segments[idx].duration;
        }
        let urls = self.urls_from_index(idx);
        info!(
            start_sec,
            segments = self.segments.len(),
            idx,
            actual_start = start,
            first_segment = self.segments[idx].url,
            "hls: urls_from"
        );
        (urls, start)
    }

    /// Like [`Media::urls_from`] without the one-segment step back. For sources (music-video segments) where FFmpeg
    /// resets timestamps to zero at each HLS segment boundary, so no overlap is needed.
    pub fn urls_from_exact(&self, start_sec: f64) -> (Vec<String>, f64) {
        if self.segments.is_empty() || start_sec <= 0.0 {
            return (self.all_urls(), 0.0);
        }
        let (idx, start) = self.locate(start_sec);
        info!(
            start_sec,
            segments = self.segments.len(),
            idx,
            actual_start = start,
            "hls: urls_from_exact"
        );
        (self.urls_from_index(idx), start)
    }

    /// Index and playlist start time of the segment holding `start_sec`, by the [`Media::urls_from_exact`] rule.
    /// `None` when the playlist has no segments.
    pub fn segment_for(&self, start_sec: f64) -> Option<(usize, f64)> {
        if self.segments.is_empty() {
            return None;
        }
        Some(self.locate(if start_sec < 0.0 { 0.0 } else { start_sec }))
    }

    /// Presentation start time of each segment: entry i is the sum of the durations before segment i.
    pub fn cumulative_segment_times(&self) -> Vec<f64> {
        self.segments
            .iter()
            .scan(0.0, |cum, s| {
                let start = *cum;
                *cum += s.duration;
                Some(start)
            })
            .collect()
    }

    /// The segment holding `start_sec` and its start time, clamped to the last segment. Needs a segment.
    fn locate(&self, start_sec: f64) -> (usize, f64) {
        let mut start = 0.0;
        for (i, s) in self.segments.iter().enumerate() {
            if start + s.duration > start_sec {
                return (i, start);
            }
            start += s.duration;
        }
        let last = self.segments.len() - 1;
        (last, self.segments[..last].iter().map(|s| s.duration).sum())
    }

    fn urls_from_index(&self, idx: usize) -> Vec<String> {
        self.init_url
            .iter()
            .cloned()
            .chain(self.segments[idx..].iter().map(|s| s.url.clone()))
            .collect()
    }
}

/// One segment of a CBCS playlist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CbcsSegment {
    /// The raw `skd://` key URI declared before this segment, sent to the FairPlay decryptor verbatim. `None` when
    /// the key does not change here.
    pub key_uri: Option<String>,
    /// Declared duration in seconds.
    pub duration: f64,
}

/// A FairPlay CBCS media playlist. Unlike [`Media`] it keeps each segment's raw key URI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CbcsMedia {
    /// The single encrypted fMP4 file; every segment is a byte range of it. Resolved from the first segment's URI.
    pub file_url: String,
    /// Base64 key ID from the playlist-level EXT-X-KEY (after its comma), used for licence acquisition.
    pub kid_base64: Option<String>,
    /// Segments in playlist order.
    pub segments: Vec<CbcsSegment>,
}

impl CbcsMedia {
    /// Fetches and parses a FairPlay CBCS media playlist.
    pub async fn open(
        client: &reqwest::Client,
        url: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self, HlsError> {
        let (base, body) = fetch(client, url, None, cancel).await?;
        Self::parse(&base, &body)
    }

    /// Parses a CBCS media playlist body. Only `com.apple.streamingkeydelivery` keys are read: Apple's enhanced-HLS
    /// playlists sometimes carry PlayReady, Widevine or identity keys alongside, and those are dropped.
    ///
    /// Fails unless the playlist is a byte-range playlist: the CBCS path downloads one fMP4 file.
    pub fn parse(base: &Url, body: &str) -> Result<Self, HlsError> {
        let Playlist::Media(pl) = m3u8::parse(body, |line| !is_foreign_key(line))? else {
            return Err(HlsError::NotMedia {
                url: base.to_string(),
            });
        };
        let first = pl
            .segments
            .first()
            .ok_or_else(|| HlsError::CbcsNoSegments {
                url: base.to_string(),
            })?;
        if first.limit <= 0 {
            return Err(HlsError::CbcsNotByteRange {
                url: base.to_string(),
            });
        }
        let file_url = base.join(&first.uri).map_err(HlsError::CbcsFileUrl)?.into();

        let kid_base64 = pl
            .first_key
            .as_ref()
            .and_then(|k| Some(k.uri.split_once(',')?.1.to_owned()));
        if let Some(kid) = &kid_base64 {
            info!(kid, "hls: CBCS playlist key ID");
        }
        let segments = pl
            .segments
            .into_iter()
            .map(|s| CbcsSegment {
                key_uri: s.key.map(|k| k.uri).filter(|u| !u.is_empty()),
                duration: s.duration,
            })
            .collect();
        Ok(Self {
            file_url,
            kid_base64,
            segments,
        })
    }
}

/// An EXT-X-KEY line for a key system other than FairPlay streamingkeydelivery.
fn is_foreign_key(line: &str) -> bool {
    line.starts_with("#EXT-X-KEY:") && !line.contains("streamingkeydelivery")
}

/// Appends `#bytes=<offset>-<end>` when the segment declares an EXT-X-BYTERANGE (`length > 0`). The segment
/// downloader reads this fragment and issues a Range request instead of a full GET.
fn byte_range_url(url: Url, offset: i64, length: i64) -> String {
    if length <= 0 {
        return url.into();
    }
    // Wrapping, like Go's int64 arithmetic: hostile values must not panic.
    format!(
        "{url}#bytes={offset}-{}",
        offset.wrapping_add(length).wrapping_sub(1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_keys_are_dropped() {
        assert!(!is_foreign_key(
            r#"#EXT-X-KEY:METHOD=SAMPLE-AES,URI="skd://itunes.apple.com/P000000000/s1/e1",KEYFORMAT="com.apple.streamingkeydelivery""#
        ));
        assert!(is_foreign_key(
            r#"#EXT-X-KEY:METHOD=SAMPLE-AES,URI="https://play.itunes.apple.com/identityKeyRequest/abc",KEYFORMAT="identity",IV=0xdeadbeef"#
        ));
        assert!(!is_foreign_key("seg0.mp4"));
        assert!(!is_foreign_key("#EXTINF:6.0,"));
    }

    #[test]
    fn byte_range_fragment() {
        let url = Url::parse("https://a.test/f.mp4").unwrap();
        assert_eq!(
            byte_range_url(url.clone(), 3872, 2432024),
            "https://a.test/f.mp4#bytes=3872-2435895"
        );
        assert_eq!(byte_range_url(url.clone(), 5, 0), "https://a.test/f.mp4");
        assert_eq!(
            byte_range_url(url, i64::MAX, i64::MAX),
            format!("https://a.test/f.mp4#bytes={}--3", i64::MAX)
        );
    }
}
