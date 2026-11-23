//! Loads pages from Apple's servers and prints only counts, never content or tokens.
//!
//! Signed out (scrapes the developer token; about 20 requests):
//!   cargo test -p rustet-app --test pages_live -- --ignored --nocapture live_signed_out
//!
//! Signed in (the music user token and its storefront from the environment):
//!   RUSTET_TEST_MUT=… RUSTET_TEST_STOREFRONT=us cargo test -p rustet-app --test pages_live \
//!     -- --ignored --nocapture --test-threads=1 live_signed_in
//! `live_signed_in_reads` only reads (Home, every library section, a library playlist and
//! album, library search, lyrics of `RUSTET_TEST_SONG`, the playlist folders (top level and the
//! first folder), a library sync into a temporary directory). `live_signed_in_writes` creates a playlist named "Rustet app test", renames it,
//! adds, reorders and removes a song, deletes it, and loves `RUSTET_TEST_SONG`, then restores
//! its rating.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;

use rustet_ampapi::api::{ResourceRef, ResourceType, UserToken};
use rustet_ampapi::{Api, ArtistView, Client, Locale};
use rustet_app::library::{Library, LibrarySync, PlaylistEntry, playlist_folder};
use rustet_app::pages::actions::{self, Action};
use rustet_app::pages::model::Listing;
use rustet_app::pages::{self, Page, catalog, lyrics, user};
use rustet_app::route::{LibrarySection, Route};

struct Run {
    failures: Vec<String>,
}

