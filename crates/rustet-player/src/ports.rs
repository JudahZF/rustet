//! The collaborators the controller drives. `rustet-engine` implements them over `rustet-playback`
//! (sessions), `rustet-vlc` (decks), `rustet-ampapi` (stations, autoplay, play reports) and the
//! config directory (store). Tests implement them with fakes.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::activity::PlayActivityEvent;
use crate::model::{CatalogId, Codec, ItemKind, QueueItemIn, Spatial, VideoSurface};

/// An engine playback session ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// The kind of session to open (spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// AAC 256.
    Aac,
    /// ALAC; `hi_res` allows up to 24/192.
    Lossless { hi_res: bool },
    /// Music video up to `max_height`.
    Video { max_height: u32 },
}

impl OpenMode {
    pub fn is_lossless(self) -> bool {
        matches!(self, Self::Lossless { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    pub id: CatalogId,
    pub kind: ItemKind,
    pub mode: OpenMode,
}

/// The parts of the engine's session descriptor the player needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedSession {
    pub id: SessionId,
    /// The codec of the audio the session plays (`h264` for a music video).
    pub codec: Codec,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub bit_rate: u32,
    pub channels: u32,
    pub spatial: Option<Spatial>,
    /// Reconciled length (`rustet_core::media::reconcile_duration_ms`); 0 when unknown.
    pub duration_ms: u64,
    pub video_height: Option<u32>,
}

impl OpenedSession {
    /// A minimal AAC session, for tests and fakes.
    pub fn test(id: &str) -> Self {
        Self {
            id: SessionId(id.to_owned()),
            codec: Codec::Aac,
            sample_rate: 44_100,
            bit_depth: 0,
            bit_rate: 256_000,
            channels: 2,
            spatial: None,
            duration_ms: 0,
            video_height: None,
        }
    }
}

/// The class of an open failure (the engine's `rustet_apple::Error` variants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenClass {
    Cancelled,
    /// Not in this storefront, or no rendition of the wanted kind.
    NotFound,
    /// Exists, but nothing this client can play here.
    Unavailable,
    /// The licence server refused the key.
    LicenceRefused,
    /// 401/403: the token is expired or missing.
    Authentication,
    /// Apple or the network failed (or the engine's circuit breaker is open).
    Transport,
    Other,
}

impl OpenClass {
    /// A lossless open that failed this way is retried once as AAC.
    pub fn falls_back_to_aac(self) -> bool {
        matches!(
            self,
            Self::NotFound | Self::Unavailable | Self::LicenceRefused
        )
    }

    /// Worth retrying a pre-warm later (renderer: everything except 401/403/404).
    pub fn is_transient(self) -> bool {
        matches!(self, Self::Transport | Self::Other)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{class:?}: {message}")]
pub struct OpenError {
    pub class: OpenClass,
    pub message: String,
}

/// Opens and manages engine sessions (`rustet_playback::Manager`).
#[async_trait]
pub trait Opener: Send + Sync {
    /// Opens a session. Returns `OpenClass::Cancelled` when `cancel` fires first.
    async fn open(
        &self,
        req: OpenRequest,
        cancel: CancellationToken,
    ) -> Result<OpenedSession, OpenError>;
    /// Starts the background disk-cache download of a lossless session (`precache`).
    async fn precache(&self, session: &SessionId);
    /// Keeps a session alive. `Err(NotFound)` means the engine dropped it.
    async fn touch(&self, session: &SessionId) -> Result<(), OpenError>;
    /// The player no longer needs the session.
    async fn release(&self, session: &SessionId);
}

// ── Decks ───────────────────────────────────────────────────────────────────

/// One of the two libvlc players.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckId {
    A,
    B,
}

impl DeckId {
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    pub(crate) fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }
}

/// Identifies one `load` on a deck. Every event carries the load it belongs to, so a late event
/// of the previous media never reaches the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LoadId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadRequest {
    pub load: LoadId,
    pub session: SessionId,
    pub start_ms: u64,
    /// Load without starting playback.
    pub paused: bool,
    /// 0–100, applied before the first sample plays.
    pub volume: u8,
    pub muted: bool,
    /// Show the video of a music-video session; false plays its audio only.
    pub video: bool,
}

/// What a deck reports between loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckEvent {
    /// libvlc entered `Playing`.
    Playing,
    /// libvlc reached the end. `source_eof`: the source delivered all its bytes and ended cleanly.
    Ended { source_eof: bool },
    /// libvlc reported an error.
    Error,
    /// libvlc stopped without the player asking (counts as an end).
    Stopped,
    /// A video load shows its frames in this ring now (a first frame, or a new ring after a size
    /// change); `None`: its ring closed.
    Video(Option<VideoSurface>),
}

/// An event of one deck, as the engine forwards it to the player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckSignal {
    pub deck: DeckId,
    pub load: LoadId,
    pub event: DeckEvent,
}

/// A snapshot of a deck.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeckTiming {
    /// `None` (or 0) before the first decoded sample.
    pub position_ms: Option<u64>,
    pub length_ms: Option<u64>,
    /// The source has delivered all its data. libvlc's demuxer is then at the end of the
    /// stream and its time stops, up to one fragment (about 15 s) before the audio ends.
    pub source_ended: bool,
    /// The volume libvlc plays at now: follows the desktop mixer.
    pub volume: u8,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeckError {
    /// The engine no longer knows the session: open a new one.
    #[error("session gone")]
    SessionGone,
    /// The target is outside the downloaded bytes: reload at the position instead.
    #[error("seek needs a reload")]
    NeedsReload,
    #[error("deck: {0}")]
    Failed(String),
}

