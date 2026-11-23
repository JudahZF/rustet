//! AutoEQ headphone correction: parametric EQ bands from the autoeq.app API, applied to the
//! FFmpeg audio pipeline as biquad filter chains.
//!
//! autoeq.app computes the filters for 5,000+ headphone measurements against Harman target
//! curves, so no Python runtime or algorithm port is needed.
//!
//! Filter text format (EqualizerAPO):
//!
//! ```text
//! Preamp: -6.1 dB
//! Filter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70
//! Filter 2: ON PK Fc 8800 Hz Gain 5.1 dB Q 1.42
//! ```

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use tokio_util::sync::CancellationToken;

const BASE_URL: &str = "https://autoeq.app";
/// Every autoeq.app request has this deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The index and target lists are fetched again after this age.
const LIST_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// An error of an AutoEQ operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("autoeq: {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("autoeq: parse eq.json: {0}")]
    ParseSettings(serde_json::Error),
    #[error("autoeq: {what}: {source}")]
    Http {
        what: &'static str,
        source: reqwest::Error,
    },
    #[error("autoeq: {what} HTTP {status}")]
    Status {
        what: &'static str,
        status: reqwest::StatusCode,
    },
    #[error("autoeq: no parametric EQ in response for {0}")]
    NoParametricEq(String),
    #[error("autoeq: response missing frequency/smoothed for {0}")]
    MissingFr(String),
    #[error("autoeq: no valid FR points for {0}")]
    NoFrPoints(String),
    #[error("autoeq: decode fr: {0}")]
    DecodeFr(String),
    #[error("autoeq: no filter bands found in text")]
    NoBands,
    #[error("autoeq: cancelled")]
    Cancelled,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// Go wrote empty slices as `null`; read those as empty.
fn null_as_empty<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    Option::<Vec<T>>::deserialize(d).map(Option::unwrap_or_default)
}

// ── Settings ─────────────────────────────────────────────────────────────────

/// The biquad shape of one band. Serialized as its EqualizerAPO label; read case-insensitively,
/// as the Go engine did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BandType {
    /// Peaking.
    #[serde(rename = "PK")]
    Peaking,
    /// Low shelf.
    #[serde(rename = "LSC")]
    LowShelf,
    /// High shelf.
    #[serde(rename = "HSC")]
    HighShelf,
}

impl<'de> Deserialize<'de> for BandType {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let label = String::deserialize(d)?;
        Self::from_apo_label(&label).ok_or_else(|| {
            serde::de::Error::invalid_value(serde::de::Unexpected::Str(&label), &"PK, LSC or HSC")
        })
    }
}

impl BandType {
    /// Parses an EqualizerAPO label, ignoring case.
    pub fn from_apo_label(label: &str) -> Option<Self> {
        [Self::Peaking, Self::LowShelf, Self::HighShelf]
            .into_iter()
            .find(|t| t.apo_label().eq_ignore_ascii_case(label))
    }

    /// The EqualizerAPO label.
    pub const fn apo_label(self) -> &'static str {
        match self {
            Self::Peaking => "PK",
            Self::LowShelf => "LSC",
            Self::HighShelf => "HSC",
        }
    }

    const fn ffmpeg_filter(self) -> &'static str {
        match self {
            Self::Peaking => "equalizer",
            Self::LowShelf => "lowshelf",
            Self::HighShelf => "highshelf",
        }
    }
}

/// One parametric EQ biquad filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Band {
    #[serde(rename = "type")]
    pub kind: BandType,
    /// Centre or shelf frequency in Hz.
    pub freq: f64,
    /// Gain in dB.
    pub gain: f64,
    /// Q factor (bandwidth).
    pub q: f64,
    /// A disabled band is skipped.
    pub enabled: bool,
}

/// The stored EQ configuration (`eq.json` in the config directory).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Preamp gain in dB (already negative for AutoEQ results).
    pub preamp: f64,
    #[serde(deserialize_with = "null_as_empty")]
    pub bands: Vec<Band>,
}

static FILTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i-u)Filter\s+[0-9]+:\s+ON\s+(PK|LSC|HSC)\s+Fc\s+([0-9.]+)\s+Hz\s+Gain\s+(-?[0-9.]+)\s+dB\s+Q\s+([0-9.]+)",
    )
    .expect("valid filter regex")
});

static PREAMP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i-u)Preamp:\s*(-?[0-9.]+)\s*dB").expect("valid preamp regex"));

impl Settings {
    /// Reads `eq.json` from the config directory. A missing file is empty settings.
    pub async fn load() -> Result<Self, Error> {
        Self::load_from(rustet_core::appdir::config_dir()).await
    }

    async fn load_from(dir: &Path) -> Result<Self, Error> {
        let path = dir.join("eq.json");
        match tokio::fs::read(&path).await {
            Ok(raw) => serde_json::from_slice(&raw).map_err(Error::ParseSettings),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// Writes these settings to `eq.json` in the config directory.
    pub async fn save(&self) -> Result<(), Error> {
        self.save_to(rustet_core::appdir::config_dir()).await
    }

    async fn save_to(&self, dir: &Path) -> Result<(), Error> {
        tokio::fs::create_dir_all(dir).await.map_err(io_err(dir))?;
        let raw = serde_json::to_vec_pretty(self).expect("settings serialize");
        let path = dir.join("eq.json");
        tokio::fs::write(&path, raw).await.map_err(io_err(&path))
    }

    /// The FFmpeg `-af` fragment for these settings, or `""` when there are no bands.
    pub fn ffmpeg_chain(&self) -> String {
        if self.bands.is_empty() {
            return String::new();
        }
        let preamp = (self.preamp != 0.0).then(|| format!("volume={:.2}dB", self.preamp));
        let bands = self.bands.iter().filter(|b| b.enabled).map(|b| {
            format!(
                "{}=f={:.0}:t=q:w={:.4}:g={:.2}",
                b.kind.ffmpeg_filter(),
                b.freq,
                b.q,
                b.gain
            )
        });
        preamp
            .into_iter()
            .chain(bands)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Parses EqualizerAPO parametric EQ text. Only `ON` filters are read.
    pub fn parse_apo(text: &str) -> Result<Self, Error> {
        // Like Go's strconv.ParseFloat with the error ignored: a malformed number is 0.
        let num = |s: &str| s.parse::<f64>().unwrap_or(0.0);
        let mut settings = Self::default();
        for line in text.lines() {
            if let Some(m) = PREAMP_RE.captures(line) {
                settings.preamp = num(&m[1]);
            } else if let Some(m) = FILTER_RE.captures(line) {
                settings.bands.push(Band {
                    kind: BandType::from_apo_label(&m[1]).expect("regex admits only known types"),
                    freq: num(&m[2]),
                    gain: num(&m[3]),
                    q: num(&m[4]),
                    enabled: true,
                });
            }
        }
        if settings.bands.is_empty() {
            return Err(Error::NoBands);
        }
        Ok(settings)
    }

    /// These settings in EqualizerAPO text format. Disabled bands are written as `OFF`.
    pub fn to_apo(&self) -> String {
        let mut out = format!("Preamp: {:.1} dB\n", self.preamp);
        for (i, b) in self.bands.iter().enumerate() {
            let _ = writeln!(
                out,
                "Filter {}: {} {} Fc {:.0} Hz Gain {:.1} dB Q {:.2}",
                i + 1,
                if b.enabled { "ON" } else { "OFF" },
                b.kind.apo_label(),
                b.freq,
                b.gain,
                b.q
            );
        }
        out
    }
}

// ── API types ────────────────────────────────────────────────────────────────

/// One headphone measurement from the AutoEQ index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub name: String,
    pub source: String,
    /// Empty when the measurement names no rig.
    pub rig: String,
    pub form: String,
}

/// A source/form/rig combination compatible with a target curve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetCompat {
    pub source: String,
    pub form: String,
    /// Empty when any rig is compatible.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rig: String,
}

/// One target curve available from autoeq.app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetEntry {
    pub label: String,
    #[serde(deserialize_with = "null_as_empty")]
    pub compatible: Vec<TargetCompat>,
}

