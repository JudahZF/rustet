//! The track openers: what runs when a caller opens a [`Track`](rustet_core::media::Track) of a session.
//!
//! - [`HlsTrack`]: AAC songs and music-video audio, the CTR / Widevine path through `rustet-streams`.
//! - [`VideoFallback`]: music-video video, the same path, stepping down a tier when the licence is refused.
//! - [`CbcsTrack`]: ALAC and Atmos, the CBCS / FairPlay path through `rustet-fairplay`.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rustet_core::media::{OpenTrack, reconcile_duration_ms};
use rustet_core::pipeline::{
    Codec, DecryptStage, PipelineError, Source, Stage, Stream, StreamKind,
};
use rustet_fairplay::{CbcsDialer, CbcsSource, first_audio_key_uri, warm_cbcs_key};
use rustet_hls::{Auth, CbcsMedia, Master, Media, Variant, cpc_summary};
use rustet_streams::Streams;
use rustet_streams::license::{HlsSource, MvVideoSource, PassthroughDecryptor};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::error::{Cause, Error};

/// Sums a media playlist's segment durations (seconds) into milliseconds; 0 when it declares none.
pub(crate) fn declared_duration_ms(segments: impl IntoIterator<Item = f64>) -> u32 {
    let total: f64 = segments.into_iter().filter(|&d| d > 0.0).sum();
    // Saturating float-to-int cast.
    (total * 1000.0 + 0.5) as u32
}

// ── CTR / Widevine ───────────────────────────────────────────────────────────

/// Opens HLS media playlists of one item into streams: fetches the playlist, acquires the licence when it is
/// encrypted, and builds the seekable segment source.
#[derive(Clone)]
pub(crate) struct HlsOpener {
    pub streams: Arc<Streams>,
    pub http: reqwest::Client,
    pub asset_id: String,
    pub token: String,
    pub mut_token: String,
    /// Fetch the playlist with the Apple Music headers (music videos on play.itunes.apple.com).
    pub auth: bool,
    pub kind: StreamKind,
    pub codec: Codec,
}

/// Opens one variant playlist. [`VideoFallback`] goes through it so tests can stand in for the network.
#[async_trait]
pub(crate) trait OpenVariant: Send + Sync {
    async fn open_variant(&self, url: &str, cancel: &CancellationToken) -> Result<Stream, Error>;
}

#[async_trait]
impl OpenVariant for HlsOpener {
    async fn open_variant(&self, url: &str, cancel: &CancellationToken) -> Result<Stream, Error> {
        let kind = self.kind;
        debug!(
            "[apple] {kind}: opening media playlist url={url} auth={}",
            self.auth
        );
        let auth = self.auth.then_some(Auth {
            developer_token: &self.token,
            media_user_token: &self.mut_token,
        });
        let media = Media::open(&self.http, url, auth, cancel)
            .await
            .map_err(|e| {
                info!("[apple] {kind}: open media playlist FAILED: {e}");
                Error::caused("open media playlist", Cause::Playlist(e))
            })?;
        debug!(
            "[apple] {kind}: media playlist OK nSegs={} enc={}",
            media.segments.len(),
            media.encryption.is_some()
        );
        let stage: Arc<dyn Stage> = match &media.encryption {
            None => {
                info!(
                    "[apple] {kind} {url}: enc=nil → passthrough (no content-level encryption detected)"
                );
                Arc::new(DecryptStage(PassthroughDecryptor))
            }
            Some(enc) => {
                debug!(
                    "[apple] {kind} {url}: enc detected prefix={:?} → acquiring licence",
                    enc.uri_prefix
                );
                let req = rustet_streams::LicenceRequest {
                    adam_id: &self.asset_id,
                    kid_base64: &enc.kid_base64,
                    uri_prefix: &enc.uri_prefix,
                    token: &self.token,
                    media_user_token: &self.mut_token,
                };
                let decryptor = self.streams.open_licence(&req, cancel).await.map_err(|e| {
                    info!(
                        "[apple] {kind} {}: licence refused: method={} keyformat={:?} keys={} uriPrefix={:?} \
                         kidLen={}: {e}",
                        self.asset_id,
                        enc.method,
                        enc.key_format,
                        enc.key_count,
                        enc.uri_prefix,
                        enc.kid_base64.len()
                    );
                    Error::caused("acquire licence", Cause::Licence(e))
                })?;
                Arc::new(DecryptStage(decryptor))
            }
        };
        let duration_ms = declared_duration_ms(media.segments.iter().map(|s| s.duration));
        let source: Arc<dyn Source> = match kind {
            // Music-video video has its own segment cache.
            StreamKind::Video => Arc::new(MvVideoSource::new(Arc::clone(&self.streams), media)),
            StreamKind::Audio => Arc::new(HlsSource::new(Arc::clone(&self.streams), media)),
        };
        Ok(Stream {
            source,
            stages: vec![stage],
            kind,
            codec: self.codec,
            duration_ms,
            codec_string: None,
        })
    }
}

