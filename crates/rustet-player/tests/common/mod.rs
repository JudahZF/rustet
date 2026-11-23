//! Fakes of the player's ports, driven by tokio's paused clock.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rustet_player::activity::PlayActivityEvent;
use rustet_player::model::{
    Capabilities, CatalogId, Codec, Command, Event, ItemKind, PlayerState, QueueItemIn, SetQueue,
    TrackMeta, VideoSurface,
};
use rustet_player::ports::*;
use rustet_player::{Player, PlayerHandle};
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, s: String) {
    log.lock().unwrap().push(s);
}

// ── Opener ──────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FakeOpener {
    pub log: Log,
    /// Scripted results per catalog ID, used in order; afterwards opens succeed.
    pub script: Mutex<HashMap<String, VecDeque<Result<(), OpenClass>>>>,
    pub delay: Mutex<Duration>,
    pub touch_gone: Mutex<bool>,
    next: Mutex<u64>,
}

impl FakeOpener {
    pub fn fail(&self, id: &str, classes: &[OpenClass]) {
        self.script
            .lock()
            .unwrap()
            .entry(id.into())
            .or_default()
            .extend(classes.iter().map(|&c| Err(c)));
    }

    pub fn opens(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("open "))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Opener for FakeOpener {
    async fn open(
        &self,
        req: OpenRequest,
        cancel: CancellationToken,
    ) -> Result<OpenedSession, OpenError> {
        let mode = match req.mode {
            OpenMode::Aac => "aac".to_string(),
            OpenMode::Lossless { hi_res } => if hi_res { "hires" } else { "lossless" }.to_string(),
            OpenMode::Video { max_height } => format!("video{max_height}"),
        };
        push(&self.log, format!("open {} {mode}", req.id.as_str()));
        let delay = *self.delay.lock().unwrap();
        tokio::select! {
            () = cancel.cancelled() => return Err(OpenError { class: OpenClass::Cancelled, message: "cancelled".into() }),
            () = tokio::time::sleep(delay) => {}
        }
        let scripted = self
            .script
            .lock()
            .unwrap()
            .get_mut(req.id.as_str())
            .and_then(VecDeque::pop_front);
        if let Some(Err(class)) = scripted {
            return Err(OpenError {
                class,
                message: format!("{class:?} for {}", req.id.as_str()),
            });
        }
        let n = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            *n
        };
        let mut s = OpenedSession::test(&format!("s{n}"));
        match req.mode {
            OpenMode::Lossless { hi_res } => {
                s.codec = Codec::Alac;
                (s.sample_rate, s.bit_depth) = if hi_res { (96_000, 24) } else { (44_100, 16) };
            }
            OpenMode::Video { max_height } => {
                s.codec = Codec::H264;
                s.video_height = Some(max_height);
            }
            OpenMode::Aac => {}
        }
        Ok(s)
    }

    async fn precache(&self, session: &SessionId) {
        push(&self.log, format!("precache {}", session.0));
    }

    async fn touch(&self, session: &SessionId) -> Result<(), OpenError> {
        push(&self.log, format!("touch {}", session.0));
        if *self.touch_gone.lock().unwrap() {
            return Err(OpenError {
                class: OpenClass::NotFound,
                message: "gone".into(),
            });
        }
        Ok(())
    }

    async fn release(&self, session: &SessionId) {
        push(&self.log, format!("release {}", session.0));
    }
}

// ── Decks ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct DeckState {
    load: Option<LoadId>,
    session: String,
    base_ms: u64,
    since: Option<Instant>,
    ended: bool,
    volume: u8,
    muted: bool,
    /// The mixer volume the deck reports, when it differs from what the player set.
    mixer: Option<(u8, bool)>,
    /// The load shows video (in the ring [`FakeDeck::ring`] names).
    video: bool,
    loaded: Option<Instant>,
    /// The time libvlc stopped at: its demuxer reached the end of the stream.
    source_ended_at: Option<u64>,
}

pub struct FakeDeck {
    pub id: DeckId,
    tx: DeckEvents,
    pub log: Log,
    st: Mutex<DeckState>,
    /// Length of every loaded media.
    pub length_ms: Arc<Mutex<u64>>,
    /// Natural ends report a clean source end.
    pub eof: Mutex<bool>,
    pub stop_hangs: Mutex<bool>,
    pub seek_error: Mutex<Option<DeckError>>,
    /// After a load the deck reports no position for this long (libvlc buffering).
    pub buffering: Mutex<Duration>,
}

