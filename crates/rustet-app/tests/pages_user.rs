//! Signed-in pages and actions against a mock server. The `/v1/me/…` responses are
//! hand-written in the shape of Apple's documentation (no signed-in call was recorded).

mod pages_common;

use pages_common::{env, json, signed_in};
use rustet_ampapi::api::{Rating, ResourceRef, ResourceType};
use rustet_app::pages::actions::{self, Action, MenuContext, MenuItem};
use rustet_app::pages::catalog::SearchGroup;
use rustet_app::pages::model::{Availability, Listing, ShelfItems, TileKind, TrackId};
use rustet_app::pages::play::{How, PlayTarget};
use rustet_app::pages::{self, LoadError, Page, user};
use rustet_app::route::{LibrarySection, Route};
use rustet_player::model::{Command, ContainerKind};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

fn library_song(id: &str, catalog: Option<&str>) -> serde_json::Value {
    let mut play_params = json!({"id": id, "kind": "song", "isLibrary": true});
    if let Some(c) = catalog {
        play_params["catalogId"] = c.into();
    }
    json!({
        "id": id, "type": "library-songs",
        "attributes": {
            "name": format!("Song {id}"), "artistName": "Artist", "albumName": "Album",
            "durationInMillis": 200000, "trackNumber": 2, "discNumber": 1,
            "contentRating": "explicit", "playParams": play_params,
            "artwork": {"url": "https://a/{w}x{h}bb.jpg", "width": 1000, "height": 1000}
        },
        "relationships": {
            "catalog": {"data": catalog.map(|c| vec![json!({"id": c, "type": "songs", "attributes": {
                "name": "S", "artistName": "Artist", "audioTraits": ["lossless", "atmos"],
                "playParams": {"id": c, "kind": "song"}}})]).unwrap_or_default()},
            "albums": {"data": [{"id": "l.Album1", "type": "library-albums"}]}
        }
    })
}

#[tokio::test]
async fn signed_out_library_pages_fail_without_a_request() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&env.server)
        .await;
    for route in [
        Route::Home,
        Route::Library(LibrarySection::Albums),
        Route::LibrarySearch { term: "x".into() },
        Route::LibraryPlaylist { id: "p.X".into() },
    ] {
        assert_eq!(
            pages::load(&env.api, &route).await,
            Err(LoadError::SignInRequired),
            "{route:?}"
        );
        let state = LoadError::SignInRequired.state(&route);
        assert_eq!(state.action, pages::Recovery::SignIn);
    }
}

#[tokio::test]
async fn library_songs_are_library_rows_and_continue_with_their_includes() {
    let env = signed_in().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/library/songs"))
        .and(header("media-user-token", "mut"))
        .and(query_param("include", "catalog,albums"))
        .respond_with(json(
            json!({"data": [library_song("i.A", Some("100")), library_song("i.B", None)],
                   "next": "/v1/me/library/songs?offset=100"})
            .to_string(),
        ))
        .mount(&env.server)
        .await;
    let page = user::library(&env.api, LibrarySection::Songs)
        .await
        .unwrap();
    let Listing::Tracks(list) = &page.listing else {
        panic!("songs are rows")
    };
    let [a, b] = &list.items[..] else {
        panic!("two rows")
    };
    assert_eq!(
        a.id,
        TrackId::Library {
            library_id: "i.A".into(),
            catalog_id: Some("100".into())
        }
    );
    assert_eq!(a.availability, Availability::Playable);
    assert!(
        a.explicit && a.quality().dolby_atmos,
        "marks from the catalog song"
    );
    assert_eq!(
        a.album.as_ref().unwrap().route,
        Some(Route::LibraryAlbum {
            id: "l.Album1".into()
        })
    );
    assert_eq!(
        b.availability,
        Availability::Unavailable,
        "not in the catalog"
    );
    assert!(page.empty_state().is_none());

    let Ok(Command::SetQueue(q)) = page.play(How::InOrder) else {
        panic!("set queue")
    };
    assert_eq!(q.items.len(), 1);
    assert_eq!(q.items[0].library_id.as_deref(), Some("i.A"));
    assert_eq!(q.context.map(|c| c.kind), Some(ContainerKind::LibrarySongs));

    // The next link lost `include`: the continuation adds it back.
    Mock::given(method("GET"))
        .and(path("/v1/me/library/songs"))
        .and(query_param("offset", "100"))
        .and(query_param("include", "catalog,albums"))
        .and(query_param("limit", "100"))
        .respond_with(json(
            json!({"data": [library_song("i.C", Some("101"))]}).to_string(),
        ))
        .with_priority(1)
        .expect(1)
        .mount(&env.server)
        .await;
    let more = list.more.as_ref().unwrap().load(&env.api).await.unwrap();
    assert_eq!(more.items.len(), 1);
    assert!(more.more.is_none());
}

