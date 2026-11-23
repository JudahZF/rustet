//! [`AppleMusicProvider`]: catalog lookup, webplayback, variant selection and the session it builds.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustet_core::media::{OpenRequest, Provider, Session, SessionKind, Track};
use rustet_core::pipeline::{BoxError, Codec, PipelineError, StreamKind};
use rustet_fairplay::CbcsDialer;
use rustet_hls::{Auth, Master, cpc_summary};
use rustet_streams::Streams;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::error::{Cause, Error};
use crate::tracks::{
    CbcsTrack, HlsOpener, HlsTrack, Refusals, VideoFallback, describe_variants, quality_name,
};
use crate::webplayback::{self, AssetFlavor, WEBPLAYBACK_URL};

/// Square artwork size requested from the catalog's `{w}x{h}` template.
const ARTWORK_SIZE: &str = "500";

/// Default music-video height limit: 1080p H.264, safe for every Linux build.
const DEFAULT_MV_MAX_HEIGHT: u32 = 1080;
const DEFAULT_MV_AUDIO_PRIORITIES: [&str; 3] = ["audio-atmos", "audio-ac3", "audio-stereo-256"];

/// Deadline for the optional progressive music-video lookup.
const PROGRESSIVE_TIMEOUT: Duration = Duration::from_secs(5);

/// A music video's progressive CDN file.
#[derive(Clone, PartialEq, Eq)]
pub struct ProgressiveMvUrl {
    pub url: String,
    /// Sent to the CDN as the `downloadKey` cookie, which authorises server-side decryption. `None` when the
    /// account has none. Auth material: never log it.
    pub download_key: Option<String>,
}

/// Native calls on the Android DRM session the provider uses. The engine implements it over the DRM manager.
#[async_trait]
pub trait AccountTokenSource: Send + Sync {
    /// The progressive video URL and its download key for `adam_id`, through the native StoreKit auth.
    async fn progressive_mv_url(&self, adam_id: u64) -> Result<ProgressiveMvUrl, BoxError>;
}

/// The Apple Music [`Provider`].
///
/// The only type in the engine that knows the Apple Music catalog API, the webplayback API and Apple's HLS variant
/// conventions. It knows nothing of DRM beyond asking `rustet-streams` for a decryptor and handing CBCS tracks to
/// `rustet-fairplay`.
pub struct AppleMusicProvider {
    http: reqwest::Client,
    catalog: rustet_ampapi::Client,
    streams: Arc<Streams>,
    /// `None`: ALAC and Atmos requests fail.
    cbcs: Option<Arc<dyn CbcsDialer>>,
    /// `None`: no progressive music-video lookup.
    account: Option<Arc<dyn AccountTokenSource>>,
    refusals: Arc<Refusals>,
}

/// How a song is fetched, decided from the request and the catalog's audio traits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioPlan {
    /// The CTR / Widevine webplayback asset.
    Aac,
    /// The catalog's enhanced-HLS master, decrypted through the CBCS dialer.
    Cbcs {
        codec: Codec,
        /// The CODECS value of the variant to pick.
        hls_codec: &'static str,
        sample_rate: u32,
        bit_depth: u32,
    },
}

impl AudioPlan {
    fn choose(req: &OpenRequest, traits: &[String]) -> Self {
        let has = |t: &str| traits.iter().any(|x| x == t);
        if req.atmos && has("atmos") {
            Self::Cbcs {
                codec: Codec::Atmos,
                hls_codec: "ec-3",
                sample_rate: 0,
                bit_depth: 0,
            }
        } else if req.lossless && req.hi_res && has("hi-res-lossless") {
            let (sample_rate, bit_depth) = alac_quality(traits);
            Self::Cbcs {
                codec: Codec::Alac,
                hls_codec: "alac",
                sample_rate,
                bit_depth,
            }
        } else if req.lossless && has("lossless") {
            Self::Cbcs {
                codec: Codec::Alac,
                hls_codec: "alac",
                sample_rate: 44_100,
                bit_depth: 16,
            }
        } else {
            Self::Aac
        }
    }
}

