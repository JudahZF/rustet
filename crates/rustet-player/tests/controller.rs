//! The controller against fake decks, a fake opener and paused time.

mod common;

use common::*;
use rustet_player::activity::{EndReason, EventType};
use rustet_player::model::*;
use rustet_player::ports::{DeckError, DeckEvent, LoadId, OpenClass, StoreKey};
use rustet_player::snapshot::{SNAPSHOT_MAX_AGE_MS, SNAPSHOT_VERSION, Snapshot, SnapshotItem};
use std::sync::Arc;
use std::time::Duration;

fn uid_of(s: &PlayerState, id: &str) -> u64 {
    s.queue
        .items
        .iter()
        .find(|i| i.id.as_str() == id)
        .unwrap()
        .uid
}

fn ids(s: &PlayerState) -> Vec<String> {
    s.queue.items.iter().map(|i| i.id.0.clone()).collect()
}

// ── Basic flow ──────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn play_now_opens_loads_and_plays() {
    let mut h = harness().await;
    h.send(play_now(songs(&["1", "2", "3"]), 1)).await;
    let s = h.until_playing("2").await;
    assert_eq!(h.opener.opens(), ["open 2 aac"]);
    assert_eq!(h.deck_log(), ["A load s1@0 paused=false vol=100"]);
    assert_eq!(s.queue.current_index, Some(1));
    let q = s.quality.unwrap();
    assert_eq!(
        (q.codec, q.tier, q.fell_back),
        (Codec::Aac, QualityTier::Aac, false)
    );
    assert!(s.can_next && s.can_previous);
    let events = h.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Track(t) if t.item.id.as_str() == "2"))
    );
    assert!(events.iter().any(|e| matches!(e, Event::Queue(_))));
    // The pre-warm scheduler got the queue window.
    let w = h.prewarm.windows.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (w.current_index, w.tracks.len(), w.tracks[0].queue_distance),
        (Some(1), 3, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn position_events_run_at_one_hertz_while_playing() {
    let mut h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    h.drain_events();
    wait(5.05).await;
    let positions: Vec<Position> = h
        .drain_events()
        .into_iter()
        .filter_map(|e| {
            if let Event::Position(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    assert!(
        (4..=6).contains(&positions.len()),
        "{} position events",
        positions.len()
    );
    assert!(
        positions
            .windows(2)
            .all(|w| w[1].position_ms > w[0].position_ms)
    );
    assert!(h.state().position_ms >= 4000);
}

// ── Queue commands through the controller ───────────────────────────────────

#[tokio::test(start_paused = true)]
async fn insert_remove_move_jump_and_clear() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2", "3"]), 0)).await;
    h.until_playing("1").await;
    h.send(Command::Insert(Insert {
        items: songs(&["n"]),
        position: InsertPosition::Next,
    }))
    .await;
    h.send(Command::Insert(Insert {
        items: songs(&["l"]),
        position: InsertPosition::Later,
    }))
    .await;
    assert_eq!(ids(&h.state()), ["1", "n", "2", "3", "l"]);
    let s = h.state();
    h.send(Command::Move(Move {
        uid: uid_of(&s, "l"),
        to_index: 1,
    }))
    .await;
    assert_eq!(ids(&h.state()), ["1", "l", "n", "2", "3"]);
    // Removing the current item plays the next one.
    let s = h.state();
    h.send(Command::Remove(Remove {
        uids: vec![uid_of(&s, "1")],
    }))
    .await;
    h.until_playing("l").await;
    // Jump plays the target.
    let s = h.state();
    h.send(Command::Jump(Jump {
        uid: uid_of(&s, "3"),
    }))
    .await;
    let s = h.until_playing("3").await;
    assert_eq!(s.queue.current_index, Some(3));
    h.send(Command::Jump(Jump {
        uid: uid_of(&s, "n"),
    }))
    .await;
    h.until_playing("n").await;
    h.send(Command::ClearUpcoming).await;
    assert_eq!(ids(&h.state()), ["l", "n"]);
    assert_eq!(
        h.player.send(Command::Jump(Jump { uid: 999 })).await,
        Err(CommandError::UnknownUid(999))
    );
    // Removing everything leaves the player idle.
    let s = h.state();
    h.send(Command::Remove(Remove {
        uids: s.queue.items.iter().map(|i| i.uid).collect(),
    }))
    .await;
    let s = h.until("idle", |s| s.status == Status::Idle).await;
    assert!(s.track.is_none());
}

#[tokio::test(start_paused = true)]
async fn play_next_on_an_empty_player_starts_playing() {
    let h = harness().await;
    h.send(Command::Insert(Insert {
        items: songs(&["x"]),
        position: InsertPosition::Next,
    }))
    .await;
    h.until_playing("x").await;
}

#[tokio::test(start_paused = true)]
async fn previous_restarts_after_three_seconds_and_goes_back_before() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 1)).await;
    h.until_playing("2").await;
    wait(5.0).await;
    let generation = h.state().generation;
    h.clear_log();
    h.send(Command::Previous).await;
    assert_eq!(
        h.deck_log(),
        ["A seek 0"],
        "restart by seeking, not reopening"
    );
    assert_eq!(h.state().generation, generation);
    assert_eq!(h.state().track.unwrap().id.as_str(), "2");
    wait(2.0).await;
    h.send(Command::Previous).await;
    let s = h.until_playing("1").await;
    assert!(s.generation > generation);
    // At the first item, previous restarts.
    wait(1.0).await;
    h.clear_log();
    h.send(Command::Previous).await;
    assert_eq!(h.deck_log(), ["B seek 0"]);
}

#[tokio::test(start_paused = true)]
async fn repeat_one_replays_on_natural_end_only() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 20_000;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.send(Command::Mode(Mode {
        repeat: Some(Repeat::One),
        ..Mode::default()
    }))
    .await;
    let first = h.until_playing("1").await;
    let replay = h
        .until("replayed", |s| {
            s.generation > first.generation && s.status == Status::Playing
        })
        .await;
    assert_eq!(
        replay.track.unwrap().uid,
        first.track.unwrap().uid,
        "the same item again"
    );
    h.send(Command::Next).await;
    h.until_playing("2").await;
}

