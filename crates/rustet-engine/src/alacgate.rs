//! Foreground-first scheduling for audio cache downloads.
//!
//! The CDN link is shared by every download, so when the user starts a track while an earlier
//! track's (or the next track's gapless pre-warm) download is still running, all of them fetch at
//! once and the track the player waits on gets a fraction of the bandwidth. [`AlacGate`] parks
//! every background download's writes from the moment the player loads a track until that track
//! has its first [`START_BYTES`] on disk (or it ends, or [`HOLD_MAX`] passes). Parking is
//! back-pressure on the writer: the stream stops pulling from the CDN until released.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Bytes the foreground download writes before the background ones may continue.
pub(crate) const START_BYTES: u64 = 3 << 20;
/// The longest a foreground hold lasts.
const HOLD_MAX: Duration = Duration::from_secs(15);

#[derive(Default)]
struct State {
    /// The session whose download the player waits on.
    fg: String,
    /// Bytes the foreground session has written since it became foreground.
    seen: u64,
    /// Invalidates the cap timer of a superseded `foreground` call.
    generation: u64,
}

/// Parks background cache writers while the player's track starts.
pub(crate) struct AlacGate {
    state: Mutex<State>,
    /// Background writers park while true.
    held: watch::Sender<bool>,
}

impl Default for AlacGate {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            held: watch::Sender::new(false),
        }
    }
}

/// Raised when a write is cancelled while parked.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub(crate) struct Cancelled;

impl AlacGate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_held(&self, held: bool) {
        self.held
            .send_if_modified(|h| std::mem::replace(h, held) != held);
    }

    /// Marks `id` as the download the player waits on. With `hold`, every other session's writer
    /// parks until `id` has written [`START_BYTES`], ends, or [`HOLD_MAX`] elapses. Needs a tokio
    /// runtime (for the cap timer).
    pub fn foreground(self: &std::sync::Arc<Self>, id: &str, hold: bool) {
        let mut st = self.lock();
        id.clone_into(&mut st.fg);
        st.seen = 0;
        st.generation += 1;
        self.set_held(hold);
        if hold {
            let generation = st.generation;
            let gate = std::sync::Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(HOLD_MAX).await;
                if let Some(gate) = gate.upgrade() {
                    let st = gate.lock();
                    if st.generation == generation {
                        gate.set_held(false);
                    }
                }
            });
        }
    }

    /// `id`'s download ended, successfully or not: a foreground that finished or failed must not
    /// keep the others parked.
    pub fn done(&self, id: &str) {
        let st = self.lock();
        if st.fg == id {
            self.set_held(false);
        }
    }

    /// Waits while the gate holds writers other than the foreground one.
    pub async fn wait(&self, cancel: &CancellationToken, id: &str) -> Result<(), Cancelled> {
        let mut rx = self.held.subscribe();
        loop {
            // Read into a local: the watch's lock must not be held while taking the state lock
            // (`foreground` takes them the other way round).
            let held = *rx.borrow_and_update();
            if !held || self.lock().fg == id {
                return Ok(());
            }
            tokio::select! {
                () = cancel.cancelled() => return Err(Cancelled),
                // The sender lives in `self`.
                _ = rx.changed() => {}
            }
        }
    }

    /// Counts `n` bytes written by `id`; the foreground reaching [`START_BYTES`] releases the hold.
    pub fn wrote(&self, id: &str, n: usize) {
        let mut st = self.lock();
        if st.fg == id && *self.held.borrow() {
            st.seen += n as u64;
            if st.seen >= START_BYTES {
                self.set_held(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn write_async(
        g: &Arc<AlacGate>,
        cancel: &CancellationToken,
        id: &str,
        n: usize,
    ) -> tokio::task::JoinHandle<bool> {
        let (g, cancel, id) = (g.clone(), cancel.clone(), id.to_owned());
        tokio::spawn(async move {
            let ok = g.wait(&cancel, &id).await.is_ok();
            if ok {
                g.wrote(&id, n);
            }
            ok
        })
    }

    async fn blocked(h: &mut tokio::task::JoinHandle<bool>, what: &str) {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), h)
                .await
                .is_err(),
            "{what}: write returned, want it parked"
        );
    }

    async fn done(h: tokio::task::JoinHandle<bool>, what: &str) -> bool {
        tokio::time::timeout(Duration::from_secs(1), h)
            .await
            .unwrap_or_else(|_| panic!("{what}: still parked"))
            .unwrap()
    }

    #[tokio::test]
    async fn background_parks_until_foreground_has_start_bytes() {
        let g = Arc::new(AlacGate::default());
        let c = CancellationToken::new();
        g.foreground("fg", true);
        let mut parked = write_async(&g, &c, "bg", 10);
        blocked(&mut parked, "background").await;
        // The foreground itself is never parked, and partial progress does not release.
        assert!(
            done(
                write_async(&g, &c, "fg", START_BYTES as usize - 1),
                "foreground"
            )
            .await
        );
        blocked(&mut parked, "background after partial foreground").await;
        assert!(done(write_async(&g, &c, "fg", 1), "foreground").await);
        assert!(done(parked, "background after release").await);
    }

    #[tokio::test]
    async fn done_releases_only_for_the_foreground() {
        let g = Arc::new(AlacGate::default());
        let c = CancellationToken::new();
        g.foreground("fg", true);
        let mut parked = write_async(&g, &c, "bg", 1);
        blocked(&mut parked, "background").await;
        g.done("someone-else");
        blocked(&mut parked, "background after unrelated done").await;
        g.done("fg");
        assert!(done(parked, "background after foreground done").await);
    }

    #[tokio::test]
    async fn no_hold_never_parks() {
        let g = Arc::new(AlacGate::default());
        g.foreground("fg", false); // already cached or well advanced
        assert!(
            done(
                write_async(&g, &CancellationToken::new(), "bg", 1),
                "background"
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_new_foreground_supersedes() {
        let g = Arc::new(AlacGate::default());
        g.foreground("a", true);
        g.foreground("b", true); // the user moved on before a got its bytes
        let mut h = write_async(&g, &CancellationToken::new(), "a", 1);
        blocked(&mut h, "former foreground").await;
    }

    #[tokio::test]
    async fn cancel_unparks_with_an_error() {
        let g = Arc::new(AlacGate::default());
        let c = CancellationToken::new();
        g.foreground("fg", true);
        let mut parked = write_async(&g, &c, "bg", 1);
        blocked(&mut parked, "background").await;
        c.cancel();
        assert!(
            !done(parked, "cancelled write").await,
            "a cancelled write must fail"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_hold_is_capped() {
        let g = Arc::new(AlacGate::default());
        g.foreground("fg", true);
        let h = write_async(&g, &CancellationToken::new(), "bg", 1);
        tokio::time::sleep(HOLD_MAX + Duration::from_millis(10)).await;
        assert!(h.await.unwrap());
    }
}