/// One libvlc player (`rustet_vlc`). Calls return once libvlc accepted them. The deck reports
/// libvlc's `Playing`, `EndReached`, `EncounteredError` and `Stopped` as [`DeckSignal`]s on the
/// [`deck_channel`], tagged with the load they belong to.
#[async_trait]
pub trait Deck: Send + Sync {
    /// Replaces the media with the session's stream. Later events carry `req.load`.
    async fn load(&self, req: LoadRequest) -> Result<(), DeckError>;
    async fn play(&self) -> Result<(), DeckError>;
    async fn pause(&self) -> Result<(), DeckError>;
    async fn seek(&self, position_ms: u64) -> Result<(), DeckError>;
    async fn stop(&self) -> Result<(), DeckError>;
    async fn set_volume(&self, level: u8, muted: bool) -> Result<(), DeckError>;
    async fn timing(&self) -> DeckTiming;
}

/// Where decks send their events. Created with [`deck_channel`].
pub type DeckEvents = mpsc::UnboundedSender<DeckSignal>;

/// The channel from the decks to the player.
pub fn deck_channel() -> (DeckEvents, mpsc::UnboundedReceiver<DeckSignal>) {
    mpsc::unbounded_channel()
}

// ── Clock ───────────────────────────────────────────────────────────────────

/// Wall-clock time for the values the player publishes (`positionAt`, `savedAt`, report
/// timestamps). Timers use `tokio::time`, so tests can pause time.
pub trait Clock: Send + Sync {
    /// Unix milliseconds.
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

// ── Catalog ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("authentication")]
    Authentication,
    #[error("{0}")]
    Other(String),
}

/// The catalog calls of the player (`rustet_ampapi`).
#[async_trait]
pub trait Catalog: Send + Sync {
    /// `GET /v1/me/stations/next-tracks/{id}?limit=`.
    async fn station_next_tracks(
        &self,
        station_id: &str,
        limit: usize,
    ) -> Result<Vec<QueueItemIn>, CatalogError>;
    /// `POST /v1/me/stations/continuous` seeded with recent items, then its next tracks.
    async fn autoplay_tracks(
        &self,
        seeds: &[CatalogId],
        limit: usize,
    ) -> Result<Vec<QueueItemIn>, CatalogError>;
}

// ── Play activity ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("play report: {0}")]
pub struct ReportError(pub String);

/// Sends play-activity batches to `universal-activity-service.itunes.apple.com/play`
/// (`rustet_ampapi`). The sender adds the client fields (see [`crate::activity`]).
#[async_trait]
pub trait ActivityReporter: Send + Sync {
    async fn report(&self, events: &[PlayActivityEvent]) -> Result<(), ReportError>;
}

// ── Storage ─────────────────────────────────────────────────────────────────

/// A file the player persists. The engine stores each one in `<config dir>/<file_name>` with
/// an atomic write. The bytes are JSON whose format this crate owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StoreKey {
    /// The queue snapshot for resume (`player-state.json`).
    Snapshot,
    /// Play history (`player-history.json`).
    History,
    /// Volume and settings (`player-prefs.json`).
    Prefs,
}

impl StoreKey {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Snapshot => "player-state.json",
            Self::History => "player-history.json",
            Self::Prefs => "player-prefs.json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("store: {0}")]
pub struct StoreError(pub String);

#[async_trait]
pub trait Store: Send + Sync {
    async fn load(&self, key: StoreKey) -> Result<Option<Vec<u8>>, StoreError>;
    async fn save(&self, key: StoreKey, bytes: Vec<u8>) -> Result<(), StoreError>;
    async fn delete(&self, key: StoreKey) -> Result<(), StoreError>;
}

// ── Pre-warm signal ─────────────────────────────────────────────────────────

/// One track of the pre-warm window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrewarmTrack {
    pub id: CatalogId,
    /// Index in the whole queue.
    pub index: usize,
    /// Distance from the current item.
    pub queue_distance: usize,
}

/// The queue window for the engine's pre-warm scheduler (`rustet_playback::prefetch`
/// `ContextPayload` with reason `queue-change`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrewarmWindow {
    pub current_index: Option<usize>,
    /// The player will ask for lossless.
    pub lossless: bool,
    pub tracks: Vec<PrewarmTrack>,
}

/// Receives the pre-warm window on every queue or track change. Must not block.
pub trait PrewarmSink: Send + Sync {
    fn submit(&self, window: PrewarmWindow);
}

/// Everything the controller drives.
#[derive(Clone)]
pub struct Ports {
    pub opener: Arc<dyn Opener>,
    pub decks: [Arc<dyn Deck>; 2],
    pub clock: Arc<dyn Clock>,
    pub catalog: Arc<dyn Catalog>,
    pub reporter: Arc<dyn ActivityReporter>,
    pub store: Arc<dyn Store>,
    pub prewarm: Arc<dyn PrewarmSink>,
}
