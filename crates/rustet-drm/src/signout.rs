//! Sign-out: the gate that keeps sessions from starting while one runs, and the sign-out itself.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::watch;
use tracing::warn;

use crate::error::Error;
use crate::logout_files::LogoutScope;
use crate::manager::DrmManager;
use crate::session::SessionError;
use crate::state::ManagerState;

/// Whether sessions may be started or reused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Gate {
    /// No sign-out work.
    #[default]
    Open,
    /// A sign-out request owns the marker and the backend.
    SigningOut,
    /// A pending sign-out on disk is being (or still has to be) completed.
    Replaying,
}

#[derive(Default)]
pub(crate) struct GateInner {
    pub(crate) gate: Gate,
    /// The one cleanup pass that may run at a time. Callers that find one running join it.
    pub(crate) cleanup: Option<CleanupOp>,
}

impl GateInner {
    pub(crate) fn signing_out(&self) -> bool {
        self.gate == Gate::SigningOut
    }

    /// Open, with no cleanup running.
    pub(crate) fn is_clear(&self) -> bool {
        self.gate == Gate::Open && self.cleanup.is_none()
    }
}

/// A running cleanup pass; it resolves to the pass's outcome.
#[derive(Clone)]
pub(crate) struct CleanupOp(watch::Receiver<Option<Result<(), Arc<SessionError>>>>);

impl CleanupOp {
    async fn finished(mut self) -> Result<(), Arc<SessionError>> {
        match self.0.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone().unwrap_or(Ok(())),
            // The pass task ended without an outcome (a panic).
            Err(_) => Err(Arc::new(SessionError::Aborted)),
        }
    }
}

/// Waits for a cleanup pass, at most `wait`.
pub(crate) async fn wait_op(op: CleanupOp, wait: Duration) -> Result<(), Error> {
    match tokio::time::timeout(wait, op.finished()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(cause)) => Err(Error::LogoutPending { cause: Some(cause) }),
        Err(_) => Err(Error::LogoutPending { cause: None }),
    }
}

/// Bounds every wait in a sign-out.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SignOutTiming {
    /// The whole sign-out request.
    pub route: Duration,
    /// Waiting for a sign-in in progress to finish.
    pub auth_wait: Duration,
    /// A start waiting for a pending sign-out's cleanup.
    pub replay_wait: Duration,
}

impl Default for SignOutTiming {
    fn default() -> Self {
        Self {
            route: Duration::from_secs(45),
            auth_wait: Duration::from_secs(15),
            replay_wait: Duration::from_secs(15),
        }
    }
}

/// Whether the account was also signed out on Apple's servers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerSignOut {
    /// Always, until the engine can sign out on Apple's servers.
    #[default]
    Skipped,
}

/// What a completed sign-out reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignOutResult {
    pub server_sign_out: ServerSignOut,
}

