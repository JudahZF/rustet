//! The engine's event stream (`GET /api/v1/events`, server-sent events) as typed events.
//!
//! Wire format of a frame: `id: N\nevent: NAME\ndata: {"meta":{…},"payload":…}\n\n`. Every
//! connection starts with an `engine.snapshot`. After a drop the stream reconnects with
//! `Last-Event-ID`; the engine replays what was missed, or sends `replay.truncated` when it
//! cannot (its ring was overwritten, or it restarted and its IDs started over).

use std::time::Duration;

use futures::StreamExt;
use rustet_player::model::{Event as PlayerEvent, PlayerState};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::client::EngineClient;
use super::types::{DrmStatus, ExportProgress, Snapshot, SnapshotCapabilities};

/// One event from the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The engine's state at the start of a connection (after every engine start and every
    /// reconnect). A view replaces what it shows with this.
    Snapshot(Box<Snapshot>),
    /// The player (spec §4.8), and `auth.expired` ([`PlayerEvent::AuthExpired`]).
    Player(PlayerEvent),
    /// The lossless account changed.
    Drm(Box<DrmStatus>),
    /// An export's progress.
    Export(ExportProgress),
    /// Events were missed and cannot be replayed: fetch lists (exports, history) again.
    ReplayTruncated,
    /// Any other event (`drm.refresh_due`, `playback.*`, …), undecoded.
    Other { name: String, payload: Value },
}

/// One parsed SSE frame.
#[derive(Debug, Default, PartialEq)]
struct Frame {
    id: Option<i64>,
    event: String,
    data: String,
}

/// Splits SSE text into frames. Keeps the unfinished tail between chunks.
#[derive(Default)]
struct FrameReader {
    buf: String,
    frame: Frame,
}

