//! Play intents: the player commands built for play, shuffle, play next/later and stations
//! (spec §4.2).

mod pages_common;

use std::time::Duration;

use pages_common::{env, fixture};
use rustet_ampapi::model::AudioTrait;
use rustet_app::pages::catalog;
use rustet_app::pages::model::{Artwork, Availability, Linked, TrackId, TrackKind, TrackRow};
use rustet_app::pages::play::{self, How, PlayError, PlayTarget, StationRef};
use rustet_player::model::{
    Command, Container, ContainerKind, Insert, InsertPosition, ItemKind, SetQueue,
};

fn row(id: &str, available: bool) -> TrackRow {
    TrackRow {
        id: TrackId::Catalog(id.to_owned()),
        kind: TrackKind::Song,
        number: None,
        disc: None,
        title: format!("Song {id}"),
        explicit: false,
        artist: Linked {
            text: "Artist".into(),
            route: None,
        },
        album: Some(Linked {
            text: "Album".into(),
            route: None,
        }),
        duration: Some(Duration::from_millis(180_500)),
        traits: vec![AudioTrait::Lossless, AudioTrait::HiResLossless],
        apple_digital_master: false,
        availability: if available {
            Availability::Playable
        } else {
            Availability::Unavailable
        },
        artwork: None,
        popularity: None,
    }
}

fn album() -> Option<Container> {
    Some(Container {
        kind: ContainerKind::Album,
        id: "1".into(),
        name: Some("A".into()),
    })
}

fn set_queue(command: Command) -> SetQueue {
    match command {
        Command::SetQueue(q) => q,
        other => panic!("not a SetQueue: {other:?}"),
    }
}

fn ids(items: &[rustet_player::model::QueueItemIn]) -> Vec<&str> {
    items.iter().map(|i| i.id.as_str()).collect()
}

#[test]
fn playing_from_a_track_skips_unavailable_ones_and_keeps_the_shuffle_mode() {
    let tracks = [
        row("a", true),
        row("b", false),
        row("c", true),
        row("d", true),
    ];
    let q = set_queue(play::play(&tracks, album(), How::From(2)).unwrap());
    assert_eq!(ids(&q.items), ["a", "c", "d"]);
    assert_eq!(q.start_index, 1, "the index among the playable tracks");
    assert_eq!(q.shuffle, None);
    assert_eq!(q.context, album());
    assert!(q.play);
    assert!(
        q.items.iter().all(|i| i.container_id.is_none()),
        "same as the context"
    );

    assert_eq!(
        play::play(&tracks, album(), How::From(1)),
        Err(PlayError::Unavailable)
    );
    assert_eq!(
        play::play(&tracks, album(), How::From(9)),
        Err(PlayError::Unavailable)
    );
    assert_eq!(
        play::play(&[row("x", false)], album(), How::InOrder),
        Err(PlayError::NothingPlayable)
    );
}

#[test]
fn play_turns_shuffle_off_and_shuffle_turns_it_on_at_a_random_start() {
    let tracks: Vec<_> = (0..20).map(|i| row(&i.to_string(), true)).collect();
    let q = set_queue(play::play(&tracks, album(), How::InOrder).unwrap());
    assert_eq!((q.start_index, q.shuffle), (0, Some(false)));

    let starts: std::collections::HashSet<_> = (0..50)
        .map(|_| {
            let q = set_queue(play::play(&tracks, album(), How::Shuffled).unwrap());
            assert_eq!(q.shuffle, Some(true));
            assert_eq!(q.items.len(), 20);
            q.start_index
        })
        .collect();
    assert!(starts.iter().all(|&s| s < 20));
    assert!(starts.len() > 1, "the first track is not always the same");
}

