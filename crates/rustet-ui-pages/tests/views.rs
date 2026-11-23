//! Page views without a display: gpui's test platform runs the views; the pages come from the
//! example's fixtures (recorded catalog responses, library responses built from them) through
//! `rustet_app`'s loaders, as they do in the app.
//!
//!     cargo test -p rustet-ui-pages --features test-support
#![cfg(feature = "test-support")]

#[path = "../examples/pages/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext, Context, Entity, IntoElement, Render, TestAppContext, Window, div, point,
    prelude::*, px,
};
use rustet_ampapi::Api;
use rustet_ampapi::api::{ResourceRef, ResourceType};
use rustet_app::library::PlaylistEntry;
use rustet_app::nav::MoreTarget;
use rustet_app::pages::actions::Action;
use rustet_app::pages::model::{Listing, TrackId};
use rustet_app::pages::play::{How, PlayTarget};
use rustet_app::pages::{self, Page};
use rustet_app::route::{LibrarySection, Route};
use rustet_player::model::Command;
use rustet_ui::content::track_list;
use rustet_ui::theme::{Appearance, Theme};
use rustet_ui_pages::menu::{self, Editing, labels};
use rustet_ui_pages::view::{self, Layout, layout, near_end};
use rustet_ui_pages::{PageContext, PageEvent, PageView, PlayRequest, Playing, PlaylistChoice};

struct Fixtures {
    rt: tokio::runtime::Runtime,
    api: Api,
}

impl Fixtures {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let api = rt.block_on(fixtures::api(None)).unwrap();
        Self { rt, api }
    }

    fn page(&self, route: &Route) -> Arc<Page> {
        Arc::new(
            self.rt
                .block_on(pages::load(&self.api, route))
                .unwrap_or_else(|e| panic!("{route:?}: {e}")),
        )
    }
}

fn signed_in() -> PageContext {
    PageContext {
        signed_in: true,
        storefront: "us".into(),
        playlists: vec![PlaylistChoice {
            id: "p.Other".into(),
            name: "Other".into(),
        }]
        .into(),
        playing: None,
    }
}

struct Root(Option<gpui::AnyView>);

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().children(self.0.clone())
    }
}

type Seen = Rc<RefCell<Vec<PageEvent>>>;

/// A page view in a window, with the events it sends.
fn open(
    cx: &mut TestAppContext,
    route: Route,
    page: Arc<Page>,
    ctx: PageContext,
) -> (Entity<PageView>, Seen, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        cx.set_global(Theme::new(Appearance::Dark));
        rustet_ui::components::bind_keys(cx);
        track_list::bind_keys(cx);
        view::bind_keys(cx);
        rustet_ui::runtime::Tokio::install(cx).unwrap();
        // No downloads in tests: the artwork keeps its placeholder.
        let config = rustet_ui::artwork::StoreConfig {
            max_fetches: 0,
            ..Default::default()
        };
        rustet_ui::artwork::ArtworkStore::install(reqwest::Client::new(), None, config, cx);
    });
    let seen: Seen = Rc::default();
    let sink = {
        let seen = seen.clone();
        Rc::new(move |e: PageEvent, _: &mut Window, _: &mut gpui::App| seen.borrow_mut().push(e))
    };
    let (root, cx) = cx.add_window_view(|_, _| Root(None));
    let view =
        cx.update(|window, cx| cx.new(|cx| PageView::new(route, page, ctx, sink, window, cx)));
    root.update(cx, |r, _| r.0 = Some(view.clone().into()));
    cx.simulate_resize(gpui::size(px(1200.), px(300.)));
    cx.run_until_parked();
    (view, seen, cx)
}

#[test]
fn near_end_is_within_a_screen_of_the_end() {
    assert!(
        !near_end(px(0.), px(3000.), px(800.)),
        "at the top of a long list"
    );
    assert!(near_end(px(-2300.), px(3000.), px(800.)), "700 px to go");
    assert!(
        near_end(px(0.), px(0.), px(800.)),
        "a list too short to scroll"
    );
}