#[tokio::test]
async fn library_albums_are_tiles_that_open_and_play_the_library_album() {
    let env = signed_in().await;
    env.get(
        "/v1/me/library/albums",
        json!({"data": [{"id": "l.Album1", "type": "library-albums", "attributes": {
            "name": "Album", "artistName": "Artist", "trackCount": 2,
            "playParams": {"id": "l.Album1", "kind": "album", "isLibrary": true}}}]})
        .to_string(),
    )
    .await;
    let page = user::library(&env.api, LibrarySection::Albums)
        .await
        .unwrap();
    let Listing::Tiles(list) = &page.listing else {
        panic!("tiles")
    };
    let tile = &list.items[0];
    assert_eq!(tile.kind, TileKind::Album);
    assert_eq!(
        tile.route,
        Some(Route::LibraryAlbum {
            id: "l.Album1".into()
        })
    );
    assert_eq!(
        tile.play,
        Some(PlayTarget::LibraryAlbum {
            id: "l.Album1".into()
        })
    );
    assert_eq!(
        tile.item,
        Some(ResourceRef::new(ResourceType::LibraryAlbums, "l.Album1"))
    );
}

#[tokio::test]
async fn recently_played_is_apples_history_as_tiles() {
    let env = signed_in().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/recent/played"))
        .and(header("media-user-token", "mut"))
        .respond_with(json(
            json!({"data": [{"id": "pl.1", "type": "playlists", "attributes": {"name": "P"}},
                            {"id": "ra.1", "type": "stations", "attributes": {"name": "R", "playParams": {"id": "ra.1", "kind": "radioStation", "format": "tracks"}}}]})
            .to_string(),
        ))
        .mount(&env.server)
        .await;
    let page = user::library(&env.api, LibrarySection::RecentlyPlayed)
        .await
        .unwrap();
    let Listing::Tiles(list) = &page.listing else {
        panic!("tiles")
    };
    let kinds: Vec<_> = list.items.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        [TileKind::Playlist, TileKind::Station { live: false }]
    );
}

