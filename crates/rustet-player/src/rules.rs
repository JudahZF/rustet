//! Pure playback rules: end-of-track classification, track length, mode choice, quality badge,
//! scrobble threshold and the status transition relation. Ported from the renderer
//! (`vlc-end.js`, `track-end.js`, `playstate.js`, `engine-playback.js`).

use std::time::Duration;

use crate::model::{
    Capabilities, Codec, ItemKind, PlayerSettings, Quality, QualityTier, Status, StreamingQuality,
    TrackMeta,
};
use crate::ports::{OpenMode, OpenedSession};

// ── End of track (`vlc-end.js`) ─────────────────────────────────────────────

/// Reloads of one track after a false end.
pub const MAX_END_RETRIES: u32 = 2;

/// How the deck stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndKind {
    /// libvlc reached the end of the media (or stopped without being asked).
    Ended,
    /// libvlc reported an error.
    Error,
}

/// What to do when a deck stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndAction {
    /// Reload the same session at `from_ms` after `delay`.
    Retry { from_ms: u64, delay: Duration },
    /// The track is over. `short_of_reported`: the data ended cleanly well before the catalog length.
    Advance { short_of_reported: bool },
}

/// Classifies a deck stop.
///
/// A genuine failure (a read that broke, a stream that never started) is worth a reload at the last position. A track
/// whose data simply ends before the reported length is not: the reload would hit the same end again, and since libvlc
/// restarts on a fragment boundary it replays up to ~15 s of the tail each time. `source_eof` is true when the source
/// delivered all its bytes and ended cleanly, so an end with it set is the real end.
pub fn classify_end(
    pos_ms: u64,
    track_end_ms: u64,
    kind: EndKind,
    retries: u32,
    source_eof: bool,
) -> EndAction {
    // The stream failed before delivering anything.
    if pos_ms < 2000 && track_end_ms > 5000 && retries < MAX_END_RETRIES {
        return EndAction::Retry {
            from_ms: 0,
            delay: Duration::from_millis(1500),
        };
    }
    // A decoded natural end can precede the reported length by one final fragment, padding and the last poll interval.
    // Error recovery is stricter; the natural window is capped at 10 % so short tracks do not lose much to an early end.
    let window_ms = match kind {
        EndKind::Error => 3000,
        EndKind::Ended => (track_end_ms / 10).min(10_000),
    };
    let short_of_end = pos_ms > 2000 && track_end_ms > 5000 && pos_ms + window_ms < track_end_ms;
    // A clean end of the data is the real end however far it is from the reported length. An error never is.
    let data_ended = source_eof && kind == EndKind::Ended;
    if short_of_end && !data_ended && retries < MAX_END_RETRIES {
        return EndAction::Retry {
            from_ms: pos_ms,
            delay: Duration::from_millis(500),
        };
    }
    EndAction::Advance {
        short_of_reported: short_of_end && data_ended,
    }
}

// ── Track length (`track-end.js`) ───────────────────────────────────────────

/// Rounding and padding, not a real length difference.
pub const LENGTH_TOLERANCE_MS: u64 = 1000;

/// True when a measured length differs from the one in use by more than the tolerance.
pub fn length_differs(measured_ms: u64, current_ms: u64) -> bool {
    measured_ms > 0 && measured_ms.abs_diff(current_ms) > LENGTH_TOLERANCE_MS
}

/// The length to plan pre-roll against: the stream's own once it is known and differs, else the catalog's.
pub fn track_length_ms(stream_ms: Option<u64>, catalog_ms: u64) -> u64 {
    match stream_ms {
        Some(s) if catalog_ms == 0 || length_differs(s, catalog_ms) => s,
        _ => catalog_ms,
    }
}

/// Playback ran past the length in use: the audio is longer than reported, so the length grows to the position
/// instead of the track being treated as finished (or cut) at the old length.
pub fn length_after_overrun(position_ms: u64, current_ms: u64) -> u64 {
    if current_ms == 0 {
        return 0;
    }
    if position_ms > current_ms + LENGTH_TOLERANCE_MS {
        position_ms
    } else {
        current_ms
    }
}

// ── Transport ───────────────────────────────────────────────────────────────

/// Previous restarts the track after this much (MusicKit rule).
pub const PREVIOUS_RESTART_MS: u64 = 3000;

/// Previous restarts the current track instead of going back.
pub fn previous_restarts(position_ms: u64) -> bool {
    position_ms > PREVIOUS_RESTART_MS
}

// ── Scrobble and history (spec §4.7, §4.2) ──────────────────────────────────

