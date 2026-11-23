//! The player task: one owner of the queue, the decks and the per-track state machine.
//!
//! Commands arrive in order on one channel and run to completion before the next one. Slow
//! work (session opens, catalog calls, delays) runs in spawned tasks that report back as
//! [`Internal`] messages tagged with the generation they belong to; a result for an older
//! generation is dropped (and its session released). Deck events carry the load they belong to,
//! so an event of a replaced media never reaches the next track.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::activity::{EndReason, Outbox, PlayActivityEvent, PlayMode, ReportContext};
use crate::history::History;
use crate::model::{
    Capabilities, CatalogId, Codec, Command, CommandError, ContainerKind, ErrorCode, Event,
    HistoryEntry, Hover, ItemKind, ItemSource, MV_HEIGHTS, PlayerError, PlayerSettings,
    PlayerState, Position, Quality, QueueItem, ScrobbleDue, SetQueue, SettingsPatch, Status,
    TrackMeta, TrackStarted, VideoSurface,
};
use crate::ports::{
    CatalogError, DeckError, DeckEvent, DeckId, DeckSignal, LoadId, LoadRequest, OpenClass,
    OpenError, OpenMode, OpenRequest, OpenedSession, Ports, PrewarmTrack, PrewarmWindow, SessionId,
    StoreKey,
};
use crate::queue::{AUTOPLAY_CAP, Advance, Queue, Refill, STATION_BATCH};
use crate::rules::{
    EndAction, EndKind, HISTORY_MIN_PLAYED, choose_mode, classify_end, is_legal_transition,
    length_after_overrun, previous_restarts, quality_of, scrobble_after, track_length_ms,
};
use crate::snapshot::{Prefs, Snapshot};

/// Deck polling period while a track plays (spec §4.6).
const TICK: Duration = Duration::from_millis(100);
/// `player.position` period.
const POSITION_EVERY: Duration = Duration::from_secs(1);
/// Resume snapshot period while playing (spec §4.1).
const SNAPSHOT_EVERY: Duration = Duration::from_secs(5);
/// Session keep-alive while paused (spec §4.6).
const KEEPALIVE_EVERY: Duration = Duration::from_secs(5 * 60);
/// Play reports wait at most this long (spec §2.4).
const REPORT_FLUSH_EVERY: Duration = Duration::from_secs(30);
/// A failed open moves to the next track after this (spec §4.6).
const FAILED_ADVANCE_DELAY: Duration = Duration::from_secs(1);
/// Consecutive failed tracks before the player gives up.
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const TOO_MANY_FAILURES: &str = "Couldn't play 3 tracks in a row. Check your connection.";
/// Deck calls (renderer transport timeout 4 s, reload 15 s, stop 2 s).
const DECK_CALL_TIMEOUT: Duration = Duration::from_secs(4);
const DECK_LOAD_TIMEOUT: Duration = Duration::from_secs(15);
const DECK_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// A mixer reading this soon after the player set the volume may predate it.
const VOLUME_ECHO_WINDOW: Duration = Duration::from_millis(1500);
/// After a seek, a position this far before the target within the window means libvlc rewound
/// (a failed range request): reload at the target instead.
const SEEK_REWIND_MS: u64 = 5000;
const SEEK_REWIND_WINDOW: Duration = Duration::from_millis(2500);
/// The next AAC track is opened this long before the end (`GAPLESS_STREAM_LEAD`).
const AAC_PREWARM_LEAD_MS: u64 = 8000;
/// The next session is loaded paused into the idle deck this long before the end.
pub const PREROLL_MS: u64 = 2000;
/// Pre-warm retries for transient failures, and their spacing.
const PREWARM_RETRIES: u32 = 3;
const PREWARM_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Hover sessions kept (renderer LRU cap).
const HOVER_CAP: usize = 4;
/// Most tracks sent to the pre-warm scheduler, and how many before the current one.
const PREWARM_WINDOW: usize = 200;
const PREWARM_WINDOW_BEHIND: usize = 10;
/// Shutdown waits this long for the last play reports.
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Starts the player.
pub struct Player;

impl Player {
    /// Spawns the player task on the current tokio runtime. `deck_events` is the receiving end
    /// of [`crate::ports::deck_channel`]; the engine gives the sending end to both decks.
    ///
    /// The task first loads the preferences, the history and (with "resume on launch") the
    /// snapshot, which it restores paused. Commands sent meanwhile wait.
    pub fn spawn(ports: Ports, deck_events: mpsc::UnboundedReceiver<DeckSignal>) -> PlayerHandle {
        let (tx, rx) = mpsc::channel(64);
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(256);
        let (state_tx, state) = watch::channel(PlayerState::default());
        let (history_tx, history) = watch::channel(Vec::new());
        let seed = ports.clock.now_ms();
        let controller = Controller {
            ports,
            queue: Queue::new(seed),
            settings: PlayerSettings::default(),
            caps: Capabilities::default(),
            volume: 100,
            muted: false,
            mv_audio_only: false,
            volume_set_at: None,
            history: History::default(),
            generation: 0,
            status: Status::Idle,
            track: None,
            active: None,
            next_load: 0,
            prewarm: None,
            hover: VecDeque::new(),
            hover_inflight: Vec::new(),
            failures: 0,
            outbox: Outbox::default(),
            refill_epoch: None,
            pending_start: None,
            restored: false,
            error: None,
            internal: internal_tx,
            events: events.clone(),
            state_tx,
            history_tx,
            last_key: None,
            last_queue_version: 0,
            snapshot_dirty: false,
            last_snapshot: Instant::now(),
            last_position_event: Instant::now(),
            flush_at: None,
        };
        tokio::spawn(controller.run(rx, internal_rx, deck_events));
        PlayerHandle {
            tx,
            state,
            history,
            events,
        }
    }
}

/// The engine's handle on the player. Cheap to clone.
#[derive(Clone)]
pub struct PlayerHandle {
    tx: mpsc::Sender<Msg>,
    state: watch::Receiver<PlayerState>,
    history: watch::Receiver<Vec<HistoryEntry>>,
    events: broadcast::Sender<Event>,
}

