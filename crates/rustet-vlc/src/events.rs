//! libvlc media player events, forwarded to a channel.
//!
//! libvlc calls event callbacks on its own threads (mostly the input thread) with its event
//! manager locked, and forbids calling back into libvlc from them. The callback here only reads
//! the event, tags it and sends it on an unbounded channel, which never blocks.

use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::Error;
use crate::ffi::{self, Api, EventManager, MediaPlayer, event};

/// Identifies a deck among the decks of one [`Instance`](crate::Instance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeckId(pub(crate) u32);

/// Identifies one load of a deck. Events carry the load they belong to, so events of a track
/// that was already replaced, still queued in the channel, can be told apart and dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LoadId(pub(crate) u64);

/// A media player event, as libvlc reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// libvlc is filling its input buffer, 0–100 %. Sent when the whole percent changes; also
    /// sent at 0 after a seek.
    Buffering(u8),
    Playing,
    Paused,
    /// libvlc reached the end of the media, after its audio output drained. A failed read also
    /// ends the media: check [`Deck::source_ended`](crate::Deck::source_ended) to tell a real
    /// end from a failure.
    EndReached,
    /// libvlc could not open or play the media.
    Error,
    /// The position in ms. At most one per 200 ms; libvlc itself reports about every 250 ms.
    TimeChanged(i64),
    /// The media length in ms, once the demuxer knows it.
    LengthChanged(i64),
    /// A video load showed its first frame in a new frame ring ([`Deck::video_ring`](crate::Deck::video_ring)
    /// has its path), or its frame size changed and a ring of the new size replaced the last.
    VideoStarted {
        generation: u64,
        width: u32,
        height: u32,
    },
    /// The load's frame ring was closed: a new load, a stop, or the end of its video.
    VideoStopped,
}

/// An [`Event`] of one load of one deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckEvent {
    pub deck: DeckId,
    pub load: LoadId,
    pub event: Event,
}

/// Minimum spacing of forwarded [`Event::TimeChanged`].
const TIME_EVENT_INTERVAL: Duration = Duration::from_millis(200);
const NONE: u64 = u64::MAX;
const KINDS: [c_int; 7] = [
    event::BUFFERING,
    event::PLAYING,
    event::PAUSED,
    event::END_REACHED,
    event::ERROR,
    event::TIME_CHANGED,
    event::LENGTH_CHANGED,
];

/// The callback's state. libvlc holds a pointer to it from attach to detach.
pub(crate) struct Sink {
    deck: DeckId,
    tx: UnboundedSender<DeckEvent>,
    load: AtomicU64,
    epoch: Instant,
    /// µs since `epoch` of the last forwarded time event, `NONE` before the first of a load.
    last_time: AtomicU64,
    /// The last forwarded buffering percent, `u32::MAX` before the first of a load.
    last_cache: AtomicU32,
}

impl Sink {
    pub(crate) fn new(deck: DeckId, tx: UnboundedSender<DeckEvent>) -> Self {
        Self {
            deck,
            tx,
            load: AtomicU64::new(0),
            epoch: Instant::now(),
            last_time: AtomicU64::new(NONE),
            last_cache: AtomicU32::new(u32::MAX),
        }
    }

    /// Starts a new load: later events carry `load`, and the first time and buffering events
    /// of the load always pass. Called before libvlc starts the input thread.
    /// A sender for events of this deck from other libvlc callbacks (video).
    pub(crate) fn sender(&self) -> UnboundedSender<DeckEvent> {
        self.tx.clone()
    }

    pub(crate) fn begin(&self, load: LoadId) {
        self.load.store(load.0, Ordering::Release);
        self.last_time.store(NONE, Ordering::Relaxed);
        self.last_cache.store(u32::MAX, Ordering::Relaxed);
    }

    /// Converts a raw event, applying the rate limits. Only libvlc's input thread calls this
    /// for one media, so the load-then-store updates do not race.
    fn convert(&self, raw: &ffi::Event, now: Instant) -> Option<Event> {
        // SAFETY: each union member is read only for the event kind libvlc fills it for.
        Some(match raw.kind {
            event::PLAYING => Event::Playing,
            event::PAUSED => Event::Paused,
            event::END_REACHED => Event::EndReached,
            event::ERROR => Event::Error,
            event::LENGTH_CHANGED => Event::LengthChanged(unsafe { raw.u.new_time }),
            event::BUFFERING => {
                let cache = unsafe { raw.u.new_cache };
                let percent = cache.clamp(0.0, 100.0) as u8;
                let last = self.last_cache.swap(u32::from(percent), Ordering::Relaxed);
                if last == u32::from(percent) {
                    return None;
                }
                Event::Buffering(percent)
            }
            event::TIME_CHANGED => {
                let now = now.duration_since(self.epoch).as_micros() as u64;
                let last = self.last_time.load(Ordering::Relaxed);
                if last != NONE && now.saturating_sub(last) < TIME_EVENT_INTERVAL.as_micros() as u64
                {
                    return None;
                }
                self.last_time.store(now, Ordering::Relaxed);
                Event::TimeChanged(unsafe { raw.u.new_time })
            }
            _ => return None,
        })
    }
}