#[tokio::test(start_paused = true)]
async fn shuffle_on_and_off_keeps_the_current_item() {
    let h = harness().await;
    let list: Vec<String> = (0..12).map(|i| i.to_string()).collect();
    let list: Vec<&str> = list.iter().map(String::as_str).collect();
    h.send(play_now(songs(&list), 4)).await;
    h.until_playing("4").await;
    let generation = h.state().generation;
    h.send(Command::Mode(Mode {
        shuffle: Some(true),
        ..Mode::default()
    }))
    .await;
    let s = h.state();
    assert!(s.shuffle);
    assert_eq!(s.track.as_ref().unwrap().id.as_str(), "4");
    assert_eq!(
        &ids(&s)[..5],
        &list[..5].iter().map(|s| s.to_string()).collect::<Vec<_>>()[..]
    );
    assert_eq!(
        s.generation, generation,
        "shuffling does not restart the track"
    );
    h.send(Command::Mode(Mode {
        shuffle: Some(false),
        ..Mode::default()
    }))
    .await;
    let s = h.state();
    assert_eq!(ids(&s), list);
    assert_eq!(s.track.unwrap().id.as_str(), "4");
}

#[tokio::test(start_paused = true)]
async fn autoplay_fetches_at_two_remaining_and_caps_at_ten() {
    let h = harness().await;
    h.catalog
        .autoplay
        .lock()
        .unwrap()
        .push_back((0..12).map(|i| song(&format!("a{i}"))).collect());
    h.send(play_now(songs(&["1", "2", "3", "4"]), 0)).await;
    h.until_playing("1").await;
    assert!(
        h.catalog.log.lock().unwrap().is_empty(),
        "three upcoming: no fetch"
    );
    h.send(Command::Next).await;
    h.until("autoplay items", |s| s.queue.items.len() > 4).await;
    assert_eq!(*h.catalog.log.lock().unwrap(), ["autoplay 1,2,3,4 10"]);
    let s = h.state();
    let auto: Vec<_> = s
        .queue
        .items
        .iter()
        .filter(|i| i.source == ItemSource::Autoplay)
        .collect();
    assert_eq!(auto.len(), 10);
    // Insert later still goes before the autoplay items.
    h.send(Command::Insert(Insert {
        items: songs(&["l"]),
        position: InsertPosition::Later,
    }))
    .await;
    assert_eq!(ids(&h.state())[4], "l");
}

#[tokio::test(start_paused = true)]
async fn autoplay_needs_a_sign_in_and_the_setting() {
    let h = harness().await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: false,
        signed_in: false,
    }))
    .await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    h.send(Command::Mode(Mode {
        autoplay: Some(false),
        ..Mode::default()
    }))
    .await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: false,
        signed_in: true,
    }))
    .await;
    h.send(Command::Next).await;
    h.until_playing("2").await;
    assert!(h.catalog.log.lock().unwrap().is_empty());
    // Turning autoplay on with the queue low fetches.
    h.send(Command::Mode(Mode {
        autoplay: Some(true),
        ..Mode::default()
    }))
    .await;
    wait(0.1).await;
    assert_eq!(*h.catalog.log.lock().unwrap(), ["autoplay 1,2 10"]);
}

#[tokio::test(start_paused = true)]
async fn a_station_starts_from_next_tracks_and_refills() {
    let h = harness().await;
    h.catalog
        .station
        .lock()
        .unwrap()
        .push_back(songs(&["s1", "s2", "s3"]));
    h.catalog
        .station
        .lock()
        .unwrap()
        .push_back(songs(&["s4", "s5"]));
    let station = Container {
        kind: ContainerKind::Station,
        id: "ra.123".into(),
        name: None,
    };
    h.send(Command::SetQueue(SetQueue {
        items: Vec::new(),
        start_index: 0,
        context: Some(station),
        shuffle: None,
        start_ms: None,
        play: true,
    }))
    .await;
    h.until_playing("s1").await;
    // Two upcoming at the start: the next batch is fetched at once.
    h.until("refill", |s| s.queue.items.len() == 5).await;
    assert_eq!(
        *h.catalog.log.lock().unwrap(),
        ["next-tracks ra.123 10", "next-tracks ra.123 10"]
    );
    assert!(
        h.state()
            .queue
            .items
            .iter()
            .all(|i| i.source == ItemSource::Station)
    );
}

#[tokio::test(start_paused = true)]
async fn live_stations_are_refused_for_now() {
    let h = harness().await;
    let live = Container {
        kind: ContainerKind::LiveStation,
        id: "ra.978194965".into(),
        name: None,
    };
    let r = h
        .player
        .send(Command::SetQueue(SetQueue {
            items: Vec::new(),
            start_index: 0,
            context: Some(live),
            shuffle: None,
            start_ms: None,
            play: true,
        }))
        .await;
    assert_eq!(r, Err(CommandError::Unsupported("live radio")));
}

// ── History ─────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn history_records_tracks_played_thirty_seconds_or_to_the_end() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2", "3", "4"]), 0)).await;
    h.until_playing("1").await;
    wait(29.0).await;
    h.send(Command::Next).await;
    h.until_playing("2").await;
    assert!(h.player.history().is_empty(), "29 s is not enough");
    // Paused time does not count.
    h.send(Command::Pause).await;
    wait(60.0).await;
    h.send(Command::Play).await;
    wait(10.0).await;
    h.send(Command::Next).await;
    h.until_playing("3").await;
    assert!(h.player.history().is_empty());
    wait(31.0).await;
    h.send(Command::Next).await;
    h.until_playing("4").await;
    let hist = h.player.history();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].id.as_str(), "3");
    assert_eq!(h.store.get(StoreKey::History).unwrap()[0]["id"], "3");
    h.send(Command::ClearHistory).await;
    assert!(h.player.history().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_short_track_played_to_the_end_enters_history() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 12_000;
    let mut short = songs(&["1", "2"]);
    short[0].meta.duration_ms = 12_000;
    h.send(play_now(short, 0)).await;
    h.until_playing("2").await;
    assert_eq!(h.player.history()[0].id.as_str(), "1");
}

// ── Restore ─────────────────────────────────────────────────────────────────

