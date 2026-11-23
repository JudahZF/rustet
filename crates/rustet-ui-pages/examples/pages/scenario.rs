//! Scripted runs of the example (`--scenario NAME`), for the headless compositor:
//!
//! - `live`: every page a signed-out user can open, from Apple's live catalog, in dark and
//!   light at two widths; the sign-in state of Home and the library; a failed load; a context
//!   menu; the modals; "load more" on a long list; Back restoring the scroll position.
//! - `fixtures`: Home and every library page from the fixtures (`--fixtures`), the playlist
//!   folders, the editable playlist's menu, the rename sheet, the delete dialog, the picker.
//! - `account`: with `--account`, the signed-in pages of the user's own library, read only:
//!   Home, every library section, the first library album and playlist, a row menu. No
//!   action is taken.
//! - `bench`: scrolls the 10,000-row library song list (`--fixtures --songs 10000`) and
//!   reports frame times.
//!
//! Screenshots go to `RUSTET_UI_SHOT_DIR`, cropped to the window (the compositor's magenta).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, AsyncApp, Entity, Keystroke, WindowHandle, point, px, size};
use rustet_app::nav::{MoreState, PageState};
use rustet_app::pages::model::{Listing, ShelfItems, TrackKind};
use rustet_app::pages::{Page, actions::Action};
use rustet_app::route::{LibrarySection, Route, artist_view};
use rustet_ui::theme::Appearance;
use rustet_ui_pages::{PageEvent, PageView, menu};
use serde_json::json;

use crate::fixtures::{MY_ALBUM, MY_PLAYLIST};
use crate::{Demo, report};

const WIDTHS: [f32; 2] = [1360., 900.];
const HEIGHT: f32 = 860.;

pub fn run(name: String, window: WindowHandle<Demo>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let s = Script { window, cx };
        if !s.until(60_000, |d, _| d.area.is_some()).await {
            report("error", json!("the app did not start"));
            s.cx.update(|cx| cx.quit());
            return;
        }
        match name.as_str() {
            "live" => live(&s).await,
            "fixtures" => fixtures(&s).await,
            "account" => account(&s).await,
            "bench" => bench(&s).await,
            other => report("error", json!(format!("unknown scenario {other}"))),
        }
        // Stop the engine before leaving.
        let app = s.update(|d, _, _| d.app.clone()).flatten();
        if let Some(app) = app {
            let job =
                s.cx.update(|cx| rustet_ui::runtime::spawn(cx, async move { app.stop().await }));
            let _ = job.await;
        }
        s.cx.update(|cx| cx.quit());
    })
    .detach();
}

struct Script<'a> {
    window: WindowHandle<Demo>,
    cx: &'a AsyncApp,
}