#[test]
fn every_route_gets_its_view() {
    let f = Fixtures::new();
    type Case = (Route, fn(&Page) -> bool, Layout);
    let cases: Vec<Case> = vec![
        (
            Route::Home,
            |p| matches!(p, Page::Browse(_)),
            Layout::Scroll,
        ),
        (Route::New, |p| matches!(p, Page::Browse(_)), Layout::Scroll),
        (
            Route::Radio,
            |p| matches!(p, Page::Browse(_)),
            Layout::Scroll,
        ),
        (
            Route::Search {
                term: String::new(),
            },
            |p| matches!(p, Page::SearchLanding(_)),
            Layout::Scroll,
        ),
        (
            Route::Search {
                term: "radiohead".into(),
            },
            |p| matches!(p, Page::Search(_)),
            Layout::Scroll,
        ),
        (
            Route::Charts { genre: None },
            |p| matches!(p, Page::Charts(_)),
            Layout::Scroll,
        ),
        (
            Route::Album {
                id: "1889568617".into(),
            },
            |p| matches!(p, Page::Album(_)),
            Layout::Scroll,
        ),
        (
            Route::Playlist {
                id: "pl.f083772167b24f5a9d691e3c3e16f968".into(),
            },
            |p| matches!(p, Page::Playlist(_)),
            Layout::Scroll,
        ),
        (
            Route::Artist {
                id: "697833299".into(),
            },
            |p| matches!(p, Page::Artist(_)),
            Layout::Scroll,
        ),
        (
            Route::ArtistView {
                id: "1".into(),
                view: rustet_app::route::artist_view("full-albums").unwrap(),
            },
            |p| matches!(p, Page::List(_)),
            Layout::Fill,
        ),
        (
            Route::Song {
                id: "6781051513".into(),
            },
            |p| matches!(p, Page::Song(_)),
            Layout::Scroll,
        ),
        (
            Route::MusicVideo {
                id: "6834539294".into(),
            },
            |p| matches!(p, Page::MusicVideo(_)),
            Layout::Scroll,
        ),
        (
            Route::Station {
                id: "ra.6833682596".into(),
            },
            |p| matches!(p, Page::Station(_)),
            Layout::Scroll,
        ),
        (
            Route::Room {
                id: "6834266064".into(),
            },
            |p| matches!(p, Page::List(_)),
            Layout::Fill,
        ),
        (
            Route::Curator {
                id: "976439548".into(),
            },
            |p| matches!(p, Page::Curator(_)),
            Layout::Scroll,
        ),
        (
            Route::Library(LibrarySection::Songs),
            |p| matches!(p, Page::Library(_)),
            Layout::Fill,
        ),
        (
            Route::Library(LibrarySection::Albums),
            |p| matches!(p, Page::Library(_)),
            Layout::Fill,
        ),
        (
            Route::LibrarySearch { term: "a".into() },
            |p| matches!(p, Page::LibrarySearch(_)),
            Layout::Scroll,
        ),
        (
            Route::LibraryAlbum {
                id: fixtures::MY_ALBUM.into(),
            },
            |p| matches!(p, Page::LibraryAlbum(_)),
            Layout::Scroll,
        ),
        (
            Route::LibraryPlaylist {
                id: fixtures::MY_PLAYLIST.into(),
            },
            |p| matches!(p, Page::LibraryPlaylist(_)),
            Layout::Scroll,
        ),
    ];
    for (route, is, expected) in cases {
        let page = f.page(&route);
        assert!(is(&page), "{route:?} loads {page:?}");
        assert_eq!(layout(&page), expected, "{route:?}");
    }
}

#[gpui::test]
fn album_discs_are_lists_that_play_the_album_from_the_row(cx: &mut TestAppContext) {
    let f = Fixtures::new();
    let route = Route::Album {
        id: "1889568617".into(),
    };
    let page = f.page(&route);
    let Page::Album(album) = &*page else {
        panic!("album")
    };
    let (view, seen, cx) = open(cx, route, page.clone(), PageContext::default());
    let lists = view.read_with(cx, |v, _| v.track_lists());
    assert_eq!(lists.len(), album.discs.len(), "one list per disc");
    // Enter on the second row of the first disc plays the album from that track.
    cx.update(|w, cx| {
        let handle = gpui::Focusable::focus_handle(lists[0].read(cx), cx);
        w.focus(&handle, cx)
    });
    cx.simulate_keystrokes("down down enter");
    cx.run_until_parked();
    let expected = album.play(How::From(1));
    assert_eq!(
        seen.borrow().last(),
        Some(&PageEvent::Play(PlayRequest::Intent(expected)))
    );
}

