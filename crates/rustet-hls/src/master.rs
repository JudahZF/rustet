//! Master playlists: variants, audio renditions and the selection rules over them.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use url::Url;

use crate::m3u8::{self, ParseError, Playlist, scan_int};
use crate::{HlsError, fetch};

static GR_RANK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_gr(\d+)_").expect("valid regex"));
static DIMENSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_?(\d+)x(\d+)").expect("valid regex"));

/// One quality level (`EXT-X-STREAM-INF` or `EXT-X-I-FRAME-STREAM-INF`) of a master playlist.
/// String attributes are empty when the playlist does not set them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Variant {
    /// Absolute variant playlist URL.
    pub url: String,
    pub bandwidth: u32,
    pub average_bandwidth: u32,
    pub codecs: String,
    pub resolution: String,
    pub video_range: String,
    /// HDCP-LEVEL ("TYPE-0", "TYPE-1", "NONE"): the output protection the variant requires before it may be played.
    pub hdcp_level: String,
    /// ALLOWED-CPC: the key systems, and the security level of each, that may play this variant (for example
    /// FairPlay, PlayReady SL3000, Widevine hardware). A key system missing from it is one the server will not grant
    /// this variant's key to.
    pub allowed_cpc: String,
}

/// One `EXT-X-MEDIA` rendition (audio, subtitles, ...) of a master playlist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Alternative {
    pub group_id: String,
    pub name: String,
    /// Absolute rendition playlist URL.
    pub uri: String,
}

/// A parsed HLS master playlist.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Master {
    pub variants: Vec<Variant>,
    /// Renditions that have a URI, in playlist order.
    pub alternatives: Vec<Alternative>,
}

impl Variant {
    /// Pixel height from RESOLUTION, or from the `<w>x<h>` dimensions in the URL path; 0 when neither says.
    pub fn height(&self) -> u32 {
        let from_resolution = scan_int(&self.resolution)
            .and_then(|(_, rest)| scan_int(rest.strip_prefix('x')?))
            .and_then(|(h, _)| u32::try_from(h).ok())
            .filter(|&h| h > 0);
        from_resolution.unwrap_or_else(|| {
            // Go's url.Parse(...).Path; relative references keep everything before the query or fragment.
            let path = match Url::parse(&self.url) {
                Ok(url) => url.path().to_owned(),
                Err(_) => self
                    .url
                    .split(['?', '#'])
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            };
            DIMENSIONS
                .captures(&path)
                .and_then(|c| c[2].parse().ok())
                .unwrap_or(0)
        })
    }

    fn codecs_contain(&self, needle: &str) -> bool {
        self.codecs.to_ascii_lowercase().contains(needle)
    }

    fn is_h264(&self) -> bool {
        self.codecs_contain("avc1")
    }

    fn is_hevc(&self) -> bool {
        self.codecs_contain("hvc1") || self.codecs_contain("hev1")
    }

    /// AVERAGE-BANDWIDTH is optional; without it every variant would tie at 0, so fall back to BANDWIDTH.
    fn rate(&self) -> u32 {
        if self.average_bandwidth > 0 {
            self.average_bandwidth
        } else {
            self.bandwidth
        }
    }
}

impl Master {
    /// Fetches and parses a master playlist. `auth` adds the Apple Music headers that play.itunes.apple.com needs.
    /// A media playlist at `url` becomes a master with that one variant.
    pub async fn open(
        client: &reqwest::Client,
        url: &str,
        auth: Option<crate::Auth<'_>>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self, HlsError> {
        let (base, body) = fetch(client, url, auth, cancel).await?;
        Ok(Self::parse(&base, &body)?)
    }

    /// Parses a master playlist body; relative URIs resolve against `base`. A media playlist becomes a master with
    /// `base` as its only variant.
    pub fn parse(base: &Url, body: &str) -> Result<Self, ParseError> {
        let pl = match m3u8::parse(body, |_| true)? {
            Playlist::Media(_) => {
                return Ok(Self {
                    variants: vec![Variant {
                        url: base.to_string(),
                        ..Variant::default()
                    }],
                    alternatives: Vec::new(),
                });
            }
            Playlist::Master(pl) => pl,
        };
        let variants = pl
            .variants
            .into_iter()
            .filter_map(|v| {
                Some(Variant {
                    url: base.join(&v.url).ok()?.into(),
                    ..v
                })
            })
            .collect();
        let alternatives = pl
            .alternatives
            .into_iter()
            .filter(|a| !a.uri.is_empty())
            .filter_map(|a| {
                Some(Alternative {
                    uri: base.join(&a.uri).ok()?.into(),
                    ..a
                })
            })
            .collect();
        Ok(Self {
            variants,
            alternatives,
        })
    }

    /// URL of the best audio rendition. The first `priorities` entry that names a GROUP-ID wins, and within that
    /// group the highest `_grN_` rank in the URI. With no match it falls back to the first rendition, so playlists
    /// with non-standard GROUP-ID names still play.
    pub fn select_audio_variant<S: AsRef<str>>(&self, priorities: &[S]) -> Result<&str, HlsError> {
        // (priority, rank, uri); a repeated priority counts at its last position, as the Go map did.
        let mut best: Option<(usize, i64, &str)> = None;
        for alt in &self.alternatives {
            let Some(prio) = priorities.iter().rposition(|p| p.as_ref() == alt.group_id) else {
                continue;
            };
            let rank = GR_RANK
                .captures(&alt.uri)
                .and_then(|c| c[1].parse().ok())
                .unwrap_or(0);
            if best.is_none_or(|(p, r, _)| prio < p || (prio == p && rank > r)) {
                best = Some((prio, rank, &alt.uri));
            }
        }
        best.map(|(_, _, uri)| uri)
            .or_else(|| {
                self.alternatives
                    .iter()
                    .map(|a| a.uri.as_str())
                    .find(|u| !u.is_empty())
            })
            .ok_or(HlsError::NoAudioAlternative)
    }