unsafe extern "C" fn on_event(raw: *const ffi::Event, data: *mut c_void) {
    // SAFETY: libvlc passes a valid event and the `Sink` pointer given at attach; the deck keeps
    // the sink alive until it has detached, and detach waits for running callbacks (libvlc 3
    // runs callbacks and detach under the event manager's lock).
    let (raw, sink) = unsafe { (&*raw, &*data.cast::<Sink>()) };
    if let Some(event) = sink.convert(raw, Instant::now()) {
        let load = LoadId(sink.load.load(Ordering::Acquire));
        // A dropped receiver only means nobody listens any more.
        let _ = sink.tx.send(DeckEvent {
            deck: sink.deck,
            load,
            event,
        });
    }
}

/// The event callbacks attached to one media player. Dropping detaches them.
pub(crate) struct Events {
    api: &'static Api,
    em: NonNull<EventManager>,
    sink: Box<Sink>,
    /// How many of [`KINDS`] are attached.
    attached: usize,
}

// SAFETY: the event manager may be used from any thread; the sink is Sync.
unsafe impl Send for Events {}

impl Events {
    /// # Safety
    /// `mp` must be a live media player that outlives the returned value.
    pub(crate) unsafe fn attach(
        api: &'static Api,
        mp: NonNull<MediaPlayer>,
        sink: Sink,
    ) -> Result<Self, Error> {
        // SAFETY: `mp` is live; its event manager lives as long as it does.
        let em = NonNull::new(unsafe { (api.libvlc_media_player_event_manager)(mp.as_ptr()) })
            .ok_or(Error::PlayerFailed)?;
        let mut events = Self {
            api,
            em,
            sink: Box::new(sink),
            attached: 0,
        };
        for kind in KINDS {
            // SAFETY: `em` is live; the sink stays at its heap address until drop detaches it.
            if unsafe { (api.libvlc_event_attach)(em.as_ptr(), kind, on_event, events.data()) } != 0
            {
                return Err(Error::PlayerFailed); // drop detaches the ones attached so far
            }
            events.attached += 1;
        }
        Ok(events)
    }

    pub(crate) fn sink(&self) -> &Sink {
        &self.sink
    }

    fn data(&self) -> *mut c_void {
        std::ptr::from_ref::<Sink>(&self.sink).cast_mut().cast()
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        for kind in &KINDS[..self.attached] {
            // SAFETY: `em` is live and `on_event`/`data` are attached for `kind`. Detach takes the
            // lock callbacks run under, so none is running once it returns.
            unsafe {
                (self.api.libvlc_event_detach)(self.em.as_ptr(), *kind, on_event, self.data())
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::EventData;
    use tokio::sync::mpsc;

    fn raw(kind: c_int, u: EventData) -> ffi::Event {
        ffi::Event {
            kind,
            _obj: std::ptr::null_mut(),
            u,
        }
    }

    fn deliver(sink: &Sink, ev: &ffi::Event) {
        // SAFETY: a valid event and sink, as libvlc would pass them.
        unsafe { on_event(ev, std::ptr::from_ref(sink).cast_mut().cast()) };
    }

    // The callback tags events with the current load and rate-limits the chatty ones: the UI
    // gets the position and buffering without flooding the controller.
    #[test]
    fn forwards_tagged_and_rate_limited() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = Sink::new(DeckId(1), tx);
        sink.begin(LoadId(7));
        let time = raw(event::TIME_CHANGED, EventData { new_time: 1000 });
        let cache = |c| raw(event::BUFFERING, EventData { new_cache: c });
        deliver(&sink, &time);
        deliver(&sink, &time); // within 200 ms: dropped
        deliver(&sink, &cache(50.2));
        deliver(&sink, &cache(50.9)); // same whole percent: dropped
        deliver(&sink, &cache(100.0));
        deliver(&sink, &raw(event::END_REACHED, EventData { new_time: 0 }));
        deliver(&sink, &raw(event::PAUSED + 0x40, EventData { new_time: 0 })); // not ours
        sink.begin(LoadId(8));
        deliver(&sink, &time); // a new load resets the limit

        let got: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| (e.load.0, e.event))
            .collect();
        assert_eq!(
            got,
            [
                (7, Event::TimeChanged(1000)),
                (7, Event::Buffering(50)),
                (7, Event::Buffering(100)),
                (7, Event::EndReached),
                (8, Event::TimeChanged(1000)),
            ]
        );
    }
}