fn snapshot(saved_at: u64) -> Vec<u8> {
    Snapshot {
        v: SNAPSHOT_VERSION,
        items: songs(&["1", "2", "3"])
            .into_iter()
            .map(|item| SnapshotItem {
                item,
                source: ItemSource::Context,
            })
            .collect(),
        current: Some(1),
        original: None,
        repeat: Repeat::All,
        shuffle: false,
        autoplay: true,
        context: None,
        position_ms: 61_000,
        saved_at,
    }
    .to_json()
}

#[tokio::test(start_paused = true)]
async fn restores_the_queue_paused_and_resumes_at_the_position() {
    let store = Arc::new(MemStore::default());
    store
        .files
        .lock()
        .unwrap()
        .insert(StoreKey::Snapshot, snapshot(T0 - 60_000));
    let h = harness_with(store).await;
    let s = h.until("restored", |s| s.restored).await;
    assert_eq!(s.status, Status::Paused);
    assert_eq!(s.track.as_ref().unwrap().id.as_str(), "2");
    assert_eq!((s.position_ms, s.repeat), (61_000, Repeat::All));
    assert!(h.opener.opens().is_empty(), "no session until Play");
    h.send(Command::Play).await;
    let s = h.until_playing("2").await;
    assert!(!s.restored);
    assert_eq!(h.deck_log(), ["A load s1@61000 paused=false vol=100"]);
}

// The queue is restored before FairPlay is ready. A real run played the restored track in AAC
// with lossless available.
#[tokio::test(start_paused = true)]
async fn a_restored_track_opens_lossless_once_fairplay_is_ready() {
    let store = Arc::new(MemStore::default());
    store
        .files
        .lock()
        .unwrap()
        .insert(StoreKey::Snapshot, snapshot(T0 - 60_000));
    let h = harness_with(store).await;
    h.until("restored", |s| s.restored).await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    h.send(Command::Play).await;
    h.until_playing("2").await;
    assert_eq!(h.opener.opens()[0], "open 2 lossless");
}

#[tokio::test(start_paused = true)]
async fn an_old_snapshot_is_ignored_and_deleted() {
    let store = Arc::new(MemStore::default());
    store
        .files
        .lock()
        .unwrap()
        .insert(StoreKey::Snapshot, snapshot(T0 - SNAPSHOT_MAX_AGE_MS - 1));
    let h = harness_with(store.clone()).await;
    let s = h.state();
    assert_eq!(s.status, Status::Idle);
    assert!(!s.restored && s.queue.items.is_empty());
    assert!(store.get(StoreKey::Snapshot).is_none());
}

#[tokio::test(start_paused = true)]
async fn the_snapshot_follows_the_queue_and_the_position() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    wait(11.0).await;
    let snap = h.store.get(StoreKey::Snapshot).unwrap();
    assert_eq!(snap["v"], 3);
    assert_eq!(snap["current"], 0);
    assert!(
        snap["positionMs"].as_u64().unwrap() >= 5000,
        "saved every 5 s while playing"
    );
    h.send(Command::Settings(SettingsPatch {
        resume_on_launch: Some(false),
        ..SettingsPatch::default()
    }))
    .await;
    assert!(h.store.get(StoreKey::Snapshot).is_none());
}

// ── Generations ─────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn work_of_a_replaced_track_is_dropped() {
    let h = harness().await;
    *h.opener.delay.lock().unwrap() = Duration::from_secs(1);
    h.send(play_now(songs(&["1", "2", "3"]), 0)).await;
    h.send(Command::Next).await;
    h.send(Command::Next).await;
    h.until_playing("3").await;
    wait(2.0).await;
    // Only the last open reached a deck; the others were cancelled before they finished.
    assert_eq!(
        h.deck_log().iter().filter(|l| l.contains(" load ")).count(),
        1
    );
    assert_eq!(h.state().track.unwrap().id.as_str(), "3");
    assert_eq!(h.state().generation, 3);
    // A seek for an old generation is refused.
    let r = h
        .player
        .send(Command::Seek(Seek {
            position_ms: 1000,
            expect_generation: Some(1),
        }))
        .await;
    assert_eq!(r, Err(CommandError::StaleGeneration { current: 3 }));
    h.send(Command::Seek(Seek {
        position_ms: 1000,
        expect_generation: Some(3),
    }))
    .await;
}

#[tokio::test(start_paused = true)]
async fn deck_events_of_a_replaced_load_are_ignored() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    let old = h.decks[0].current_load().unwrap();
    h.send(Command::Next).await;
    h.until_playing("2").await;
    h.decks[0].send_raw(old, DeckEvent::Ended { source_eof: true });
    h.decks[0].send_raw(LoadId(old.0), DeckEvent::Error);
    wait(1.0).await;
    let s = h.state();
    assert_eq!(
        (s.track.unwrap().id.as_str(), s.status),
        ("2", Status::Playing)
    );
}

