//! Lease recovery (`native/main.cpp`).
//!
//! ```text
//!   Running ──lease end / playback error──▶ Scheduled ──backoff elapsed──▶ Refreshing
//!      ▲                                                                     │
//!      ├──────────────── prefetch context rebuilt ◀──────────────────────────┤
//!      │                                                                     ▼
//!      └──────── (retry through the queue, same backoff) ◀──────────── Failed
//! ```
//!
//! The lease manager's callbacks only queue an event and return. One worker thread owns
//! retry timing and reacquisition: it drains the whole queue on each wake (a burst of events
//! gives one refresh), waits out an exponential backoff, then refreshes the lease and the key
//! contexts. When the refresh leaves no prefetch context (a transient Apple or network
//! failure), no further Apple event would arrive, so the worker schedules its own retry
//! through the same queue and backoff. While the state is not `Running`, key and asset
//! requests are refused so they fail fast instead of queueing behind the refresh.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, Once};
use std::time::Duration;

use tracing::{info, warn};

use super::session;

/// The recovery state machine's states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum State {
    /// Normal operation; all requests served.
    Running = 0,
    /// An event is queued; the worker is waking or waiting out the backoff.
    Scheduled = 1,
    /// The refresh is in flight; requests are refused.
    Refreshing = 2,
    /// The last refresh failed; a retry is queued.
    Failed = 3,
}

/// What woke the worker.
#[derive(Clone, Copy, Debug)]
enum Event {
    /// The lease manager ended the lease (3084: server revoke, simultaneous playback or
    /// natural expiry).
    LeaseEnd(i32),
    /// The lease manager reported a playback error.
    PlaybackError,
    /// Self-scheduled after a failed refresh.
    Retry,
}

/// Exponential backoff 1 → 2 → 5 → 10 → 30 s; the last step repeats, so retries never stop.
const BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];

fn backoff(consecutive_fails: usize) -> u64 {
    BACKOFF_SECS[consecutive_fails.min(BACKOFF_SECS.len() - 1)]
}

static STATE: AtomicU8 = AtomicU8::new(State::Running as u8);
static QUEUE: Mutex<Vec<Event>> = Mutex::new(Vec::new());
static WAKE: Condvar = Condvar::new();

fn set_state(s: State) {
    STATE.store(s as u8, Ordering::SeqCst);
}

/// Whether a recovery cycle is under way (`is_recovery_active`).
pub(crate) fn active() -> bool {
    STATE.load(Ordering::SeqCst) != State::Running as u8
}

fn schedule(event: Event) {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(event);
    WAKE.notify_one();
}

/// The lease manager's lease-end callback (`endLeaseCb`). Must return at once: no library
/// call, no I/O, no lock the lease manager might hold.
pub(crate) unsafe extern "C" fn on_lease_end(code: *const c_void) {
    // SAFETY: the lease manager passes `int const&`.
    let code = unsafe { *code.cast::<i32>() };
    info!("[.] LEASE_END code={code} — scheduling async recovery");
    schedule(Event::LeaseEnd(code));
}

/// The lease manager's playback-error callback (`pbErrCb`). Same contract as `on_lease_end`.
pub(crate) unsafe extern "C" fn on_playback_error(_error: *const c_void) {
    info!("[.] PLAYBACK_ERROR — scheduling async recovery");
    schedule(Event::PlaybackError);
}

/// Starts the worker once the lease manager and the FootHill session exist. The C started one
/// more worker on every start of the session; one is enough, as they shared the queue.
pub(crate) fn start_worker() -> std::io::Result<()> {
    static STARTED: Once = Once::new();
    let mut result = Ok(());
    STARTED.call_once(|| {
        result = std::thread::Builder::new()
            .name("fairplay-recovery".into())
            .stack_size(session::GUEST_STACK_SIZE)
            .spawn(worker)
            .map(drop);
    });
    result
}

fn worker() {
    let mut consecutive_fails = 0usize;
    loop {
        let burst: Vec<Event> = {
            let queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            let mut queue = WAKE
                .wait_while(queue, |q| q.is_empty())
                .unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *queue)
        };
        set_state(State::Scheduled);

        info!(
            "[recovery] STATE=Scheduled — {} event(s) coalesced:",
            burst.len()
        );
        for event in &burst {
            match event {
                Event::PlaybackError => info!("[recovery]   PLAYBACK_ERROR"),
                Event::Retry => {
                    info!("[recovery]   RETRY_INTERNAL (self-scheduled after failed refresh)")
                }
                Event::LeaseEnd(3084) => {
                    info!(
                        "[recovery]   LEASE_END code=3084 (server revoke — simultaneous playback or natural expiry)"
                    );
                }
                Event::LeaseEnd(code) => info!(
                    "[recovery]   LEASE_END code={code} (unknown — logging for future mapping)"
                ),
            }
        }

        let delay = backoff(consecutive_fails);
        if consecutive_fails == 0 {
            info!("[recovery] RECOVERY_SCHEDULED attempt=1 backoff={delay}s");
        } else {
            info!(
                "[recovery] RECOVERY_SCHEDULED attempt={} backoff={delay}s (Apple may still be rejecting — retrying indefinitely)",
                consecutive_fails + 1
            );
        }
        std::thread::sleep(Duration::from_secs(delay));

        set_state(State::Refreshing);
        info!("[recovery] STATE=Refreshing — calling refresh_decrypt_ctx()");
        // The worker is the only caller of the refresh. A rebuilt prefetch context means Apple
        // accepted the new lease and key derivation works again.
        if session::refresh_decrypt_ctx() {
            consecutive_fails = 0;
            set_state(State::Running);
            info!(
                "[recovery] STATE=Running — REFRESH_SUCCESS, decrypt context ready, HTTP servers resuming normal operation"
            );
        } else {
            consecutive_fails += 1;
            set_state(State::Failed);
            warn!(
                "[recovery] STATE=Failed — REFRESH_FAILED ctx_ready=false consecutive_fails={consecutive_fails} retrying_in={}s",
                backoff(consecutive_fails)
            );
            schedule(Event::Retry);
        }
    }
}