#[test]
fn play_next_and_later_insert_with_their_container() {
    let tracks = [row("a", true), row("b", false)];
    for (how, position) in [
        (How::Next, InsertPosition::Next),
        (How::Later, InsertPosition::Later),
    ] {
        let Command::Insert(Insert {
            items,
            position: got,
        }) = play::play(&tracks, album(), how).unwrap()
        else {
            panic!("an insert")
        };
        assert_eq!(got, position);
        assert_eq!(ids(&items), ["a"]);
        assert_eq!(items[0].container_id.as_deref(), Some("1"));
    }
}

#[test]
fn a_queue_item_carries_what_the_row_shows() {
    let mut video = row("9", true);
    video.kind = TrackKind::MusicVideo;
    video.explicit = true;
    video.id = TrackId::Library {
        library_id: "i.X".into(),
        catalog_id: Some("9".into()),
    };
    video.artwork = Some(Artwork {
        template: "https://a/{w}x{h}bb.jpg".into(),
        width: None,
        height: None,
        palette: None,
    });
    let item = play::queue_item(&video, None).unwrap();
    assert_eq!(item.id.as_str(), "9");
    assert_eq!(item.library_id.as_deref(), Some("i.X"));
    assert_eq!(item.kind, ItemKind::MusicVideo);
    assert_eq!(item.meta.name, "Song 9");
    assert_eq!(item.meta.album_name, "Album");
    assert_eq!(item.meta.artwork_url, "https://a/{w}x{h}bb.jpg");
    assert_eq!(item.meta.duration_ms, 180_500);
    assert_eq!(item.meta.audio_traits, ["lossless", "hi-res-lossless"]);
    assert_eq!(item.meta.content_rating.as_deref(), Some("explicit"));

    // A library song that is not in the catalog cannot be opened by the engine.
    let mut local = row("x", true);
    local.id = TrackId::Library {
        library_id: "i.Y".into(),
        catalog_id: None,
    };
    assert!(play::queue_item(&local, None).is_none());
}

#[test]
fn a_station_starts_empty_and_cannot_be_queued() {
    let station = StationRef {
        id: "ra.1".into(),
        name: "R".into(),
        live: false,
    };
    let q = set_queue(play::station(&station, How::InOrder).unwrap());
    assert!(q.items.is_empty());
    assert_eq!(
        q.context.as_ref().map(|c| c.kind),
        Some(ContainerKind::Station)
    );
    let live = StationRef {
        live: true,
        ..station.clone()
    };
    let q = set_queue(play::station(&live, How::InOrder).unwrap());
    assert_eq!(q.context.map(|c| c.kind), Some(ContainerKind::LiveStation));
    assert_eq!(
        play::station(&station, How::Next),
        Err(PlayError::NotQueueable)
    );
}

#[tokio::test]
async fn stations_need_the_user_token() {
    let env = env().await;
    let target = PlayTarget::Station(StationRef {
        id: "ra.1".into(),
        name: "R".into(),
        live: false,
    });
    assert_eq!(
        play::play_target(&env.api, &target, How::InOrder).await,
        Err(PlayError::SignInRequired)
    );
}

#[tokio::test]
async fn an_album_tile_loads_its_tracks_and_plays_them_as_the_album() {
    let env = env().await;
    env.get("/v1/catalog/us/albums/1889568617", fixture("album.json"))
        .await;
    let target = PlayTarget::Album {
        id: "1889568617".into(),
    };
    let q = set_queue(
        play::play_target(&env.api, &target, How::InOrder)
            .await
            .unwrap(),
    );
    assert_eq!(q.items.len(), 3);
    let context = q.context.unwrap();
    assert_eq!(
        (context.kind, context.id.as_str()),
        (ContainerKind::Album, "1889568617")
    );
    assert_eq!(context.name.as_deref(), Some("WARM UP REMIXES"));

    let page = catalog::album(&env.api, "1889568617").await.unwrap();
    let q = set_queue(page.play(How::From(2)).unwrap());
    assert_eq!(q.start_index, 2);
    assert_eq!(
        q.items[2].id.as_str(),
        page.tracks().nth(2).unwrap().id.catalog_id().unwrap()
    );
}
