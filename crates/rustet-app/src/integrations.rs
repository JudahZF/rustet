//! The desktop integrations driven by the player: MPRIS, media keys, the tray, Discord, Last.fm
//! and ListenBrainz (`rustet-integrations`, unchanged).
//!
//! Player events feed them: `player.state` and `player.position` become MPRIS state and the
//! Discord presence, `player.track` the "now playing" of the scrobblers, `player.scrobble_due`
//! (the engine applies the min(240 s, half the track) rule) the scrobble. Their commands
//! ([`PageCommand`]) become player commands ([`route_command`]); what only the UI can do (show
//! the window, quit, love a track) arrives as an [`Intent`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustet_integrations::SecretStore;
use rustet_integrations::command::{Action, LoopStatus, PageCommand, Transport};
use rustet_integrations::discord::{Discord, PresenceUpdate, ipc_paths};
use rustet_integrations::lastfm::Lastfm;
use rustet_integrations::listenbrainz::ListenBrainz;
use rustet_integrations::now_playing::{MprisUpdate, PlaybackStatus};
use rustet_integrations::scrobble::ScrobbleTrack;
use rustet_player::model::{
    Command, Event as PlayerEvent, ItemKind, Mode, PlayerState, QueueItem, Repeat, Seek, Status,
    Volume,
};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::account::WebSession;
use crate::engine::{EngineClient, EngineEvent};
use crate::settings::Settings;

/// What a desktop integration asks the UI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Bring the main window to the front (MPRIS `Raise`).
    ShowWindow,
    /// Tray click: show or hide the main window.
    ToggleWindow,
    ToggleMaximize,
    ToggleMiniPlayer,
    /// Tray "Restart App": quit and start again, without the "resume" offer.
    Restart,
    Quit,
    /// MPRIS `OpenUri`: a link to dispatch (`rustet_desktop::links::parse_link`).
    OpenUri(String),
    /// Love or unlove the playing track (mini player).
    ToggleLove,
}

/// Where a command from a desktop control goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Routed {
    Player(Command),
    Ui(Intent),
}

/// Unix time in ms.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub use crate::player::position_now;

/// A command from MPRIS, the media keys, the tray, the mini player or the Windows controls, as
/// a player command against `state`. Seeks name the current generation, so a seek meant for a
/// track that just ended is refused instead of landing in the next one.
pub fn route_command(command: PageCommand, state: &PlayerState, now_ms: u64) -> Option<Routed> {
    let seek_to = |ms: f64| {
        let max = if state.duration_ms > 0 {
            state.duration_ms as f64
        } else {
            f64::MAX
        };
        Command::Seek(Seek {
            position_ms: ms.clamp(0.0, max) as u64,
            expect_generation: Some(state.generation),
        })
    };
    let command = match command {
        PageCommand::Transport(t) => match t {
            Transport::Play => Command::Play,
            Transport::Pause => Command::Pause,
            Transport::PlayPause => Command::Toggle,
            Transport::Next => Command::Next,
            Transport::Previous => Command::Previous,
        },
        PageCommand::Action(a) => match a {
            Action::Seek { delta_ms } if delta_ms.is_finite() && state.track.is_some() => {
                seek_to(position_now(state, now_ms) as f64 + delta_ms)
            }
            Action::SetPosition { ms } if ms.is_finite() && state.track.is_some() => seek_to(ms),
            Action::Seek { .. } | Action::SetPosition { .. } => return None,
            Action::Shuffle { value } => Command::Mode(Mode {
                shuffle: Some(value),
                ..Mode::default()
            }),
            Action::SetLoopStatus { value } => Command::Mode(Mode {
                repeat: Some(match value {
                    LoopStatus::None => Repeat::Off,
                    LoopStatus::Track => Repeat::One,
                    LoopStatus::Playlist => Repeat::All,
                }),
                ..Mode::default()
            }),
            Action::SetVolume { value } if value.is_finite() => Command::Volume(Volume {
                level: value.round().clamp(0.0, 100.0) as u8,
                muted: None,
            }),
            Action::SetVolume { .. } => return None,
            // The mini player's repeat button: off → all → one → off.
            Action::Repeat => Command::Mode(Mode {
                repeat: Some(match state.repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                }),
                ..Mode::default()
            }),
            Action::Love => return Some(Routed::Ui(Intent::ToggleLove)),
        },
    };
    Some(Routed::Player(command))
}