impl Script<'_> {
    async fn wait(&self, ms: u64) {
        self.cx
            .background_executor()
            .timer(Duration::from_millis(ms))
            .await;
    }

    fn update<R>(
        &self,
        f: impl FnOnce(&mut Demo, &mut gpui::Window, &mut gpui::Context<Demo>) -> R,
    ) -> Option<R> {
        let mut cx = self.cx.clone();
        self.window.update(&mut cx, f).ok()
    }

    /// Waits until `f` holds, at most `ms`.
    async fn until(
        &self,
        ms: u64,
        f: impl Fn(&mut Demo, &mut gpui::Context<Demo>) -> bool,
    ) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            if self.update(|d, _, cx| f(d, cx)) == Some(true) {
                return true;
            }
            self.wait(100).await;
        }
        false
    }

    async fn keys(&self, keys: &'static str) {
        for key in keys.split(' ') {
            let Ok(stroke) = Keystroke::parse(key) else {
                panic!("bad keystroke {key}")
            };
            let mut cx = self.cx.clone();
            cx.update_window(self.window.into(), |_, window, cx| {
                window.dispatch_keystroke(stroke, cx)
            })
            .ok();
            self.wait(80).await;
        }
    }

    fn view(&self) -> Option<Entity<PageView>> {
        self.update(|d, _, cx| d.area.as_ref()?.read(cx).current().cloned())
            .flatten()
    }

    fn page(&self) -> Option<Arc<Page>> {
        self.update(|d, _, cx| match &d.area.as_ref()?.read(cx).state().page {
            PageState::Ready(p) => Some(p.clone()),
            _ => None,
        })
        .flatten()
    }

    /// Hands `event` to the area as a page would (outside the window's own update: the
    /// area's host requests update the window's view).
    fn handle(&self, event: PageEvent) {
        let Some(area) = self.update(|d, _, _| d.area.clone()).flatten() else {
            return;
        };
        let mut cx = self.cx.clone();
        cx.update_window(self.window.into(), |_, window, cx| {
            area.update(cx, |a, cx| a.handle(event, window, cx))
        })
        .ok();
    }

    /// Opens `route` and waits for its page; reports how it ended.
    async fn open(&self, name: &str, route: Route) -> &'static str {
        self.update(|d, _, _| d.nav.as_ref().map(|n| n.go(route.clone())));
        let started = Instant::now();
        let done = self
            .until(30_000, |d, cx| {
                d.area.as_ref().is_some_and(|a| {
                    let s = a.read(cx).state();
                    s.route == route && !matches!(s.page, PageState::Loading)
                })
            })
            .await;
        let state = self
            .update(|d, _, cx| d.area.as_ref().map(|a| a.read(cx).state().page.clone()))
            .flatten();
        let error = match &state {
            Some(PageState::Failed(e)) => Some(e.message.clone()),
            _ => None,
        };
        let outcome = match state {
            _ if !done => "timeout",
            Some(PageState::Ready(_)) => "ready",
            Some(PageState::Failed(_)) => "failed",
            _ => "other",
        };
        report(
            "page",
            json!({"name": name, "route": route.to_link(), "outcome": outcome, "error": error, "ms": started.elapsed().as_millis()}),
        );
        // The artwork of the first screen.
        self.wait(2000).await;
        outcome
    }

    async fn appearance(&self, a: Appearance) {
        self.update(|d, _, cx| d.set_appearance(a, cx));
        self.wait(300).await;
    }

    async fn resize(&self, width: f32) {
        self.update(|_, window, _| window.resize(size(px(width), px(HEIGHT))));
        self.wait(700).await;
    }

    /// Screenshots of the shown page in dark and light at both widths.
    async fn capture(&self, name: &str) {
        for (appearance, tag) in [(Appearance::Dark, "dark"), (Appearance::Light, "light")] {
            self.appearance(appearance).await;
            for width in WIDTHS {
                self.resize(width).await;
                self.shot(&format!("{name}-{tag}-{}", width as u32)).await;
            }
        }
        self.appearance(Appearance::Dark).await;
        self.resize(WIDTHS[0]).await;
    }

    async fn shot(&self, name: &str) {
        self.wait(400).await;
        let dir =
            PathBuf::from(std::env::var_os("RUSTET_UI_SHOT_DIR").unwrap_or_else(|| ".".into()));
        let path = dir.join(format!("{name}.png"));
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let ok = self
            .cx
            .background_executor()
            .spawn(async move {
                let captured = if wayland {
                    Command::new("grim").arg(&path).status()
                } else {
                    Command::new("import")
                        .args(["-window", "root"])
                        .arg(&path)
                        .status()
                };
                captured.is_ok_and(|s| s.success())
                    && Command::new("magick")
                        .arg(&path)
                        .args([
                            "-bordercolor",
                            "#ff00ff",
                            "-border",
                            "1",
                            "-fuzz",
                            "3%",
                            "-trim",
                            "+repage",
                        ])
                        .arg(&path)
                        .status()
                        .is_ok_and(|s| s.success())
            })
            .await;
        report("shot", json!({ "name": name, "ok": ok }));
    }

    /// The page scroll offset (scroll layout) or the first list's or grid's (fill layout).
    fn scroll_y(&self) -> Option<f32> {
        self.update(|d, _, cx| {
            let view = d.area.as_ref()?.read(cx).current()?.clone();
            let v = view.read(cx);
            let handle = match (v.track_lists().first(), v.grid()) {
                (_, Some(g)) => g.read(cx).scroll.0.borrow().base_handle.clone(),
                (Some(l), _)
                    if rustet_ui_pages::view::layout(v.page())
                        == rustet_ui_pages::view::Layout::Fill =>
                {
                    l.read(cx).scroll.0.borrow().base_handle.clone()
                }
                _ => v.scroll.clone(),
            };
            Some(f32::from(handle.offset().y))
        })
        .flatten()
    }

    /// Scrolls the page (or its list or grid) to `y` px from the top, or to the end.
    async fn scroll_to(&self, y: Option<f32>) {
        self.update(|d, _, cx| {
            let Some(view) = d.area.as_ref().and_then(|a| a.read(cx).current().cloned()) else {
                return;
            };
            let (handle, notify) = {
                let v = view.read(cx);
                match (v.track_lists().first(), v.grid()) {
                    (_, Some(g)) => (
                        g.read(cx).scroll.0.borrow().base_handle.clone(),
                        g.entity_id(),
                    ),
                    (Some(l), _)
                        if rustet_ui_pages::view::layout(v.page())
                            == rustet_ui_pages::view::Layout::Fill =>
                    {
                        (
                            l.read(cx).scroll.0.borrow().base_handle.clone(),
                            l.entity_id(),
                        )
                    }
                    _ => (v.scroll.clone(), view.entity_id()),
                }
            };
            let max = handle.max_offset().y;
            let target = y.map_or(max, |y| px(y).min(max));
            handle.set_offset(point(px(0.), -target));
            App::notify(cx, notify);
        });
        self.wait(800).await;
    }
}

