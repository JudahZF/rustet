//! The player controller (`rustet-player`) wired to the engine (spec §4).
//!
//! The ports are implemented over the engine's own parts: sessions and the pre-warm scheduler
//! ([`opener`], [`QueuePrewarm`]), two libvlc decks fed from the disk cache ([`deck`], [`spool`];
//! a music video's video and audio joined by [`mvmux`], its frames in a ring in `video_dir`),
//! the engine data directory ([`store`]) and Apple's API for stations, autoplay and play reports
//! ([`apple`]). Every adapter holds a weak reference: the engine owns the player, not the
//! reverse. Player events go to the SSE stream under their own names (`player.state`, …).

mod apple;
pub(crate) use apple::AppleLink;
pub(crate) mod deck;
mod mvmux;
mod opener;
mod spool;
mod store;

use std::sync::{Arc, Weak};

use rustet_playback::prefetch::{ContextPayload, PayloadContext, TrackItem, TrackSignals};
use rustet_player::model::Event;
use rustet_player::ports::{Deck, DeckSignal, Ports, PrewarmSink, PrewarmWindow, SystemClock};
use rustet_player::{Player, PlayerHandle};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::state::Engine;

#[cfg(test)]
pub(crate) use spool::start as spool_start;
pub(crate) use store::create_private_dir;

impl Engine {
    /// Starts the player over `decks`, whose events arrive on `deck_events`, and forwards its
    /// events to the SSE stream. Once per engine; later calls do nothing.
    pub fn start_player(
        self: &Arc<Self>,
        decks: [Arc<dyn Deck>; 2],
        deck_events: UnboundedReceiver<DeckSignal>,
    ) {
        if self.player.get().is_some() {
            return;
        }
        let me = Arc::downgrade(self);
        let apple = self.apple().clone();
        let ports = Ports {
            opener: Arc::new(opener::EngineOpener(me.clone())),
            decks: decks.clone(),
            clock: Arc::new(SystemClock),
            catalog: apple.clone(),
            reporter: apple,
            store: Arc::new(store::FileStore::new(self.data_dir.clone())),
            prewarm: Arc::new(QueuePrewarm(me.clone())),
        };
        let handle = Player::spawn(ports, deck_events);
        tokio::spawn(forward(me, handle.clone()));
        let _ = self.player_decks.set(decks);
        let _ = self.player.set(handle);
        self.notify_player_caps();
    }
}

/// Starts the player on two libvlc decks (on a libvlc instance of their own, beside the web
/// shell's player), or on [`deck::NoDeck`]s without libvlc.
pub(crate) async fn start_with_libvlc(engine: &Arc<Engine>) {
    let (dir, video) = (engine.spool_dir.clone(), engine.video_dir.clone());
    let _ = tokio::task::spawn_blocking(move || {
        spool::sweep(&dir);
        // Frame rings of an engine that crashed.
        if let Some(video) = video {
            rustet_framering::sweep(&video);
        }
    })
    .await;
    let (tx, rx) = rustet_player::ports::deck_channel();
    let decks: [Arc<dyn Deck>; 2] = match deck::libvlc_decks(engine, &[], tx).await {
        Some([a, b]) => [a, b],
        None => [Arc::new(deck::NoDeck), Arc::new(deck::NoDeck)],
    };
    engine.start_player(decks, rx);
}

/// Stops both decks (engine shutdown).
pub(crate) async fn stop_decks(engine: &Engine) {
    for deck in engine.player_decks.get().into_iter().flatten() {
        let _ = deck.stop().await;
    }
}

/// Emits a player event on the SSE stream: the event's name, its payload as the frame payload.
fn emit(engine: &Engine, event: &Event) {
    let payload = serde_json::to_value(event)
        .ok()
        .and_then(|mut v| v.get_mut("payload").map(serde_json::Value::take))
        .unwrap_or_else(|| serde_json::json!({}));
    engine.emit(event.name(), &payload);
}

/// Copies player events to the SSE bus until the engine stops. A lag skips to the current state.
async fn forward(engine: Weak<Engine>, handle: PlayerHandle) {
    let mut events = handle.subscribe();
    let Some(shutdown) = engine.upgrade().map(|e| e.shutdown.clone()) else {
        return;
    };
    loop {
        let event = tokio::select! {
            () = shutdown.cancelled() => return,
            r = events.recv() => match r {
                Ok(event) => event,
                Err(RecvError::Lagged(_)) => Event::State(Box::new(handle.state())),
                Err(RecvError::Closed) => return,
            },
        };
        let Some(engine) = engine.upgrade() else {
            return;
        };
        emit(&engine, &event);
    }
}

/// The player's queue window, submitted to the pre-warm scheduler as `PUT /api/v1/playback/context`
/// would with reason `queue-change`.
struct QueuePrewarm(Weak<Engine>);

impl PrewarmSink for QueuePrewarm {
    fn submit(&self, window: PrewarmWindow) {
        let Some(engine) = self.0.upgrade() else {
            return;
        };
        let Some(playback) = &engine.playback else {
            return;
        };
        playback.scheduler.submit(&context_payload(
            &window,
            &engine.tokens.playback_storefront(),
        ));
    }
}

fn context_payload(window: &PrewarmWindow, storefront: &str) -> ContextPayload {
    // The scheduler counts from the current track's position in the list it is given.
    let current = window
        .current_index
        .and_then(|c| window.tracks.iter().position(|t| t.index == c))
        .map_or(-1, |i| i as i64);
    ContextPayload {
        context: PayloadContext {
            kind: "queue".into(),
            id: String::new(),
            reason: "queue-change".into(),
        },
        current_index: current,
        tracks: window
            .tracks
            .iter()
            .map(|t| TrackItem {
                asset_id: t.id.0.clone(),
                storefront: storefront.to_owned(),
                signals: TrackSignals {
                    queue_distance: t.queue_distance as i64,
                    ..TrackSignals::default()
                },
                ..TrackItem::default()
            })
            .collect(),
        lossless: window.lossless,
    }
}

#[cfg(test)]
mod tests {
    use rustet_player::model::CatalogId;
    use rustet_player::ports::PrewarmTrack;

    use super::*;

    // The scheduler warms the tracks after `current_index` of the list it gets, and the player's
    // window starts up to ten tracks before the current one.
    #[test]
    fn the_window_is_a_queue_change_context() {
        let track = |index, distance| PrewarmTrack {
            id: CatalogId(format!("{index}")),
            index,
            queue_distance: distance,
        };
        let window = PrewarmWindow {
            current_index: Some(12),
            lossless: true,
            tracks: vec![track(11, 1), track(12, 0), track(13, 1)],
        };
        let p = context_payload(&window, "143441");
        assert_eq!(
            (p.context.reason.as_str(), p.current_index, p.lossless),
            ("queue-change", 1, true)
        );
        assert_eq!(
            p.tracks
                .iter()
                .map(|t| t.asset_id.as_str())
                .collect::<Vec<_>>(),
            ["11", "12", "13"]
        );
        assert_eq!(
            (
                p.tracks[2].storefront.as_str(),
                p.tracks[2].signals.queue_distance
            ),
            ("143441", 1)
        );
        let empty = PrewarmWindow {
            current_index: None,
            lossless: false,
            tracks: vec![track(0, 0)],
        };
        assert_eq!(
            context_payload(&empty, "").current_index,
            -1,
            "no current track: warm from the start"
        );
    }
}
