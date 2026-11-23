//! Fragment-level seek (the "direct" path).
//!
//! An HLS video segment is a flat sequence of moof + mdat fragments, each starting on a sync sample and carrying its
//! own senc, so a fragment decrypts independently of the ones before it. A seek therefore needs only the fragment
//! that holds the target and everything after it, not the bytes of the segment before it.
//! [`Streams::plan_direct_seek`] maps the segment with small range reads (there is no sidx);
//! [`DirectPlan::stream`] then emits `[init][fragments from the target][following segments]` as one raw, still
//! encrypted fMP4 stream, which the normal decrypt stage turns into clear output.

use std::sync::Arc;

use bytes::Bytes;
use rustet_mp4::{BoxHeader, FourCC, Mp4Box};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::download::{cancellable, stable_cache_key};
use crate::priority::has_seek_priority;
use crate::{Error, Streams};

/// moof headers are a few KiB; the mdat is only peeked at (8 bytes).
const PROBE_WINDOW: u64 = 16 << 10;
/// Seconds a fragment may start after the target and still be chosen.
const TARGET_SLACK: f64 = 0.05;
/// Fragments mapped in one segment at most.
const MAX_FRAGMENTS: usize = 512;
/// Connections the segments after the target segment stream with (see `download_mv_segments_streaming`).
const REST_PREFETCH: usize = 5;

fn unsupported(reason: impl Into<String>) -> Error {
    Error::DirectUnsupported(reason.into())
}

/// The mapped segment that holds a seek target.
#[derive(Debug)]
pub struct DirectPlan {
    streams: Arc<Streams>,
    init: Bytes,
    seg_url: String,
    /// The whole segment, when it was already in the music-video cache.
    seg_cached: Option<Bytes>,
    pub(crate) seg_size: u64,
    /// Offset of the first fragment to emit.
    pub(crate) frag_off: u64,
    /// Whole segments after the target segment.
    rest: Vec<String>,
    /// The first emitted fragment's tfdt in seconds on the raw timeline.
    pub raw_frag_start: f64,
    /// The segment's first fragment's tfdt in seconds. Its difference from the playlist time is the constant HLS
    /// timeline offset.
    pub raw_seg_start: f64,
    /// Range requests spent on mapping, for logging.
    pub probes: usize,
}