/// Every route a page links to: tiles, "see all", links, rows' songs.
fn routes(page: &Page) -> Vec<Route> {
    let mut out = Vec::new();
    let shelves = match page {
        Page::Browse(p) | Page::SearchLanding(p) => p.shelves.clone(),
        Page::Album(p) => p.shelves.clone(),
        Page::Artist(p) => p.shelves.clone(),
        Page::Playlist(p) => p.shelves.clone(),
        Page::Charts(p) => p.shelves.clone(),
        Page::MusicVideo(p) => p.shelves.clone(),
        Page::Curator(p) => p.shelves.clone(),
        _ => Vec::new(),
    };
    for s in &shelves {
        out.extend(s.see_all.clone());
        match &s.items {
            ShelfItems::Tiles { tiles, .. } | ShelfItems::Featured(tiles) => {
                out.extend(tiles.iter().filter_map(|t| t.route.clone()))
            }
            ShelfItems::Tracks(rows) => out.extend(rows.iter().filter_map(|r| {
                let id = r.id.catalog_id()?.to_owned();
                Some(match r.kind {
                    TrackKind::Song => Route::Song { id },
                    TrackKind::MusicVideo => Route::MusicVideo { id },
                })
            })),
            ShelfItems::Links(links) => out.extend(links.iter().filter_map(|l| match &l.target {
                rustet_app::pages::model::LinkTarget::Route(r) => Some(r.clone()),
                _ => None,
            })),
        }
    }
    if let Page::Search(p) = page {
        for s in &p.sections {
            if let Listing::Tiles(l) = &s.listing {
                out.extend(l.items.iter().filter_map(|t| t.route.clone()));
            }
        }
    }
    if let Page::Album(p) = page {
        out.extend(p.tracks().filter_map(|r| {
            Some(Route::Song {
                id: r.id.catalog_id()?.to_owned(),
            })
        }));
    }
    out
}

fn find(found: &[Route], f: impl Fn(&Route) -> bool) -> Option<Route> {
    found.iter().find(|r| f(r)).cloned()
}

