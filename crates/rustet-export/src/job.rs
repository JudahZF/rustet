//! Export jobs: the request, the job state machine and the JSON the renderer reads.
//!
//! Field names are a contract with the renderer. Requests are decoded with the Go engine's
//! leniency: the renderer sends PascalCase keys (`AssetID`, `Options.EmbedArtwork`), which Go's
//! case-insensitive decoder accepted, so every field also takes that spelling; `null` means
//! "not set" (see [`ExportRequest::from_json`]).

use rustet_ampapi::lyrics::LyricsFormat;
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use crate::fs::OverwritePolicy;

/// The stage of an export job. A job only moves forward through the non-terminal phases, and
/// never leaves a terminal one: retrying a failed or cancelled export enqueues a new job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Queued,
    /// Fetching catalog metadata.
    Resolving,
    /// Streaming and decrypting the media.
    Downloading,
    /// Embedding metadata, artwork and lyrics.
    Tagging,
    /// Temp file → final path.
    Moving,
    Done,
    Failed,
    Cancelled,
}

impl Phase {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::Tagging => "tagging",
            Self::Moving => "moving",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// True for a final state.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    /// The order of the non-terminal phases.
    const fn rank(self) -> Option<u8> {
        match self {
            Self::Queued => Some(0),
            Self::Resolving => Some(1),
            Self::Downloading => Some(2),
            Self::Tagging => Some(3),
            Self::Moving => Some(4),
            Self::Done | Self::Failed | Self::Cancelled => None,
        }
    }

    /// The transition relation. Nothing leaves a terminal state; any other phase may end in a
    /// terminal one (failure and cancellation strike anywhere, and skipped or collection jobs
    /// finish without visiting every stage); otherwise the move stays in place (a progress
    /// update) or goes forward.
    pub fn can_transition(self, to: Self) -> bool {
        match (self.rank(), to.rank()) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(from), Some(to)) => to >= from,
        }
    }
}

/// Where an export's bytes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The committed playback disk cache.
    Cache,
    /// The tail of an in-progress playback download.
    Playback,
    /// A fresh export stream.
    Network,
}

/// The content variant to export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportCapabilities {
    /// ALAC.
    #[serde(alias = "Lossless")]
    pub lossless: bool,
    /// Dolby Atmos.
    #[serde(alias = "Atmos")]
    pub atmos: bool,
    /// Music video.
    #[serde(alias = "Video")]
    pub video: bool,
    /// Expand a playlist into per-track jobs.
    #[serde(alias = "Playlist")]
    pub playlist: bool,
    /// With `playlist`: a library playlist (`p.` IDs), read through the library API.
    #[serde(alias = "LibraryPlaylist")]
    pub library_playlist: bool,
}

/// Post-processing of the downloaded file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportOptions {
    /// Download and embed the cover art.
    #[serde(alias = "EmbedArtwork")]
    pub embed_artwork: bool,
    /// Square size requested from the Apple CDN; 0 or less selects 3000.
    #[serde(alias = "ArtworkSize")]
    pub artwork_size: i64,
    /// Fetch lyrics and embed them.
    #[serde(alias = "EmbedLyrics")]
    pub embed_lyrics: bool,
    /// `"lrc"` (default), `"ttml"`, `"srt"` or `"vtt"`; also the sidecar extension.
    #[serde(alias = "LrcFormat", deserialize_with = "lyrics_format")]
    pub lrc_format: LyricsFormat,
    /// `"lyrics"` (default) or `"syllable-lyrics"`.
    #[serde(alias = "LrcType")]
    pub lrc_type: String,
    /// Write the lyrics next to the audio file.
    #[serde(alias = "SaveLrcSidecar")]
    pub save_lrc_sidecar: bool,
    #[serde(alias = "OverwritePolicy")]
    pub overwrite_policy: OverwritePolicy,
    /// Convert ALAC to FLAC after tagging. VLC decodes (it handles Apple's fMP4 with identical
    /// tfdts); FFmpeg encodes and tags. The `.m4a` is kept only with `keep_original`.
    #[serde(rename = "convertToFlac", alias = "ConvertToFLAC")]
    pub convert_to_flac: bool,
    /// Empty: `cvlc`, then `vlc`, from PATH.
    #[serde(alias = "VLCPath")]
    pub vlc_path: String,
    /// Empty: `ffmpeg` from PATH.
    #[serde(rename = "ffmpegPath", alias = "FFmpegPath")]
    pub ffmpeg_path: String,
    #[serde(alias = "KeepOriginal")]
    pub keep_original: bool,
    /// Markers for `{tag}` in the filename template; empty disables one.
    #[serde(alias = "ExplicitChoice")]
    pub explicit_choice: String,
    #[serde(alias = "CleanChoice")]
    pub clean_choice: String,
    #[serde(alias = "MasterChoice")]
    pub master_choice: String,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            embed_artwork: false,
            artwork_size: 0,
            embed_lyrics: false,
            lrc_format: LyricsFormat::Lrc,
            lrc_type: String::new(),
            save_lrc_sidecar: false,
            overwrite_policy: OverwritePolicy::Skip,
            convert_to_flac: false,
            vlc_path: String::new(),
            ffmpeg_path: String::new(),
            keep_original: false,
            explicit_choice: String::new(),
            clean_choice: String::new(),
            master_choice: String::new(),
        }
    }
}

