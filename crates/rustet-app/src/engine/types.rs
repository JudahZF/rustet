//! The JSON of the engine routes the UI calls, other than the player's (those are
//! `rustet_player::model`). Field names follow the engine's handlers; every reply type reads
//! leniently (`default` everywhere, unknown enum names become `Unknown`), so a newer engine
//! never breaks an older UI.

use std::collections::BTreeMap;

use rustet_player::model::PlayerState;
use serde::{Deserialize, Serialize};

use crate::settings::Downloads;

// ── Auth ─────────────────────────────────────────────────────────────────────

/// `GET /api/v1/auth`: which tokens the engine holds. No token is ever returned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AuthStatus {
    /// A web music user token was pushed.
    pub web_token: bool,
    /// The FairPlay session has a music token.
    pub fairplay_token: bool,
    /// The pushed storefront, else the FairPlay session's; empty when neither is known.
    pub storefront: String,
    pub language: String,
}

/// `GET /api/v1/auth/session`: the account's music user token and storefront. Holds a token,
/// so its `Debug` shows only the storefront.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSession {
    pub music_user_token: String,
    /// The account's storefront code (`gb`); empty when the engine does not know it.
    #[serde(default)]
    pub storefront: String,
}

impl std::fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSession")
            .field("storefront", &self.storefront)
            .finish_non_exhaustive()
    }
}

/// `POST /api/v1/library/token`: the web session's token for the engine. Holds a token, so it
/// has no `Debug`.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPush<'a> {
    pub music_user_token: &'a str,
    /// Empty: the engine keeps its own scraped developer token.
    pub developer_token: &'a str,
    pub storefront: &'a str,
    /// The UI language (`en-GB`); empty: the engine uses its default.
    pub language: &'a str,
}

// ── DRM (the lossless account) ───────────────────────────────────────────────

/// `GET /api/v1/drm/status`, the `drm` event and the sign-in answers: the DRM snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DrmStatus {
    pub state: DrmDimensions,
    pub capabilities: DrmCapabilities,
    /// Set while `state.authentication` is `Challenging`.
    pub challenge: Option<Challenge>,
    /// Why the last sign-in failed; set while `state.authentication` is `Failed`.
    pub failure: Option<SignInFailure>,
    /// RFC 3339; `0001-01-01T00:00:00Z` before the first change.
    pub timestamp: String,
    /// A log line. Never parsed for state.
    pub message: String,
    /// `/status` only: false when the build has no DRM layer (lossless can never be ready).
    /// Events do not carry it; they come only from an engine that has the layer.
    #[serde(default = "yes")]
    pub available: bool,
    /// `/status` only: when FairPlay became ready (Unix ms) and the session's age.
    pub ready_since_ms: Option<i64>,
    pub session_age_ms: Option<i64>,
    #[serde(rename = "sessionTTLMs")]
    pub session_ttl_ms: Option<i64>,
}

fn yes() -> bool {
    true
}

impl DrmStatus {
    /// Lossless sign-in finished: FairPlay is ready (`promptAction`'s `close` rule).
    pub fn signed_in(&self) -> bool {
        self.state.fairplay == FairPlayState::Ready
            || self.capabilities.cbcs
            || self.state.authentication == AuthState::LoggedIn
    }
}

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $wire:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant,)+
            #[default]
            #[serde(other)]
            Unknown,
        }
    };
}

