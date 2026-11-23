//! A fixed-capacity circular buffer for `i64` latency samples.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A thread-safe window over the last `capacity` samples.
pub struct Buffer {
    samples: Mutex<VecDeque<i64>>,
    capacity: usize,
}

/// Mean and 95th percentile of a [`Buffer`] window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub avg: f64,
    pub p95: f64,
}

impl Buffer {
    /// A buffer that keeps the last `capacity` samples.
    ///
    /// # Panics
    /// If `capacity` is 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be positive");
        Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<i64>> {
        self.samples.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds one sample, dropping the oldest when the buffer is full.
    pub fn record(&self, v: i64) {
        let mut s = self.lock();
        if s.len() == self.capacity {
            s.pop_front();
        }
        s.push_back(v);
    }

    /// A copy of the current window, oldest to newest.
    pub fn snapshot(&self) -> Vec<i64> {
        self.lock().iter().copied().collect()
    }

    /// The mean and p95 over the current window. Both are 0 when the buffer is empty.
    pub fn stats(&self) -> Stats {
        let mut s = self.snapshot();
        if s.is_empty() {
            return Stats::default();
        }
        let avg = s.iter().sum::<i64>() as f64 / s.len() as f64;
        s.sort_unstable();
        // Nearest-rank p95: ceil(n * 0.95) - 1, never past the end.
        let idx = ((s.len() as f64 * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(s.len() - 1);
        Stats {
            avg,
            p95: s[idx] as f64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn empty() {
        let b = Buffer::new(10);
        assert!(b.snapshot().is_empty());
        assert_eq!(b.stats(), Stats::default());
    }

    #[test]
    fn record_and_snapshot() {
        let b = Buffer::new(5);
        (1..=3).for_each(|i| b.record(i));
        assert_eq!(b.snapshot(), [1, 2, 3]);
    }

    #[test]
    fn wraps_oldest_to_newest() {
        let b = Buffer::new(3);
        (1..=6).for_each(|i| b.record(i));
        assert_eq!(b.snapshot(), [4, 5, 6]);
    }

    #[test]
    fn stats() {
        let b = Buffer::new(10);
        (1..=10).for_each(|i| b.record(i));
        // p95 of 1..10: idx = ceil(10 * 0.95) - 1 = 9, value 10.
        assert_eq!(
            b.stats(),
            Stats {
                avg: 5.5,
                p95: 10.0
            }
        );
    }

    #[test]
    fn concurrent_use() {
        let b = Arc::new(Buffer::new(100));
        let handles: Vec<_> = (0..50)
            .map(|i| {
                let b = Arc::clone(&b);
                std::thread::spawn(move || {
                    b.record(i);
                    b.snapshot();
                    b.stats();
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(b.snapshot().len(), 50);
    }
}