/// A single frequency-response point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrPoint {
    pub freq: f64,
    pub gain: f64,
}

/// The original (smoothed) and corrected (equalized, smoothed) frequency-response curves.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrData {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub original: Vec<FrPoint>,
    /// Empty when the API did not return a usable corrected curve.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub corrected: Vec<FrPoint>,
}

/// The default Harman target for a headphone form factor.
pub fn default_target(form: &str) -> &'static str {
    if form.eq_ignore_ascii_case("in-ear") {
        "Harman in-ear 2019"
    } else {
        "Harman over-ear 2018"
    }
}

/// Lowercases and replaces every character outside `[a-z0-9]` with `_`, one per character.
/// Cache file names depend on it, so it must match the Go engine exactly: Go lowercases rune
/// by rune (`İ` → `i`), where `str::to_lowercase` would give two characters.
fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            let l = c.to_lowercase().next().unwrap_or(c);
            if l.is_ascii_lowercase() || l.is_ascii_digit() {
                l
            } else {
                '_'
            }
        })
        .collect()
}

/// `null` rigs in the API are empty strings in our types.
#[derive(Deserialize)]
struct RawVariant {
    form: String,
    rig: Option<String>,
    source: String,
}

#[derive(Deserialize)]
struct RawTarget {
    label: String,
    compatible: Vec<RawVariant>,
}

/// Which result the `/equalize` request asks for.
#[derive(Clone, Copy)]
enum Want {
    ParametricEq,
    FrequencyResponse,
}

#[derive(Serialize)]
struct EqualizeResponseSpec {
    fr_f_step: f64,
    fr_fields: &'static [&'static str],
    base64fp16: bool,
}

/// The body sent to autoeq.app/equalize. The values are the site's own defaults.
#[derive(Serialize)]
struct EqualizeRequest<'a> {
    name: &'a str,
    source: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    rig: &'a str,
    target: &'a str,
    sound_signature: (),
    sound_signature_smoothing_window_size: f64,
    bass_boost_gain: f64,
    bass_boost_fc: f64,
    bass_boost_q: f64,
    treble_boost_gain: f64,
    treble_boost_fc: f64,
    treble_boost_q: f64,
    tilt: f64,
    fs: u32,
    bit_depth: u32,
    phase: &'static str,
    f_res: u32,
    preamp: f64,
    max_gain: f64,
    max_slope: f64,
    window_size: f64,
    treble_window_size: f64,
    treble_f_lower: f64,
    treble_f_upper: f64,
    treble_gain_k: f64,
    graphic_eq: bool,
    parametric_eq: bool,
    fixed_band_eq: bool,
    convolution_eq: bool,
    response: EqualizeResponseSpec,
}

impl<'a> EqualizeRequest<'a> {
    fn new(model: &'a Model, target: &'a str, want: Want) -> Self {
        let (parametric_eq, fr_fields, base64fp16): (_, &'static [&'static str], _) = match want {
            Want::ParametricEq => (true, &[], false),
            Want::FrequencyResponse => (
                false,
                &["frequency", "smoothed", "equalized_smoothed"],
                true,
            ),
        };
        Self {
            name: &model.name,
            source: &model.source,
            rig: &model.rig,
            target,
            sound_signature: (),
            sound_signature_smoothing_window_size: 1.0,
            bass_boost_gain: 0.0,
            bass_boost_fc: 105.0,
            bass_boost_q: 0.7,
            treble_boost_gain: 0.0,
            treble_boost_fc: 10000.0,
            treble_boost_q: 0.7,
            tilt: 0.0,
            fs: 48000,
            bit_depth: 16,
            phase: "minimum",
            f_res: 16,
            preamp: 0.0,
            max_gain: 12.0,
            max_slope: 18.0,
            window_size: 0.08,
            treble_window_size: 2.0,
            treble_f_lower: 6000.0,
            treble_f_upper: 8000.0,
            treble_gain_k: 1.0,
            graphic_eq: false,
            parametric_eq,
            fixed_band_eq: false,
            convolution_eq: false,
            response: EqualizeResponseSpec {
                fr_f_step: 1.02,
                fr_fields,
                base64fp16,
            },
        }
    }
}

