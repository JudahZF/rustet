//! Seek-priority gate for music-video segment downloads.
//!
//! The CDN's bandwidth is shared across connections (measured: one connection already saturates it, and N parallel
//! ranges are no faster), so while a seek producer fetches the segment the player is waiting on, the long-running
//! from-0 producer must not compete for it. [`Streams::hold_background`](crate::Streams::hold_background) parks every
//! download that does not run under [`with_seek_priority`] until released.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::Error;

tokio::task_local! {
    static SEEK_PRIORITY: ();
}

/// Runs `fut` with seek priority: the downloads it makes are exempt from background holds.
///
/// The mark is task-local: wrap the future that runs the seek producer (the pipeline run). The download tasks this
/// crate spawns from inside it carry the mark over; tasks the caller spawns do not.
pub async fn with_seek_priority<F: Future>(fut: F) -> F::Output {
    SEEK_PRIORITY.scope((), fut).await
}

/// Whether the current task runs under [`with_seek_priority`].
pub(crate) fn has_seek_priority() -> bool {
    SEEK_PRIORITY.try_with(|()| ()).is_ok()
}

/// Counts the active holds. Downloads without priority wait while it is above zero.
#[derive(Debug)]
pub(crate) struct BackgroundGate {
    holds: Arc<watch::Sender<usize>>,
}

impl BackgroundGate {
    pub(crate) fn new() -> Self {
        Self {
            holds: Arc::new(watch::Sender::new(0)),
        }
    }

    /// Takes a hold that ends when released, dropped, or after `max_hold`, whichever is first. The cap guarantees a
    /// lost release can never park the from-0 producer for good. Needs a tokio runtime (for the cap timer).
    pub(crate) fn hold(&self, max_hold: Duration) -> BackgroundHold {
        self.holds.send_modify(|n| *n += 1);
        let state = Arc::new(HoldState {
            holds: Arc::clone(&self.holds),
            released: AtomicBool::new(false),
        });
        let timer = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(max_hold).await;
            timer.release();
        });
        BackgroundHold(state)
    }

    /// Waits while a hold is active, unless `priority` is set.
    pub(crate) async fn wait(
        &self,
        priority: bool,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        if priority || *self.holds.borrow() == 0 {
            return Ok(());
        }
        let mut rx = self.holds.subscribe();
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            // An error means the gate is gone with the engine: nothing holds any more.
            _ = rx.wait_for(|n| *n == 0) => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        *self.holds.borrow()
    }
}

#[derive(Debug)]
struct HoldState {
    holds: Arc<watch::Sender<usize>>,
    released: AtomicBool,
}

impl HoldState {
    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.holds.send_modify(|n| *n -= 1);
        }
    }
}

/// One hold on background downloads. Holds nest; releasing twice is harmless; dropping releases.
#[derive(Debug)]
pub struct BackgroundHold(Arc<HoldState>);

impl BackgroundHold {
    pub fn release(&self) {
        self.0.release();
    }
}

impl Drop for BackgroundHold {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn gated(gate: Arc<BackgroundGate>, priority: bool) -> tokio::task::JoinHandle<()> {
        let cancel = CancellationToken::new();
        tokio::spawn(async move { gate.wait(priority, &cancel).await.unwrap() })
    }

    async fn finishes(h: &mut tokio::task::JoinHandle<()>, within: Duration) -> bool {
        tokio::time::timeout(within, h).await.is_ok()
    }

    #[tokio::test]
    async fn hold_parks_background_but_not_priority() {
        let gate = Arc::new(BackgroundGate::new());
        let hold = gate.hold(Duration::from_secs(60));

        let mut prio = gated(Arc::clone(&gate), true).await;
        let mut bg = gated(Arc::clone(&gate), false).await;
        assert!(
            finishes(&mut prio, Duration::from_secs(1)).await,
            "priority download was parked"
        );
        assert!(
            !finishes(&mut bg, Duration::from_millis(100)).await,
            "background ran during a hold"
        );

        hold.release();
        assert!(
            finishes(&mut bg, Duration::from_secs(1)).await,
            "background not resumed after release"
        );
    }

    #[tokio::test]
    async fn holds_are_bounded_and_nest() {
        let gate = Arc::new(BackgroundGate::new());
        let a = gate.hold(Duration::from_secs(60));
        let b = gate.hold(Duration::from_millis(80)); // expires on its own
        let mut bg = gated(Arc::clone(&gate), false).await;

        tokio::time::sleep(Duration::from_millis(150)).await; // b has expired, a still holds
        assert!(
            !finishes(&mut bg, Duration::from_millis(1)).await,
            "resumed while an outer hold was active"
        );
        a.release();
        a.release(); // idempotent: must not release b's slot or underflow
        drop(b);
        assert!(
            finishes(&mut bg, Duration::from_secs(1)).await,
            "not resumed after all holds released"
        );
        assert_eq!(gate.active(), 0);
    }

    #[tokio::test]
    async fn wait_honours_cancellation() {
        let gate = BackgroundGate::new();
        let _hold = gate.hold(Duration::from_secs(60));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let res = tokio::time::timeout(Duration::from_secs(1), gate.wait(false, &cancel)).await;
        assert!(matches!(res, Ok(Err(Error::Cancelled))));
    }

    #[tokio::test]
    async fn priority_mark_is_scoped() {
        assert!(!has_seek_priority());
        assert!(with_seek_priority(async { has_seek_priority() }).await);
        assert!(!has_seek_priority());
    }
}