impl FakeDeck {
    fn name(&self) -> &'static str {
        match self.id {
            DeckId::A => "A",
            DeckId::B => "B",
        }
    }

    fn emit(&self, load: Option<LoadId>, event: DeckEvent) {
        if let Some(load) = load {
            let _ = self.tx.send(DeckSignal {
                deck: self.id,
                load,
                event,
            });
        }
    }

    fn position(st: &DeckState, length: u64) -> u64 {
        let p = st.base_ms + st.since.map_or(0, |s| s.elapsed().as_millis() as u64);
        let p = st.source_ended_at.map_or(p, |end| p.min(end));
        if length > 0 { p.min(length) } else { p }
    }

    /// The source delivers its last data: libvlc's time stops at `ms` while the audio plays on.
    pub fn end_source_at(&self, ms: u64) {
        self.st.lock().unwrap().source_ended_at = Some(ms);
    }

    /// Ends the current media now, as libvlc would.
    pub fn end(&self, kind: DeckEvent) {
        let mut st = self.st.lock().unwrap();
        let length = *self.length_ms.lock().unwrap();
        st.base_ms = Self::position(&st, length);
        st.since = None;
        st.ended = true;
        let load = st.load;
        drop(st);
        self.emit(load, kind);
    }

    /// Jumps the position (a rewind after a seek, a stall).
    pub fn set_position(&self, ms: u64) {
        let mut st = self.st.lock().unwrap();
        st.base_ms = ms;
        if st.since.is_some() {
            st.since = Some(Instant::now());
        }
    }

    pub fn set_mixer(&self, volume: u8, muted: bool) {
        self.st.lock().unwrap().mixer = Some((volume, muted));
    }

    pub fn current_load(&self) -> Option<LoadId> {
        self.st.lock().unwrap().load
    }

    pub fn session(&self) -> String {
        self.st.lock().unwrap().session.clone()
    }

    pub fn playing(&self) -> bool {
        self.st.lock().unwrap().since.is_some()
    }

    /// The ring a video load of `session` reports.
    pub fn ring(session: &str, load: LoadId) -> VideoSurface {
        VideoSurface {
            ring: format!("/run/rustet/{session}.ring"),
            generation: load.0,
            width: 1280,
            height: 720,
        }
    }

    pub fn send_raw(&self, load: LoadId, event: DeckEvent) {
        let _ = self.tx.send(DeckSignal {
            deck: self.id,
            load,
            event,
        });
    }
}

#[async_trait]
impl Deck for FakeDeck {
    async fn load(&self, req: LoadRequest) -> Result<(), DeckError> {
        let video = if req.video { " video" } else { "" };
        push(
            &self.log,
            format!(
                "{} load {}@{} paused={} vol={}{video}",
                self.name(),
                req.session.0,
                req.start_ms,
                req.paused,
                req.volume
            ),
        );
        let mut st = self.st.lock().unwrap();
        *st = DeckState {
            load: Some(req.load),
            session: req.session.0.clone(),
            base_ms: req.start_ms,
            since: (!req.paused).then(Instant::now),
            ended: false,
            volume: req.volume,
            muted: req.muted,
            mixer: None,
            video: req.video,
            loaded: Some(Instant::now()),
            source_ended_at: None,
        };
        drop(st);
        // libvlc shows the first frame (also of a pre-roll) before it reports playing.
        if req.video {
            self.emit(
                Some(req.load),
                DeckEvent::Video(Some(Self::ring(&req.session.0, req.load))),
            );
        }
        if !req.paused {
            self.emit(Some(req.load), DeckEvent::Playing);
        }
        Ok(())
    }

    async fn play(&self) -> Result<(), DeckError> {
        push(&self.log, format!("{} play", self.name()));
        let mut st = self.st.lock().unwrap();
        if st.since.is_none() {
            st.since = Some(Instant::now());
        }
        let load = st.load;
        drop(st);
        self.emit(load, DeckEvent::Playing);
        Ok(())
    }

    async fn pause(&self) -> Result<(), DeckError> {
        push(&self.log, format!("{} pause", self.name()));
        let mut st = self.st.lock().unwrap();
        let length = *self.length_ms.lock().unwrap();
        st.base_ms = Self::position(&st, length);
        st.since = None;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), DeckError> {
        push(&self.log, format!("{} seek {position_ms}", self.name()));
        if let Some(e) = self.seek_error.lock().unwrap().clone() {
            return Err(e);
        }
        self.set_position(position_ms);
        Ok(())
    }

