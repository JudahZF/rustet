//! Engine process ownership and lifecycle (`engine-lifecycle.mjs`): one owner for the async
//! preparation, the child, and the restart timer.
//!
//! - Concurrent starts share one preparation and one child.
//! - Every exit of a running engine restarts it (exit code 3 is the engine asking for it, after a
//!   sign-out it must finish in a fresh process). The delay is 1 s after a run of more than 30 s,
//!   otherwise it doubles, up to 30 s.
//! - A start that cannot exec (missing file, no permission) is reported once, never retried.
//! - Stop sends SIGTERM, then SIGKILL after the kill delay, and gives up waiting 800 ms later.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{Level, debug, error, info, warn};

use super::EngineError;

/// What the supervisor starts. The real one is [`super::RustetEngine`]; tests use fake children.
pub trait EngineLauncher: Send + Sync + 'static {
    /// Work before each start (orphan recovery, port check, config). `quit` is cancelled when the
    /// supervisor stops.
    fn prepare(
        &self,
        quit: &CancellationToken,
    ) -> impl Future<Output = Result<(), EngineError>> + Send;
    /// The process to start. Output pipes are set up by the supervisor.
    fn command(&self) -> Command;
    /// Bytes written to the child's standard input, which is then closed (the engine's TLS
    /// identity). `None`: no standard input.
    fn stdin(&self) -> Option<Vec<u8>> {
        None
    }
}

/// What happened to the engine, for the shell (it shows a dialog for a failed start).
#[derive(Debug)]
pub enum EngineEvent {
    Started {
        pid: u32,
    },
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Restarting {
        delay: Duration,
    },
    StartFailed(EngineError),
}

/// Restart and shutdown delays.
#[derive(Debug, Clone, Copy)]
pub struct SupervisorTiming {
    pub restart_delay: Duration,
    /// Grace time after SIGTERM before SIGKILL (the engine needs it for VLC teardown).
    pub kill_delay: Duration,
}

impl Default for SupervisorTiming {
    fn default() -> Self {
        Self {
            restart_delay: Duration::from_secs(1),
            kill_delay: Duration::from_secs(3),
        }
    }
}

const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A run longer than this resets the restart delay.
const HEALTHY_RUN: Duration = Duration::from_secs(30);

enum Signal {
    Term,
    Kill,
}

struct Running {
    id: u64,
    control: mpsc::UnboundedSender<Signal>,
    exited: watch::Receiver<bool>,
}

#[derive(Default)]
struct State {
    child: Option<Running>,
    restart: Option<JoinHandle<()>>,
    backoff: Option<Duration>,
    next_id: u64,
}

type EventSink = Box<dyn Fn(EngineEvent) + Send + Sync>;

struct Shared<L> {
    launcher: L,
    timing: SupervisorTiming,
    on_event: EventSink,
    quit: CancellationToken,
    /// Held for a whole start, so concurrent starts join the one in progress.
    starting: Mutex<()>,
    state: SyncMutex<State>,
}

/// Owns the engine process. Cheap to clone.
pub struct EngineSupervisor<L>(Arc<Shared<L>>);