wire_enum! {
    ManagerState { Disabled => "disabled", Initializing => "initializing", Ready => "ready",
        ShuttingDown => "shutting_down", Failed => "failed", }
}
wire_enum! {
    ProcessState { Stopped => "stopped", Starting => "starting", Running => "running",
        Failed => "failed", }
}
wire_enum! {
    /// Where the FairPlay sign-in is.
    AuthState { LoggedOut => "logged_out", LoggingIn => "logging_in", Challenging => "challenging",
        LoggedIn => "logged_in", Failed => "failed", }
}
wire_enum! {
    FairPlayState { Initializing => "initializing", Ready => "ready", Failed => "failed", }
}
wire_enum! {
    SessionState { Empty => "empty", Valid => "valid", Expired => "expired", }
}
wire_enum! {
    RecoveryState { Idle => "idle", Scheduled => "scheduled", Refreshing => "refreshing",
        Failed => "failed", }
}
wire_enum! {
    /// What Apple asked for during a sign-in.
    ChallengeKind { Credentials => "credentials", TwoFactor => "two_factor",
        DeviceApproval => "device_approval", DialogChoice => "dialog_choice", }
}
wire_enum! {
    FailureKind { InvalidCredentials => "invalid_credentials", Locked => "locked", Terms => "terms",
        NotStoreAccount => "not_store_account", Timeout => "timeout", Setup => "setup", }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DrmDimensions {
    pub manager: ManagerState,
    pub process: ProcessState,
    pub authentication: AuthState,
    pub fairplay: FairPlayState,
    pub session: SessionState,
    pub recovery: RecoveryState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DrmCapabilities {
    pub cbcs: bool,
    pub alac: bool,
    pub atmos: bool,
    pub hi_res: bool,
}

/// A prompt Apple raised during the sign-in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Challenge {
    /// Numbers each announced challenge, so Apple asking again (a wrong code) differs from the
    /// same challenge repeated in a later snapshot. 0: not numbered.
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: ChallengeKind,
    pub title: String,
    pub description: String,
    /// For a dialog choice: `buttons`, the labels separated by newlines.
    pub metadata: BTreeMap<String, String>,
}

impl Challenge {
    /// The labels of a dialog choice, in reply-index order.
    pub fn options(&self) -> Vec<String> {
        self.metadata
            .get("buttons")
            .map(|b| {
                b.split('\n')
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Why a sign-in did not complete, worded for the user, with the place to fix it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SignInFailure {
    pub kind: FailureKind,
    pub message: String,
    pub url: Option<String>,
}

// ── Metadata and lyrics ──────────────────────────────────────────────────────

/// `GET /api/v1/metadata/{id}`: track info and the qualities the catalog lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackMetadata {
    pub id: String,
    /// `song`, `mv` or `album`.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub artist_name: String,
    pub album_name: String,
    pub duration_ms: u64,
    /// 500 px.
    pub artwork_url: String,
    pub has_lyrics: bool,
    #[serde(rename = "has4k")]
    pub has_4k: bool,
    pub has_hdr: bool,
    /// Best first, AAC last; `None` for albums.
    pub available_streams: Option<Vec<StreamInfo>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StreamInfo {
    /// `ALAC`, `AAC`, `Dolby Atmos`, `H.264`, …
    pub codec: String,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub bitrate: u32,
}

/// The format `GET /api/v1/lyrics/{id}` answers in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LyricsFormat {
    /// Line-synced LRC.
    #[default]
    Lrc,
    /// Apple's TTML (syllable timing, translations).
    Ttml,
    Vtt,
    Srt,
}

impl LyricsFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lrc => "lrc",
            Self::Ttml => "ttml",
            Self::Vtt => "vtt",
            Self::Srt => "srt",
        }
    }
}

/// Which lyrics: line-timed or syllable-timed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LyricsKind {
    #[default]
    Lines,
    Syllables,
}

impl LyricsKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lines => "lyrics",
            Self::Syllables => "syllable-lyrics",
        }
    }
}

// ── Exports ──────────────────────────────────────────────────────────────────

/// What to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Song,
    MusicVideo,
    Album,
    Playlist,
    /// A playlist of the user's library (`p.` ID).
    LibraryPlaylist,
}

/// `POST /api/v1/export`. Without tokens: the engine uses the session's own.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub asset_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub storefront: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub language: String,
    pub capabilities: ExportCapabilities,
    #[serde(rename = "mvMaxHeight")]
    pub mv_max_height: u32,
    /// Empty: `~/Music/Rustet-Downloads`.
    pub output_dir: String,
    /// `<album folder>/<track file>`, relative to `output_dir`.
    pub filename_template: String,
    pub options: ExportOptions,
    /// Shown in the job row until the catalog resolves.
    pub hint_title: String,
    pub hint_artist: String,
    pub hint_artwork: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportCapabilities {
    pub lossless: bool,
    pub atmos: bool,
    pub video: bool,
    pub playlist: bool,
    pub library_playlist: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportOptions {
    pub embed_artwork: bool,
    pub artwork_size: u32,
    pub embed_lyrics: bool,
    pub lrc_format: &'static str,
    pub lrc_type: &'static str,
    pub save_lrc_sidecar: bool,
    pub overwrite_policy: &'static str,
    #[serde(rename = "convertToFlac")]
    pub convert_to_flac: bool,
    #[serde(rename = "ffmpegPath")]
    pub ffmpeg_path: String,
    pub keep_original: bool,
    pub explicit_choice: String,
    pub clean_choice: String,
    pub master_choice: String,
}

impl ExportRequest {
    /// An export with the user's Downloads settings. `lossless_allowed`: the lossless account is
    /// signed in and lossless is on (the settings panel greyed the choice out otherwise).
    pub fn new(asset_id: &str, kind: ExportKind, d: &Downloads, lossless_allowed: bool) -> Self {
        use rustet_player::model::StreamingQuality;
        Self {
            asset_id: asset_id.to_owned(),
            capabilities: ExportCapabilities {
                lossless: lossless_allowed && d.quality != StreamingQuality::HighQuality,
                atmos: false,
                video: kind == ExportKind::MusicVideo,
                playlist: matches!(kind, ExportKind::Playlist | ExportKind::LibraryPlaylist),
                library_playlist: kind == ExportKind::LibraryPlaylist,
            },
            mv_max_height: d.mv_quality.max_height(),
            output_dir: d.folder.clone(),
            filename_template: format!("{}/{}", d.album_template, d.track_template),
            options: ExportOptions {
                embed_artwork: d.embed_artwork,
                artwork_size: d.artwork_size.pixels(),
                embed_lyrics: d.embed_lyrics,
                lrc_format: LyricsFormat::Lrc.as_str(),
                lrc_type: LyricsKind::Lines.as_str(),
                save_lrc_sidecar: false,
                overwrite_policy: "skip",
                convert_to_flac: d.ffmpeg_enabled,
                ffmpeg_path: d.ffmpeg_path.clone(),
                keep_original: false,
                explicit_choice: d.explicit_marker.choice().to_owned(),
                clean_choice: d.clean_marker.choice().to_owned(),
                master_choice: d.master_marker.choice().to_owned(),
            },
            ..Self::default()
        }
    }
}

wire_enum! {
    /// The stage of an export job. Done, failed and cancelled are final.
    ExportPhase { Queued => "queued", Resolving => "resolving", Downloading => "downloading",
        Tagging => "tagging", Moving => "moving", Done => "done", Failed => "failed",
        Cancelled => "cancelled", }
}

impl ExportPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// One export job (`GET /api/v1/export`, `…/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportJob {
    pub job_id: String,
    pub asset_id: String,
    pub phase: ExportPhase,
    /// 0–100.
    pub percent: u32,
    pub queue_pos: i64,
    /// 1-based run order while queued; 0 once running or finished.
    pub queue_index: usize,
    pub priority: i32,
    pub title: String,
    pub artist_name: String,
    pub artwork_url: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub output: String,
    pub error: String,
    /// `cache`, `playback` or `network`, once downloading.
    pub source: Option<String>,
    /// Bandwidth limit in bytes/s; `None` is unlimited.
    pub limit_bps: Option<i64>,
    pub throttled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// The `export` event: a job's progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ExportProgress {
    pub job_id: String,
    pub asset_id: String,
    pub phase: ExportPhase,
    pub percent: u32,
    pub output: String,
    pub error: String,
}