    async fn stop(&self) -> Result<(), DeckError> {
        push(&self.log, format!("{} stop", self.name()));
        if *self.stop_hangs.lock().unwrap() {
            std::future::pending::<()>().await;
        }
        let mut st = self.st.lock().unwrap();
        st.since = None;
        let (load, video) = (st.load, std::mem::take(&mut st.video));
        drop(st);
        // libvlc reports the stop; the player must treat it as an event of a replaced load.
        if video {
            self.emit(load, DeckEvent::Video(None));
        }
        self.emit(load, DeckEvent::Stopped);
        Ok(())
    }

    async fn set_volume(&self, level: u8, muted: bool) -> Result<(), DeckError> {
        push(
            &self.log,
            format!("{} volume {level} muted={muted}", self.name()),
        );
        let mut st = self.st.lock().unwrap();
        st.volume = level;
        st.muted = muted;
        st.mixer = None;
        Ok(())
    }

    async fn timing(&self) -> DeckTiming {
        let length = *self.length_ms.lock().unwrap();
        let mut st = self.st.lock().unwrap();
        let position = Self::position(&st, length);
        let (volume, muted) = st.mixer.unwrap_or((st.volume, st.muted));
        let buffering = st
            .loaded
            .is_some_and(|at| at.elapsed() < *self.buffering.lock().unwrap());
        let timing = DeckTiming {
            position_ms: (!buffering).then_some(position),
            length_ms: (length > 0).then_some(length),
            source_ended: st.source_ended_at.is_some(),
            volume,
            muted,
        };
        // libvlc reaches the end of the media by itself.
        if st.since.is_some() && length > 0 && position >= length && !st.ended {
            st.ended = true;
            st.since = None;
            st.base_ms = length;
            let load = st.load;
            drop(st);
            self.emit(
                load,
                DeckEvent::Ended {
                    source_eof: *self.eof.lock().unwrap(),
                },
            );
        }
        timing
    }
}

// ── Other ports ─────────────────────────────────────────────────────────────

pub struct FakeClock {
    base_ms: u64,
    start: Instant,
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.base_ms + self.start.elapsed().as_millis() as u64
    }
}

/// The fake wall clock starts here.
pub const T0: u64 = 1_800_000_000_000;

#[derive(Default)]
pub struct FakeCatalog {
    pub log: Log,
    pub station: Mutex<VecDeque<Vec<QueueItemIn>>>,
    pub autoplay: Mutex<VecDeque<Vec<QueueItemIn>>>,
}

#[async_trait]
impl Catalog for FakeCatalog {
    async fn station_next_tracks(
        &self,
        station_id: &str,
        limit: usize,
    ) -> Result<Vec<QueueItemIn>, CatalogError> {
        push(&self.log, format!("next-tracks {station_id} {limit}"));
        Ok(self.station.lock().unwrap().pop_front().unwrap_or_default())
    }

    async fn autoplay_tracks(
        &self,
        seeds: &[CatalogId],
        limit: usize,
    ) -> Result<Vec<QueueItemIn>, CatalogError> {
        let seeds: Vec<&str> = seeds.iter().map(CatalogId::as_str).collect();
        push(&self.log, format!("autoplay {} {limit}", seeds.join(",")));
        Ok(self
            .autoplay
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }
}

#[derive(Default)]
pub struct FakeReporter {
    pub events: Mutex<Vec<PlayActivityEvent>>,
}

#[async_trait]
impl ActivityReporter for FakeReporter {
    async fn report(&self, events: &[PlayActivityEvent]) -> Result<(), ReportError> {
        self.events.lock().unwrap().extend_from_slice(events);
        Ok(())
    }
}

#[derive(Default)]
pub struct MemStore {
    pub files: Mutex<HashMap<StoreKey, Vec<u8>>>,
}

impl MemStore {
    pub fn get(&self, key: StoreKey) -> Option<serde_json::Value> {
        self.files
            .lock()
            .unwrap()
            .get(&key)
            .map(|b| serde_json::from_slice(b).unwrap())
    }
}

#[async_trait]
impl Store for MemStore {
    async fn load(&self, key: StoreKey) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.files.lock().unwrap().get(&key).cloned())
    }
    async fn save(&self, key: StoreKey, bytes: Vec<u8>) -> Result<(), StoreError> {
        self.files.lock().unwrap().insert(key, bytes);
        Ok(())
    }
    async fn delete(&self, key: StoreKey) -> Result<(), StoreError> {
        self.files.lock().unwrap().remove(&key);
        Ok(())
    }
}

#[derive(Default)]
pub struct FakePrewarm {
    pub windows: Mutex<Vec<PrewarmWindow>>,
}