/// The quality of a hi-res track from a trait ending in `-<rate>-<depth>`; 96 kHz / 24-bit when none says.
fn alac_quality(traits: &[String]) -> (u32, u32) {
    // Like Go's Sscanf("%d"): the leading digits, and a part that does not parse leaves the value as it was.
    let leading = |s: &str| {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse::<u32>().ok()
    };
    let (mut rate, mut depth) = (0, 0);
    for t in traits {
        let parts: Vec<&str> = t.split('-').collect();
        if let [.., r, d] = parts[..] {
            rate = leading(r).unwrap_or(rate);
            depth = leading(d).unwrap_or(depth);
            if rate > 0 && depth > 0 {
                return (rate, depth);
            }
        }
    }
    (96_000, 24)
}

/// Bit rate (bit/s), channel count and MIME type for a song track. Apple Music defaults, informational only.
fn audio_format(codec: Codec) -> (u32, u32, &'static str) {
    match codec {
        Codec::Aac => (256_000, 2, r#"audio/mp4; codecs="mp4a.40.2""#),
        // Lossless bit rate depends on the track: 0 is unknown.
        Codec::Alac => (0, 2, r#"audio/mp4; codecs="alac""#),
        // Dolby Atmos EC-3: the bed is 7.1 (8 channels).
        Codec::Atmos => (768_000, 8, r#"audio/mp4; codecs="ec-3""#),
        Codec::H264 => (0, 0, ""),
    }
}

fn artwork_url(template: &str) -> String {
    template
        .replace("{w}", ARTWORK_SIZE)
        .replace("{h}", ARTWORK_SIZE)
}

fn catalog_ms(ms: u64) -> u32 {
    u32::try_from(ms).unwrap_or(u32::MAX)
}

impl AppleMusicProvider {
    /// A provider over the engine's shared HTTP client and AAC stream context. `cbcs` opens the FairPlay
    /// connections for ALAC and Atmos (without it those requests fail). `account`, when set, supplies the
    /// progressive music-video URL.
    pub fn new(
        http: reqwest::Client,
        streams: Arc<Streams>,
        cbcs: Option<Arc<dyn CbcsDialer>>,
        account: Option<Arc<dyn AccountTokenSource>>,
    ) -> Self {
        Self {
            catalog: rustet_ampapi::Client::new(http.clone()),
            http,
            streams,
            cbcs,
            account,
            refusals: Arc::default(),
        }
    }

    /// The request's developer token, or a freshly scraped one when it has none. Every call of one open shares it.
    async fn token(&self, req: &OpenRequest) -> Result<String, Error> {
        let token = req.token.strip_prefix("Bearer ").unwrap_or(&req.token);
        if !token.is_empty() {
            return Ok(token.to_owned());
        }
        self.catalog
            .token()
            .await
            .map_err(|e| Error::caused("auto-fetch developer token", Cause::Catalog(e)))
    }

    async fn webplayback(
        &self,
        req: &OpenRequest,
        token: &str,
        cancel: &CancellationToken,
    ) -> Result<webplayback::Response, Error> {
        let request = webplayback::Request {
            adam_id: &req.asset_id,
            storefront: &req.storefront,
            token,
            mut_token: &req.mut_token,
        };
        webplayback::fetch(&self.http, WEBPLAYBACK_URL, &request, cancel).await
    }

    fn hls_opener(
        &self,
        req: &OpenRequest,
        token: &str,
        auth: bool,
        kind: StreamKind,
        codec: Codec,
    ) -> HlsOpener {
        HlsOpener {
            streams: Arc::clone(&self.streams),
            http: self.http.clone(),
            asset_id: req.asset_id.clone(),
            token: token.to_owned(),
            mut_token: req.mut_token.clone(),
            auth,
            kind,
            codec,
        }
    }

    async fn open_song(
        &self,
        cancel: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<Session, Error> {
        let token = self.token(req).await?;
        let not_found = || format!("song {} not found in {}", req.asset_id, req.storefront);
        let song = self
            .catalog
            .song(
                cancel,
                &req.storefront,
                &req.asset_id,
                &req.language,
                &token,
            )
            .await
            .map_err(|e| Error::caused(not_found(), Cause::Catalog(e)))?
            .data
            .into_iter()
            .next()
            .ok_or_else(|| Error::not_found(not_found()))?;
        let a = song.attributes;
        let enhanced_hls = &a.extended_asset_urls.enhanced_hls;
        if enhanced_hls.is_empty() {
            return Err(Error::unavailable(format!(
                "no HLS URL for track {}",
                req.asset_id
            )));
        }
        let duration_ms = catalog_ms(a.duration_in_millis);

        let (codec, sample_rate, bit_depth, open): (
            _,
            _,
            _,
            Arc<dyn rustet_core::media::OpenTrack>,
        ) = match AudioPlan::choose(req, &a.audio_traits) {
            AudioPlan::Aac => {
                // Standard AAC (256 kbps) is CTR-encrypted in the webplayback API's 28:ctrp256 asset: its
                // media playlists carry URI="data:;base64,<kid>", which the Widevine licence provider handles.
                // The catalog's enhanced-HLS AAC variants carry skd:// (FairPlay) keys instead.
                let url = self
                    .webplayback(req, &token, cancel)
                    .await
                    .and_then(|r| r.asset_url(AssetFlavor::Ctr256))
                    .map_err(|e| e.context("webplayback AAC asset"))?;
                let opener = self.hls_opener(req, &token, false, StreamKind::Audio, Codec::Aac);
                (Codec::Aac, 0, 0, Arc::new(HlsTrack { opener, url }))
            }
            AudioPlan::Cbcs {
                codec,
                hls_codec,
                sample_rate,
                bit_depth,
            } => {
                let master = Master::open(&self.http, enhanced_hls, None, cancel)
                    .await
                    .map_err(|e| Error::caused("open master playlist", Cause::Playlist(e)))?;
                let url = master
                    .select_by_codec(hls_codec)
                    .ok_or_else(|| {
                        Error::unavailable(format!("no HLS variant matching codec {hls_codec:?}"))
                    })?
                    .to_owned();
                let dialer = self.cbcs.clone().ok_or_else(|| {
                    Error::Other(crate::error::Failure::new(
                        "ALAC/Atmos requires CBCS decryption (no CBCS dialer configured)",
                        None,
                    ))
                })?;
                let track = CbcsTrack {
                    http: self.http.clone(),
                    dialer,
                    asset_id: req.asset_id.clone(),
                    url,
                    codec,
                    duration_ms,
                };
                (codec, sample_rate, bit_depth, Arc::new(track))
            }
        };

        let (bit_rate, channel_count, mime) = audio_format(codec);
        Ok(Session {
            kind: SessionKind::Song,
            metadata: rustet_core::media::Metadata {
                title: a.name,
                artist_name: a.artist_name,
                album_name: a.album_name,
                duration_ms,
                artwork_url: artwork_url(&a.artwork.url),
                has_lyrics: a.has_lyrics,
            },
            tracks: vec![Track {
                kind: StreamKind::Audio,
                codec,
                codec_string: String::new(),
                sample_rate,
                bit_depth,
                bit_rate,
                channel_count,
                codec_mime_type: mime.to_owned(),
                spatial_audio: None,
                open,
            }],
            video_heights: Vec::new(),
            mv_progressive_url: None,
            mv_download_key: None,
        })
    }

    async fn open_mv(
        &self,
        cancel: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<Session, Error> {
        let id = &req.asset_id;
        let requested_height = match req.mv_max_height {
            0 => DEFAULT_MV_MAX_HEIGHT,
            h => h,
        };
        let token = self.token(req).await?;

        let mv = self
            .catalog
            .music_video(cancel, &req.storefront, id, &req.language, &token)
            .await
            .map_err(|e| {
                Error::caused(
                    format!("MV catalog lookup {}/{id}", req.storefront),
                    Cause::Catalog(e),
                )
            })?
            .data
            .into_iter()
            .next()
            .ok_or_else(|| Error::not_found(format!("MV {id} not found in {}", req.storefront)))?;
        let a = mv.attributes;

        let master_url = self
            .webplayback(req, &token, cancel)
            .await
            .and_then(webplayback::Response::playlist_url)
            .map_err(|e| e.context("webplayback API"))?;
        let auth = Auth {
            developer_token: &token,
            media_user_token: &req.mut_token,
        };
        let master = Master::open(&self.http, &master_url, Some(auth), cancel)
            .await
            .map_err(|e| Error::caused("open MV master playlist", Cause::Playlist(e)))?;
        let video_heights = master.video_heights();
        info!(
            "[mv] assetID={id} masterURL={master_url} nVariants={} videoHeights={video_heights:?}",
            master.variants.len()
        );

        let audio_url = if req.mv_audio_priorities.is_empty() {
            master.select_audio_variant(&DEFAULT_MV_AUDIO_PRIORITIES)
        } else {
            master.select_audio_variant(&req.mv_audio_priorities)
        }
        .map_err(|e| Error::caused("select audio variant", Cause::Playlist(e)))?
        .to_owned();

        // Video always takes the HLS CBCS segment path: the progressive CDN files carry iTunes FairPlay DRM (itun)
        // that Chromium on Linux cannot decrypt and FFmpeg cannot process.
        let max_height = self.refusals.cap(id, requested_height);
        if max_height != requested_height {
            info!(
                "[mv-quality] {id}: requested {}, capped to {max_height}p: the licence for a taller tier was refused \
                 earlier",
                quality_name(requested_height)
            );
        }
        let selected = master
            .select_video_variant(max_height)
            .map_err(|e| Error::caused("select video variant", Cause::Playlist(e)))?
            .clone();
        rustet_ampapi::mvlabel::set(&selected.resolution);
        info!(
            "[mv-quality] {id}: requested {} (limit {max_height}p), selected {} {} range={} hdcp={:?} allowed key \
             systems: {}",
            quality_name(requested_height),
            selected.resolution,
            selected.codecs,
            selected.video_range,
            selected.hdcp_level,
            cpc_summary(&selected.allowed_cpc)
        );
        info!(
            "[mv-quality] {id}: variants offered: {}",
            describe_variants(&master.variants)
        );

        let video = Track {
            kind: StreamKind::Video,
            codec: Codec::H264,
            codec_string: selected.codecs.clone(),
            sample_rate: 0,
            bit_depth: 0,
            bit_rate: 0,
            channel_count: 0,
            codec_mime_type: String::new(),
            spatial_audio: None,
            open: Arc::new(VideoFallback {
                master,
                first: selected,
                asset_id: id.clone(),
                requested_height,
                refusals: Arc::clone(&self.refusals),
                opener: self.hls_opener(req, &token, true, StreamKind::Video, Codec::H264),
            }),
        };
        let audio = Track {
            kind: StreamKind::Audio,
            codec: Codec::Aac,
            codec_string: String::new(),
            sample_rate: 0,
            bit_depth: 0,
            bit_rate: 0,
            channel_count: 0,
            codec_mime_type: String::new(),
            spatial_audio: None,
            open: Arc::new(HlsTrack {
                opener: self.hls_opener(req, &token, true, StreamKind::Audio, Codec::Aac),
                url: audio_url,
            }),
        };

        let progressive = self.progressive_mv_url(id).await;
        info!(
            "[mv] openMV {id}: returning sess with 2 tracks mvProgressiveURL={}",
            progressive.is_some()
        );
        Ok(Session {
            kind: SessionKind::MusicVideo,
            metadata: rustet_core::media::Metadata {
                title: a.name,
                artist_name: a.artist_name,
                album_name: a.album_name,
                duration_ms: catalog_ms(a.duration_in_millis),
                artwork_url: artwork_url(&a.artwork.url),
                has_lyrics: false,
            },
            tracks: vec![video, audio],
            video_heights,
            mv_download_key: progressive.as_ref().and_then(|p| p.download_key.clone()),
            mv_progressive_url: progressive.map(|p| p.url),
        })
    }

    /// The progressive CDN URL and download key from the native session. An optimisation, never fatal: the HLS
    /// path is always the primary. Opt-in (`RUSTET_MV_PROGRESSIVE=1`) because the lookup costs seconds on every open
    /// and only the native `<video>` backend consumes it.
    async fn progressive_mv_url(&self, asset_id: &str) -> Option<ProgressiveMvUrl> {
        let account = self.account.as_ref()?;
        if std::env::var("RUSTET_MV_PROGRESSIVE").as_deref() != Ok("1") {
            return None;
        }
        let adam_id = asset_id.parse::<u64>().ok()?;
        match tokio::time::timeout(PROGRESSIVE_TIMEOUT, account.progressive_mv_url(adam_id)).await {
            Ok(Ok(p)) if !p.url.is_empty() => {
                info!(
                    "[mv] progressive URL obtained for adamID={asset_id} keyLen={}",
                    p.download_key.as_ref().map_or(0, String::len)
                );
                Some(p)
            }
            Ok(Ok(_)) => None,
            Ok(Err(e)) => {
                info!("[mv] GetProgressiveMVURL adamID={asset_id}: {e} (non-fatal)");
                None
            }
            Err(_) => {
                info!("[mv] GetProgressiveMVURL adamID={asset_id}: timed out (non-fatal)");
                None
            }
        }
    }
}

#[async_trait]
impl Provider for AppleMusicProvider {
    /// Errors are [`Error`] inside [`PipelineError::Other`]; a cancelled open is [`PipelineError::Cancelled`].
    async fn open(
        &self,
        cancel: &CancellationToken,
        req: &OpenRequest,
    ) -> Result<Session, PipelineError> {
        let session = if req.video {
            self.open_mv(cancel, req).await
        } else {
            self.open_song(cancel, req).await
        };
        Ok(session?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traits(t: &[&str]) -> Vec<String> {
        t.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn alac_quality_reads_the_trailing_rate_and_depth() {
        assert_eq!(alac_quality(&traits(&["lossless-audio-192-24"])), (192, 24));
        assert_eq!(
            alac_quality(&traits(&["lossless", "atmos"])),
            (96_000, 24),
            "no parseable quality"
        );
    }

    #[test]
    fn the_plan_follows_the_request_and_the_traits() {
        let all = traits(&["atmos", "lossless", "hi-res-lossless", "lossy-stereo"]);
        let req = |lossless, hi_res, atmos| OpenRequest {
            lossless,
            hi_res,
            atmos,
            ..OpenRequest::default()
        };
        let codec = |plan| match plan {
            AudioPlan::Aac => (Codec::Aac, 0, 0),
            AudioPlan::Cbcs {
                codec,
                sample_rate,
                bit_depth,
                ..
            } => (codec, sample_rate, bit_depth),
        };
        let cases = [
            (
                "atmos wins",
                req(true, true, true),
                &all,
                (Codec::Atmos, 0, 0),
            ),
            (
                "hi-res",
                req(true, true, false),
                &all,
                (Codec::Alac, 96_000, 24),
            ),
            (
                "lossless capped at CD quality",
                req(true, false, false),
                &all,
                (Codec::Alac, 44_100, 16),
            ),
            (
                "hi-res needs lossless",
                req(false, true, false),
                &all,
                (Codec::Aac, 0, 0),
            ),
            (
                "nothing asked",
                req(false, false, false),
                &all,
                (Codec::Aac, 0, 0),
            ),
        ];
        for (name, r, t, want) in cases {
            assert_eq!(codec(AudioPlan::choose(&r, t)), want, "{name}");
        }
        let aac_only = traits(&["lossy-stereo"]);
        assert_eq!(
            AudioPlan::choose(&req(true, true, true), &aac_only),
            AudioPlan::Aac,
            "traits missing"
        );
    }
}
