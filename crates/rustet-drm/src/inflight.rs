//! In-flight accounting for calls into a native library that a stop must not tear down under.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::backend::BackendError;

/// One run of the backend, between a start and the next. Key contexts handed out by one run
/// are meaningless after the next start, so calls that use them carry the run they belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation(u64);

#[derive(Default)]
struct State {
    open: bool,
    active: usize,
    generation: u64,
}

/// Admits calls into a native library while the backend runs, and lets a stop wait for the
/// calls already inside it. A backend keeps one in an `Arc` for its lifetime.
#[derive(Default)]
pub struct CallGate {
    state: Mutex<State>,
    drained: Condvar,
}

impl CallGate {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admits calls again after a successful start, as a new generation.
    pub fn open(&self) -> Generation {
        let mut s = self.lock();
        s.open = true;
        s.generation += 1;
        Generation(s.generation)
    }

    /// Whether calls are admitted: the backend runs.
    pub fn is_open(&self) -> bool {
        self.lock().open
    }

    /// The current generation.
    pub fn generation(&self) -> Generation {
        Generation(self.lock().generation)
    }

    /// Registers one call into the library; it ends when the guard drops. A call that uses
    /// state from a run (a key context) passes that run's generation, and is refused once a
    /// later start replaced it. Refused while the gate is closed.
    pub fn enter(
        self: &Arc<Self>,
        generation: Option<Generation>,
    ) -> Result<CallGuard, BackendError> {
        let mut s = self.lock();
        if !s.open || generation.is_some_and(|g| g.0 != s.generation) {
            return Err(BackendError::NotRunning);
        }
        s.active += 1;
        Ok(CallGuard {
            gate: Arc::clone(self),
        })
    }

    /// Refuses new calls at once, then waits up to `timeout` for the calls already inside the
    /// library. Returns whether they all finished in time. Blocks: call it off the runtime.
    pub fn quiesce(&self, timeout: Duration) -> bool {
        let mut s = self.lock();
        s.open = false;
        let (s, waited) = self
            .drained
            .wait_timeout_while(s, timeout, |s| s.active > 0)
            .unwrap_or_else(PoisonError::into_inner);
        drop(s);
        !waited.timed_out()
    }
}

/// One call inside the library; see [`CallGate::enter`].
pub struct CallGuard {
    gate: Arc<CallGate>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut s = self.gate.lock();
        s.active -= 1;
        if s.active == 0 {
            self.gate.drained.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    fn running(generations: u64) -> Arc<CallGate> {
        let gate = Arc::new(CallGate::default());
        for _ in 0..generations {
            gate.open();
        }
        gate
    }

    // A stop must not tear the library down under a call still inside it, and must refuse new
    // calls from the moment it starts.
    #[test]
    fn quiesce_waits_for_in_flight_calls() {
        let gate = running(1);
        let call = gate.enter(None).unwrap();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn({
            let gate = gate.clone();
            move || tx.send(gate.quiesce(Duration::from_secs(5))).unwrap()
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while let Ok(probe) = gate.enter(None) {
            drop(probe); // accepted before the stop got going
            assert!(
                Instant::now() < deadline,
                "a new call was accepted after the stop began"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "quiesce returned with a call inside the library"
        );

        drop(call);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(true));
    }

    #[test]
    fn quiesce_gives_up_on_a_stuck_call() {
        let gate = running(1);
        let _stuck = gate.enter(None).unwrap();
        assert!(!gate.quiesce(Duration::from_millis(50)));
    }

    // A key context opened before a restart must not be used after it.
    #[test]
    fn calls_from_a_previous_run_are_refused() {
        let gate = running(1);
        let first = gate.generation();
        gate.open();
        assert!(
            gate.enter(Some(first)).is_err(),
            "generation 1 was accepted by generation 2"
        );
        drop(gate.enter(Some(gate.generation())).unwrap());
        drop(
            gate.enter(None)
                .expect("calls without a generation are allowed while open"),
        );
        assert!(gate.quiesce(Duration::ZERO));
        assert!(gate.enter(None).is_err());
    }
}