    /// The best video variant at or below `max_height`.
    ///
    /// H.264 is preferred because HEVC hardware decode is not universal on Linux. When `max_height` is above the
    /// tallest H.264 variant, Apple's HEVC-only tiers (for example 4K at 3840x1920) are the only way to honour it, so
    /// an HEVC variant taller than that ceiling and at or below `max_height` wins. When no variant has a known
    /// height, the highest-rate one is returned.
    pub fn select_video_variant(&self, max_height: u32) -> Result<&Variant, HlsError> {
        let mut sorted: Vec<&Variant> = self.variants.iter().collect();
        sorted.sort_by_key(|v| Reverse(v.rate()));

        let max_h264 = sorted
            .iter()
            .filter(|v| v.is_h264())
            .map(|v| v.height())
            .max()
            .unwrap_or(0);
        if max_height > max_h264 && max_h264 > 0 {
            let above_ceiling = max_h264 + 1..=max_height;
            if let Some(v) = sorted
                .iter()
                .find(|v| v.is_hevc() && above_ceiling.contains(&v.height()))
            {
                return Ok(v);
            }
        }
        for prefer_h264 in [true, false] {
            let mut any_height = false;
            for v in sorted.iter().filter(|v| !prefer_h264 || v.is_h264()) {
                let h = v.height();
                if h > 0 {
                    any_height = true;
                    if h <= max_height {
                        return Ok(v);
                    }
                }
            }
            if !prefer_h264
                && !any_height
                && let Some(v) = sorted.first()
            {
                return Ok(v);
            }
        }
        Err(HlsError::NoVideoVariant { max_height })
    }

    /// URL of the highest-bandwidth variant whose CODECS contains `codec` (case-insensitive), else of the
    /// highest-bandwidth variant. `None` when no variant declares a bandwidth.
    pub fn select_by_codec(&self, codec: &str) -> Option<&str> {
        let codec = codec.to_ascii_lowercase();
        let mut best: Option<&Variant> = None;
        let mut fallback: Option<&Variant> = None;
        let bandwidth = |v: Option<&Variant>| v.map_or(0, |v| v.bandwidth);
        for v in &self.variants {
            if v.bandwidth > bandwidth(fallback) {
                fallback = Some(v);
            }
            if !codec.is_empty() && v.codecs_contain(&codec) && v.bandwidth > bandwidth(best) {
                best = Some(v);
            }
        }
        best.filter(|v| !v.url.is_empty())
            .or(fallback)
            .map(|v| v.url.as_str())
            .filter(|url| !url.is_empty())
    }

    /// The variant to try after the licence for `failed` was refused: the tallest variant below it, H.264 preferred
    /// because every Linux and Windows build can decode it. `None` when nothing lower exists.
    pub fn video_fallback(&self, failed: &Variant) -> Option<&Variant> {
        let limit = failed.height();
        let better = |c: &Variant, b: &Variant| {
            if c.is_h264() != b.is_h264() {
                return c.is_h264();
            }
            if c.height() != b.height() {
                return c.height() > b.height();
            }
            c.bandwidth > b.bandwidth
        };
        self.variants
            .iter()
            .filter(|v| (1..limit).contains(&v.height()))
            .fold(None, |best, v| match best {
                Some(b) if !better(v, b) => Some(b),
                _ => Some(v),
            })
    }

    /// The variant with this URL (as returned by [`Master::select_video_variant`]).
    pub fn variant_for(&self, url: &str) -> Option<&Variant> {
        self.variants.iter().find(|v| v.url == url)
    }

    /// The sorted, unique video heights the quality menu may offer: those [`Master::select_video_variant`] will
    /// actually deliver.
    ///
    /// H.264 heights are always included. HEVC-only heights above the tallest H.264 one are included too, because the
    /// selector switches to HEVC there. HEVC heights at or below that ceiling are not: the selector would return a
    /// lower H.264 stream and the menu would lie. Variants with no CODECS count as H.264: they cannot be ruled out.
    pub fn video_heights(&self) -> Vec<u32> {
        let h264: BTreeSet<u32> = self
            .variants
            .iter()
            .filter(|v| v.codecs.is_empty() || v.is_h264())
            .map(Variant::height)
            .filter(|&h| h > 0)
            .collect();
        let ceiling = h264.last().copied().unwrap_or(0);
        let hevc_above = self
            .variants
            .iter()
            .filter(|v| v.is_hevc())
            .map(Variant::height)
            .filter(|&h| h > ceiling);
        h264.iter()
            .copied()
            .chain(hevc_above)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// Shortens an ALLOWED-CPC value for a log line: the key systems by name with their levels, for example
/// "fairplay:Main/AppleMain playready:SL3000".
pub fn cpc_summary(cpc: &str) -> String {
    if cpc.is_empty() {
        return "unspecified".to_owned();
    }
    cpc.replace("com.apple.streamingkeydelivery", "fairplay")
        .replace("com.microsoft.playready", "playready")
        .replace("urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed", "widevine")
        .replace(',', " ")
}