/// A track on one media playlist.
pub(crate) struct HlsTrack {
    pub opener: HlsOpener,
    pub url: String,
}

#[async_trait]
impl OpenTrack for HlsTrack {
    async fn open(&self, cancel: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(self.opener.open_variant(&self.url, cancel).await?)
    }
}

// ── Music-video tier fallback ────────────────────────────────────────────────

/// How long a refused tier stays remembered.
const REFUSAL_TTL: Duration = Duration::from_secs(3600);

/// Per video, the lowest height whose licence was refused recently, so later opens start below it.
#[derive(Debug, Default)]
pub(crate) struct Refusals(Mutex<HashMap<String, Refusal>>);

#[derive(Debug, Clone, Copy)]
struct Refusal {
    height: u32,
    at: Instant,
}

impl Refusals {
    fn remember(&self, asset_id: &str, height: u32) {
        if height == 0 {
            return;
        }
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = map.get(asset_id)
            && prev.at.elapsed() <= REFUSAL_TTL
            && prev.height <= height
        {
            return; // keep the lower ceiling
        }
        map.insert(
            asset_id.to_owned(),
            Refusal {
                height,
                at: Instant::now(),
            },
        );
    }

    /// Lowers a requested maximum height to just below one whose licence was refused for this video recently.
    pub(crate) fn cap(&self, asset_id: &str, max_height: u32) -> u32 {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = map.get(asset_id).copied() else {
            return max_height;
        };
        if r.at.elapsed() > REFUSAL_TTL {
            map.remove(asset_id);
            return max_height;
        }
        // `remember` never stores height 0, so this does not underflow.
        if max_height >= r.height {
            r.height - 1
        } else {
            max_height
        }
    }
}

/// The video track of a music video. It opens `first` and, when the licence server refuses its key (a tier this
/// client may not play, such as 4K for a software Widevine identity), retries on the next lower variant until one
/// opens or none is left. The variant that opened sets the stream's codec string and the quality label, and the
/// refusal is remembered so a reopen of the same video does not repeat it.
pub(crate) struct VideoFallback<O> {
    pub master: Master,
    pub first: Variant,
    pub asset_id: String,
    pub requested_height: u32,
    pub refusals: Arc<Refusals>,
    pub opener: O,
}

impl<O: OpenVariant> VideoFallback<O> {
    async fn open_with_fallback(&self, cancel: &CancellationToken) -> Result<Stream, Error> {
        let mut v = &self.first;
        loop {
            match self.opener.open_variant(&v.url, cancel).await {
                Ok(mut stream) => {
                    stream.codec_string = Some(v.codecs.clone());
                    if v.url != self.first.url {
                        rustet_ampapi::mvlabel::set(&v.resolution);
                    }
                    info!(
                        "{}",
                        quality_line(&self.asset_id, self.requested_height, &self.first, v)
                    );
                    return Ok(stream);
                }
                Err(e) if !matches!(e, Error::LicenceRefused(_)) || cancel.is_cancelled() => {
                    return Err(e);
                }
                Err(e) => {
                    self.refusals.remember(&self.asset_id, v.height());
                    let Some(next) = self.master.video_fallback(v) else {
                        info!(
                            "[mv-quality] {}: requested {}: NO PLAYABLE VARIANT, the licence was refused for every tier",
                            self.asset_id,
                            quality_name(self.requested_height)
                        );
                        return Err(e);
                    };
                    info!(
                        "[mv-quality] {}: licence refused for {} {}; trying {} {}",
                        self.asset_id, v.resolution, v.codecs, next.resolution, next.codecs
                    );
                    v = next;
                }
            }
        }
    }
}