#[derive(Deserialize)]
struct ApiFilter {
    #[serde(rename = "type")]
    kind: String,
    fc: f64,
    q: f64,
    gain: f64,
}

#[derive(Deserialize)]
struct ApiParametricEq {
    #[serde(default, deserialize_with = "null_as_empty")]
    filters: Vec<ApiFilter>,
    #[serde(default)]
    preamp: f64,
}

#[derive(Deserialize)]
struct BandsResult {
    parametric_eq: Option<ApiParametricEq>,
}

#[derive(Deserialize)]
struct FrResult {
    /// Kept raw: a malformed map is a decode error, not a reason to retry the request.
    #[serde(default)]
    fr: serde_json::Value,
}

// ── Client ───────────────────────────────────────────────────────────────────

/// An autoeq.app client with an on-disk cache in `<cache dir>/autoeq`.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    cache_dir: PathBuf,
}

impl Client {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            base_url: BASE_URL.to_owned(),
            cache_dir: rustet_core::appdir::cache_dir().join("autoeq"),
        }
    }

    async fn ensure_cache_dir(&self) -> Result<&Path, Error> {
        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .map_err(io_err(&self.cache_dir))?;
        Ok(&self.cache_dir)
    }

    /// The target curves. Cached for 24 hours in `targets.json`.
    pub async fn targets(&self, cancel: &CancellationToken) -> Result<Vec<TargetEntry>, Error> {
        let path = self.ensure_cache_dir().await?.join("targets.json");
        if let Some(cached) = read_fresh_list(&path).await {
            return Ok(cached);
        }
        let raw: Vec<RawTarget> = self.get_json(cancel, "/targets", "targets").await?;
        let targets: Vec<_> = raw
            .into_iter()
            .map(|t| TargetEntry {
                label: t.label,
                compatible: t
                    .compatible
                    .into_iter()
                    .map(|c| TargetCompat {
                        source: c.source,
                        form: c.form,
                        rig: c.rig.unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect();
        write_cache(&path, &targets).await;
        Ok(targets)
    }

    /// The headphone index, one [`Model`] per measurement. Cached for 24 hours in `index.json`.
    pub async fn index(&self, cancel: &CancellationToken) -> Result<Vec<Model>, Error> {
        let path = self.ensure_cache_dir().await?.join("index.json");
        if let Some(cached) = read_fresh_list(&path).await {
            return Ok(cached);
        }
        // {"headphone name": [{form, rig, source}, ...], ...}
        let raw: HashMap<String, Vec<RawVariant>> =
            self.get_json(cancel, "/entries", "index").await?;
        let models: Vec<_> = raw
            .into_iter()
            .flat_map(|(name, variants)| {
                variants.into_iter().map(move |v| Model {
                    name: name.clone(),
                    source: v.source,
                    rig: v.rig.unwrap_or_default(),
                    form: v.form,
                })
            })
            .collect();
        write_cache(&path, &models).await;
        tracing::info!(models = models.len(), "autoeq: fetched index");
        Ok(models)
    }

    /// The optimised parametric EQ for `model`. Cached per model and target in
    /// `<slug>_bands.json`. `None` or an empty target means [`default_target`] for the model's
    /// form; an incompatible target falls back to it too.
    pub async fn bands(
        &self,
        cancel: &CancellationToken,
        model: &Model,
        target: Option<&str>,
    ) -> Result<Settings, Error> {
        let dir = self.ensure_cache_dir().await?;
        let target = resolve_target(model, target);
        let cache = |t: &str| dir.join(cache_name(model, t, "bands"));
        let cached = |s: &Settings| !s.bands.is_empty();
        if let Some(s) = read_cache(&cache(target), cached).await {
            return Ok(s);
        }

        let (result, used): (BandsResult, _) = self
            .equalize(cancel, model, target, Want::ParametricEq)
            .await?;
        if used != target
            && let Some(s) = read_cache(&cache(used), cached).await
        {
            return Ok(s);
        }

        let peq = result
            .parametric_eq
            .filter(|p| !p.filters.is_empty())
            .ok_or_else(|| Error::NoParametricEq(model.name.clone()))?;
        let settings = Settings {
            preamp: peq.preamp,
            bands: peq
                .filters
                .into_iter()
                .map(|f| Band {
                    kind: match f.kind.as_str() {
                        "LOW_SHELF" => BandType::LowShelf,
                        "HIGH_SHELF" => BandType::HighShelf,
                        _ => BandType::Peaking,
                    },
                    freq: f.fc,
                    gain: f.gain,
                    q: f.q,
                    enabled: true,
                })
                .collect(),
        };
        write_cache(&cache(used), &settings).await;
        Ok(settings)
    }

    /// The measured and corrected frequency response of `model`. Cached per model and target
    /// in `<slug>_fr.json`. Targets resolve as in [`Client::bands`].
    pub async fn frequency_response(
        &self,
        cancel: &CancellationToken,
        model: &Model,
        target: Option<&str>,
    ) -> Result<FrData, Error> {
        let dir = self.ensure_cache_dir().await?;
        let target = resolve_target(model, target);
        let cache = |t: &str| dir.join(cache_name(model, t, "fr"));
        let cached = |d: &FrData| !d.original.is_empty();
        if let Some(d) = read_cache(&cache(target), cached).await {
            return Ok(d);
        }

        let (result, used): (FrResult, _) = self
            .equalize(cancel, model, target, Want::FrequencyResponse)
            .await?;
        if used != target
            && let Some(d) = read_cache(&cache(used), cached).await
        {
            return Ok(d);
        }

        let fr: HashMap<String, String> = serde_json::from_value(result.fr)
            .map_err(|e| Error::DecodeFr(format!("fr map: {e}")))?;
        let (Some(freq), Some(smoothed)) = (fr.get("frequency"), fr.get("smoothed")) else {
            return Err(Error::MissingFr(model.name.clone()));
        };
        let decode = |field: &str, b64: &str| {
            BASE64
                .decode(b64)
                .map_err(|e| Error::DecodeFr(format!("{field}: {e}")))
        };
        let freq = decode("frequency", freq)?;
        let original = decode_fr_curve(&freq, &decode("smoothed", smoothed)?)
            .map_err(|e| Error::DecodeFr(format!("original: {e}")))?;
        if original.is_empty() {
            return Err(Error::NoFrPoints(model.name.clone()));
        }
        // The corrected curve is optional: the API may leave it out.
        let corrected = fr
            .get("equalized_smoothed")
            .filter(|s| !s.is_empty())
            .and_then(|s| BASE64.decode(s).ok())
            .and_then(|eq| decode_fr_curve(&freq, &eq).ok())
            .unwrap_or_default();

        let data = FrData {
            original,
            corrected,
        };
        write_cache(&cache(used), &data).await;
        Ok(data)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        cancel: &CancellationToken,
        path: &str,
        what: &'static str,
    ) -> Result<T, Error> {
        let req = self
            .http
            .get(format!("{}{path}", self.base_url))
            .timeout(REQUEST_TIMEOUT);
        send_json(cancel, req, what).await
    }

    /// POSTs to `/equalize`. When that fails and `target` is not the default target for the
    /// model's form, it retries once with the default. Returns the target actually used.
    async fn equalize<'t, T: DeserializeOwned>(
        &self,
        cancel: &CancellationToken,
        model: &Model,
        target: &'t str,
        want: Want,
    ) -> Result<(T, &'t str), Error> {
        let post = |target: &str| {
            let req = self
                .http
                .post(format!("{}/equalize", self.base_url))
                .header("Origin", BASE_URL)
                .json(&EqualizeRequest::new(model, target, want))
                .timeout(REQUEST_TIMEOUT);
            send_json(cancel, req, "equalize")
        };
        let err = match post(target).await {
            Ok(r) => return Ok((r, target)),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => e,
        };
        let default = default_target(&model.form);
        if target == default {
            return Err(err);
        }
        tracing::warn!(target, model = %model.name, retry = default, error = %err,
            "autoeq: target failed, retrying with the default");
        Ok((post(default).await?, default))
    }
}

fn resolve_target<'a>(model: &Model, target: Option<&'a str>) -> &'a str {
    target
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| default_target(&model.form))
}