async fn live(s: &Script<'_>) {
    let mut found: Vec<Route> = Vec::new();
    let visit = async |name: &str, route: Route, found: &mut Vec<Route>| {
        let outcome = s.open(name, route).await;
        if outcome != "timeout" {
            s.capture(name).await;
        }
        if let Some(p) = s.page() {
            found.extend(routes(&p));
        }
        outcome
    };
    visit("new", Route::New, &mut found).await;
    s.scroll_to(Some(900.)).await;
    s.shot("new-scrolled-dark-1360").await;
    let new_scroll = s.scroll_y();
    visit("radio", Route::Radio, &mut found).await;
    visit(
        "search-landing",
        Route::Search {
            term: String::new(),
        },
        &mut found,
    )
    .await;
    visit(
        "search",
        Route::Search {
            term: "radiohead".into(),
        },
        &mut found,
    )
    .await;
    // "See all" of the songs section.
    if let (Some(view), Some(Page::Search(p))) = (s.view(), s.page().as_deref()) {
        let songs = p
            .sections
            .iter()
            .position(|x| matches!(x.listing, Listing::Tracks(_)));
        if let Some(i) = songs {
            s.update(|_, w, cx| view.update(cx, |v, cx| v.expand(i, w, cx)));
            s.wait(1200).await;
            s.capture("search-see-all").await;
        }
    }
    visit("charts", Route::Charts { genre: None }, &mut found).await;
    if let Some(Page::Charts(p)) = s.page().as_deref()
        && let Some(g) = p.genres.get(1)
    {
        visit("charts-genre", g.route.clone(), &mut found).await;
    }
    visit(
        "album",
        Route::Album {
            id: "1097861387".into(),
        },
        &mut found,
    )
    .await;
    s.scroll_to(None).await;
    s.shot("album-end-dark-1360").await;

    // A context menu of the first row, from the keyboard: Tab into the list, Down, Menu.
    if let Some(view) = s.view() {
        let list = s
            .update(|_, _, cx| view.read(cx).track_lists().first().cloned())
            .flatten();
        if let Some(list) = list {
            s.scroll_to(Some(0.)).await;
            s.update(|_, w, cx| {
                let h = gpui::Focusable::focus_handle(list.read(cx), cx);
                w.focus(&h, cx);
            });
            s.keys("down menu").await;
            s.wait(400).await;
            s.shot("menu-row-dark-1360").await;
            s.keys("escape").await;
        }
    }
    // A tile's menu.
    if let Some(Page::Album(p)) = s.page().as_deref()
        && let Some(tile) = p.shelves.iter().find_map(|sh| match &sh.items {
            ShelfItems::Tiles { tiles, .. } => tiles.first().cloned(),
            _ => None,
        })
    {
        let entries = menu::tile_entries(
            &tile,
            &rustet_ui_pages::PageContext {
                storefront: "us".into(),
                ..Default::default()
            },
        );
        s.handle(PageEvent::Menu {
            entries,
            position: point(px(400.), px(420.)),
        });
        s.wait(400).await;
        s.shot("menu-tile-dark-1360").await;
        s.keys("escape").await;
    }
    // The modals.
    s.handle(PageEvent::NewPlaylist { tracks: Vec::new() });
    s.wait(500).await;
    s.keys("N i g h t space d r i v e").await;
    s.capture("modal-new-playlist").await;
    s.keys("escape").await;
    s.handle(PageEvent::Confirm {
        title: "Remove “OK Computer” from your library?".into(),
        message: "It also leaves your playlists. Downloads of it stay on this computer.".into(),
        answer: "Remove".into(),
        action: Action::DeletePlaylist { id: "none".into() },
        then_back: false,
    });
    s.wait(500).await;
    s.shot("modal-confirm-dark-1360").await;
    s.keys("escape").await;

    if let Some(song) = find(&found, |r| matches!(r, Route::Song { .. })) {
        visit("song", song, &mut found).await;
    }
    visit(
        "artist",
        Route::Artist {
            id: "657515".into(),
        },
        &mut found,
    )
    .await;
    s.scroll_to(Some(1200.)).await;
    s.shot("artist-scrolled-dark-1360").await;
    if let Some(see_all) = find(&found, |r| matches!(r, Route::ArtistView { .. })) {
        visit("artist-see-all", see_all, &mut found).await;
    }
    if let Some(mv) = find(&found, |r| matches!(r, Route::MusicVideo { .. })) {
        visit("music-video", mv, &mut found).await;
    }
    visit(
        "station",
        Route::Station {
            id: "ra.978194965".into(),
        },
        &mut found,
    )
    .await;
    if let Some(room) = find(&found, |r| matches!(r, Route::Room { .. })) {
        visit("room", room, &mut found).await;
    }
    if let Some(curator) = find(&found, |r| matches!(r, Route::Curator { .. })) {
        visit("curator", curator, &mut found).await;
    }
    if let Some(playlist) = find(&found, |r| matches!(r, Route::Playlist { .. })) {
        visit("playlist", playlist, &mut found).await;
    }

    // "Load more": an artist's singles, 20 at a time.
    let albums = Route::ArtistView {
        id: "159260351".into(),
        view: artist_view("singles").expect("a view"),
    };
    if s.open("load-more", albums).await == "ready" {
        let count = |s: &Script<'_>| {
            s.page().map_or(0, |p| match &*p {
                Page::List(l) => l.listing.len(),
                _ => 0,
            })
        };
        let before = count(s);
        let continues = s.page().is_some_and(|p| {
            rustet_ui_pages::view::has_more(&p, rustet_app::nav::MoreTarget::Main)
        });
        report(
            "load_more_start",
            json!({"items": before, "continues": continues}),
        );
        s.scroll_to(None).await;
        let at_end = s.scroll_y();
        s.shot("load-more-asked-dark-1360").await;
        s.until(20_000, |d, cx| {
            d.area
                .as_ref()
                .is_some_and(|a| matches!(a.read(cx).state().more, MoreState::Idle))
        })
        .await;
        s.wait(1500).await;
        let after = count(s);
        report(
            "load_more",
            json!({"before": before, "after": after, "scroll_at_end": at_end, "scroll_after": s.scroll_y()}),
        );
        s.shot("load-more-done-dark-1360").await;
    }

    // Signed out: the pages that need the account show the sign-in state.
    visit("home-signed-out", Route::Home, &mut found).await;
    visit(
        "library-signed-out",
        Route::Library(LibrarySection::Albums),
        &mut found,
    )
    .await;
    // A failed load: an id Apple does not have.
    visit("failed", Route::Album { id: "1".into() }, &mut found).await;

    // Back returns to the scrolled New page.
    s.open("new-again", Route::New).await;
    s.scroll_to(Some(900.)).await;
    let left_at = s.scroll_y();
    s.open(
        "album-again",
        Route::Album {
            id: "1097861387".into(),
        },
    )
    .await;
    s.update(|d, _, _| d.nav.as_ref().map(|n| n.back()));
    s.wait(1500).await;
    let back_at = s.scroll_y();
    report(
        "back_scroll",
        json!({"first_visit": new_scroll, "left_at": left_at, "after_back": back_at}),
    );
    s.shot("back-restored-dark-1360").await;
}