/// The MPRIS track ID of a queue entry: the queue uid, so the same song queued twice is two
/// tracks.
fn track_id(uid: u64) -> String {
    format!("/com/apple/music/track/{uid}")
}

/// The full MPRIS state for `state` (spec §4.7): status, metadata (art at 512 px), position,
/// loop status, shuffle, volume.
pub fn mpris_update(state: &PlayerState, now_ms: u64) -> MprisUpdate {
    let status = match state.status {
        Status::Playing => PlaybackStatus::Playing,
        // A track is loaded and will play: desktops show it paused, not gone.
        Status::Paused | Status::Opening | Status::Loading => PlaybackStatus::Paused,
        Status::Idle | Status::Stopped | Status::Failed => PlaybackStatus::Stopped,
    };
    let metadata = state.track.as_ref().map(|item| {
        let m = &item.meta;
        let length_ms = if state.duration_ms > 0 {
            state.duration_ms
        } else {
            m.duration_ms
        };
        let mut map = Map::new();
        map.insert("mpris:trackid".into(), json!(track_id(item.uid)));
        map.insert("mpris:length".into(), json!(length_ms * 1000));
        map.insert("xesam:title".into(), json!(m.name));
        map.insert("xesam:artist".into(), json!([m.artist_name]));
        map.insert("xesam:album".into(), json!(m.album_name));
        if !m.artist_name.is_empty() {
            map.insert("xesam:albumArtist".into(), json!([m.artist_name]));
        }
        if !m.artwork_url.is_empty() {
            map.insert(
                "mpris:artUrl".into(),
                json!(m.artwork_url.replace("{w}", "512").replace("{h}", "512")),
            );
        }
        map
    });
    MprisUpdate {
        status: Some(status),
        metadata: Some(metadata.unwrap_or_default()),
        position_us: Some(position_now(state, now_ms) as i64 * 1000),
        shuffle: Some(state.shuffle),
        loop_status: Some(match state.repeat {
            Repeat::Off => LoopStatus::None,
            Repeat::One => LoopStatus::Track,
            Repeat::All => LoopStatus::Playlist,
        }),
        volume: Some(if state.muted {
            0.0
        } else {
            f64::from(state.volume) / 100.0
        }),
        seeked: false,
    }
}

/// The Discord "Listening to" presence for `state`; `None` when nothing is loaded.
pub fn presence(state: &PlayerState, storefront: &str, now_ms: u64) -> Option<PresenceUpdate> {
    let item = state.track.as_ref()?;
    let m = &item.meta;
    let playing = state.status == Status::Playing;
    let duration = if state.duration_ms > 0 {
        state.duration_ms
    } else {
        m.duration_ms
    };
    let started = now_ms.saturating_sub(position_now(state, now_ms));
    let is_video = item.kind == ItemKind::MusicVideo;
    let slug = {
        let s: String = m
            .name
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let s = s
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        if s.is_empty() { "track".to_owned() } else { s }
    };
    let kind = if is_video { "music-video" } else { "song" };
    Some(PresenceUpdate {
        name: m.name.clone(),
        artist: m.artist_name.clone(),
        album: m.album_name.clone(),
        artwork_url: m.artwork_url.replace("{w}", "512").replace("{h}", "512"),
        playing,
        started_at_ms: if playing { started as f64 } else { 0.0 },
        ends_at_ms: if playing && duration > 0 {
            (started + duration) as f64
        } else {
            0.0
        },
        apple_url: format!(
            "https://music.apple.com/{storefront}/{kind}/{slug}/{}",
            item.id.as_str()
        ),
        is_video,
    })
}

/// The scrobblers' track. `timestamp` (Unix s) only for a scrobble.
pub fn scrobble_track(item: &QueueItem, duration_ms: u64, timestamp: Option<u64>) -> ScrobbleTrack {
    let m = &item.meta;
    let duration_ms = if duration_ms > 0 {
        duration_ms
    } else {
        m.duration_ms
    };
    ScrobbleTrack {
        artist: m.artist_name.clone(),
        track: m.name.clone(),
        album: Some(m.album_name.clone()).filter(|a| !a.is_empty()),
        album_artist: Some(m.artist_name.clone()).filter(|a| !a.is_empty()),
        duration: (duration_ms > 0).then(|| duration_ms as f64 / 1000.0),
        timestamp: timestamp.map(serde_json::Number::from),
    }
}