fn cache_name(model: &Model, target: &str, suffix: &str) -> String {
    format!(
        "{}_{suffix}.json",
        slugify(&format!("{}_{}_{target}", model.source, model.name))
    )
}

async fn send_json<T: DeserializeOwned>(
    cancel: &CancellationToken,
    req: reqwest::RequestBuilder,
    what: &'static str,
) -> Result<T, Error> {
    let http = |source| Error::Http { what, source };
    let fetch = async {
        let resp = req.send().await.map_err(http)?;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            return Err(Error::Status { what, status });
        }
        resp.json().await.map_err(http)
    };
    tokio::select! {
        () = cancel.cancelled() => Err(Error::Cancelled),
        r = fetch => r,
    }
}

/// A cached non-empty list younger than [`LIST_CACHE_TTL`].
async fn read_fresh_list<T: DeserializeOwned>(path: &Path) -> Option<Vec<T>> {
    let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    if age >= LIST_CACHE_TTL {
        return None;
    }
    read_cache(path, |v: &Vec<T>| !v.is_empty()).await
}

/// A cache entry that parses and passes `usable`. Anything else is a miss.
async fn read_cache<T: DeserializeOwned>(path: &Path, usable: impl Fn(&T) -> bool) -> Option<T> {
    let raw = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&raw).ok().filter(|v| usable(v))
}

