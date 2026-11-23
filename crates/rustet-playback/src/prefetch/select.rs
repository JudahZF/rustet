//! Which tracks of a context to warm, in what order, and how to read a failure.

use std::sync::LazyLock;

use regex::Regex;

use super::{ContextPayload, FailReason, TrackItem};

/// Most tracks warmed for a context that is a queue, and for a large scored context.
const NEXT_IN_QUEUE: usize = 5;
/// Largest scored context that is warmed whole; above it only the top [`NEXT_IN_QUEUE`] are.
const SCORE_ALL_UP_TO: usize = 25;
/// Tracks warmed for a context of unknown kind.
const DEFAULT_COUNT: usize = 3;

/// The tracks of `payload` to warm. The policy depends on why the renderer sent the context.
pub(super) fn select_tracks(payload: &ContextPayload) -> Vec<TrackItem> {
    let tracks = &payload.tracks;
    if tracks.is_empty() {
        return Vec::new();
    }
    match payload.context.reason.as_str() {
        "queue-change" | "manual-play" => {
            let start = usize::try_from(payload.current_index.saturating_add(1)).unwrap_or(0);
            tracks
                .iter()
                .skip(start)
                .take(NEXT_IN_QUEUE)
                .cloned()
                .collect()
        }
        "hover-preview" => tracks[..1].to_vec(),
        "startup" | "album-open" | "playlist-open" | "radio" => {
            if tracks.len() > SCORE_ALL_UP_TO {
                // Top 5 in one pass: insert into a sorted window of 5. Ties keep the
                // earlier track first.
                let mut top: Vec<(f64, &TrackItem)> = Vec::with_capacity(NEXT_IN_QUEUE + 1);
                for t in tracks {
                    let s = score_track(t);
                    if top.len() < NEXT_IN_QUEUE || top.last().is_some_and(|&(last, _)| s > last) {
                        top.push((s, t));
                        let mut i = top.len() - 1;
                        while i > 0 && top[i].0 > top[i - 1].0 {
                            top.swap(i, i - 1);
                            i -= 1;
                        }
                        top.truncate(NEXT_IN_QUEUE);
                    }
                }
                top.into_iter().map(|(_, t)| t.clone()).collect()
            } else {
                let mut scored: Vec<(f64, &TrackItem)> =
                    tracks.iter().map(|t| (score_track(t), t)).collect();
                scored.sort_by(|a, b| b.0.total_cmp(&a.0));
                scored.into_iter().map(|(_, t)| t.clone()).collect()
            }
        }
        _ => tracks.iter().take(DEFAULT_COUNT).cloned().collect(),
    }
}

/// How likely the user is to play `t` soon. Computed once, when the track is queued.
pub(super) fn score_track(t: &TrackItem) -> f64 {
    let s = &t.signals;
    let mut score = 0.0;
    if s.favorite {
        score += 0.40;
    }
    match s.queue_distance.unsigned_abs() {
        0 => score += 0.35,
        d @ 1..=5 => score += 0.35 * (1.0 - d as f64 / 6.0),
        _ => {}
    }
    score += s.apple_popularity * 0.15;
    score += (s.play_count as f64 / 10.0).min(1.0) * 0.10;
    if s.recent_interaction {
        score += 0.05;
    }
    score -= (s.skip_count as f64 / 5.0).min(1.0) * 0.10;
    score
}

// Status codes must stand alone: error messages carry catalog IDs such as 1440340123, which
// contain "401" as a substring.
static AUTH_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(401|403)\b").expect("valid regex"));
static NOT_FOUND_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b404\b").expect("valid regex"));