#[async_trait]
impl<O: OpenVariant> OpenTrack for VideoFallback<O> {
    async fn open(&self, cancel: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(self.open_with_fallback(cancel).await?)
    }
}

/// Names a requested maximum height the way the quality menu does: 2160 is 4K, the rest are NNNp.
pub(crate) fn quality_name(height: u32) -> String {
    if height >= 2160 {
        "4K (2160p)".to_owned()
    } else {
        format!("{height}p")
    }
}

/// The one line that says what quality a music video was asked for and what it plays at. `selected` is what the
/// quality choice picked from the master; `played` is what actually opened, which differs after a fallback.
fn quality_line(
    asset_id: &str,
    requested_height: u32,
    selected: &Variant,
    played: &Variant,
) -> String {
    let mut line = format!(
        "[mv-quality] {asset_id}: requested {} -> PLAYING {} {} range={} {:.1} Mbps",
        quality_name(requested_height),
        played.resolution,
        played.codecs,
        played.video_range,
        f64::from(played.bandwidth) / 1e6
    );
    if played.url != selected.url {
        line += &format!(
            " (FELL BACK from {} {}: licence refused)",
            selected.resolution, selected.codecs
        );
    } else if requested_height > 0 && played.height() > 0 && played.height() < requested_height {
        line += " (the tallest this video offers within the request)";
    }
    line
}

/// Lists a master's video variants as `height:codec:range:hdcp:[allowed key systems]`, tallest first, so a log
/// shows which tiers exist and what each one declares (HDCP-LEVEL says whether a tier demands output protection).
pub(crate) fn describe_variants(variants: &[Variant]) -> String {
    let mut seen = HashSet::new();
    let mut tiers: Vec<(u32, String)> = variants
        .iter()
        .filter(|v| v.height() > 0)
        .filter_map(|v| {
            let codec = v.codecs.split(',').next().unwrap_or_default();
            let codec = match codec.find('.') {
                Some(i) if i > 0 => &codec[..i],
                _ => codec,
            };
            let d = format!(
                "{}p:{codec}:{}:{}:[{}]",
                v.height(),
                v.video_range,
                v.hdcp_level,
                cpc_summary(&v.allowed_cpc)
            );
            seen.insert(d.clone()).then_some((v.height(), d))
        })
        .collect();
    tiers.sort_by_key(|&(h, _)| Reverse(h));
    tiers
        .into_iter()
        .map(|(_, d)| d)
        .collect::<Vec<_>>()
        .join(" ")
}

// ── CBCS / FairPlay ──────────────────────────────────────────────────────────

/// An ALAC or Atmos track. Opening parses the media playlist and builds a [`CbcsSource`] that decrypts through the
/// dialer (in-process DRM). The stream has no stages: all decryption happens inside the source.
pub(crate) struct CbcsTrack {
    pub http: reqwest::Client,
    pub dialer: Arc<dyn CbcsDialer>,
    pub asset_id: String,
    pub url: String,
    pub codec: Codec,
    /// The catalog length.
    pub duration_ms: u32,
}

