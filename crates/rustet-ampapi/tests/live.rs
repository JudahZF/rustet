//! Calls Apple's servers, one request per endpoint, a second apart. They print only
//! status classes and counts, never content or tokens.
//!
//! Signed out (scrapes the developer token):
//!   cargo test -p rustet-ampapi --test live -- --ignored --nocapture live_signed_out
//!
//! Signed in (the music user token and its storefront from the environment):
//!   RUSTET_TEST_MUT=… RUSTET_TEST_STOREFRONT=us cargo test -p rustet-ampapi --test live \
//!     -- --ignored --nocapture --test-threads=1 live_signed_in
//! `live_signed_in_reads` only reads. `live_signed_in_writes` creates a playlist named
//! "Rustet API test", edits it and deletes it, and rates a song, then restores its rating.
//! `live_signed_in_play_activity` reports one skipped play of `RUSTET_TEST_SONG` (default:
//! a song in the `us` storefront) and so appears in Recently Played.

use std::fmt::Debug;
use std::future::Future;
use std::time::Duration;

use futures::StreamExt;
use rustet_ampapi::api::{
    EndReason, History, ItemId, LibraryContainer, LibraryKind, MediaKind, NewPlaylist, PlayEvent,
    PlayMode, PlayPhase, PlayedItem, PlaylistEdit, Rating, Repeat, ResourceRef, ResourceType,
    UserToken,
};
use rustet_ampapi::model::{Page, Resource};
use rustet_ampapi::{Api, ArtistView, BrowsePage, Client, Error, Locale};
use tokio_util::sync::CancellationToken;

/// Collects the outcome of each step; the test fails at the end if any step failed.
struct Run {
    cancel: CancellationToken,
    failures: Vec<String>,
}

fn outcome(err: &Error) -> String {
    match err {
        Error::Status(s) | Error::Unauthorized(s) | Error::DeveloperTokenRejected(s) => {
            format!("{:?} (HTTP {})", err.kind(), s.as_u16())
        }
        _ => format!("{:?}", err.kind()),
    }
}

impl Run {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            failures: Vec::new(),
        }
    }

    /// Runs one request, prints `name: ok, <summary>` or the error class, waits a second.
    async fn step<T, F>(
        &mut self,
        name: &str,
        request: F,
        summary: impl Fn(&T) -> String,
    ) -> Option<T>
    where
        F: Future<Output = Result<T, Error>>,
    {
        let result = request.await;
        let out = match &result {
            Ok(value) => format!("ok, {}", summary(value)),
            Err(err) => {
                self.failures.push(name.to_owned());
                format!("FAILED: {}", outcome(err))
            }
        };
        println!("{name:<28} {out}");
        tokio::time::sleep(Duration::from_secs(1)).await;
        result.ok()
    }

    fn finish(self) {
        assert!(self.failures.is_empty(), "failed: {:?}", self.failures);
    }
}

fn count<T: Debug>(items: &[T]) -> String {
    format!("{} items", items.len())
}

fn page_count(page: &Page<Resource>) -> String {
    format!("{} items, next: {}", page.data.len(), page.next.is_some())
}

fn signed_in_api() -> Api {
    let token = std::env::var("RUSTET_TEST_MUT").expect("RUSTET_TEST_MUT");
    let storefront = std::env::var("RUSTET_TEST_STOREFRONT").unwrap_or_else(|_| "us".into());
    let api = Api::new(
        Client::new(reqwest::Client::new()),
        Locale::new(storefront, "en-US"),
    );
    api.set_user_token(Some(UserToken::new(token)));
    api
}