/// The track changed or playback started or stopped: the presence needs an update. Position
/// updates alone do not (Discord rate-limits; the 10 s refresh covers drift).
fn presence_key(state: &PlayerState) -> (Option<u64>, bool) {
    (
        state.track.as_ref().map(|t| t.uid),
        state.status == Status::Playing,
    )
}

/// Discord limits `SET_ACTIVITY` (about 5 in 15 s); the presence is refreshed this often.
const DISCORD_REFRESH: Duration = Duration::from_secs(10);

/// The scrobblers, for Settings → Integrations (connect, status, disconnect).
pub struct Scrobblers {
    pub lastfm: Lastfm,
    pub listenbrainz: ListenBrainz,
}

impl Scrobblers {
    /// Both scrobblers over the encrypted store, under the web shell's key names.
    pub fn new(http: reqwest::Client, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            lastfm: Lastfm::new(http.clone(), secrets.clone()),
            listenbrainz: ListenBrainz::new(http, secrets),
        }
    }
}

/// The scrobblers' credentials in the encrypted store, under the web shell's key names.
pub struct StoreSecrets(pub rustet_desktop::store::Store);

impl SecretStore for StoreSecrets {
    fn get(&self, key: &str) -> Option<String> {
        match self.0.get(key)? {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    fn set(&self, key: &str, value: String) {
        self.0
            .update(|m| m.insert(key.to_owned(), Value::String(value)));
    }

    fn delete(&self, key: &str) {
        self.0.update(|m| m.remove(key));
    }
}

/// What drives the integrations.
pub struct Inputs {
    pub client: EngineClient,
    pub events: broadcast::Receiver<EngineEvent>,
    pub settings: watch::Receiver<Settings>,
    /// The account session ([`crate::account::AccountModel::session`]), for the storefront of
    /// Discord's "Open in Apple Music" link.
    pub session: watch::Receiver<Option<WebSession>>,
    /// Start MPRIS, the tray and the media keys. `false`: only the scrobblers and Discord
    /// (tests, a headless run).
    pub desktop: bool,
}

/// The running desktop services. Each fails on its own: no session bus, no tray host or no X
/// display only disables that one.
pub struct Integrations {
    scrobblers: Arc<Scrobblers>,
    commands: mpsc::UnboundedSender<PageCommand>,
    cancel: CancellationToken,
    driver: tokio::task::JoinHandle<()>,
    #[cfg(target_os = "linux")]
    linux: Option<linux::Services>,
}

impl Integrations {
    /// Starts every service and the task that feeds them. Intents for the UI arrive on the
    /// returned receiver. Must run inside the tokio runtime.
    pub async fn start(
        inputs: Inputs,
        scrobblers: Arc<Scrobblers>,
    ) -> (Self, mpsc::UnboundedReceiver<Intent>) {
        let (intents, intents_rx) = mpsc::unbounded_channel();
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        #[cfg(target_os = "linux")]
        let (linux, mpris) = if inputs.desktop {
            let (services, mpris) = linux::Services::start(commands.clone(), intents.clone()).await;
            (Some(services), Some(mpris))
        } else {
            (None, None)
        };
        let driver = Driver {
            client: inputs.client,
            scrobblers: scrobblers.clone(),
            settings: inputs.settings,
            session: inputs.session,
            intents,
            #[cfg(target_os = "linux")]
            mpris,
            discord: None,
            state: PlayerState::default(),
        };
        let driver = tokio::spawn(driver.run(inputs.events, commands_rx, cancel.clone()));
        (
            Self {
                scrobblers,
                commands,
                cancel,
                driver,
                #[cfg(target_os = "linux")]
                linux,
            },
            intents_rx,
        )
    }

    pub fn scrobblers(&self) -> &Scrobblers {
        &self.scrobblers
    }

    /// A command from a control the UI hosts (the mini player, Windows media controls), routed
    /// like the desktop's.
    pub fn send(&self, command: PageCommand) {
        let _ = self.commands.send(command);
    }

    /// The tray menu says "Hide Mini Player" while it is open.
    pub async fn set_mini_player_open(&self, open: bool) {
        #[cfg(target_os = "linux")]
        if let Some(linux) = &self.linux {
            linux.set_mini_player_open(open).await;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = open;
    }

    /// Unregisters MPRIS, clears Discord, removes the tray icon, releases the media keys.
    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.driver.await;
        #[cfg(target_os = "linux")]
        if let Some(linux) = self.linux {
            linux.stop().await;
        }
    }
}

/// The task that feeds the services from player events and routes their commands.
struct Driver {
    client: EngineClient,
    scrobblers: Arc<Scrobblers>,
    settings: watch::Receiver<Settings>,
    session: watch::Receiver<Option<WebSession>>,
    intents: mpsc::UnboundedSender<Intent>,
    #[cfg(target_os = "linux")]
    mpris: Option<rustet_integrations::mpris::Mpris>,
    discord: Option<Discord>,
    /// The latest player state, with positions and queue changes applied.
    state: PlayerState,
}

impl Driver {
    async fn run(
        mut self,
        mut events: broadcast::Receiver<EngineEvent>,
        mut commands: mpsc::UnboundedReceiver<PageCommand>,
        cancel: CancellationToken,
    ) {
        let mut refresh = tokio::time::interval(DISCORD_REFRESH);
        let mut settings = self.settings.clone();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                event = events.recv() => match event {
                    Ok(event) => self.on_event(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Ok(state) = self.client.player_state().await {
                            self.on_state(state, true);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                Some(command) = commands.recv() => self.on_command(command).await,
                Ok(()) = settings.changed() => {
                    settings.borrow_and_update();
                    self.update_discord();
                }
                _ = refresh.tick() => {
                    if self.state.status == Status::Playing {
                        self.update_discord();
                    }
                }
            }
        }
        if let Some(d) = self.discord.take() {
            d.disable();
        }
        #[cfg(target_os = "linux")]
        if let Some(m) = self.mpris.take() {
            m.stop().await;
        }
    }

    fn on_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Snapshot(s) => {
                if let Some(state) = s.player {
                    self.on_state(state, true);
                }
            }
            EngineEvent::Player(event) => match event {
                PlayerEvent::State(state) => self.on_state(*state, false),
                PlayerEvent::Position(p) => {
                    if self.state.generation == p.generation {
                        let jumped = position_now(&self.state, p.position_at)
                            .abs_diff(p.position_ms)
                            > SEEK_JUMP_MS;
                        self.state.position_ms = p.position_ms;
                        self.state.duration_ms = p.duration_ms;
                        self.state.position_at = p.position_at;
                        // Position alone: MPRIS clients interpolate; only a jump is signalled.
                        if jumped {
                            self.mpris(MprisUpdate {
                                position_us: Some(p.position_ms as i64 * 1000),
                                seeked: true,
                                ..MprisUpdate::default()
                            });
                        }
                    }
                }
                PlayerEvent::Queue(queue) => self.state.queue = queue,
                PlayerEvent::Track(t) => {
                    let track = scrobble_track(&t.item, t.duration_ms, None);
                    let s = self.scrobblers.clone();
                    tokio::spawn(async move {
                        tokio::join!(
                            s.lastfm.now_playing(&track),
                            s.listenbrainz.now_playing(&track)
                        );
                    });
                }
                PlayerEvent::ScrobbleDue(due) => {
                    let track = scrobble_track(&due.item, due.duration_ms, Some(due.timestamp));
                    let s = self.scrobblers.clone();
                    tokio::spawn(async move {
                        tokio::join!(s.lastfm.scrobble(&track), s.listenbrainz.scrobble(&track));
                    });
                }
                PlayerEvent::Error(_) | PlayerEvent::History(_) | PlayerEvent::AuthExpired => {}
            },
            _ => {}
        }
    }

    fn on_state(&mut self, state: PlayerState, resync: bool) {
        let presence_changed = resync || presence_key(&state) != presence_key(&self.state);
        let now = now_ms();
        // Same track, position away from where it should be: a seek (MPRIS `Seeked`).
        let seeked = !resync
            && state.generation == self.state.generation
            && state.track.is_some()
            && position_now(&state, now).abs_diff(position_now(&self.state, now)) > SEEK_JUMP_MS;
        self.state = state;
        self.mpris(MprisUpdate {
            seeked,
            ..mpris_update(&self.state, now)
        });
        if presence_changed {
            self.update_discord();
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn mpris(&self, update: MprisUpdate) {
        #[cfg(target_os = "linux")]
        if let Some(m) = &self.mpris {
            m.update(update);
        }
    }

    fn update_discord(&mut self) {
        if !self.settings.borrow().integrations.discord {
            if let Some(d) = self.discord.take() {
                d.disable();
            }
            return;
        }
        let storefront = self
            .session
            .borrow()
            .as_ref()
            .map_or_else(|| "us".to_owned(), |s| s.storefront.clone());
        let discord = self.discord.get_or_insert_with(|| {
            Discord::spawn(ipc_paths(cfg!(windows), |k| std::env::var(k).ok()))
        });
        match presence(&self.state, &storefront, now_ms()) {
            Some(p) => discord.update(p),
            None => discord.clear(),
        }
    }

    async fn on_command(&mut self, command: PageCommand) {
        match route_command(command, &self.state, now_ms()) {
            Some(Routed::Player(c)) => {
                if let Err(e) = self.client.command(&c).await {
                    debug!("player command {c:?}: {e}");
                }
            }
            Some(Routed::Ui(intent)) => {
                let _ = self.intents.send(intent);
            }
            None => {}
        }
    }
}

/// A position this far from the expected one is a seek, not drift.
const SEEK_JUMP_MS: u64 = 1500;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use rustet_integrations::media_keys::MediaKeys;
    use rustet_integrations::mpris::{Bus, Mpris, MprisEvent};
    use rustet_integrations::tray::{Tray, TrayEvent};
    use tracing::info;

    const TRAY_ICON: &[u8] = include_bytes!("../../../assets/tray-icon.png");

    /// The Linux services: MPRIS on the session bus (it waits 1.5 s, then connects and
    /// reconnects by itself), the tray icon, the X11 media-key grabs.
    pub(super) struct Services {
        tray: Arc<tokio::sync::Mutex<Option<Tray>>>,
        media_keys: Mutex<Option<MediaKeys>>,
    }

    impl Services {
        pub(super) async fn start(
            commands: mpsc::UnboundedSender<PageCommand>,
            intents: mpsc::UnboundedSender<Intent>,
        ) -> (Self, Mpris) {
            let (mpris, mut mpris_events) = Mpris::spawn(Bus::Session);
            {
                let (commands, intents) = (commands.clone(), intents.clone());
                tokio::spawn(async move {
                    while let Some(event) = mpris_events.recv().await {
                        match event {
                            MprisEvent::Command(c) => drop(commands.send(c)),
                            MprisEvent::Raise => drop(intents.send(Intent::ShowWindow)),
                            MprisEvent::Quit => drop(intents.send(Intent::Quit)),
                            MprisEvent::OpenUri(uri) => drop(intents.send(Intent::OpenUri(uri))),
                        }
                    }
                });
            }
            let tray = Arc::new(tokio::sync::Mutex::new(None));
            match Tray::spawn(TRAY_ICON).await {
                Ok((t, mut events)) => {
                    *tray.lock().await = Some(t);
                    let (commands, intents) = (commands.clone(), intents.clone());
                    tokio::spawn(async move {
                        while let Some(event) = events.recv().await {
                            let intent = match event {
                                TrayEvent::Playback(t) => {
                                    let _ = commands.send(t.into());
                                    continue;
                                }
                                TrayEvent::ToggleWindow => Intent::ToggleWindow,
                                TrayEvent::ToggleMaximize => Intent::ToggleMaximize,
                                TrayEvent::ToggleMiniPlayer => Intent::ToggleMiniPlayer,
                                TrayEvent::Restart => Intent::Restart,
                                TrayEvent::Quit => Intent::Quit,
                            };
                            let _ = intents.send(intent);
                        }
                    });
                }
                Err(e) => warn!("no tray icon: {e}"),
            }
            // Global media keys (X11 grabs; Bluetooth AVRCP buttons arrive as key events).
            let media_keys = match MediaKeys::spawn(None) {
                Ok((keys, mut presses)) => {
                    tokio::spawn(async move {
                        while let Some(t) = presses.recv().await {
                            let _ = commands.send(t.into());
                        }
                    });
                    Some(keys)
                }
                Err(e) => {
                    info!("media keys not grabbed: {e}");
                    None
                }
            };
            let services = Self {
                tray,
                media_keys: Mutex::new(media_keys),
            };
            (services, mpris)
        }

        pub(super) async fn set_mini_player_open(&self, open: bool) {
            if let Some(t) = self.tray.lock().await.as_ref() {
                t.set_mini_player_open(open).await;
            }
        }

        pub(super) async fn stop(self) {
            if let Some(t) = self.tray.lock().await.take() {
                t.shutdown().await;
            }
            self.media_keys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
    }
}
