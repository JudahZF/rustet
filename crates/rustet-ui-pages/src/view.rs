//! [`PageView`]: one loaded page, drawn from its model with the kit's content views.
//!
//! Two layouts:
//!
//! - **Scroll**: the page scrolls as one (header, track lists as tall as their rows, shelves).
//!   Album, playlist, artist, song, video, station, curator, browse pages, charts, search.
//! - **Fill**: a title over a list or grid that scrolls itself and draws only what is on
//!   screen. Library sections and "see all" lists, which can have thousands of items.
//!
//! "Load more" starts when the user is within a screen of the end of a list that continues
//! ([`near_end`]); the items are appended, so the scroll position does not move.
//!
//! The page frame (`docs/native-ui-look.md`): a page draws from the top of the content area,
//! under the window's 52 px strip, so it starts its content `theme.size.strip` down (the
//! artist hero alone draws under the strip) and ends `theme.size.player_clearance` above its
//! bottom, clear of the player pill. Side gutter 40 px; page titles 34 / 700. Search results
//! have no title: the search field in the strip is their title.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
    ParentElement, Pixels, Render, Role, ScrollHandle, SharedString, Styled, Subscription, Window,
    actions, canvas, div, prelude::*, px,
};
use rustet_ampapi::api::ResourceRef;
use rustet_app::library::PlaylistEntry;
use rustet_app::nav::{MoreState, MoreTarget};
use rustet_app::pages::catalog::{Disc, SearchGroup, SearchPage};
use rustet_app::pages::model::{
    Date, Header, Listing, Shelf, ShelfItems, Tile, TileKind, TileSize, TrackRow,
};
use rustet_app::pages::play::{self, How, PlayError, PlayTarget};
use rustet_app::pages::{LibraryPlaylistPage, Page};
use rustet_app::route::{LibrarySection, Route};
use rustet_player::model::Command;
use rustet_ui::components::{
    Button, EmptyStateView, ErrorStateView, Placeholder, Scrollbar, Select, SelectEvent,
};
use rustet_ui::content::{
    ContentEvent, DraggedTracks, HeaderAction, OnContent, PageHeader, ShelfView, TileGrid,
    TileView, TrackListOptions, TrackListView,
};
use rustet_ui::format;
use rustet_ui::icons::Icon;
use rustet_ui::theme::{ActiveTheme, BOLD, SEMIBOLD, Theme};

use crate::context::PageContext;
use crate::event::{DownloadRequest, PageEvent, PlayRequest, Sink};
use crate::menu::{self, Editing};

actions!(
    page,
    [
        FocusNext,
        FocusPrevious,
        ScrollDown,
        ScrollUp,
        PageDown,
        PageUp
    ]
);

const CONTEXT: &str = "Page";

/// Binds the page's keys: Left and Right move between tiles, Up and Down scroll outside lists.
pub fn bind_keys(cx: &mut App) {
    let outside = "Page && !TrackList && !TextInput && !Select && !Menu && !SearchField";
    cx.bind_keys([
        KeyBinding::new("right", FocusNext, Some(outside)),
        KeyBinding::new("left", FocusPrevious, Some(outside)),
        KeyBinding::new("down", ScrollDown, Some(outside)),
        KeyBinding::new("up", ScrollUp, Some(outside)),
        KeyBinding::new("pagedown", PageDown, Some(outside)),
        KeyBinding::new("pageup", PageUp, Some(outside)),
    ]);
}

/// The command that plays `page`'s own tracks the way `how` says; `None` for a page with no
/// tracks of its own (browse, search, library tiles).
pub fn page_play(page: &Page, how: How) -> Option<Result<Command, PlayError>> {
    Some(match page {
        Page::Album(p) => p.play(how),
        Page::Playlist(p) => p.play(how),
        Page::Artist(p) => p.play_top_songs(how),
        Page::Song(p) => play::play(std::slice::from_ref(&p.track), None, how),
        Page::MusicVideo(p) => play::play(std::slice::from_ref(&p.video), None, how),
        Page::LibraryAlbum(p) => p.play(how),
        Page::LibraryPlaylist(p) => p.play(how),
        Page::Library(p) if matches!(p.listing, Listing::Tracks(_)) => p.play(how),
        _ => return None,
    })
}

/// Whether a list scrolled to `offset` (negative down, as gpui keeps it) of `max` with a
/// viewport of `viewport` is within a screen of its end. A list too short to scroll is.
pub fn near_end(offset: Pixels, max: Pixels, viewport: Pixels) -> bool {
    let remaining = max + offset;
    remaining <= viewport
}

/// Which page layout a page uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Scroll,
    Fill,
}

pub fn layout(page: &Page) -> Layout {
    match page {
        Page::Library(_) | Page::List(_) => Layout::Fill,
        _ => Layout::Scroll,
    }
}

/// How a track list's rows play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListRole {
    /// The page's own tracks from `offset` (a disc of an album, a playlist, library songs).
    Page { offset: usize },
    /// Rows on their own: a "see all" list.
    Rows,
}

struct ListSpec {
    role: ListRole,
    rows: Arc<[TrackRow]>,
    options: TrackListOptions,
}

fn list_options(label: &str, artwork: bool, album: bool, fit: bool) -> TrackListOptions {
    TrackListOptions {
        label: label.to_owned().into(),
        artwork,
        album,
        fit_content: fit,
        album_artist: None,
    }
}

/// An album's discs: numbered rows, the artist line only where it is not the album's.
fn disc_specs(discs: &[Disc], header: &Header) -> Vec<ListSpec> {
    let album_artist = header.subtitle.as_ref().map(|s| s.text.clone().into());
    let mut offset = 0;
    discs
        .iter()
        .map(|d| {
            let spec = ListSpec {
                role: ListRole::Page { offset },
                rows: d.tracks.clone().into(),
                options: TrackListOptions {
                    album_artist: album_artist.clone(),
                    ..list_options(&format!("Disc {}", d.number), false, false, true)
                },
            };
            offset += d.tracks.len();
            spec
        })
        .collect()
}

