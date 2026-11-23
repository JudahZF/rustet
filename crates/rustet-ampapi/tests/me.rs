//! Requests with the music user token against a mock server.
//!
//! No signed-in response was ever recorded (no sign-in was allowed), so the answers here
//! are small bodies in the shape Apple documents. The request side (method, path, query,
//! headers, body) is what these tests pin. `me_nou.json` is a real recorded 403.

mod common;

use std::time::{Duration, SystemTime};

use common::{PLAY, env, expect, fixture, json, query, signed_in};
use futures::TryStreamExt;
use rustet_ampapi::api::{
    EndReason, History, ItemId, LibraryContainer, LibraryKind, MediaKind, NewPlaylist,
    PlayContainer, PlayEvent, PlayMode, PlayPhase, PlayedItem, PlaylistEdit, Rating, Repeat,
    ResourceRef, ResourceType,
};
use rustet_ampapi::model::{Resource, Shelf};
use rustet_ampapi::{Error, ErrorKind};
use serde_json::json as j;
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;

fn no_content() -> ResponseTemplate {
    ResponseTemplate::new(204)
}

fn song(id: &str) -> ResourceRef {
    ResourceRef::new(ResourceType::Songs, id)
}

#[tokio::test]
async fn signed_out_calls_fail_without_a_request() {
    let env = env().await;
    let cancel = CancellationToken::new();
    let err = env
        .api
        .library(&cancel, LibraryKind::Songs)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::MissingUserToken));
    assert_eq!(err.kind(), ErrorKind::Unauthorized);
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_refused_user_token_is_unauthorized() {
    let env = signed_in().await;
    expect(
        "GET",
        "/v1/me/library/songs",
        query(&[
            ("include", "catalog,albums"),
            ("limit", "100"),
            ("l", "en-US"),
        ]),
        true,
    )
    .respond_with(ResponseTemplate::new(403).set_body_string(fixture("me_nou.json")))
    .expect(1)
    .mount(&env.server)
    .await;
    let err = env
        .api
        .library(&CancellationToken::new(), LibraryKind::Songs)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized(s) if s == 403));
    assert_eq!(err.kind(), ErrorKind::Unauthorized);
}

