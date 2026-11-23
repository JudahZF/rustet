//! The licence provider and the pipeline pieces of the AAC and music-video path.
//!
//! - [`Streams::open_licence`] turns the key metadata of an HLS playlist into a [`WidevineDecryptor`]. Key bytes
//!   enter that decryptor and never leave it: they are not stored elsewhere, logged, or returned to any caller.
//! - [`PassthroughDecryptor`] is the "decryptor" for AAC that Apple serves without content-level encryption.
//! - [`HlsSource`] (audio) and [`MvVideoSource`] (music-video video) are the seekable segment sources.

use std::sync::Arc;

use async_trait::async_trait;
use rustet_core::pipeline::{
    Decryptor, DirectInfo, DirectSeekable, PipelineError, Seekable, Sink, Source, StreamContext,
};
use rustet_hls::Media;
use rustet_widevine::wvkey::ContentKey;
use tokio::io::AsyncRead;
use tokio_util::sync::CancellationToken;

use crate::{
    DirectPlan, Error, LicenceRequest, Streams, decrypt_mp4_streaming, passthrough_streaming,
};

/// Audio segments downloaded at once.
const AUDIO_CONCURRENCY: usize = 3;
/// Prefetch requested for music-video segment streams (capped at two open connections).
const MV_PREFETCH: usize = 5;

impl Streams {
    /// Acquires the content key described by `req` (from the key cache when possible) and returns the decryptor
    /// that holds it. A refusal by the licence server is reported as an error for which
    /// [`Error::is_licence_refused`] holds.
    pub async fn open_licence(
        self: &Arc<Self>,
        req: &LicenceRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<WidevineDecryptor, Error> {
        let key = self.acquire_key(req, false, cancel).await?;
        Ok(WidevineDecryptor {
            streams: Arc::clone(self),
            key,
            kid: req.kid_base64.to_owned(),
            uri_prefix: req.uri_prefix.to_owned(),
        })
    }
}

/// Decrypts a Widevine stream ([`decrypt_mp4_streaming`]). The only type of this path that holds key bytes; it
/// deliberately implements neither `Debug` nor `Clone`.
pub struct WidevineDecryptor {
    streams: Arc<Streams>,
    key: ContentKey,
    /// For evicting the cached key after a decryption failure.
    kid: String,
    uri_prefix: String,
}

#[async_trait]
impl Decryptor for WidevineDecryptor {
    async fn decrypt(
        &self,
        ctx: &StreamContext,
        r: &mut (dyn AsyncRead + Send + Unpin),
        w: &mut dyn Sink,
    ) -> Result<(), PipelineError> {
        let res = decrypt_mp4_streaming(ctx, r, &self.key.key, w).await;
        if let Err(e) = &res
            && !self.kid.is_empty()
            && should_evict_key(ctx, e)
        {
            // The cached key may be stale: the next open re-negotiates, as Android's
            // FootHillDecryptionKey.fetchKeyData(forceRefresh=true) does after a decrypt failure.
            self.streams.invalidate_key(&self.kid, &self.uri_prefix);
        }
        res.map_err(Into::into)
    }
}

/// Whether a decrypt failure may mean a stale key. A cancelled request (the user skipped) or a closed downstream
/// is not a key problem; evicting on those would force a licence round trip on every skip.
fn should_evict_key(ctx: &StreamContext, err: &Error) -> bool {
    !ctx.cancel.is_cancelled() && !err.is_consumer_gone()
}

/// Copies a clear AAC stream ([`passthrough_streaming`]): drops the init segment's pssh and puts the fragments on
/// one continuous timeline.
#[derive(Debug, Clone, Copy, Default)]
pub struct PassthroughDecryptor;

#[async_trait]
impl Decryptor for PassthroughDecryptor {
    async fn decrypt(
        &self,
        ctx: &StreamContext,
        r: &mut (dyn AsyncRead + Send + Unpin),
        w: &mut dyn Sink,
    ) -> Result<(), PipelineError> {
        Ok(passthrough_streaming(ctx, r, w).await?)
    }
}

// ── Sources ──────────────────────────────────────────────────────────────────

/// A fixed list of audio segment URLs (init first).
struct AudioSegments {
    streams: Arc<Streams>,
    urls: Vec<String>,
    /// The track length for the init segment's moov, when `urls[0]` is the init segment.
    init_duration_ms: Option<u64>,
}

#[async_trait]
impl Source for AudioSegments {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        let res = self
            .streams
            .download_audio_segments(
                &self.urls,
                self.init_duration_ms,
                w,
                AUDIO_CONCURRENCY,
                &ctx.cancel,
            )
            .await;
        Ok(res?)
    }
}

/// A fixed list of music-video segment URLs (init first), streamed through the MV cache.
struct MvSegments {
    streams: Arc<Streams>,
    urls: Vec<String>,
}

#[async_trait]
impl Source for MvSegments {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        let res = self
            .streams
            .download_mv_segments_streaming(&self.urls, w, MV_PREFETCH, &ctx.cancel)
            .await;
        Ok(res?)
    }
}

/// The audio source of a media playlist. Seeking recomputes the URL list from the playlist
/// ([`Media::urls_from`], one segment of overlap).
pub struct HlsSource {
    streams: Arc<Streams>,
    media: Media,
}