/// The first tile route of the shown library grid.
fn first_tile(page: &Page) -> Option<Route> {
    match page {
        Page::Library(p) => match &p.listing {
            Listing::Tiles(l) => l.items.first()?.route.clone(),
            Listing::Tracks(_) => None,
        },
        _ => None,
    }
}

async fn account(s: &Script<'_>) {
    let mut album = None;
    let mut playlist = None;
    for (name, route) in [
        ("home", Route::Home),
        (
            "library-recently-added",
            Route::Library(LibrarySection::RecentlyAdded),
        ),
        (
            "library-recently-played",
            Route::Library(LibrarySection::RecentlyPlayed),
        ),
        ("library-albums", Route::Library(LibrarySection::Albums)),
        ("library-songs", Route::Library(LibrarySection::Songs)),
        ("library-artists", Route::Library(LibrarySection::Artists)),
        (
            "library-music-videos",
            Route::Library(LibrarySection::MusicVideos),
        ),
        (
            "library-playlists",
            Route::Library(LibrarySection::Playlists),
        ),
        (
            "library-search",
            Route::LibrarySearch {
                term: "love".into(),
            },
        ),
    ] {
        if s.open(name, route.clone()).await != "timeout" {
            s.capture(name).await;
        }
        match route {
            Route::Library(LibrarySection::Albums) => album = s.page().and_then(|p| first_tile(&p)),
            Route::Library(LibrarySection::Playlists) => {
                playlist = s.page().and_then(|p| first_tile(&p))
            }
            _ => {}
        }
    }
    // "Load more" of the songs: the end of the first 100.
    s.open("library-songs-more", Route::Library(LibrarySection::Songs))
        .await;
    let before = s.page().map_or(0, |p| p.len_hint());
    s.scroll_to(None).await;
    s.wait(3000).await;
    report(
        "library_songs_more",
        json!({"before": before, "after": s.page().map_or(0, |p| p.len_hint())}),
    );
    for (name, route) in [("library-album", album), ("library-playlist", playlist)] {
        let Some(route) = route else { continue };
        if s.open(name, route).await == "ready" {
            s.capture(name).await;
            if let Some(view) = s.view() {
                let list = s
                    .update(|_, _, cx| view.read(cx).track_lists().first().cloned())
                    .flatten();
                if let Some(list) = list {
                    s.update(|_, w, cx| {
                        let h = gpui::Focusable::focus_handle(list.read(cx), cx);
                        w.focus(&h, cx);
                    });
                    s.keys("down menu").await;
                    s.wait(300).await;
                    s.shot(&format!("{name}-row-menu-dark-1360")).await;
                    s.keys("escape").await;
                }
            }
        }
    }
}