#[tokio::test]
#[ignore = "calls Apple's servers"]
async fn live_signed_out() {
    let api = Api::new(Client::new(reqwest::Client::new()), Locale::default());
    let mut run = Run::new();
    let c = run.cancel.clone();

    let token = run
        .step("developer token", api.developer_token(&c), |t| {
            format!(
                "expires {:?}",
                t.expires_at()
                    .map(|e| e.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs())
            )
        })
        .await;
    assert!(token.is_some(), "no developer token");

    run.step("browse New", api.browse(&c, BrowsePage::New), |p| {
        let unknown = p
            .shelves
            .iter()
            .filter(|s| matches!(s, rustet_ampapi::model::Shelf::Unknown { .. }))
            .count();
        format!("{} shelves, {unknown} unknown", p.shelves.len())
    })
    .await;
    run.step("browse Radio", api.browse(&c, BrowsePage::Radio), |p| {
        let unknown = p
            .shelves
            .iter()
            .filter(|s| matches!(s, rustet_ampapi::model::Shelf::Unknown { .. }))
            .count();
        format!("{} shelves, {unknown} unknown", p.shelves.len())
    })
    .await;
    run.step("live stations", api.live_stations(&c), |s| count(s))
        .await;
    run.step("search landing", api.search_landing(&c), |s| count(s))
        .await;
    run.step(
        "search suggestions",
        api.search_suggestions(&c, "phoebe"),
        |s| count(s),
    )
    .await;
    let search = run
        .step("search", api.search(&c, "radiohead"), |r| {
            format!("{} groups", r.groups.len())
        })
        .await;
    if let Some(next) = search
        .as_ref()
        .and_then(|r| r.groups.iter().find_map(|g| g.next.clone()))
    {
        run.step("search next page", api.search_next(&c, &next), |r| {
            let ids: Vec<_> = r
                .groups
                .iter()
                .map(|g| format!("{}:{}", g.group_id, g.data.len()))
                .collect();
            ids.join(" ")
        })
        .await;
    }
    run.step("artist", api.artist(&c, "697833299"), |a| {
        let v = &a.views;
        let n = [
            &v.top_songs,
            &v.latest_release,
            &v.full_albums,
            &v.singles,
            &v.appears_on_albums,
            &v.top_music_videos,
            &v.featured_playlists,
            &v.similar_artists,
        ]
        .iter()
        .filter(|v| v.is_some())
        .count();
        format!(
            "{n}/8 views, bio: {}",
            a.attributes
                .as_ref()
                .is_some_and(|a| a.artist_bio.is_some())
        )
    })
    .await;
    let view = run
        .step(
            "artist see-all",
            api.artist_view(&c, "697833299", ArtistView::Singles),
            |v| format!("{} items, next: {}", v.data.len(), v.next.is_some()),
        )
        .await;
    if let Some(next) = view.and_then(|v| v.next) {
        run.step(
            "artist see-all next page",
            api.next_page::<Resource>(&c, &next),
            page_count,
        )
        .await;
    }
    run.step("album", api.album(&c, "1889568617"), |a| {
        let tracks = a.relationships.tracks.as_ref().map_or(0, |t| t.data.len());
        format!("{tracks} tracks")
    })
    .await;
    let id = "pl.f083772167b24f5a9d691e3c3e16f968";
    run.step("playlist", api.playlist(&c, id), |p| {
        let tracks = p.relationships.tracks.as_ref().map_or(0, |t| t.data.len());
        format!("{tracks} tracks")
    })
    .await;
    run.step(
        "playlist tracks",
        api.playlist_tracks(&c, id, 20),
        page_count,
    )
    .await;
    run.step("song", api.song(&c, "6781051513"), |s| {
        let credits = s.relationships.credits.as_ref().map_or(0, |c| c.data.len());
        format!("{credits} credit groups")
    })
    .await;
    run.step("music video", api.music_video(&c, "6834539294"), |m| {
        format!("4K: {}", m.attributes.as_ref().is_some_and(|a| a.has_4k))
    })
    .await;
    run.step("station", api.station(&c, "ra.978194965"), |s| {
        format!("live: {}", s.attributes.as_ref().is_some_and(|a| a.is_live))
    })
    .await;
    run.step("room", api.room(&c, "6834266064"), |r| {
        format!(
            "{} items",
            r.relationships
                .contents
                .as_ref()
                .map_or(0, |c| c.data.len())
        )
    })
    .await;
    run.step("curator", api.curator(&c, "976439548"), |cur| {
        format!(
            "{} playlists",
            cur.relationships
                .playlists
                .as_ref()
                .map_or(0, |p| p.data.len())
        )
    })
    .await;
    run.step("charts", api.charts(&c, Some("34")), |ch| {
        format!("{} song charts", ch.songs.len())
    })
    .await;
    run.step("genres", api.genres(&c), |g| count(g)).await;
    run.step("storefront", api.storefront(&c, "gb"), |s| s.id.clone())
        .await;
    run.step("storefronts", api.storefronts(&c), |s| count(s))
        .await;
    // Served from the response cache: no request.
    run.step("genres again (cached)", api.genres(&c), |g| count(g))
        .await;

    // Does Apple answer 304 to If-None-Match? (Not cached by the client for max-age.)
    if let Some(token) = token {
        let http = reqwest::Client::new();
        let get = |etag: Option<&str>| {
            let mut r = http
                .get("https://amp-api.music.apple.com/v1/storefronts/gb")
                .bearer_auth(token.secret())
                .header("origin", rustet_ampapi::WEB_ORIGIN);
            if let Some(etag) = etag {
                r = r.header("if-none-match", etag);
            }
            r.send()
        };
        let first = get(None).await.unwrap();
        let etag = first
            .headers()
            .get("etag")
            .map(|v| v.to_str().unwrap().to_owned());
        println!(
            "{:<28} HTTP {}, etag: {}",
            "storefront (raw)",
            first.status().as_u16(),
            etag.is_some()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(etag) = etag {
            let again = get(Some(&etag)).await.unwrap();
            println!(
                "{:<28} HTTP {}",
                "storefront If-None-Match",
                again.status().as_u16()
            );
        }
    }
    run.finish();
}

#[tokio::test]
#[ignore = "needs RUSTET_TEST_MUT; calls Apple's servers"]
async fn live_signed_in_reads() {
    let api = signed_in_api();
    let mut run = Run::new();
    let c = run.cancel.clone();

    run.step("account storefront", api.account_storefront(&c), |s| {
        s.id.clone()
    })
    .await;
    for kind in [
        LibraryKind::Songs,
        LibraryKind::Albums,
        LibraryKind::Artists,
        LibraryKind::Playlists,
        LibraryKind::MusicVideos,
    ] {
        let first = run
            .step(
                &format!("library {kind:?}"),
                api.library(&c, kind),
                page_count,
            )
            .await;
        if kind == LibraryKind::Songs
            && let Some(first) = first
        {
            // Two pages at most: enough to prove the cursor, no full crawl.
            let pages: Vec<_> = api.pages(&c, first).take(2).collect().await;
            let ok = pages.iter().all(Result::is_ok);
            println!(
                "{:<28} {} pages, all ok: {ok}",
                "library songs pages",
                pages.len()
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    run.step("recently added", api.library_recently_added(&c), page_count)
        .await;
    let folders = run
        .step(
            "playlist folders",
            api.library_playlist_folder(&c, None),
            page_count,
        )
        .await;
    let playlist = folders.and_then(|f| {
        f.data.into_iter().find_map(|r| match r {
            Resource::LibraryPlaylist(p) => Some(p.id),
            _ => None,
        })
    });
    if let Some(id) = playlist {
        run.step(
            "library playlist",
            api.library_item(&c, LibraryKind::Playlists, &id),
            |r| r.kind().to_owned(),
        )
        .await;
        run.step(
            "library playlist tracks",
            api.library_tracks(&c, LibraryContainer::Playlist, &id),
            page_count,
        )
        .await;
    }
    run.step("library search", api.library_search(&c, "a"), |r| {
        format!("songs: {}", r.songs.as_ref().map_or(0, |s| s.data.len()))
    })
    .await;
    run.step("home", api.home(&c), |s| count(s)).await;
    for history in [
        History::RecentlyPlayed,
        History::RecentTracks,
        History::RecentStations,
        History::HeavyRotation,
    ] {
        run.step(
            &format!("history {history:?}"),
            api.history(&c, history),
            page_count,
        )
        .await;
    }
    run.step(
        "rating (catalog song)",
        api.rating(&c, &ResourceRef::new(ResourceType::Songs, "6781051513")),
        |r| format!("{r:?}"),
    )
    .await;
    run.step(
        "station next tracks",
        api.station_next_tracks(&c, "ra.978194965", 5),
        |t| count(t),
    )
    .await;
    let seeds = [ResourceRef::new(ResourceType::Songs, "6781051513")];
    run.step("autoplay", api.autoplay(&c, &seeds, 5), |a| {
        format!(
            "station: {}, {} tracks",
            a.station.is_some(),
            a.tracks.len()
        )
    })
    .await;
    run.finish();
}

#[tokio::test]
#[ignore = "needs RUSTET_TEST_MUT; changes the account, then restores it"]
async fn live_signed_in_writes() {
    let api = signed_in_api();
    let mut run = Run::new();
    let c = run.cancel.clone();
    let song = std::env::var("RUSTET_TEST_SONG").unwrap_or_else(|_| "6781051513".into());
    let other = std::env::var("RUSTET_TEST_SONG2").unwrap_or_else(|_| "1889568624".into());

    let new = NewPlaylist {
        name: "Rustet API test".into(),
        description: Some("Created by an rustet-ampapi test; safe to delete.".into()),
        tracks: vec![ResourceRef::new(ResourceType::Songs, &song)],
    };
    let created = run
        .step("create playlist", api.create_playlist(&c, &new), |p| {
            p.id.clone()
        })
        .await;
    if let Some(playlist) = created {
        let id = playlist.id;
        let edit = PlaylistEdit {
            name: Some("Rustet API test (renamed)".into()),
            description: None,
        };
        run.step("rename playlist", api.edit_playlist(&c, &id, &edit), |()| {
            String::new()
        })
        .await;
        run.step(
            "add playlist track",
            api.add_playlist_tracks(&c, &id, &[ResourceRef::new(ResourceType::Songs, &other)]),
            |()| String::new(),
        )
        .await;
        let tracks = run
            .step(
                "library playlist tracks",
                api.library_tracks(&c, LibraryContainer::Playlist, &id),
                page_count,
            )
            .await;
        let mut library_ids: Vec<_> = tracks
            .map(|t| t.data.iter().map(|r| r.id().to_owned()).collect())
            .unwrap_or_default();
        if library_ids.len() == 2 {
            library_ids.reverse();
            let order: Vec<_> = library_ids
                .iter()
                .map(|i| ResourceRef::new(ResourceType::LibrarySongs, i))
                .collect();
            run.step(
                "reorder playlist",
                api.set_playlist_tracks(&c, &id, &order),
                |()| String::new(),
            )
            .await;
            run.step(
                "remove playlist track",
                api.remove_playlist_track(&c, &id, &library_ids[0]),
                |()| String::new(),
            )
            .await;
        }
        run.step(
            "delete playlist",
            api.remove_from_library(&c, &ResourceRef::new(ResourceType::Playlists, &id)),
            |()| String::new(),
        )
        .await;
    }

    let target = ResourceRef::new(ResourceType::Songs, &song);
    if let Some(before) = run
        .step("rating before", api.rating(&c, &target), |r| {
            format!("{r:?}")
        })
        .await
    {
        run.step(
            "set rating (love)",
            api.set_rating(&c, &target, Rating::Love),
            |()| String::new(),
        )
        .await;
        match before {
            Some(rating) => {
                run.step(
                    "restore rating",
                    api.set_rating(&c, &target, rating),
                    |()| String::new(),
                )
                .await
            }
            None => {
                run.step("clear rating", api.clear_rating(&c, &target), |()| {
                    String::new()
                })
                .await
            }
        };
    }
    run.finish();
}

#[tokio::test]
#[ignore = "needs RUSTET_TEST_MUT; reports a play to the account's history"]
async fn live_signed_in_play_activity() {
    let api = signed_in_api();
    let mut run = Run::new();
    let c = run.cancel.clone();
    let song = std::env::var("RUSTET_TEST_SONG").unwrap_or_else(|_| "6781051513".into());
    let started = std::time::SystemTime::now() - Duration::from_secs(40);
    let event = |phase, at| PlayEvent {
        item: PlayedItem {
            id: ItemId::Catalog(song.clone()),
            media: MediaKind::Audio,
        },
        container: None,
        phase,
        duration: Duration::from_secs(200),
        mode: PlayMode {
            shuffle: false,
            repeat: Repeat::Off,
            autoplay: false,
        },
        at,
        utc_offset_seconds: Some(0),
        reco_data: None,
    };
    let events = [
        event(
            PlayPhase::Start {
                position: Duration::ZERO,
                container_changed: true,
            },
            started,
        ),
        event(
            PlayPhase::End {
                start: Duration::ZERO,
                end: Duration::from_secs(35),
                reason: EndReason::SkippedForwards,
            },
            started + Duration::from_secs(35),
        ),
    ];
    run.step("report plays", api.report_plays(&c, &events), |()| {
        String::new()
    })
    .await;
    // Apple may take minutes to list it; this only shows whether the history answers.
    run.step(
        "recent tracks",
        api.history(&c, History::RecentTracks),
        page_count,
    )
    .await;
    run.finish();
}