impl CbcsTrack {
    async fn open_stream(&self, cancel: &CancellationToken) -> Result<Stream, Error> {
        let media = CbcsMedia::open(&self.http, &self.url, cancel)
            .await
            .map_err(|e| Error::caused("open CBCS media playlist", Cause::Playlist(e)))?;
        // Pre-warm the FairPlay key in the background so it is already in the Android key cache by the time the
        // CBCS pipeline needs it. This overlaps the ~2 s key fetch with the ~2 s CDN init-segment download, cutting
        // ALAC start latency roughly in half for first plays.
        if let Some(key) = first_audio_key_uri(&media) {
            let (dialer, id, key) = (
                Arc::clone(&self.dialer),
                self.asset_id.clone(),
                key.to_owned(),
            );
            tokio::spawn(async move { warm_cbcs_key(&*dialer, &id, &key).await });
        }
        // The file's own length wins over the catalog's: it is stored in the moov (the player reports it) and is
        // where the stream really ends.
        let declared = declared_duration_ms(media.segments.iter().map(|s| s.duration));
        let duration_ms = reconcile_duration_ms(self.duration_ms, declared);
        let source = CbcsSource::new(
            self.asset_id.clone(),
            Arc::clone(&self.dialer),
            self.http.clone(),
            &media,
            duration_ms,
        );
        Ok(Stream {
            source: Arc::new(source),
            stages: Vec::new(),
            kind: StreamKind::Audio,
            codec: self.codec,
            duration_ms: declared,
            codec_string: None,
        })
    }
}

#[async_trait]
impl OpenTrack for CbcsTrack {
    async fn open(&self, cancel: &CancellationToken) -> Result<Stream, PipelineError> {
        Ok(self.open_stream(cancel).await?)
    }
}

#[cfg(test)]
mod tests {
    use rustet_core::pipeline::{Sink, StreamContext};
    use rustet_fairplay::TcpDialer;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::error::Failure;

    /// Serialises the tests that read the process-wide quality label.
    static LABEL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn declared_duration() {
        for (name, segs, want) in [
            ("none declared", &[][..], 0),
            ("sums segments", &[10.0, 10.0, 4.3456], 24346),
            ("rounds to the millisecond", &[1.0004, 1.0004], 2001),
            ("ignores non-positive entries", &[5.0, -3.0, 0.0, 2.5], 7500),
        ] {
            assert_eq!(declared_duration_ms(segs.iter().copied()), want, "{name}");
        }
    }

    struct NoSource;

    #[async_trait]
    impl Source for NoSource {
        async fn stream(&self, _: &StreamContext, _: &mut dyn Sink) -> Result<(), PipelineError> {
            Ok(())
        }
    }