/// Played time after which a track enters the play history.
pub const HISTORY_MIN_PLAYED: Duration = Duration::from_secs(30);

/// Played time after which a track is scrobbled, or `None` for tracks under 30 s (Last.fm rule:
/// `min(4 min, half the track)`; an unknown length waits 60 s, as the renderer did).
pub fn scrobble_after(duration_ms: u64) -> Option<Duration> {
    match duration_ms {
        0 => Some(Duration::from_secs(60)),
        d if d < 30_000 => None,
        d => Some(Duration::from_millis((d / 2).min(240_000))),
    }
}

// ── Mode choice (spec §4.4) ─────────────────────────────────────────────────

/// The session to ask for. A quality or FairPlay change applies at the next track start.
pub fn choose_mode(
    kind: ItemKind,
    meta: &TrackMeta,
    settings: &PlayerSettings,
    caps: Capabilities,
) -> OpenMode {
    if kind == ItemKind::MusicVideo {
        return OpenMode::Video {
            max_height: settings.mv_max_height,
        };
    }
    let lossless_wanted = caps.lossless_ready
        && settings.lossless_enabled
        && settings.quality != StreamingQuality::HighQuality;
    if lossless_wanted && meta.may_be_lossless() {
        OpenMode::Lossless {
            hi_res: settings.quality == StreamingQuality::HiResLossless,
        }
    } else {
        OpenMode::Aac
    }
}

/// The badge data of a session (`showQualityBadge`): spatial wins, ALAC above 16/48 is hi-res.
pub fn quality_of(session: &OpenedSession, fell_back: bool) -> Quality {
    let tier = match (session.codec, session.spatial) {
        (Codec::H264, _) => QualityTier::Video,
        (_, Some(_)) => QualityTier::Spatial,
        (Codec::Atmos, None) => QualityTier::DolbyAtmos,
        (Codec::Alac, None) if session.sample_rate > 48_000 || session.bit_depth > 16 => {
            QualityTier::HiResLossless
        }
        (Codec::Alac, None) => QualityTier::Lossless,
        (Codec::Aac, None) => QualityTier::Aac,
    };
    let nonzero = |v: u32| (v > 0).then_some(v);
    Quality {
        codec: session.codec,
        tier,
        bit_depth: nonzero(session.bit_depth),
        sample_rate: nonzero(session.sample_rate),
        bitrate: nonzero(session.bit_rate),
        channels: nonzero(session.channels),
        spatial: session.spatial,
        video_height: session.video_height,
        fell_back,
    }
}

// ── Status transitions (spec §4.3, `playstate.js`) ──────────────────────────

/// The states each state may move to. Any state may stop, fail, or go idle (queue cleared).
pub fn transitions(from: Status) -> &'static [Status] {
    use Status::*;
    match from {
        // Paused from idle: a restored or `play: false` queue waits without a session.
        Idle => &[Opening, Paused, Stopped, Failed],
        Opening => &[Opening, Loading, Paused, Idle, Stopped, Failed],
        Loading => &[Opening, Loading, Playing, Paused, Idle, Stopped, Failed],
        Playing => &[Opening, Loading, Paused, Idle, Stopped, Failed],
        Paused => &[Opening, Loading, Playing, Idle, Stopped, Failed],
        Stopped => &[Opening, Paused, Idle, Failed],
        Failed => &[Opening, Paused, Idle, Stopped],
    }
}