impl PlayerHandle {
    /// Runs a command. Returns once the player executed it (spawned opens may still be running).
    pub async fn send(&self, command: Command) -> Result<(), CommandError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(command, reply))
            .await
            .map_err(|_| CommandError::Closed)?;
        rx.await.map_err(|_| CommandError::Closed)?
    }

    /// The current state (`GET /api/v1/player`). The position is at most 1 s old while playing.
    pub fn state(&self) -> PlayerState {
        self.state.borrow().clone()
    }

    /// The state as a watch channel.
    pub fn watch_state(&self) -> watch::Receiver<PlayerState> {
        self.state.clone()
    }

    /// The play history, newest first (`GET /api/v1/player/history`).
    pub fn history(&self) -> Vec<HistoryEntry> {
        self.history.borrow().clone()
    }

    /// Player events for the SSE stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Plays music videos as audio only, or with their video again (`POST /api/v1/player/video`).
    /// A playing music video reloads at its position at once. The choice is kept across restarts.
    pub async fn set_mv_audio_only(&self, audio_only: bool) -> Result<(), CommandError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::AudioOnly(audio_only, reply))
            .await
            .map_err(|_| CommandError::Closed)?;
        rx.await.map_err(|_| CommandError::Closed)
    }

    /// Reports the open play as "exited", flushes play reports and saves the snapshot, then
    /// stops the task. The decks are left as they are.
    pub async fn shutdown(&self) {
        let (done, rx) = oneshot::channel();
        if self.tx.send(Msg::Shutdown(done)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

enum Msg {
    Command(Command, oneshot::Sender<Result<(), CommandError>>),
    AudioOnly(bool, oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

/// Results of spawned work.
enum Internal {
    Opened {
        generation: u64,
        result: Result<OpenedSession, OpenError>,
    },
    Reload {
        generation: u64,
        from_ms: u64,
    },
    AdvanceAfterFailure {
        generation: u64,
    },
    PrewarmOpened {
        uid: u64,
        mode: OpenMode,
        result: Result<OpenedSession, OpenError>,
    },
    PrewarmRetry {
        uid: u64,
    },
    HoverOpened {
        id: CatalogId,
        kind: ItemKind,
        mode: OpenMode,
        result: Result<OpenedSession, OpenError>,
    },
    Refilled {
        epoch: u64,
        source: ItemSource,
        result: Result<Vec<crate::model::QueueItemIn>, CatalogError>,
    },
    Touched {
        generation: u64,
        result: Result<(), OpenError>,
    },
    /// A report batch was sent; the events to send again (empty on success).
    Reported(Vec<PlayActivityEvent>),
}

/// The state of the current queue item. Replaced at every track change.
struct Track {
    generation: u64,
    uid: u64,
    mode: OpenMode,
    /// The first open asked for lossless.
    wanted_lossless: bool,
    /// The lossless open failed and this track was reopened as AAC.
    fell_back: bool,
    cancel: CancellationToken,
    session: Option<OpenedSession>,
    quality: Option<Quality>,
    /// The engine dropped the session during a long pause: reopen on play.
    session_lost: bool,
    load: Option<(DeckId, LoadId)>,
    /// Where the next open or load starts.
    start_ms: u64,
    position_ms: u64,
    position_at: u64,
    /// The position the deck last reported, to tell when it stops moving.
    deck_position: Option<u64>,
    /// Length in use: the catalog's, replaced by the stream's own when it differs.
    duration_ms: u64,
    stream_length_seen: bool,
    want_playing: bool,
    started: bool,
    started_at: u64,
    end_retries: u32,
    /// A session-gone reload already reopened the track once.
    reopened: bool,
    /// Time spent playing (not paused), for scrobble and history.
    played: Duration,
    last_tick: Option<Instant>,
    scrobbled: bool,
    natural_end: bool,
    /// Start position of the open play-activity segment.
    segment: Option<u64>,
    seek_guard: Option<(u64, Instant)>,
    touch_at: Option<Instant>,
    /// The frame ring the deck shows the current load's video in.
    video: Option<VideoSurface>,
}

impl Track {
    fn new(
        generation: u64,
        item: &QueueItem,
        mode: OpenMode,
        start_ms: u64,
        play: bool,
        now_ms: u64,
    ) -> Self {
        Self {
            generation,
            uid: item.uid,
            mode,
            wanted_lossless: mode.is_lossless(),
            fell_back: false,
            cancel: CancellationToken::new(),
            session: None,
            quality: None,
            session_lost: false,
            load: None,
            start_ms,
            position_ms: start_ms,
            position_at: now_ms,
            deck_position: None,
            duration_ms: item.meta.duration_ms,
            stream_length_seen: false,
            want_playing: play,
            started: false,
            started_at: 0,
            end_retries: 0,
            reopened: false,
            played: Duration::ZERO,
            last_tick: None,
            scrobbled: false,
            natural_end: false,
            segment: None,
            seek_guard: None,
            touch_at: None,
            video: None,
        }
    }

    /// Adds the time since the last tick to the played time and returns it.
    /// Whether the position takes the deck's time `deck_ms`. After the source ended, libvlc's
    /// time stops while the last fragment still plays and the position runs on the clock
    /// (`tick`). A late value from the deck is then behind the audio and must not pull the
    /// position back. A seek in progress always follows the deck, so a rewind is seen.
    fn follows_deck(&self, deck_ms: u64, source_ended: bool) -> bool {
        !source_ended || self.seek_guard.is_some() || deck_ms >= self.position_ms
    }

    fn account_played(&mut self, now: Instant) -> Duration {
        let Some(last) = self.last_tick.replace(now) else {
            self.last_tick = None;
            return Duration::ZERO;
        };
        self.played += now - last;
        now - last
    }
}

/// The next track, opened ahead (spec §4.5).
struct Prewarm {
    uid: u64,
    mode: OpenMode,
    state: PrewarmState,
    retries: u32,
}

enum PrewarmState {
    Opening(CancellationToken),
    /// A retry is scheduled.
    Waiting,
    Ready(OpenedSession),
    /// Loaded paused at 0, volume 0, in the idle deck: the gapless pre-roll. A music video's
    /// ring, when the deck already made one, goes to the track when it takes the load over.
    Loaded {
        session: OpenedSession,
        deck: DeckId,
        load: LoadId,
        video: Option<VideoSurface>,
    },
    /// Failed for good; the track opens normally when it starts.
    Failed,
}

enum Prewarmed {
    Session(OpenedSession),
    Loaded {
        session: OpenedSession,
        deck: DeckId,
        load: LoadId,
        video: Option<VideoSurface>,
    },
}

/// When the next track is opened ahead.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PrewarmAt {
    /// The current track started playing: lossless and music-video next tracks.
    Start,
    /// The current track is within 8 s of its end: AAC next tracks.
    Tail,
}

struct HoverEntry {
    id: CatalogId,
    kind: ItemKind,
    mode: OpenMode,
    session: OpenedSession,
}

/// Start an item once a refill arrives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingStart {
    /// A station queue started empty: play its first track.
    First { play: bool },
    /// The queue ran out: play the next appended track.
    Next,
}

/// The parts of the state whose change is a `player.state` event (the position has its own).
#[derive(Clone, PartialEq)]
struct StateKey {
    generation: u64,
    status: Status,
    uid: Option<u64>,
    quality: Option<Quality>,
    volume: u8,
    muted: bool,
    queue_version: u64,
    error: Option<PlayerError>,
    settings: PlayerSettings,
    caps: Capabilities,
    restored: bool,
    video: Option<VideoSurface>,
    mv_audio_only: bool,
}

struct Controller {
    ports: Ports,
    queue: Queue,
    settings: PlayerSettings,
    caps: Capabilities,
    volume: u8,
    muted: bool,
    mv_audio_only: bool,
    volume_set_at: Option<Instant>,
    history: History,
    generation: u64,
    status: Status,
    track: Option<Track>,
    /// The deck that plays (or last played) the current track.
    active: Option<DeckId>,
    next_load: u64,
    prewarm: Option<Prewarm>,
    hover: VecDeque<HoverEntry>,
    hover_inflight: Vec<CatalogId>,
    failures: u32,
    outbox: Outbox,
    /// The queue epoch a refill is in flight for.
    refill_epoch: Option<u64>,
    pending_start: Option<PendingStart>,
    restored: bool,
    error: Option<PlayerError>,
    internal: mpsc::UnboundedSender<Internal>,
    events: broadcast::Sender<Event>,
    state_tx: watch::Sender<PlayerState>,
    history_tx: watch::Sender<Vec<HistoryEntry>>,
    last_key: Option<StateKey>,
    last_queue_version: u64,
    snapshot_dirty: bool,
    last_snapshot: Instant,
    last_position_event: Instant,
    flush_at: Option<Instant>,
}

async fn deck_call(
    f: impl Future<Output = Result<(), DeckError>>,
    limit: Duration,
) -> Result<(), DeckError> {
    time::timeout(limit, f)
        .await
        .unwrap_or_else(|_| Err(DeckError::Failed("timed out".into())))
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

impl Controller {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Msg>,
        mut internal: mpsc::UnboundedReceiver<Internal>,
        mut decks: mpsc::UnboundedReceiver<DeckSignal>,
    ) {
        self.init().await;
        self.after_step().await;
        let mut ticker = time::interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                msg = commands.recv() => match msg {
                    Some(Msg::Command(command, reply)) => {
                        let result = self.command(command).await;
                        let _ = reply.send(result);
                    }
                    Some(Msg::AudioOnly(audio_only, reply)) => {
                        self.set_mv_audio_only(audio_only).await;
                        let _ = reply.send(());
                    }
                    Some(Msg::Shutdown(done)) => {
                        self.shutdown().await;
                        let _ = done.send(());
                        return;
                    }
                    None => {
                        self.shutdown().await;
                        return;
                    }
                },
                Some(msg) = internal.recv() => self.internal(msg).await,
                Some(signal) = decks.recv() => self.deck_signal(signal).await,
                _ = ticker.tick(), if self.ticking() => self.tick().await,
                () = sleep_until(deadline), if deadline.is_some() => self.timers().await,
            }
            self.after_step().await;
        }
    }

    // ── Startup and shutdown ────────────────────────────────────────────────

    async fn load(&self, key: StoreKey) -> Option<Vec<u8>> {
        match self.ports.store.load(key).await {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(file = key.file_name(), "player: {e}");
                None
            }
        }
    }

    async fn save(&self, key: StoreKey, bytes: Vec<u8>) {
        if let Err(e) = self.ports.store.save(key, bytes).await {
            warn!(file = key.file_name(), "player: {e}");
        }
    }

    async fn init(&mut self) {
        if let Some(bytes) = self.load(StoreKey::Prefs).await {
            let prefs = Prefs::parse(&bytes);
            self.volume = prefs.volume;
            self.settings = prefs.settings;
            self.mv_audio_only = prefs.mv_audio_only;
        }
        if let Some(bytes) = self.load(StoreKey::History).await {
            self.history = History::parse(&bytes);
            self.history_tx
                .send_replace(self.history.entries().to_vec());
        }
        if !self.settings.resume_on_launch {
            return;
        }
        let Some(bytes) = self.load(StoreKey::Snapshot).await else {
            return;
        };
        match Snapshot::parse(&bytes, self.ports.clock.now_ms()) {
            Some(snap) => {
                let position = snap.position_ms;
                self.queue.restore(snap);
                if self.queue.current().is_some() {
                    self.start_current(position, false).await;
                    self.restored = true;
                    info!(
                        position,
                        items = self.queue.items().len(),
                        "player: queue restored paused"
                    );
                }
            }
            None => {
                debug!("player: snapshot old or unreadable, ignored");
                let _ = self.ports.store.delete(StoreKey::Snapshot).await;
            }
        }
    }

    async fn shutdown(&mut self) {
        self.refresh_position().await;
        self.close_segment(EndReason::ExitedApplication);
        let batch = self.outbox.take();
        if !batch.is_empty() {
            match time::timeout(SHUTDOWN_FLUSH_TIMEOUT, self.ports.reporter.report(&batch)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("player: {e}"),
                Err(_) => warn!("player: play reports timed out at shutdown"),
            }
        }
        self.save_snapshot().await;
    }

    // ── Publishing ──────────────────────────────────────────────────────────

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn position(&self) -> u64 {
        self.track.as_ref().map_or(0, |t| t.position_ms)
    }

    fn set_status(&mut self, status: Status) {
        if !is_legal_transition(self.status, status) {
            warn!(
                "player: unexpected transition {:?} → {status:?}",
                self.status
            );
        }
        if self.status != status {
            debug!("player: {:?} → {status:?}", self.status);
        }
        self.status = status;
    }

    fn state_key(&self) -> StateKey {
        StateKey {
            generation: self.generation,
            status: self.status,
            uid: self.queue.current().map(|i| i.uid),
            quality: self.track.as_ref().and_then(|t| t.quality),
            volume: self.volume,
            muted: self.muted,
            queue_version: self.queue.version(),
            error: self.error.clone(),
            settings: self.settings,
            caps: self.caps,
            restored: self.restored,
            video: self.track.as_ref().and_then(|t| t.video.clone()),
            mv_audio_only: self.mv_audio_only,
        }
    }

    fn build_state(&self) -> PlayerState {
        let t = self.track.as_ref();
        PlayerState {
            generation: self.generation,
            status: self.status,
            track: self
                .queue
                .current()
                .filter(|_| self.status != Status::Idle)
                .cloned(),
            position_ms: self.position(),
            duration_ms: t.map_or_else(
                || self.queue.current().map_or(0, |i| i.meta.duration_ms),
                |t| t.duration_ms,
            ),
            position_at: t.map_or_else(|| self.ports.clock.now_ms(), |t| t.position_at),
            quality: t.and_then(|t| t.quality),
            volume: self.volume,
            muted: self.muted,
            repeat: self.queue.repeat(),
            shuffle: self.queue.shuffle(),
            autoplay: self.queue.autoplay(),
            queue: self.queue.view(),
            error: self.error.clone(),
            video: t.and_then(|t| t.video.clone()),
            mv_audio_only: self.mv_audio_only,
            settings: self.settings,
            capabilities: self.caps,
            can_previous: self.queue.current().is_some(),
            can_next: self.queue.next_index(Advance::Manual).is_some(),
            restored: self.restored,
        }
    }

    async fn after_step(&mut self) {
        if self.queue.version() != self.last_queue_version {
            self.last_queue_version = self.queue.version();
            self.emit(Event::Queue(self.queue.view()));
            self.submit_prewarm_window();
            self.check_prewarm().await;
            self.maybe_refill();
            self.snapshot_dirty = true;
        }
        let key = self.state_key();
        if self.last_key.as_ref() != Some(&key) {
            self.last_key = Some(key);
            let state = self.build_state();
            self.state_tx.send_replace(state.clone());
            self.emit(Event::State(Box::new(state)));
            self.snapshot_dirty = true;
        }
        if self.snapshot_dirty {
            self.snapshot_dirty = false;
            self.save_snapshot().await;
        }
    }

    async fn save_snapshot(&mut self) {
        self.last_snapshot = Instant::now();
        if !self.settings.resume_on_launch {
            return;
        }
        if self.queue.is_empty() {
            let _ = self.ports.store.delete(StoreKey::Snapshot).await;
            return;
        }
        let snap = self
            .queue
            .to_snapshot(self.position(), self.ports.clock.now_ms());
        self.save(StoreKey::Snapshot, snap.to_json()).await;
    }

    async fn save_prefs(&self) {
        let prefs = Prefs {
            volume: self.volume,
            settings: self.settings,
            mv_audio_only: self.mv_audio_only,
        };
        self.save(StoreKey::Prefs, prefs.to_json()).await;
    }

    fn submit_prewarm_window(&self) {
        let items = self.queue.items();
        let cur = self.queue.current_index();
        let start = if items.len() > PREWARM_WINDOW {
            cur.unwrap_or(0)
                .saturating_sub(PREWARM_WINDOW_BEHIND)
                .min(items.len() - PREWARM_WINDOW)
        } else {
            0
        };
        let tracks = items
            .iter()
            .enumerate()
            .skip(start)
            .take(PREWARM_WINDOW)
            .map(|(index, item)| PrewarmTrack {
                id: item.id.clone(),
                index,
                queue_distance: cur.map_or(index, |c| index.abs_diff(c)),
            })
            .collect();
        let lossless = choose_mode(
            ItemKind::Song,
            &TrackMeta::default(),
            &self.settings,
            self.caps,
        )
        .is_lossless();
        self.ports.prewarm.submit(PrewarmWindow {
            current_index: cur,
            lossless,
            tracks,
        });
    }

    // ── Spawned work ────────────────────────────────────────────────────────

    fn spawn(&self, work: impl Future<Output = Internal> + Send + 'static) {
        let tx = self.internal.clone();
        tokio::spawn(async move {
            let _ = tx.send(work.await);
        });
    }

    fn spawn_after(&self, delay: Duration, msg: Internal) {
        self.spawn(async move {
            time::sleep(delay).await;
            msg
        });
    }

    fn release(&self, session: SessionId) {
        let opener = self.ports.opener.clone();
        tokio::spawn(async move { opener.release(&session).await });
    }

    fn spawn_open(&self, generation: u64, req: OpenRequest, cancel: CancellationToken) {
        let opener = self.ports.opener.clone();
        self.spawn(async move {
            Internal::Opened {
                generation,
                result: opener.open(req, cancel).await,
            }
        });
    }

    async fn stop_deck(&self, deck: DeckId) {
        // A failed or hung stop must not hold up the next track (`leaveLossless`).
        if let Err(e) = deck_call(self.ports.decks[deck.index()].stop(), DECK_STOP_TIMEOUT).await {
            warn!(?deck, "player: stop did not complete: {e}");
        }
    }

    fn new_load(&mut self) -> LoadId {
        self.next_load += 1;
        LoadId(self.next_load)
    }

    // ── Commands ────────────────────────────────────────────────────────────

    async fn command(&mut self, command: Command) -> Result<(), CommandError> {
        if matches!(
            command,
            Command::SetQueue(_)
                | Command::Jump(_)
                | Command::Play
                | Command::Toggle
                | Command::Next
                | Command::Previous
                | Command::Stop
                | Command::Seek(_)
        ) {
            self.restored = false;
        }
        match command {
            Command::SetQueue(q) => self.set_queue(q).await?,
            Command::Insert(ins) => {
                let was_empty = self.queue.is_empty();
                self.queue.insert(ins.items, ins.position);
                if was_empty && !self.queue.is_empty() {
                    self.start_current(0, true).await;
                }
            }
            Command::Remove(r) => self.remove(&r.uids).await,
            Command::Move(m) => self
                .queue
                .move_item(m.uid, m.to_index)
                .map_err(|_| CommandError::UnknownUid(m.uid))?,
            Command::Jump(j) => {
                self.queue
                    .index_of(j.uid)
                    .ok_or(CommandError::UnknownUid(j.uid))?;
                self.leave_current(EndReason::OtherItemChosen, true).await;
                self.queue
                    .jump(j.uid)
                    .map_err(|_| CommandError::UnknownUid(j.uid))?;
                self.failures = 0;
                self.start_current(0, true).await;
            }
            Command::ClearUpcoming => self.queue.clear_upcoming(),
            Command::Play => self.play().await,
            Command::Pause => self.pause().await,
            Command::Toggle => {
                if self.track.as_ref().is_some_and(|t| t.want_playing)
                    && matches!(
                        self.status,
                        Status::Opening | Status::Loading | Status::Playing
                    )
                {
                    self.pause().await;
                } else {
                    self.play().await;
                }
            }
            Command::Next => self.next().await,
            Command::Previous => self.previous().await,
            Command::Stop => {
                if self.track.is_some() {
                    self.leave_current(EndReason::ManuallyPaused, true).await;
                    self.set_status(Status::Stopped);
                }
            }
            Command::Seek(s) => {
                if let Some(expected) = s.expect_generation
                    && expected != self.generation
                {
                    return Err(CommandError::StaleGeneration {
                        current: self.generation,
                    });
                }
                self.seek_to(s.position_ms, EndReason::ScrubBegin).await;
            }
            Command::Volume(v) => self.set_volume(v.level, v.muted).await?,
            Command::Mode(m) => {
                if let Some(r) = m.repeat {
                    self.queue.set_repeat(r);
                }
                if let Some(s) = m.shuffle {
                    self.queue.set_shuffle(s);
                }
                if let Some(a) = m.autoplay {
                    self.queue.set_autoplay(a);
                }
            }
            Command::Settings(patch) => self.apply_settings(patch).await?,
            Command::Hover(h) => self.hover(h),
            Command::ClearHistory => {
                self.history.clear();
                self.history_changed().await;
            }
            Command::Capabilities(c) => {
                self.caps = c;
                self.maybe_refill();
            }
        }
        Ok(())
    }

    async fn set_queue(&mut self, q: SetQueue) -> Result<(), CommandError> {
        if q.context
            .as_ref()
            .is_some_and(|c| c.kind == ContainerKind::LiveStation)
        {
            return Err(CommandError::Unsupported("live radio"));
        }
        if !q.items.is_empty() && q.start_index >= q.items.len() {
            return Err(CommandError::BadIndex {
                index: q.start_index,
                len: q.items.len(),
            });
        }
        self.leave_current(EndReason::OtherItemChosen, true).await;
        let station = q
            .context
            .as_ref()
            .is_some_and(|c| c.kind == ContainerKind::Station);
        self.queue
            .set(q.items, q.start_index, q.context, q.shuffle)
            .map_err(|_| CommandError::BadIndex {
                index: q.start_index,
                len: 0,
            })?;
        self.failures = 0;
        self.error = None;
        self.pending_start = None;
        if !self.queue.is_empty() {
            self.start_current(q.start_ms.unwrap_or(0), q.play).await;
        } else if station {
            // The station's first tracks come from `next-tracks`.
            self.pending_start = Some(PendingStart::First { play: q.play });
            self.set_status(Status::Opening);
            self.maybe_refill();
        } else {
            self.set_status(Status::Idle);
        }
        Ok(())
    }

    async fn remove(&mut self, uids: &[u64]) {
        let Some(cur) = self.queue.current_index() else {
            return;
        };
        let current_uid = self.queue.items()[cur].uid;
        if !uids.contains(&current_uid) {
            self.queue.remove(uids);
            return;
        }
        let play = self.track.as_ref().is_some_and(|t| t.want_playing);
        let replaced = self.queue.items()[cur + 1..]
            .iter()
            .any(|i| !uids.contains(&i.uid));
        self.leave_current(EndReason::OtherItemChosen, true).await;
        self.queue.remove(uids);
        if self.queue.is_empty() {
            self.set_status(Status::Idle);
        } else if replaced {
            self.start_current(0, play).await;
        } else {
            self.set_status(Status::Stopped);
        }
    }

    async fn play(&mut self) {
        match self.status {
            Status::Paused => {
                let Some(t) = self.track.as_mut() else { return };
                t.want_playing = true;
                t.touch_at = None;
                let position = t.position_ms;
                match (t.load, t.session.is_some() && !t.session_lost) {
                    (Some((deck, _)), true) => {
                        match deck_call(self.ports.decks[deck.index()].play(), DECK_CALL_TIMEOUT)
                            .await
                        {
                            Ok(()) => self.resumed(),
                            Err(DeckError::SessionGone) => self.reopen(position).await,
                            Err(e) => warn!("player: play: {e}"),
                        }
                    }
                    // Restored, `play: false`, or the engine dropped the session in a long pause.
                    _ => self.reopen(position).await,
                }
            }
            Status::Opening | Status::Loading => {
                if let Some(t) = self.track.as_mut() {
                    t.want_playing = true;
                    if let (Some((deck, _)), Status::Loading) = (t.load, self.status)
                        && let Err(e) =
                            deck_call(self.ports.decks[deck.index()].play(), DECK_CALL_TIMEOUT)
                                .await
                    {
                        warn!("player: play: {e}");
                    }
                }
            }
            Status::Stopped | Status::Failed | Status::Idle => {
                if self.queue.current().is_some() {
                    self.failures = 0;
                    self.error = None;
                    self.leave_current(EndReason::OtherItemChosen, true).await;
                    self.start_current(0, true).await;
                }
            }
            Status::Playing => {}
        }
    }

    /// The deck plays again after a pause.
    fn resumed(&mut self) {
        let now_ms = self.ports.clock.now_ms();
        if let Some(t) = self.track.as_mut() {
            t.last_tick = Some(Instant::now());
            t.position_at = now_ms;
        }
        self.set_status(Status::Playing);
        self.report_start();
    }

    async fn pause(&mut self) {
        self.refresh_position().await;
        let now = Instant::now();
        let Some(t) = self.track.as_mut() else { return };
        if !t.want_playing {
            return; // A repeated pause sends nothing (transport dedupe).
        }
        t.want_playing = false;
        // Opening: the load will start paused.
        if let (Status::Playing | Status::Loading, Some((deck, _))) = (self.status, t.load) {
            t.account_played(now);
            t.last_tick = None;
            t.touch_at = Some(now + KEEPALIVE_EVERY);
            if let Err(e) =
                deck_call(self.ports.decks[deck.index()].pause(), DECK_CALL_TIMEOUT).await
            {
                warn!("player: pause: {e}");
            }
            self.report_end(EndReason::ManuallyPaused);
            self.set_status(Status::Paused);
        }
    }

    async fn next(&mut self) {
        let Some(i) = self.queue.next_index(Advance::Manual) else {
            return;
        };
        let play = self.track.as_ref().is_some_and(|t| t.want_playing);
        self.leave_current(EndReason::SkippedForwards, true).await;
        self.queue.advance_to(i);
        self.start_current(0, play).await;
    }

    async fn previous(&mut self) {
        let play = self.track.as_ref().is_some_and(|t| t.want_playing);
        match self.queue.previous_index() {
            Some(i) if !previous_restarts(self.position()) => {
                self.leave_current(EndReason::SkippedBackwards, true).await;
                self.queue.advance_to(i);
                self.start_current(0, play).await;
            }
            _ if self.track.is_some() => self.seek_to(0, EndReason::SkippedBackwards).await,
            _ => {}
        }
    }

    /// Seeks the current track. A playing segment ends with `reason` and a new one starts at
    /// the target. Before the deck has the media, the target becomes the load's start.
    async fn seek_to(&mut self, target: u64, reason: EndReason) {
        let now_ms = self.ports.clock.now_ms();
        let Some(t) = self.track.as_mut() else { return };
        let target = if t.duration_ms > 0 {
            target.min(t.duration_ms)
        } else {
            target
        };
        let loaded = matches!(
            self.status,
            Status::Playing | Status::Paused | Status::Loading
        ) && t.session.is_some()
            && !t.session_lost;
        let Some((deck, _)) = t.load.filter(|_| loaded) else {
            t.start_ms = target;
            t.position_ms = target;
            t.position_at = now_ms;
            return;
        };
        let playing = self.status == Status::Playing;
        if playing {
            self.refresh_position().await;
            self.report_end(reason);
        }
        match deck_call(
            self.ports.decks[deck.index()].seek(target),
            DECK_CALL_TIMEOUT,
        )
        .await
        {
            Ok(()) => {
                let t = self.track.as_mut().expect("track checked above");
                t.position_ms = target;
                t.position_at = now_ms;
                t.seek_guard = Some((target, Instant::now() + SEEK_REWIND_WINDOW));
            }
            Err(DeckError::SessionGone) => return self.reopen(target).await,
            // Outside the downloaded bytes, or a failed set_time: reload at the target.
            Err(e) => {
                debug!("player: seek by reload: {e}");
                self.reload(target).await;
            }
        }
        if playing {
            self.report_start();
        }
    }

    async fn set_volume(&mut self, level: u8, muted: Option<bool>) -> Result<(), CommandError> {
        if level > 100 {
            return Err(CommandError::BadVolume(level));
        }
        self.volume = level;
        // Raising the volume unmutes (renderer volume setter).
        self.muted = muted.unwrap_or(self.muted && level == 0);
        self.volume_set_at = Some(Instant::now());
        if let Some(deck) = self.track.as_ref().and_then(|t| t.load).map(|(d, _)| d)
            && let Err(e) = deck_call(
                self.ports.decks[deck.index()].set_volume(level, self.muted),
                DECK_CALL_TIMEOUT,
            )
            .await
        {
            warn!("player: volume: {e}");
        }
        if level > 0 {
            self.save_prefs().await;
        }
        Ok(())
    }

    /// Whether a load in `mode` shows video.
    fn shows_video(&self, mode: OpenMode) -> bool {
        matches!(mode, OpenMode::Video { .. }) && !self.mv_audio_only
    }

    async fn set_mv_audio_only(&mut self, audio_only: bool) {
        if self.mv_audio_only == audio_only {
            return;
        }
        self.mv_audio_only = audio_only;
        self.save_prefs().await;
        // A music video pre-rolled for the other choice is unloaded; the next pre-roll loads it again.
        let unload = self.prewarm.as_mut().and_then(|p| {
            if !matches!(
                (p.mode, &p.state),
                (OpenMode::Video { .. }, PrewarmState::Loaded { .. })
            ) {
                return None;
            }
            let PrewarmState::Loaded { session, deck, .. } =
                std::mem::replace(&mut p.state, PrewarmState::Failed)
            else {
                unreachable!("matched above")
            };
            p.state = PrewarmState::Ready(session);
            Some(deck)
        });
        if let Some(deck) = unload {
            self.stop_deck(deck).await;
        }
        // The music video that plays switches now, at its position.
        let loaded = matches!(
            self.status,
            Status::Playing | Status::Paused | Status::Loading
        ) && self.track.as_ref().is_some_and(|t| {
            matches!(t.mode, OpenMode::Video { .. })
                && t.load.is_some()
                && t.session.is_some()
                && !t.session_lost
        });
        if loaded {
            self.refresh_position().await;
            let at = self.position();
            info!(
                audio_only,
                at, "player: music video reloads with the new video choice"
            );
            self.reload(at).await;
        }
    }

    async fn apply_settings(&mut self, p: SettingsPatch) -> Result<(), CommandError> {
        if let Some(h) = p.mv_max_height
            && !MV_HEIGHTS.contains(&h)
        {
            return Err(CommandError::BadHeight(h));
        }
        let s = &mut self.settings;
        s.quality = p.quality.unwrap_or(s.quality);
        s.lossless_enabled = p.lossless_enabled.unwrap_or(s.lossless_enabled);
        s.mv_max_height = p.mv_max_height.unwrap_or(s.mv_max_height);
        s.crossfade = p.crossfade.unwrap_or(s.crossfade);
        s.sound_check = p.sound_check.unwrap_or(s.sound_check);
        s.history_enabled = p.history_enabled.unwrap_or(s.history_enabled);
        s.report_plays = p.report_plays.unwrap_or(s.report_plays);
        s.resume_on_launch = p.resume_on_launch.unwrap_or(s.resume_on_launch);
        if !self.settings.resume_on_launch {
            let _ = self.ports.store.delete(StoreKey::Snapshot).await;
        }
        self.save_prefs().await;
        // The new quality applies at the next track start; a pre-warmed session of the old
        // mode is dropped when it does not match.
        self.check_prewarm().await;
        Ok(())
    }

    fn hover(&mut self, h: Hover) {
        let busy = self.queue.current().is_some_and(|c| c.id == h.id)
            || self.hover.iter().any(|e| e.id == h.id)
            || self.hover_inflight.contains(&h.id);
        if busy {
            return;
        }
        let mode = choose_mode(h.kind, &TrackMeta::default(), &self.settings, self.caps);
        self.hover_inflight.push(h.id.clone());
        let opener = self.ports.opener.clone();
        let req = OpenRequest {
            id: h.id.clone(),
            kind: h.kind,
            mode,
        };
        self.spawn(async move {
            let result = opener.open(req, CancellationToken::new()).await;
            Internal::HoverOpened {
                id: h.id,
                kind: h.kind,
                mode,
                result,
            }
        });
    }

    // ── Track lifecycle ─────────────────────────────────────────────────────

    /// Ends the current track: closes its play report, records history, stops its deck
    /// (unless a gapless hand-over stops it after the next one started) and releases its session.
    async fn leave_current(&mut self, reason: EndReason, stop_deck: bool) {
        self.refresh_position().await;
        self.report_end(reason);
        let Some(mut t) = self.track.take() else {
            return;
        };
        t.cancel.cancel();
        t.account_played(Instant::now());
        if t.played >= HISTORY_MIN_PLAYED || t.natural_end {
            let item = self.queue.items().iter().find(|i| i.uid == t.uid).cloned();
            if let Some(item) = item {
                let entry = HistoryEntry {
                    id: item.id,
                    kind: item.kind,
                    meta: item.meta,
                    played_at: self.ports.clock.now_ms(),
                };
                if self.history.push(entry) {
                    self.history_changed().await;
                }
            }
        }
        if stop_deck && let Some((deck, _)) = t.load {
            self.stop_deck(deck).await;
        }
        if let Some(s) = t.session {
            self.release(s.id);
        }
    }

    async fn history_changed(&self) {
        let entries = self.history.entries().to_vec();
        self.history_tx.send_replace(entries.clone());
        self.emit(Event::History(entries));
        self.save(StoreKey::History, self.history.to_json()).await;
    }

    /// Starts the current queue item as a new generation. `play: false` keeps it paused
    /// without a session; it opens on Play.
    async fn start_current(&mut self, start_ms: u64, play: bool) {
        let Some(item) = self.queue.current().cloned() else {
            self.set_status(Status::Idle);
            return;
        };
        self.generation += 1;
        let mode = choose_mode(item.kind, &item.meta, &self.settings, self.caps);
        self.track = Some(Track::new(
            self.generation,
            &item,
            mode,
            start_ms,
            play,
            self.ports.clock.now_ms(),
        ));
        if play {
            self.open_current().await;
        } else {
            self.set_status(Status::Paused);
        }
    }

    /// Opens the current track: a pre-warmed or hovered session when one matches, else a new open.
    async fn open_current(&mut self) {
        let Some(t) = self.track.as_ref() else { return };
        let (uid, mode, generation, cancel) = (t.uid, t.mode, t.generation, t.cancel.clone());
        let Some(item) = self.queue.current().cloned() else {
            return;
        };
        self.set_status(Status::Opening);
        match self.take_prewarmed(uid, mode) {
            Some(Prewarmed::Session(s)) => return self.opened(s).await,
            Some(Prewarmed::Loaded {
                session,
                deck,
                load,
                video,
            }) => return self.adopt_loaded(session, deck, load, video).await,
            None => {}
        }
        if let Some(i) = self
            .hover
            .iter()
            .position(|e| e.id == item.id && e.kind == item.kind && e.mode == mode)
        {
            let entry = self.hover.remove(i).expect("index from position");
            return self.opened(entry.session).await;
        }
        self.spawn_open(
            generation,
            OpenRequest {
                id: item.id,
                kind: item.kind,
                mode,
            },
            cancel,
        );
    }

    /// Opens a new session for the current track at `from_ms` (session gone, long pause, restore).
    async fn reopen(&mut self, from_ms: u64) {
        let Some(t) = self.track.as_mut() else { return };
        if let Some(s) = t.session.take() {
            self.release(s.id);
        }
        // The quality is chosen again: a restored queue is built before FairPlay is ready, and
        // the setting can change in a pause. A track that fell back to AAC stays there.
        let mode = self
            .queue
            .current()
            .map(|item| choose_mode(item.kind, &item.meta, &self.settings, self.caps));
        let t = self.track.as_mut().expect("track checked above");
        if let Some(mode) = mode.filter(|_| !t.fell_back) {
            t.mode = mode;
            t.wanted_lossless = mode.is_lossless();
        }
        t.session_lost = false;
        t.start_ms = from_ms;
        t.position_ms = from_ms;
        t.want_playing = true;
        self.open_current().await;
    }

    fn take_prewarmed(&mut self, uid: u64, mode: OpenMode) -> Option<Prewarmed> {
        let p = self.prewarm.take_if(|p| p.uid == uid && p.mode == mode)?;
        match p.state {
            PrewarmState::Ready(s) => Some(Prewarmed::Session(s)),
            PrewarmState::Loaded {
                session,
                deck,
                load,
                video,
            } => Some(Prewarmed::Loaded {
                session,
                deck,
                load,
                video,
            }),
            PrewarmState::Opening(cancel) => {
                // The engine joins an open in flight for the same asset; cancelling ours lets
                // the track's own open take the lead.
                cancel.cancel();
                None
            }
            PrewarmState::Waiting | PrewarmState::Failed => None,
        }
    }

    fn set_session(&mut self, session: OpenedSession) {
        let t = self.track.as_mut().expect("callers hold a track");
        if session.duration_ms > 0 {
            t.duration_ms = session.duration_ms;
        }
        let fell_back = t.fell_back || (t.wanted_lossless && session.codec == Codec::Aac);
        t.quality = Some(quality_of(&session, fell_back));
        t.session = Some(session);
    }

    /// The deck for a new load: the idle one. A pre-roll sitting there is unloaded.
    fn claim_idle_deck(&mut self) -> DeckId {
        let deck = self.active.map_or(DeckId::A, DeckId::other);
        if let Some(p) = self.prewarm.as_mut()
            && let PrewarmState::Loaded { deck: d, .. } = &p.state
            && *d == deck
        {
            let PrewarmState::Loaded { session, .. } =
                std::mem::replace(&mut p.state, PrewarmState::Failed)
            else {
                unreachable!()
            };
            p.state = PrewarmState::Ready(session);
        }
        deck
    }

    /// A session for the current track is open: load it into the idle deck.
    async fn opened(&mut self, session: OpenedSession) {
        self.set_session(session);
        let deck = self.claim_idle_deck();
        self.load_into(deck).await;
    }

    /// Loads the current session into `deck` at the track's start position.
    async fn load_into(&mut self, deck: DeckId) {
        let load = self.new_load();
        let (volume, muted) = (self.volume, self.muted);
        let video = self
            .track
            .as_ref()
            .is_some_and(|t| self.shows_video(t.mode));
        let t = self.track.as_mut().expect("callers hold a track");
        let Some(session) = t.session.as_ref() else {
            return;
        };
        let req = LoadRequest {
            load,
            session: session.id.clone(),
            start_ms: t.start_ms,
            paused: !t.want_playing,
            volume,
            muted,
            video,
        };
        t.load = Some((deck, load));
        // The new load reports its own ring and its own position.
        t.video = None;
        t.deck_position = None;
        t.position_ms = t.start_ms;
        self.volume_set_at = Some(Instant::now());
        let result = deck_call(self.ports.decks[deck.index()].load(req), DECK_LOAD_TIMEOUT).await;
        self.active = Some(deck);
        let t = self.track.as_mut().expect("callers hold a track");
        match result {
            Ok(()) if t.want_playing => self.set_status(Status::Loading),
            Ok(()) => {
                t.touch_at = Some(Instant::now() + KEEPALIVE_EVERY);
                self.set_status(Status::Paused);
            }
            Err(DeckError::SessionGone) if !t.reopened => {
                // The engine dropped the session (a hover or pre-warm one that expired): open anew.
                t.reopened = true;
                if let Some(s) = t.session.take() {
                    self.release(s.id);
                }
                let t = self.track.as_ref().expect("callers hold a track");
                let item = self
                    .queue
                    .current()
                    .cloned()
                    .expect("a track has a queue item");
                let req = OpenRequest {
                    id: item.id,
                    kind: item.kind,
                    mode: t.mode,
                };
                self.spawn_open(t.generation, req, t.cancel.clone());
                self.set_status(Status::Opening);
            }
            Err(e) => {
                self.track_failed(ErrorCode::PlaybackFailed, format!("load: {e}"))
                    .await
            }
        }
    }

    /// The next track was pre-rolled in the idle deck: take it over.
    async fn adopt_loaded(
        &mut self,
        session: OpenedSession,
        deck: DeckId,
        load: LoadId,
        video: Option<VideoSurface>,
    ) {
        self.set_session(session);
        let (volume, muted) = (self.volume, self.muted);
        let t = self.track.as_mut().expect("callers hold a track");
        t.load = Some((deck, load));
        t.video = video;
        let want = t.want_playing;
        self.active = Some(deck);
        self.volume_set_at = Some(Instant::now());
        let d = self.ports.decks[deck.index()].clone();
        if let Err(e) = deck_call(d.set_volume(volume, muted), DECK_CALL_TIMEOUT).await {
            warn!("player: volume: {e}");
        }
        if want {
            self.set_status(Status::Loading);
            if let Err(e) = deck_call(d.play(), DECK_CALL_TIMEOUT).await {
                self.track_failed(ErrorCode::PlaybackFailed, format!("play: {e}"))
                    .await;
            }
        } else {
            if let Some(t) = self.track.as_mut() {
                t.touch_at = Some(Instant::now() + KEEPALIVE_EVERY);
            }
            self.set_status(Status::Paused);
        }
    }

    /// Reloads the current session at `from_ms` on its deck (end retry, seek fallback).
    async fn reload(&mut self, from_ms: u64) {
        let Some(t) = self.track.as_mut() else { return };
        t.start_ms = from_ms;
        let deck = t
            .load
            .map_or_else(|| self.active.unwrap_or(DeckId::A), |(d, _)| d);
        self.load_into(deck).await;
    }

    async fn open_failed(&mut self, err: OpenError) {
        let Some(t) = self.track.as_mut() else { return };
        if err.class == OpenClass::Cancelled {
            return;
        }
        if t.mode.is_lossless() && err.class.falls_back_to_aac() && !t.fell_back {
            info!("player: lossless open failed ({err}), playing AAC");
            t.fell_back = true;
            t.mode = OpenMode::Aac;
            let item = self
                .queue
                .current()
                .cloned()
                .expect("a track has a queue item");
            let (generation, cancel) = (t.generation, t.cancel.clone());
            self.spawn_open(
                generation,
                OpenRequest {
                    id: item.id,
                    kind: item.kind,
                    mode: OpenMode::Aac,
                },
                cancel,
            );
            return;
        }
        if err.class == OpenClass::Authentication {
            self.emit(Event::AuthExpired);
        }
        self.track_failed(ErrorCode::OpenFailed, err.message).await;
    }

    /// A track could not be played: move on after 1 s, or give up after three in a row.
    async fn track_failed(&mut self, code: ErrorCode, message: String) {
        self.failures += 1;
        warn!(failures = self.failures, "player: {message}");
        if self.failures >= MAX_CONSECUTIVE_FAILURES {
            self.leave_current(EndReason::FailedToLoad, true).await;
            self.fail(ErrorCode::TooManyFailures, TOO_MANY_FAILURES.to_owned());
            return;
        }
        let err = PlayerError { code, message };
        self.error = Some(err.clone());
        self.emit(Event::Error(err));
        let generation = self.generation;
        self.spawn_after(
            FAILED_ADVANCE_DELAY,
            Internal::AdvanceAfterFailure { generation },
        );
    }

    fn fail(&mut self, code: ErrorCode, message: String) {
        let err = PlayerError { code, message };
        self.error = Some(err.clone());
        self.emit(Event::Error(err));
        self.set_status(Status::Failed);
    }

    // ── Deck events ─────────────────────────────────────────────────────────

    async fn deck_signal(&mut self, s: DeckSignal) {
        let current = self.track.as_ref().and_then(|t| t.load) == Some((s.deck, s.load));
        if let (false, DeckEvent::Video(v)) = (current, &s.event)
            && let Some(Prewarm {
                state:
                    PrewarmState::Loaded {
                        deck, load, video, ..
                    },
                ..
            }) = self.prewarm.as_mut()
            && (*deck, *load) == (s.deck, s.load)
        {
            // The pre-rolled music video showed its first frame: kept for the hand-over.
            *video = v.clone();
            return;
        }
        if !current {
            debug!(?s, "player: event of a replaced load dropped");
            return;
        }
        match s.event {
            DeckEvent::Playing => self.on_playing(),
            DeckEvent::Ended { source_eof } => self.on_end(EndKind::Ended, source_eof).await,
            DeckEvent::Error => self.on_end(EndKind::Error, false).await,
            // libvlc goes playing → ended → stopped; a stop nobody asked for is an end too.
            DeckEvent::Stopped if matches!(self.status, Status::Playing | Status::Loading) => {
                self.on_end(EndKind::Ended, false).await;
            }
            DeckEvent::Stopped => {}
            DeckEvent::Video(v) => {
                if let Some(t) = self.track.as_mut() {
                    t.video = v;
                }
            }
        }
    }

    fn on_playing(&mut self) {
        let now_ms = self.ports.clock.now_ms();
        let Some(t) = self.track.as_mut() else { return };
        if !t.want_playing || self.status == Status::Playing {
            return;
        }
        t.last_tick = Some(Instant::now());
        t.position_at = now_ms;
        let first = !t.started;
        t.started = true;
        self.set_status(Status::Playing);
        if first {
            let t = self.track.as_mut().expect("track checked above");
            t.started_at = now_ms;
            self.failures = 0;
            self.error = None;
            let started = TrackStarted {
                generation: t.generation,
                item: self
                    .queue
                    .current()
                    .cloned()
                    .expect("a track has a queue item"),
                duration_ms: t.duration_ms,
                quality: t.quality,
                started_at: now_ms,
            };
            self.emit(Event::Track(Box::new(started)));
            self.prewarm_next(PrewarmAt::Start);
            self.maybe_refill();
        }
        self.report_start();
    }

    async fn on_end(&mut self, kind: EndKind, source_eof: bool) {
        let Some((deck, _)) = self.track.as_ref().and_then(|t| t.load) else {
            return;
        };
        let timing = self.ports.decks[deck.index()].timing().await;
        let t = self.track.as_mut().expect("track checked above");
        t.position_ms = t.position_ms.max(timing.position_ms.unwrap_or(0));
        t.account_played(Instant::now());
        match classify_end(
            t.position_ms,
            t.duration_ms,
            kind,
            t.end_retries,
            source_eof,
        ) {
            EndAction::Retry { from_ms, delay } => {
                t.end_retries += 1;
                t.last_tick = None;
                warn!(
                    at = t.position_ms,
                    from_ms,
                    retry = t.end_retries,
                    "player: false end, reloading"
                );
                let generation = t.generation;
                self.set_status(Status::Loading);
                self.spawn_after(
                    delay,
                    Internal::Reload {
                        generation,
                        from_ms,
                    },
                );
            }
            EndAction::Advance { short_of_reported } => {
                if short_of_reported {
                    info!(
                        at = t.position_ms,
                        length = t.duration_ms,
                        "player: the data ends before the reported length"
                    );
                }
                t.natural_end = true;
                self.natural_end().await;
            }
        }
    }

    async fn natural_end(&mut self) {
        let Some(i) = self.queue.next_index(Advance::Natural) else {
            self.leave_current(EndReason::NaturalEnd, true).await;
            self.set_status(Status::Stopped);
            if self.refill_epoch == Some(self.queue.epoch()) {
                self.pending_start = Some(PendingStart::Next);
            }
            return;
        };
        let next = self.queue.items()[i].clone();
        let mode = choose_mode(next.kind, &next.meta, &self.settings, self.caps);
        let preloaded = self.prewarm.as_ref().is_some_and(|p| {
            p.uid == next.uid && p.mode == mode && matches!(p.state, PrewarmState::Loaded { .. })
        });
        if !preloaded || self.queue.current_index() == Some(i) {
            self.leave_current(EndReason::NaturalEnd, true).await;
            self.queue.advance_to(i);
            return self.start_current(0, true).await;
        }
        // Gapless hand-over: the next track waits paused in the idle deck. Start it first,
        // then stop the deck that just ended.
        let old = self.active;
        self.leave_current(EndReason::NaturalEnd, false).await;
        self.queue.advance_to(i);
        self.generation += 1;
        self.track = Some(Track::new(
            self.generation,
            &next,
            mode,
            0,
            true,
            self.ports.clock.now_ms(),
        ));
        let Some(Prewarmed::Loaded {
            session,
            deck,
            load,
            video,
        }) = self.take_prewarmed(next.uid, mode)
        else {
            unreachable!("checked above")
        };
        self.adopt_loaded(session, deck, load, video).await;
        if let Some(old) = old.filter(|&d| d != deck) {
            self.stop_deck(old).await;
        }
    }

    // ── Ticks ───────────────────────────────────────────────────────────────

    fn ticking(&self) -> bool {
        matches!(self.status, Status::Playing | Status::Loading)
            && self.track.as_ref().is_some_and(|t| t.load.is_some())
    }

    async fn tick(&mut self) {
        let Some((deck, _)) = self.track.as_ref().and_then(|t| t.load) else {
            return;
        };
        let timing = self.ports.decks[deck.index()].timing().await;
        let now = Instant::now();
        let now_ms = self.ports.clock.now_ms();
        self.follow_mixer(timing.volume, timing.muted).await;
        let playing = self.status == Status::Playing;
        let Some(t) = self.track.as_mut() else { return };
        // libvlc reports -1/0 before the first decoded sample: keep the last position.
        let reported = timing.position_ms.filter(|&p| p > 0);
        let moved = reported.is_some() && reported != t.deck_position;
        // libvlc says playing before it has buffered (seconds for a music video that downloads):
        // until the deck reports a position, the track stands at its start, so the UI's
        // interpolation from `positionAt` does not run ahead. The first position is published
        // at once.
        let first = moved && t.deck_position.is_none();
        if playing && reported.is_none() && t.deck_position.is_none() {
            t.position_at = now_ms;
        }
        if let Some(p) = reported.filter(|_| moved) {
            t.deck_position = Some(p);
            if t.follows_deck(p, timing.source_ended) {
                t.position_ms = p;
                t.position_at = now_ms;
            }
        }
        if !t.stream_length_seen
            && let Some(len) = timing.length_ms.filter(|&l| l > 0)
        {
            t.stream_length_seen = true;
            t.duration_ms = track_length_ms(Some(len), t.duration_ms);
        }
        t.duration_ms = length_after_overrun(t.position_ms, t.duration_ms);
        if !playing {
            return;
        }
        let elapsed = t.account_played(now);
        if reported.is_some() && !moved {
            // libvlc's position steps every 250 ms, and stops for good when its demuxer reaches
            // the end of the stream, seconds before the audio ends. Run on the clock meanwhile,
            // so the last seconds of a track still count down and the pre-roll gets its turn.
            let estimate = t.position_ms + u64::try_from(elapsed.as_millis()).unwrap_or(0);
            t.position_ms = if t.duration_ms > 0 {
                estimate.min(t.duration_ms)
            } else {
                estimate
            };
            t.position_at = now_ms;
        }
        let mut rewound = None;
        if let Some((target, until)) = t.seek_guard {
            if now > until {
                t.seek_guard = None;
            } else if t.position_ms + SEEK_REWIND_MS < target {
                warn!(
                    target,
                    at = t.position_ms,
                    "player: libvlc rewound after a seek, reloading at the target"
                );
                t.seek_guard = None;
                rewound = Some(target);
            }
        }
        let scrobble =
            !t.scrobbled && scrobble_after(t.duration_ms).is_some_and(|after| t.played >= after);
        let remaining = t.duration_ms.saturating_sub(t.position_ms);
        let has_length = t.duration_ms > 0;
        if scrobble {
            t.scrobbled = true;
            let due = ScrobbleDue {
                generation: t.generation,
                item: self
                    .queue
                    .current()
                    .cloned()
                    .expect("a track has a queue item"),
                duration_ms: t.duration_ms,
                timestamp: t.started_at / 1000,
            };
            self.emit(Event::ScrobbleDue(Box::new(due)));
        }
        if let Some(target) = rewound {
            self.reload(target).await;
            return;
        }
        if first || now - self.last_position_event >= POSITION_EVERY {
            self.last_position_event = now;
            let state = self.build_state();
            self.emit(Event::Position(Position {
                generation: state.generation,
                uid: state.track.as_ref().map_or(0, |i| i.uid),
                position_ms: state.position_ms,
                duration_ms: state.duration_ms,
                position_at: state.position_at,
            }));
            self.state_tx.send_replace(state);
        }
        if now - self.last_snapshot >= SNAPSHOT_EVERY {
            self.save_snapshot().await;
        }
        if has_length && remaining <= AAC_PREWARM_LEAD_MS {
            self.prewarm_next(PrewarmAt::Tail);
        }
        if has_length && remaining <= PREROLL_MS {
            self.preroll().await;
        }
    }

    /// Follows the desktop mixer: a deck volume the player did not set is adopted and published.
    async fn follow_mixer(&mut self, volume: u8, muted: bool) {
        if self
            .volume_set_at
            .is_some_and(|at| at.elapsed() < VOLUME_ECHO_WINDOW)
        {
            return;
        }
        if (volume, muted) == (self.volume, self.muted) {
            return;
        }
        self.volume = volume.min(100);
        self.muted = muted;
        if volume > 0 {
            self.save_prefs().await;
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let touch = self.track.as_ref().and_then(|t| t.touch_at);
        match (touch, self.flush_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    async fn timers(&mut self) {
        let now = Instant::now();
        if self.flush_at.is_some_and(|at| at <= now) {
            self.flush_at = None;
            self.flush();
        }
        let Some(t) = self.track.as_mut() else { return };
        if t.touch_at.is_some_and(|at| at <= now) {
            t.touch_at = Some(now + KEEPALIVE_EVERY);
            if let Some(session) = t.session.as_ref().filter(|_| !t.session_lost) {
                let (opener, id, generation) =
                    (self.ports.opener.clone(), session.id.clone(), t.generation);
                self.spawn(async move {
                    Internal::Touched {
                        generation,
                        result: opener.touch(&id).await,
                    }
                });
            }
        }
    }

    // ── Pre-warm, pre-roll, refill ──────────────────────────────────────────

    /// The next item of a natural end and the mode it would open in (none with Repeat One).
    fn prewarm_target(&self) -> Option<(QueueItem, OpenMode)> {
        let i = self
            .queue
            .next_index(Advance::Natural)
            .filter(|&i| Some(i) != self.queue.current_index())?;
        let item = self.queue.items()[i].clone();
        let mode = choose_mode(item.kind, &item.meta, &self.settings, self.caps);
        Some((item, mode))
    }

    fn prewarm_next(&mut self, at: PrewarmAt) {
        let Some((item, mode)) = self.prewarm_target() else {
            return;
        };
        let due = match mode {
            OpenMode::Lossless { .. } | OpenMode::Video { .. } => at == PrewarmAt::Start,
            OpenMode::Aac => at == PrewarmAt::Tail,
        };
        // A pre-warm for another item or mode was already dropped by `check_prewarm`.
        if !due || self.prewarm.is_some() {
            return;
        }
        debug!(
            id = item.id.as_str(),
            ?mode,
            "player: pre-warming the next track"
        );
        let cancel = CancellationToken::new();
        let uid = item.uid;
        self.spawn_prewarm(item, mode, cancel.clone());
        self.prewarm = Some(Prewarm {
            uid,
            mode,
            state: PrewarmState::Opening(cancel),
            retries: 0,
        });
    }

    fn spawn_prewarm(&self, item: QueueItem, mode: OpenMode, cancel: CancellationToken) {
        let opener = self.ports.opener.clone();
        let uid = item.uid;
        let req = OpenRequest {
            id: item.id,
            kind: item.kind,
            mode,
        };
        self.spawn(async move {
            Internal::PrewarmOpened {
                uid,
                mode,
                result: opener.open(req, cancel).await,
            }
        });
    }

    /// Drops a pre-warm that is no longer for the next item (or no longer in the right mode).
    async fn check_prewarm(&mut self) {
        let target = self.prewarm_target();
        let stale = self.prewarm.as_ref().is_some_and(|p| {
            !target
                .as_ref()
                .is_some_and(|(item, mode)| item.uid == p.uid && *mode == p.mode)
        });
        if !stale {
            return;
        }
        let Some(p) = self.prewarm.take() else { return };
        match p.state {
            PrewarmState::Opening(cancel) => cancel.cancel(),
            PrewarmState::Ready(s) => self.release(s.id),
            PrewarmState::Loaded { session, deck, .. } => {
                if Some(deck) != self.track.as_ref().and_then(|t| t.load).map(|(d, _)| d) {
                    self.stop_deck(deck).await;
                }
                self.release(session.id);
            }
            PrewarmState::Waiting | PrewarmState::Failed => {}
        }
    }

    /// Loads the pre-warmed next session paused at 0 with volume 0 into the idle deck.
    async fn preroll(&mut self) {
        let Some(active) = self.active else { return };
        let deck = active.other();
        let load = self.new_load();
        let video = self
            .prewarm
            .as_ref()
            .is_some_and(|p| self.shows_video(p.mode));
        let Some(p) = self.prewarm.as_mut() else {
            return;
        };
        let PrewarmState::Ready(session) = &p.state else {
            return;
        };
        let req = LoadRequest {
            load,
            session: session.id.clone(),
            start_ms: 0,
            paused: true,
            volume: 0,
            muted: false,
            video,
        };
        let result = deck_call(self.ports.decks[deck.index()].load(req), DECK_LOAD_TIMEOUT).await;
        let Some(p) = self.prewarm.as_mut() else {
            return;
        };
        match (
            result,
            std::mem::replace(&mut p.state, PrewarmState::Failed),
        ) {
            (Ok(()), PrewarmState::Ready(session)) => {
                p.state = PrewarmState::Loaded {
                    session,
                    deck,
                    load,
                    video: None,
                }
            }
            (Err(e), PrewarmState::Ready(session)) => {
                warn!("player: pre-roll: {e}");
                p.state = PrewarmState::Ready(session);
            }
            (_, other) => p.state = other,
        }
    }

    /// Asks for more tracks when the queue runs low (autoplay, stations). Needs a sign-in.
    fn maybe_refill(&mut self) {
        if !self.caps.signed_in || self.refill_epoch == Some(self.queue.epoch()) {
            return;
        }
        let Some(refill) = self.queue.refill() else {
            return;
        };
        let epoch = self.queue.epoch();
        self.refill_epoch = Some(epoch);
        let catalog = self.ports.catalog.clone();
        self.spawn(async move {
            let (source, result) = match refill {
                Refill::Station { id } => (
                    ItemSource::Station,
                    catalog.station_next_tracks(&id, STATION_BATCH).await,
                ),
                Refill::Autoplay { seeds } => (
                    ItemSource::Autoplay,
                    catalog.autoplay_tracks(&seeds, AUTOPLAY_CAP).await,
                ),
            };
            Internal::Refilled {
                epoch,
                source,
                result,
            }
        });
    }

    // ── Play activity ───────────────────────────────────────────────────────

    fn reporting(&self) -> bool {
        self.settings.report_plays && self.caps.signed_in
    }

    fn play_mode(&self) -> PlayMode {
        PlayMode::new(
            self.queue.autoplay(),
            self.queue.repeat(),
            self.queue.shuffle(),
        )
    }

    /// Opens a play segment at the current position (`PLAY_START`).
    fn report_start(&mut self) {
        if !self.reporting() {
            return;
        }
        let now_ms = self.ports.clock.now_ms();
        let play_mode = self.play_mode();
        let Some(t) = self.track.as_mut() else { return };
        if t.segment.is_some() {
            return;
        }
        t.segment = Some(t.position_ms);
        let (uid, position, duration) = (t.uid, t.position_ms, t.duration_ms);
        let Some(item) = self.queue.items().iter().find(|i| i.uid == uid) else {
            return;
        };
        let ctx = ReportContext {
            item,
            container: self.queue.context(),
            duration_ms: duration,
            play_mode,
        };
        let event = ctx.start(position, now_ms);
        self.outbox.push(event);
        self.flush_at
            .get_or_insert(Instant::now() + REPORT_FLUSH_EVERY);
    }

    /// Closes the open play segment (`PLAY_END`) at the current position and flushes.
    fn report_end(&mut self, reason: EndReason) {
        self.close_segment(reason);
        self.flush();
    }

    fn close_segment(&mut self, reason: EndReason) {
        let now_ms = self.ports.clock.now_ms();
        let play_mode = self.play_mode();
        let Some(t) = self.track.as_mut() else { return };
        let Some(start) = t.segment.take() else {
            return;
        };
        let (uid, end, duration) = (t.uid, t.position_ms, t.duration_ms);
        let Some(item) = self.queue.items().iter().find(|i| i.uid == uid) else {
            return;
        };
        let ctx = ReportContext {
            item,
            container: self.queue.context(),
            duration_ms: duration,
            play_mode,
        };
        let event = ctx.end(start, end, reason, now_ms);
        self.outbox.push(event);
    }

    /// Reads the playing deck's position, so a report or a seek starts from where the audio is.
    async fn refresh_position(&mut self) {
        if self.status != Status::Playing {
            return;
        }
        let Some((deck, _)) = self.track.as_ref().and_then(|t| t.load) else {
            return;
        };
        let timing = self.ports.decks[deck.index()].timing().await;
        let now_ms = self.ports.clock.now_ms();
        if let (Some(t), Some(p)) = (self.track.as_mut(), timing.position_ms.filter(|&p| p > 0))
            && t.follows_deck(p, timing.source_ended)
        {
            t.position_ms = p;
            t.position_at = now_ms;
        }
    }

    fn flush(&mut self) {
        if self.outbox.is_empty() {
            return;
        }
        self.flush_at = None;
        let batch = self.outbox.take();
        let reporter = self.ports.reporter.clone();
        self.spawn(async move {
            match reporter.report(&batch).await {
                Ok(()) => Internal::Reported(Vec::new()),
                Err(e) => {
                    warn!("player: {e}");
                    Internal::Reported(batch)
                }
            }
        });
    }

    // ── Results of spawned work ─────────────────────────────────────────────

    fn is_current(&self, generation: u64) -> bool {
        self.track
            .as_ref()
            .is_some_and(|t| t.generation == generation)
    }

    async fn internal(&mut self, msg: Internal) {
        match msg {
            Internal::Opened { generation, result } => {
                if !self.is_current(generation) {
                    if let Ok(s) = result {
                        self.release(s.id);
                    }
                    return;
                }
                match result {
                    Ok(session) => self.opened(session).await,
                    Err(e) => self.open_failed(e).await,
                }
            }
            Internal::Reload {
                generation,
                from_ms,
            } => {
                if self.is_current(generation) {
                    self.reload(from_ms).await;
                }
            }
            Internal::AdvanceAfterFailure { generation } => {
                if !self.is_current(generation) {
                    return;
                }
                match self.queue.next_index(Advance::Manual) {
                    Some(i) => {
                        self.leave_current(EndReason::FailedToLoad, true).await;
                        self.queue.advance_to(i);
                        self.start_current(0, true).await;
                    }
                    None => {
                        self.leave_current(EndReason::FailedToLoad, true).await;
                        self.set_status(Status::Failed);
                    }
                }
            }
            Internal::PrewarmOpened { uid, mode, result } => self.prewarm_opened(uid, mode, result),
            Internal::PrewarmRetry { uid } => {
                let target = self.prewarm_target();
                let Some(p) = self
                    .prewarm
                    .as_mut()
                    .filter(|p| p.uid == uid && matches!(p.state, PrewarmState::Waiting))
                else {
                    return;
                };
                match target.filter(|(item, mode)| item.uid == uid && *mode == p.mode) {
                    Some((item, mode)) => {
                        let cancel = CancellationToken::new();
                        p.state = PrewarmState::Opening(cancel.clone());
                        self.spawn_prewarm(item, mode, cancel);
                    }
                    None => self.prewarm = None,
                }
            }
            Internal::HoverOpened {
                id,
                kind,
                mode,
                result,
            } => {
                self.hover_inflight.retain(|i| *i != id);
                if let Ok(session) = result {
                    if self.hover.len() >= HOVER_CAP
                        && let Some(old) = self.hover.pop_front()
                    {
                        self.release(old.session.id);
                    }
                    self.hover.push_back(HoverEntry {
                        id,
                        kind,
                        mode,
                        session,
                    });
                }
            }
            Internal::Refilled {
                epoch,
                source,
                result,
            } => {
                if self.refill_epoch == Some(epoch) {
                    self.refill_epoch = None;
                }
                let appended = match result {
                    Ok(items) => self.queue.append_refill(epoch, items, source),
                    Err(e) => {
                        warn!("player: refill: {e}");
                        if e == CatalogError::Authentication {
                            self.emit(Event::AuthExpired);
                        }
                        false
                    }
                };
                match (self.pending_start.take(), appended) {
                    (Some(PendingStart::First { play }), true) => self.start_current(0, play).await,
                    (Some(PendingStart::Next), true) => {
                        if let Some(i) = self.queue.next_index(Advance::Manual) {
                            self.queue.advance_to(i);
                            self.start_current(0, true).await;
                        }
                    }
                    (Some(PendingStart::First { .. }), false) => self.set_status(Status::Stopped),
                    _ => {}
                }
            }
            Internal::Touched { generation, result } => {
                if let (true, Err(e)) = (self.is_current(generation), result)
                    && e.class == OpenClass::NotFound
                    && let Some(t) = self.track.as_mut()
                {
                    info!(
                        at = t.position_ms,
                        "player: session expired during the pause, reopening on play"
                    );
                    t.session_lost = true;
                }
            }
            Internal::Reported(batch) => {
                if !batch.is_empty() {
                    self.outbox.requeue(batch);
                    self.flush_at
                        .get_or_insert(Instant::now() + REPORT_FLUSH_EVERY);
                }
            }
        }
    }

    fn prewarm_opened(
        &mut self,
        uid: u64,
        mode: OpenMode,
        result: Result<OpenedSession, OpenError>,
    ) {
        let mine = self.prewarm.as_ref().is_some_and(|p| {
            p.uid == uid && p.mode == mode && matches!(p.state, PrewarmState::Opening(_))
        });
        if !mine {
            if let Ok(s) = result {
                self.release(s.id);
            }
            return;
        }
        let p = self.prewarm.as_mut().expect("checked above");
        match result {
            Ok(session) => {
                if mode.is_lossless() {
                    let (opener, id) = (self.ports.opener.clone(), session.id.clone());
                    tokio::spawn(async move { opener.precache(&id).await });
                }
                p.state = PrewarmState::Ready(session);
            }
            Err(e) if e.class.is_transient() && p.retries < PREWARM_RETRIES => {
                p.retries += 1;
                p.state = PrewarmState::Waiting;
                debug!(
                    retry = p.retries,
                    "player: pre-warm failed ({e}), retrying in 5 s"
                );
                self.spawn_after(PREWARM_RETRY_DELAY, Internal::PrewarmRetry { uid });
            }
            Err(e) => {
                debug!("player: pre-warm failed for good: {e}");
                p.state = PrewarmState::Failed;
            }
        }
    }
}
