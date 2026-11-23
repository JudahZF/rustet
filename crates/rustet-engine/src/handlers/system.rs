//! Health, capabilities, metrics and the SSE event channel.

use std::time::{Duration, SystemTime};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::mpsc;

use crate::epoch::EpochReason;
use crate::events::{Subscription, frame};
use crate::http::json;
use crate::state::{AppState, unix_ms};

pub(crate) async fn status() -> Response {
    json(StatusCode::OK, &json!({"status": "ok"}))
}

/// Feature flags for the renderer, from the DRM manager's state.
pub(crate) async fn capabilities(State(s): State<AppState>) -> Response {
    let snap = s.drm_status();
    let cap = snap.capabilities;
    json(
        StatusCode::OK,
        &json!({
            "lossless": cap.alac,
            "hiRes": cap.hi_res,
            "atmos": cap.atmos,
            "musicVideo": true,
            "downloads": true,
            "lyrics": true,
            // DRM detail for frontends that show granular state.
            "drm": {
                "process": snap.state.process,
                "fairplay": snap.state.fair_play,
                "session": snap.state.session,
                "cbcs": cap.cbcs,
                // False when the build has no DRM layer: lossless can never become ready.
                "available": s.drm.is_some(),
            },
        }),
    )
}

/// `GET /api/v1/tools[?ffmpegPath=]`: whether FFmpeg can really be used. It is resolved the
/// way exports resolve it (the optional custom path, validated the same way) and run, not just
/// looked up.
pub(crate) async fn tools(uri: axum::http::Uri) -> Response {
    // A struct, not `json!`: the probe's fields keep their order.
    #[derive(serde::Serialize)]
    struct Tools {
        ffmpeg: rustet_export::FfmpegInfo,
    }
    let custom = crate::http::query_param(&uri, "ffmpegPath");
    json(
        StatusCode::OK,
        &Tools {
            ffmpeg: rustet_export::probe_ffmpeg(&custom).await,
        },
    )
}

/// Session-open latency statistics (last 100 opens, ms) and the circuit-breaker state.
pub(crate) async fn metrics(State(s): State<AppState>) -> Response {
    let samples = s.open_latency.snapshot();
    let (mut avg_ms, mut p95_ms) = (0.0, 0.0);
    if !samples.is_empty() {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        avg_ms = sorted.iter().sum::<i64>() as f64 / sorted.len() as f64;
        let idx = ((sorted.len() as f64 * 0.95) as usize).saturating_sub(1);
        p95_ms = sorted[idx] as f64;
    }
    json(
        StatusCode::OK,
        &json!({
            // `null` when empty, as Go wrote a nil slice.
            "sessionOpen": {"count": samples.len(), "avgMs": avg_ms, "p95Ms": p95_ms, "samples": (!samples.is_empty()).then_some(&samples)},
            "circuitBreaker": {"state": s.open_breaker.state()},
        }),
    )
}

/// Process-internal runtime metrics for the benchmark harness: what cannot be read from
/// `/proc`.
pub(crate) async fn runtime_stats(State(s): State<AppState>) -> Response {
    let m = tokio::runtime::Handle::current().metrics();
    let mut resp = json!({
        "tasks": m.num_alive_tasks(),
        "workers": m.num_workers(),
        "globalQueueDepth": m.global_queue_depth(),
    });
    if let Some(p) = &s.playback {
        resp["prefetch"] = json!(p.scheduler.stats());
    }
    json(StatusCode::OK, &resp)
}

/// How often a `ping` keeps idle proxies from closing the stream (and gives clients a
/// watchdog signal).
const HEARTBEAT: Duration = Duration::from_secs(30);