/// Maps an open failure to a stable reason code. The provider's errors reach here as text from
/// several layers (HTTP, playlist, licence), so the message is all there is to go on.
pub(super) fn classify_error(message: &str) -> FailReason {
    let msg = message.to_lowercase();
    let has = |needle: &str| msg.contains(needle);
    if has("unauthorized") || has("forbidden") || AUTH_STATUS.is_match(&msg) {
        FailReason::Auth
    } else if has("not found") || NOT_FOUND_STATUS.is_match(&msg) {
        FailReason::NotFound
    } else if has("timeout") || has("deadline exceeded") {
        FailReason::Timeout
    } else if has("connection") || has("network") || has("dial") || has("reset by peer") {
        FailReason::Network
    } else {
        FailReason::RetryExhausted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::TrackSignals;

    #[test]
    fn classify_error_ignores_digits_in_ids() {
        for (msg, want) in [
            (
                "open 1440340123: connection reset by peer",
                FailReason::Network,
            ),
            ("webplayback 1440340123: HTTP 401", FailReason::Auth),
            ("status 403 forbidden", FailReason::Auth),
            ("catalog 1404123: 404", FailReason::NotFound),
            ("asset 9403401: timeout", FailReason::Timeout),
            ("something else", FailReason::RetryExhausted),
        ] {
            assert_eq!(classify_error(msg), want, "{msg}");
        }
    }

    fn track(id: &str, play_count: i64) -> TrackItem {
        TrackItem {
            asset_id: id.into(),
            signals: TrackSignals {
                play_count,
                queue_distance: 99,
                ..TrackSignals::default()
            },
            ..TrackItem::default()
        }
    }

    fn payload(reason: &str, current: i64, tracks: Vec<TrackItem>) -> ContextPayload {
        let mut p = ContextPayload {
            current_index: current,
            tracks,
            ..ContextPayload::default()
        };
        p.context.reason = reason.into();
        p
    }

    fn ids(v: &[TrackItem]) -> Vec<&str> {
        v.iter().map(|t| t.asset_id.as_str()).collect()
    }

    #[test]
    fn selection_policies() {
        let ten: Vec<TrackItem> = (0..10).map(|i| track(&i.to_string(), i)).collect();
        // A queue: the next five after the current track.
        assert_eq!(
            ids(&select_tracks(&payload("queue-change", 2, ten.clone()))),
            ["3", "4", "5", "6", "7"]
        );
        assert!(select_tracks(&payload("manual-play", 9, ten.clone())).is_empty());
        assert_eq!(
            ids(&select_tracks(&payload("manual-play", -5, ten.clone()))),
            ["0", "1", "2", "3", "4"]
        );
        assert_eq!(
            ids(&select_tracks(&payload("hover-preview", 0, ten.clone()))),
            ["0"]
        );
        // A small scored context: everything, best first.
        assert_eq!(
            ids(&select_tracks(&payload("album-open", 0, ten.clone()))),
            ["9", "8", "7", "6", "5", "4", "3", "2", "1", "0"]
        );
        assert_eq!(
            ids(&select_tracks(&payload("other", 0, ten))),
            ["0", "1", "2"]
        );
        // A large scored context: only the top five.
        let many: Vec<TrackItem> = (0..30).map(|i| track(&i.to_string(), i % 12)).collect();
        assert_eq!(
            ids(&select_tracks(&payload("radio", 0, many))),
            ["10", "11", "22", "23", "9"]
        );
    }

    #[test]
    fn score_weights() {
        let mut t = TrackItem::default();
        assert!((score_track(&t) - 0.35).abs() < 1e-9, "distance 0");
        t.signals.queue_distance = -3;
        assert!(
            (score_track(&t) - 0.175).abs() < 1e-9,
            "distance is absolute"
        );
        t.signals = TrackSignals {
            favorite: true,
            queue_distance: 6,
            apple_popularity: 1.0,
            play_count: 50,
            skip_count: 50,
            recent_interaction: true,
            ..TrackSignals::default()
        };
        assert!((score_track(&t) - (0.40 + 0.15 + 0.10 + 0.05 - 0.10)).abs() < 1e-9);
    }
}