impl Run {
    /// Runs one step, prints `name: ok, <summary>` or the error, then pauses.
    async fn step<T, E: Display>(
        &mut self,
        name: &str,
        work: impl Future<Output = Result<T, E>>,
        summary: impl Fn(&T) -> String,
    ) -> Option<T> {
        let result = work.await;
        let line = match &result {
            Ok(value) => format!("ok, {}", summary(value)),
            Err(err) => {
                self.failures.push(name.to_owned());
                format!("FAILED: {err}")
            }
        };
        println!("{name:<30} {line}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        result.ok()
    }

    fn finish(self) {
        assert!(self.failures.is_empty(), "failed: {:?}", self.failures);
    }
}

fn shelves(page: &Page) -> String {
    let shelves = match page {
        Page::Browse(p) | Page::SearchLanding(p) => &p.shelves,
        Page::Charts(p) => &p.shelves,
        Page::Album(p) => &p.shelves,
        Page::Playlist(p) => &p.shelves,
        Page::Artist(p) => &p.shelves,
        Page::MusicVideo(p) => &p.shelves,
        Page::Curator(p) => &p.shelves,
        _ => return counts(page),
    };
    let items: Vec<String> = shelves.iter().map(|s| s.items.len().to_string()).collect();
    format!(
        "{} shelves [{}]; {}",
        shelves.len(),
        items.join(" "),
        counts(page)
    )
}

fn listing(l: &Listing) -> String {
    let more = match l {
        Listing::Tiles(l) => l.more.is_some(),
        Listing::Tracks(l) => l.more.is_some(),
    };
    format!("{} items{}", l.len(), if more { ", more" } else { "" })
}

fn counts(page: &Page) -> String {
    match page {
        Page::Search(p) | Page::LibrarySearch(p) => {
            let n: Vec<String> = p
                .sections
                .iter()
                .map(|s| s.listing.len().to_string())
                .collect();
            format!("{} sections [{}]", p.sections.len(), n.join(" "))
        }
        Page::Album(p) => format!("{} tracks", p.tracks().count()),
        Page::Playlist(p) => format!("{} tracks", p.tracks.items.len()),
        Page::Artist(p) => format!("latest release: {}", p.latest_release.is_some()),
        Page::List(p) => listing(&p.listing),
        Page::Song(p) => format!("{} credit groups", p.credits.len()),
        Page::Station(p) => format!("live: {}, playable: {}", p.live, p.play.is_some()),
        Page::Curator(p) => format!("{} playlists", p.playlists.items.len()),
        Page::Library(p) => listing(&p.listing),
        Page::LibraryAlbum(p) => format!("{} tracks", p.tracks().count()),
        Page::LibraryPlaylist(p) => {
            format!("{} tracks, editable: {}", p.tracks.items.len(), p.editable)
        }
        Page::Charts(p) => format!("{} genres", p.genres.len()),
        Page::Browse(_) | Page::SearchLanding(_) | Page::MusicVideo(_) => String::new(),
    }
}

fn first_room(page: &Page) -> Option<String> {
    let Page::Browse(p) = page else { return None };
    p.shelves.iter().find_map(|s| match &s.see_all {
        Some(Route::Room { id }) => Some(id.clone()),
        _ => None,
    })
}

#[tokio::test]
#[ignore = "calls Apple's servers"]
async fn live_signed_out() {
    let api = Api::new(Client::new(reqwest::Client::new()), Locale::default());
    let mut run = Run {
        failures: Vec::new(),
    };
    let id = |s: &str| s.to_owned();

    let new = run
        .step("new", pages::load(&api, &Route::New), shelves)
        .await;
    let mut routes = vec![
        Route::Radio,
        Route::Search { term: id("") },
        Route::Search {
            term: id("phoebe bridgers"),
        },
        Route::Charts { genre: None },
        Route::Album {
            id: id("1889568617"),
        },
        Route::Playlist {
            id: id("pl.f083772167b24f5a9d691e3c3e16f968"),
        },
        Route::Artist {
            id: id("697833299"),
        },
        Route::ArtistView {
            id: id("697833299"),
            view: ArtistView::TopSongs,
        },
        Route::Song {
            id: id("6781051513"),
        },
        Route::MusicVideo {
            id: id("6834539294"),
        },
        Route::Station {
            id: id("ra.978194965"),
        },
        Route::Curator {
            id: id("976439548"),
        },
    ];
    // Rooms come and go: take one the New page links to.
    routes.extend(
        new.as_ref()
            .and_then(first_room)
            .map(|id| Route::Room { id }),
    );
    for route in &routes {
        let name = route.to_link();
        let page = run.step(&name, pages::load(&api, route), shelves).await;
        // One continuation of each kind Apple pages: search groups (`with=` re-added) and an
        // artist's see-all list.
        let more = match &page {
            Some(Page::Search(p)) => p.sections.iter().find_map(|s| match &s.listing {
                Listing::Tracks(l) => l.more.clone(),
                Listing::Tiles(_) => None,
            }),
            Some(Page::List(p)) => match &p.listing {
                Listing::Tracks(l) => l.more.clone(),
                Listing::Tiles(_) => None,
            },
            _ => None,
        };
        if let Some(more) = more {
            run.step(&format!("{name} (more)"), more.load(&api), |l| {
                format!("{} items", l.items.len())
            })
            .await;
        }
    }
    run.step("hints", catalog::hints(&api, "phoebe"), |h| {
        format!("{} hints", h.len())
    })
    .await;
    run.finish();
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

fn test_song() -> String {
    std::env::var("RUSTET_TEST_SONG").unwrap_or_else(|_| "6781051513".into())
}

#[tokio::test]
#[ignore = "needs RUSTET_TEST_MUT; calls Apple's servers"]
async fn live_signed_in_reads() {
    let api = signed_in_api();
    let mut run = Run {
        failures: Vec::new(),
    };
    run.step("home", pages::load(&api, &Route::Home), shelves)
        .await;
    let mut playlist = None;
    let mut album = None;
    for section in [
        LibrarySection::RecentlyAdded,
        LibrarySection::RecentlyPlayed,
        LibrarySection::Albums,
        LibrarySection::Songs,
        LibrarySection::Artists,
        LibrarySection::Playlists,
        LibrarySection::MusicVideos,
    ] {
        let route = Route::Library(section);
        let page = run
            .step(&route.to_link(), pages::load(&api, &route), counts)
            .await;
        if let Some(Page::Library(page)) = page
            && let Listing::Tiles(list) = &page.listing
        {
            for tile in &list.items {
                match &tile.route {
                    Some(Route::LibraryPlaylist { id }) if playlist.is_none() => {
                        playlist = Some(id.clone())
                    }
                    Some(Route::LibraryAlbum { id }) if album.is_none() => album = Some(id.clone()),
                    _ => {}
                }
            }
            if let Some(more) = &list.more {
                run.step(
                    &format!("{} (more)", route.to_link()),
                    more.load(&api),
                    |l| format!("{} items", l.items.len()),
                )
                .await;
            }
        }
    }
    for route in [
        playlist.map(|id| Route::LibraryPlaylist { id }),
        album.map(|id| Route::LibraryAlbum { id }),
        Some(Route::LibrarySearch { term: "a".into() }),
    ]
    .into_iter()
    .flatten()
    {
        run.step("library item", pages::load(&api, &route), counts)
            .await;
    }
    run.step("lyrics", lyrics::load(&api, &test_song()), |l| {
        match &l.lines {
            lyrics::LyricLines::Synced(lines) => format!(
                "{} timed lines, word-timed: {}",
                lines.len(),
                lines.iter().any(|l| !l.words.is_empty())
            ),
            lyrics::LyricLines::Unsynced(lines) => format!("{} plain lines", lines.len()),
        }
    })
    .await;

    let top = run
        .step("playlist folders", playlist_folder(&api, None), |e| {
            let folders = e
                .iter()
                .filter(|e| matches!(e, PlaylistEntry::Folder { .. }))
                .count();
            format!("{folders} folders, {} playlists", e.len() - folders)
        })
        .await;
    let first_folder = top.iter().flatten().find_map(|e| match e {
        PlaylistEntry::Folder { id, .. } => Some(id.clone()),
        PlaylistEntry::Playlist { .. } => None,
    });
    if let Some(id) = first_folder {
        run.step("a playlist folder", playlist_folder(&api, Some(&id)), |e| {
            format!("{} entries", e.len())
        })
        .await;
    }

    let dir = tempfile::tempdir().unwrap();
    let library = Library::open_in(dir.path()).await.unwrap();
    let mut pages_fetched = 0;
    run.step(
        "library sync",
        library.sync(&api, &mut LibrarySync::default(), |_| pages_fetched += 1),
        |s| {
            format!(
                "{} songs, {} albums, {} playlists",
                s.songs, s.albums, s.playlists
            )
        },
    )
    .await;
    println!("{:<30} {pages_fetched} progress events", "");
    run.finish();
}

#[tokio::test]
#[ignore = "needs RUSTET_TEST_MUT; changes the account, then restores it"]
async fn live_signed_in_writes() {
    let api = signed_in_api();
    let mut run = Run {
        failures: Vec::new(),
    };
    let song = ResourceRef::new(ResourceType::Songs, test_song());

    let created = run
        .step(
            "create playlist",
            actions::perform(
                &api,
                &Action::CreatePlaylist {
                    name: "Rustet app test".into(),
                    description: Some("Made by rustet-app's live test; safe to delete.".into()),
                    tracks: vec![],
                },
            ),
            |o| format!("created: {}", o.created.is_some()),
        )
        .await
        .and_then(|o| o.created);
    if let Some(id) = created {
        let edit = Action::EditPlaylist {
            id: id.clone(),
            name: Some("Rustet app test (renamed)".into()),
            description: None,
        };
        run.step("rename playlist", actions::perform(&api, &edit), |o| {
            o.toast.clone()
        })
        .await;
        let add = Action::AddToPlaylist {
            playlist_id: id.clone(),
            tracks: vec![song.clone(), song.clone()],
        };
        run.step("add tracks", actions::perform(&api, &add), |o| {
            o.toast.clone()
        })
        .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let page = run
            .step("load playlist", user::library_playlist(&api, &id), |p| {
                format!("{} tracks", p.tracks.items.len())
            })
            .await;
        if let Some(page) = page
            && let Some(order) = page.moved(0, page.tracks.items.len().saturating_sub(1))
        {
            let tracks = order.iter().filter_map(|r| r.library_ref()).collect();
            let reorder = Action::SetPlaylistOrder {
                id: id.clone(),
                tracks,
            };
            run.step("reorder", actions::perform(&api, &reorder), |o| {
                o.toast.clone()
            })
            .await;
            if let Some(lid) = order.first().and_then(|r| r.id.library_id()) {
                let remove = Action::RemoveFromPlaylist {
                    id: id.clone(),
                    library_song_id: lid.to_owned(),
                };
                run.step("remove track", actions::perform(&api, &remove), |o| {
                    o.toast.clone()
                })
                .await;
            }
        }
        let delete = Action::DeletePlaylist { id };
        run.step("delete playlist", actions::perform(&api, &delete), |o| {
            o.toast.clone()
        })
        .await;
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let before = run
        .step("read rating", api.rating(&cancel, &song), |r| {
            format!("{r:?}")
        })
        .await;
    if let Some(before) = before {
        let love = Action::Rate {
            item: song.clone(),
            rating: Some(rustet_ampapi::api::Rating::Love),
        };
        run.step("love", actions::perform(&api, &love), |o| o.toast.clone())
            .await;
        let restore = Action::Rate {
            item: song.clone(),
            rating: before,
        };
        run.step("restore rating", actions::perform(&api, &restore), |o| {
            o.toast.clone()
        })
        .await;
    }
    run.finish();
}