/// `GET /api/v1/events`: the SSE push channel.
///
/// Every connection starts with an `engine.snapshot` of the current state. A reconnecting
/// client sends `Last-Event-ID` and gets the events it missed from the ring, or a
/// `replay.truncated` when they are gone and it must resync.
pub(crate) async fn events(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&id| id >= 0);
    let sub = s.events.subscribe(after);

    // The body ends when the writer task ends; the task ends when the client goes away (the
    // body, and with it the receiver, is dropped) or the engine stops.
    let (tx, rx) = mpsc::channel::<Bytes>(16);
    tokio::spawn(write_events(s, sub, after, tx));
    let body = Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|b| (Ok::<_, std::convert::Infallible>(b), rx))
    }));
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (header::CONNECTION, HeaderValue::from_static("keep-alive")),
        ],
        body,
    )
        .into_response()
}

async fn write_events(
    s: AppState,
    mut sub: Subscription,
    after: Option<i64>,
    tx: mpsc::Sender<Bytes>,
) {
    let send = |f: String| {
        let tx = tx.clone();
        async move { tx.send(Bytes::from(f)).await.is_ok() }
    };
    let epoch = s.epoch.current();
    let live = async {
        if !send(snapshot_frame(&s, epoch.generation, epoch.reason)).await {
            return;
        }
        if sub.truncated {
            let (oldest, newest) = s.events.ring_bounds();
            let payload = json!({
                "requestedAfter": after.unwrap_or(-1),
                "oldestAvailable": oldest,
                "newestAvailable": newest,
                "reason": "history_evicted",
            });
            if !send(frame(
                s.events.next_id(),
                "replay.truncated",
                &payload.to_string(),
                epoch.generation,
                None,
            ))
            .await
            {
                return;
            }
        } else {
            // Replayed events keep their original generation, so the client can order them
            // against the snapshot.
            for ev in std::mem::take(&mut sub.replay) {
                if !send(frame(ev.id, ev.kind, &ev.payload, ev.generation, None)).await {
                    return;
                }
            }
        }

        let mut heartbeat =
            tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
        loop {
            let f = tokio::select! {
                () = tx.closed() => return,
                _ = heartbeat.tick() => {
                    let payload = json!({"ts": unix_ms(SystemTime::now())}).to_string();
                    frame(s.events.next_id(), "ping", &payload, epoch.generation, None)
                }
                ev = sub.events.recv() => match ev {
                    Some(ev) => frame(ev.id, ev.kind, &ev.payload, ev.generation, None),
                    // The bus disconnected this subscriber because it fell behind (it is never
                    // ended for any other reason while the client is connected).
                    None => {
                        let payload = json!({"reason": "slow_consumer", "action": "reconnect with Last-Event-ID"});
                        send(frame(s.events.next_id(), "stream.lagged", &payload.to_string(), epoch.generation, None)).await;
                        return;
                    }
                },
            };
            if !send(f).await {
                return;
            }
        }
    };
    tokio::select! {
        () = live => {}
        // Engine shutdown: end the stream so the graceful shutdown does not wait on it.
        () = s.shutdown.cancelled() => {}
    }
    s.events.unsubscribe(sub.id);
}

/// The `engine.snapshot` frame: the current epoch, the reason it last advanced, and the state
/// a client needs without replaying history.
fn snapshot_frame(s: &AppState, generation: u64, reason: EpochReason) -> String {
    let snap = s.drm_status();
    let cap = snap.capabilities;
    let mut state = json!({
        "drm": {
            "state": snap.state,
            "session": snap.state.session,
            "cbcs": cap.cbcs,
            "lossless": cap.alac,
            "hiRes": cap.hi_res,
            "atmos": cap.atmos,
        },
        "capabilities": {"lossless": cap.alac, "hiRes": cap.hi_res, "atmos": cap.atmos, "musicVideo": true},
    });
    if let Some(p) = &s.playback {
        state["prefetch"] = json!(p.scheduler.stats());
    }
    if let Some(player) = s.player.get() {
        state["player"] = json!(player.state());
    }
    let payload = json!({"version": 1, "snapshot": state}).to_string();
    frame(
        s.events.next_id(),
        "engine.snapshot",
        &payload,
        generation,
        Some(reason),
    )
}