impl FrameReader {
    fn push(&mut self, chunk: &str, out: &mut Vec<Frame>) {
        self.buf.push_str(chunk);
        while let Some(end) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                let frame = std::mem::take(&mut self.frame);
                if !frame.data.is_empty() || !frame.event.is_empty() {
                    out.push(frame);
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "id" => self.frame.id = value.parse().ok(),
                "event" => self.frame.event = value.to_owned(),
                "data" => {
                    if !self.frame.data.is_empty() {
                        self.frame.data.push('\n');
                    }
                    self.frame.data.push_str(value);
                }
                _ => {}
            }
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Meta {
    generation: u64,
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Envelope {
    meta: Meta,
    payload: Value,
}

/// The `engine.snapshot` payload: `{"version":1,"snapshot":{…}}`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct SnapshotPayload {
    snapshot: SnapshotBody,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SnapshotBody {
    capabilities: SnapshotCapabilities,
    player: Option<PlayerState>,
}

/// What the stream does with a frame.
#[derive(Debug, PartialEq)]
enum Decoded {
    Event(EngineEvent),
    /// The engine dropped this subscriber for falling behind: reconnect now.
    Lagged,
    /// Heartbeats and anything that does not parse.
    Skip,
}

fn decode(frame: &Frame) -> Decoded {
    let Ok(env) = serde_json::from_str::<Envelope>(&frame.data) else {
        debug!("undecodable {} event", frame.event);
        return Decoded::Skip;
    };
    let event = match frame.event.as_str() {
        "ping" => return Decoded::Skip,
        "stream.lagged" => return Decoded::Lagged,
        "replay.truncated" => EngineEvent::ReplayTruncated,
        "engine.snapshot" => {
            let body = serde_json::from_value::<SnapshotPayload>(env.payload)
                .unwrap_or_default()
                .snapshot;
            EngineEvent::Snapshot(Box::new(Snapshot {
                generation: env.meta.generation,
                reason: env.meta.reason,
                player: body.player,
                capabilities: body.capabilities,
            }))
        }
        // The engine sends `{}` as its payload; the player's unit variant takes none.
        "auth.expired" => EngineEvent::Player(PlayerEvent::AuthExpired),
        "drm" => match serde_json::from_value(env.payload) {
            Ok(status) => EngineEvent::Drm(Box::new(status)),
            Err(_) => return Decoded::Skip,
        },
        "export" => match serde_json::from_value(env.payload) {
            Ok(progress) => EngineEvent::Export(progress),
            Err(_) => return Decoded::Skip,
        },
        // The engine strips the player event's envelope: put it back to decode the variant.
        name if name.starts_with("player.") => {
            match serde_json::from_value(json!({ "type": name, "payload": env.payload })) {
                Ok(event) => EngineEvent::Player(event),
                Err(e) => {
                    debug!("undecodable {name} event: {e}");
                    return Decoded::Skip;
                }
            }
        }
        name => EngineEvent::Other {
            name: name.to_owned(),
            payload: env.payload,
        },
    };
    Decoded::Event(event)
}

/// Retry delays of the stream: 250 ms, doubled after each failed connect, at most 2 s.
const RETRY_FIRST: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(2);
/// Events a slow receiver may fall behind before it sees `Lagged`.
const CHANNEL: usize = 256;

/// Starts the event stream: it connects, reconnects with `Last-Event-ID` after every drop, and
/// stops when `cancel` fires. The `bool` watch is true while a connection is open (from its
/// snapshot on). A receiver that lags must fetch the state again
/// ([`EngineClient::player_state`]).
pub fn spawn(
    client: EngineClient,
    cancel: CancellationToken,
) -> (broadcast::Sender<EngineEvent>, watch::Receiver<bool>) {
    let (tx, _) = broadcast::channel(CHANNEL);
    let (link_tx, link) = watch::channel(false);
    tokio::spawn(run(client, tx.clone(), link_tx, cancel));
    (tx, link)
}

async fn run(
    client: EngineClient,
    tx: broadcast::Sender<EngineEvent>,
    link: watch::Sender<bool>,
    cancel: CancellationToken,
) {
    let mut last_id: Option<i64> = None;
    let mut delay = RETRY_FIRST;
    loop {
        let connected = tokio::select! {
            () = cancel.cancelled() => return,
            c = connect_once(&client, &tx, &link, &mut last_id) => c,
        };
        link.send_replace(false);
        if connected {
            delay = RETRY_FIRST;
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        if !connected {
            delay = (delay * 2).min(RETRY_MAX);
        }
    }
}

/// One connection, until it ends. Returns whether it got as far as the first frame.
async fn connect_once(
    client: &EngineClient,
    tx: &broadcast::Sender<EngineEvent>,
    link: &watch::Sender<bool>,
    last_id: &mut Option<i64>,
) -> bool {
    let mut req = client
        .http()
        .get(client.url("/api/v1/events"))
        .header(reqwest::header::ACCEPT, "text/event-stream");
    if let Some(id) = *last_id {
        req = req.header("Last-Event-ID", id.to_string());
    }
    let res = match req.send().await {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => {
            debug!("event stream refused: {}", res.status());
            return false;
        }
        Err(e) => {
            debug!("event stream: {e}");
            return false;
        }
    };
    let mut body = res.bytes_stream();
    let mut reader = FrameReader::default();
    let mut frames = Vec::new();
    let mut opened = false;
    // A chunk may end inside a UTF-8 sequence: keep the incomplete bytes for the next one.
    let mut pending = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        pending.extend_from_slice(&chunk);
        let valid = match std::str::from_utf8(&pending) {
            Ok(s) => s.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
        pending.drain(..valid);
        reader.push(&text, &mut frames);
        for frame in frames.drain(..) {
            if let Some(id) = frame.id {
                *last_id = Some(id);
            }
            match decode(&frame) {
                Decoded::Event(event) => {
                    if !opened {
                        opened = true;
                        info!("engine event stream connected");
                        link.send_replace(true);
                    }
                    // No receiver is fine: nobody listens yet.
                    let _ = tx.send(event);
                }
                Decoded::Lagged => return opened,
                Decoded::Skip => {}
            }
        }
    }
    opened
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(text: &str) -> Vec<Frame> {
        let mut r = FrameReader::default();
        let mut out = Vec::new();
        // Byte-by-byte: frames survive any chunking.
        for c in text.chars() {
            r.push(&c.to_string(), &mut out);
        }
        out
    }

    #[test]
    fn frames_parse_across_chunks() {
        let got =
            frames(": hi\r\nid: 7\nevent: drm\ndata: {\"a\":\ndata: 1}\n\nevent: x\ndata: 2\n\n");
        assert_eq!(
            got,
            [
                Frame {
                    id: Some(7),
                    event: "drm".into(),
                    data: "{\"a\":\n1}".into()
                },
                Frame {
                    id: None,
                    event: "x".into(),
                    data: "2".into()
                },
            ]
        );
    }

    #[test]
    fn engine_frames_decode_to_typed_events() {
        let f = |event: &str, payload: Value| {
            Frame {
            id: Some(1),
            event: event.into(),
            data: json!({ "meta": { "id": 1, "generation": 4, "reason": "engine-start" }, "payload": payload })
                .to_string(),
        }
        };
        let Decoded::Event(EngineEvent::Snapshot(s)) = decode(&f(
            "engine.snapshot",
            json!({ "version": 1, "snapshot": { "capabilities": { "lossless": true, "musicVideo": true },
                    "player": serde_json::to_value(PlayerState::default()).unwrap() } }),
        )) else {
            panic!("snapshot")
        };
        assert_eq!(
            (s.generation, s.reason.as_deref()),
            (4, Some("engine-start"))
        );
        assert!(s.capabilities.lossless && s.player.is_some());

        let position =
            json!({ "generation": 1, "uid": 2, "positionMs": 3, "durationMs": 4, "positionAt": 5 });
        assert!(matches!(
            decode(&f("player.position", position)),
            Decoded::Event(EngineEvent::Player(PlayerEvent::Position(p))) if p.uid == 2
        ));
        assert_eq!(
            decode(&f("auth.expired", json!({}))),
            Decoded::Event(EngineEvent::Player(PlayerEvent::AuthExpired))
        );
        assert!(matches!(
            decode(&f("drm", json!({ "state": { "authentication": "challenging", "fairplay": "brand_new" },
                "challenge": { "id": 3, "type": "two_factor", "title": "Code" } }))),
            Decoded::Event(EngineEvent::Drm(d)) if d.challenge.as_ref().is_some_and(|c| c.id == 3)
        ));
        assert_eq!(decode(&f("ping", json!({ "ts": 1 }))), Decoded::Skip);
        assert_eq!(decode(&f("stream.lagged", json!({}))), Decoded::Lagged);
        assert_eq!(
            decode(&f("replay.truncated", json!({}))),
            Decoded::Event(EngineEvent::ReplayTruncated)
        );
        assert!(matches!(
            decode(&f("drm.refresh_due", json!({ "x": 1 }))),
            Decoded::Event(EngineEvent::Other { name, .. }) if name == "drm.refresh_due"
        ));
    }
}