#[tokio::test]
async fn account_storefront() {
    let env = signed_in().await;
    expect("GET", "/v1/me/storefront", query(&[]), true)
        .respond_with(json(fixture("storefront.json")))
        .expect(1)
        .mount(&env.server)
        .await;
    let sf = env
        .api
        .account_storefront(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(sf.id, "gb");
}

#[tokio::test]
async fn library_lists_page_through_and_are_not_cached() {
    let env = signed_in().await;
    expect(
        "GET",
        "/v1/me/library/albums",
        query(&[("include", "catalog"), ("limit", "100"), ("l", "en-US")]),
        true,
    )
    .respond_with(json(
        r#"{"next":"/v1/me/library/albums?offset=100","data":[
            {"id":"l.a","type":"library-albums","attributes":{"name":"A","playParams":{"id":"l.a","kind":"album","isLibrary":true}},
             "relationships":{"catalog":{"data":[{"id":"1","type":"albums","attributes":{"name":"A"}}]}}}]}"#
            .into(),
    ))
    .expect(2)
    .mount(&env.server)
    .await;
    expect(
        "GET",
        "/v1/me/library/albums",
        query(&[("offset", "100")]),
        true,
    )
    .respond_with(json(
        r#"{"data":[{"id":"l.b","type":"library-albums"}]}"#.into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;

    let cancel = CancellationToken::new();
    let first = env.api.library(&cancel, LibraryKind::Albums).await.unwrap();
    let Resource::LibraryAlbum(album) = &first.data[0] else {
        panic!("library album")
    };
    let catalog = &album.relationships.catalog.as_ref().unwrap().data[0];
    assert_eq!(catalog.id, "1");
    let pages: Vec<_> = env.api.pages(&cancel, first).try_collect().await.unwrap();
    assert_eq!(
        pages.concat().iter().map(Resource::id).collect::<Vec<_>>(),
        ["l.a", "l.b"]
    );
    // Library reads are never served from the cache.
    env.api.library(&cancel, LibraryKind::Albums).await.unwrap();
}

#[tokio::test]
async fn library_reads() {
    let env = signed_in().await;
    let list = || {
        json(r#"{"data":[{"id":"p.1","type":"library-playlists","attributes":{"name":"Mix","canEdit":true}}]}"#.into())
    };
    expect(
        "GET",
        "/v1/me/library/recently-added",
        query(&[("limit", "25"), ("include", "catalog"), ("l", "en-US")]),
        true,
    )
    .respond_with(list())
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "GET",
        "/v1/me/library/playlist-folders/p.playlistsroot/children",
        query(&[
            ("include", "catalog"),
            ("omit[resource]", "autos"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        true,
    )
    .respond_with(list())
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "GET",
        "/v1/me/library/playlists/p.1",
        query(&[("include", "catalog,tracks"), ("l", "en-US")]),
        true,
    )
    .respond_with(list())
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "GET",
        "/v1/me/library/playlists/p.1/tracks",
        query(&[("include", "catalog"), ("limit", "100")]),
        true,
    )
    .respond_with(json(
        r#"{"data":[{"id":"i.1","type":"library-songs","attributes":{"name":"S","playParams":{"id":"i.1","kind":"song","catalogId":"9"}}}]}"#.into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "GET",
        "/v1/me/library/search",
        query(&[
            ("term", "mix"),
            (
                "types",
                "library-songs,library-albums,library-artists,library-playlists",
            ),
            ("limit", "21"),
            ("include[library-songs]", "catalog"),
            ("include[library-artists]", "catalog"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        true,
    )
    .respond_with(json(
        r#"{"results":{"library-playlists":{"data":[{"id":"p.1","type":"library-playlists"}]}}}"#
            .into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;

    let cancel = CancellationToken::new();
    let api = &env.api;
    assert_eq!(
        api.library_recently_added(&cancel)
            .await
            .unwrap()
            .data
            .len(),
        1
    );
    let root = api.library_playlist_folder(&cancel, None).await.unwrap();
    assert!(
        matches!(&root.data[0], Resource::LibraryPlaylist(p) if p.attributes.as_ref().unwrap().can_edit)
    );
    let item = api
        .library_item(&cancel, LibraryKind::Playlists, "p.1")
        .await
        .unwrap();
    assert_eq!(item.name(), Some("Mix"));
    let tracks = api
        .library_tracks(&cancel, LibraryContainer::Playlist, "p.1")
        .await
        .unwrap();
    let Resource::LibrarySong(track) = &tracks.data[0] else {
        panic!("library song")
    };
    let play = track
        .attributes
        .as_ref()
        .unwrap()
        .play_params
        .as_ref()
        .unwrap();
    assert_eq!(play.catalog_id.as_deref(), Some("9"));
    let found = api.library_search(&cancel, "mix").await.unwrap();
    assert_eq!(found.playlists.unwrap().data[0].id(), "p.1");
}

#[tokio::test]
async fn home_and_history() {
    let env = signed_in().await;
    // Apple refuses Home without the local UTC offset.
    let timezone = chrono::Local::now().format("%:z").to_string();
    expect(
        "GET",
        "/v1/me/recommendations",
        query(&[
            ("timezone", &timezone),
            ("name", "listen-now"),
            ("extend[playlists]", "artistNames"),
            ("include[albums]", "artists"),
            ("include[library-playlists]", "catalog"),
            ("include[stations]", "radio-show"),
            ("include[personal-recommendation]", "primary-content"),
            ("fields[artists]", "name,artwork,url"),
            ("omit[resource]", "autos"),
            ("extend[stations]", "airTime,supportsAirTimeUpdates"),
            ("meta[stations]", "inflectionPoints"),
            ("platform", "web"),
            ("l", "en-US"),
        ]),
        true,
    )
    .respond_with(json(
        r#"{"data":[{"id":"r1","type":"personal-recommendation",
            "attributes":{"title":{"stringForDisplay":"Top Picks"},"display":{"kind":"MusicCoverShelf"}},
            "relationships":{"contents":{"data":[{"id":"1","type":"albums"}]}}}]}"#
            .into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;
    // Apple refuses more than 10 for heavy rotation.
    for (path, limit) in [
        ("/v1/me/recent/played", "20"),
        ("/v1/me/recent/played/tracks", "20"),
        ("/v1/me/recent/radio-stations", "20"),
        ("/v1/me/history/heavy-rotation", "10"),
    ] {
        expect(
            "GET",
            path,
            query(&[("limit", limit), ("l", "en-US")]),
            true,
        )
        .respond_with(json(r#"{"data":[{"id":"1","type":"albums"}]}"#.into()))
        .expect(1)
        .mount(&env.server)
        .await;
    }

    let cancel = CancellationToken::new();
    let shelves = env.api.home(&cancel).await.unwrap();
    assert!(
        matches!(&shelves[..], [Shelf::Grid { title: Some(t), items, .. }]
        if t == "Top Picks" && items.len() == 1)
    );
    for history in [
        History::RecentlyPlayed,
        History::RecentTracks,
        History::RecentStations,
        History::HeavyRotation,
    ] {
        assert_eq!(
            env.api.history(&cancel, history).await.unwrap().data.len(),
            1
        );
    }
}

#[tokio::test]
async fn library_writes() {
    let env = signed_in().await;
    expect(
        "POST",
        "/v1/me/library",
        query(&[
            ("ids[songs]", "1,2"),
            ("ids[albums]", "3"),
            ("representation", "ids"),
        ]),
        true,
    )
    .respond_with(ResponseTemplate::new(202))
    .expect(1)
    .mount(&env.server)
    .await;
    expect("DELETE", "/v1/me/library/songs/i.1", query(&[]), true)
        .respond_with(no_content())
        .expect(1)
        .mount(&env.server)
        .await;

    let cancel = CancellationToken::new();
    let items = [
        song("1"),
        ResourceRef::new(ResourceType::Albums, "3"),
        song("2"),
    ];
    env.api.add_to_library(&cancel, &items).await.unwrap();
    env.api
        .remove_from_library(&cancel, &song("i.1"))
        .await
        .unwrap();
}

#[tokio::test]
async fn playlist_create_edit_reorder_delete() {
    let env = signed_in().await;
    let id = "p.new";
    expect("POST", "/v1/me/library/playlists", query(&[]), true)
        .and(body_json(j!({
            "attributes": {"name": "Road", "description": "For driving"},
            "relationships": {"tracks": {"data": [{"id": "1", "type": "songs"}]}},
        })))
        .respond_with(ResponseTemplate::new(201).set_body_string(
            r#"{"data":[{"id":"p.new","type":"library-playlists","attributes":{"name":"Road"}}]}"#,
        ))
        .expect(1)
        .mount(&env.server)
        .await;
    expect(
        "PATCH",
        &format!("/v1/me/library/playlists/{id}"),
        query(&[]),
        true,
    )
    .and(body_json(j!({"attributes": {"name": "Road trip"}})))
    .respond_with(no_content())
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "POST",
        &format!("/v1/me/library/playlists/{id}/tracks"),
        query(&[]),
        true,
    )
    .and(body_json(j!({"data": [{"id": "2", "type": "songs"}]})))
    .respond_with(no_content())
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "PUT",
        &format!("/v1/me/library/playlists/{id}/tracks"),
        query(&[]),
        true,
    )
    .and(body_json(j!({"data": [
        {"id": "i.2", "type": "library-songs"},
        {"id": "i.1", "type": "library-songs"},
    ]})))
    .respond_with(no_content())
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "DELETE",
        &format!("/v1/me/library/playlists/{id}/tracks"),
        query(&[("ids[library-songs]", "i.1"), ("mode", "all")]),
        true,
    )
    .respond_with(no_content())
    .expect(1)
    .mount(&env.server)
    .await;
    expect(
        "DELETE",
        &format!("/v1/me/library/playlists/{id}"),
        query(&[]),
        true,
    )
    .respond_with(no_content())
    .expect(1)
    .mount(&env.server)
    .await;

    let cancel = CancellationToken::new();
    let api = &env.api;
    let created = api
        .create_playlist(
            &cancel,
            &NewPlaylist {
                name: "Road".into(),
                description: Some("For driving".into()),
                tracks: vec![song("1")],
            },
        )
        .await
        .unwrap();
    assert_eq!(created.id, id);
    let rename = PlaylistEdit {
        name: Some("Road trip".into()),
        description: None,
    };
    api.edit_playlist(&cancel, id, &rename).await.unwrap();
    api.add_playlist_tracks(&cancel, id, &[song("2")])
        .await
        .unwrap();
    let order = [
        ResourceRef::new(ResourceType::LibrarySongs, "i.2"),
        ResourceRef::new(ResourceType::LibrarySongs, "i.1"),
    ];
    api.set_playlist_tracks(&cancel, id, &order).await.unwrap();
    api.remove_playlist_track(&cancel, id, "i.1").await.unwrap();
    api.remove_from_library(&cancel, &ResourceRef::new(ResourceType::Playlists, id))
        .await
        .unwrap();
}

#[tokio::test]
async fn ratings_and_favourites() {
    let env = signed_in().await;
    expect("GET", "/v1/me/ratings/songs/1", query(&[]), true)
        .respond_with(json(
            r#"{"data":[{"id":"1","type":"ratings","attributes":{"value":1}}]}"#.into(),
        ))
        .expect(1)
        .mount(&env.server)
        .await;
    expect("GET", "/v1/me/ratings/songs/2", query(&[]), true)
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&env.server)
        .await;
    expect("PUT", "/v1/me/ratings/library-songs/i.1", query(&[]), true)
        .and(body_json(
            j!({"type": "rating", "attributes": {"value": -1}}),
        ))
        .respond_with(no_content())
        .expect(1)
        .mount(&env.server)
        .await;
    expect("DELETE", "/v1/me/ratings/songs/1", query(&[]), true)
        .respond_with(no_content())
        .expect(1)
        .mount(&env.server)
        .await;
    for verb in ["POST", "DELETE"] {
        expect(
            verb,
            "/v1/me/favorites",
            query(&[("ids[albums]", "3")]),
            true,
        )
        .respond_with(no_content())
        .expect(1)
        .mount(&env.server)
        .await;
    }

    let cancel = CancellationToken::new();
    let api = &env.api;
    assert_eq!(
        api.rating(&cancel, &song("1")).await.unwrap(),
        Some(Rating::Love)
    );
    assert_eq!(api.rating(&cancel, &song("2")).await.unwrap(), None);
    let library_song = ResourceRef::new(ResourceType::LibrarySongs, "i.1");
    api.set_rating(&cancel, &library_song, Rating::Dislike)
        .await
        .unwrap();
    api.clear_rating(&cancel, &song("1")).await.unwrap();
    let album = ResourceRef::new(ResourceType::Albums, "3");
    api.set_favorite(&cancel, &album, true).await.unwrap();
    api.set_favorite(&cancel, &album, false).await.unwrap();
}

#[tokio::test]
async fn station_tracks_and_autoplay() {
    let env = signed_in().await;
    expect(
        "POST",
        "/v1/me/stations/next-tracks/ra.1",
        query(&[("limit", "10")]),
        true,
    )
    .respond_with(json(
        r#"{"data":[{"id":"1","type":"songs"},{"id":"2","type":"songs"}]}"#.into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;
    // Twelve seeds: only the last ten are sent.
    let seeds: Vec<_> = (1..=12).map(|n| song(&n.to_string())).collect();
    let sent: Vec<_> = (3..=12)
        .map(|n| j!({"id": n.to_string(), "type": "songs"}))
        .collect();
    expect(
        "POST",
        "/v1/me/stations/continuous",
        query(&[("limit[results:tracks]", "10"), ("with", "tracks")]),
        true,
    )
    .and(body_json(j!({ "data": sent })))
    .respond_with(json(
        r#"{"results":{"station":{"id":"ra.auto","type":"stations"},"tracks":[{"id":"20","type":"songs"}]}}"#.into(),
    ))
    .expect(1)
    .mount(&env.server)
    .await;

    let cancel = CancellationToken::new();
    let next = env
        .api
        .station_next_tracks(&cancel, "ra.1", 10)
        .await
        .unwrap();
    assert_eq!(next.len(), 2);
    let auto = env.api.autoplay(&cancel, &seeds, 10).await.unwrap();
    assert_eq!(auto.station.unwrap().id(), "ra.auto");
    assert_eq!(auto.tracks[0].id(), "20");
}

#[tokio::test]
async fn play_activity_is_one_batch_with_both_tokens() {
    let env = signed_in().await;
    expect("POST", PLAY, query(&[]), true)
        .and(|r: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            let events = body["data"].as_array().unwrap();
            body["client_id"] == "JSCLIENT"
                && body["event_type"] == "JSPLAY"
                && events.len() == 2
                && events[0]["event-type"] == 1
                && events[1]["event-type"] == 0
                && events[1]["end-reason-type"] == 2
                && events[1]["container-ids"] == j!({"station-id": "ra.1", "station-hash": "h"})
                && events.iter().all(|e| {
                    e["developer-token"] == "dev"
                        && e["user-token"] == "mut"
                        && e["store-front"] == "us"
                        && e["milliseconds-since-play"].as_u64().unwrap() >= 5_000
                })
                && r.headers
                    .get("content-type")
                    .is_some_and(|v| v == "application/json")
        })
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&env.server)
        .await;

    let at = SystemTime::now() - Duration::from_secs(5);
    let event = |phase| PlayEvent {
        item: PlayedItem {
            id: ItemId::Catalog("1".into()),
            media: MediaKind::Audio,
        },
        container: Some(PlayContainer::Station {
            id: "ra.1".into(),
            hash: Some("h".into()),
        }),
        phase,
        duration: Duration::from_secs(180),
        mode: PlayMode {
            shuffle: false,
            repeat: Repeat::Off,
            autoplay: true,
        },
        at,
        utc_offset_seconds: None,
        reco_data: None,
    };
    let events = [
        event(PlayPhase::Start {
            position: Duration::ZERO,
            container_changed: false,
        }),
        event(PlayPhase::End {
            start: Duration::ZERO,
            end: Duration::from_secs(30),
            reason: EndReason::SkippedForwards,
        }),
    ];
    let cancel = CancellationToken::new();
    env.api.report_plays(&cancel, &events).await.unwrap();
    // Nothing to send: no request.
    env.api.report_plays(&cancel, &[]).await.unwrap();
}