impl<L> Clone for EngineSupervisor<L> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<L: EngineLauncher> EngineSupervisor<L> {
    pub fn new(
        launcher: L,
        timing: SupervisorTiming,
        on_event: impl Fn(EngineEvent) + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Shared {
            launcher,
            timing,
            on_event: Box::new(on_event),
            quit: CancellationToken::new(),
            starting: Mutex::new(()),
            state: SyncMutex::new(State::default()),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: EngineEvent) {
        (self.0.on_event)(event);
    }

    /// Starts the engine unless it runs, is starting, or the supervisor has stopped.
    pub async fn start(&self) {
        if self.0.quit.is_cancelled() {
            return;
        }
        let _starting = self.0.starting.lock().await;
        if self.0.quit.is_cancelled() || self.state().child.is_some() {
            return;
        }
        if let Some(timer) = self.state().restart.take() {
            timer.abort();
        }
        if let Err(e) = self.0.launcher.prepare(&self.0.quit).await {
            self.emit(EngineEvent::StartFailed(e));
            return;
        }
        if self.0.quit.is_cancelled() {
            return;
        }
        let mut cmd = self.0.launcher.command();
        let input = self.0.launcher.stdin();
        cmd.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            // A failed exec needs a visible error, not an endless series of retries.
            Err(e) => {
                self.emit(EngineEvent::StartFailed(EngineError::Spawn(e)));
                return;
            }
        };
        let pid = child.id().unwrap_or_default();
        info!("engine started (pid {pid})");
        self.emit(EngineEvent::Started { pid });
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            tokio::spawn(async move {
                // Dropping `stdin` closes it: the reader sees the end of its input.
                if let Err(e) = stdin.write_all(&input).await {
                    warn!("could not write the engine's standard input: {e}");
                }
            });
        }
        if let Some(out) = child.stdout.take() {
            tokio::spawn(forward_output(out, false));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(forward_output(err, true));
        }
        let (control, control_rx) = mpsc::unbounded_channel();
        let (exited_tx, exited) = watch::channel(false);
        let id = {
            let mut state = self.state();
            state.next_id += 1;
            state.child = Some(Running {
                id: state.next_id,
                control,
                exited,
            });
            state.next_id
        };
        tokio::spawn(self.clone().monitor(child, id, control_rx, exited_tx));
    }

    async fn monitor(
        self,
        mut child: Child,
        id: u64,
        mut control: mpsc::UnboundedReceiver<Signal>,
        exited: watch::Sender<bool>,
    ) {
        let started = Instant::now();
        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                Some(signal) = control.recv() => match signal {
                    Signal::Term => terminate(&mut child),
                    Signal::Kill => { let _ = child.start_kill(); }
                },
            }
        };
        let (code, signal) = match &status {
            Ok(s) => (s.code(), exit_signal(s)),
            Err(_) => (None, None),
        };
        info!("engine exited (code {code:?}, signal {signal:?})");
        let _ = exited.send(true);
        self.emit(EngineEvent::Exited { code, signal });
        let ours = {
            let mut state = self.state();
            let ours = state.child.as_ref().is_some_and(|c| c.id == id);
            if ours {
                state.child = None;
            }
            ours
        };
        if ours {
            self.schedule_restart(started.elapsed());
        }
    }

    fn schedule_restart(&self, ran: Duration) {
        if self.0.quit.is_cancelled() {
            return;
        }
        let mut state = self.state();
        if state.restart.is_some() {
            return;
        }
        let base = self.0.timing.restart_delay;
        let delay = match state.backoff {
            Some(prev) if ran <= HEALTHY_RUN => (prev * 2).min(MAX_BACKOFF),
            None if ran <= HEALTHY_RUN => (base * 2).min(MAX_BACKOFF),
            _ => base,
        };
        state.backoff = Some(delay);
        info!("restarting engine in {delay:?}");
        self.emit(EngineEvent::Restarting { delay });
        let me = self.clone();
        let restart: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
            tokio::select! {
                () = me.0.quit.cancelled() => {}
                () = tokio::time::sleep(delay) => {
                    me.state().restart = None;
                    me.start().await;
                }
            }
        });
        state.restart = Some(tokio::spawn(restart));
    }

    /// Stops the engine for good: no restart after this. Waits for a start in progress, then for
    /// the child to exit.
    pub async fn stop(&self) {
        self.0.quit.cancel();
        if let Some(timer) = self.state().restart.take() {
            timer.abort();
        }
        let _starting = self.0.starting.lock().await;
        let Some(child) = self.state().child.take() else {
            return;
        };
        let mut exited = child.exited;
        if child.control.send(Signal::Term).is_err() {
            return;
        }
        let kill_delay = self.0.timing.kill_delay;
        if tokio::time::timeout(kill_delay, exited.wait_for(|e| *e))
            .await
            .is_err()
        {
            warn!("engine did not stop after SIGTERM; killing it");
            let _ = child.control.send(Signal::Kill);
            let _ = tokio::time::timeout(Duration::from_millis(800), exited.wait_for(|e| *e)).await;
        }
    }

    /// Restarts a running engine (so it picks up newly installed libraries, or a fresh process
    /// after a sign-out): the exit path brings it back. Nothing once stopped.
    pub fn reload(&self) {
        if self.0.quit.is_cancelled() {
            return;
        }
        if let Some(child) = &self.state().child {
            let _ = child.control.send(Signal::Term);
        }
    }

    /// Whether an engine process runs.
    pub fn is_running(&self) -> bool {
        self.state().child.is_some()
    }
}

/// SIGTERM on Unix; Windows has no SIGTERM and terminates the process.
fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child
        .id()
        .and_then(|p| rustix::process::Pid::from_raw(p as i32))
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        return;
    }
    let _ = child.start_kill();
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        std::os::unix::process::ExitStatusExt::signal(status)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// The engine's output goes to the log, line by line. A line of the engine's own logger keeps
/// its level; other output on standard error (libvlc, Apple's libraries) is a warning.
async fn forward_output(stream: impl AsyncRead + Unpin, stderr: bool) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match engine_log_line(&line) {
            Some((Level::ERROR, message)) => error!(target: "engine", "{message}"),
            Some((Level::WARN, message)) => warn!(target: "engine", "{message}"),
            Some((Level::INFO, message)) => info!(target: "engine", "{message}"),
            Some((_, message)) => debug!(target: "engine", "{message}"),
            None if stderr => warn!(target: "engine", "{line}"),
            None => info!(target: "engine", "{line}"),
        }
    }
}

