//! The SSE event bus: monotonic event IDs, a replay ring for reconnecting clients and the
//! fan-out to subscribers.
//!
//! Wire format of one frame: `id: N\nevent: TYPE\ndata: {"meta":{…},"payload":…}\n\n`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use tokio::sync::mpsc;

use crate::epoch::{Epoch, EpochReason};

/// Events kept for replay.
pub(crate) const RING_SIZE: usize = 256;
/// Events buffered per subscriber. A subscriber that falls this far behind is disconnected.
const SUBSCRIBER_BUFFER: usize = 64;

/// One event. The payload is serialised once, when it is emitted.
#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub id: i64,
    pub kind: &'static str,
    /// JSON text.
    pub payload: Arc<str>,
    /// The engine epoch when the event was emitted.
    pub generation: u64,
}

/// What a new subscriber gets.
pub(crate) struct Subscription {
    pub id: u64,
    pub events: mpsc::Receiver<Event>,
    /// Ring events after the requested ID, oldest first.
    pub replay: Vec<Event>,
    /// The client asked for a replay the ring cannot give: it must resync.
    pub truncated: bool,
}

pub(crate) struct EventBus {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The last event ID handed out. Every allocation goes through this lock, so IDs are
    /// strictly ordered with respect to ring writes.
    seq: i64,
    ring: VecDeque<Event>,
    next_subscriber: u64,
    clients: HashMap<u64, mpsc::Sender<Event>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                ring: VecDeque::with_capacity(RING_SIZE),
                ..Inner::default()
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Allocates an ID for a frame that is written to one client only (snapshot, ping, …).
    pub fn next_id(&self) -> i64 {
        let mut inner = self.lock();
        inner.seq += 1;
        inner.seq
    }

    /// Assigns an ID, appends to the ring and broadcasts, under one lock.
    ///
    /// A subscriber whose buffer is full is disconnected rather than skipped: silently dropped
    /// events would leave it believing a stale state is current. It sees the end of its
    /// channel, tells the client, and the client reconnects with `Last-Event-ID`.
    pub fn emit(&self, epoch: &Epoch, kind: &'static str, data: &impl Serialize) {
        let payload: Arc<str> = serde_json::to_string(data)
            .unwrap_or_else(|_| "null".to_owned())
            .into();
        // An event emitted just before an epoch advance gets the old generation, which is
        // right: it was produced before the boundary.
        let generation = epoch.current().generation;
        let mut inner = self.lock();
        inner.seq += 1;
        let ev = Event {
            id: inner.seq,
            kind,
            payload,
            generation,
        };
        if inner.ring.len() == RING_SIZE {
            inner.ring.pop_front();
        }
        inner.ring.push_back(ev.clone());
        inner
            .clients
            .retain(|_, tx| tx.try_send(ev.clone()).is_ok());
    }

    /// The IDs of the oldest and newest ring events; `(0, 0)` when the ring is empty.
    pub fn ring_bounds(&self) -> (i64, i64) {
        let inner = self.lock();
        match (inner.ring.front(), inner.ring.back()) {
            (Some(oldest), Some(newest)) => (oldest.id, newest.id),
            _ => (0, 0),
        }
    }

    /// Registers a subscriber and collects the ring events after `after_id`, under one lock:
    /// no event can fall between the replay and the live channel. `None`: a first connect,
    /// no replay.
    pub fn subscribe(&self, after_id: Option<i64>) -> Subscription {
        let mut inner = self.lock();
        let (tx, rx) = mpsc::channel(SUBSCRIBER_BUFFER);
        let id = inner.next_subscriber;
        inner.next_subscriber += 1;
        inner.clients.insert(id, tx);

        let mut replay = Vec::new();
        let mut truncated = false;
        if let Some(after) = after_id {
            if after > inner.seq {
                // The client saw IDs this bus never issued: the engine restarted and its IDs
                // started over. Nothing it remembers applies.
                truncated = true;
            } else if let Some(oldest) = inner.ring.front() {
                if after < oldest.id - 1 {
                    // The events between `after` and the oldest ring event were evicted.
                    truncated = true;
                } else {
                    replay.extend(inner.ring.iter().filter(|ev| ev.id > after).cloned());
                }
            }
        }
        Subscription {
            id,
            events: rx,
            replay,
            truncated,
        }
    }

    /// Removes a subscriber; its channel ends.
    pub fn unsubscribe(&self, id: u64) {
        self.lock().clients.remove(&id);
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.lock().clients.len()
    }
}

/// The transport metadata of every frame. `reason` and `snapshot` are only on snapshots.
#[derive(Serialize)]
struct Meta {
    id: i64,
    generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<EpochReason>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    snapshot: bool,
}