// ── Mode choice and fallback ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn lossless_falls_back_to_aac_for_the_listed_classes() {
    for class in [
        OpenClass::NotFound,
        OpenClass::Unavailable,
        OpenClass::LicenceRefused,
    ] {
        let h = harness().await;
        h.send(Command::Capabilities(Capabilities {
            lossless_ready: true,
            signed_in: true,
        }))
        .await;
        h.opener.fail("1", &[class]);
        h.send(play_now(songs(&["1"]), 0)).await;
        let s = h.until_playing("1").await;
        assert_eq!(
            h.opener.opens(),
            ["open 1 lossless", "open 1 aac"],
            "{class:?}"
        );
        let q = s.quality.unwrap();
        assert_eq!(
            (q.codec, q.tier, q.fell_back),
            (Codec::Aac, QualityTier::Aac, true)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_transport_failure_does_not_fall_back() {
    let h = harness().await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    h.opener.fail("1", &[OpenClass::Transport]);
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("2").await;
    assert_eq!(h.opener.opens(), ["open 1 lossless", "open 2 lossless"]);
}

#[tokio::test(start_paused = true)]
async fn quality_settings_apply_at_the_next_track() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2", "3"]), 0)).await;
    h.until_playing("1").await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    assert_eq!(
        h.opener.opens(),
        ["open 1 aac"],
        "no reload of the playing track"
    );
    h.send(Command::Next).await;
    let s = h.until_playing("2").await;
    assert_eq!(s.quality.unwrap().tier, QualityTier::Lossless);
    h.send(Command::Settings(SettingsPatch {
        quality: Some(StreamingQuality::HiResLossless),
        ..SettingsPatch::default()
    }))
    .await;
    h.send(Command::Next).await;
    let s = h.until_playing("3").await;
    assert_eq!(s.quality.unwrap().tier, QualityTier::HiResLossless);
    assert!(h.opener.opens().contains(&"open 3 hires".to_string()));
    let bad = h
        .player
        .send(Command::Settings(SettingsPatch {
            mv_max_height: Some(999),
            ..SettingsPatch::default()
        }))
        .await;
    assert_eq!(bad, Err(CommandError::BadHeight(999)));
}

#[tokio::test(start_paused = true)]
async fn music_videos_open_as_video() {
    let h = harness().await;
    h.send(Command::Settings(SettingsPatch {
        mv_max_height: Some(1080),
        ..SettingsPatch::default()
    }))
    .await;
    let mut mv = song("mv");
    mv.kind = ItemKind::MusicVideo;
    h.send(play_now(vec![mv], 0)).await;
    let s = h.until_playing("mv").await;
    assert_eq!(h.opener.opens(), ["open mv video1080"]);
    assert_eq!(s.quality.unwrap().tier, QualityTier::Video);
}

fn mv(id: &str, duration_ms: u64) -> QueueItemIn {
    let mut item = song(id);
    item.kind = ItemKind::MusicVideo;
    item.meta.duration_ms = duration_ms;
    item
}

fn ring_of(s: &PlayerState) -> Option<String> {
    s.video.as_ref().map(|v| v.ring.clone())
}

// A music video loads with video on, the state names its ring once the deck reports it, and the
// ring goes from the state when the queue moves on to a song, or back from it.
#[tokio::test(start_paused = true)]
async fn a_music_video_publishes_its_ring_while_it_is_the_track() {
    let h = harness().await;
    h.send(play_now(vec![song("a"), mv("v", 200_000), song("c")], 0))
        .await;
    let s = h.until_playing("a").await;
    assert_eq!(s.video, None, "a song has no video");
    h.send(Command::Next).await;
    let s = h.until_playing("v").await;
    let video = s.video.clone().expect("the ring is in the state");
    assert_eq!(
        (video.ring.as_str(), video.width, video.height),
        ("/run/rustet/s2.ring", 1280, 720)
    );
    assert!(
        h.deck_log()
            .iter()
            .any(|l| l.ends_with("load s2@0 paused=false vol=100 video")),
        "{:?}",
        h.deck_log()
    );
    assert_eq!(s.quality.unwrap().tier, QualityTier::Video);
    // Pause keeps the ring (its last frame stays on screen).
    h.send(Command::Pause).await;
    assert_eq!(
        h.until("paused", |s| s.status == Status::Paused)
            .await
            .video,
        Some(video.clone())
    );
    h.send(Command::Play).await;
    h.send(Command::Next).await;
    assert_eq!(ring_of(&h.until_playing("c").await), None);
    h.send(Command::Previous).await;
    h.send(Command::Previous).await;
    let back = h.until_playing("v").await;
    assert!(
        back.video
            .as_ref()
            .is_some_and(|v| v.generation > video.generation),
        "a new load reports a new ring"
    );
}

// A seek outside the downloaded part reloads: the old ring is dropped from the state until the
// new load reports its own.
#[tokio::test(start_paused = true)]
async fn a_reload_of_a_music_video_replaces_its_ring() {
    let h = harness().await;
    h.send(play_now(vec![mv("v", 200_000)], 0)).await;
    let before = h.until_playing("v").await.video.unwrap();
    *h.decks[0].seek_error.lock().unwrap() = Some(DeckError::NeedsReload);
    h.send(Command::Seek(Seek {
        position_ms: 90_000,
        expect_generation: None,
    }))
    .await;
    let after = h
        .until("a new ring", |s| {
            s.video
                .as_ref()
                .is_some_and(|v| v.generation != before.generation)
        })
        .await;
    assert!(
        h.deck_log()
            .iter()
            .any(|l| l.contains("load s1@90000 paused=false vol=100 video")),
        "{:?}",
        h.deck_log()
    );
    assert_eq!(
        after.video.unwrap().ring,
        before.ring,
        "same session, same name in the fake"
    );
}

// Audio only: no frames are asked for, the state says so and keeps no ring, the choice is saved,
// and switching it while a music video plays reloads that video at its position.
#[tokio::test(start_paused = true)]
async fn music_videos_can_play_as_audio_only() {
    let h = harness().await;
    h.player.set_mv_audio_only(true).await.unwrap();
    h.send(play_now(vec![mv("v", 200_000)], 0)).await;
    let s = h.until_playing("v").await;
    assert!(s.mv_audio_only);
    assert_eq!(s.video, None);
    assert!(
        h.deck_log()
            .iter()
            .any(|l| l.ends_with("load s1@0 paused=false vol=100")),
        "audio only: {:?}",
        h.deck_log()
    );
    assert_eq!(h.store.get(StoreKey::Prefs).unwrap()["mvAudioOnly"], true);

    wait(30.0).await;
    h.clear_log();
    h.player.set_mv_audio_only(false).await.unwrap();
    let s = h.until("the video back", |s| s.video.is_some()).await;
    assert!(!s.mv_audio_only);
    let log = h.deck_log();
    let reload = log
        .iter()
        .find(|l| l.contains(" load s1@"))
        .unwrap_or_else(|| panic!("no reload: {log:?}"));
    let at: u64 = reload
        .split('@')
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (29_000..=31_000).contains(&at),
        "the video comes back at the position: {reload}"
    );
    assert!(reload.ends_with(" video"));
    assert_eq!(
        h.state().generation,
        s.generation,
        "the same track, not a new one"
    );
}