impl HlsSource {
    pub fn new(streams: Arc<Streams>, media: Media) -> Self {
        Self { streams, media }
    }

    /// The playlist's length for the init segment's moov (Apple's says 0, see
    /// [`patch_init_duration`](crate::stream::patch_init_duration)); `None` without an init segment.
    fn init_duration_ms(&self) -> Option<u64> {
        self.media.init_url.as_ref()?;
        let seconds: f64 = self.media.segments.iter().map(|s| s.duration).sum();
        Some((seconds * 1000.0).round() as u64)
    }
}

#[async_trait]
impl Source for HlsSource {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        let urls = self.media.all_urls();
        let res = self
            .streams
            .download_audio_segments(
                &urls,
                self.init_duration_ms(),
                w,
                AUDIO_CONCURRENCY,
                &ctx.cancel,
            )
            .await;
        Ok(res?)
    }

    fn seekable(&self) -> Option<&dyn Seekable> {
        Some(self)
    }
}

impl Seekable for HlsSource {
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64) {
        let (urls, actual) = self.media.urls_from(start_sec);
        let source = AudioSegments {
            streams: Arc::clone(&self.streams),
            urls,
            init_duration_ms: self.init_duration_ms(),
        };
        (Arc::new(source), actual)
    }
}

/// The video source of a music-video playlist, on the separate MV segment cache (larger, independently toggled).
pub struct MvVideoSource {
    streams: Arc<Streams>,
    media: Media,
}

impl MvVideoSource {
    pub fn new(streams: Arc<Streams>, media: Media) -> Self {
        Self { streams, media }
    }
}

#[async_trait]
impl Source for MvVideoSource {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        let res = self
            .streams
            .download_mv_segments_streaming(&self.media.all_urls(), w, MV_PREFETCH, &ctx.cancel)
            .await;
        Ok(res?)
    }

    fn seekable(&self) -> Option<&dyn Seekable> {
        Some(self)
    }
}

impl Seekable for MvVideoSource {
    /// No step back ([`Media::urls_from_exact`]): FFmpeg resets timestamps at each HLS segment boundary, so no
    /// tfdt-overlap context is needed.
    fn source_from(&self, start_sec: f64) -> (Arc<dyn Source>, f64) {
        let (urls, actual) = self.media.urls_from_exact(start_sec);
        let source = MvSegments {
            streams: Arc::clone(&self.streams),
            urls,
        };
        (Arc::new(source), actual)
    }

    fn segment_timings(&self) -> Option<Vec<f64>> {
        Some(self.media.cumulative_segment_times())
    }

    fn direct_seekable(&self) -> Option<&dyn DirectSeekable> {
        Some(self)
    }
}

#[async_trait]
impl DirectSeekable for MvVideoSource {
    /// Plans a fragment-level seek: only the fragment holding `start_sec` and everything after it is fetched,
    /// instead of the whole segment containing it.
    async fn direct_source_from(
        &self,
        cancel: &CancellationToken,
        start_sec: f64,
    ) -> Result<(Arc<dyn Source>, DirectInfo), PipelineError> {
        let (Some((idx, seg_start)), Some(init_url)) = (
            self.media.segment_for(start_sec),
            self.media.init_url.as_deref(),
        ) else {
            return Err(Error::DirectUnsupported("no segment or init segment".into()).into());
        };
        let rest = self.media.segments[idx + 1..]
            .iter()
            .map(|s| s.url.clone())
            .collect();
        let plan = self
            .streams
            .plan_direct_seek(
                init_url,
                &self.media.segments[idx].url,
                rest,
                start_sec - seg_start,
                cancel,
            )
            .await?;
        // The raw timeline sits at a constant offset from the playlist's (10.000 s for Apple's music videos);
        // measure it from the segment's own first fragment, to the millisecond.
        let base = ((plan.raw_seg_start - seg_start) * 1000.0).round() / 1000.0;
        let info = DirectInfo {
            start_sec: plan.raw_frag_start - base,
            ts_offset: -base,
        };
        Ok((Arc::new(plan), info))
    }
}

#[async_trait]
impl Source for DirectPlan {
    async fn stream(&self, ctx: &StreamContext, w: &mut dyn Sink) -> Result<(), PipelineError> {
        Ok(DirectPlan::stream(self, w, &ctx.cancel).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn should_evict_key_only_for_real_failures() {
        let live = StreamContext::default();
        let cancelled = StreamContext::default();
        cancelled.cancel.cancel();
        let decrypt_failure = || Error::Mp4 {
            context: "decrypt fragment",
            source: rustet_mp4::Error::Senc("bad"),
        };
        assert!(should_evict_key(&live, &decrypt_failure()));
        assert!(
            !should_evict_key(&cancelled, &decrypt_failure()),
            "user skipped"
        );
        assert!(!should_evict_key(&live, &Error::Cancelled));
        assert!(
            !should_evict_key(&live, &Error::Io(io::ErrorKind::BrokenPipe.into())),
            "downstream closed"
        );
        assert!(should_evict_key(
            &live,
            &Error::Io(io::ErrorKind::InvalidData.into())
        ));
    }
}
