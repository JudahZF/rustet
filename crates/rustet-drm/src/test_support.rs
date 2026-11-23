//! A fake backend and profile helpers for the manager and sign-out tests.

use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthCoordinator;
use crate::backend::{
    AccountInfo, BackendConfig, BackendError, BackendLink, Credentials, DrmBackend,
    ProgressiveMvUrl,
};
use crate::manager::{DrmManager, RestartPolicy};
use crate::session::SessionManager;
use crate::signout::SignOutTiming;
use crate::state::{DrmEvent, StateUpdate};

pub(crate) const ACCOUNT_NAMES: &[&str] = &[
    "session-ready",
    "MUSIC_TOKEN",
    "STOREFRONT_ID",
    "drm-state",
    "33F633BF6AC7D676168B84C3594A7941AF2BEC09",
];
pub(crate) const DEVICE_NAMES: &[&str] = &["device-id", "adi.pb", "IC-Info.sids", "fsi.pdat"];

pub(crate) const FAST_TIMING: SignOutTiming = SignOutTiming {
    route: Duration::from_secs(2),
    auth_wait: Duration::from_secs(1),
    replay_wait: Duration::from_secs(1),
};

pub(crate) fn policy() -> RestartPolicy {
    RestartPolicy {
        max_crash_restarts: 3,
        restart_backoff: vec![Duration::from_millis(10), Duration::from_millis(20)],
        startup_timeout: Duration::from_secs(5),
        auth_timeout: Duration::from_secs(5),
    }
}

pub(crate) async fn manager(
    dir: &Path,
    fake: Arc<Fake>,
    policy: RestartPolicy,
) -> (DrmManager, Arc<Fake>) {
    manager_with(dir, fake, policy, FAST_TIMING).await
}

pub(crate) async fn manager_with(
    dir: &Path,
    fake: Arc<Fake>,
    policy: RestartPolicy,
    timing: SignOutTiming,
) -> (DrmManager, Arc<Fake>) {
    let session = Arc::new(SessionManager::new(dir));
    let m = DrmManager::with_timing(
        fake.clone(),
        session,
        BackendConfig::default(),
        policy,
        timing,
    )
    .await;
    (m, fake)
}

pub(crate) async fn wait_until(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !cond() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub(crate) fn write(dir: &Path, name: &str, content: &str) {
    fs::write(dir.join(name), content).unwrap();
}

/// A minimal signed-in profile: what `has_session` looks for.
pub(crate) fn make_session(dir: &Path) {
    fs::create_dir_all(dir.join("mpl_db")).unwrap();
    write(dir, "mpl_db/accounts.sqlitedb", "mock_session");
    write(dir, "session-ready", "1");
}

/// A signed-in profile with every account and device file, and the given device ID.
pub(crate) fn write_signed_in(dir: &Path, device_id: &str) {
    for n in ACCOUNT_NAMES.iter().chain(&DEVICE_NAMES[1..]) {
        write(dir, n, "x");
    }
    write(dir, "device-id", &format!("{device_id}\n"));
    make_session(dir);
}

pub(crate) fn assert_gone(dir: &Path, names: &[&str]) {
    let survived: Vec<_> = names.iter().filter(|n| dir.join(n).exists()).collect();
    assert!(survived.is_empty(), "survived: {survived:?}");
}

pub(crate) fn assert_kept(dir: &Path, names: &[&str]) {
    let removed: Vec<_> = names.iter().filter(|n| !dir.join(n).exists()).collect();
    assert!(removed.is_empty(), "removed: {removed:?}");
}

pub(crate) fn no_requests(dir: &Path) {
    let left: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("logout-pending"))
        .collect();
    assert!(left.is_empty(), "sign-out requests left behind: {left:?}");
}

type StartFn =
    Arc<dyn Fn(Arc<AuthCoordinator>) -> BoxFuture<'static, Result<(), BackendError>> + Send + Sync>;