async fn fixtures(s: &Script<'_>) {
    let pages = [
        ("home", Route::Home),
        (
            "library-recently-added",
            Route::Library(LibrarySection::RecentlyAdded),
        ),
        (
            "library-recently-played",
            Route::Library(LibrarySection::RecentlyPlayed),
        ),
        ("library-albums", Route::Library(LibrarySection::Albums)),
        ("library-songs", Route::Library(LibrarySection::Songs)),
        ("library-artists", Route::Library(LibrarySection::Artists)),
        (
            "library-music-videos",
            Route::Library(LibrarySection::MusicVideos),
        ),
        (
            "library-album",
            Route::LibraryAlbum {
                id: MY_ALBUM.into(),
            },
        ),
        ("library-search", Route::LibrarySearch { term: "a".into() }),
        (
            "library-playlists",
            Route::Library(LibrarySection::Playlists),
        ),
    ];
    for (name, route) in pages {
        if s.open(name, route).await != "timeout" {
            s.capture(name).await;
        }
    }
    // A folder of the Playlists page.
    s.handle(PageEvent::OpenFolder {
        id: Some("p.FolderRoad".into()),
        name: "Road trips".into(),
    });
    s.wait(1500).await;
    let shown = s
        .view()
        .and_then(|v| s.update(|_, _, cx| v.read(cx).folder_tiles(cx)));
    report("folder_playlists", json!(shown));
    s.shot("library-playlists-folder-dark-1360").await;

    // Library songs: "load more" at the end of the first 100.
    s.open("library-songs-more", Route::Library(LibrarySection::Songs))
        .await;
    s.scroll_to(None).await;
    s.wait(1500).await;
    let count = s.page().map_or(0, |p| p.len_hint());
    report("library_songs_after_end", json!(count));

    // The editable playlist: its row menu (Move, Remove from playlist, Add to playlist ▸).
    if s.open(
        "library-playlist",
        Route::LibraryPlaylist {
            id: MY_PLAYLIST.into(),
        },
    )
    .await
        == "ready"
    {
        s.capture("library-playlist").await;
        // The playing track's mark, as the player state would set it.
        if let (Some(view), Some(Page::LibraryPlaylist(p))) = (s.view(), s.page().as_deref()) {
            let row = &p.tracks.items[2];
            let ctx = rustet_ui_pages::PageContext {
                signed_in: true,
                storefront: "us".into(),
                playlists: crate::fixtures::playlist_choices().into(),
                playing: row.id.catalog_id().map(|c| rustet_ui_pages::Playing {
                    catalog_id: c.to_owned(),
                    library_id: row.id.library_id().map(str::to_owned),
                }),
            };
            s.update(|_, _, cx| view.update(cx, |v, cx| v.set_context(ctx, cx)));
            s.wait(300).await;
            s.shot("library-playlist-playing-dark-1360").await;
        }
        if let Some(view) = s.view() {
            let list = s
                .update(|_, _, cx| view.read(cx).track_lists().first().cloned())
                .flatten();
            if let Some(list) = list {
                s.update(|_, w, cx| {
                    let h = gpui::Focusable::focus_handle(list.read(cx), cx);
                    w.focus(&h, cx);
                });
                s.keys("down down menu").await;
                s.wait(300).await;
                s.shot("library-playlist-row-menu-dark-1360").await;
                // Down to "Add to playlist", Right opens its sub-menu.
                s.keys("down down down right").await;
                s.wait(300).await;
                s.shot("library-playlist-submenu-dark-1360").await;
                s.keys("escape escape escape").await;
                // Several rows: Shift+Down, then the menu.
                s.update(|_, w, cx| {
                    let h = gpui::Focusable::focus_handle(list.read(cx), cx);
                    w.focus(&h, cx);
                });
                s.keys("shift-down shift-down menu").await;
                s.wait(300).await;
                s.shot("library-playlist-multi-menu-dark-1360").await;
                s.keys("escape").await;
            }
        }
        if let Some(page) = s.page() {
            let entries = menu::page_entries(
                &page,
                &Route::LibraryPlaylist {
                    id: MY_PLAYLIST.into(),
                },
                &rustet_ui_pages::PageContext {
                    signed_in: true,
                    storefront: "us".into(),
                    ..Default::default()
                },
            );
            report("library_playlist_more", json!(menu::labels(&entries)));
            s.handle(PageEvent::Menu {
                entries,
                position: point(px(700.), px(330.)),
            });
            s.wait(300).await;
            s.shot("library-playlist-more-menu-dark-1360").await;
            s.keys("escape").await;
        }
        s.handle(PageEvent::RenamePlaylist {
            id: MY_PLAYLIST.into(),
            name: "Late night drive".into(),
        });
        s.wait(500).await;
        s.capture("modal-rename").await;
        s.keys("escape").await;
        s.handle(PageEvent::Confirm {
            title: "Delete “Late night drive”?".into(),
            message:
                "The playlist leaves your library on every device. Its songs stay in your library."
                    .into(),
            answer: "Delete".into(),
            action: Action::DeletePlaylist { id: "none".into() },
            then_back: false,
        });
        s.wait(500).await;
        s.capture("modal-delete").await;
        s.keys("escape").await;
        s.handle(PageEvent::PickPlaylist { tracks: Vec::new() });
        s.wait(500).await;
        s.capture("modal-picker").await;
        s.keys("escape").await;
    }
}