// ── Cache and storage ────────────────────────────────────────────────────────

/// `GET /api/v1/cache/stats`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CacheStats {
    pub persistent: PersistentCache,
    pub prewarm: PrewarmCache,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PersistentCache {
    pub available: bool,
    pub size_bytes: u64,
    /// The limit, 500 MB when none was set.
    pub limit_bytes: u64,
    /// 0: songs never expire.
    pub ttl_days: u32,
    pub count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrewarmCache {
    pub entries: u64,
    pub size_bytes: u64,
    pub limit_bytes: u64,
}

/// What `DELETE /api/v1/cache/playback` clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheScope {
    All,
    Prewarm,
    /// The song cache on disk.
    Persistent,
    Segments,
}

impl CacheScope {
    pub(crate) const fn query(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Prewarm => "?what=prewarm",
            Self::Persistent => "?what=persistent",
            Self::Segments => "?what=segments",
        }
    }
}

/// `GET /api/v1/cache/mv`: the music-video cache (its settings are the engine's).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MvCache {
    pub enabled: bool,
    pub max_bytes: i64,
    pub quality: String,
    pub size_bytes: i64,
}

/// `PUT /api/v1/cache/mv`: only the given fields change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MvCacheUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<i64>,
}

// ── Status ───────────────────────────────────────────────────────────────────

/// `GET /api/v1/capabilities`: what this engine can play, and the DRM detail for Settings →
/// Engine Status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EngineCapabilities {
    pub lossless: bool,
    pub hi_res: bool,
    pub atmos: bool,
    pub music_video: bool,
    pub downloads: bool,
    pub lyrics: bool,
    pub drm: DrmDetail,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DrmDetail {
    pub process: ProcessState,
    pub fairplay: FairPlayState,
    pub session: SessionState,
    pub cbcs: bool,
    /// False when the build has no DRM layer.
    pub available: bool,
}

/// `GET /api/v1/tools`: whether FFmpeg really works.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Tools {
    pub ffmpeg: FfmpegInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct FfmpegInfo {
    pub available: bool,
    pub path: String,
    pub version: String,
    pub error: String,
    /// `aac`, `flac`, `libx264`; `None` from an engine that does not list them.
    pub encoders: Option<BTreeMap<String, bool>>,
}

impl FfmpegInfo {
    /// FFmpeg can re-encode downloads: it runs and has an AAC or FLAC encoder.
    pub fn can_reencode(&self) -> bool {
        let has = |e: &str| {
            self.encoders
                .as_ref()
                .is_none_or(|m| m.get(e) == Some(&true))
        };
        self.available && (has("aac") || has("flac"))
    }
}

/// The first frame of every event-stream connection: the engine's state now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The engine epoch; it changes when the engine restarts or its DRM session is replaced.
    pub generation: u64,
    /// Why the epoch last advanced (`engine-start`, …).
    pub reason: Option<String>,
    pub player: Option<PlayerState>,
    pub capabilities: SnapshotCapabilities,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SnapshotCapabilities {
    pub lossless: bool,
    pub hi_res: bool,
    pub atmos: bool,
    pub music_video: bool,
}
