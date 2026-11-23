//! The interface the engine serves and the UI consumes (spec §4.8): commands, state and events.
//!
//! Every type here is serde with camelCase JSON names. `rustet-engine` maps each route body to one
//! [`Command`] variant and forwards [`Event`]s to the SSE stream unchanged.

use serde::{Deserialize, Serialize};

/// A catalog adam ID: what the engine opens a session for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogId(pub String);

impl CatalogId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Song or music video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    Song,
    MusicVideo,
}

/// Why an item is in the queue. User items always precede autoplay items in the upcoming part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemSource {
    /// Added with play next / play later, or moved by the user.
    User,
    /// Part of the album, playlist or list that started playback.
    Context,
    /// Appended by Apple's autoplay station when the queue ran low.
    Autoplay,
    /// Appended from the station that is playing.
    Station,
}

/// What the UI already displays about a track, so the engine needs no catalog call to play it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrackMeta {
    pub name: String,
    pub artist_name: String,
    pub album_name: String,
    /// Apple's artwork template with `{w}` and `{h}`.
    pub artwork_url: String,
    /// Catalog length; 0 when unknown.
    pub duration_ms: u64,
    /// Apple's `audioTraits` (`lossless`, `hi-res-lossless`, `lossy-stereo`, `atmos`, …).
    pub audio_traits: Vec<String>,
    /// `explicit` or `clean`, when Apple gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<String>,
}

impl TrackMeta {
    /// True when the catalog says a lossless rendition exists. An empty list means "unknown",
    /// which also counts as lossless-capable: the open falls back to AAC when it is not.
    pub fn may_be_lossless(&self) -> bool {
        self.audio_traits.is_empty()
            || self
                .audio_traits
                .iter()
                .any(|t| t == "lossless" || t == "hi-res-lossless")
    }
}

/// A track as the UI sends it (`QueueItemIn`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItemIn {
    pub id: CatalogId,
    /// The library ID (`i.…`) when the item came from the user's library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_id: Option<String>,
    pub kind: ItemKind,
    #[serde(default)]
    pub meta: TrackMeta,
    /// The album or playlist the item belongs to, when it differs from the queue context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
}

/// One queue entry. `uid` is stable for the UI: the same song can be queued twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub uid: u64,
    pub id: CatalogId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_id: Option<String>,
    pub kind: ItemKind,
    pub source: ItemSource,
    pub meta: TrackMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
}

/// What started playback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub kind: ContainerKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContainerKind {
    Album,
    Playlist,
    Artist,
    /// A station whose `playParams.format` is `tracks`: the player fetches `next-tracks`.
    Station,
    /// A live station (`format: "stream"`). Later work: rejected with `unsupported` for now.
    LiveStation,
    LibraryAlbum,
    LibraryPlaylist,
    /// A library list (all songs, recently added).
    LibrarySongs,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// Player status (spec §4.3). `Ended` of the diagram is transient and never published.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    #[default]
    Idle,
    /// A session is being opened.
    Opening,
    /// The deck has the media; libvlc is not playing yet.
    Loading,
    Playing,
    Paused,
    Stopped,
    Failed,
}

/// Streaming quality setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingQuality {
    /// AAC 256.
    HighQuality,
    /// ALAC up to 24/48.
    #[default]
    Lossless,
    /// ALAC up to 24/192.
    HiResLossless,
}

/// Crossfade (later work: stored and published, not applied yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CrossfadeMode {
    #[default]
    Off,
    /// 6 s.
    Automatic,
    /// 2–10 s from audio analysis.
    Adaptive,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Crossfade {
    pub mode: CrossfadeMode,
    /// Fade length for [`CrossfadeMode::Manual`].
    pub manual_seconds: u8,
    /// No fade between consecutive tracks of one album.
    pub keep_albums_gapless: bool,
    /// Crossfade station tracks too.
    pub stations: bool,
}

impl Default for Crossfade {
    fn default() -> Self {
        Self {
            mode: CrossfadeMode::Off,
            manual_seconds: 6,
            keep_albums_gapless: true,
            stations: true,
        }
    }
}