impl PrewarmSink for FakePrewarm {
    fn submit(&self, window: PrewarmWindow) {
        self.windows.lock().unwrap().push(window);
    }
}

// ── Harness ─────────────────────────────────────────────────────────────────

pub struct H {
    pub player: PlayerHandle,
    pub opener: Arc<FakeOpener>,
    pub decks: [Arc<FakeDeck>; 2],
    /// Deck operations of both decks, in order.
    pub log: Log,
    pub length_ms: Arc<Mutex<u64>>,
    pub catalog: Arc<FakeCatalog>,
    pub reporter: Arc<FakeReporter>,
    pub store: Arc<MemStore>,
    pub prewarm: Arc<FakePrewarm>,
    pub events: broadcast::Receiver<Event>,
}

/// A player with a 200 s media length, signed in, without lossless.
pub async fn harness() -> H {
    harness_with(Arc::new(MemStore::default())).await
}

pub async fn harness_with(store: Arc<MemStore>) -> H {
    let (tx, rx) = deck_channel();
    let log: Log = Log::default();
    let length_ms = Arc::new(Mutex::new(200_000));
    let deck = |id| {
        Arc::new(FakeDeck {
            id,
            tx: tx.clone(),
            log: log.clone(),
            st: Mutex::default(),
            length_ms: length_ms.clone(),
            eof: Mutex::new(true),
            stop_hangs: Mutex::new(false),
            seek_error: Mutex::new(None),
            buffering: Mutex::new(Duration::ZERO),
        })
    };
    let decks = [deck(DeckId::A), deck(DeckId::B)];
    let opener = Arc::new(FakeOpener::default());
    let catalog = Arc::new(FakeCatalog::default());
    let reporter = Arc::new(FakeReporter::default());
    let prewarm = Arc::new(FakePrewarm::default());
    let ports = Ports {
        opener: opener.clone(),
        decks: [decks[0].clone(), decks[1].clone()],
        clock: Arc::new(FakeClock {
            base_ms: T0,
            start: Instant::now(),
        }),
        catalog: catalog.clone(),
        reporter: reporter.clone(),
        store: store.clone(),
        prewarm: prewarm.clone(),
    };
    let player = Player::spawn(ports, rx);
    let events = player.subscribe();
    player
        .send(Command::Capabilities(Capabilities {
            lossless_ready: false,
            signed_in: true,
        }))
        .await
        .unwrap();
    H {
        player,
        opener,
        decks,
        log,
        length_ms,
        catalog,
        reporter,
        store,
        prewarm,
        events,
    }
}

pub fn song(id: &str) -> QueueItemIn {
    QueueItemIn {
        id: CatalogId(id.into()),
        library_id: None,
        kind: ItemKind::Song,
        meta: TrackMeta {
            name: format!("Song {id}"),
            artist_name: "Artist".into(),
            duration_ms: 200_000,
            ..TrackMeta::default()
        },
        container_id: None,
    }
}

pub fn songs(ids: &[&str]) -> Vec<QueueItemIn> {
    ids.iter().map(|id| song(id)).collect()
}

pub fn play_now(items: Vec<QueueItemIn>, start: usize) -> Command {
    Command::SetQueue(SetQueue {
        items,
        start_index: start,
        context: None,
        shuffle: None,
        start_ms: None,
        play: true,
    })
}

impl H {
    pub async fn send(&self, c: Command) {
        self.player.send(c).await.unwrap();
    }

    pub fn state(&self) -> PlayerState {
        self.player.state()
    }

    /// Waits (in paused time) until the state matches.
    pub async fn until(&self, what: &str, f: impl Fn(&PlayerState) -> bool) -> PlayerState {
        for _ in 0..60_000 {
            let s = self.player.state();
            if f(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "timed out waiting for {what}; state: {:?} {:?}",
            self.state().status,
            self.state().track.map(|t| t.id)
        );
    }

    pub async fn until_playing(&self, id: &str) -> PlayerState {
        self.until(&format!("{id} playing"), |s| {
            s.status == rustet_player::model::Status::Playing
                && s.track.as_ref().is_some_and(|t| t.id.as_str() == id)
        })
        .await
    }

    pub fn deck_log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub fn clear_log(&self) {
        self.log.lock().unwrap().clear();
    }

    /// The deck playing the current track.
    pub fn active_deck(&self) -> &FakeDeck {
        self.decks
            .iter()
            .find(|d| d.playing())
            .expect("a deck plays")
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        out
    }

    pub fn reports(&self) -> Vec<PlayActivityEvent> {
        self.reporter.events.lock().unwrap().clone()
    }
}

pub async fn wait(secs: f64) {
    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
}