#[gpui::test]
fn header_actions_follow_the_page_and_the_account(cx: &mut TestAppContext) {
    let f = Fixtures::new();
    let album = f.page(&Route::Album {
        id: "1889568617".into(),
    });
    use rustet_ui::content::HeaderAction::*;
    assert_eq!(
        view::header_actions(&album, &PageContext::default()),
        [Play, Shuffle, Download, More]
    );
    assert_eq!(
        view::header_actions(&album, &signed_in()),
        [Play, Shuffle, AddToLibrary, Download, More]
    );
    let station = f.page(&Route::Station {
        id: "ra.6833682596".into(),
    });
    assert_eq!(
        view::header_actions(&station, &signed_in())[0],
        Play,
        "a station plays its target"
    );
    let _ = cx;
}

#[test]
fn the_playing_row_is_found_by_library_then_catalog_id() {
    let f = Fixtures::new();
    let Page::LibraryPlaylist(p) = &*f.page(&Route::LibraryPlaylist {
        id: fixtures::MY_PLAYLIST.into(),
    }) else {
        panic!("library playlist")
    };
    let rows = &p.tracks.items;
    let TrackId::Library {
        library_id,
        catalog_id: Some(catalog_id),
    } = rows[2].id.clone()
    else {
        panic!("library rows")
    };
    let by_catalog = Playing {
        catalog_id: catalog_id.clone(),
        library_id: None,
    };
    assert_eq!(by_catalog.row_id(rows), Some(rows[2].id.clone()));
    let by_library = Playing {
        catalog_id: "other".into(),
        library_id: Some(library_id),
    };
    assert_eq!(
        by_library.row_id(rows),
        Some(rows[2].id.clone()),
        "the library song wins"
    );
    let elsewhere = Playing {
        catalog_id: "0".into(),
        library_id: None,
    };
    assert_eq!(elsewhere.row_id(rows), None);
}

#[test]
fn row_menus_follow_the_item_and_the_account() {
    let f = Fixtures::new();
    let album = f.page(&Route::Album {
        id: "1889568617".into(),
    });
    let Page::Album(a) = &*album else { panic!() };
    let row = a.tracks().next().unwrap().clone();
    let signed_out = PageContext {
        storefront: "us".into(),
        ..Default::default()
    };
    assert_eq!(
        labels(&menu::rows_menu(
            std::slice::from_ref(&row),
            &signed_out,
            None
        )),
        [
            "Play next",
            "Play later",
            "Go to album",
            "Go to artist",
            "Download",
            "Share link"
        ]
    );
    let entries = menu::rows_menu(std::slice::from_ref(&row), &signed_in(), None);
    assert_eq!(
        labels(&entries),
        [
            "Play next",
            "Play later",
            "Add to library",
            "Add to playlist",
            "Love",
            "Dislike",
            "Go to album",
            "Go to artist",
            "Download",
            "Share link"
        ]
    );
    // "Add to playlist ▸" lists the user's playlists after "New playlist…".
    let sub = entries.iter().find_map(|e| match e {
        menu::Entry::Sub { entries, .. } => Some(labels(entries)),
        _ => None,
    });
    assert_eq!(sub, Some(vec!["New playlist…", "Other"]));
    // Play next queues the row; the download keeps the row (its title for the job list).
    let event = |label: &str| {
        entries.iter().find_map(|e| match e {
            menu::Entry::Item {
                label: l, event, ..
            } if l == label => Some(event.clone()),
            _ => None,
        })
    };
    assert!(matches!(
        event("Play next"),
        Some(PageEvent::Play(PlayRequest::Intent(Ok(Command::Insert(_)))))
    ));
    assert!(
        matches!(event("Download"), Some(PageEvent::Download(rustet_ui_pages::DownloadRequest::Tracks(r))) if r == vec![row.clone()])
    );

    // Several rows: what applies to all of them.
    let rows: Vec<_> = a.tracks().take(3).cloned().collect();
    assert_eq!(
        labels(&menu::rows_menu(&rows, &signed_in(), None)),
        [
            "Play next",
            "Play later",
            "Add to library",
            "Add to playlist",
            "Download"
        ]
    );
}

