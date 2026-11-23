use std::path::Path;

use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::api::{AmRef, fetch_playlist_tracks};

fn song(id: &str, cid: &str, album: &str, disc: u32, track: u32) -> AmItem {
    let mut it = AmItem {
        id: id.into(),
        ..Default::default()
    };
    it.attributes.name = format!("Song {id}");
    it.attributes.play_params.catalog_id = cid.into();
    it.attributes.track_number = track;
    it.attributes.disc_number = disc;
    it.relationships
        .albums
        .data
        .push(AmRef { id: album.into() });
    it
}

fn lids(tracks: &[PlaylistTrack]) -> Vec<&str> {
    tracks.iter().map(|t| t.library_id.as_str()).collect()
}

/// Album ordering, the albums table and the sync revision must survive a save/load
/// cycle, that is an engine restart.
#[tokio::test]
async fn persistence_round_trip() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).await.unwrap();
    let mut album = AmItem {
        id: "l.album1".into(),
        ..Default::default()
    };
    album.attributes.name = "Album".into();
    album.attributes.track_count = 3;
    let mut pl = AmItem {
        id: "p.1".into(),
        ..Default::default()
    };
    pl.attributes.name = "Mix".into();

    s.ingest(IngestPayload {
        songs: vec![
            song("i.3", "103", "l.album1", 2, 1),
            song("i.1", "101", "l.album1", 1, 1),
            song("i.2", "102", "l.album1", 1, 2),
            song("i.2", "102", "l.album1", 1, 2), // a duplicate ID must not abort the ingest
        ],
        albums: vec![album],
        playlists: vec![pl],
        playlist_tracks: BTreeMap::from([("p.1".into(), vec![song("i.2", "102", "", 0, 0)])]),
        revision: "rev-42".into(),
    })
    .await
    .unwrap();

    let r = Store::open(dir.path()).await.unwrap();
    assert_eq!(
        lids(&r.songs_by_album("l.album1").await.unwrap()),
        ["i.1", "i.2", "i.3"]
    );
    let stats = r.stats().await.unwrap();
    assert_eq!((stats.songs, stats.albums, stats.playlists), (3, 1, 1));
    assert!(stats.synced_at.is_some());
    assert!(!r.needs_sync().await);
    let rev: String = r
        .inner
        .conn()
        .query_row("SELECT value FROM meta WHERE key='revision'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rev, "rev-42");
    let tracks = r.playlist_tracks("p.1").await.unwrap();
    assert_eq!(
        tracks,
        [PlaylistTrack {
            library_id: "i.2".into(),
            catalog_id: "102".into()
        }]
    );
    assert!(
        !dir.path().join("library.enc.tmp").exists(),
        "temp file left behind after save"
    );
}

/// A cache written by the Go engine must load without a re-sync.
#[tokio::test]
async fn opens_cache_written_by_go_engine() {
    let dir = TempDir::new().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/go-cache");
    for name in ["library.key", "library.enc"] {
        fs::copy(fixture.join(name), dir.path().join(name)).unwrap();
    }
    let s = Store::open(dir.path()).await.unwrap();
    let stats = s.stats().await.unwrap();
    assert_eq!((stats.songs, stats.albums, stats.playlists), (3, 1, 1));
    assert_eq!(
        stats
            .synced_at
            .unwrap()
            .to_rfc3339_opts(SecondsFormat::Secs, true),
        "2026-11-19T09:22:23Z"
    );
    // `i.1` has no playParams catalogId: Go took playParams.id before the catalog relationship.
    let album = s.songs_by_album("l.album1").await.unwrap();
    assert_eq!(
        album,
        [
            PlaylistTrack {
                library_id: "i.1".into(),
                catalog_id: "i.1".into()
            },
            PlaylistTrack {
                library_id: "i.2".into(),
                catalog_id: "102".into()
            },
        ]
    );
    let tracks = s.playlist_tracks("p.1").await.unwrap();
    assert_eq!(
        tracks,
        [
            PlaylistTrack {
                library_id: "i.3".into(),
                catalog_id: "103".into()
            },
            PlaylistTrack {
                library_id: "i.1".into(),
                catalog_id: "101".into()
            },
        ]
    );
    assert_eq!(
        s.playlists().await.unwrap(),
        [PlaylistInfo {
            library_id: "p.1".into(),
            name: "Mix".into(),
            track_count: 2
        }]
    );
}

#[tokio::test]
async fn load_rejects_tampered_cache() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).await.unwrap();
    s.inner
        .conn()
        .execute(
            "INSERT INTO songs(lid,cid,name,artist,album,ms) VALUES('i.1','1','n','a','b',1)",
            [],
        )
        .unwrap();
    s.inner.save();
    let enc = dir.path().join("library.enc");
    let mut b = fs::read(&enc).unwrap();
    *b.last_mut().unwrap() ^= 0xff;
    fs::write(&enc, b).unwrap();
    assert_eq!(
        Store::open(dir.path())
            .await
            .unwrap()
            .stats()
            .await
            .unwrap()
            .songs,
        0
    );
}