/// The level and the rest (`target: message`) of a line the engine's logger wrote, which is
/// `<RFC 3339 time> LEVEL target: message`. `None` for any other line.
fn engine_log_line(line: &str) -> Option<(Level, &str)> {
    let (time, rest) = line.split_once(' ')?;
    if !time.starts_with(|c: char| c.is_ascii_digit()) || !time.ends_with('Z') {
        return None;
    }
    let (level, message) = rest.trim_start().split_once(' ')?;
    Some((level.parse().ok()?, message))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn an_engine_log_line_keeps_its_level() {
        assert_eq!(
            engine_log_line(
                "2026-11-23T10:49:47.553741Z  INFO rustet_engine::state: DRM backend name=\"rustetplay\""
            ),
            Some((
                Level::INFO,
                "rustet_engine::state: DRM backend name=\"rustetplay\""
            ))
        );
        assert_eq!(
            engine_log_line(
                "2026-11-23T15:41:07.841797Z  WARN rustet_player::controller: player: failed"
            ),
            Some((Level::WARN, "rustet_player::controller: player: failed"))
        );
        // libvlc and Apple's libraries write their own lines.
        assert_eq!(
            engine_log_line(
                "[0000e7616816a270] imem demux error: Invalid get/release function pointers"
            ),
            None
        );
        assert_eq!(
            engine_log_line("__bionic_open_tzdata: couldn't find any tzdata when looking for GMT!"),
            None
        );
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct Fake {
        script: &'static str,
        prepares: AtomicUsize,
        hold: Option<Arc<Notify>>,
        program: &'static str,
    }

    impl Fake {
        fn new(script: &'static str) -> Self {
            Self {
                script,
                prepares: AtomicUsize::new(0),
                hold: None,
                program: "/bin/sh",
            }
        }
    }

    impl EngineLauncher for Fake {
        async fn prepare(&self, _quit: &CancellationToken) -> Result<(), EngineError> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            if let Some(hold) = &self.hold {
                hold.notified().await;
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(())
        }
        fn command(&self) -> Command {
            let mut c = Command::new(self.program);
            c.args(["-c", self.script]);
            c
        }
    }

    /// Writes what it reads on standard input to a file, one line per run, then exits.
    struct Echo {
        out: std::path::PathBuf,
    }

    impl EngineLauncher for Echo {
        async fn prepare(&self, _quit: &CancellationToken) -> Result<(), EngineError> {
            Ok(())
        }
        fn command(&self) -> Command {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", "cat >> \"$1\"; echo >> \"$1\"", "sh"])
                .arg(&self.out);
            c
        }
        fn stdin(&self) -> Option<Vec<u8>> {
            Some(b"identity".to_vec())
        }
    }

    #[tokio::test]
    async fn every_start_gets_the_same_standard_input() {
        // The shell pins the engine's TLS key in Chromium once; a restarted engine must be
        // handed the same identity, and the child must see its input end (stdin closed).
        let dir = std::env::temp_dir().join(format!("rustet-sup-stdin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("seen");
        let _ = std::fs::remove_file(&out);
        let log = Log::default();
        let sup = EngineSupervisor::new(Echo { out: out.clone() }, fast(), log.sink());
        sup.start().await;
        for _ in 0..100 {
            if log.count("exited") >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        sup.stop().await;
        let seen = std::fs::read_to_string(&out).unwrap();
        let runs: Vec<&str> = seen.lines().collect();
        assert!(runs.len() >= 2, "restarted: {runs:?}");
        assert!(runs.iter().all(|r| *r == "identity"), "{runs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Default, Clone)]
    struct Log(Arc<SyncMutex<Vec<String>>>);

    impl Log {
        fn sink(&self) -> impl Fn(EngineEvent) + Send + Sync + 'static {
            let log = self.clone();
            move |e| {
                let s = match e {
                    EngineEvent::Started { .. } => "started".to_owned(),
                    EngineEvent::Exited { code, signal } => format!("exited {code:?} {signal:?}"),
                    EngineEvent::Restarting { .. } => "restarting".to_owned(),
                    EngineEvent::StartFailed(e) => format!("failed {e}"),
                };
                log.0.lock().unwrap().push(s);
            }
        }
        fn count(&self, prefix: &str) -> usize {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.starts_with(prefix))
                .count()
        }
    }

    fn fast() -> SupervisorTiming {
        SupervisorTiming {
            restart_delay: Duration::from_millis(5),
            kill_delay: Duration::from_millis(300),
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    #[tokio::test]
    async fn concurrent_starts_share_one_preparation_and_one_child() {
        let log = Log::default();
        let sup = EngineSupervisor::new(Fake::new("sleep 30"), fast(), log.sink());
        tokio::join!(sup.start(), sup.start(), sup.start());
        assert_eq!(sup.0.launcher.prepares.load(Ordering::SeqCst), 1);
        assert_eq!(log.count("started"), 1);
        let t = Instant::now();
        sup.stop().await;
        assert!(
            t.elapsed() < Duration::from_millis(300),
            "SIGTERM is enough for a well-behaved child"
        );
        assert!(!sup.is_running());
    }

    #[tokio::test]
    async fn quit_during_preparation_prevents_spawning() {
        let log = Log::default();
        let hold = Arc::new(Notify::new());
        let fake = Fake {
            hold: Some(hold.clone()),
            ..Fake::new("sleep 30")
        };
        let sup = EngineSupervisor::new(fake, fast(), log.sink());
        let starting = tokio::spawn({
            let sup = sup.clone();
            async move { sup.start().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let stopping = tokio::spawn({
            let sup = sup.clone();
            async move { sup.stop().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        hold.notify_one();
        starting.await.unwrap();
        stopping.await.unwrap();
        sup.start().await;
        assert_eq!(log.count("started"), 0);
    }

    #[tokio::test]
    async fn stop_cancels_a_pending_crash_restart() {
        let log = Log::default();
        let timing = SupervisorTiming {
            restart_delay: Duration::from_millis(100),
            ..fast()
        };
        let sup = EngineSupervisor::new(Fake::new("exit 1"), timing, log.sink());
        sup.start().await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(log.count("restarting"), 1);
        sup.stop().await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(log.count("started"), 1);
    }

    #[tokio::test]
    async fn a_crash_and_exit_code_3_restart() {
        let log = Log::default();
        let sup = EngineSupervisor::new(Fake::new("exit 3"), fast(), log.sink());
        sup.start().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(log.count("started") >= 2, "{:?}", log.0.lock().unwrap());
        assert!(
            log.0
                .lock()
                .unwrap()
                .iter()
                .any(|s| s == "exited Some(3) None")
        );
        sup.stop().await;
    }

    #[tokio::test]
    async fn backoff_doubles_for_quick_crashes() {
        let log = Log::default();
        let sup = EngineSupervisor::new(
            Fake::new("exit 1"),
            SupervisorTiming {
                restart_delay: Duration::from_millis(20),
                ..fast()
            },
            log.sink(),
        );
        sup.start().await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        sup.stop().await;
        // 40, 80, 160 ms: three or four starts in 250 ms, not a dozen.
        let starts = log.count("started");
        assert!((2..=4).contains(&starts), "{starts}");
    }

    #[tokio::test]
    async fn a_failed_exec_is_reported_once_without_retries() {
        let log = Log::default();
        let fake = Fake {
            program: "/nonexistent/rustet-test-engine",
            ..Fake::new("")
        };
        let sup = EngineSupervisor::new(fake, fast(), log.sink());
        sup.start().await;
        settle().await;
        assert_eq!(log.count("failed"), 1);
        assert_eq!(log.count("started"), 0);
        assert_eq!(log.count("restarting"), 0);
        sup.stop().await;
    }

    #[tokio::test]
    async fn shutdown_escalates_only_for_an_unresponsive_child() {
        let log = Log::default();
        let sup =
            EngineSupervisor::new(Fake::new("trap '' TERM; exec sleep 30"), fast(), log.sink());
        sup.start().await;
        settle().await;
        let t = Instant::now();
        sup.stop().await;
        assert!(
            t.elapsed() >= Duration::from_millis(300),
            "waited the kill delay"
        );
        assert!(
            log.0
                .lock()
                .unwrap()
                .iter()
                .any(|s| s == "exited None Some(9)"),
            "{:?}",
            log.0.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn reload_restarts_a_running_engine_and_is_a_no_op_once_stopped() {
        let log = Log::default();
        let sup = EngineSupervisor::new(Fake::new("sleep 30"), fast(), log.sink());
        sup.start().await;
        sup.reload();
        settle().await;
        assert_eq!(
            log.count("started"),
            2,
            "the engine comes back after a reload"
        );
        assert!(
            log.0
                .lock()
                .unwrap()
                .iter()
                .any(|s| s == "exited None Some(15)")
        );
        sup.stop().await;
        sup.reload();
        settle().await;
        assert_eq!(log.count("started"), 2, "no new engine after stop");
    }
}
