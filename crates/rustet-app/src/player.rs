//! The view side of the engine's player (spec §4.8): the current [`PlayerState`] kept in step
//! with the event stream, commands with typed errors, play intents in one call, the scrubber
//! position, and the queue and history lists.
//!
//! The state follows the stream: every connection starts with a snapshot (engine start and
//! reconnect), then `player.state`, `player.position` and `player.queue` change it. When events
//! were missed (`replay.truncated`, a lagging receiver) the state and the history are fetched
//! again.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rustet_ampapi::Api;
use rustet_player::model::{
    Command, Event as PlayerEvent, HistoryEntry, PlayerState, QueueItem, Status,
};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::engine::{ClientError, EngineClient, EngineEvent};
use crate::pages::play::{self, How, PlayError, PlayTarget};

/// Unix time in ms, the clock of `positionAt`.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The position to draw at `now_ms`: `positionMs` plus the time since `positionAt` while
/// playing, never past the track's length. The engine sends positions about once a second and
/// may pause them near a track's end (libvlc stops reporting; the player runs on its clock), so
/// a scrubber redraws from this, not from events.
pub fn position_now(state: &PlayerState, now_ms: u64) -> u64 {
    let pos = if state.status == Status::Playing {
        state.position_ms + now_ms.saturating_sub(state.position_at)
    } else {
        state.position_ms
    };
    if state.duration_ms > 0 {
        pos.min(state.duration_ms)
    } else {
        pos
    }
}

/// The queue after the current track (Up Next).
pub fn up_next(state: &PlayerState) -> &[QueueItem] {
    let items = &state.queue.items;
    match state.queue.current_index {
        Some(i) => items.get(i + 1..).unwrap_or_default(),
        None => items,
    }
}

/// Why a player command did not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The command named a track that is no longer current. Nothing to tell the user: the
    /// state already shows the new track.
    #[error("the player moved on to another track")]
    Stale,
    /// The engine does not answer (restarting or stopped); the offline view says so.
    #[error("the engine is not reachable")]
    Offline,
    /// The engine needs the Apple Music sign-in for this (stations, autoplay).
    #[error("Sign in to Apple Music to play this.")]
    SignInRequired,
    /// The engine refused the command as invalid.
    #[error("{0}")]
    Refused(String),
    /// The feature is not in this engine (no player, live radio).
    #[error("{0}")]
    Unavailable(String),
}

impl From<ClientError> for SendError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::StaleGeneration => Self::Stale,
            ClientError::Unreachable(_) => Self::Offline,
            ClientError::NotAuthenticated(_) => Self::SignInRequired,
            ClientError::Unavailable(m) => Self::Unavailable(m),
            ClientError::BadRequest(m) | ClientError::Conflict(m) | ClientError::NotFound(m) => {
                Self::Refused(m)
            }
            e => Self::Refused(e.to_string()),
        }
    }
}

impl SendError {
    /// The toast to show, `None` when the user needs no message.
    pub fn toast(&self) -> Option<String> {
        match self {
            Self::Stale => None,
            Self::Offline => {
                Some("Rustet's engine is not running. Wait a moment, then try again.".to_owned())
            }
            e => Some(e.to_string()),
        }
    }
}

