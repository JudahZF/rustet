//! A three-state circuit breaker for session-open failures.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

/// The breaker state, as `/api/v1/metrics` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BreakerState {
    Closed,
    /// Fast-fail until the cooldown ends.
    Open,
    /// The cooldown ended but no success reset the counter: the next request is a trial, and
    /// one more failure re-opens the breaker.
    HalfOpen,
}

/// Closed → open after `threshold` consecutive failures → half-open after `reset_after` →
/// closed on the next success.
pub(crate) struct CircuitBreaker {
    threshold: u32,
    reset_after: Duration,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    failures: u32,
    open_until: Option<Instant>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, reset_after: Duration) -> Self {
        Self {
            threshold,
            reset_after,
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// False while the breaker is open (fast-fail).
    pub fn allow(&self) -> bool {
        !self.lock().open_until.is_some_and(|t| Instant::now() < t)
    }

    pub fn record_success(&self) {
        *self.lock() = Inner::default();
    }

    pub fn record_failure(&self) {
        let mut inner = self.lock();
        inner.failures += 1;
        if inner.failures >= self.threshold {
            inner.open_until = Some(Instant::now() + self.reset_after);
        }
    }
}

impl CircuitBreaker {
    pub fn state(&self) -> BreakerState {
        let inner = self.lock();
        if inner.open_until.is_some_and(|t| Instant::now() < t) {
            BreakerState::Open
        } else if inner.failures >= self.threshold {
            BreakerState::HalfOpen
        } else {
            BreakerState::Closed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn starts_closed() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(1));
        assert!(cb.allow());
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn trips_at_threshold() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(3600));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), BreakerState::Closed);
        cb.record_failure();
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(!cb.allow());
    }

    #[test]
    fn cooldown_gives_half_open_then_a_failed_trial_reopens_and_success_closes() {
        let cb = CircuitBreaker::new(2, Duration::from_millis(20));
        cb.record_failure();
        assert!(
            cb.state() == BreakerState::Closed && cb.allow(),
            "one failure must not trip"
        );
        cb.record_failure();
        assert!(cb.state() == BreakerState::Open && !cb.allow());
        std::thread::sleep(Duration::from_millis(30));
        assert!(cb.state() == BreakerState::HalfOpen && cb.allow());
        cb.record_failure();
        assert_eq!(
            cb.state(),
            BreakerState::Open,
            "a failed trial must re-open"
        );
        std::thread::sleep(Duration::from_millis(30));
        cb.record_success();
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn success_clears_failures() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(3600));
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn state_names() {
        let names: Vec<_> = [
            BreakerState::Closed,
            BreakerState::Open,
            BreakerState::HalfOpen,
        ]
        .iter()
        .map(|s| serde_json::to_string(s).unwrap())
        .collect();
        assert_eq!(names, [r#""closed""#, r#""open""#, r#""half-open""#]);
    }

    #[test]
    fn concurrent_use() {
        let cb = Arc::new(CircuitBreaker::new(100, Duration::from_secs(1)));
        let threads: Vec<_> = (0..50)
            .map(|_| {
                let cb = cb.clone();
                std::thread::spawn(move || {
                    cb.record_failure();
                    cb.record_success();
                    cb.allow();
                    cb.state();
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
    }
}