/// Where the segment's bytes come from while mapping.
enum Ranges<'a> {
    Memory(&'a [u8]),
    Remote(&'a str),
}

impl Streams {
    /// Maps `seg_url` and picks the fragment holding the target, given as `offset_in_seg` seconds into the segment
    /// (the target minus the segment's playlist start). The raw tfdt timeline sits at a constant offset from the
    /// playlist's, so the target is resolved against the segment's own first fragment rather than assuming that
    /// offset. `init_url` and `rest` (the segments after `seg_url`) come from the playlist.
    ///
    /// [`Error::DirectUnsupported`] when the layout cannot be seeked at fragment level (byte-range playlist
    /// entries, unexpected boxes, no Range support): fall back to the whole-segment path.
    pub async fn plan_direct_seek(
        self: &Arc<Self>,
        init_url: &str,
        seg_url: &str,
        rest: Vec<String>,
        offset_in_seg: f64,
        cancel: &CancellationToken,
    ) -> Result<DirectPlan, Error> {
        if seg_url.contains("#bytes=") || init_url.contains("#bytes=") {
            return Err(unsupported("byte-range playlist entry"));
        }
        let init = self.fetch_mv_segment(init_url, cancel).await?;
        let timescale = video_timescale(&init)?;
        if timescale == 0 {
            return Err(unsupported("no video timescale"));
        }
        let mut plan = DirectPlan {
            streams: Arc::clone(self),
            init,
            seg_url: seg_url.to_owned(),
            seg_cached: self.cached_mv(&stable_cache_key(seg_url)).await,
            seg_size: 0,
            frag_off: 0,
            rest,
            raw_frag_start: 0.0,
            raw_seg_start: 0.0,
            probes: 0,
        };
        let cached = plan.seg_cached.clone();
        let ranges = match &cached {
            Some(b) => Ranges::Memory(b),
            None => Ranges::Remote(seg_url),
        };
        cancellable(
            cancel,
            plan.map_fragments(&ranges, timescale, offset_in_seg),
        )
        .await?;
        Ok(plan)
    }

    /// Bytes `[off, end]` of `url` and the resource's total size; `None` past its end (416). The reply must be 206:
    /// a 200 means the server ignored Range.
    async fn fetch_range(
        &self,
        url: &str,
        off: u64,
        end: u64,
    ) -> Result<Option<(Bytes, u64)>, Error> {
        let range = format!("bytes={off}-{end}");
        let mut resp = match self.request(url, Some(&range)).await {
            Ok(resp) => resp,
            Err(Error::Status(416)) => return Ok(None),
            Err(Error::RangeIgnored(_)) => return Err(unsupported("server ignored Range")),
            Err(e) => return Err(e),
        };
        let total = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(total_from_content_range)
            .ok_or_else(|| unsupported("bad Content-Range"))?;
        let want = (end - off + 1) as usize;
        let mut data = Vec::new();
        while data.len() < want
            && let Some(chunk) = resp.chunk().await.map_err(Error::Body)?
        {
            data.extend_from_slice(&chunk[..chunk.len().min(want - data.len())]);
        }
        Ok(Some((data.into(), total)))
    }

    /// Streams bytes `[off, end]` of `url` to `w` as they arrive. Only the header request is retried: once bytes
    /// are written a retry would duplicate them. Paused by background holds like any other download.
    async fn stream_range(
        &self,
        url: &str,
        off: u64,
        end: u64,
        w: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let range = format!("bytes={off}-{end}");
        let resp = cancellable(cancel, self.open_range(url, Some(&range))).await?;
        let n = self
            .copy_body(resp, w, None, has_seek_priority(), cancel)
            .await?;
        let want = end - off + 1;
        if n != want {
            return Err(Error::ShortRange { got: n, want });
        }
        Ok(())
    }
}

impl DirectPlan {
    async fn read(
        &mut self,
        ranges: &Ranges<'_>,
        off: u64,
        end: u64,
    ) -> Result<Option<(Bytes, u64)>, Error> {
        match ranges {
            Ranges::Memory(b) => {
                let total = b.len() as u64;
                if off >= total {
                    return Ok(None);
                }
                let end = end.min(total - 1);
                Ok(Some((
                    Bytes::copy_from_slice(&b[off as usize..=end as usize]),
                    total,
                )))
            }
            Ranges::Remote(url) => {
                self.probes += 1;
                self.streams.fetch_range(url, off, end).await
            }
        }
    }

    /// Walks the moof / mdat chain reading only box headers, until it passes the target. Everything after the
    /// chosen fragment streams as one range, so later fragments need no mapping.
    async fn map_fragments(
        &mut self,
        ranges: &Ranges<'_>,
        timescale: u64,
        offset_in_seg: f64,
    ) -> Result<(), Error> {
        let offset_in_seg = offset_in_seg.max(0.0);
        let (mut off, mut total) = (0u64, 0u64);
        let mut chosen = None;
        for i in 0..MAX_FRAGMENTS {
            let Some((mut hdr, tot)) = self.read(ranges, off, off + PROBE_WINDOW - 1).await? else {
                if chosen.is_some() {
                    break;
                }
                return Err(unsupported(format!("probe fragment {i}: end of segment")));
            };
            total = tot;
            if hdr.len() < 16 {
                return Err(unsupported(format!("short header at {off}")));
            }
            let moof_size = u64::from(u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]));
            if &hdr[4..8] != b"moof" || moof_size < 16 {
                return Err(unsupported(format!(
                    "expected moof at {off}, got {:?}",
                    FourCC([hdr[4], hdr[5], hdr[6], hdr[7]])
                )));
            }
            if moof_size + 16 > hdr.len() as u64 {
                // The moof is larger than the window: fetch it whole.
                hdr = match self.read(ranges, off, off + moof_size + 15).await {
                    Ok(Some((h, _))) if h.len() as u64 >= moof_size + 8 => h,
                    _ => return Err(unsupported(format!("moof at {off} unreadable"))),
                };
            }
            let moof_len = moof_size as usize;
            let tfdt = moof_tfdt(&hdr[..moof_len])?;
            let mut mdat_head = hdr.slice(moof_len..);
            if mdat_head.len() < 8 {
                mdat_head = match self
                    .read(ranges, off + moof_size, off + moof_size + 15)
                    .await
                {
                    Ok(Some((h, _))) if h.len() >= 8 => h,
                    _ => {
                        return Err(unsupported(format!(
                            "mdat header at {} unreadable",
                            off + moof_size
                        )));
                    }
                };
            }
            if &mdat_head[4..8] != b"mdat" {
                return Err(unsupported(format!("expected mdat after moof at {off}")));
            }
            let mdat_size = match BoxHeader::parse(&mdat_head) {
                Ok(Some(h)) => h.size,
                Ok(None) => return Err(unsupported("truncated largesize mdat")),
                Err(_) => return Err(unsupported("bad mdat size")),
            };

            let start = tfdt as f64 / timescale as f64;
            if i == 0 {
                self.raw_seg_start = start;
            }
            if chosen.is_none() || start <= self.raw_seg_start + offset_in_seg + TARGET_SLACK {
                chosen = Some(off);
                self.raw_frag_start = start;
            } else {
                break;
            }
            off += moof_size + mdat_size;
            if off >= total {
                break;
            }
        }
        self.frag_off = chosen.ok_or_else(|| unsupported("no fragment found"))?;
        self.seg_size = total;
        Ok(())
    }

    /// Writes the raw (encrypted) stream: init, the target fragment onward within the mapped segment, then the
    /// following whole segments.
    pub async fn stream(
        &self,
        w: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        w.write_all(&self.init).await?;
        match &self.seg_cached {
            Some(seg) => w.write_all(&seg[self.frag_off as usize..]).await?,
            None => self
                .streams
                .stream_range(&self.seg_url, self.frag_off, self.seg_size - 1, w, cancel)
                .await
                .map_err(|e| Error::Segment {
                    index: 0,
                    source: Box::new(e),
                })?,
        }
        if self.rest.is_empty() {
            return Ok(());
        }
        debug!(
            probes = self.probes,
            from = self.frag_off,
            of = self.seg_size,
            segments = self.rest.len(),
            "direct: target segment tail done"
        );
        self.streams
            .download_mv_segments_streaming(&self.rest, w, REST_PREFETCH, cancel)
            .await
    }
}