/// Formats one SSE frame. `snapshot` carries the epoch reason of an `engine.snapshot`.
pub(crate) fn frame(
    id: i64,
    kind: &str,
    payload: &str,
    generation: u64,
    snapshot: Option<EpochReason>,
) -> String {
    let meta = Meta {
        id,
        generation,
        reason: snapshot,
        snapshot: snapshot.is_some(),
    };
    let meta = serde_json::to_string(&meta).expect("plain struct serialises");
    format!("id: {id}\nevent: {kind}\ndata: {{\"meta\":{meta},\"payload\":{payload}}}\n\n")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn bus() -> (Epoch, EventBus) {
        (Epoch::new(), EventBus::new())
    }

    #[tokio::test]
    async fn emit_reaches_subscriber_with_increasing_ids() {
        let (e, b) = bus();
        let mut sub = b.subscribe(None);
        for kind in ["a", "b", "c"] {
            b.emit(&e, kind, &serde_json::json!({"k": "v"}));
        }
        let mut prev = 0;
        for want in ["a", "b", "c"] {
            let ev = tokio::time::timeout(Duration::from_millis(100), sub.events.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(ev.kind, want);
            assert!(ev.id > prev);
            prev = ev.id;
        }
    }

    #[test]
    fn replay_after_reconnect() {
        let (e, b) = bus();
        let first = b.subscribe(None);
        for _ in 0..3 {
            b.emit(&e, "ev", &());
        }
        b.unsubscribe(first.id);
        let (_, last) = b.ring_bounds();
        let sub = b.subscribe(Some(last - 1));
        assert!(!sub.truncated);
        assert_eq!(
            sub.replay.iter().map(|ev| ev.id).collect::<Vec<_>>(),
            [last]
        );
    }

    #[test]
    fn first_connect_does_not_replay() {
        let (e, b) = bus();
        b.emit(&e, "old", &());
        let sub = b.subscribe(None);
        assert!(sub.replay.is_empty() && !sub.truncated);
    }

    #[test]
    fn truncated_when_ring_evicted() {
        let (e, b) = bus();
        for _ in 0..RING_SIZE + 10 {
            b.emit(&e, "fill", &());
        }
        assert!(b.subscribe(Some(1)).truncated);
    }

    #[test]
    fn ring_bounds() {
        let (e, b) = bus();
        assert_eq!(b.ring_bounds(), (0, 0));
        b.emit(&e, "x", &());
        b.emit(&e, "y", &());
        let (oldest, newest) = b.ring_bounds();
        assert!(oldest > 0 && oldest < newest);
    }

    #[tokio::test]
    async fn unsubscribe_ends_the_channel() {
        let (_, b) = bus();
        let mut sub = b.subscribe(None);
        b.unsubscribe(sub.id);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), sub.events.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn concurrent_emit_and_subscribe() {
        let (e, b) = bus();
        let (e, b) = (Arc::new(e), Arc::new(b));
        let mut threads = Vec::new();
        for _ in 0..10 {
            let (e, b) = (e.clone(), b.clone());
            threads.push(std::thread::spawn(move || b.emit(&e, "concurrent", &())));
        }
        for _ in 0..5 {
            let b = b.clone();
            threads.push(std::thread::spawn(move || {
                let sub = b.subscribe(None);
                std::thread::sleep(Duration::from_millis(1));
                b.unsubscribe(sub.id);
            }));
        }
        threads.into_iter().for_each(|t| t.join().unwrap());
    }

    // A consumer that stops reading is disconnected, not silently dropped from; others are
    // unaffected and the bus never blocks.
    #[test]
    fn slow_consumer_is_disconnected() {
        let (e, b) = bus();
        let mut slow = b.subscribe(None);
        let mut fast = b.subscribe(None);
        for i in 0..200 {
            b.emit(&e, "tick", &());
            assert!(
                fast.events.try_recv().is_ok(),
                "fast subscriber missed event {i}"
            );
        }
        let mut delivered = 0;
        loop {
            match slow.events.try_recv() {
                Ok(_) => delivered += 1,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty) => {
                    panic!("the slow subscriber's channel must be closed")
                }
            }
        }
        assert!(
            (1..=SUBSCRIBER_BUFFER).contains(&delivered),
            "drained {delivered}"
        );
        assert_eq!(b.subscriber_count(), 1);
    }

    // After an engine restart IDs start over: a Last-Event-ID beyond anything issued means resync.
    #[test]
    fn last_event_id_from_before_a_restart_is_truncated() {
        let (e, b) = bus();
        b.emit(&e, "a", &());
        b.emit(&e, "b", &());
        let sub = b.subscribe(Some(500));
        assert!(sub.truncated && sub.replay.is_empty());
        let sub = b.subscribe(Some(1));
        assert!(
            !sub.truncated && sub.replay.len() == 1,
            "a current Last-Event-ID still replays"
        );
        assert!(
            !EventBus::new().subscribe(Some(0)).truncated,
            "a fresh bus and ID 0 is not flagged"
        );
    }

    #[test]
    fn frame_format() {
        assert_eq!(
            frame(7, "drm", r#"{"a":1}"#, 2, None),
            "id: 7\nevent: drm\ndata: {\"meta\":{\"id\":7,\"generation\":2},\"payload\":{\"a\":1}}\n\n"
        );
        assert_eq!(
            frame(
                1,
                "engine.snapshot",
                "null",
                1,
                Some(EpochReason::EngineStart)
            ),
            "id: 1\nevent: engine.snapshot\ndata: {\"meta\":{\"id\":1,\"generation\":1,\"reason\":\"engine-start\",\"snapshot\":true},\"payload\":null}\n\n"
        );
    }
}
