//! The engine epoch and the lifecycle coordinator that advances it.
//!
//! The epoch generation goes into every SSE event. A client discards an event whose generation
//! is older than the last `engine.snapshot` it received: the event belongs to an earlier engine
//! state.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::task::AbortHandle;

/// Apple's approximate FairPlay session lifetime.
pub(crate) const DRM_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long before [`DRM_SESSION_TTL`] the `drm.refresh_due` event fires.
pub(crate) const DRM_REFRESH_LEAD: Duration = Duration::from_secs(5 * 60);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why the epoch advanced. Serialised into the `engine.snapshot` metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum EpochReason {
    #[serde(rename = "engine-start")]
    EngineStart,
    #[serde(rename = "session-changed")]
    SessionChanged,
}

/// One epoch. Generation 0 is never used: it means "client has never seen a snapshot".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EpochInfo {
    pub generation: u64,
    pub reason: EpochReason,
    /// When this epoch began.
    pub since: SystemTime,
}

pub(crate) struct Epoch(Mutex<EpochInfo>);

impl Epoch {
    pub fn new() -> Self {
        Self(Mutex::new(EpochInfo {
            generation: 1,
            reason: EpochReason::EngineStart,
            since: SystemTime::now(),
        }))
    }

    pub fn advance(&self, reason: EpochReason) -> EpochInfo {
        let mut info = lock(&self.0);
        *info = EpochInfo {
            generation: info.generation + 1,
            reason,
            since: SystemTime::now(),
        };
        *info
    }

    pub fn current(&self) -> EpochInfo {
        *lock(&self.0)
    }
}

/// The single place where subsystems report authoritative engine state changes. Callers name
/// the change; the lifecycle advances the epoch with the right reason.
pub(crate) struct Lifecycle {
    last_drm_session: Mutex<&'static str>,
    ready: Mutex<FairPlayReady>,
}

#[derive(Default)]
struct FairPlayReady {
    /// When FairPlay was last reported ready.
    since: Option<SystemTime>,
    /// The pending `drm.refresh_due` timer.
    timer: Option<AbortHandle>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            last_drm_session: Mutex::new(""),
            ready: Mutex::default(),
        }
    }

    /// Advances the epoch when the DRM session state changes. Repeated calls with the same
    /// value do nothing.
    pub fn on_drm_state_changed(&self, epoch: &Epoch, session: &'static str) {
        let mut last = lock(&self.last_drm_session);
        if *last != session {
            *last = session;
            epoch.advance(EpochReason::SessionChanged);
        }
    }

    /// Records that FairPlay is ready and schedules `on_refresh_due` shortly before the session
    /// is expected to expire, replacing any earlier schedule. The renderer pre-warms a new
    /// session on that event instead of stalling after the expiry.
    pub fn on_fair_play_ready(&self, on_refresh_due: impl FnOnce() + Send + 'static) {
        self.on_fair_play_ready_after(DRM_SESSION_TTL - DRM_REFRESH_LEAD, on_refresh_due);
    }

    fn on_fair_play_ready_after(
        &self,
        delay: Duration,
        on_refresh_due: impl FnOnce() + Send + 'static,
    ) {
        let mut ready = lock(&self.ready);
        ready.since = Some(SystemTime::now());
        if let Some(timer) = ready.timer.take() {
            timer.abort();
        }
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            on_refresh_due();
        });
        ready.timer = Some(task.abort_handle());
    }

    /// When FairPlay was last reported ready, if ever.
    pub fn drm_ready_since(&self) -> Option<SystemTime> {
        lock(&self.ready).since
    }
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        if let Some(timer) = lock(&self.ready).timer.take() {
            timer.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn epoch_starts_at_generation_one() {
        let e = Epoch::new();
        assert_eq!(e.current().generation, 1);
        assert_eq!(e.current().reason, EpochReason::EngineStart);
    }

    #[test]
    fn advance_increments_and_current_follows() {
        let e = Epoch::new();
        let info = e.advance(EpochReason::SessionChanged);
        assert_eq!(
            (info.generation, info.reason),
            (2, EpochReason::SessionChanged)
        );
        assert_eq!(e.advance(EpochReason::SessionChanged).generation, 3);
        assert_eq!(e.current().generation, 3);
    }

    #[test]
    fn since_is_recent() {
        let before = SystemTime::now();
        let since = Epoch::new().current().since;
        assert!(since >= before && since <= SystemTime::now());
    }

    #[test]
    fn drm_state_change_is_idempotent() {
        let (e, lc) = (Epoch::new(), Lifecycle::new());
        lc.on_drm_state_changed(&e, "valid");
        let g1 = e.current().generation;
        lc.on_drm_state_changed(&e, "valid");
        assert_eq!(e.current().generation, g1, "same session must not advance");
        lc.on_drm_state_changed(&e, "empty");
        assert!(e.current().generation > g1, "a new session must advance");
    }

    #[tokio::test]
    async fn fair_play_ready_sets_timestamp_and_fires() {
        let lc = Lifecycle::new();
        assert!(lc.drm_ready_since().is_none());
        let before = SystemTime::now();
        let (tx, rx) = tokio::sync::oneshot::channel();
        lc.on_fair_play_ready_after(Duration::from_millis(10), move || {
            let _ = tx.send(());
        });
        let since = lc.drm_ready_since().unwrap();
        assert!(since >= before && since <= SystemTime::now());
        tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("refresh callback fired")
            .unwrap();
    }

    #[tokio::test]
    async fn a_new_ready_replaces_the_pending_timer() {
        let lc = Lifecycle::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        lc.on_fair_play_ready_after(Duration::from_millis(20), move || {
            f.fetch_add(1, Ordering::SeqCst);
        });
        let first = lc.drm_ready_since().unwrap();
        let f = fired.clone();
        lc.on_fair_play_ready_after(Duration::from_millis(20), move || {
            f.fetch_add(10, Ordering::SeqCst);
        });
        assert!(lc.drm_ready_since().unwrap() >= first);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fired.load(Ordering::SeqCst),
            10,
            "only the second timer may fire"
        );
    }
}