/// The timescale of the first track of an init segment (0 when it declares none).
fn video_timescale(init: &[u8]) -> Result<u64, Error> {
    let mut pos = 0;
    while pos < init.len() {
        let (b, used) =
            Mp4Box::decode(&init[pos..], pos as u64).map_err(Error::mp4("init parse"))?;
        if let Mp4Box::Moov(moov) = b {
            return Ok(moov
                .traks()
                .next()
                .and_then(|t| t.mdhd())
                .map_or(0, |m| u64::from(m.timescale())));
        }
        pos += used;
    }
    Err(Error::NoMoov)
}

fn moof_tfdt(moof: &[u8]) -> Result<u64, Error> {
    match Mp4Box::decode(moof, 0) {
        Ok((Mp4Box::Moof(moof), _)) => moof
            .traf()
            .and_then(|t| t.tfdt())
            .map(|t| t.base_media_decode_time())
            .ok_or_else(|| unsupported("moof without traf/tfdt")),
        Ok(_) => Err(unsupported("not a moof")),
        Err(e) => Err(unsupported(format!("decode moof: {e}"))),
    }
}

/// The total size from a `Content-Range: bytes a-b/total` header.
fn total_from_content_range(cr: &str) -> Option<u64> {
    let (_, total) = cr.rsplit_once('/')?;
    total.parse().ok().filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_total() {
        assert_eq!(total_from_content_range("bytes 0-99/2000"), Some(2000));
        assert_eq!(total_from_content_range("bytes 0-99/*"), None);
        assert_eq!(total_from_content_range("bytes 0-99/0"), None);
        assert_eq!(total_from_content_range("garbage"), None);
    }
}