#[tokio::test]
async fn an_empty_library_section_says_how_to_fill_it() {
    let env = signed_in().await;
    env.get("/v1/me/library/playlists", r#"{"data":[]}"#).await;
    let page = user::library(&env.api, LibrarySection::Playlists)
        .await
        .unwrap();
    let empty = page.empty_state().unwrap();
    assert_eq!(empty.title, "No playlists in your library");
}

#[tokio::test]
async fn library_playlist_page_is_editable_and_reorders() {
    let env = signed_in().await;
    env.get(
        "/v1/me/library/playlists/p.X",
        json!({"data": [{"id": "p.X", "type": "library-playlists",
            "attributes": {"name": "Mine", "canEdit": true, "description": {"standard": "My <b>mix</b>"}},
            "relationships": {
                "tracks": {"data": [library_song("i.A", Some("1")), library_song("i.B", Some("2")),
                                     library_song("i.C", Some("3"))]},
                "catalog": {"data": [{"id": "pl.u-1", "type": "playlists",
                                       "attributes": {"name": "Mine", "curatorName": "Me"}}]}}}]})
        .to_string(),
    )
    .await;
    let Page::LibraryPlaylist(page) =
        pages::load(&env.api, &Route::LibraryPlaylist { id: "p.X".into() })
            .await
            .unwrap()
    else {
        panic!("library playlist page")
    };
    assert!(page.editable);
    assert_eq!(
        page.header.notes.as_ref().unwrap().full.as_deref(),
        Some("My mix")
    );
    assert_eq!(
        page.catalog,
        Some(Route::Playlist {
            id: "pl.u-1".into()
        })
    );
    let order: Vec<_> = page
        .moved(0, 2)
        .unwrap()
        .iter()
        .map(|r| r.title.clone())
        .collect();
    assert_eq!(order, ["Song i.B", "Song i.C", "Song i.A"]);
    assert!(page.moved(0, 3).is_none());
}

#[tokio::test]
async fn library_search_has_a_section_per_type() {
    let env = signed_in().await;
    env.get(
        "/v1/me/library/search",
        json!({"results": {
            "library-songs": {"data": [library_song("i.A", Some("1"))]},
            "library-albums": {"data": [{"id": "l.1", "type": "library-albums", "attributes": {"name": "Al"}}]}
        }})
        .to_string(),
    )
    .await;
    let page = user::library_search(&env.api, "a").await.unwrap();
    let groups: Vec<_> = page.sections.iter().map(|s| s.group.clone()).collect();
    assert_eq!(groups, [SearchGroup::Songs, SearchGroup::Albums]);
}

#[tokio::test]
async fn home_shelves_come_from_personal_recommendations() {
    let env = signed_in().await;
    env.get(
        "/v1/me/recommendations",
        json!({"data": [{"id": "r1", "type": "personal-recommendation",
            "attributes": {"title": {"stringForDisplay": "Made for You"}, "display": {"kind": "MusicCoverShelf"}},
            "relationships": {"contents": {"data": [
                {"id": "pl.pm-1", "type": "playlists", "attributes": {"name": "Favourites Mix"}}]}}}]})
        .to_string(),
    )
    .await;
    let page = user::home(&env.api).await.unwrap();
    assert_eq!(page.shelves[0].title.as_deref(), Some("Made for You"));
    assert!(
        matches!(&page.shelves[0].items, ShelfItems::Tiles { tiles, .. } if tiles[0].title == "Favourites Mix")
    );
}

/// Mounts one write that must happen exactly once.
async fn expect_write(env: &pages_common::Env, verb: &str, route: &str, status: u16, body: &str) {
    Mock::given(method(verb))
        .and(path(route))
        .and(header("media-user-token", "mut"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .expect(1)
        .mount(&env.server)
        .await;
}

#[tokio::test]
async fn actions_send_the_library_writes() {
    let env = signed_in().await;
    let song = ResourceRef::new(ResourceType::Songs, "1");
    Mock::given(method("POST"))
        .and(path("/v1/me/library"))
        .and(query_param("ids[songs]", "1"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&env.server)
        .await;
    // The web player removes with the catalog type and the library ID.
    expect_write(&env, "DELETE", "/v1/me/library/songs/i.A", 204, "").await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/ratings/songs/1"))
        .and(body_json(
            json!({"type": "rating", "attributes": {"value": 1}}),
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&env.server)
        .await;
    expect_write(&env, "DELETE", "/v1/me/ratings/songs/1", 204, "").await;
    expect_write(
        &env,
        "POST",
        "/v1/me/library/playlists",
        201,
        r#"{"data":[{"id":"p.NEW","type":"library-playlists"}]}"#,
    )
    .await;
    expect_write(&env, "PATCH", "/v1/me/library/playlists/p.NEW", 204, "").await;
    Mock::given(method("POST"))
        .and(path("/v1/me/library/playlists/p.NEW/tracks"))
        .and(body_json(json!({"data": [{"id": "1", "type": "songs"}]})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/me/library/playlists/p.NEW/tracks"))
        .and(body_json(json!({"data": [{"id": "i.B", "type": "library-songs"}, {"id": "i.A", "type": "library-songs"}]})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/me/library/playlists/p.NEW/tracks"))
        .and(query_param("ids[library-songs]", "i.A"))
        .and(query_param("mode", "all"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&env.server)
        .await;
    expect_write(&env, "DELETE", "/v1/me/library/playlists/p.NEW", 204, "").await;

    let lib = |id: &str| ResourceRef::new(ResourceType::LibrarySongs, id);
    let steps = [
        (Action::AddToLibrary(vec![song.clone()]), "Added to Library"),
        (
            Action::RemoveFromLibrary(lib("i.A")),
            "Removed from Library",
        ),
        (
            Action::Rate {
                item: song.clone(),
                rating: Some(Rating::Love),
            },
            "Loved",
        ),
        (
            Action::Rate {
                item: song.clone(),
                rating: None,
            },
            "Rating removed",
        ),
        (
            Action::CreatePlaylist {
                name: "New".into(),
                description: None,
                tracks: vec![],
            },
            "Playlist created",
        ),
        (
            Action::EditPlaylist {
                id: "p.NEW".into(),
                name: Some("Renamed".into()),
                description: None,
            },
            "Playlist changed",
        ),
        (
            Action::AddToPlaylist {
                playlist_id: "p.NEW".into(),
                tracks: vec![song.clone()],
            },
            "Added to Playlist",
        ),
        (
            Action::SetPlaylistOrder {
                id: "p.NEW".into(),
                tracks: vec![lib("i.B"), lib("i.A")],
            },
            "Playlist reordered",
        ),
        (
            Action::RemoveFromPlaylist {
                id: "p.NEW".into(),
                library_song_id: "i.A".into(),
            },
            "Removed from Playlist",
        ),
        (
            Action::DeletePlaylist { id: "p.NEW".into() },
            "Playlist deleted",
        ),
    ];
    for (action, toast) in steps {
        let outcome = actions::perform(&env.api, &action)
            .await
            .unwrap_or_else(|e| panic!("{action:?}: {e}"));
        assert_eq!(outcome.toast, toast);
        if matches!(action, Action::CreatePlaylist { .. }) {
            assert_eq!(outcome.created.as_deref(), Some("p.NEW"));
        }
    }
}

#[tokio::test]
async fn a_refused_action_says_what_failed_and_what_to_do() {
    let env = signed_in().await;
    expect_write(&env, "PUT", "/v1/me/ratings/songs/1", 403, "").await;
    let action = Action::Rate {
        item: ResourceRef::new(ResourceType::Songs, "1"),
        rating: Some(Rating::Love),
    };
    let err = actions::perform(&env.api, &action).await.unwrap_err();
    assert_eq!(err.cause, LoadError::SignInRequired);
    assert_eq!(
        err.toast,
        "Could not love this. Sign in to Apple Music, then try again."
    );

    let signed_out = env_signed_out().await;
    let err = actions::perform(&signed_out.api, &action)
        .await
        .unwrap_err();
    assert_eq!(err.cause, LoadError::SignInRequired);
}

async fn env_signed_out() -> pages_common::Env {
    let env = env().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&env.server)
        .await;
    env
}

#[tokio::test]
async fn menus_offer_what_the_item_and_the_account_allow() {
    let env = signed_in().await;
    env.get(
        "/v1/me/library/playlists/p.X",
        json!({"data": [{"id": "p.X", "type": "library-playlists", "attributes": {"name": "Mine", "canEdit": true},
            "relationships": {"tracks": {"data": [library_song("i.A", Some("1"))]}}}]})
        .to_string(),
    )
    .await;
    let page = user::library_playlist(&env.api, "p.X").await.unwrap();
    let row = &page.tracks.items[0];

    let signed_in = MenuContext {
        signed_in: true,
        storefront: "us",
        playlist: Some("p.X"),
    };
    let menu = actions::track_menu(row, signed_in);
    assert_eq!(menu[..2], [MenuItem::PlayNext, MenuItem::PlayLater]);
    let library_song = ResourceRef::new(ResourceType::LibrarySongs, "i.A");
    assert!(menu.contains(&MenuItem::RemoveFromLibrary(library_song.clone())));
    assert!(menu.contains(&MenuItem::Love(library_song)));
    assert!(menu.contains(&MenuItem::ShareLink(
        "https://music.apple.com/us/song/1".into()
    )));
    assert!(menu.contains(&MenuItem::RemoveFromPlaylist {
        playlist_id: "p.X".into(),
        library_song_id: "i.A".into()
    }));

    let signed_out = MenuContext {
        signed_in: false,
        storefront: "us",
        playlist: None,
    };
    let menu = actions::track_menu(row, signed_out);
    assert!(menu.iter().all(|m| !matches!(
        m,
        MenuItem::AddToLibrary(_)
            | MenuItem::RemoveFromLibrary(_)
            | MenuItem::Love(_)
            | MenuItem::AddToPlaylist(_)
            | MenuItem::RemoveFromPlaylist { .. }
    )));
}

#[tokio::test]
async fn playlist_folders_list_folders_and_playlists_across_pages() {
    use rustet_app::library::{PlaylistEntry, playlist_folder};
    let env = env().await;
    assert_eq!(
        playlist_folder(&env.api, None).await,
        Err(LoadError::SignInRequired)
    );
    let env = signed_in().await;
    let root = "/v1/me/library/playlist-folders/p.playlistsroot/children";
    Mock::given(method("GET"))
        .and(path(root))
        .and(query_param("omit[resource]", "autos"))
        .and(wiremock::matchers::query_param_is_missing("offset"))
        .respond_with(json(
            json!({"data": [{"id": "p.F1", "type": "library-playlist-folders", "attributes": {"name": "Moods"}}],
                   "next": format!("{root}?offset=1")})
            .to_string(),
        ))
        .mount(&env.server)
        .await;
    // The `next` link lost the first request's parameters; they are sent again.
    Mock::given(method("GET"))
        .and(path(root))
        .and(query_param("offset", "1"))
        .and(query_param("include", "catalog"))
        .respond_with(json(
            json!({"data": [{"id": "p.P1", "type": "library-playlists", "attributes": {"name": "Mine", "canEdit": true}}]})
                .to_string(),
        ))
        .mount(&env.server)
        .await;
    let entries = playlist_folder(&env.api, None).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0],
        PlaylistEntry::Folder {
            id: "p.F1".into(),
            name: "Moods".into()
        }
    );
    assert!(
        matches!(&entries[1], PlaylistEntry::Playlist { id, editable: true, .. } if id == "p.P1")
    );
    assert_eq!(
        entries[1].route(),
        Some(Route::LibraryPlaylist { id: "p.P1".into() })
    );
    // An empty folder answers 404.
    assert_eq!(
        playlist_folder(&env.api, Some("p.empty")).await,
        Ok(Vec::new())
    );
}

#[test]
fn actions_name_the_pages_they_change() {
    let add = Action::AddToLibrary(vec![ResourceRef::new(ResourceType::Albums, "1")]);
    assert!(add.changes(&Route::Library(LibrarySection::Albums)));
    assert!(add.changes(&Route::LibraryPlaylist { id: "p.1".into() }));
    assert!(
        !add.changes(&Route::Album { id: "1".into() }),
        "catalog pages do not show the library"
    );
    let reorder = Action::SetPlaylistOrder {
        id: "p.1".into(),
        tracks: Vec::new(),
    };
    assert!(reorder.changes(&Route::LibraryPlaylist { id: "p.1".into() }));
    assert!(!reorder.changes(&Route::LibraryPlaylist { id: "p.2".into() }));
    assert!(reorder.changes(&Route::Library(LibrarySection::Playlists)));
    let love = Action::Rate {
        item: ResourceRef::new(ResourceType::Songs, "1"),
        rating: Some(Rating::Love),
    };
    assert!(!love.changes(&Route::Library(LibrarySection::Songs)));
}
