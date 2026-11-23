//! Routes: parsing music.apple.com and `rustet://` links, and the round trip back to links.

use rustet_ampapi::ArtistView;
use rustet_app::route::{ARTIST_VIEWS, LibrarySection, Route};

fn id(s: &str) -> String {
    s.to_owned()
}

/// One of every route.
fn every_route() -> Vec<Route> {
    let mut routes = vec![
        Route::Search { term: id("") },
        Route::Search {
            term: id("phoebe bridgers & co?"),
        },
        Route::New,
        Route::Radio,
        Route::Home,
        Route::Charts { genre: None },
        Route::Charts {
            genre: Some(id("20")),
        },
        Route::Album {
            id: id("1441164426"),
        },
        Route::Playlist {
            id: id("pl.f4d106fed2bd41149aaacabb233eb5eb"),
        },
        Route::Artist { id: id("136975") },
        Route::Song {
            id: id("1441164430"),
        },
        Route::MusicVideo {
            id: id("1440123456"),
        },
        Route::Station {
            id: id("ra.978194965"),
        },
        Route::Room {
            id: id("6834267408"),
        },
        Route::Curator {
            id: id("976439548"),
        },
        Route::LibrarySearch { term: id("abba") },
        Route::LibraryAlbum {
            id: id("l.1OBwMCC"),
        },
        Route::LibraryPlaylist { id: id("p.Ab12Cd") },
        Route::Downloads,
        Route::Settings { section: None },
        Route::Settings {
            section: Some(id("playback")),
        },
    ];
    routes.extend(ARTIST_VIEWS.map(|view| Route::ArtistView {
        id: id("136975"),
        view,
    }));
    routes.extend(
        [
            LibrarySection::RecentlyAdded,
            LibrarySection::RecentlyPlayed,
            LibrarySection::Albums,
            LibrarySection::Songs,
            LibrarySection::Artists,
            LibrarySection::Playlists,
            LibrarySection::MusicVideos,
        ]
        .map(Route::Library),
    );
    routes
}

#[test]
fn every_route_round_trips_through_its_rustet_link() {
    for route in every_route() {
        let link = route.to_link();
        assert!(link.starts_with("rustet://"), "{link}");
        assert_eq!(Route::from_link(&link), Some(route.clone()), "{link}");
    }
}

#[test]
fn catalog_routes_round_trip_through_their_share_link_and_personal_ones_have_none() {
    for route in every_route() {
        match route.share_link("US") {
            Some(link) => {
                assert!(link.starts_with("https://music.apple.com/us/"), "{link}");
                assert_eq!(Route::from_link(&link), Some(route.clone()), "{link}");
            }
            None => assert!(
                route.needs_user_token()
                    || matches!(route, Route::Downloads | Route::Settings { .. }),
                "{route:?} has no share link"
            ),
        }
    }
}

#[test]
fn apple_music_urls_open_their_page() {
    let cases = [
        (
            "https://music.apple.com/us/album/abbey-road/1441164426",
            Route::Album {
                id: id("1441164426"),
            },
        ),
        // An album link with ?i= is one song of it.
        (
            "https://music.apple.com/us/album/abbey-road/1441164426?i=1441164430",
            Route::Song {
                id: id("1441164430"),
            },
        ),
        (
            "https://music.apple.com/gb/artist/the-beatles/136975",
            Route::Artist { id: id("136975") },
        ),
        (
            "https://music.apple.com/us/artist/the-beatles/136975/see-all?section=full-albums",
            Route::ArtistView {
                id: id("136975"),
                view: ArtistView::FullAlbums,
            },
        ),
        (
            "https://music.apple.com/us/station/apple-music-1/ra.978194965",
            Route::Station {
                id: id("ra.978194965"),
            },
        ),
        (
            "https://music.apple.com/us/room/6834267408",
            Route::Room {
                id: id("6834267408"),
            },
        ),
        (
            "https://music.apple.com/us/curator/apple-music-pop/976439548",
            Route::Curator {
                id: id("976439548"),
            },
        ),
        ("https://music.apple.com/us/browse", Route::New),
        ("https://music.apple.com/us/new", Route::New),
        ("https://music.apple.com/us/radio", Route::Radio),
        ("https://music.apple.com/us/listen-now", Route::Home),
        (
            "https://music.apple.com/us/search?term=a%20b",
            Route::Search { term: id("a b") },
        ),
        (
            "https://music.apple.com/library/recently-added",
            Route::Library(LibrarySection::RecentlyAdded),
        ),
        (
            "https://music.apple.com/library/all-playlists",
            Route::Library(LibrarySection::Playlists),
        ),
        (
            "https://music.apple.com/library/playlist/p.Ab12Cd",
            Route::LibraryPlaylist { id: id("p.Ab12Cd") },
        ),
        (
            "itmss://music.apple.com/us/album/x/1441164426",
            Route::Album {
                id: id("1441164426"),
            },
        ),
    ];
    for (link, route) in cases {
        assert_eq!(Route::from_link(link), Some(route), "{link}");
    }
}

#[test]
fn rustet_links_open_their_page() {
    let cases = [
        (
            "rustet://album/1441164426",
            Route::Album {
                id: id("1441164426"),
            },
        ),
        (
            "rustet://play/a/1441164426",
            Route::Album {
                id: id("1441164426"),
            },
        ),
        (
            "rustet://open/r/ra.978194965",
            Route::Station {
                id: id("ra.978194965"),
            },
        ),
        (
            "rustet://library/songs",
            Route::Library(LibrarySection::Songs),
        ),
        (
            "rustet://library/search?term=abba",
            Route::LibrarySearch { term: id("abba") },
        ),
        ("rustet://search?term=x", Route::Search { term: id("x") }),
        (
            "rustet://artist/136975/see-all/top-songs",
            Route::ArtistView {
                id: id("136975"),
                view: ArtistView::TopSongs,
            },
        ),
    ];
    for (link, route) in cases {
        assert_eq!(Route::from_link(link), Some(route), "{link}");
    }
}

#[test]
fn links_rustet_cannot_show_are_none() {
    for link in [
        "",
        "https://example.com/us/album/x/1",
        "https://music.apple.com/WebObjects/MZStore.woa/wa/viewGrouping?cc=us&id=168981",
        "rustet://station/..",
        "rustet://library/nonsense",
        "rustet://artist/136975/see-all/nonsense",
        "https://music.apple.com/us/artist/x/136975/see-all?section=nonsense",
    ] {
        assert_eq!(Route::from_link(link), None, "{link}");
    }
}

#[test]
fn only_home_and_library_routes_need_the_user_token() {
    let needs: Vec<_> = every_route()
        .into_iter()
        .filter(Route::needs_user_token)
        .collect();
    assert!(needs.iter().all(|r| matches!(
        r,
        Route::Home
            | Route::Library(_)
            | Route::LibrarySearch { .. }
            | Route::LibraryAlbum { .. }
            | Route::LibraryPlaylist { .. }
    )));
    assert_eq!(needs.len(), 11);
}