trait LenHint {
    fn len_hint(&self) -> usize;
}

impl LenHint for Page {
    fn len_hint(&self) -> usize {
        match self {
            Page::Library(p) => p.listing.len(),
            _ => 0,
        }
    }
}

/// One frame of the bench's scrolling: the library list moves `speed` px, and wraps.
pub fn autoscroll_step(demo: &mut Demo, speed: f32, cx: &mut gpui::Context<Demo>) {
    let Some(view) = demo
        .area
        .as_ref()
        .and_then(|a| a.read(cx).current().cloned())
    else {
        return;
    };
    let Some(list) = view.read(cx).track_lists().first().cloned() else {
        return;
    };
    let base = list.read(cx).scroll.0.borrow().base_handle.clone();
    let max = base.max_offset().y;
    let mut offset = base.offset();
    offset.y -= px(speed);
    if -offset.y > max {
        offset.y = px(0.);
    }
    base.set_offset(offset);
    App::notify(cx, list.entity_id());
}

async fn bench(s: &Script<'_>) {
    if s.open("bench-songs", Route::Library(LibrarySection::Songs))
        .await
        != "ready"
    {
        return;
    }
    let rows = s.page().map_or(0, |p| p.len_hint());
    s.shot("bench-songs-dark-1360").await;
    s.update(|d, _, cx| {
        d.frame_stats.reset();
        d.autoscroll = Some(40.);
        cx.notify();
    });
    s.wait(12_000).await;
    let stats = s.update(|d, _, cx| {
        d.autoscroll = None;
        cx.notify();
        (
            d.frame_stats.intervals(),
            d.frame_stats.work(),
            d.frame_stats.late(Duration::from_micros(25_000)),
        )
    });
    if let Some((intervals, work, late)) = stats {
        report(
            "list_library_songs",
            json!({
                "rows": rows,
                "speed_px_per_frame": 40,
                "interval_ms": intervals.map(|s| json!({"count": s.count, "p50": s.p50, "p95": s.p95, "p99": s.p99, "max": s.max})),
                "cpu_work_ms": work.map(|s| json!({"p50": s.p50, "p95": s.p95, "p99": s.p99, "max": s.max})),
                "frames_over_25ms": late,
            }),
        );
    }
}