/// The player settings. Quality changes apply at the next track start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlayerSettings {
    pub quality: StreamingQuality,
    pub lossless_enabled: bool,
    /// Music-video height limit: 480, 720, 1080 or 2160.
    pub mv_max_height: u32,
    pub crossfade: Crossfade,
    /// Sound Check (later work: stored, not applied).
    pub sound_check: bool,
    /// Show play history in the Up Next panel. History is recorded either way.
    pub history_enabled: bool,
    /// Report plays to Apple (play activity).
    pub report_plays: bool,
    /// Restore the queue paused on launch.
    pub resume_on_launch: bool,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            quality: StreamingQuality::Lossless,
            lossless_enabled: true,
            mv_max_height: 2160,
            crossfade: Crossfade::default(),
            sound_check: false,
            history_enabled: true,
            report_plays: true,
            resume_on_launch: true,
        }
    }
}

/// The music-video heights a user may pick.
pub const MV_HEIGHTS: [u32; 4] = [480, 720, 1080, 2160];

/// What the engine can do now. Sent by the engine, not the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Capabilities {
    /// FairPlay is ready (DRM snapshot `cbcs`/`alac`).
    pub lossless_ready: bool,
    /// A music user token is present: autoplay, stations and play reports need it.
    pub signed_in: bool,
}

/// Audio or video codec of the playing session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Codec {
    Alac,
    Aac,
    /// Dolby Atmos (E-AC-3 JOC).
    Atmos,
    H264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Spatial {
    Binaural,
    BinauralLossless,
}

/// The quality badge to draw, decided here so every view shows the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QualityTier {
    Aac,
    Lossless,
    HiResLossless,
    Spatial,
    DolbyAtmos,
    Video,
}

/// The quality of the playing session (the badge and its popup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quality {
    pub codec: Codec,
    pub tier: QualityTier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial: Option<Spatial>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_height: Option<u32>,
    /// Lossless was wanted but this track plays in AAC (the open fell back).
    pub fell_back: bool,
}

/// The frame ring of a playing music video (spec §4.9). The UI maps `ring` read-only with
/// `rustet_framering::Reader` and draws its newest frame; a new `generation` (a new path) replaces
/// the ring, for example after a seek or when the frame size changes. `None` in the state: no
/// video to show (an audio track, a video played as audio, or before the first frame).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSurface {
    /// The ring file's path.
    pub ring: String,
    /// Unique per ring in the engine process.
    #[serde(default)]
    pub generation: u64,
    pub width: u32,
    pub height: u32,
}

/// `POST /api/v1/player/video`: play music videos as audio only (no frames are decoded).
/// Applies at once to a playing music video, which reloads at its position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoMode {
    pub audio_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// A track could not be opened; the player moves on.
    OpenFailed,
    /// Three tracks in a row failed: the player stopped.
    TooManyFailures,
    /// Playback of an opened track failed and recovery gave up.
    PlaybackFailed,
    /// Apple rejected the credentials.
    AuthExpired,
    /// A feature that is not built yet (live stations).
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerError {
    pub code: ErrorCode,
    pub message: String,
}

/// The queue as the UI shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueView {
    pub items: Vec<QueueItem>,
    pub current_index: Option<usize>,
}

/// `GET /api/v1/player` and the `player.state` event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    /// Bumped at every track change; a command with a stale `expectGeneration` is refused.
    pub generation: u64,
    pub status: Status,
    pub track: Option<QueueItem>,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// Unix ms when `positionMs` was sampled; the UI interpolates while playing.
    pub position_at: u64,
    pub quality: Option<Quality>,
    /// 0–100.
    pub volume: u8,
    pub muted: bool,
    pub repeat: Repeat,
    pub shuffle: bool,
    pub autoplay: bool,
    pub queue: QueueView,
    pub error: Option<PlayerError>,
    pub video: Option<VideoSurface>,
    /// Music videos play as audio only ([`VideoMode`]).
    #[serde(default)]
    pub mv_audio_only: bool,
    pub settings: PlayerSettings,
    pub capabilities: Capabilities,
    /// Previous does something (restart or an earlier item).
    pub can_previous: bool,
    pub can_next: bool,
    /// The queue was restored from the last run and nothing was played yet: show "Resume".
    pub restored: bool,
}

/// One play-history entry (`GET /api/v1/player/history`), newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: CatalogId,
    pub kind: ItemKind,
    pub meta: TrackMeta,
    /// Unix ms when the track stopped playing.
    pub played_at: u64,
}

// ── Commands ────────────────────────────────────────────────────────────────

