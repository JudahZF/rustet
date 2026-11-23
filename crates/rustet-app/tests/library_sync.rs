//! Library sync against a fake Apple Music API: what lands in the cache, resuming after an
//! interruption, and reusing unchanged playlists.

mod pages_common;

use pages_common::{Env, env, json, signed_in};
use rustet_app::library::{Library, LibrarySync, SyncError, SyncEvent, SyncPhase};
use rustet_app::pages::LoadError;
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, ResponseTemplate};

fn song(id: &str, cid: &str, album: &str, disc: u32, track: u32) -> serde_json::Value {
    json!({"id": id, "type": "library-songs", "attributes": {
        "name": format!("Song {id}"), "artistName": "Artist", "albumName": "Album",
        "durationInMillis": 1000, "trackNumber": track, "discNumber": disc,
        "playParams": {"id": id, "kind": "song", "isLibrary": true, "catalogId": cid}},
      "relationships": {"albums": {"data": [{"id": album, "type": "library-albums"}]},
                        "catalog": {"data": [{"id": cid, "type": "songs"}]}}})
}

fn entry(id: &str, cid: &str) -> serde_json::Value {
    json!({"id": id, "type": "library-songs", "attributes": {"name": id,
        "playParams": {"id": id, "kind": "song", "isLibrary": true, "catalogId": cid}}})
}

fn playlist(id: &str, name: &str, modified: &str) -> serde_json::Value {
    json!({"id": id, "type": "library-playlists",
           "attributes": {"name": name, "canEdit": true, "lastModifiedDate": modified}})
}

/// A first page (no `offset`) answered with `data` and `next`.
async fn first_page(env: &Env, route: &str, data: serde_json::Value, next: Option<&str>) {
    Mock::given(method("GET"))
        .and(path(route))
        .and(query_param_is_missing("offset"))
        .respond_with(json(json!({"data": data, "next": next}).to_string()))
        .mount(&env.server)
        .await;
}

async fn page_at(env: &Env, route: &str, offset: &str, data: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(route))
        .and(query_param("offset", offset))
        .respond_with(json(json!({"data": data}).to_string()))
        .mount(&env.server)
        .await;
}

/// A library of three songs on two pages, one album, two playlists (one empty).
async fn library_api(env: &Env, modified: &str) {
    first_page(
        env,
        "/v1/me/library/songs",
        json!([
            song("i.2", "102", "l.A", 1, 2),
            song("i.1", "101", "l.A", 1, 1)
        ]),
        Some("/v1/me/library/songs?offset=2"),
    )
    .await;
    page_at(
        env,
        "/v1/me/library/songs",
        "2",
        json!([song("i.3", "103", "l.A", 2, 1)]),
    )
    .await;
    first_page(
        env,
        "/v1/me/library/albums",
        json!([{"id": "l.A", "type": "library-albums", "attributes": {"name": "Album", "trackCount": 3}}]),
        None,
    )
    .await;
    first_page(
        env,
        "/v1/me/library/playlists",
        json!([
            playlist("p.1", "zebra", modified),
            playlist("p.2", "Alpha", modified)
        ]),
        None,
    )
    .await;
    first_page(
        env,
        "/v1/me/library/playlists/p.1/tracks",
        json!([entry("i.3", "103")]),
        Some("/v1/me/library/playlists/p.1/tracks?offset=1"),
    )
    .await;
    page_at(
        env,
        "/v1/me/library/playlists/p.1/tracks",
        "1",
        json!([entry("i.1", "101")]),
    )
    .await;
    // Apple answers 404 for the tracks of an empty playlist.
    Mock::given(method("GET"))
        .and(path("/v1/me/library/playlists/p.2/tracks"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&env.server)
        .await;
}

#[tokio::test]
async fn a_sync_fills_the_cache_and_reports_progress() {
    let env = signed_in().await;
    library_api(&env, "2026-01-01T00:00:00Z").await;
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open_in(dir.path()).await.unwrap();
    assert!(library.status().await.unwrap().needs_sync);

    let mut events = Vec::new();
    let stats = library
        .sync(&env.api, &mut LibrarySync::default(), |e| events.push(e))
        .await
        .unwrap();
    assert_eq!((stats.songs, stats.albums, stats.playlists), (3, 1, 2));
    assert!(events.contains(&SyncEvent::Fetched {
        phase: SyncPhase::Songs,
        fetched: 3
    }));
    assert!(matches!(events.last(), Some(SyncEvent::Saved(s)) if *s == stats));

    let store = library.store();
    let tracks: Vec<_> = store.playlist_tracks("p.1").await.unwrap();
    let order: Vec<_> = tracks
        .iter()
        .map(|t| (t.library_id.as_str(), t.catalog_id.as_str()))
        .collect();
    assert_eq!(order, [("i.3", "103"), ("i.1", "101")]);
    let album: Vec<_> = store.songs_by_album("l.A").await.unwrap();
    let album: Vec<_> = album.iter().map(|t| t.library_id.as_str()).collect();
    assert_eq!(album, ["i.1", "i.2", "i.3"], "disc, then track number");
    let names: Vec<_> = library
        .playlists()
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.name, p.track_count))
        .collect();
    assert_eq!(names, [("Alpha".to_owned(), 0), ("zebra".to_owned(), 2)]);
    assert!(!library.status().await.unwrap().needs_sync);

    // The cache is on disk where the engine kept it: a new process reads it back.
    drop(library);
    assert!(dir.path().join("library.enc").exists() && dir.path().join("library.key").exists());
    let reopened = Library::open_in(dir.path()).await.unwrap();
    assert_eq!(reopened.status().await.unwrap().stats.songs, 3);
}