#[test]
fn rows_of_an_editable_playlist_move_and_leave_it() {
    let f = Fixtures::new();
    let page = f.page(&Route::LibraryPlaylist {
        id: fixtures::MY_PLAYLIST.into(),
    });
    let Page::LibraryPlaylist(p) = &*page else {
        panic!()
    };
    let row = p.tracks.items[0].clone();
    let entries = menu::rows_menu(
        std::slice::from_ref(&row),
        &signed_in(),
        Some(Editing { page: p, index: 0 }),
    );
    let l = labels(&entries);
    assert!(
        l.contains(&"Move down") && !l.contains(&"Move up"),
        "the first row moves down only: {l:?}"
    );
    assert!(
        l.contains(&"Remove from playlist") && !l.contains(&"Remove from library"),
        "{l:?}"
    );
    let moved = entries.iter().find_map(|e| match e {
        menu::Entry::Item {
            label,
            event: PageEvent::Act(Action::SetPlaylistOrder { tracks, .. }),
            ..
        } if label == "Move down" => Some(tracks.clone()),
        _ => None,
    });
    let ids: Vec<String> = moved.unwrap().into_iter().take(2).map(|r| r.id).collect();
    let first_two: Vec<String> = p.tracks.items[..2]
        .iter()
        .map(|r| r.id.library_id().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        [first_two[1].clone(), first_two[0].clone()],
        "rows 0 and 1 swap"
    );

    // The page's own menu renames and deletes.
    let page_menu = labels(&menu::page_entries(
        &page,
        &Route::LibraryPlaylist { id: p.id.clone() },
        &signed_in(),
    ))
    .join(", ");
    assert!(
        page_menu.contains("Rename…") && page_menu.contains("Delete playlist…"),
        "{page_menu}"
    );
}

#[test]
fn tile_menus_follow_the_tile_kind() {
    let f = Fixtures::new();
    let Page::Library(albums) = &*f.page(&Route::Library(LibrarySection::Albums)) else {
        panic!()
    };
    let Listing::Tiles(list) = &albums.listing else {
        panic!()
    };
    assert_eq!(
        labels(&menu::tile_entries(&list.items[0], &signed_in())),
        [
            "Play next",
            "Play later",
            "Remove from library",
            "Love",
            "Dislike"
        ],
        "a library album"
    );
    let Page::Browse(radio) = &*f.page(&Route::Radio) else {
        panic!()
    };
    let station = radio
        .shelves
        .iter()
        .flat_map(|s| match &s.items {
            rustet_app::pages::model::ShelfItems::Tiles { tiles, .. } => tiles.clone(),
            _ => Vec::new(),
        })
        .find(|t| matches!(t.play, Some(PlayTarget::Station(_))))
        .expect("a station tile");
    let station_menu = menu::tile_entries(&station, &signed_in());
    let l = labels(&station_menu);
    assert!(
        !l.contains(&"Play next"),
        "a station cannot be queued: {l:?}"
    );
    // A library album is removed after a question.
    let remove = menu::tile_entries(&list.items[0], &signed_in())
        .into_iter()
        .find_map(|e| match e {
            menu::Entry::Item {
                event: PageEvent::Confirm { action, .. },
                ..
            } => Some(action),
            _ => None,
        });
    assert_eq!(
        remove,
        Some(Action::RemoveFromLibrary(ResourceRef::new(
            ResourceType::LibraryAlbums,
            list.items[0].item.clone().unwrap().id
        )))
    );
}