/// `POST /api/v1/player/queue`: replace the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetQueue {
    pub items: Vec<QueueItemIn>,
    #[serde(default)]
    pub start_index: usize,
    #[serde(default)]
    pub context: Option<Container>,
    /// Absent: keep the current shuffle mode.
    #[serde(default)]
    pub shuffle: Option<bool>,
    #[serde(default)]
    pub start_ms: Option<u64>,
    #[serde(default = "yes")]
    pub play: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InsertPosition {
    /// Right after the current item.
    Next,
    /// After the last user item, before any autoplay item.
    Later,
}

/// `POST /api/v1/player/queue/insert`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Insert {
    pub items: Vec<QueueItemIn>,
    pub position: InsertPosition,
}

/// `POST /api/v1/player/queue/remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Remove {
    pub uids: Vec<u64>,
}

/// `POST /api/v1/player/queue/move`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Move {
    pub uid: u64,
    pub to_index: usize,
}

/// `POST /api/v1/player/queue/jump`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Jump {
    pub uid: u64,
}

/// `POST /api/v1/player/seek`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Seek {
    pub position_ms: u64,
    #[serde(default)]
    pub expect_generation: Option<u64>,
}

/// `POST /api/v1/player/volume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    /// 0–100.
    pub level: u8,
    #[serde(default)]
    pub muted: Option<bool>,
}

/// `POST /api/v1/player/mode`: only the given fields change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Mode {
    pub repeat: Option<Repeat>,
    pub shuffle: Option<bool>,
    pub autoplay: Option<bool>,
}

/// `POST /api/v1/player/settings`: only the given fields change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SettingsPatch {
    pub quality: Option<StreamingQuality>,
    pub lossless_enabled: Option<bool>,
    pub mv_max_height: Option<u32>,
    pub crossfade: Option<Crossfade>,
    pub sound_check: Option<bool>,
    pub history_enabled: Option<bool>,
    pub report_plays: Option<bool>,
    pub resume_on_launch: Option<bool>,
}

/// `POST /api/v1/player/hover`: the pointer rests on an item (the UI applies the 350 ms intent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hover {
    pub id: CatalogId,
    pub kind: ItemKind,
}

/// Every command the player accepts. One task executes them in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum Command {
    SetQueue(SetQueue),
    Insert(Insert),
    Remove(Remove),
    Move(Move),
    Jump(Jump),
    /// Remove every upcoming item.
    ClearUpcoming,
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    Seek(Seek),
    Volume(Volume),
    Mode(Mode),
    Settings(SettingsPatch),
    Hover(Hover),
    /// `DELETE /api/v1/player/history`.
    ClearHistory,
    /// Engine only: FairPlay or sign-in state changed.
    Capabilities(Capabilities),
}

/// Why a command was refused. The engine maps `StaleGeneration` to 409, the rest to 400/404.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("stale generation (current {current})")]
    StaleGeneration { current: u64 },
    #[error("no queue item with uid {0}")]
    UnknownUid(u64),
    #[error("start index {index} is outside a queue of {len}")]
    BadIndex { index: usize, len: usize },
    #[error("volume {0} is above 100")]
    BadVolume(u8),
    #[error("music-video height {0} is not one of 480, 720, 1080, 2160")]
    BadHeight(u32),
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    #[error("the player has stopped")]
    Closed,
}

// ── Events ──────────────────────────────────────────────────────────────────

/// `player.position`: 1 Hz while playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub generation: u64,
    pub uid: u64,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub position_at: u64,
}

/// `player.track`: a track started playing (now-playing for Last.fm, ListenBrainz, Discord).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackStarted {
    pub generation: u64,
    pub item: QueueItem,
    pub duration_ms: u64,
    pub quality: Option<Quality>,
    /// Unix ms of the start.
    pub started_at: u64,
}

/// `player.scrobble_due`: the track played long enough to scrobble. Sent once per track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrobbleDue {
    pub generation: u64,
    pub item: QueueItem,
    pub duration_ms: u64,
    /// Unix seconds of the track start (Last.fm `timestamp`).
    pub timestamp: u64,
}