/// A dump written by a newer layout is ignored instead of half-loaded.
#[tokio::test]
async fn load_ignores_newer_cache_format() {
    let dir = TempDir::new().unwrap();
    let s = Store::open(dir.path()).await.unwrap();
    let enc = s
        .inner
        .key
        .encrypt(br#"{"version":99,"songs":[{"lid":"x","name":"n"}]}"#)
        .unwrap();
    fs::write(dir.path().join("library.enc"), enc).unwrap();
    assert_eq!(
        Store::open(dir.path())
            .await
            .unwrap()
            .stats()
            .await
            .unwrap()
            .songs,
        0
    );
}

/// A key file that is unreadable for any reason other than "absent" must not be replaced:
/// minting a new key would overwrite the real one and orphan the encrypted cache.
#[cfg(unix)]
#[test]
fn key_is_not_regenerated_on_read_failure() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let key_path = dir.path().join("library.key");
    let first = KeyFile::new(key_path.clone()).key().unwrap();
    assert_eq!(
        fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&key_path).is_ok() {
        return; // root: file permissions are not enforced
    }
    // A fresh KeyFile, as in a new process: no cached key.
    assert!(KeyFile::new(key_path.clone()).key().is_err());
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fs::read(&key_path).unwrap(),
        first,
        "the original key was overwritten"
    );
}

#[test]
fn key_creation_is_exclusive() {
    let dir = TempDir::new().unwrap();
    let key_path = dir.path().join("library.key");
    let keys: Vec<_> = std::thread::scope(|s| {
        let racers: Vec<_> = (0..8)
            .map(|_| s.spawn(|| KeyFile::new(key_path.clone()).key().unwrap()))
            .collect();
        racers.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(
        keys.iter().all(|k| *k == keys[0]),
        "concurrent starts ended up with different keys"
    );
}

/// The renderer reads these shapes; `syncedAt` must be Go's zero time when never synced.
#[test]
fn wire_format() {
    let stats = LibraryStats {
        songs: 1,
        albums: 2,
        playlists: 3,
        synced_at: None,
    };
    assert_eq!(
        serde_json::to_value(stats).unwrap(),
        json!({"songs":1,"albums":2,"playlists":3,"syncedAt":"0001-01-01T00:00:00Z"})
    );
    let tracks = [
        PlaylistTrack {
            library_id: "i.1".into(),
            catalog_id: "1".into(),
        },
        PlaylistTrack {
            library_id: "i.2".into(),
            catalog_id: String::new(),
        },
    ];
    assert_eq!(
        serde_json::to_value(tracks).unwrap(),
        json!([{"lid":"i.1","cid":"1"},{"lid":"i.2"}])
    );
    let pl = PlaylistInfo {
        library_id: "p.1".into(),
        name: "Mix".into(),
        track_count: 4,
    };
    assert_eq!(
        serde_json::to_value(pl).unwrap(),
        json!({"lid":"p.1","name":"Mix","trackCount":4})
    );
}

/// The renderer's album fallback posts `"playlists": {}`. That must fail to decode (as in
/// Go, where the handler answers 400) rather than wipe the library with a partial ingest.
#[test]
fn ingest_payload_rejects_object_for_list() {
    let r = serde_json::from_str::<IngestPayload>(
        r#"{"songs":[],"albums":[],"playlists":{},"playlistTracks":{}}"#,
    );
    assert!(r.is_err());
    let p: IngestPayload = serde_json::from_str(
        r#"{"songs":[{"id":"i.1","attributes":{"name":null}}],"albums":null}"#,
    )
    .unwrap();
    assert_eq!(p.songs[0].id, "i.1");
}

#[tokio::test]
async fn fetch_playlist_tracks_repeated_cursor_terminates() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":[{"id":"i.1"}],"next":"/v1/me/library/playlists/p.1/tracks?offset=100&limit=100"}),
        ))
        .up_to_n_times(5)
        .mount(&srv)
        .await;
    let r = fetch_playlist_tracks(
        &reqwest::Client::new(),
        &CancellationToken::new(),
        &srv.uri(),
        "t",
        "m",
        "p.1",
    )
    .await;
    assert!(matches!(r, Err(FetchError::Pagination { .. })), "{r:?}");
    assert!(srv.received_requests().await.unwrap().len() <= 5);
}

#[tokio::test]
async fn fetch_playlist_tracks_follows_pages() {
    let srv = MockServer::start().await;
    Mock::given(path("/v1/me/library/playlists/p.1/tracks"))
        .and(query_param("offset", "100"))
        .and(query_param("limit", "100")) // re-attached on page 2+
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"data":[{"id":"i.2","attributes":{"playParams":{"id":"2"}}}]}),
            ),
        )
        .mount(&srv)
        .await;
    Mock::given(path("/v1/me/library/playlists/p.1/tracks"))
        .and(query_param("limit", "100"))
        .and(header("authorization", "Bearer tok"))
        .and(header("music-user-token", "mut"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data":[{"id":"i.1","attributes":{"playParams":{"id":"i.1","catalogId":"1"}}}],
            "next":"/v1/me/library/playlists/p.1/tracks?offset=100"
        })))
        .mount(&srv)
        .await;
    let got = fetch_playlist_tracks(
        &reqwest::Client::new(),
        &CancellationToken::new(),
        &srv.uri(),
        "tok",
        "mut",
        "p.1",
    )
    .await
    .unwrap();
    assert_eq!(
        got,
        [
            PlaylistTrack {
                library_id: "i.1".into(),
                catalog_id: "1".into()
            },
            PlaylistTrack {
                library_id: "i.2".into(),
                catalog_id: "2".into()
            },
        ]
    );
}

#[tokio::test]
async fn fetch_playlist_tracks_reports_http_status() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&srv)
        .await;
    let r = fetch_playlist_tracks(
        &reqwest::Client::new(),
        &CancellationToken::new(),
        &srv.uri(),
        "t",
        "m",
        "p.1",
    )
    .await;
    assert!(
        matches!(r, Err(FetchError::Status { status, .. }) if status == 401),
        "{r:?}"
    );
}