// Gapless into a music video: the pre-roll loads it with video, paused; its ring is in the state
// as soon as it becomes the track. Out of a music video into a song, the ring goes.
#[tokio::test(start_paused = true)]
async fn hand_overs_into_and_out_of_a_music_video() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 20_000;
    let mut a = song("a");
    a.meta.duration_ms = 20_000;
    let mut c = song("c");
    c.meta.duration_ms = 20_000;
    h.send(play_now(vec![a, mv("v", 20_000), c], 0)).await;
    h.until_playing("a").await;
    h.clear_log();
    let s = h.until_playing("v").await;
    let log = h.deck_log();
    assert!(
        log.iter()
            .any(|l| l == "B load s2@0 paused=true vol=0 video"),
        "pre-rolled with video: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|l| l.starts_with("B load") && !l.contains("paused=true")),
        "taken over, not reloaded: {log:?}"
    );
    assert_eq!(
        ring_of(&s).as_deref(),
        Some("/run/rustet/s2.ring"),
        "the pre-roll's ring"
    );
    let s = h.until_playing("c").await;
    assert_eq!(s.video, None);
}

// libvlc says playing seconds before a downloading music video shows anything. Meanwhile the
// track stands at its start (the UI interpolates from `positionAt`), and the first position the
// deck reports is published at once, not at the next 1 Hz event.
#[tokio::test(start_paused = true)]
async fn the_position_waits_for_the_deck_to_start() {
    let mut h = harness().await;
    for d in &h.decks {
        *d.buffering.lock().unwrap() = Duration::from_millis(2500);
    }
    h.send(play_now(vec![mv("v", 200_000)], 0)).await;
    h.until_playing("v").await;
    h.drain_events();
    wait(2.0).await;
    let s = h.state();
    let now = T0 + 2000;
    assert_eq!(s.position_ms, 0);
    assert!(
        now - s.position_at < 1100,
        "positionAt follows the clock while the deck buffers: {} ms old",
        now - s.position_at
    );
    wait(0.7).await;
    let positions: Vec<Position> = h
        .drain_events()
        .into_iter()
        .filter_map(|e| {
            if let Event::Position(p) = e {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    let first = positions
        .iter()
        .find(|p| p.position_ms > 0)
        .expect("a position once the deck reports one");
    assert!(
        first.position_at <= T0 + 2650,
        "published at the deck's first position, not a second later"
    );
}

// ── Failures ────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn failed_opens_move_on_after_a_second_and_give_up_after_three() {
    let mut h = harness().await;
    for id in ["1", "2", "3"] {
        h.opener.fail(id, &[OpenClass::Transport]);
    }
    h.send(play_now(songs(&["1", "2", "3", "4"]), 0)).await;
    let s = h.until("failed", |s| s.status == Status::Failed).await;
    let err = s.error.unwrap();
    assert_eq!(err.code, ErrorCode::TooManyFailures);
    assert_eq!(
        err.message,
        "Couldn't play 3 tracks in a row. Check your connection."
    );
    assert_eq!(h.opener.opens(), ["open 1 aac", "open 2 aac", "open 3 aac"]);
    let errors = h
        .drain_events()
        .into_iter()
        .filter(|e| matches!(e, Event::Error(_)))
        .count();
    assert_eq!(errors, 3);
    // Play starts over: the counter is reset.
    h.send(Command::Play).await;
    h.until_playing("3").await;
}

#[tokio::test(start_paused = true)]
async fn a_success_resets_the_failure_count() {
    let h = harness().await;
    for id in ["1", "2", "4", "5"] {
        h.opener.fail(id, &[OpenClass::Transport]);
    }
    h.send(play_now(songs(&["1", "2", "3", "4", "5", "6"]), 0))
        .await;
    h.until_playing("3").await;
    h.send(Command::Next).await;
    h.until_playing("6").await;
}

#[tokio::test(start_paused = true)]
async fn an_auth_failure_raises_auth_expired() {
    let mut h = harness().await;
    h.opener.fail("1", &[OpenClass::Authentication]);
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("2").await;
    assert!(
        h.drain_events()
            .iter()
            .any(|e| matches!(e, Event::AuthExpired))
    );
}

// ── End of track ────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn a_stream_that_stalls_at_the_start_reloads_from_zero() {
    let h = harness().await;
    *h.decks[0].eof.lock().unwrap() = false;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    wait(0.5).await;
    h.decks[0].end(DeckEvent::Ended { source_eof: false });
    wait(1.0).await;
    assert_eq!(
        h.deck_log().iter().filter(|l| l.contains("load")).count(),
        1,
        "the reload waits 1.5 s"
    );
    wait(1.0).await;
    h.until_playing("1").await;
    assert_eq!(
        h.deck_log(),
        [
            "A load s1@0 paused=false vol=100",
            "A load s1@0 paused=false vol=100"
        ]
    );
}

// libvlc's time stops when its demuxer reads the last fragment, up to 15 s before the audio
// ends. The position runs on the clock from there. A late value from libvlc is behind the
// audio and does not pull the position back, so the track ends at its length.
#[tokio::test(start_paused = true)]
async fn the_last_fragment_runs_on_the_clock() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 40_000;
    let mut items = songs(&["1", "2"]);
    items[0].meta.duration_ms = 40_000;
    h.send(play_now(items, 0)).await;
    h.until_playing("1").await;
    wait(25.0).await;
    h.decks[0].end_source_at(25_000);
    wait(10.0).await;
    let on_clock = h.state().position_ms;
    assert!(
        (34_000..=36_000).contains(&on_clock),
        "the position runs on the clock: {on_clock}"
    );
    h.decks[0].end_source_at(25_200);
    wait(1.5).await;
    let after_late_value = h.state().position_ms;
    assert!(
        after_late_value >= on_clock,
        "a late deck time does not pull the position back: {after_late_value}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_end_short_of_the_length_without_eof_reloads_at_the_position_twice_then_advances() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    for _ in 0..2 {
        h.decks[0].set_position(100_000);
        h.decks[0].end(DeckEvent::Ended { source_eof: false });
        wait(0.6).await;
        h.until_playing("1").await;
        assert!(
            h.deck_log().last().unwrap().starts_with("A load s1@100"),
            "{:?}",
            h.deck_log()
        );
    }
    h.decks[0].set_position(100_000);
    h.decks[0].end(DeckEvent::Ended { source_eof: false });
    h.until_playing("2").await;
}