/// Events on `/api/v1/events`. JSON: `{"type": "player.state", "payload": {…}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Event {
    #[serde(rename = "player.state")]
    State(Box<PlayerState>),
    #[serde(rename = "player.position")]
    Position(Position),
    #[serde(rename = "player.queue")]
    Queue(QueueView),
    #[serde(rename = "player.track")]
    Track(Box<TrackStarted>),
    #[serde(rename = "player.scrobble_due")]
    ScrobbleDue(Box<ScrobbleDue>),
    #[serde(rename = "player.error")]
    Error(PlayerError),
    #[serde(rename = "player.history")]
    History(Vec<HistoryEntry>),
    #[serde(rename = "auth.expired")]
    AuthExpired,
}

impl Event {
    /// The SSE event name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::State(_) => "player.state",
            Self::Position(_) => "player.position",
            Self::Queue(_) => "player.queue",
            Self::Track(_) => "player.track",
            Self::ScrobbleDue(_) => "player.scrobble_due",
            Self::Error(_) => "player.error",
            Self::History(_) => "player.history",
            Self::AuthExpired => "auth.expired",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn queue_item_in_takes_the_spec_shape() {
        let item: QueueItemIn = serde_json::from_value(json!({
            "id": "1440857781", "kind": "musicVideo",
            "meta": {"name": "N", "artistName": "A", "albumName": "B", "artworkUrl": "u/{w}x{h}",
                     "durationMs": 1000, "audioTraits": ["lossless"], "contentRating": "explicit"},
            "containerId": "p.1"
        }))
        .unwrap();
        assert_eq!(item.kind, ItemKind::MusicVideo);
        assert_eq!(item.meta.audio_traits, ["lossless"]);
        assert_eq!(item.container_id.as_deref(), Some("p.1"));
        // meta is optional: the UI may send only id and kind.
        let bare: QueueItemIn = serde_json::from_value(json!({"id": "1", "kind": "song"})).unwrap();
        assert_eq!(bare.meta, TrackMeta::default());
    }

    #[test]
    fn route_bodies_parse_with_their_defaults() {
        let q: SetQueue = serde_json::from_value(json!({"items": [], "startIndex": 2})).unwrap();
        assert!(q.play, "play defaults to true");
        assert_eq!(q.shuffle, None);
        let m: Mode = serde_json::from_value(json!({"repeat": "one"})).unwrap();
        assert_eq!(
            m,
            Mode {
                repeat: Some(Repeat::One),
                ..Mode::default()
            }
        );
        let i: Insert = serde_json::from_value(json!({"items": [], "position": "later"})).unwrap();
        assert_eq!(i.position, InsertPosition::Later);
        let s: Seek =
            serde_json::from_value(json!({"positionMs": 5, "expectGeneration": 3})).unwrap();
        assert_eq!(s.expect_generation, Some(3));
    }

    #[test]
    fn commands_are_tagged() {
        let c: Command = serde_json::from_value(json!({"command": "next"})).unwrap();
        assert_eq!(c, Command::Next);
        let c: Command = serde_json::from_value(json!({"command": "jump", "uid": 4})).unwrap();
        assert_eq!(c, Command::Jump(Jump { uid: 4 }));
    }

    #[test]
    fn event_envelope_and_names() {
        let e = Event::Position(Position {
            generation: 1,
            uid: 2,
            position_ms: 3,
            duration_ms: 4,
            position_at: 5,
        });
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"type": "player.position", "payload":
                {"generation": 1, "uid": 2, "positionMs": 3, "durationMs": 4, "positionAt": 5}})
        );
        assert_eq!(e.name(), "player.position");
        assert_eq!(
            serde_json::to_value(Event::AuthExpired).unwrap(),
            json!({"type": "auth.expired"})
        );
        let state = serde_json::to_value(Event::State(Box::default())).unwrap();
        assert_eq!(state["type"], "player.state");
        for key in [
            "generation",
            "status",
            "positionMs",
            "positionAt",
            "queue",
            "canNext",
            "restored",
        ] {
            assert!(state["payload"].get(key).is_some(), "{key}");
        }
        assert_eq!(state["payload"]["status"], "idle");
    }

    #[test]
    fn may_be_lossless_reads_audio_traits() {
        let meta = |t: &[&str]| TrackMeta {
            audio_traits: t.iter().map(|s| s.to_string()).collect(),
            ..TrackMeta::default()
        };
        assert!(meta(&[]).may_be_lossless(), "unknown traits try lossless");
        assert!(meta(&["lossy-stereo", "lossless"]).may_be_lossless());
        assert!(meta(&["hi-res-lossless"]).may_be_lossless());
        assert!(!meta(&["lossy-stereo"]).may_be_lossless());
    }
}
