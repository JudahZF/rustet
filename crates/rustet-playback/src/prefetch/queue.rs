//! The bounded max-score work queue of the warming workers.

use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

/// Pops the highest score first (FIFO among equal scores). Bounded: when full, the lowest
/// score, new or queued, is evicted.
pub(super) struct WorkQueue<T> {
    state: Mutex<State<T>>,
    available: Notify,
    capacity: usize,
}

struct State<T> {
    items: Vec<(f64, T)>,
    closed: bool,
}

impl<T> WorkQueue<T> {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                items: Vec::new(),
                closed: false,
            }),
            available: Notify::new(),
            capacity,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enqueues `item`. Returns the item that did not make it, so the caller can account for
    /// it: `item` itself when the queue is closed or `item` scores no higher than everything
    /// queued in a full queue, otherwise the evicted lowest-scored queued item.
    pub(super) fn push(&self, score: f64, item: T) -> Option<T> {
        let mut st = self.lock();
        if st.closed {
            // No worker will ever pop it: hand it back so its job can finish.
            return Some(item);
        }
        let mut evicted = None;
        if st.items.len() >= self.capacity {
            let Some(min) = position_by(&st.items, |a, b| a < b) else {
                return Some(item);
            };
            if score <= st.items[min].0 {
                return Some(item);
            }
            evicted = Some(st.items.remove(min).1);
        }
        st.items.push((score, item));
        drop(st);
        self.available.notify_one();
        evicted
    }

    /// Waits for an item and removes the highest-scored one. Returns `None` once the queue is
    /// closed and drained.
    pub(super) async fn pop(&self) -> Option<T> {
        loop {
            // Registered before the check, so a push or close between check and await is seen.
            let notified = self.available.notified();
            {
                let mut st = self.lock();
                if let Some(max) = position_by(&st.items, |a, b| a > b) {
                    let item = st.items.remove(max).1;
                    let more = !st.items.is_empty();
                    drop(st);
                    // One wake-up per push: pass it on while items remain.
                    if more {
                        self.available.notify_one();
                    }
                    return Some(item);
                }
                if st.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Wakes every waiting pop and rejects later pushes. Queued items can still be popped.
    pub(super) fn close(&self) {
        self.lock().closed = true;
        self.available.notify_waiters();
    }

    pub(super) fn len(&self) -> usize {
        self.lock().items.len()
    }
}

/// The index of the first item whose score beats every other by `better`.
fn position_by<T>(items: &[(f64, T)], better: impl Fn(f64, f64) -> bool) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, (score, _)) in items.iter().enumerate() {
        if best.is_none_or(|b| better(*score, items[b].0)) {
            best = Some(i);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn close_wakes_blocked_pop() {
        let q = Arc::new(WorkQueue::<u32>::new(4));
        let waiter = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.pop().await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        q.close();
        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("close did not wake the blocked pop")
            .unwrap();
        assert_eq!(got, None, "pop on a closed empty queue reported an item");
    }

    #[test]
    fn push_after_close_returns_item() {
        let q = WorkQueue::new(4);
        q.close();
        assert_eq!(
            q.push(1.0, "x"),
            Some("x"),
            "a closed queue must hand the item back"
        );
        assert_eq!(q.len(), 0, "a closed queue must not accept work");
    }

    #[tokio::test]
    async fn evicts_lowest_score() {
        const CAP: usize = 512;
        let q = WorkQueue::new(CAP);
        for i in 0..CAP {
            assert_eq!(
                q.push((i + 1) as f64, i + 1),
                None,
                "eviction while filling"
            );
        }
        assert_eq!(
            q.push(0.5, 0),
            Some(0),
            "a lower-scored newcomer must be the one rejected"
        );
        assert_eq!(
            q.push(1e9, 1_000_000_000),
            Some(1),
            "the lowest queued item must be evicted"
        );
        assert_eq!(q.len(), CAP);
        assert_eq!(
            q.pop().await,
            Some(1_000_000_000),
            "pop must return the highest score first"
        );
    }

    #[tokio::test]
    async fn equal_scores_pop_in_order() {
        let q = WorkQueue::new(8);
        for s in ["a", "b", "c"] {
            q.push(1.0, s);
        }
        q.push(2.0, "top");
        q.close();
        let mut got = Vec::new();
        while let Some(s) = q.pop().await {
            got.push(s);
        }
        assert_eq!(got, ["top", "a", "b", "c"]);
    }
}