#[tokio::test(start_paused = true)]
async fn a_clean_data_end_short_of_the_length_advances() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    h.decks[0].set_position(150_000);
    h.decks[0].end(DeckEvent::Ended { source_eof: true });
    h.until_playing("2").await;
    assert_eq!(
        h.deck_log()
            .iter()
            .filter(|l| l.contains("load s1"))
            .count(),
        1,
        "no replay of the tail"
    );
}

#[tokio::test(start_paused = true)]
async fn a_decoder_error_near_the_end_is_retried() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    h.decks[0].set_position(195_000);
    h.decks[0].end(DeckEvent::Error);
    wait(0.6).await;
    h.until_playing("1").await;
    assert!(h.deck_log().last().unwrap().starts_with("A load s1@195"));
}

#[tokio::test(start_paused = true)]
async fn an_unasked_stop_while_playing_counts_as_an_end() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    h.decks[0].set_position(199_500);
    h.decks[0].end(DeckEvent::Stopped);
    h.until_playing("2").await;
}

#[tokio::test(start_paused = true)]
async fn the_queue_end_stops_and_play_restarts_the_last_track() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 20_000;
    h.send(Command::Mode(Mode {
        autoplay: Some(false),
        ..Mode::default()
    }))
    .await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    let s = h.until("stopped", |s| s.status == Status::Stopped).await;
    assert_eq!((s.position_ms, s.track.unwrap().id.as_str()), (0, "1"));
    h.send(Command::Play).await;
    h.until_playing("1").await;
}

#[tokio::test(start_paused = true)]
async fn a_longer_stream_grows_the_length() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 0; // libvlc does not know the length
    let mut s = songs(&["1"]);
    s[0].meta.duration_ms = 10_000;
    h.send(play_now(s, 0)).await;
    h.until_playing("1").await;
    wait(14.0).await;
    assert!(
        h.state().duration_ms > 11_000,
        "the length follows the position: {}",
        h.state().duration_ms
    );
}

// ── Pre-warm and gapless ────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn aac_prewarm_then_gapless_hand_over() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 20_000;
    let mut list = songs(&["1", "2", "3"]);
    for s in &mut list {
        s.meta.duration_ms = 20_000;
    }
    h.send(play_now(list, 0)).await;
    let first = h.until_playing("1").await;
    wait(11.0).await;
    assert_eq!(
        h.opener.opens(),
        ["open 1 aac"],
        "AAC opens the next track 8 s before the end"
    );
    wait(1.5).await;
    assert_eq!(h.opener.opens(), ["open 1 aac", "open 2 aac"]);
    h.clear_log();
    let s = h.until_playing("2").await;
    assert_eq!(s.generation, first.generation + 1);
    let log = h.deck_log();
    let pos = |needle: &str| {
        log.iter()
            .position(|l| l.starts_with(needle))
            .unwrap_or_else(|| panic!("{needle} in {log:?}"))
    };
    assert!(
        log[pos("B load")].ends_with("paused=true vol=0"),
        "pre-roll loads paused at volume 0: {log:?}"
    );
    assert!(pos("B load") < pos("B volume 100") && pos("B volume 100") < pos("B play"));
    assert!(
        pos("B play") < pos("A stop"),
        "the next deck starts before the ended one stops"
    );
    assert_eq!(h.opener.opens().len(), 2, "the pre-warmed session was used");
}

#[tokio::test(start_paused = true)]
async fn lossless_prewarm_starts_with_the_track_and_precaches() {
    let h = harness().await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    wait(0.2).await;
    let log = h.opener.log.lock().unwrap().clone();
    assert_eq!(log, ["open 1 lossless", "open 2 lossless", "precache s2"]);
    // Next uses the pre-warmed session.
    h.send(Command::Next).await;
    h.until_playing("2").await;
    assert_eq!(h.opener.opens().len(), 2);
    assert!(h.deck_log().last().unwrap().contains("load s2@0"));
}

#[tokio::test(start_paused = true)]
async fn a_queue_change_drops_a_stale_prewarm() {
    let h = harness().await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    wait(0.2).await;
    h.send(Command::Insert(Insert {
        items: songs(&["n"]),
        position: InsertPosition::Next,
    }))
    .await;
    wait(0.2).await;
    assert!(
        h.opener
            .log
            .lock()
            .unwrap()
            .contains(&"release s2".to_string())
    );
    h.send(Command::Next).await;
    h.until_playing("n").await;
}

#[tokio::test(start_paused = true)]
async fn prewarm_retries_transient_failures_three_times_five_seconds_apart() {
    let h = harness().await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    h.opener.fail("2", &[OpenClass::Transport; 4]);
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    wait(4.0).await;
    assert_eq!(
        h.opener
            .opens()
            .iter()
            .filter(|o| o.starts_with("open 2"))
            .count(),
        1
    );
    wait(20.0).await;
    assert_eq!(
        h.opener
            .opens()
            .iter()
            .filter(|o| o.starts_with("open 2"))
            .count(),
        4,
        "1 + 3 retries"
    );
    wait(20.0).await;
    assert_eq!(
        h.opener
            .opens()
            .iter()
            .filter(|o| o.starts_with("open 2"))
            .count(),
        4
    );
}

