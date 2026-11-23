//! A task-safe priority queue keyed by ID.
//!
//! Items pop in (priority descending, insertion sequence ascending) order, so equal priorities
//! are FIFO. [`Queue::pop`] waits until an item is available or the queue is closed. Pop removes
//! the item from the order and the ID index under one lock, so once an item has been popped,
//! [`Queue::update`] and [`Queue::remove`] report it as no longer queued. Callers use that to
//! tell queued work from running work without a race.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

/// One queued item as returned by [`Queue::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<T> {
    pub id: String,
    pub item: T,
    pub prio: i32,
}

/// Position in pop order: higher priority first, then lower sequence first.
type Key = (Reverse<i32>, u64);

struct Inner<T> {
    order: BTreeMap<Key, String>,
    by_id: HashMap<String, (Key, T)>,
    seq: u64,
    closed: bool,
}

/// A max-priority queue with FIFO tie-breaking and an async, blocking [`pop`](Queue::pop).
pub struct Queue<T> {
    inner: Mutex<Inner<T>>,
    available: Notify,
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Queue<T> {
    /// An empty queue.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                order: BTreeMap::new(),
                by_id: HashMap::new(),
                seq: 0,
                closed: false,
            }),
            available: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<T>> {
        // The critical sections cannot leave the maps inconsistent, so a poisoned lock is safe.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Enqueues `item` under `id`. The FIFO sequence number is assigned now, at push time.
    /// Returns false if `id` is already queued or the queue is closed.
    pub fn push(&self, id: impl Into<String>, item: T, prio: i32) -> bool {
        let id = id.into();
        let mut q = self.lock();
        if q.closed || q.by_id.contains_key(&id) {
            return false;
        }
        q.seq += 1;
        let key = (Reverse(prio), q.seq);
        q.order.insert(key, id.clone());
        q.by_id.insert(id, (key, item));
        drop(q);
        self.available.notify_one();
        true
    }

    /// Waits until an item is available and removes the highest-priority one.
    /// Returns `None` once the queue is closed and drained.
    pub async fn pop(&self) -> Option<T> {
        loop {
            // Register for a wake-up before checking, so a push between the check and the
            // await is not missed.
            let notified = self.available.notified();
            {
                let mut q = self.lock();
                if let Some((_, id)) = q.order.pop_first() {
                    let item = q.by_id.remove(&id).map(|(_, item)| item);
                    let more = !q.order.is_empty();
                    drop(q);
                    // `notify_one` wakes one waiter per push; pass the baton on when items remain,
                    // so a waiter does not sleep while the queue is non-empty.
                    if more {
                        self.available.notify_one();
                    }
                    return item;
                }
                if q.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Changes the priority of a queued item, keeping its original sequence number.
    /// Returns false if `id` is not queued (never pushed, removed, or already popped).
    pub fn update(&self, id: &str, prio: i32) -> bool {
        let mut q = self.lock();
        let Inner { order, by_id, .. } = &mut *q;
        let Some((key, _)) = by_id.get_mut(id) else {
            return false;
        };
        let Some(id) = order.remove(key) else {
            return false;
        };
        key.0 = Reverse(prio);
        order.insert(*key, id);
        true
    }

    /// Removes a queued item and returns it. Returns `None` if `id` is not queued (for example,
    /// a worker already popped it).
    pub fn remove(&self, id: &str) -> Option<T> {
        let mut q = self.lock();
        let (key, item) = q.by_id.remove(id)?;
        q.order.remove(&key);
        Some(item)
    }

    /// Wakes every waiting [`pop`](Queue::pop). Later pushes are rejected, and pop returns
    /// `None` once the remaining items are drained.
    pub fn close(&self) {
        self.lock().closed = true;
        self.available.notify_waiters();
    }

    /// The number of queued items.
    pub fn len(&self) -> usize {
        self.lock().order.len()
    }

    /// True when no item is queued.
    pub fn is_empty(&self) -> bool {
        self.lock().order.is_empty()
    }
}

impl<T: Clone> Queue<T> {
    /// The queued items in pop order.
    pub fn snapshot(&self) -> Vec<Entry<T>> {
        let q = self.lock();
        q.order
            .iter()
            .map(|(&(Reverse(prio), _), id)| Entry {
                id: id.clone(),
                item: q.by_id[id].1.clone(),
                prio,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn pop_all(q: &Queue<String>) -> Vec<String> {
        let mut out = Vec::new();
        while !q.is_empty() {
            out.push(q.pop().await.expect("pop returned None with items queued"));
        }
        out
    }

    #[tokio::test]
    async fn priority_then_fifo() {
        let q = Queue::new();
        for (id, item, prio) in [
            ("a", "0/1", 0),
            ("b", "5/2", 5),
            ("c", "10/3", 10),
            ("d", "10/4", 10),
            ("e", "0/5", 0),
        ] {
            q.push(id, item.to_owned(), prio);
        }
        assert_eq!(pop_all(&q).await, ["10/3", "10/4", "5/2", "0/1", "0/5"]);
    }

    #[tokio::test]
    async fn fifo_many_equal() {
        let q = Queue::new();
        let want: Vec<String> = (0..100).map(|i| i.to_string()).collect();
        for s in &want {
            q.push(s.clone(), s.clone(), 1);
        }
        assert_eq!(pop_all(&q).await, want);
    }

    #[test]
    fn duplicate_push_rejected() {
        let q = Queue::new();
        assert!(q.push("a", "x", 0));
        assert!(!q.push("a", "y", 5));
    }

    #[tokio::test]
    async fn update_queued_and_popped() {
        let q = Queue::new();
        for id in ["a", "b", "c"] {
            q.push(id, id.to_owned(), 0);
        }
        assert!(q.update("c", 9));
        assert_eq!(q.pop().await.as_deref(), Some("c"));
        assert!(!q.update("c", 1), "update succeeded on a popped item");
        assert!(!q.update("zzz", 1), "update succeeded on an unknown item");
    }

    /// Raising then lowering keeps the original sequence for tie-breaking.
    #[tokio::test]
    async fn update_keeps_sequence() {
        let q = Queue::new();
        q.push("a", "a".to_owned(), 0);
        q.push("b", "b".to_owned(), 0);
        q.update("a", 5);
        q.update("a", 0);
        assert_eq!(pop_all(&q).await, ["a", "b"]);
    }

    #[tokio::test]
    async fn remove_queued_and_popped() {
        let q = Queue::new();
        for id in ["a", "b", "c"] {
            q.push(id, id.to_owned(), 0);
        }
        assert_eq!(q.remove("b").as_deref(), Some("b"));
        assert_eq!(q.len(), 2);
        let popped = q.pop().await.unwrap();
        assert_eq!(q.remove(&popped), None, "remove succeeded on a popped item");
        assert_eq!(pop_all(&q).await, ["c"]);
    }

    #[tokio::test]
    async fn snapshot_matches_pop_order() {
        let q = Queue::new();
        for (i, p) in [1, 7, 3, 7, 0, 9, 3].into_iter().enumerate() {
            q.push(i.to_string(), i.to_string(), p);
        }
        let snap = q.snapshot();
        assert!(
            snap.windows(2).all(|w| w[0].prio >= w[1].prio),
            "snapshot not sorted: {snap:?}"
        );
        let ids: Vec<String> = snap.into_iter().map(|e| e.id).collect();
        assert_eq!(pop_all(&q).await, ids);
    }

    #[tokio::test]
    async fn pop_waits_until_push() {
        let q = Arc::new(Queue::new());
        let waiter = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.pop().await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished(), "pop returned on an empty queue");
        q.push("a", "a".to_owned(), 0);
        let got = timeout(Duration::from_secs(2), waiter)
            .await
            .expect("pop did not wake on push");
        assert_eq!(got.unwrap().as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn close_wakes_and_drains() {
        let q = Arc::new(Queue::<String>::new());
        let waiter = tokio::spawn({
            let q = Arc::clone(&q);
            async move { q.pop().await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        q.close();
        assert_eq!(
            waiter.await.unwrap(),
            None,
            "pop on a closed empty queue returned an item"
        );
        assert!(!q.push("a", "a".to_owned(), 0), "push accepted after close");

        let q2 = Queue::new();
        q2.push("a", "a".to_owned(), 0);
        q2.close();
        assert_eq!(
            q2.pop().await.as_deref(),
            Some("a"),
            "close dropped queued items"
        );
        assert_eq!(q2.pop().await, None, "pop after drain returned an item");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_producers_consumers() {
        const N: i32 = 500;
        let q = Arc::new(Queue::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let (q, tx) = (Arc::clone(&q), tx.clone());
                tokio::spawn(async move {
                    while let Some(v) = q.pop().await {
                        tx.send(v).unwrap();
                    }
                })
            })
            .collect();
        for i in 0..N {
            let q = Arc::clone(&q);
            tokio::spawn(async move { q.push(i.to_string(), i, i % 3) });
        }
        let mut seen = HashSet::new();
        timeout(Duration::from_secs(5), async {
            while seen.len() < N as usize {
                seen.insert(rx.recv().await.unwrap());
            }
        })
        .await
        .unwrap_or_else(|_| panic!("only {}/{N} items popped", seen.len()));
        q.close();
        for w in workers {
            w.await.unwrap();
        }
    }
}