#[gpui::test]
fn load_more_is_asked_once_near_the_end_and_keeps_the_scroll_position(cx: &mut TestAppContext) {
    let f = Fixtures::new();
    let route = Route::Library(LibrarySection::Songs);
    let page = f.page(&route);
    let Page::Library(p) = &*page else { panic!() };
    let Listing::Tracks(first) = &p.listing else {
        panic!()
    };
    assert!(
        first.more.is_some(),
        "the fixtures page library songs by 10"
    );
    let (view, seen, cx) = open(cx, route.clone(), page.clone(), PageContext::default());
    let list = view.read_with(cx, |v, _| v.track_lists()[0].clone());
    let scroll_to_end = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            let handle = list.read(cx).scroll.0.borrow().base_handle.clone();
            handle.set_offset(point(px(0.), -handle.max_offset().y));
            list.update(cx, |_, cx| cx.notify());
        });
        cx.run_until_parked();
    };
    scroll_to_end(cx);
    scroll_to_end(cx);
    let asked = seen
        .borrow()
        .iter()
        .filter(|e| **e == PageEvent::LoadMore(MoreTarget::Main))
        .count();
    assert_eq!(asked, 1, "asked once for these rows: {:?}", seen.borrow());

    // The next part arrives: rows are appended where the list is.
    let offset = cx.update(|_, cx| list.read(cx).scroll.0.borrow().base_handle.offset());
    let next =
        f.rt.block_on(first.more.as_ref().unwrap().load(&f.api))
            .unwrap();
    let mut longer = p.clone();
    if let Listing::Tracks(l) = &mut longer.listing {
        l.extend(next);
    }
    let longer = Arc::new(Page::Library(longer));
    cx.update(|w, cx| view.update(cx, |v, cx| v.set_page(longer.clone(), w, cx)));
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| list.read(cx).rows().len()),
        longer_len(&longer)
    );
    assert_eq!(
        cx.update(|_, cx| list.read(cx).scroll.0.borrow().base_handle.offset()),
        offset,
        "no jump"
    );
}

fn longer_len(page: &Page) -> usize {
    match page {
        Page::Library(p) => p.listing.len(),
        _ => 0,
    }
}

#[gpui::test]
fn playlist_folders_open_in_place(cx: &mut TestAppContext) {
    let f = Fixtures::new();
    let route = Route::Library(LibrarySection::Playlists);
    let page = f.page(&route);
    let (view, _, cx) = open(cx, route, page, signed_in());
    let root =
        f.rt.block_on(rustet_app::library::playlist_folder(&f.api, None))
            .unwrap();
    assert!(
        root.iter()
            .any(|e| matches!(e, PlaylistEntry::Folder { .. }))
    );
    let inside =
        f.rt.block_on(rustet_app::library::playlist_folder(
            &f.api,
            Some("p.FolderRoad"),
        ))
        .unwrap();
    cx.update(|_, cx| {
        view.update(cx, |v, cx| {
            v.set_root_folders(root, cx);
            v.open_folder(Some("p.FolderRoad".into()), "Road trips".into(), cx);
            v.set_folder_entries("p.FolderRoad", inside.clone(), cx);
        })
    });
    cx.run_until_parked();
    let shown = view.read_with(cx, |v, cx| v.folder_tiles(cx));
    assert_eq!(
        shown,
        inside
            .iter()
            .filter(|e| matches!(e, PlaylistEntry::Playlist { .. }))
            .count()
    );
    assert!(shown > 0);
}

#[gpui::test]
fn search_see_all_shows_a_section_in_full(cx: &mut TestAppContext) {
    let f = Fixtures::new();
    let route = Route::Search {
        term: "radiohead".into(),
    };
    let page = f.page(&route);
    let Page::Search(p) = &*page else { panic!() };
    let songs = p
        .sections
        .iter()
        .position(|s| matches!(s.listing, Listing::Tracks(_)))
        .unwrap();
    let (view, _, cx) = open(cx, route, page.clone(), PageContext::default());
    cx.update(|w, cx| view.update(cx, |v, cx| v.expand(songs, w, cx)));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| v.expanded_section()), Some(songs));
}