#[tokio::test(start_paused = true)]
async fn a_permanent_prewarm_failure_is_not_retried() {
    let h = harness().await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    h.opener.fail("2", &[OpenClass::NotFound]);
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    wait(30.0).await;
    assert_eq!(
        h.opener
            .opens()
            .iter()
            .filter(|o| o.starts_with("open 2"))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn hover_prewarms_and_is_used_by_the_next_play() {
    let h = harness().await;
    h.send(Command::Hover(Hover {
        id: CatalogId("9".into()),
        kind: ItemKind::Song,
    }))
    .await;
    wait(0.1).await;
    h.send(play_now(songs(&["9"]), 0)).await;
    h.until_playing("9").await;
    assert_eq!(h.opener.opens(), ["open 9 aac"]);
}

// ── Handoff (ported from handoff.test.mjs) ──────────────────────────────────

#[tokio::test(start_paused = true)]
async fn the_old_deck_is_stopped_before_the_next_track_loads() {
    let h = harness().await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: true,
        signed_in: true,
    }))
    .await;
    let mut mv = song("mv");
    mv.kind = ItemKind::MusicVideo;
    let mut lossy = song("aac");
    lossy.meta.audio_traits = vec!["lossy-stereo".into()];
    h.send(play_now(vec![song("l1"), lossy, mv, song("l2")], 0))
        .await;
    h.until_playing("l1").await;
    for id in ["aac", "mv", "l2"] {
        h.clear_log();
        h.send(Command::Next).await;
        h.until_playing(id).await;
        let log = h.deck_log();
        let stops = log.iter().filter(|l| l.ends_with(" stop")).count();
        assert_eq!(stops, 1, "exactly one stop per track change: {log:?}");
        let stop = log.iter().position(|l| l.ends_with(" stop")).unwrap();
        let load = log.iter().position(|l| l.contains(" load ")).unwrap();
        assert!(
            stop < load,
            "the old track is silent before the next one starts: {log:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_hung_stop_does_not_hold_up_the_next_track() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    *h.decks[0].stop_hangs.lock().unwrap() = true;
    h.send(Command::Next).await;
    h.until_playing("2").await;
}

// ── Transport (ported from transport.test.mjs) ──────────────────────────────

#[tokio::test(start_paused = true)]
async fn transport_commands_reach_the_deck_in_order_and_repeats_are_dropped() {
    let h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    h.clear_log();
    for c in [
        Command::Pause,
        Command::Pause,
        Command::Play,
        Command::Play,
        Command::Toggle,
        Command::Toggle,
    ] {
        h.send(c).await;
    }
    assert_eq!(h.deck_log(), ["A pause", "A play", "A pause", "A play"]);
    assert_eq!(h.state().status, Status::Playing);
}

#[tokio::test(start_paused = true)]
async fn a_pause_while_opening_loads_the_track_paused() {
    let h = harness().await;
    *h.opener.delay.lock().unwrap() = Duration::from_secs(1);
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    h.send(Command::Next).await;
    h.send(Command::Pause).await;
    let s = h.until("paused", |s| s.status == Status::Paused).await;
    assert_eq!(s.track.unwrap().id.as_str(), "2");
    assert!(
        h.deck_log()
            .last()
            .unwrap()
            .ends_with("paused=true vol=100")
    );
    assert!(
        !h.deck_log().iter().any(|l| l == "A pause"),
        "the old track never gets the new track's pause"
    );
}

#[tokio::test(start_paused = true)]
async fn rapid_skips_run_in_order() {
    let h = harness().await;
    h.send(play_now(songs(&["1", "2", "3", "4", "5"]), 0)).await;
    for _ in 0..3 {
        h.send(Command::Next).await;
    }
    h.send(Command::Previous).await;
    h.until_playing("3").await;
    assert_eq!(h.state().generation, 5);
}

// ── Seek, volume, keep-alive ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn seek_outside_the_buffer_reloads_at_the_target() {
    let h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    *h.decks[0].seek_error.lock().unwrap() = Some(DeckError::NeedsReload);
    h.send(Command::Seek(Seek {
        position_ms: 150_000,
        expect_generation: None,
    }))
    .await;
    assert_eq!(
        h.deck_log().last().unwrap(),
        "A load s1@150000 paused=false vol=100"
    );
}

#[tokio::test(start_paused = true)]
async fn a_rewind_after_a_seek_reloads_at_the_target() {
    let h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    h.send(Command::Seek(Seek {
        position_ms: 120_000,
        expect_generation: None,
    }))
    .await;
    h.decks[0].set_position(3000); // a failed range request restarted libvlc near the start
    wait(0.3).await;
    assert!(
        h.deck_log()
            .contains(&"A load s1@120000 paused=false vol=100".to_string()),
        "{:?}",
        h.deck_log()
    );
}

#[tokio::test(start_paused = true)]
async fn a_seek_before_the_load_sets_the_start() {
    let h = harness().await;
    *h.opener.delay.lock().unwrap() = Duration::from_secs(1);
    h.send(play_now(songs(&["1"]), 0)).await;
    h.send(Command::Seek(Seek {
        position_ms: 30_000,
        expect_generation: None,
    }))
    .await;
    h.until_playing("1").await;
    assert_eq!(h.deck_log(), ["A load s1@30000 paused=false vol=100"]);
}

#[tokio::test(start_paused = true)]
async fn volume_is_applied_persisted_and_follows_the_mixer() {
    let h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    h.send(Command::Volume(Volume {
        level: 40,
        muted: None,
    }))
    .await;
    assert_eq!(h.deck_log().last().unwrap(), "A volume 40 muted=false");
    assert_eq!(h.store.get(StoreKey::Prefs).unwrap()["volume"], 40);
    // A mixer change right after our own is a stale reading.
    h.decks[0].set_mixer(70, false);
    wait(0.5).await;
    assert_eq!(h.state().volume, 40);
    wait(1.5).await;
    let s = h.until("mixer", |s| s.volume == 70).await;
    assert!(!s.muted);
    assert_eq!(h.store.get(StoreKey::Prefs).unwrap()["volume"], 70);
    // A mute is never stored.
    h.send(Command::Volume(Volume {
        level: 0,
        muted: Some(true),
    }))
    .await;
    assert_eq!(h.store.get(StoreKey::Prefs).unwrap()["volume"], 70);
    assert_eq!(
        h.player
            .send(Command::Volume(Volume {
                level: 101,
                muted: None
            }))
            .await,
        Err(CommandError::BadVolume(101))
    );
    // The next track opens at the current level.
    h.send(Command::Volume(Volume {
        level: 55,
        muted: None,
    }))
    .await;
    h.send(play_now(songs(&["2"]), 0)).await;
    h.until_playing("2").await;
    assert!(h.deck_log().last().unwrap().ends_with("vol=55"));
}

#[tokio::test(start_paused = true)]
async fn a_long_pause_keeps_the_session_alive_and_reopens_when_it_is_gone() {
    let h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    wait(20.0).await;
    h.send(Command::Pause).await;
    wait(5.0 * 60.0 + 1.0).await;
    assert!(
        h.opener
            .log
            .lock()
            .unwrap()
            .contains(&"touch s1".to_string())
    );
    *h.opener.touch_gone.lock().unwrap() = true;
    wait(5.0 * 60.0).await;
    h.send(Command::Play).await;
    h.until_playing("1").await;
    let last = h.deck_log().last().unwrap().clone();
    let at: u64 = last
        .strip_prefix("B load s2@")
        .and_then(|r| r.split(' ').next())
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (19_000..=21_000).contains(&at),
        "reopened at the paused position: {last}"
    );
}