/// Staying in a state is always fine; otherwise the move must be an edge.
pub fn is_legal_transition(from: Status, to: Status) -> bool {
    from == to || transitions(from).contains(&to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Spatial;

    const TRACK: u64 = 212_310; // reported length of a real ALAC track whose data ends at 208948

    fn advance(a: EndAction) -> bool {
        matches!(a, EndAction::Advance { .. })
    }

    // ── vlc-end.test.mjs ──

    #[test]
    fn a_natural_end_close_to_the_reported_length_advances() {
        assert!(advance(classify_end(
            208_948,
            TRACK,
            EndKind::Ended,
            0,
            true
        )));
        assert!(advance(classify_end(
            208_948,
            TRACK,
            EndKind::Ended,
            0,
            false
        )));
    }

    #[test]
    fn a_data_end_far_short_of_the_reported_length_advances_instead_of_replaying_the_tail() {
        assert_eq!(
            classify_end(190_000, TRACK, EndKind::Ended, 0, true),
            EndAction::Advance {
                short_of_reported: true
            }
        );
    }

    #[test]
    fn an_end_short_of_the_length_without_a_clean_source_end_reloads_at_that_position() {
        assert_eq!(
            classify_end(190_000, TRACK, EndKind::Ended, 0, false),
            EndAction::Retry {
                from_ms: 190_000,
                delay: Duration::from_millis(500)
            }
        );
    }

    #[test]
    fn reloads_are_bounded() {
        assert!(advance(classify_end(
            190_000,
            TRACK,
            EndKind::Ended,
            2,
            false
        )));
    }

    #[test]
    fn a_decoder_error_is_recovered_even_when_the_source_had_reached_its_end() {
        // 5 s short is inside the 10 s natural window but outside the stricter 3 s error window.
        assert!(!advance(classify_end(
            TRACK - 5000,
            TRACK,
            EndKind::Error,
            0,
            true
        )));
        assert!(!advance(classify_end(
            190_000,
            TRACK,
            EndKind::Error,
            0,
            true
        )));
    }

    #[test]
    fn a_stream_that_never_started_is_retried_from_zero() {
        for eof in [true, false] {
            assert_eq!(
                classify_end(300, TRACK, EndKind::Ended, 0, eof),
                EndAction::Retry {
                    from_ms: 0,
                    delay: Duration::from_millis(1500)
                }
            );
        }
        assert!(advance(classify_end(300, TRACK, EndKind::Ended, 2, false)));
    }

    #[test]
    fn short_tracks_use_a_proportionally_smaller_natural_window() {
        // 20 s track: the window is 2 s, so an end 3 s short is not natural, but a clean source end still advances.
        assert!(!advance(classify_end(
            17_000,
            20_000,
            EndKind::Ended,
            0,
            false
        )));
        assert!(advance(classify_end(
            17_000,
            20_000,
            EndKind::Ended,
            0,
            true
        )));
    }

    #[test]
    fn a_track_with_no_usable_length_just_advances() {
        assert!(advance(classify_end(5000, 0, EndKind::Ended, 0, false)));
    }

    // ── track-end.test.mjs ──

    #[test]
    fn the_streams_own_length_wins_once_known() {
        assert_eq!(track_length_ms(Some(187_400), 210_000), 187_400); // shorter: plan against 187.4
        assert_eq!(track_length_ms(Some(218_200), 210_000), 218_200); // longer: nothing may cut at 210
        assert_eq!(track_length_ms(None, 205_000), 205_000);
        assert_eq!(
            track_length_ms(Some(0), 200_000),
            200_000,
            "an unusable stream length falls back"
        );
        assert_eq!(
            track_length_ms(Some(210_400), 210_000),
            210_000,
            "inside the tolerance"
        );
        assert_eq!(track_length_ms(Some(5000), 0), 5000, "no catalog length");
    }

    #[test]
    fn only_a_real_difference_replaces_the_length() {
        assert!(!length_differs(210_400, 210_000));
        assert!(!length_differs(209_100, 210_000));
        assert!(length_differs(187_000, 210_000));
        assert!(length_differs(218_000, 210_000));
        assert!(!length_differs(0, 210_000));
    }

    #[test]
    fn playing_past_the_reported_length_grows_it_instead_of_ending_the_track() {
        assert_eq!(length_after_overrun(150_000, 210_000), 210_000);
        assert_eq!(length_after_overrun(210_600, 210_000), 210_000);
        assert_eq!(length_after_overrun(213_500, 210_000), 213_500);
        assert_eq!(length_after_overrun(50_000, 0), 0);
    }

    // ── previous / scrobble ──

    #[test]
    fn previous_restarts_after_three_seconds() {
        assert!(!previous_restarts(0));
        assert!(!previous_restarts(3000));
        assert!(previous_restarts(3001));
    }

    #[test]
    fn scrobble_threshold_follows_lastfm() {
        assert_eq!(scrobble_after(29_999), None);
        assert_eq!(scrobble_after(30_000), Some(Duration::from_secs(15)));
        assert_eq!(scrobble_after(200_000), Some(Duration::from_secs(100)));
        assert_eq!(scrobble_after(600_000), Some(Duration::from_secs(240)));
        assert_eq!(scrobble_after(0), Some(Duration::from_secs(60)));
    }

    // ── mode choice ──

    #[test]
    fn mode_choice_follows_settings_capabilities_and_traits() {
        let ready = Capabilities {
            lossless_ready: true,
            signed_in: true,
        };
        let s = PlayerSettings::default();
        let any = TrackMeta::default();
        let lossy = TrackMeta {
            audio_traits: vec!["lossy-stereo".into()],
            ..TrackMeta::default()
        };
        assert_eq!(
            choose_mode(ItemKind::Song, &any, &s, ready),
            OpenMode::Lossless { hi_res: false }
        );
        assert_eq!(
            choose_mode(ItemKind::Song, &lossy, &s, ready),
            OpenMode::Aac
        );
        assert_eq!(
            choose_mode(ItemKind::Song, &any, &s, Capabilities::default()),
            OpenMode::Aac
        );
        let hq = PlayerSettings {
            quality: StreamingQuality::HighQuality,
            ..s
        };
        assert_eq!(choose_mode(ItemKind::Song, &any, &hq, ready), OpenMode::Aac);
        let off = PlayerSettings {
            lossless_enabled: false,
            ..s
        };
        assert_eq!(
            choose_mode(ItemKind::Song, &any, &off, ready),
            OpenMode::Aac
        );
        let hires = PlayerSettings {
            quality: StreamingQuality::HiResLossless,
            ..s
        };
        assert_eq!(
            choose_mode(ItemKind::Song, &any, &hires, ready),
            OpenMode::Lossless { hi_res: true }
        );
        let mv = PlayerSettings {
            mv_max_height: 720,
            ..s
        };
        assert_eq!(
            choose_mode(ItemKind::MusicVideo, &any, &mv, ready),
            OpenMode::Video { max_height: 720 }
        );
    }

    #[test]
    fn quality_badge_tiers() {
        let s = |codec, sr, bd, spatial| OpenedSession {
            codec,
            sample_rate: sr,
            bit_depth: bd,
            spatial,
            ..OpenedSession::test("s")
        };
        assert_eq!(
            quality_of(&s(Codec::Alac, 44_100, 16, None), false).tier,
            QualityTier::Lossless
        );
        assert_eq!(
            quality_of(&s(Codec::Alac, 48_000, 24, None), false).tier,
            QualityTier::HiResLossless
        );
        assert_eq!(
            quality_of(&s(Codec::Alac, 96_000, 16, None), false).tier,
            QualityTier::HiResLossless
        );
        assert_eq!(
            quality_of(&s(Codec::Aac, 44_100, 0, None), true).tier,
            QualityTier::Aac
        );
        assert_eq!(
            quality_of(
                &s(Codec::Alac, 48_000, 24, Some(Spatial::BinauralLossless)),
                false
            )
            .tier,
            QualityTier::Spatial
        );
        assert_eq!(
            quality_of(&s(Codec::Atmos, 48_000, 0, None), false).tier,
            QualityTier::DolbyAtmos
        );
        let q = quality_of(&s(Codec::Aac, 44_100, 0, None), true);
        assert_eq!(
            (q.bit_depth, q.sample_rate, q.fell_back),
            (None, Some(44_100), true)
        );
    }

    // ── playstate.test.mjs ──

    const ALL: [Status; 7] = [
        Status::Idle,
        Status::Opening,
        Status::Loading,
        Status::Playing,
        Status::Paused,
        Status::Stopped,
        Status::Failed,
    ];

    fn reach(from: Status) -> Vec<Status> {
        let mut seen = vec![from];
        let mut i = 0;
        while i < seen.len() {
            for &n in transitions(seen[i]) {
                if !seen.contains(&n) {
                    seen.push(n);
                }
            }
            i += 1;
        }
        seen
    }

    #[test]
    fn every_state_is_reachable_from_idle_and_idle_from_every_state() {
        let from_idle = reach(Status::Idle);
        for s in ALL {
            assert!(from_idle.contains(&s), "{s:?} unreachable");
            assert!(
                reach(s).contains(&Status::Idle),
                "{s:?} cannot get back to idle"
            );
        }
    }

    #[test]
    fn any_state_may_stop_or_fail() {
        for s in ALL {
            assert!(is_legal_transition(s, Status::Stopped), "{s:?}");
            assert!(is_legal_transition(s, Status::Failed), "{s:?}");
        }
    }

    #[test]
    fn the_design_edges_are_legal_and_the_shortcuts_are_not() {
        use Status::*;
        for (a, b) in [
            (Idle, Opening),
            (Opening, Loading),
            (Loading, Playing),
            (Playing, Paused),
            (Paused, Playing),
            (Playing, Opening),
        ] {
            assert!(is_legal_transition(a, b), "{a:?} → {b:?}");
        }
        for (a, b) in [
            (Idle, Playing),
            (Idle, Loading),
            (Opening, Playing),
            (Stopped, Playing),
            (Failed, Playing),
        ] {
            assert!(!is_legal_transition(a, b), "{a:?} → {b:?} must be illegal");
        }
    }
}