    /// Fails the variants in `refuse` with `error` and opens any other.
    struct FakeOpener {
        refuse: &'static [&'static str],
        error: fn() -> Error,
        tried: Mutex<Vec<String>>,
    }

    impl FakeOpener {
        fn new(refuse: &'static [&'static str], error: fn() -> Error) -> Self {
            Self {
                refuse,
                error,
                tried: Mutex::default(),
            }
        }
    }

    #[async_trait]
    impl OpenVariant for FakeOpener {
        async fn open_variant(&self, url: &str, _: &CancellationToken) -> Result<Stream, Error> {
            self.tried.lock().unwrap().push(url.to_owned());
            if self.refuse.contains(&url) {
                return Err((self.error)());
            }
            Ok(Stream {
                source: Arc::new(NoSource),
                stages: Vec::new(),
                kind: StreamKind::Video,
                codec: Codec::H264,
                duration_ms: 0,
                codec_string: None,
            })
        }
    }

    fn refused() -> Error {
        Error::LicenceRefused(Failure::new(
            "acquire licence: fairplay licence: error in license response, code: -42604, status: -1021",
            None,
        ))
    }

    fn net_error() -> Error {
        Error::Transport(Failure::new("dial tcp: i/o timeout", None))
    }

    fn variant(url: &str, resolution: &str, codecs: &str, bandwidth: u32) -> Variant {
        Variant {
            url: url.to_owned(),
            resolution: resolution.to_owned(),
            codecs: codecs.to_owned(),
            bandwidth,
            ..Variant::default()
        }
    }

    fn fallback(
        id: &str,
        opener: FakeOpener,
        refusals: &Arc<Refusals>,
    ) -> VideoFallback<FakeOpener> {
        let master = Master {
            variants: vec![
                variant("uhd", "3840x1598", "hvc1.2.4", 20_000_000),
                variant("hd", "1918x798", "avc1.640028", 10_000_000),
                variant("sd", "864x360", "avc1.64001e", 1_000_000),
            ],
            alternatives: Vec::new(),
        };
        VideoFallback {
            first: master.variants[0].clone(),
            master,
            asset_id: id.to_owned(),
            requested_height: 2160,
            refusals: Arc::clone(refusals),
            opener,
        }
    }

    fn tried(f: &VideoFallback<FakeOpener>) -> Vec<String> {
        f.opener.tried.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn video_open_falls_back_when_the_licence_is_refused() {
        let _label = LABEL.lock().await;
        let refusals = Arc::default();
        let f = fallback("a1", FakeOpener::new(&["uhd"], refused), &refusals);
        let stream = f
            .open_with_fallback(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(tried(&f), ["uhd", "hd"]);
        assert_eq!(
            stream.codec_string.as_deref(),
            Some("avc1.640028"),
            "the fallback variant's codecs"
        );
        assert_eq!(rustet_ampapi::mvlabel::get(), "1918x798");
        // The refusal is remembered: a reopen starts below it.
        assert_eq!(refusals.cap("a1", 2160), 1597);
        assert_eq!(
            refusals.cap("a1", 720),
            720,
            "a request already below the refused height"
        );
        assert_eq!(
            refusals.cap("other-asset", 2160),
            2160,
            "another video is not capped"
        );
    }

    #[tokio::test]
    async fn video_open_keeps_working_down_the_tiers() {
        let _label = LABEL.lock().await;
        let refusals = Arc::default();
        let f = fallback("a2", FakeOpener::new(&["uhd", "hd"], refused), &refusals);
        f.open_with_fallback(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(tried(&f), ["uhd", "hd", "sd"]);
        // The lowest refused tier is the ceiling.
        assert_eq!(refusals.cap("a2", 2160), 797);
    }

    #[tokio::test]
    async fn video_open_fails_when_every_tier_is_refused() {
        let refusals = Arc::default();
        let f = fallback(
            "a3",
            FakeOpener::new(&["uhd", "hd", "sd"], refused),
            &refusals,
        );
        let Err(err) = f.open_with_fallback(&CancellationToken::new()).await else {
            panic!("want an error");
        };
        assert!(matches!(err, Error::LicenceRefused(_)), "{err:?}");
        assert_eq!(tried(&f), ["uhd", "hd", "sd"]);
    }

    /// Only a refusal moves down a tier; a network failure or a cancelled open is reported as it is.
    #[tokio::test]
    async fn video_open_does_not_fall_back_on_other_errors() {
        let refusals = Arc::default();
        let f = fallback("a4", FakeOpener::new(&["uhd"], net_error), &refusals);
        let Err(err) = f.open_with_fallback(&CancellationToken::new()).await else {
            panic!("want an error");
        };
        assert!(matches!(err, Error::Transport(_)), "{err:?}");
        assert_eq!(tried(&f), ["uhd"]);

        let cancel = CancellationToken::new();
        cancel.cancel();
        let f = fallback("a5", FakeOpener::new(&["uhd"], refused), &refusals);
        assert!(
            f.open_with_fallback(&cancel).await.is_err(),
            "a cancelled open fails"
        );
        assert_eq!(tried(&f), ["uhd"], "and does not fall back");
    }

    #[test]
    fn refusals_keep_the_lower_ceiling() {
        let r = Refusals::default();
        r.remember("v", 0);
        assert_eq!(
            r.cap("v", 2160),
            2160,
            "an unknown height is not remembered"
        );
        r.remember("v", 798);
        r.remember("v", 1598);
        assert_eq!(r.cap("v", 2160), 797);
    }

    #[test]
    fn quality_line_says_what_was_requested_and_what_plays() {
        let uhd = Variant {
            video_range: "PQ".into(),
            ..variant("uhd", "3840x1598", "hvc1.2.4", 20_000_000)
        };
        let hd = Variant {
            video_range: "SDR".into(),
            ..variant("hd", "1918x798", "avc1.640028", 10_200_000)
        };
        for (name, requested, selected, played, want) in [
            (
                "fell back from 4K",
                2160,
                &uhd,
                &hd,
                "[mv-quality] a1: requested 4K (2160p) -> PLAYING 1918x798 avc1.640028 range=SDR 10.2 Mbps \
                 (FELL BACK from 3840x1598 hvc1.2.4: licence refused)",
            ),
            (
                "as requested",
                1080,
                &hd,
                &hd,
                "[mv-quality] a1: requested 1080p -> PLAYING 1918x798 avc1.640028 range=SDR 10.2 Mbps \
                 (the tallest this video offers within the request)",
            ),
            (
                "exact match",
                798,
                &hd,
                &hd,
                "[mv-quality] a1: requested 798p -> PLAYING 1918x798 avc1.640028 range=SDR 10.2 Mbps",
            ),
        ] {
            assert_eq!(
                quality_line("a1", requested, selected, played),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn describe_variants_lists_each_tier_with_its_protection() {
        let got = describe_variants(&[
            Variant {
                video_range: "SDR".into(),
                ..variant("", "864x360", "avc1.64001e,mp4a.40.2", 0)
            },
            Variant {
                video_range: "SDR".into(),
                hdcp_level: "TYPE-1".into(),
                allowed_cpc:
                    "com.apple.streamingkeydelivery:Main/AppleMain,com.microsoft.playready:SL3000"
                        .into(),
                ..variant("", "3840x1598", "hvc1.2.20000000.H150.B0,mp4a.40.2", 0)
            },
            Variant {
                video_range: "SDR".into(),
                ..variant("", "1918x798", "avc1.640028,mp4a.40.2", 0)
            },
            // A second bitrate of the same tier.
            Variant {
                video_range: "SDR".into(),
                ..variant("", "1918x798", "avc1.640028,mp4a.40.2", 0)
            },
            // An audio-only entry: no height.
            variant("", "", "mp4a.40.2", 0),
        ]);
        assert_eq!(
            got,
            "1598p:hvc1:SDR:TYPE-1:[fairplay:Main/AppleMain playready:SL3000] 798p:avc1:SDR::[unspecified] \
             360p:avc1:SDR::[unspecified]"
        );
    }

    #[tokio::test]
    async fn cbcs_track_reports_the_declared_length_and_decrypts_in_the_source() {
        let playlist = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:10\n\
            #EXT-X-MAP:URI=\"track.mp4\",BYTERANGE=\"1000@0\"\n\
            #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://itunes.apple.com/P000000000/s1/e1\",KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\"\n\
            #EXTINF:10.0,\n#EXT-X-BYTERANGE:5000@1000\ntrack.mp4\n\
            #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://itunes.apple.com/AAAA/s1/e1\",KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\"\n\
            #EXTINF:4.5,\n#EXT-X-BYTERANGE:2000@6000\ntrack.mp4\n#EXT-X-ENDLIST\n";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(playlist))
            .mount(&server)
            .await;
        let track = CbcsTrack {
            http: reqwest::Client::new(),
            // Nothing listens there: the background key warm-up fails quietly.
            dialer: Arc::new(TcpDialer("127.0.0.1:9".into())),
            asset_id: "1".into(),
            url: format!("{}/p.m3u8", server.uri()),
            codec: Codec::Alac,
            duration_ms: 200_000,
        };
        let stream = track.open_stream(&CancellationToken::new()).await.unwrap();
        assert_eq!(stream.duration_ms, 14_500);
        assert_eq!(stream.codec, Codec::Alac);
        assert!(stream.stages.is_empty());
    }

    #[tokio::test]
    async fn a_missing_cbcs_playlist_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let track = CbcsTrack {
            http: reqwest::Client::new(),
            dialer: Arc::new(TcpDialer("127.0.0.1:9".into())),
            asset_id: "1".into(),
            url: format!("{}/p.m3u8", server.uri()),
            codec: Codec::Alac,
            duration_ms: 0,
        };
        let Err(err) = track.open_stream(&CancellationToken::new()).await else {
            panic!("want an error");
        };
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    }
}