// ── Scrobble and now playing ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn scrobble_is_due_after_half_the_played_time() {
    let mut h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    let track_events: Vec<_> = h
        .drain_events()
        .into_iter()
        .filter(|e| matches!(e, Event::Track(_)))
        .collect();
    assert_eq!(track_events.len(), 1, "now playing at the start");
    wait(60.0).await;
    h.send(Command::Pause).await;
    wait(120.0).await;
    h.send(Command::Play).await;
    wait(38.0).await;
    assert!(
        !h.drain_events()
            .iter()
            .any(|e| matches!(e, Event::ScrobbleDue(_))),
        "98 s played"
    );
    wait(3.0).await;
    let due: Vec<_> = h
        .drain_events()
        .into_iter()
        .filter_map(|e| {
            if let Event::ScrobbleDue(d) = e {
                Some(d)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].timestamp, T0 / 1000, "the start time");
    wait(60.0).await;
    assert!(
        !h.drain_events()
            .iter()
            .any(|e| matches!(e, Event::ScrobbleDue(_))),
        "once per track"
    );
}

#[tokio::test(start_paused = true)]
async fn short_tracks_are_not_scrobbled() {
    let mut h = harness().await;
    *h.length_ms.lock().unwrap() = 25_000;
    let mut s = songs(&["1"]);
    s[0].meta.duration_ms = 25_000;
    h.send(play_now(s, 0)).await;
    h.until_playing("1").await;
    wait(26.0).await;
    assert!(
        !h.drain_events()
            .iter()
            .any(|e| matches!(e, Event::ScrobbleDue(_)))
    );
}

// ── Play activity ───────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn play_activity_for_play_pause_seek_skip_and_natural_end() {
    let h = harness().await;
    *h.length_ms.lock().unwrap() = 100_000;
    let album = Container {
        kind: ContainerKind::Album,
        id: "al".into(),
        name: None,
    };
    let mut list = songs(&["1", "2", "3"]);
    for s in &mut list {
        s.meta.duration_ms = 100_000;
    }
    h.send(Command::SetQueue(SetQueue {
        items: list,
        start_index: 0,
        context: Some(album),
        shuffle: None,
        start_ms: None,
        play: true,
    }))
    .await;
    h.until_playing("1").await;
    wait(10.0).await;
    h.send(Command::Pause).await;
    h.send(Command::Play).await;
    wait(5.0).await;
    h.send(Command::Seek(Seek {
        position_ms: 50_000,
        expect_generation: None,
    }))
    .await;
    wait(5.0).await;
    h.send(Command::Next).await;
    h.until_playing("2").await;
    h.until_playing("3").await; // natural end of 2
    // Shutdown flushes and closes the open play as "exited".
    h.player.shutdown().await;
    let r = h.reports();
    type Row<'a> = (EventType, &'a str, u64, Option<u64>, Option<EndReason>);
    let summary: Vec<Row> = r
        .iter()
        .map(|e| {
            let s = |ms: u64| (ms + 500) / 1000;
            (
                e.event_type,
                e.ids.subscription_adam_id.as_str(),
                s(e.start_position_in_milliseconds),
                e.end_position_in_milliseconds.map(s),
                e.end_reason_type,
            )
        })
        .collect();
    use EventType::*;
    assert_eq!(
        summary,
        [
            (PlayStart, "1", 0, None, None),
            (PlayEnd, "1", 0, Some(10), Some(EndReason::ManuallyPaused)),
            (PlayStart, "1", 10, None, None),
            (PlayEnd, "1", 10, Some(15), Some(EndReason::ScrubBegin)),
            (PlayStart, "1", 50, None, None),
            (PlayEnd, "1", 50, Some(55), Some(EndReason::SkippedForwards)),
            (PlayStart, "2", 0, None, None),
            (PlayEnd, "2", 0, Some(100), Some(EndReason::NaturalEnd)),
            (PlayStart, "3", 0, None, None),
            (PlayEnd, "3", 0, Some(0), Some(EndReason::ExitedApplication)),
        ]
    );
    assert!(
        r.iter()
            .all(|e| e.container_type == 3 && e.media_duration_in_milliseconds == 100_000)
    );
}

#[tokio::test(start_paused = true)]
async fn play_starts_are_flushed_within_thirty_seconds() {
    let h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    wait(29.0).await;
    assert!(h.reports().is_empty());
    wait(2.0).await;
    assert_eq!(h.reports().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn no_play_reports_when_signed_out_or_disabled() {
    let h = harness().await;
    h.send(Command::Settings(SettingsPatch {
        report_plays: Some(false),
        ..SettingsPatch::default()
    }))
    .await;
    h.send(play_now(songs(&["1", "2"]), 0)).await;
    h.until_playing("1").await;
    h.send(Command::Next).await;
    h.until_playing("2").await;
    h.send(Command::Settings(SettingsPatch {
        report_plays: Some(true),
        ..SettingsPatch::default()
    }))
    .await;
    h.send(Command::Capabilities(Capabilities {
        lossless_ready: false,
        signed_in: false,
    }))
    .await;
    h.send(Command::Pause).await;
    h.send(Command::Play).await;
    wait(40.0).await;
    assert!(h.reports().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_state_event_carries_everything_the_ui_draws() {
    let mut h = harness().await;
    h.send(play_now(songs(&["1"]), 0)).await;
    h.until_playing("1").await;
    let state = h
        .drain_events()
        .into_iter()
        .rev()
        .find_map(|e| {
            if let Event::State(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();
    let v = serde_json::to_value(Event::State(state)).unwrap();
    assert_eq!(v["type"], "player.state");
    let p = &v["payload"];
    assert_eq!(p["status"], "playing");
    assert_eq!(p["track"]["id"], "1");
    assert_eq!(p["quality"]["codec"], "aac");
    assert_eq!(p["quality"]["tier"], "aac");
    assert_eq!(p["queue"]["currentIndex"], 0);
    assert_eq!(p["settings"]["quality"], "lossless");
    assert_eq!(p["volume"], 100);
}