/// Best effort: a cache that cannot be written only costs a refetch.
async fn write_cache<T: Serialize>(path: &Path, value: &T) {
    let raw = serde_json::to_vec(value).expect("cache entry serializes");
    if let Err(e) = tokio::fs::write(path, raw).await {
        tracing::debug!(path = %path.display(), error = %e, "autoeq: cache write failed");
    }
}

/// Pairs little-endian float16 frequencies and gains into points, keeping 20 Hz – 20 kHz.
fn decode_fr_curve(freq: &[u8], gain: &[u8]) -> Result<Vec<FrPoint>, String> {
    let n = freq.len() / 2;
    if n == 0 || gain.len() / 2 != n {
        return Err(format!(
            "mismatched lengths freq={} gain={}",
            freq.len(),
            gain.len()
        ));
    }
    let f16s = |b: &[u8]| {
        b.as_chunks::<2>()
            .0
            .iter()
            .map(|&c| f64::from(half::f16::from_le_bytes(c).to_f32()))
            .collect::<Vec<_>>()
    };
    Ok(f16s(freq)
        .into_iter()
        .zip(f16s(gain))
        .filter(|&(f, _)| (20.0..=20000.0).contains(&f))
        .map(|(freq, gain)| FrPoint { freq, gain })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn band(kind: BandType, freq: f64, gain: f64, q: f64, enabled: bool) -> Band {
        Band {
            kind,
            freq,
            gain,
            q,
            enabled,
        }
    }

    /// `to_apo` and `parse_apo` are the two halves of EqualizerAPO import/export: what one
    /// writes the other must read back (disabled bands are exported as OFF and not imported).
    #[test]
    fn apo_round_trip() {
        let input = Settings {
            preamp: -3.5,
            bands: vec![
                band(BandType::Peaking, 1000.0, 2.5, 1.41, true),
                band(BandType::LowShelf, 105.0, -4.0, 0.7, true),
                band(BandType::HighShelf, 8000.0, 1.5, 0.7, false),
                band(BandType::Peaking, 250.0, -1.2, 2.0, true),
            ],
        };
        let out = Settings::parse_apo(&input.to_apo()).unwrap();
        assert!((out.preamp - input.preamp).abs() <= 0.05, "{}", out.preamp);
        let want: Vec<_> = input.bands.iter().filter(|b| b.enabled).collect();
        assert_eq!(out.bands.len(), want.len(), "the OFF band must be dropped");
        for (b, w) in out.bands.iter().zip(want) {
            assert!(
                b.kind == w.kind
                    && (b.freq - w.freq).abs() <= 0.5
                    && (b.gain - w.gain).abs() <= 0.05
                    && (b.q - w.q).abs() <= 0.01
                    && b.enabled,
                "{b:?} != {w:?}"
            );
        }
    }

    #[test]
    fn parse_apo_rejects_text_without_filters() {
        assert!(matches!(
            Settings::parse_apo("Preamp: -2.0 dB\nnot a filter line\n"),
            Err(Error::NoBands)
        ));
    }

    /// The FFmpeg chain is the contract with the binaural renderer.
    #[test]
    fn ffmpeg_chain() {
        let s = Settings {
            preamp: -6.1,
            bands: vec![
                band(BandType::LowShelf, 105.0, 6.4, 0.7, true),
                band(BandType::Peaking, 8800.4, 5.1, 1.42, true),
                band(BandType::HighShelf, 10000.0, 2.0, 0.7, false),
            ],
        };
        assert_eq!(
            s.ffmpeg_chain(),
            "volume=-6.10dB,lowshelf=f=105:t=q:w=0.7000:g=6.40,equalizer=f=8800:t=q:w=1.4200:g=5.10"
        );
        assert_eq!(Settings::default().ffmpeg_chain(), "");
    }

    /// Cache file names must match what the Go engine wrote.
    #[test]
    fn slugify_matches_go() {
        assert_eq!(
            slugify("oratory1990_Sennheiser HD 600_Harman over-ear 2018"),
            "oratory1990_sennheiser_hd_600_harman_over_ear_2018"
        );
        assert_eq!(slugify("İé"), "i_");
    }

    /// Go wrote empty settings with `"bands": null`.
    #[tokio::test]
    async fn settings_save_load_and_go_nulls() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Settings::load_from(dir.path()).await.unwrap(),
            Settings::default()
        );
        std::fs::write(
            dir.path().join("eq.json"),
            r#"{"preamp": -1, "bands": null}"#,
        )
        .unwrap();
        assert_eq!(Settings::load_from(dir.path()).await.unwrap().preamp, -1.0);
        let s = Settings {
            preamp: -2.0,
            bands: vec![band(BandType::Peaking, 100.0, 1.0, 1.0, true)],
        };
        s.save_to(dir.path()).await.unwrap();
        assert_eq!(Settings::load_from(dir.path()).await.unwrap(), s);
    }

    #[test]
    fn decode_fr_curve_filters_audible_range() {
        let le = |v: &[f32]| -> Vec<u8> {
            v.iter()
                .flat_map(|&x| half::f16::from_f32(x).to_le_bytes())
                .collect()
        };
        let pts = decode_fr_curve(
            &le(&[10.0, 20.0, 1000.0, 30000.0]),
            &le(&[1.0, 2.0, -3.5, 4.0]),
        )
        .unwrap();
        assert_eq!(
            pts,
            [
                FrPoint {
                    freq: 20.0,
                    gain: 2.0
                },
                FrPoint {
                    freq: 1000.0,
                    gain: -3.5
                }
            ]
        );
        assert!(decode_fr_curve(&le(&[1.0]), &le(&[1.0, 2.0])).is_err());
        assert!(decode_fr_curve(&[], &[]).is_err());
    }

    fn client(server: &MockServer, cache: &Path) -> Client {
        Client {
            http: reqwest::Client::new(),
            base_url: server.uri(),
            cache_dir: cache.to_owned(),
        }
    }

    fn model() -> Model {
        Model {
            name: "HD 600".into(),
            source: "oratory1990".into(),
            rig: String::new(),
            form: "over-ear".into(),
        }
    }

    #[tokio::test]
    async fn index_flattens_variants_and_caches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/entries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "HD 600": [{"form": "over-ear", "rig": null, "source": "oratory1990"},
                           {"form": "over-ear", "rig": "GRAS", "source": "crinacle"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let c = client(&server, dir.path());
        let cancel = CancellationToken::new();
        let mut models = c.index(&cancel).await.unwrap();
        models.sort_by(|a, b| a.source.cmp(&b.source));
        assert_eq!(models[0].rig, "GRAS");
        assert_eq!(models[1], model());
        // The second call is served from the cache (the mock expects one request).
        assert_eq!(c.index(&cancel).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bands_retry_with_default_target_and_cache_under_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/equalize"))
            .and(header("origin", "https://autoeq.app"))
            .and(body_partial_json(serde_json::json!({"target": "Bogus"})))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/equalize"))
            .and(body_partial_json(serde_json::json!({
                "target": "Harman over-ear 2018", "parametric_eq": true, "sound_signature": null
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "fr": {},
                "parametric_eq": {"fs": 48000, "preamp": -5.2, "filters": [
                    {"type": "LOW_SHELF", "fc": 105.0, "q": 0.7, "gain": 5.0},
                    {"type": "PEAKING", "fc": 2000.0, "q": 1.2, "gain": -2.0}
                ]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let c = client(&server, dir.path());
        let s = c
            .bands(&CancellationToken::new(), &model(), Some("Bogus"))
            .await
            .unwrap();
        assert_eq!(s.preamp, -5.2);
        assert_eq!(s.bands[0].kind, BandType::LowShelf);
        assert_eq!(s.bands[1].kind, BandType::Peaking);
        assert!(
            dir.path()
                .join("oratory1990_hd_600_harman_over_ear_2018_bands.json")
                .exists()
        );
        // No target: the default, now cached.
        let again = c
            .bands(&CancellationToken::new(), &model(), None)
            .await
            .unwrap();
        assert_eq!(again, s);
    }

    #[tokio::test]
    async fn cancelled_request() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = client(&server, dir.path())
            .targets(&cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err:?}");
    }

    #[tokio::test]
    #[ignore = "needs the network (autoeq.app)"]
    async fn live_targets_and_index() {
        let dir = tempfile::tempdir().unwrap();
        // Not Client::new: resolving the app cache dir would touch the user's ~/.cache.
        let c = Client {
            http: reqwest::Client::new(),
            base_url: BASE_URL.to_owned(),
            cache_dir: dir.path().to_owned(),
        };
        let cancel = CancellationToken::new();
        assert!(!c.targets(&cancel).await.unwrap().is_empty());
        let models = c.index(&cancel).await.unwrap();
        let m = models
            .iter()
            .find(|m| m.name == "Sennheiser HD 600" && m.source == "oratory1990")
            .unwrap();
        assert!(!c.bands(&cancel, m, None).await.unwrap().bands.is_empty());
        assert!(
            !c.frequency_response(&cancel, m, None)
                .await
                .unwrap()
                .original
                .is_empty()
        );
    }
}