fn lyrics_format<'de, D: Deserializer<'de>>(d: D) -> Result<LyricsFormat, D::Error> {
    Ok(LyricsFormat::from(String::deserialize(d)?.as_str()))
}

/// The file extension of a lyrics format, for the sidecar.
pub(crate) const fn lyrics_ext(f: LyricsFormat) -> &'static str {
    match f {
        LyricsFormat::Ttml => "ttml",
        LyricsFormat::Vtt => "vtt",
        LyricsFormat::Srt => "srt",
        LyricsFormat::Lrc => "lrc",
    }
}

/// One export, as `POST /api/v1/export` receives it. Holds tokens, so it has no `Debug`.
#[derive(Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportRequest {
    #[serde(alias = "AssetID")]
    pub asset_id: String,
    #[serde(alias = "Storefront")]
    pub storefront: String,
    /// Developer token.
    #[serde(alias = "Token")]
    pub token: String,
    /// `x-apple-music-user-token`.
    #[serde(rename = "mut", alias = "MUT")]
    pub mut_token: String,
    #[serde(alias = "Language")]
    pub language: String,
    #[serde(alias = "Capabilities")]
    pub capabilities: ExportCapabilities,
    /// Highest music-video height; 0 lets the provider choose (1080p).
    #[serde(rename = "mvMaxHeight", alias = "MVMaxHeight")]
    pub mv_max_height: i64,
    /// Empty: `~/Music/Rustet-Downloads`.
    #[serde(alias = "OutputDir")]
    pub output_dir: String,
    /// Path template relative to `output_dir`; see [`crate::template`]. Empty: the default.
    #[serde(alias = "FilenameTemplate")]
    pub filename_template: String,
    #[serde(alias = "Options")]
    pub options: ExportOptions,
    /// Shown in the job row until the catalog resolves.
    #[serde(alias = "HintTitle")]
    pub hint_title: String,
    #[serde(alias = "HintArtist")]
    pub hint_artist: String,
    #[serde(alias = "HintArtwork")]
    pub hint_artwork: String,
    /// Higher runs sooner, FIFO among equals. Tracks expanded from an album or playlist inherit
    /// it.
    #[serde(alias = "Priority")]
    pub priority: i32,
}

impl ExportRequest {
    /// Decodes a request body. `null` members are treated as absent, as Go's decoder did, so a
    /// `null` from the renderer (`parseInt` of nothing) does not reject the request.
    pub fn from_json(body: &[u8]) -> serde_json::Result<Self> {
        let mut value: serde_json::Value = serde_json::from_slice(body)?;
        strip_nulls(&mut value);
        serde_json::from_value(value)
    }
}

fn strip_nulls(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            map.values_mut().for_each(strip_nulls);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// Emitted on every significant state change and forwarded to SSE clients as the `export` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportEvent {
    pub job_id: String,
    pub asset_id: String,
    pub phase: Phase,
    /// 0–100.
    pub percent: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// The public view of a job, safe to serialise to API clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportJob {
    #[serde(rename = "jobId")]
    pub id: String,
    pub asset_id: String,
    pub phase: Phase,
    pub percent: u32,
    /// Monotonically increasing enqueue order.
    pub queue_pos: i64,
    /// 1-based position in run order while queued; 0 once running or finished.
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub queue_index: usize,
    pub priority: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artist_name: String,
    #[serde(rename = "artworkUrl", skip_serializing_if = "String::is_empty")]
    pub artwork_url: String,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub bytes_done: u64,
    /// An estimate from the duration and a nominal bit rate.
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub bytes_total: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// `None` until downloading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// The export bandwidth limit (bytes/s) when the snapshot was taken; `null` is unlimited.
    pub limit_bps: Option<i64>,
    /// `limit_bps` is set.
    pub throttled: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}

/// Job timestamps are UTC: the local offset cannot be read safely in a multi-threaded process.
pub(crate) fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

impl ExportJob {
    /// Moves to `phase` with `percent`, if the transition relation allows it: a late update from
    /// a task that lost a race (cancel, failure) must not revive or rewind a job.
    pub(crate) fn advance(&mut self, phase: Phase, percent: u32) -> bool {
        if !self.phase.can_transition(phase) {
            return false;
        }
        self.phase = phase;
        self.percent = percent;
        self.updated_at = now();
        true
    }