#[tokio::test]
async fn an_interrupted_sync_resumes_at_the_page_that_failed() {
    let env = signed_in().await;
    // The second songs page fails once.
    Mock::given(method("GET"))
        .and(path("/v1/me/library/songs"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&env.server)
        .await;
    library_api(&env, "2026-01-01T00:00:00Z").await;
    // The first songs page is fetched once over both runs.
    Mock::given(method("GET"))
        .and(path("/v1/me/library/songs"))
        .and(query_param_is_missing("offset"))
        .respond_with(json(
            json!({"data": [song("i.2", "102", "l.A", 1, 2), song("i.1", "101", "l.A", 1, 1)],
                   "next": "/v1/me/library/songs?offset=2"})
            .to_string(),
        ))
        .with_priority(1)
        .expect(1)
        .mount(&env.server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open_in(dir.path()).await.unwrap();
    let mut sync = LibrarySync::default();

    let err = library.sync(&env.api, &mut sync, |_| {}).await.unwrap_err();
    assert!(
        matches!(err, SyncError::Fetch(LoadError::Apple(_))),
        "{err}"
    );
    assert!(sync.is_partial());
    assert_eq!(
        library.status().await.unwrap().stats.songs,
        0,
        "nothing saved yet"
    );

    let stats = library.sync(&env.api, &mut sync, |_| {}).await.unwrap();
    assert_eq!(stats.songs, 3);
    assert!(!sync.is_partial(), "reset after the save");
}

#[tokio::test]
async fn unchanged_playlists_keep_their_cached_tracks() {
    let env = signed_in().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/library/playlists/p.1/tracks"))
        .and(query_param_is_missing("offset"))
        .respond_with(json(
            json!({"data": [entry("i.3", "103")], "next": "/v1/me/library/playlists/p.1/tracks?offset=1"})
                .to_string(),
        ))
        .with_priority(1)
        .expect(1) // the first sync only
        .mount(&env.server)
        .await;
    library_api(&env, "2026-01-01T00:00:00Z").await;
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open_in(dir.path()).await.unwrap();

    library
        .sync(&env.api, &mut LibrarySync::default(), |_| {})
        .await
        .unwrap();
    let mut events = Vec::new();
    library
        .sync(&env.api, &mut LibrarySync::default(), |e| events.push(e))
        .await
        .unwrap();
    assert!(
        events.iter().any(
            |e| matches!(e, SyncEvent::Playlist { name, reused: true, .. } if name == "zebra")
        )
    );
    let tracks = library.store().playlist_tracks("p.1").await.unwrap();
    assert_eq!(tracks.len(), 2, "the cached order survives the replace");

    // A new modification date fetches the tracks again.
    env.server.verify().await;
    env.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v1/me/library/playlists/p.1/tracks"))
        .and(query_param_is_missing("offset"))
        .respond_with(json(json!({"data": [entry("i.2", "102")]}).to_string()))
        .with_priority(1)
        .expect(1)
        .mount(&env.server)
        .await;
    library_api(&env, "2026-02-01T00:00:00Z").await;
    library
        .sync(&env.api, &mut LibrarySync::default(), |_| {})
        .await
        .unwrap();
    let tracks = library.store().playlist_tracks("p.1").await.unwrap();
    assert_eq!(
        tracks
            .iter()
            .map(|t| t.library_id.as_str())
            .collect::<Vec<_>>(),
        ["i.2"]
    );
}

#[tokio::test]
async fn signed_out_sync_fails_without_a_request() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&env.server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open_in(dir.path()).await.unwrap();
    let err = library
        .sync(&env.api, &mut LibrarySync::default(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, SyncError::Fetch(LoadError::SignInRequired)));
}
