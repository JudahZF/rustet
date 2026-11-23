//! The supervised engine child, its event stream, and the connection state the UI draws.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustet_desktop::engine::{
    EngineEvent as ProcessEvent, EngineLauncher, EngineSupervisor, SupervisorTiming,
};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use super::client::EngineClient;
use super::events::{self, EngineEvent};

/// The UI's link to the engine, for the offline view and the "engine status" row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// The engine process is starting, or the event stream is connecting to it.
    Starting,
    /// The event stream is open: requests work and state is current.
    Ready,
    /// The engine exited; it starts again after `retry_in` (`None` while that is being
    /// decided).
    Restarting { retry_in: Option<Duration> },
    /// The engine cannot start, and it is not retried by itself (missing binary, port taken).
    /// `reason` tells the user what to do. [`Engine::retry`] tries again.
    Failed { reason: String },
    /// Stopped for good (the app quits).
    Stopped,
}

#[derive(Debug, Clone)]
enum Process {
    Starting,
    Running,
    Restarting(Option<Duration>),
    Failed(String),
    Stopped,
}

struct LinkState {
    process: Process,
    stream_open: bool,
}

impl LinkState {
    fn connection(&self) -> Connection {
        match &self.process {
            Process::Failed(reason) => Connection::Failed {
                reason: reason.clone(),
            },
            Process::Stopped => Connection::Stopped,
            Process::Restarting(delay) => Connection::Restarting { retry_in: *delay },
            Process::Starting | Process::Running if self.stream_open => Connection::Ready,
            Process::Starting | Process::Running => Connection::Starting,
        }
    }
}

struct Shared {
    state: Mutex<LinkState>,
    tx: watch::Sender<Connection>,
}

impl Shared {
    fn change(&self, f: impl FnOnce(&mut LinkState)) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut state);
        let connection = state.connection();
        drop(state);
        self.tx.send_if_modified(|c| {
            let changed = *c != connection;
            *c = connection;
            changed
        });
    }
}

/// The engine: the supervised child process, the typed client, the event stream and the
/// connection state. Cheap to clone.
pub struct Engine<L> {
    supervisor: EngineSupervisor<L>,
    client: EngineClient,
    events: broadcast::Sender<EngineEvent>,
    connection: watch::Receiver<Connection>,
    shared: Arc<Shared>,
    cancel: CancellationToken,
}

impl<L> Clone for Engine<L> {
    fn clone(&self) -> Self {
        Self {
            supervisor: self.supervisor.clone(),
            client: self.client.clone(),
            events: self.events.clone(),
            connection: self.connection.clone(),
            shared: self.shared.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

impl<L: EngineLauncher> Engine<L> {
    /// Starts the engine and its event stream. Must run inside the tokio runtime.
    ///
    /// `launcher` is `super::launcher(…)` in the app; `client` must trust the identity the
    /// launcher hands the engine.
    pub fn start(launcher: L, client: EngineClient, timing: SupervisorTiming) -> Self {
        let (tx, connection) = watch::channel(Connection::Starting);
        let shared = Arc::new(Shared {
            state: Mutex::new(LinkState {
                process: Process::Starting,
                stream_open: false,
            }),
            tx,
        });
        let on_process = {
            let shared = shared.clone();
            move |event: ProcessEvent| {
                let process = match event {
                    ProcessEvent::Started { pid } => {
                        info!("engine running (pid {pid})");
                        Process::Running
                    }
                    ProcessEvent::Exited { code, signal } => {
                        info!("engine exited (code {code:?}, signal {signal:?})");
                        Process::Restarting(None)
                    }
                    ProcessEvent::Restarting { delay } => Process::Restarting(Some(delay)),
                    ProcessEvent::StartFailed(e) => {
                        error!("the engine could not start: {e}");
                        Process::Failed(e.to_string())
                    }
                };
                shared.change(|s| {
                    // A late event of a stopped supervisor does not revive the state.
                    if !matches!(s.process, Process::Stopped) {
                        s.process = process;
                    }
                });
            }
        };
        let supervisor = EngineSupervisor::new(launcher, timing, on_process);
        let cancel = CancellationToken::new();
        let (events, mut link) = events::spawn(client.clone(), cancel.clone());
        tokio::spawn({
            let shared = shared.clone();
            async move {
                while link.changed().await.is_ok() {
                    let open = *link.borrow_and_update();
                    shared.change(|s| s.stream_open = open);
                }
            }
        });
        tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.start().await }
        });
        Self {
            supervisor,
            client,
            events,
            connection,
            shared,
            cancel,
        }
    }

    pub fn client(&self) -> &EngineClient {
        &self.client
    }

    /// The engine's events from now on.
    pub fn events(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    /// The connection state; `changed()` wakes on every change.
    pub fn connection(&self) -> watch::Receiver<Connection> {
        self.connection.clone()
    }

    /// Restarts a running engine (new FairPlay libraries, a finished sign-out). Nothing once
    /// stopped.
    pub fn reload(&self) {
        self.supervisor.reload();
    }

    /// After [`Connection::Failed`]: tries to start the engine again.
    pub async fn retry(&self) {
        self.shared.change(|s| {
            if matches!(s.process, Process::Failed(_)) {
                s.process = Process::Starting;
            }
        });
        self.supervisor.start().await;
    }

    /// Apple's FairPlay libraries cannot ship with Rustet: an install fetches them on first run
    /// (`fetch-fairplay-libs.sh` beside the executable). The engine loads them only at start,
    /// so it is reloaded once they arrive: at once when nothing plays, else at the next pause.
    #[cfg(target_os = "linux")]
    pub async fn fetch_fairplay_libs(
        &self,
        config_dir: &std::path::Path,
        exe_dir: &std::path::Path,
    ) {
        use rustet_desktop::fairplay::{FetchOutcome, ensure_fairplay_libs};
        use rustet_player::model::{Event as PlayerEvent, Status};
        let mut events = self.events();
        let lib64 = rustet_desktop::engine::fairplay_lib64(config_dir);
        let script = exe_dir.join("fetch-fairplay-libs.sh");
        if ensure_fairplay_libs(&lib64, &script).await != FetchOutcome::Installed {
            return;
        }
        let playing = self
            .client
            .player_state()
            .await
            .is_ok_and(|s| s.status == Status::Playing);
        if playing {
            info!("FairPlay libraries installed: reloading the engine at the next pause");
            loop {
                match events.recv().await {
                    Ok(EngineEvent::Player(PlayerEvent::State(s)))
                        if s.status != Status::Playing =>
                    {
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                    _ => {}
                }
            }
        }
        self.reload();
    }

    /// Stops the engine for good: the event stream ends, the child gets SIGTERM, then SIGKILL.
    pub async fn stop(&self) {
        self.shared.change(|s| s.process = Process::Stopped);
        self.cancel.cancel();
        self.supervisor.stop().await;
    }
}