    /// Ends the job with `error`: cancelled when its token fired, else failed. A finished job
    /// keeps its outcome.
    pub(crate) fn fail(&mut self, error: String, cancelled: bool) -> bool {
        if self.phase.is_terminal() {
            return false;
        }
        self.phase = if cancelled {
            Phase::Cancelled
        } else {
            Phase::Failed
        };
        self.error = error;
        self.updated_at = now();
        true
    }

    /// The event for the current state.
    pub(crate) fn event(&self, percent: u32, error: &str) -> ExportEvent {
        ExportEvent {
            job_id: self.id.clone(),
            asset_id: self.asset_id.clone(),
            phase: self.phase,
            percent,
            output: self.output.clone(),
            error: error.to_owned(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    const ALL: [Phase; 8] = [
        Phase::Queued,
        Phase::Resolving,
        Phase::Downloading,
        Phase::Tagging,
        Phase::Moving,
        Phase::Done,
        Phase::Failed,
        Phase::Cancelled,
    ];

    pub(crate) fn job() -> ExportJob {
        ExportJob {
            id: "j".into(),
            asset_id: "1".into(),
            phase: Phase::Queued,
            percent: 0,
            queue_pos: 1,
            queue_index: 0,
            priority: 0,
            title: String::new(),
            artist_name: String::new(),
            artwork_url: String::new(),
            bytes_done: 0,
            bytes_total: 0,
            output: String::new(),
            error: String::new(),
            source: None,
            limit_bps: None,
            throttled: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn transition_table() {
        for from in ALL {
            for to in ALL {
                let want = if from.is_terminal() {
                    false
                } else if to.is_terminal() {
                    true
                } else {
                    to.rank() >= from.rank()
                };
                assert_eq!(from.can_transition(to), want, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn terminal_states_are_absorbing() {
        for end in [Phase::Done, Phase::Failed, Phase::Cancelled] {
            let mut j = job();
            j.phase = Phase::Downloading;
            match end {
                Phase::Done => assert!(j.advance(Phase::Done, 100)),
                Phase::Failed => assert!(j.fail("boom".into(), false)),
                _ => assert!(j.fail("cancelled".into(), true)),
            }
            assert_eq!(j.phase, end);
            assert!(!j.advance(Phase::Downloading, 10));
            assert!(!j.advance(Phase::Done, 100));
            assert!(!j.fail("late error".into(), false));
            assert_eq!(j.phase, end, "{end:?} was left");
        }
    }

    // Whatever order racing tasks call advance and fail in, the phases a job passes through form
    // a path in the transition relation, and a terminal state is the last one.
    #[test]
    fn random_event_sequences_follow_the_relation() {
        let mut seed: u64 = 1;
        let mut rand = |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        for _ in 0..2000 {
            let mut j = job();
            let mut prev = j.phase;
            for _ in 0..12 {
                if rand(5) == 0 {
                    j.fail("x".into(), false);
                } else {
                    j.advance(ALL[rand(8) as usize], rand(101) as u32);
                }
                assert!(
                    j.phase == prev || prev.can_transition(j.phase),
                    "{prev:?} -> {:?}",
                    j.phase
                );
                assert!(!prev.is_terminal() || j.phase == prev);
                prev = j.phase;
            }
        }
    }

    #[test]
    fn job_json_field_names() {
        let mut j = job();
        assert_eq!(
            serde_json::to_value(&j).unwrap(),
            json!({
                "jobId": "j", "assetId": "1", "phase": "queued", "percent": 0, "queuePos": 1,
                "priority": 0, "limitBps": null, "throttled": false,
                "createdAt": "1970-01-01T00:00:00Z", "updatedAt": "1970-01-01T00:00:00Z"
            })
        );
        j.phase = Phase::Downloading;
        j.queue_index = 2;
        j.title = "T".into();
        j.artist_name = "A".into();
        j.artwork_url = "u".into();
        j.bytes_done = 5;
        j.bytes_total = 9;
        j.output = "/o".into();
        j.error = "e".into();
        j.source = Some(Source::Network);
        j.limit_bps = Some(1 << 20);
        j.throttled = true;
        let v = serde_json::to_value(&j).unwrap();
        for (k, want) in [
            ("phase", json!("downloading")),
            ("queueIndex", json!(2)),
            ("title", json!("T")),
            ("artistName", json!("A")),
            ("artworkUrl", json!("u")),
            ("bytesDone", json!(5)),
            ("bytesTotal", json!(9)),
            ("output", json!("/o")),
            ("error", json!("e")),
            ("source", json!("network")),
            ("limitBps", json!(1 << 20)),
            ("throttled", json!(true)),
        ] {
            assert_eq!(v[k], want, "{k}");
        }
        for (phase, name) in ALL.map(|p| (p, p.as_str())) {
            assert_eq!(serde_json::to_value(phase).unwrap(), json!(name));
        }
        for (s, name) in [(Source::Cache, "cache"), (Source::Playback, "playback")] {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(name));
        }
    }

    #[test]
    fn event_json_field_names() {
        let mut j = job();
        assert_eq!(
            serde_json::to_value(j.event(0, "")).unwrap(),
            json!({"jobId": "j", "assetId": "1", "phase": "queued", "percent": 0})
        );
        j.output = "/o".into();
        assert_eq!(
            serde_json::to_value(j.event(7, "bad")).unwrap(),
            json!({"jobId": "j", "assetId": "1", "phase": "queued", "percent": 7, "output": "/o", "error": "bad"})
        );
    }

    /// The body the renderer posts (`startDownload` in engine-playback.js).
    #[test]
    fn renderer_request_decodes() {
        let body = json!({
            "AssetID": "123", "Storefront": "gb", "Token": "tok", "MUT": "mut",
            "Language": "en-GB",
            "Capabilities": {"Lossless": true, "Atmos": false, "Video": false, "Playlist": true, "LibraryPlaylist": true},
            "MVMaxHeight": 720, "OutputDir": "/music", "FilenameTemplate": "{album}/{title}",
            "Options": {
                "EmbedArtwork": true, "ArtworkSize": null, "EmbedLyrics": true, "LrcType": "syllable-lyrics",
                "LrcFormat": "srt", "SaveLrcSidecar": true, "OverwritePolicy": "rename",
                "ConvertToFLAC": true, "FFmpegPath": "/usr/bin/ffmpeg", "KeepOriginal": true,
                "ExplicitChoice": "[E]", "CleanChoice": "", "MasterChoice": "[M]"
            },
            "HintTitle": "T", "HintArtist": "A", "HintArtwork": "W"
        });
        let r = ExportRequest::from_json(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            (
                r.asset_id.as_str(),
                r.storefront.as_str(),
                r.token.as_str(),
                r.mut_token.as_str()
            ),
            ("123", "gb", "tok", "mut")
        );
        assert_eq!(r.language, "en-GB");
        assert!(
            r.capabilities.lossless && r.capabilities.playlist && r.capabilities.library_playlist
        );
        assert_eq!(r.mv_max_height, 720);
        assert_eq!(
            (r.output_dir.as_str(), r.filename_template.as_str()),
            ("/music", "{album}/{title}")
        );
        let o = &r.options;
        assert!(
            o.embed_artwork
                && o.embed_lyrics
                && o.save_lrc_sidecar
                && o.convert_to_flac
                && o.keep_original
        );
        assert_eq!(o.artwork_size, 0, "null is absent");
        assert_eq!(o.lrc_type, "syllable-lyrics");
        assert_eq!(o.lrc_format, LyricsFormat::Srt);
        assert_eq!(o.overwrite_policy, OverwritePolicy::Rename);
        assert_eq!(o.ffmpeg_path, "/usr/bin/ffmpeg");
        assert_eq!(
            (o.explicit_choice.as_str(), o.master_choice.as_str()),
            ("[E]", "[M]")
        );
        assert_eq!(
            (
                r.hint_title.as_str(),
                r.hint_artist.as_str(),
                r.hint_artwork.as_str()
            ),
            ("T", "A", "W")
        );
    }

    #[test]
    fn camel_case_request_decodes() {
        let body = json!({
            "assetId": "1", "mut": "m", "mvMaxHeight": 1080, "priority": 5,
            "capabilities": {"video": true},
            "options": {"convertToFlac": true, "ffmpegPath": "ffmpeg", "vlcPath": "cvlc",
                        "overwritePolicy": "bogus", "lrcFormat": "../../etc/x"}
        });
        let r = ExportRequest::from_json(body.to_string().as_bytes()).unwrap();
        assert_eq!(
            (
                r.asset_id.as_str(),
                r.mut_token.as_str(),
                r.mv_max_height,
                r.priority
            ),
            ("1", "m", 1080, 5)
        );
        assert!(r.capabilities.video && r.options.convert_to_flac);
        assert_eq!(
            (r.options.ffmpeg_path.as_str(), r.options.vlc_path.as_str()),
            ("ffmpeg", "cvlc")
        );
        assert_eq!(
            r.options.overwrite_policy,
            OverwritePolicy::Skip,
            "unknown policy is skip"
        );
        assert_eq!(
            lyrics_ext(r.options.lrc_format),
            "lrc",
            "a format never reaches the sidecar path"
        );
        assert!(ExportRequest::from_json(b"not json").is_err());
        assert!(ExportRequest::from_json(br#"{"priority":"high"}"#).is_err());
    }
}