/// The track lists of a page.
fn list_specs(page: &Page) -> Vec<ListSpec> {
    match page {
        Page::Album(p) => disc_specs(&p.discs, &p.header),
        Page::LibraryAlbum(p) => disc_specs(&p.discs, &p.header),
        Page::Playlist(p) => vec![ListSpec {
            role: ListRole::Page { offset: 0 },
            rows: p.tracks.items.clone().into(),
            options: list_options("Songs", true, true, true),
        }],
        Page::LibraryPlaylist(p) => vec![ListSpec {
            role: ListRole::Page { offset: 0 },
            rows: p.tracks.items.clone().into(),
            options: list_options("Songs", true, true, true),
        }],
        Page::Library(p) => match &p.listing {
            Listing::Tracks(l) => vec![ListSpec {
                role: ListRole::Page { offset: 0 },
                rows: l.items.clone().into(),
                options: list_options(section_title(p.section), true, true, false),
            }],
            Listing::Tiles(_) => Vec::new(),
        },
        Page::List(p) => match &p.listing {
            Listing::Tracks(l) => vec![ListSpec {
                role: ListRole::Rows,
                rows: l.items.clone().into(),
                options: list_options(p.title.as_deref().unwrap_or("Songs"), true, true, false),
            }],
            Listing::Tiles(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The tiles of a page drawn as a virtualised grid.
fn grid_tiles(page: &Page) -> Option<(String, Arc<[Tile]>)> {
    match page {
        Page::Library(p) => match &p.listing {
            Listing::Tiles(l) => {
                Some((section_title(p.section).to_owned(), l.items.clone().into()))
            }
            Listing::Tracks(_) => None,
        },
        Page::List(p) => match &p.listing {
            Listing::Tiles(l) => {
                Some((p.title.clone().unwrap_or_default(), l.items.clone().into()))
            }
            Listing::Tracks(_) => None,
        },
        _ => None,
    }
}

/// Whether the page's list `target` continues.
pub fn has_more(page: &Page, target: MoreTarget) -> bool {
    let listing = |l: &Listing| match l {
        Listing::Tiles(l) => l.more.is_some(),
        Listing::Tracks(l) => l.more.is_some(),
    };
    match (page, target) {
        (Page::Playlist(p), MoreTarget::Main) => p.tracks.more.is_some(),
        (Page::LibraryPlaylist(p), MoreTarget::Main) => p.tracks.more.is_some(),
        (Page::Curator(p), MoreTarget::Main) => p.playlists.more.is_some(),
        (Page::List(p), MoreTarget::Main) => listing(&p.listing),
        (Page::Library(p), MoreTarget::Main) => listing(&p.listing),
        (Page::Search(p) | Page::LibrarySearch(p), MoreTarget::Section(i)) => {
            p.sections.get(i).is_some_and(|s| listing(&s.listing))
        }
        _ => false,
    }
}

/// How many items the list `target` has now (a load-more answer changes it).
fn item_count(page: &Page, target: MoreTarget) -> usize {
    match (page, target) {
        (Page::Playlist(p), MoreTarget::Main) => p.tracks.items.len(),
        (Page::LibraryPlaylist(p), MoreTarget::Main) => p.tracks.items.len(),
        (Page::Curator(p), MoreTarget::Main) => p.playlists.items.len(),
        (Page::List(p), MoreTarget::Main) => p.listing.len(),
        (Page::Library(p), MoreTarget::Main) => p.listing.len(),
        (Page::Search(p) | Page::LibrarySearch(p), MoreTarget::Section(i)) => {
            p.sections.get(i).map_or(0, |s| s.listing.len())
        }
        _ => 0,
    }
}

pub fn section_title(section: LibrarySection) -> &'static str {
    match section {
        LibrarySection::RecentlyAdded => "Recently Added",
        LibrarySection::RecentlyPlayed => "Recently Played",
        LibrarySection::Albums => "Albums",
        LibrarySection::Songs => "Songs",
        LibrarySection::Artists => "Artists",
        LibrarySection::Playlists => "Playlists",
        LibrarySection::MusicVideos => "Music Videos",
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// "12 March 2026", "March 2026" or "2026".
pub fn date_text(d: Date) -> String {
    match (d.month, d.day) {
        (Some(m), Some(day)) => format!("{day} {} {}", MONTHS[m as usize - 1], d.year),
        (Some(m), None) => format!("{} {}", MONTHS[m as usize - 1], d.year),
        _ => d.year.to_string(),
    }
}

fn songs(n: usize) -> String {
    if n == 1 {
        "1 song".to_owned()
    } else {
        format!("{n} songs")
    }
}

/// The facts under a header's subtitle, as Apple's meta line ("Rock · 2000"; the header adds
/// the quality marks). Song counts and lengths are in [`summary`], under the tracks.
pub fn facts(page: &Page) -> Vec<String> {
    let mut out = Vec::new();
    match page {
        Page::Album(p) => {
            out.extend(p.info.genre.clone());
            match (p.info.prerelease, p.info.released) {
                (true, Some(d)) => out.push(format!("Coming {}", date_text(d))),
                (false, Some(d)) => out.push(d.year.to_string()),
                _ => {}
            }
        }
        Page::Playlist(p) => {
            out.extend(p.updated.map(|d| format!("Updated {}", date_text(d))));
        }
        Page::Artist(p) => out.extend(p.genre.clone()),
        Page::Song(p) => {
            out.extend(p.genre.clone());
            out.extend(p.released.map(|d| d.year.to_string()));
            out.extend(p.track.duration.map(format::duration));
        }
        Page::MusicVideo(p) => {
            out.extend(p.genre.clone());
            out.extend(p.released.map(|d| d.year.to_string()));
            out.extend(p.video_traits.iter().cloned());
        }
        Page::Station(p) => {
            out.push(if p.live { "Live radio" } else { "Station" }.to_owned());
            if p.requires_subscription {
                out.push("Needs an Apple Music subscription".to_owned());
            }
        }
        Page::Curator(p) => out.extend(p.kind.clone()),
        _ => {}
    }
    out
}

/// The line under an album's or a playlist's tracks: "27 songs, 1 hour 19 minutes".
pub fn summary(page: &Page) -> Option<String> {
    let with_length =
        |count: usize, length| format!("{}, {}", songs(count), format::length(length));
    Some(match page {
        Page::Album(p) => with_length(p.tracks().count(), p.duration),
        Page::LibraryAlbum(p) => with_length(
            p.tracks().count(),
            p.tracks().filter_map(|r| r.duration).sum(),
        ),
        Page::Playlist(p) => songs(p.track_count.map_or(p.tracks.items.len(), |c| c as usize)),
        Page::LibraryPlaylist(p) if !p.tracks.items.is_empty() => songs(p.tracks.items.len()),
        _ => return None,
    })
}

/// The header actions a page offers, in the header's order.
pub fn header_actions(page: &Page, ctx: &PageContext) -> Vec<HeaderAction> {
    use HeaderAction::*;
    let playable = page_play(page, How::InOrder).is_some_and(|r| r.is_ok());
    let mut out = Vec::new();
    match page {
        Page::Station(p) if p.play.is_some() => out.push(Play),
        Page::Song(_) | Page::MusicVideo(_) if playable => out.push(Play),
        // The artist hero puts play, the key circle, between the others.
        Page::Artist(_) if playable => out.extend([Shuffle, Play]),
        _ if playable => out.extend([Play, Shuffle]),
        _ => {}
    }
    if ctx.signed_in && menu::page_item(page).is_some() {
        out.push(AddToLibrary);
    }
    if menu::page_downloadable(page) {
        out.push(Download);
    }
    out.push(More);
    out
}

struct TrackList {
    view: Entity<TrackListView>,
    role: ListRole,
}

/// A search section shown in full.
struct Expanded {
    section: usize,
    list: Option<Entity<TrackListView>>,
    grid: Option<Entity<TileGrid>>,
    _subscriptions: Vec<Subscription>,
}

/// The Playlists page's folders: the top level's folders, and the open folder.
#[derive(Default)]
struct Folders {
    root: Vec<PlaylistEntry>,
    /// The open folders, outermost first.
    trail: Vec<(String, String)>,
    /// The open folder's entries; `None` while they load. Its playlists show in the page's
    /// grid.
    open: Option<Vec<PlaylistEntry>>,
}

/// One loaded page.
pub struct PageView {
    route: Route,
    page: Arc<Page>,
    ctx: PageContext,
    sink: Sink,
    more: MoreState,
    focus: FocusHandle,
    /// The scroll layout's scroll position (kept with the view for Back).
    pub scroll: ScrollHandle,
    lists: Vec<TrackList>,
    grid: Option<Entity<TileGrid>>,
    genre: Option<Entity<Select>>,
    expanded: Option<Expanded>,
    folders: Folders,
    /// Where the editable playlist's rows are on screen, for drops.
    drop_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The item count when "load more" was last asked for, so it is asked once per page.
    asked_more: Option<(MoreTarget, usize)>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for PageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PageView {
    pub fn new(
        route: Route,
        page: Arc<Page>,
        ctx: PageContext,
        sink: Sink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            route,
            page: page.clone(),
            ctx,
            sink,
            more: MoreState::Idle,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            lists: Vec::new(),
            grid: None,
            genre: None,
            expanded: None,
            folders: Folders::default(),
            drop_bounds: Rc::new(Cell::new(Bounds::default())),
            asked_more: None,
            _subscriptions: Vec::new(),
        };
        view.build(window, cx);
        view
    }

    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn page(&self) -> &Arc<Page> {
        &self.page
    }

    /// The track lists, in page order (tests, the area's focus restore).
    pub fn track_lists(&self) -> Vec<Entity<TrackListView>> {
        self.lists.iter().map(|l| l.view.clone()).collect()
    }

    pub fn grid(&self) -> Option<&Entity<TileGrid>> {
        self.grid.as_ref()
    }

    /// The search section shown in full, if any.
    pub fn expanded_section(&self) -> Option<usize> {
        self.expanded.as_ref().map(|e| e.section)
    }

    /// The playlists shown in the open folder (0 at the top level).
    pub fn folder_tiles(&self, cx: &App) -> usize {
        match (self.folders.trail.is_empty(), &self.grid) {
            (false, Some(grid)) => grid.read(cx).tiles().len(),
            _ => 0,
        }
    }

    /// Builds the content entities of the page from scratch.
    fn build(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        self.lists = list_specs(&self.page)
            .into_iter()
            .map(|spec| {
                let view = cx.new(|cx| TrackListView::new(spec.rows, spec.options, cx));
                let role = spec.role;
                subscriptions.push(cx.subscribe_in(
                    &view,
                    window,
                    move |this, _, e: &ContentEvent, w, cx| this.on_content(e, Some(role), w, cx),
                ));
                if layout(&self.page) == Layout::Fill {
                    subscriptions.push(cx.observe_in(&view, window, |this, list, w, cx| {
                        let handle = list.read(cx).scroll.0.borrow().base_handle.clone();
                        this.check_more(&handle, MoreTarget::Main, w, cx);
                    }));
                }
                TrackList { view, role }
            })
            .collect();
        self.grid = grid_tiles(&self.page).map(|(label, tiles)| {
            let grid = cx.new(|_| TileGrid::new(label, tiles));
            subscriptions.push(cx.subscribe_in(
                &grid,
                window,
                |this, _, e: &ContentEvent, w, cx| this.on_content(e, None, w, cx),
            ));
            subscriptions.push(cx.observe_in(&grid, window, |this, grid, w, cx| {
                if this.folders.trail.is_empty() {
                    let handle = grid.read(cx).scroll.0.borrow().base_handle.clone();
                    this.check_more(&handle, MoreTarget::Main, w, cx);
                }
            }));
            grid
        });
        self.genre = match &*self.page {
            Page::Charts(p) if !p.genres.is_empty() => {
                let mut options: Vec<SharedString> = vec!["All genres".into()];
                options.extend(p.genres.iter().map(|g| SharedString::from(g.name.clone())));
                let selected = p
                    .genres
                    .iter()
                    .position(|g| matches!(&g.route, Route::Charts { genre } if *genre == p.genre))
                    .map_or(0, |i| i + 1);
                let select = cx.new(|cx| Select::new("Genre", options, selected, cx));
                subscriptions.push(cx.subscribe_in(
                    &select,
                    window,
                    |this, _, e: &SelectEvent, w, cx| {
                        let SelectEvent::Changed(ix) = *e;
                        let route = match (&*this.page, ix) {
                            (_, 0) => Route::Charts { genre: None },
                            (Page::Charts(p), ix) => match p.genres.get(ix - 1) {
                                Some(g) => g.route.clone(),
                                None => return,
                            },
                            _ => return,
                        };
                        (this.sink)(PageEvent::Navigate(route), w, cx);
                    },
                ));
                Some(select)
            }
            _ => None,
        };
        self._subscriptions = subscriptions;
        self.expanded = None;
        self.mark_playing(cx);
    }

    /// A new model for the same route ("load more", a reload in place). Lists keep their
    /// scroll position; their rows change only when they differ.
    pub fn set_page(&mut self, page: Arc<Page>, window: &mut Window, cx: &mut Context<Self>) {
        if Arc::ptr_eq(&page, &self.page) {
            return;
        }
        let specs = list_specs(&page);
        let same_shape = std::mem::discriminant(&*page) == std::mem::discriminant(&*self.page)
            && specs.len() == self.lists.len()
            && specs.iter().zip(&self.lists).all(|(s, l)| s.role == l.role)
            && grid_tiles(&page).is_some() == self.grid.is_some();
        self.page = page;
        self.asked_more = None;
        if !same_shape {
            self.build(window, cx);
            cx.notify();
            return;
        }
        for (spec, list) in specs.into_iter().zip(&self.lists) {
            list.view.update(cx, |l, cx| {
                if l.rows()[..] != spec.rows[..] {
                    l.set_rows(spec.rows, cx);
                }
            });
        }
        // An open playlist folder keeps the grid until the user leaves it.
        if let (Some(grid), Some((_, tiles)), true) = (
            &self.grid,
            grid_tiles(&self.page),
            self.folders.trail.is_empty(),
        ) {
            grid.update(cx, |g, cx| {
                if g.tiles()[..] != tiles[..] {
                    g.set_tiles(tiles, cx);
                }
            });
        }
        if let Some(expanded) = &self.expanded {
            let section = expanded.section;
            let listing = match &*self.page {
                Page::Search(p) | Page::LibrarySearch(p) => {
                    p.sections.get(section).map(|s| s.listing.clone())
                }
                _ => None,
            };
            match (listing, &expanded.list, &expanded.grid) {
                (Some(Listing::Tracks(l)), Some(list), _) => {
                    list.update(cx, |v, cx| v.set_rows(l.items.into(), cx));
                }
                (Some(Listing::Tiles(l)), _, Some(grid)) => {
                    grid.update(cx, |g, cx| g.set_tiles(l.items.into(), cx));
                }
                _ => {}
            }
        }
        self.mark_playing(cx);
        cx.notify();
    }

    pub fn set_context(&mut self, ctx: PageContext, cx: &mut Context<Self>) {
        if ctx != self.ctx {
            let playing_changed = ctx.playing != self.ctx.playing;
            self.ctx = ctx;
            if playing_changed {
                self.mark_playing(cx);
            }
            cx.notify();
        }
    }

    pub fn set_more(&mut self, more: MoreState, cx: &mut Context<Self>) {
        if more != self.more {
            self.more = more;
            cx.notify();
        }
    }

    /// Marks the playing track in every list.
    fn mark_playing(&self, cx: &mut Context<Self>) {
        let playing = self.ctx.playing.clone();
        let lists = self
            .lists
            .iter()
            .map(|l| l.view.clone())
            .chain(self.expanded.as_ref().and_then(|e| e.list.clone()));
        for list in lists {
            list.update(cx, |l, cx| {
                let id = playing.as_ref().and_then(|p| p.row_id(l.rows()));
                l.set_now_playing(id, cx);
            });
        }
    }

    /// Asks for more of `target` when `handle` is near its end, once per page of items.
    fn check_more(
        &mut self,
        handle: &ScrollHandle,
        target: MoreTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !has_more(&self.page, target) || !matches!(self.more, MoreState::Idle) {
            return;
        }
        let viewport = handle.bounds().size.height;
        if viewport <= px(0.) || !near_end(handle.offset().y, handle.max_offset().y, viewport) {
            return;
        }
        let count = item_count(&self.page, target);
        if self.asked_more == Some((target, count)) {
            return;
        }
        self.asked_more = Some((target, count));
        let sink = self.sink.clone();
        cx.defer_in(window, move |_, w, cx| {
            sink(PageEvent::LoadMore(target), w, cx)
        });
    }

    fn on_event(&self, cx: &mut Context<Self>) -> OnContent {
        let this = cx.entity().downgrade();
        Rc::new(move |e, w, cx| {
            this.update(cx, |this, cx| this.on_content(e, None, w, cx))
                .ok();
        })
    }

    fn send(&self, event: PageEvent, window: &mut Window, cx: &mut App) {
        (self.sink)(event, window, cx);
    }

    /// The editable library playlist whose every track is loaded.
    fn editable(&self) -> Option<&LibraryPlaylistPage> {
        match &*self.page {
            Page::LibraryPlaylist(p)
                if p.editable && p.tracks.more.is_none() && self.ctx.signed_in =>
            {
                Some(p)
            }
            _ => None,
        }
    }

    fn on_content(
        &mut self,
        e: &ContentEvent,
        role: Option<ListRole>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match e {
            ContentEvent::Navigate(route) => {
                self.send(PageEvent::Navigate(route.clone()), window, cx)
            }
            ContentEvent::OpenExternal(url) => {
                self.send(PageEvent::OpenExternal(url.clone()), window, cx)
            }
            ContentEvent::Play(target) => self.send(
                PageEvent::Play(PlayRequest::Target(target.clone(), How::InOrder)),
                window,
                cx,
            ),
            ContentEvent::PlayTracks { rows, start } => {
                let intent = self.play_rows(rows, *start, role);
                self.send(PageEvent::Play(PlayRequest::Intent(intent)), window, cx);
            }
            ContentEvent::TileMenu { tile, position } => {
                let entries = menu::tile_entries(tile, &self.ctx);
                self.send(
                    PageEvent::Menu {
                        entries,
                        position: *position,
                    },
                    window,
                    cx,
                );
            }
            ContentEvent::TracksMenu { rows, position } => {
                let editing = self.editable().and_then(|page| {
                    // Several rows: only the playlist matters (removal), not a row's place.
                    let [row] = &rows[..] else {
                        return Some(Editing {
                            page,
                            index: usize::MAX,
                        });
                    };
                    let index = page.tracks.items.iter().position(|r| r.id == row.id)?;
                    Some(Editing { page, index })
                });
                let entries = menu::rows_menu(rows, &self.ctx, editing);
                self.send(
                    PageEvent::Menu {
                        entries,
                        position: *position,
                    },
                    window,
                    cx,
                );
            }
        }
    }

    /// The command for a row played in a list or shelf: the page's own play order when the
    /// rows are the page's tracks, else the rows on their own.
    fn play_rows(
        &self,
        rows: &Arc<[TrackRow]>,
        start: usize,
        role: Option<ListRole>,
    ) -> Result<Command, PlayError> {
        if let Some(ListRole::Page { offset }) = role
            && let Some(intent) = page_play(&self.page, How::From(offset + start))
        {
            return intent;
        }
        if let Page::Artist(p) = &*self.page
            && p.top_songs() == &rows[..]
        {
            return p.play_top_songs(How::From(start));
        }
        play::play(rows, None, How::From(start))
    }

    fn on_header(&mut self, action: HeaderAction, window: &mut Window, cx: &mut Context<Self>) {
        let event = match action {
            HeaderAction::Play | HeaderAction::Shuffle => {
                let how = if action == HeaderAction::Play {
                    How::InOrder
                } else {
                    How::Shuffled
                };
                match (&*self.page, page_play(&self.page, how)) {
                    (Page::Station(p), _) => match &p.play {
                        Some(target) => PageEvent::Play(PlayRequest::Target(target.clone(), how)),
                        None => return,
                    },
                    (_, Some(intent)) => PageEvent::Play(PlayRequest::Intent(intent)),
                    (_, None) => return,
                }
            }
            HeaderAction::AddToLibrary => match menu::page_item(&self.page) {
                Some(item) => {
                    PageEvent::Act(rustet_app::pages::actions::Action::AddToLibrary(vec![item]))
                }
                None => return,
            },
            HeaderAction::Download => PageEvent::Download(DownloadRequest::Page(self.page.clone())),
            HeaderAction::More => PageEvent::Menu {
                entries: menu::page_entries(&self.page, &self.route, &self.ctx),
                position: window.mouse_position(),
            },
        };
        self.send(event, window, cx);
    }

    /// Shows search section `n` in full, in place of the results.
    pub fn expand(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Page::Search(p) | Page::LibrarySearch(p)) = &*self.page else {
            return;
        };
        let Some(section) = p.sections.get(n) else {
            return;
        };
        let mut subscriptions = Vec::new();
        let target = MoreTarget::Section(n);
        let (list, grid) = match &section.listing {
            Listing::Tracks(l) => {
                let list = cx.new(|cx| {
                    TrackListView::new(
                        l.items.clone().into(),
                        list_options(&section.title, true, true, false),
                        cx,
                    )
                });
                subscriptions.push(cx.subscribe_in(
                    &list,
                    window,
                    |this, _, e: &ContentEvent, w, cx| this.on_content(e, None, w, cx),
                ));
                subscriptions.push(cx.observe_in(&list, window, move |this, list, w, cx| {
                    let handle = list.read(cx).scroll.0.borrow().base_handle.clone();
                    this.check_more(&handle, target, w, cx);
                }));
                (Some(list), None)
            }
            Listing::Tiles(l) => {
                let grid = cx.new(|_| TileGrid::new(section.title.clone(), l.items.clone().into()));
                subscriptions.push(cx.subscribe_in(
                    &grid,
                    window,
                    |this, _, e: &ContentEvent, w, cx| this.on_content(e, None, w, cx),
                ));
                subscriptions.push(cx.observe_in(&grid, window, move |this, grid, w, cx| {
                    let handle = grid.read(cx).scroll.0.borrow().base_handle.clone();
                    this.check_more(&handle, target, w, cx);
                }));
                (None, Some(grid))
            }
        };
        if let Some(list) = &list {
            window.focus(&list.focus_handle(cx), cx);
        }
        self.expanded = Some(Expanded {
            section: n,
            list,
            grid,
            _subscriptions: subscriptions,
        });
        self.mark_playing(cx);
        cx.notify();
    }

    fn collapse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// The Playlists page's top-level entries arrived: its folders show above the grid.
    pub fn set_root_folders(&mut self, entries: Vec<PlaylistEntry>, cx: &mut Context<Self>) {
        self.folders.root = entries;
        cx.notify();
    }

    /// Opens a folder (`None`: back to all playlists); its entries follow with
    /// [`PageView::set_folder_entries`].
    pub fn open_folder(&mut self, id: Option<String>, name: String, cx: &mut Context<Self>) {
        match id {
            None => {
                self.folders.trail.clear();
                if let (Some(grid), Some((_, tiles))) = (&self.grid, grid_tiles(&self.page)) {
                    grid.update(cx, |g, cx| g.set_tiles(tiles, cx));
                }
            }
            Some(id) => match self.folders.trail.iter().position(|(f, _)| *f == id) {
                Some(i) => self.folders.trail.truncate(i + 1),
                None => self.folders.trail.push((id, name)),
            },
        }
        self.folders.open = None;
        cx.notify();
    }

    pub fn set_folder_entries(
        &mut self,
        id: &str,
        entries: Vec<PlaylistEntry>,
        cx: &mut Context<Self>,
    ) {
        if self.folders.trail.last().map(|(f, _)| f.as_str()) != Some(id) {
            return;
        }
        let tiles: Arc<[Tile]> = entries.iter().filter_map(folder_tile).collect();
        if let Some(grid) = &self.grid {
            grid.update(cx, |g, cx| {
                g.set_tiles(tiles, cx);
                g.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
            });
        }
        self.folders.open = Some(entries);
        cx.notify();
    }

    /// A drop of dragged rows on the editable playlist: the row moves to where it fell.
    fn drop_rows(&mut self, dragged: &DraggedTracks, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.editable() else { return };
        let [row] = &dragged.rows[..] else { return };
        let Some(from) = page.tracks.items.iter().position(|r| r.id == row.id) else {
            return;
        };
        let bounds = self.drop_bounds.get();
        let theme = cx.theme();
        let y = window.mouse_position().y - bounds.origin.y;
        let to = ((y / theme.size.row).floor().max(0.) as usize).min(page.tracks.items.len() - 1);
        let Some(order) = page.moved(from, to).filter(|_| from != to) else {
            return;
        };
        let action = rustet_app::pages::actions::Action::SetPlaylistOrder {
            id: page.id.clone(),
            tracks: order.iter().filter_map(menu::playlist_ref).collect(),
        };
        // Shown at once; the page reloads when Apple has it.
        if let Some(list) = self.lists.first() {
            list.view.update(cx, |l, cx| l.set_rows(order.into(), cx));
        }
        self.mark_playing(cx);
        self.send(PageEvent::Act(action), window, cx);
    }

    fn scroll_by(&self, dy: Pixels, cx: &mut Context<Self>) {
        let mut offset = self.scroll.offset();
        let max = self.scroll.max_offset().y;
        offset.y = (offset.y - dy).clamp(-max, px(0.));
        self.scroll.set_offset(offset);
        cx.notify();
    }
}

fn folder_tile(entry: &PlaylistEntry) -> Option<Tile> {
    let PlaylistEntry::Playlist {
        id, name, artwork, ..
    } = entry
    else {
        return None;
    };
    Some(Tile {
        kind: TileKind::Playlist,
        title: name.clone(),
        subtitle: None,
        artwork: artwork.clone(),
        badge: None,
        explicit: false,
        route: Some(Route::LibraryPlaylist { id: id.clone() }),
        play: Some(PlayTarget::LibraryPlaylist { id: id.clone() }),
        item: Some(ResourceRef::new(
            rustet_ampapi::api::ResourceType::LibraryPlaylists,
            id.clone(),
        )),
    })
}

/// A page's title: 34 / 700 ("New", "Albums").
pub(crate) fn page_title(
    theme: &Theme,
    text: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let text = text.into();
    div()
        .id("page-title")
        .role(Role::Heading)
        .aria_level(1)
        .aria_label(text.clone())
        .text_size(theme.text.page_title)
        .line_height(theme.text.page_title_line)
        .font_weight(BOLD)
        .text_color(theme.colors.text)
        .truncate()
        .child(text)
}

/// A section heading (17 / 700), as a shelf's head.
fn heading(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    let text = text.into();
    div()
        .text_size(theme.text.title2)
        .font_weight(BOLD)
        .text_color(theme.colors.text)
        .truncate()
        .child(text)
}

/// The title of a page of shelves: the sidebar's name for New, Radio and Home (Apple's own
/// title for them is "Music" or "Radio - Main"), else Apple's.
fn browse_title(route: &Route, title: &str) -> String {
    match route {
        Route::New => "New",
        Route::Radio => "Radio",
        Route::Home => "Home",
        _ => title,
    }
    .to_owned()
}

fn shelves(theme: &Theme, shelves: &[Shelf], on_event: &OnContent) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(theme.space.xxl)
        .px(theme.space.xl)
        .children(
            shelves
                .iter()
                .map(|s| ShelfView::new(s.clone()).on_event(on_event.clone())),
        )
}

impl Render for PageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let page = self.page.clone();
        let on_event = self.on_event(cx);
        let root = div()
            .id(SharedString::from(format!("page:{}", self.route.to_link())))
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(|_: &FocusNext, w, cx| w.focus_next(cx))
            .on_action(|_: &FocusPrevious, w, cx| w.focus_prev(cx))
            .on_action(cx.listener(|this, _: &ScrollDown, _, cx| {
                let step = cx.theme().size.row * 2.;
                this.scroll_by(step, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollUp, _, cx| {
                let step = cx.theme().size.row * 2.;
                this.scroll_by(-step, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                let step = this.scroll.bounds().size.height * 0.9;
                this.scroll_by(step, cx)
            }))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                let step = this.scroll.bounds().size.height * 0.9;
                this.scroll_by(-step, cx)
            }))
            .relative()
            .size_full();

        if let Some(expanded) = self.expanded.as_ref() {
            let title = match &*page {
                Page::Search(p) | Page::LibrarySearch(p) => p
                    .sections
                    .get(expanded.section)
                    .map(|s| s.title.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let body = expanded
                .list
                .clone()
                .map(|l| l.into_any_element())
                .or_else(|| expanded.grid.clone().map(|g| g.into_any_element()));
            let back = cx.listener(|this, _, w, cx| this.collapse(w, cx));
            let more = self.more_footer(MoreTarget::Section(expanded.section), &theme);
            return root
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.space.m)
                        .px(theme.space.xl)
                        .pt(theme.size.strip)
                        .pb(theme.space.m)
                        .child(
                            Button::new("back-to-results", "All results")
                                .icon(Icon::ChevronLeft)
                                .on_click(back),
                        )
                        .child(page_title(&theme, title).flex_1().min_w_0()),
                )
                .child(div().flex_1().min_h_0().children(body))
                .children(more)
                .into_any_element();
        }

        if layout(&page) == Layout::Fill {
            return root
                .flex()
                .flex_col()
                .child(self.fill_body(&theme))
                .into_any_element();
        }

        let body = self.scroll_body(&theme, &on_event, cx);
        // "Load more" for the lists inside the scrolling page.
        if matches!(
            &*page,
            Page::Playlist(_) | Page::LibraryPlaylist(_) | Page::Curator(_)
        ) {
            let handle = self.scroll.clone();
            self.check_more(&handle, MoreTarget::Main, window, cx);
        }
        root.child(
            div()
                .id("page-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(body),
        )
        .child(Scrollbar::new("page-scrollbar", self.scroll.clone()))
        .into_any_element()
    }
}

impl PageView {
    /// The row under a list that continues: "Loading more" or why it failed, with Try again.
    fn more_footer(&self, target: MoreTarget, theme: &Theme) -> Option<AnyElement> {
        match &self.more {
            MoreState::Loading(t) if *t == target => Some(
                rustet_ui::components::loading("loading-more")
                    .px(theme.space.xl)
                    .py(theme.space.s)
                    .child(Placeholder::Rows(2))
                    .into_any_element(),
            ),
            MoreState::Failed { target: t, error } if *t == target => {
                let sink = self.sink.clone();
                Some(
                    div()
                        .px(theme.space.xl)
                        .child(
                            ErrorStateView::new(&rustet_app::pages::ErrorState {
                                title: "Rustet could not load more.".to_owned(),
                                ..error.clone()
                            })
                            .on_recover(move |_, w, cx| sink(PageEvent::LoadMore(target), w, cx)),
                        )
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }

    /// The kit's header over `header`, with this page's facts and actions.
    fn header(&self, header: &Header, on_event: &OnContent, cx: &mut Context<Self>) -> PageHeader {
        let this = cx.entity().downgrade();
        PageHeader::new(header.clone())
            .facts(facts(&self.page))
            .actions(header_actions(&self.page, &self.ctx))
            .on_action(move |a, w, cx| {
                this.update(cx, |this, cx| this.on_header(a, w, cx)).ok();
            })
            .on_event(on_event.clone())
    }

    fn lists_from(&self, role: impl Fn(ListRole) -> bool) -> Vec<Entity<TrackListView>> {
        self.lists
            .iter()
            .filter(|l| role(l.role))
            .map(|l| l.view.clone())
            .collect()
    }

    fn scroll_body(
        &mut self,
        theme: &Theme,
        on_event: &OnContent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = self.page.clone();
        let col = || div().flex().flex_col().pb(theme.size.player_clearance);
        let muted = |t: String| {
            div()
                .text_size(theme.text.callout)
                .text_color(theme.colors.text_secondary)
                .child(t)
        };
        // The lines under an album's or a playlist's tracks, as Apple's: the release date,
        // the song count and length, the label.
        let footer = |lines: Vec<String>| {
            div()
                .flex()
                .flex_col()
                .gap(theme.space.xxs)
                .px(theme.space.xl)
                .pt(theme.space.l)
                .children(lines.into_iter().map(muted))
        };
        let summary = summary(&page);
        match &*page {
            Page::Album(p) => {
                let multi = p.discs.len() > 1;
                let lists = self.lists_from(|_| true);
                let lines = p
                    .info
                    .released
                    .filter(|_| !p.info.prerelease)
                    .map(date_text)
                    .into_iter()
                    .chain(summary)
                    .chain(p.info.record_label.clone())
                    .chain(p.info.copyright.clone())
                    .collect();
                col()
                    .child(self.header(&p.header, on_event, cx))
                    .children(p.discs.iter().zip(lists).map(|(d, l)| {
                        div()
                            .flex()
                            .flex_col()
                            .when(multi, |c| {
                                c.child(
                                    div()
                                        .px(theme.space.xl)
                                        .pt(theme.space.m)
                                        .pb(theme.space.xs)
                                        .child(heading(theme, format!("Disc {}", d.number))),
                                )
                            })
                            .child(l)
                    }))
                    .child(footer(lines))
                    .child(shelves(theme, &p.shelves, on_event).pt(theme.space.xxl))
                    .into_any_element()
            }
            Page::LibraryAlbum(p) => {
                let multi = p.discs.len() > 1;
                let lists = self.lists_from(|_| true);
                col()
                    .child(self.header(&p.header, on_event, cx))
                    .children(p.discs.iter().zip(lists).map(|(d, l)| {
                        div()
                            .flex()
                            .flex_col()
                            .when(multi, |c| {
                                c.child(
                                    div()
                                        .px(theme.space.xl)
                                        .pt(theme.space.m)
                                        .pb(theme.space.xs)
                                        .child(heading(theme, format!("Disc {}", d.number))),
                                )
                            })
                            .child(l)
                    }))
                    .child(footer(summary.into_iter().collect()))
                    .into_any_element()
            }
            Page::Playlist(p) => col()
                .child(self.header(&p.header, on_event, cx))
                .children(self.lists_from(|_| true))
                .children(self.more_footer(MoreTarget::Main, theme))
                .when(p.tracks.more.is_none(), |c| {
                    c.child(footer(summary.into_iter().collect()))
                })
                .child(shelves(theme, &p.shelves, on_event).pt(theme.space.xxl))
                .into_any_element(),
            Page::LibraryPlaylist(p) => {
                let list = self.lists.first().map(|l| l.view.clone());
                let editable = self.editable().is_some();
                let bounds = self.drop_bounds.clone();
                let empty = p.tracks.items.is_empty() && p.tracks.more.is_none();
                col()
                    .child(self.header(&p.header, on_event, cx))
                    .when(empty, |c| {
                        c.child(div().px(theme.space.xl).child(EmptyStateView::new(
                            &rustet_app::pages::model::EmptyState {
                                title: "This playlist is empty".to_owned(),
                                message:
                                    "Add songs with “Add to playlist” in a song’s menu.".to_owned(),
                            },
                        )))
                    })
                    .children(list.map(|l| {
                        let hover = theme.colors.hover;
                        div()
                            .relative()
                            .when(editable, |d| {
                                d.drag_over::<DraggedTracks>(move |s, _, _, _| s.bg(hover))
                                    .on_drop(cx.listener(|this, dragged: &DraggedTracks, w, cx| {
                                        this.drop_rows(dragged, w, cx)
                                    }))
                            })
                            .child(l)
                            .child(
                                canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
                                    .absolute()
                                    .size_full(),
                            )
                    }))
                    .children(self.more_footer(MoreTarget::Main, theme))
                    .when(p.tracks.more.is_none(), |c| {
                        c.child(footer(summary.into_iter().collect()))
                    })
                    .into_any_element()
            }
            Page::Artist(p) => {
                // "Latest Release" beside "Top Songs", then the other shelves.
                let top = p
                    .shelves
                    .iter()
                    .position(|s| matches!(s.items, ShelfItems::Tracks(_)));
                let rest: Vec<Shelf> = p
                    .shelves
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| Some(*i) != top)
                    .map(|(_, s)| s.clone())
                    .collect();
                let latest = p.latest_release.clone().map(|t| {
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap(theme.space.m)
                        .child(heading(theme, "Latest Release"))
                        .child(TileView::new(t, 0, theme.size.tile_min).on_event(on_event.clone()))
                });
                let top = top.map(|i| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(ShelfView::new(p.shelves[i].clone()).on_event(on_event.clone()))
                });
                let lead = latest.is_some() || top.is_some();
                col()
                    .child(self.header(&p.header, on_event, cx).hero())
                    .when(lead, |c| {
                        c.child(
                            div()
                                .px(theme.space.xl)
                                .pb(theme.space.xxl)
                                .flex()
                                .gap(theme.space.xl)
                                .children(latest)
                                .children(top),
                        )
                    })
                    .child(shelves(theme, &rest, on_event))
                    .children(p.bio.clone().map(|bio| {
                        div()
                            .px(theme.space.xl)
                            .pt(theme.space.xxl)
                            .flex()
                            .flex_col()
                            .gap(theme.space.m)
                            .child(heading(theme, "About"))
                            .child(
                                div()
                                    .max_w(theme.size.notes_max_width)
                                    .text_size(theme.text.body)
                                    .line_height(theme.text.body_line)
                                    .text_color(theme.colors.text_secondary)
                                    .child(bio),
                            )
                    }))
                    .into_any_element()
            }
            Page::Song(p) => {
                let width = theme.size.tile_min;
                col()
                    .child(self.header(&p.header, on_event, cx))
                    .child(
                        div()
                            .px(theme.space.xl)
                            .flex()
                            .gap(theme.space.xxl)
                            .children(p.album.clone().map(|t| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(theme.space.m)
                                    .child(heading(theme, "Album"))
                                    .child(TileView::new(t, 0, width).on_event(on_event.clone()))
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(theme.space.m)
                                    .when(p.composer.is_some() || !p.credits.is_empty(), |d| {
                                        d.child(heading(theme, "Credits"))
                                    })
                                    .children(p.composer.clone().map(|c| {
                                        credit_block(theme, "Composer".to_owned(), vec![c])
                                    }))
                                    .children(p.credits.iter().map(|g| {
                                        credit_block(
                                            theme,
                                            g.title.clone(),
                                            g.credits
                                                .iter()
                                                .map(|c| {
                                                    if c.roles.is_empty() {
                                                        c.name.clone()
                                                    } else {
                                                        format!(
                                                            "{}, {}",
                                                            c.name,
                                                            c.roles.join(", ")
                                                        )
                                                    }
                                                })
                                                .collect(),
                                        )
                                    }))
                                    .children(p.lyrics_excerpt.clone().map(|l| {
                                        credit_block(theme, "Lyrics".to_owned(), vec![l])
                                    })),
                            ),
                    )
                    .children(
                        p.music_videos
                            .clone()
                            .map(|s| shelves(theme, &[s], on_event).pt(theme.space.xxl)),
                    )
                    .into_any_element()
            }
            Page::MusicVideo(p) => col()
                .child(self.header(&p.header, on_event, cx))
                .child(shelves(theme, &p.shelves, on_event))
                .into_any_element(),
            Page::Station(p) => {
                let width = theme.size.tile_min;
                col()
                    .child(self.header(&p.header, on_event, cx))
                    .children(p.show.clone().map(|t| {
                        div()
                            .px(theme.space.xl)
                            .flex()
                            .flex_col()
                            .gap(theme.space.m)
                            .child(heading(theme, "Show"))
                            .child(TileView::new(t, 0, width).on_event(on_event.clone()))
                    }))
                    .into_any_element()
            }
            Page::Curator(p) => {
                let tiles = p.playlists.items.clone();
                col()
                    .child(self.header(&p.header, on_event, cx))
                    .child(shelves(theme, &p.shelves, on_event))
                    .when(!tiles.is_empty(), |c| {
                        c.child(
                            div()
                                .px(theme.space.xl)
                                .pt(if p.shelves.is_empty() {
                                    px(0.)
                                } else {
                                    theme.space.xxl
                                })
                                .flex()
                                .flex_col()
                                .gap(theme.space.m)
                                .child(heading(theme, "Playlists"))
                                .child(wrap_grid(theme, tiles, on_event.clone())),
                        )
                    })
                    .children(self.more_footer(MoreTarget::Main, theme))
                    .into_any_element()
            }
            Page::Browse(p) => col()
                .pt(theme.size.strip)
                .child(
                    div()
                        .px(theme.space.xl)
                        .pb(theme.space.l)
                        .child(page_title(theme, browse_title(&self.route, &p.title))),
                )
                .child(shelves(theme, &p.shelves, on_event))
                .into_any_element(),
            // The search field in the strip is the landing's title.
            Page::SearchLanding(p) => col()
                .pt(theme.size.strip + theme.space.l)
                .child(shelves(theme, &p.shelves, on_event))
                .into_any_element(),
            Page::Charts(p) => col()
                .pt(theme.size.strip)
                .child(
                    div()
                        .px(theme.space.xl)
                        .pb(theme.space.l)
                        .flex()
                        .items_center()
                        .gap(theme.space.l)
                        .child(page_title(theme, "Top charts").flex_1().min_w_0())
                        .children(self.genre.clone()),
                )
                .child(shelves(theme, &p.shelves, on_event))
                .into_any_element(),
            Page::Search(p) | Page::LibrarySearch(p) => self.search_body(p, theme, on_event, cx),
            // Fill pages are drawn by `fill_body`.
            Page::Library(_) | Page::List(_) => div().into_any_element(),
        }
    }

    /// Search results without a title (the field in the strip is the title): "Top Results"
    /// as cards, then a shelf per group (artists as circles, songs as the song grid). A
    /// section's head shows it in full when it has more than the shelf holds.
    fn search_body(
        &mut self,
        p: &SearchPage,
        theme: &Theme,
        on_event: &OnContent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = div()
            .flex()
            .flex_col()
            .gap(theme.space.xxl)
            .px(theme.space.xl)
            .pt(theme.size.strip + theme.space.l)
            .pb(theme.size.player_clearance);
        if let Some(empty) = p.empty_state() {
            return body.child(EmptyStateView::new(&empty)).into_any_element();
        }
        let page = cx.entity();
        body.children(p.sections.iter().enumerate().map(|(i, section)| {
            let (items, expandable) = match &section.listing {
                Listing::Tiles(l) => (
                    ShelfItems::Tiles {
                        tiles: l.items.clone(),
                        size: TileSize::Regular,
                    },
                    l.more.is_some(),
                ),
                // A full song list is worth opening even when every song is here.
                Listing::Tracks(l) => (ShelfItems::Tracks(l.items.clone()), true),
            };
            let shelf = ShelfView::new(Shelf {
                id: format!("search-{i}"),
                title: Some(section.title.clone()),
                items,
                see_all: None,
            })
            .on_event(on_event.clone())
            .when(section.group == SearchGroup::Top, ShelfView::cards);
            let page = page.clone();
            shelf.when(expandable, move |shelf| {
                shelf.on_see_all(move |w, cx| page.update(cx, |this, cx| this.expand(i, w, cx)))
            })
        }))
        .into_any_element()
    }

    fn fill_body(&self, theme: &Theme) -> AnyElement {
        let page = self.page.clone();
        let (title, empty) = match &*page {
            Page::Library(p) => (section_title(p.section).to_owned(), p.empty_state()),
            Page::List(p) => (
                p.title.clone().unwrap_or_else(|| match &self.route {
                    Route::Room { .. } => "Featured".to_owned(),
                    _ => "All".to_owned(),
                }),
                None,
            ),
            _ => (String::new(), None),
        };
        let playlists =
            matches!(&*page, Page::Library(p) if p.section == LibrarySection::Playlists);
        let mut actions = div().flex().items_center().gap(theme.space.s);
        if let Some(intent) = page_play(&page, How::InOrder).filter(|r| r.is_ok()) {
            let shuffle = page_play(&page, How::Shuffled);
            let sink = self.sink.clone();
            let sink2 = self.sink.clone();
            actions = actions
                .child(
                    Button::new("fill-play", "Play")
                        .primary()
                        .large()
                        .icon(Icon::Play)
                        .on_click(move |_, w, cx| {
                            sink(PageEvent::Play(PlayRequest::Intent(intent.clone())), w, cx)
                        }),
                )
                .children(shuffle.map(|s| {
                    Button::new("fill-shuffle", "Shuffle")
                        .primary()
                        .large()
                        .icon(Icon::Shuffle)
                        .on_click(move |_, w, cx| {
                            sink2(PageEvent::Play(PlayRequest::Intent(s.clone())), w, cx)
                        })
                }));
        }
        if playlists && self.ctx.signed_in {
            let sink = self.sink.clone();
            actions = actions.child(
                Button::new("new-playlist", "New playlist")
                    .icon(Icon::Plus)
                    .on_click(move |_, w, cx| {
                        sink(PageEvent::NewPlaylist { tracks: Vec::new() }, w, cx)
                    }),
            );
        }
        let folder_bar = playlists.then(|| self.folder_bar(theme)).flatten();
        let in_folder = playlists && !self.folders.trail.is_empty();
        let body: AnyElement = if in_folder {
            match (&self.folders.open, &self.grid) {
                (Some(entries), Some(grid)) if entries.iter().any(|e| matches!(e, PlaylistEntry::Playlist { .. })) => {
                    grid.clone().into_any_element()
                }
                (Some(_), _) => div()
                    .px(theme.space.xl)
                    .child(EmptyStateView::new(&rustet_app::pages::model::EmptyState {
                        title: "No playlists in this folder".to_owned(),
                        message: "Folders and their playlists are made in Apple Music on a phone or a Mac.".to_owned(),
                    }))
                    .into_any_element(),
                (None, _) => rustet_ui::components::loading("grid-loading")
                    .px(theme.space.xl)
                    .child(Placeholder::Tiles { count: 6, size: theme.size.tile_min })
                    .into_any_element(),
            }
        } else if let Some(empty) = empty {
            div()
                .px(theme.space.xl)
                .child(EmptyStateView::new(&empty))
                .into_any_element()
        } else if let Some(grid) = &self.grid {
            grid.clone().into_any_element()
        } else if let Some(list) = self.lists.first() {
            list.view.clone().into_any_element()
        } else {
            div().into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(theme.space.xl)
                    .pt(theme.size.strip)
                    .pb(theme.space.s)
                    .flex()
                    .items_center()
                    .gap(theme.space.l)
                    .child(page_title(theme, title).flex_1().min_w_0())
                    .child(actions),
            )
            .children(folder_bar)
            .child(div().flex_1().min_h_0().child(body))
            .children(self.more_footer(MoreTarget::Main, theme))
            .into_any_element()
    }

    /// The folders of the Playlists page: where the user is, and the folders inside.
    fn folder_bar(&self, theme: &Theme) -> Option<AnyElement> {
        let entries: &[PlaylistEntry] = if self.folders.trail.is_empty() {
            &self.folders.root
        } else {
            self.folders.open.as_deref().unwrap_or_default()
        };
        let folders: Vec<(String, String)> = entries
            .iter()
            .filter_map(|e| match e {
                PlaylistEntry::Folder { id, name } => Some((id.clone(), name.clone())),
                PlaylistEntry::Playlist { .. } => None,
            })
            .collect();
        if folders.is_empty() && self.folders.trail.is_empty() {
            return None;
        }
        let open = |id: Option<String>, name: String| {
            let sink = self.sink.clone();
            move |_: &gpui::ClickEvent, w: &mut Window, cx: &mut App| {
                sink(
                    PageEvent::OpenFolder {
                        id: id.clone(),
                        name: name.clone(),
                    },
                    w,
                    cx,
                )
            }
        };
        let mut trail = None;
        if !self.folders.trail.is_empty() {
            let mut t = div().flex().items_center().gap(theme.space.xs);
            t = t.child(
                Button::new("folder-root", "All playlists").on_click(open(None, String::new())),
            );
            for (i, (id, name)) in self.folders.trail.iter().enumerate() {
                t = t
                    .child(
                        Icon::ChevronRight
                            .element()
                            .size(theme.text.callout)
                            .text_color(theme.colors.text_secondary),
                    )
                    .child(
                        Button::new(("folder-trail", i), name.clone())
                            .on_click(open(Some(id.clone()), name.clone())),
                    );
            }
            trail = Some(t);
        }
        Some(
            div()
                .id("playlist-folders")
                .role(Role::Group)
                .aria_label("Folders")
                .px(theme.space.xl)
                .pb(theme.space.m)
                .flex()
                .flex_wrap()
                .items_center()
                .gap(theme.space.s)
                .children(trail)
                .children(folders.into_iter().enumerate().map(|(i, (id, name))| {
                    Button::new(("folder", i), name.clone())
                        .icon(Icon::Library)
                        .on_click(open(Some(id), name))
                }))
                .into_any_element(),
        )
    }
}

fn credit_block(theme: &Theme, title: String, lines: Vec<String>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(theme.space.xs)
        .child(
            div()
                .text_size(theme.text.callout)
                .font_weight(SEMIBOLD)
                .text_color(theme.colors.text)
                .child(title),
        )
        .children(lines.into_iter().map(|l| {
            div()
                .max_w(theme.size.notes_max_width)
                .text_size(theme.text.callout)
                .line_height(theme.text.body_line)
                .text_color(theme.colors.text_secondary)
                .child(l)
        }))
}

/// Tiles in rows, not virtualised (a curator's playlists: dozens).
fn wrap_grid(theme: &Theme, tiles: Vec<Tile>, on_event: OnContent) -> impl IntoElement {
    let width = theme.size.tile_min;
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .gap(theme.space.l)
        .children(
            tiles
                .into_iter()
                .enumerate()
                .map(|(i, t)| TileView::new(t, i, width).on_event(on_event.clone())),
        )
}