/// Behaves like the native backend where the manager depends on it: a start holds the backend
/// (a sign-in waiting for a code keeps holding it), and a stop needs it.
#[derive(Default)]
pub(crate) struct Fake {
    pub running: AtomicBool,
    pub starts: AtomicU32,
    pub stops: AtomicU32,
    pub stop_error: Mutex<Option<String>>,
    pub account: Mutex<AccountInfo>,
    pub last_credentials: Mutex<Option<Credentials>>,
    pub retries: Mutex<Vec<String>>,
    busy: tokio::sync::Mutex<()>,
    start_fn: Mutex<Option<StartFn>>,
    emit_on_start: Mutex<Vec<DrmEvent>>,
    link: Mutex<Option<(Arc<AuthCoordinator>, mpsc::Sender<DrmEvent>)>>,
}

impl Fake {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// What a start does, given the auth coordinator, before it succeeds.
    pub fn set_start<F, Fut>(&self, f: F)
    where
        F: Fn(Arc<AuthCoordinator>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), BackendError>> + Send + 'static,
    {
        *self.start_fn.lock().unwrap() = Some(Arc::new(move |a| Box::pin(f(a))));
    }

    pub fn set_emit_on_start(&self, events: Vec<DrmEvent>) {
        *self.emit_on_start.lock().unwrap() = events;
    }

    pub fn auth(&self) -> Arc<AuthCoordinator> {
        self.link
            .lock()
            .unwrap()
            .as_ref()
            .expect("attached")
            .0
            .clone()
    }

    pub fn send(&self, ev: DrmEvent) {
        let tx = self
            .link
            .lock()
            .unwrap()
            .as_ref()
            .expect("attached")
            .1
            .clone();
        tx.try_send(ev).unwrap();
    }

    pub fn emit(&self, update: StateUpdate) {
        self.send(DrmEvent {
            update,
            intentional: false,
        });
    }
}

#[async_trait]
impl DrmBackend for Fake {
    fn attach(&self, link: BackendLink) {
        *self.link.lock().unwrap() = Some((link.auth, link.events));
    }

    async fn start(
        &self,
        cfg: BackendConfig,
        _cancel: CancellationToken,
    ) -> Result<(), BackendError> {
        let _busy = self.busy.lock().await;
        self.starts.fetch_add(1, Ordering::SeqCst);
        *self.last_credentials.lock().unwrap() = cfg.credentials;
        let start_fn = self.start_fn.lock().unwrap().clone();
        if let Some(f) = start_fn {
            f(self.auth()).await?;
        }
        self.running.store(true, Ordering::SeqCst);
        let events = self.emit_on_start.lock().unwrap().clone();
        for ev in events {
            self.send(ev);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), BackendError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        let _busy = self.busy.lock().await;
        self.running.store(false, Ordering::SeqCst);
        match self.stop_error.lock().unwrap().clone() {
            Some(e) => Err(BackendError::Failed(e)),
            None => Ok(()),
        }
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn get_m3u8(&self, _adam_id: u64) -> Result<String, BackendError> {
        Ok("https://example.invalid/master.m3u8".into())
    }

    async fn get_account(&self) -> Result<AccountInfo, BackendError> {
        Ok(self.account.lock().unwrap().clone())
    }

    async fn get_progressive_mv_url(
        &self,
        _adam_id: u64,
    ) -> Result<ProgressiveMvUrl, BackendError> {
        Err(BackendError::Failed("not implemented by the fake".into()))
    }

    async fn decrypt_itun_samples(
        &self,
        _adam_id: u64,
        _samples: Vec<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>, BackendError> {
        Err(BackendError::Failed("not implemented by the fake".into()))
    }

    async fn dial_cbcs(&self, _cancel: CancellationToken) -> Result<DuplexStream, BackendError> {
        Err(BackendError::Failed("not implemented by the fake".into()))
    }

    async fn retry_fair_play(&self, music_user_token: String) {
        self.retries.lock().unwrap().push(music_user_token);
    }
}