/// Why playing something failed: the intent (nothing playable, loading the tracks) or the
/// command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayFailure {
    #[error(transparent)]
    Intent(#[from] PlayError),
    #[error(transparent)]
    Send(#[from] SendError),
}

impl PlayFailure {
    /// The toast to show, `None` when the user needs no message.
    pub fn toast(&self) -> Option<String> {
        match self {
            Self::Intent(e) => Some(match e {
                PlayError::NothingPlayable => "Nothing here can be played.".to_owned(),
                PlayError::Unavailable => {
                    "This track is not available in your country or region.".to_owned()
                }
                PlayError::NotQueueable => "A station cannot be added to the queue.".to_owned(),
                PlayError::SignInRequired => "Sign in to Apple Music to play stations.".to_owned(),
                PlayError::Load(_) => "Rustet could not load the tracks. Try again.".to_owned(),
            }),
            Self::Send(e) => e.toast(),
        }
    }
}

struct Inner {
    client: EngineClient,
    state: watch::Sender<Option<PlayerState>>,
    history: watch::Sender<Vec<HistoryEntry>>,
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

/// The player as a view sees it. Cheap to clone.
#[derive(Clone)]
pub struct Player(Arc<Inner>);

impl Player {
    /// Follows `events` (from [`crate::engine::Engine::events`]). Must run inside the tokio
    /// runtime.
    pub fn start(client: EngineClient, events: broadcast::Receiver<EngineEvent>) -> Self {
        let player = Self(Arc::new(Inner {
            client,
            state: watch::channel(None).0,
            history: watch::channel(Vec::new()).0,
            task: std::sync::Mutex::new(None),
        }));
        // The task holds only the senders: dropping the last `Player` stops it.
        let task = tokio::spawn(follow(
            player.0.client.clone(),
            player.0.state.clone(),
            player.0.history.clone(),
            events,
        ));
        *player.0.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        player
    }

    /// The player state; `None` until the engine's first snapshot.
    pub fn state(&self) -> watch::Receiver<Option<PlayerState>> {
        self.0.state.subscribe()
    }

    /// The play history, newest first (Up Next → History).
    pub fn history(&self) -> watch::Receiver<Vec<HistoryEntry>> {
        self.0.history.subscribe()
    }

    /// Sends a command. `Ok` once the player executed it; the new state arrives as an event.
    pub async fn send(&self, command: &Command) -> Result<(), SendError> {
        self.0
            .client
            .command(command)
            .await
            .map_err(SendError::from)
    }

    /// Music videos play as audio only (`true`) or with their video. The new choice arrives in
    /// the state (`mvAudioOnly`).
    pub async fn set_mv_audio_only(&self, audio_only: bool) -> Result<(), SendError> {
        let mode = rustet_player::model::VideoMode { audio_only };
        self.0
            .client
            .set_video_mode(mode)
            .await
            .map_err(SendError::from)
    }

    /// Applies a play intent of [`crate::pages::play`] (`page.play(How::Shuffled)`, …).
    pub async fn apply(&self, intent: Result<Command, PlayError>) -> Result<(), PlayFailure> {
        Ok(self.send(&intent?).await?)
    }

    /// Plays a tile's target, loading an album's or playlist's tracks first.
    pub async fn play_target(
        &self,
        api: &Api,
        target: &PlayTarget,
        how: How,
    ) -> Result<(), PlayFailure> {
        self.apply(play::play_target(api, target, how).await).await
    }

    /// The scrubber position now (see [`position_now`]); 0 before the first snapshot.
    pub fn position_ms(&self) -> u64 {
        self.0
            .state
            .borrow()
            .as_ref()
            .map_or(0, |s| position_now(s, unix_ms()))
    }
}

async fn follow(
    client: EngineClient,
    state: watch::Sender<Option<PlayerState>>,
    history: watch::Sender<Vec<HistoryEntry>>,
    mut events: broadcast::Receiver<EngineEvent>,
) {
    let refetch = async |with_state: bool| {
        if with_state {
            match client.player_state().await {
                Ok(s) => drop(state.send_replace(Some(s))),
                Err(e) => debug!("player state: {e}"),
            }
        }
        match client.history().await {
            Ok(h) => drop(history.send_replace(h)),
            Err(e) => debug!("play history: {e}"),
        }
    };
    loop {
        match events.recv().await {
            Ok(EngineEvent::Snapshot(s)) => match s.player {
                Some(p) => {
                    state.send_replace(Some(p));
                    refetch(false).await;
                }
                None => refetch(true).await,
            },
            Ok(EngineEvent::Player(event)) => match event {
                PlayerEvent::State(s) => drop(state.send_replace(Some(*s))),
                PlayerEvent::Position(p) => {
                    state.send_if_modified(|s| match s {
                        Some(s) if s.generation == p.generation => {
                            s.position_ms = p.position_ms;
                            s.duration_ms = p.duration_ms;
                            s.position_at = p.position_at;
                            true
                        }
                        _ => false,
                    });
                }
                PlayerEvent::Queue(q) => {
                    state.send_if_modified(|s| match s {
                        Some(s) => {
                            s.queue = q;
                            true
                        }
                        None => false,
                    });
                }
                PlayerEvent::History(h) => drop(history.send_replace(h)),
                PlayerEvent::Track(_)
                | PlayerEvent::ScrobbleDue(_)
                | PlayerEvent::Error(_)
                | PlayerEvent::AuthExpired => {}
            },
            Ok(EngineEvent::ReplayTruncated) | Err(broadcast::error::RecvError::Lagged(_)) => {
                refetch(true).await;
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scrubber_runs_on_while_playing_and_stops_at_the_end() {
        let mut s = PlayerState {
            status: Status::Playing,
            position_ms: 10_000,
            position_at: 1_000_000,
            duration_ms: 12_000,
            ..PlayerState::default()
        };
        assert_eq!(position_now(&s, 1_000_000), 10_000);
        assert_eq!(
            position_now(&s, 1_001_500),
            11_500,
            "time since positionAt is added"
        );
        assert_eq!(position_now(&s, 1_009_000), 12_000, "never past the length");
        assert_eq!(
            position_now(&s, 999_000),
            10_000,
            "a clock behind positionAt adds nothing"
        );
        s.status = Status::Paused;
        assert_eq!(
            position_now(&s, 1_005_000),
            10_000,
            "paused: the sampled position"
        );
        s.status = Status::Playing;
        s.duration_ms = 0;
        assert_eq!(
            position_now(&s, 1_005_000),
            15_000,
            "no known length: no cap"
        );
    }
}