impl DrmManager {
    /// Starts the one cleanup pass. The caller holds the gate lock. A replay moves the gate to
    /// `Replaying`; a sign-out's own wipe keeps `SigningOut`, and the sign-out decides the state
    /// afterwards.
    pub(crate) fn start_cleanup_locked(&self, g: &mut GateInner) -> CleanupOp {
        let (tx, rx) = watch::channel(None);
        let op = CleanupOp(rx);
        g.cleanup = Some(op.clone());
        if g.gate != Gate::SigningOut {
            g.gate = Gate::Replaying;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let m = &me.inner;
            // A pending request may meet a running backend (a backstop written while the engine
            // ran): stop it first, or the native library writes the files back. No start can run
            // meanwhile: the gate is closed.
            if m.backend.running() {
                let _ = m.backend.stop().await;
            }
            let session = m.session.clone();
            let outcome = tokio::task::spawn_blocking(move || session.run_pending_pass())
                .await
                .unwrap_or(Err(SessionError::Aborted))
                .map_err(Arc::new);
            {
                let mut g = m
                    .gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                g.cleanup = None;
                if outcome.is_ok() && g.gate == Gate::Replaying {
                    g.gate = Gate::Open;
                }
            }
            tx.send_replace(Some(outcome));
        });
        op
    }

    /// The gate check every start goes through, with `auth_sem` held. It decides under the gate
    /// lock, waits for a cleanup without it, and returns `Ok` only once the gate is open and no
    /// sign-out request is on disk.
    pub(crate) async fn await_clear_gate(&self, replay: bool) -> Result<(), Error> {
        let m = &self.inner;
        for _ in 0..3 {
            let op = {
                let mut g = m
                    .gate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if g.signing_out() {
                    return Err(Error::LogoutPending { cause: None });
                }
                if let Some(op) = &g.cleanup {
                    op.clone()
                } else if m.session.has_pending() {
                    if !replay {
                        return Err(Error::LogoutPending { cause: None });
                    }
                    self.start_cleanup_locked(&mut g)
                } else {
                    g.gate = Gate::Open;
                    return Ok(());
                }
            };
            wait_op(op, m.timing.replay_wait).await?;
        }
        Err(Error::LogoutPending { cause: None })
    }

    /// Whether a sign-out is running or still has to complete.
    pub fn sign_out_pending(&self) -> bool {
        let m = &self.inner;
        let g = m
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !g.is_clear() || m.session.has_pending()
    }

    /// Whether a sign-out needs the process to restart (see [`Error::RestartRequired`]).
    pub fn restart_pending(&self) -> bool {
        self.inner.restart_pending.load(Ordering::SeqCst)
    }

    /// Signs the account out: the session ends and Apple's account files go. The device
    /// identity stays, so signing in again here normally needs no verification code.
    pub async fn logout(&self) -> Result<SignOutResult, Error> {
        self.sign_out(LogoutScope::Account).await
    }

    /// Signs out and removes the device identity too, like "Deauthorize This Computer": the
    /// next sign-in comes from a new device and asks for a code.
    pub async fn forget_device(&self) -> Result<SignOutResult, Error> {
        self.sign_out(LogoutScope::Device).await
    }

    async fn sign_out(&self, scope: LogoutScope) -> Result<SignOutResult, Error> {
        let m = &self.inner;
        let deadline = Instant::now() + m.timing.route;
        let left = || deadline.saturating_duration_since(Instant::now());

        // One sign-out at a time: a second one (Forget This Device right after Sign Out) waits,
        // then runs with its own scope.
        let Ok(Ok(_serial)) = tokio::time::timeout(left(), m.sign_out_sem.acquire()).await else {
            return Err(Error::SignOutBusy);
        };

        // A replay in progress finishes first, so this request's marker is not consumed by a
        // pass that started with a weaker scope.
        let running = m
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cleanup
            .clone();
        if let Some(op) = running
            && let Err(Error::LogoutPending { .. }) = wait_op(op, left()).await
            && left().is_zero()
        {
            return self.restart_path("a pending sign-out did not finish");
        }

        // Close the gate, then record the request. A start that checks the gate after this sees
        // the sign-out; one that checked before ran its replay before the marker existed. The
        // marker is written without the gate lock, so no thread waits on an fsync for it.
        let prev = std::mem::replace(
            &mut m
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .gate,
            Gate::SigningOut,
        );
        let session = m.session.clone();
        let marked = tokio::task::spawn_blocking(move || session.mark_logout_pending(scope))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        if let Err(e) = marked {
            m.gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .gate = prev;
            return Err(Error::RecordSignOut(e));
        }
        m.hub.set_manager_state(ManagerState::ShuttingDown);

        // End a sign-in waiting for a verification code, then wait for the sign-in to return:
        // nothing may stop the backend or delete files under it.
        m.auth.abort();
        let wait = m.timing.auth_wait.min(left());
        let Ok(Ok(_start_permit)) =
            tokio::time::timeout(wait, Arc::clone(&m.auth_sem).acquire_owned()).await
        else {
            return self.restart_path("a sign-in did not finish");
        };

        let stop = m.backend.stop().await.err();

        let op = self.start_cleanup_locked(
            &mut m
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let Ok(outcome) = tokio::time::timeout(left(), op.finished()).await else {
            return self.restart_path("the sign-out cleanup did not finish in time");
        };
        // On failure the request stays on disk; later starts retry or join the cleanup.
        m.gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gate = if outcome.is_ok() {
            Gate::Open
        } else {
            Gate::Replaying
        };
        if let Err(clear) = outcome {
            return Err(Error::SignOutClear { clear, stop });
        }

        m.hub.reset_signed_out();
        match stop {
            Some(e) => Err(Error::SignOutStop(e)),
            None => Ok(SignOutResult::default()),
        }
    }

    /// Leaves the recorded request for the next process: the gate stays closed here, nothing is
    /// stopped or deleted under native code that may still be running, and the caller answers
    /// and then ends the process.
    fn restart_path(&self, why: &'static str) -> Result<SignOutResult, Error> {
        warn!("drm: sign-out finishes after the engine restarts: {why}");
        self.inner.restart_pending.store(true, Ordering::SeqCst);
        Err(Error::RestartRequired(why))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, mpsc};

    use super::*;
    use crate::auth::{AuthChallenge, AuthChallengeType};
    use crate::backend::{BackendError, Credentials};
    use crate::deviceid::{LEGACY_ANDROID_ID, device_info};
    use crate::logout_files::{FileHooks, sync_directory, write_durable};
    use crate::manager::RestartPolicy;
    use crate::session::SessionManager;
    use crate::state::{AuthState, SessionState};
    use crate::test_support::{
        ACCOUNT_NAMES, DEVICE_NAMES, Fake, assert_gone, assert_kept, manager, manager_with,
        no_requests, policy, write, write_signed_in,
    };

    const MARKER: &str = "logout-pending";

    fn creds() -> Credentials {
        Credentials {
            email: "a@b.c".into(),
            password: "p".into(),
        }
    }

    fn gate(m: &DrmManager) -> Gate {
        m.inner.gate.lock().unwrap().gate
    }

    fn remove_all(p: &Path) -> io::Result<()> {
        if p.is_dir() {
            fs::remove_dir_all(p)
        } else {
            fs::remove_file(p)
        }
    }

    /// Hooks whose `remove_all` fails on `mpl_db` while `fail` is set.
    fn failing_mpl_db(fail: Arc<AtomicBool>) -> FileHooks {
        FileHooks {
            remove_all: Box::new(move |p| {
                if fail.load(Ordering::SeqCst) && p.ends_with("mpl_db") {
                    Err(io::Error::other("busy"))
                } else {
                    remove_all(p)
                }
            }),
            ..FileHooks::default()
        }
    }

    /// Hooks whose `remove_all` blocks once on `mpl_db`: `blocked` reports it, `release` ends it.
    fn blocking_mpl_db() -> (
        FileHooks,
        tokio::sync::oneshot::Receiver<()>,
        mpsc::Sender<()>,
    ) {
        let (blocked_tx, blocked) = tokio::sync::oneshot::channel();
        let (release, release_rx) = mpsc::channel();
        let once = Mutex::new(Some((blocked_tx, release_rx)));
        let hooks = FileHooks {
            remove_all: Box::new(move |p| {
                if p.ends_with("mpl_db")
                    && let Some((blocked, release)) = once.lock().unwrap().take()
                {
                    let _ = blocked.send(());
                    let _ = release.recv();
                }
                remove_all(p)
            }),
            ..FileHooks::default()
        };
        (hooks, blocked, release)
    }

    fn all_account_files() -> Vec<&'static str> {
        ACCOUNT_NAMES.iter().copied().chain(["mpl_db"]).collect()
    }

    // ── Scopes and the device ID ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sign_out_keeps_the_device_identity() {
        for id in [LEGACY_ANDROID_ID, "0123456789abcdef"] {
            let dir = tempfile::tempdir().unwrap();
            let dir = dir.path();
            write_signed_in(dir, id);
            let (m, _) = manager(dir, Fake::new(), policy()).await;
            assert_eq!(
                m.logout().await.unwrap().server_sign_out,
                ServerSignOut::Skipped
            );
            assert_gone(dir, &all_account_files());
            assert_kept(dir, DEVICE_NAMES);
            no_requests(dir);
            assert!(
                device_info(dir).unwrap().ends_with(id),
                "the device ID changed after sign-out"
            );
            assert_eq!(gate(&m), Gate::Open);
            let s = m.status();
            assert_eq!(
                (s.state.authentication, s.state.session),
                (AuthState::LoggedOut, SessionState::Empty)
            );
            assert_eq!(s.state.manager, ManagerState::Ready);
            assert!(!s.capabilities.cbcs);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forget_device_gives_a_new_device() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let (m, _) = manager(dir, Fake::new(), policy()).await;
        m.forget_device().await.unwrap();
        assert_gone(dir, &all_account_files());
        assert_gone(dir, DEVICE_NAMES);
        no_requests(dir);
        assert!(
            !device_info(dir).unwrap().ends_with(LEGACY_ANDROID_ID),
            "Forget This Device must give a new ID"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sign_out_clears_fair_play_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        write_signed_in(dir.path(), LEGACY_ANDROID_ID);
        let fake = Fake::new();
        fake.running.store(true, Ordering::SeqCst);
        let (m, fake) = manager(dir.path(), fake, policy()).await;
        m.inner.hub.merge(crate::state::StateUpdate::new(
            crate::state::DrmState {
                fair_play: crate::state::FairPlayState::Ready,
                ..Default::default()
            },
            "",
        ));
        m.logout().await.unwrap();
        assert_eq!(
            m.status().capabilities,
            crate::state::CapabilityState::default()
        );
        assert_eq!(
            m.status().state.process,
            crate::state::ProcessState::Stopped
        );
        assert!(fake.stops.load(Ordering::SeqCst) >= 1);
    }

    // ── Failures before anything changes ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_marker_write_failure_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let (m, fake) = manager(dir, Fake::new(), policy()).await;
        m.inner.session.set_hooks(FileHooks {
            write_marker: Box::new(|_, _| Err(io::Error::other("disk full"))),
            ..FileHooks::default()
        });
        let before = m.status();
        assert!(matches!(m.logout().await, Err(Error::RecordSignOut(_))));
        assert_eq!(m.status(), before);
        assert_eq!(
            fake.stops.load(Ordering::SeqCst),
            0,
            "the backend must not be stopped"
        );
        assert_kept(dir, &all_account_files());
        assert_kept(dir, DEVICE_NAMES);
        assert_eq!(gate(&m), Gate::Open, "the gate must be restored");
    }

    // ── Sign-in and sign-out ordering ─────────────────────────────────────────

    // Signing out while Apple waits for a code returns promptly and completely.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sign_out_during_a_verification_code_is_immediate_and_complete() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let fake = Fake::new();
        fake.set_start(|auth| async move {
            let req = AuthChallenge {
                kind: AuthChallengeType::TwoFactor,
                ..Default::default()
            };
            tokio::task::spawn_blocking(move || auth.challenge(req, Duration::from_secs(60)))
                .await
                .unwrap()
                .map(drop)
                .map_err(|e| BackendError::Failed(e.to_string()))
        });
        let (m, _) = manager(dir, fake, policy()).await;
        let sign_in = tokio::spawn({
            let m = m.clone();
            async move { m.authenticate(creds()).await }
        });
        crate::test_support::wait_until(|| m.inner.auth.pending()).await;

        let out = tokio::time::timeout(Duration::from_secs(3), m.logout())
            .await
            .expect("blocked on the pending code");
        out.unwrap();
        assert!(
            sign_in.await.unwrap().is_err(),
            "the cancelled sign-in must fail"
        );
        assert_gone(dir, &all_account_files());
        assert_kept(dir, DEVICE_NAMES);
        assert!(
            !m.inner.auth.has_stored_credentials(),
            "sign-out must forget the credentials held in memory"
        );
        let st = m.status().state;
        assert_eq!(
            (st.session, st.authentication),
            (SessionState::Empty, AuthState::LoggedOut)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sign_out_wipes_even_when_stop_fails() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let fake = Fake::new();
        *fake.stop_error.lock().unwrap() = Some("stuck".into());
        let (m, _) = manager(dir, fake, policy()).await;
        assert!(matches!(m.logout().await, Err(Error::SignOutStop(_))));
        assert_gone(dir, &all_account_files());
        no_requests(dir);
    }

    // A sign-in stuck in native code: sign-out leaves the request on disk and asks for a
    // restart instead of stopping or deleting under it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stuck_sign_in_takes_the_restart_path() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let fake = Fake::new();
        let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
        let entered_tx = Mutex::new(Some(entered_tx));
        let stuck = Arc::new(tokio::sync::Notify::new());
        fake.set_start({
            let stuck = stuck.clone();
            move |_| {
                let (stuck, entered) = (stuck.clone(), entered_tx.lock().unwrap().take());
                async move {
                    entered.map(|e| e.send(()));
                    stuck.notified().await;
                    Ok(())
                }
            }
        });
        let timing = SignOutTiming {
            auth_wait: Duration::from_millis(50),
            ..crate::test_support::FAST_TIMING
        };
        let (m, fake) = manager_with(dir, fake, policy(), timing).await;
        tokio::spawn({
            let m = m.clone();
            async move { m.authenticate(creds()).await }
        });
        entered.await.unwrap();

        assert!(matches!(m.logout().await, Err(Error::RestartRequired(_))));
        assert!(m.restart_pending());
        assert_eq!(
            fake.stops.load(Ordering::SeqCst),
            0,
            "stop must be skipped while a start is stuck"
        );
        assert_kept(dir, ACCOUNT_NAMES);
        assert_kept(dir, &[MARKER]);

        // The next process completes the request at construction.
        let _next = manager(dir, Fake::new(), policy()).await;
        assert!(
            matches!(
                m.await_clear_gate(true).await,
                Err(Error::LogoutPending { .. })
            ),
            "the old gate stays closed"
        );
        stuck.notify_one();
        assert_gone(dir, ACCOUNT_NAMES);
        assert_gone(dir, &[MARKER]);
        assert_kept(dir, DEVICE_NAMES);
    }

    // A request left by an earlier process is completed before a sign-in starts, and the new
    // session survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stale_marker_is_consumed_before_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let path = dir.path().to_owned();
        fake.set_start(move |_| {
            let path = path.clone();
            async move {
                if path.join(MARKER).exists() {
                    return Err(BackendError::Failed(
                        "started with a request still on disk".into(),
                    ));
                }
                write(&path, "session-ready", "1");
                Ok(())
            }
        });
        let (m, _) = manager(dir.path(), fake, policy()).await;
        write_signed_in(dir.path(), LEGACY_ANDROID_ID);
        write(dir.path(), MARKER, "account"); // appears after construction
        m.authenticate(creds()).await.unwrap();
        assert_kept(dir.path(), &["session-ready"]);
        no_requests(dir.path());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_replay_blocks_sign_in_until_it_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        write(dir, MARKER, "account");
        let fail = Arc::new(AtomicBool::new(true));
        let session = Arc::new(SessionManager::new(dir));
        session.set_hooks(failing_mpl_db(fail.clone()));
        let fake = Fake::new();
        let m = DrmManager::with_timing(
            fake.clone(),
            session,
            Default::default(),
            policy(),
            crate::test_support::FAST_TIMING,
        )
        .await;
        assert_eq!(m.status().message, "sign-out incomplete");
        assert!(matches!(
            m.authenticate(creds()).await,
            Err(Error::LogoutPending { cause: Some(_) })
        ));
        assert!(matches!(
            m.ensure_running().await,
            Err(Error::LogoutPending { .. })
        ));
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
        assert!(!m.inner.auth.has_stored_credentials());
        assert_kept(dir, &[MARKER]);

        fail.store(false, Ordering::SeqCst);
        m.authenticate(creds()).await.unwrap();
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            1,
            "the sign-in must run once the sign-out completed"
        );
        no_requests(dir);
    }

    // The gate closes before the request is recorded: a sign-in arriving in between is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_sign_in_during_the_gate_close_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let (m, fake) = manager(dir, Fake::new(), policy()).await;
        let (in_write_tx, in_write) = tokio::sync::oneshot::channel();
        let (proceed, proceed_rx) = mpsc::channel::<()>();
        let once = Mutex::new(Some((in_write_tx, proceed_rx)));
        m.inner.session.set_hooks(FileHooks {
            write_marker: Box::new(move |p, d| {
                if let Some((in_write, proceed)) = once.lock().unwrap().take() {
                    let _ = in_write.send(());
                    let _ = proceed.recv();
                }
                write_durable(p, d)
            }),
            ..FileHooks::default()
        });
        let logout = tokio::spawn({
            let m = m.clone();
            async move { m.logout().await }
        });
        in_write.await.unwrap();
        let sign_in = tokio::spawn({
            let m = m.clone();
            async move { m.authenticate(creds()).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        proceed.send(()).unwrap();
        assert!(matches!(
            sign_in.await.unwrap(),
            Err(Error::LogoutPending { .. })
        ));
        logout.await.unwrap().unwrap();
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            0,
            "no start during the sign-out"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_second_sign_out_waits_and_runs_its_own_scope() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let (m, _) = manager(dir, Fake::new(), policy()).await;
        let (hooks, blocked, release) = blocking_mpl_db();
        m.inner.session.set_hooks(hooks);
        let first = tokio::spawn({
            let m = m.clone();
            async move { m.logout().await }
        });
        blocked.await.unwrap();
        let second = tokio::spawn({
            let m = m.clone();
            async move { m.forget_device().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            fs::read_to_string(dir.join(MARKER)).unwrap(),
            "account",
            "the second request touched the marker"
        );
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_gone(dir, DEVICE_NAMES);
        no_requests(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_backstop_written_during_cleanup_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let (m, _) = manager(dir, Fake::new(), policy()).await;
        let (hooks, blocked, release) = blocking_mpl_db();
        m.inner.session.set_hooks(hooks);
        let logout = tokio::spawn({
            let m = m.clone();
            async move { m.logout().await }
        });
        blocked.await.unwrap();
        write(
            dir,
            &format!("logout-pending.backstop-{}", "d".repeat(32)),
            "device",
        );
        release.send(()).unwrap();
        logout.await.unwrap().unwrap();
        assert_gone(dir, DEVICE_NAMES);
        no_requests(dir);
        assert_eq!(
            gate(&m),
            Gate::Open,
            "the gate opens only after the late backstop was completed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_backstop_is_recovered_by_the_next_process() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        write(
            dir,
            &format!("logout-pending.backstop-{}", "e".repeat(32)),
            "device",
        );
        let (m, _) = manager(dir, Fake::new(), policy()).await;
        assert_gone(dir, DEVICE_NAMES);
        no_requests(dir);
        assert_eq!(m.status().state.session, SessionState::Empty);
        assert_eq!(gate(&m), Gate::Open);
        assert!(!m.sign_out_pending());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_directory_sync_failure_keeps_the_gate_closed() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        write(dir, MARKER, "account");
        let fail = Arc::new(AtomicBool::new(true));
        let session = Arc::new(SessionManager::new(dir));
        session.set_hooks(FileHooks {
            sync_dir: Box::new({
                let fail = fail.clone();
                move |d| {
                    if fail.load(Ordering::SeqCst) {
                        Err(io::Error::other("EIO"))
                    } else {
                        sync_directory(d)
                    }
                }
            }),
            ..FileHooks::default()
        });
        let fake = Fake::new();
        let m = DrmManager::with_timing(
            fake.clone(),
            session,
            Default::default(),
            policy(),
            crate::test_support::FAST_TIMING,
        )
        .await;
        assert_eq!(
            gate(&m),
            Gate::Replaying,
            "the gate must stay closed after a failed sync"
        );
        assert!(m.sign_out_pending());
        assert!(matches!(
            m.authenticate(creds()).await,
            Err(Error::LogoutPending { .. })
        ));
        fail.store(false, Ordering::SeqCst);
        m.authenticate(creds()).await.unwrap();
        assert_eq!(gate(&m), Gate::Open);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_sign_out_wipe_leaves_replaying() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_signed_in(dir, LEGACY_ANDROID_ID);
        let (m, _) = manager(dir, Fake::new(), policy()).await;
        let fail = Arc::new(AtomicBool::new(true));
        m.inner.session.set_hooks(failing_mpl_db(fail.clone()));
        assert!(matches!(m.logout().await, Err(Error::SignOutClear { .. })));
        assert_eq!(gate(&m), Gate::Replaying);
        fail.store(false, Ordering::SeqCst);
        m.await_clear_gate(true).await.unwrap();
        no_requests(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nothing_starts_during_a_sign_out() {
        let dir = tempfile::tempdir().unwrap();
        write_signed_in(dir.path(), LEGACY_ANDROID_ID);
        let fake = Fake::new();
        fake.running.store(true, Ordering::SeqCst);
        let p = RestartPolicy {
            restart_backoff: vec![Duration::from_millis(1)],
            ..policy()
        };
        let (m, fake) = manager(dir.path(), fake, p).await;
        m.inner.gate.lock().unwrap().gate = Gate::SigningOut;
        assert!(
            matches!(m.ensure_running().await, Err(Error::LogoutPending { .. })),
            "a running backend must not be reused during a sign-out"
        );
        fake.running.store(false, Ordering::SeqCst);
        m.handle_crash().await;
        assert_eq!(
            fake.starts.load(Ordering::SeqCst),
            0,
            "a crash restart must not run during a sign-out"
        );
    }
}
