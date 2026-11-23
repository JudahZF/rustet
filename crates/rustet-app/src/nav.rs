//! The navigation model of the main window (spec §6.1): the page stack with back and forward,
//! the page being shown as a typed state, a page cache so Back is instant, "load more", and
//! the search field's hints.
//!
//! - Moving on cancels what the last page was loading (its request is dropped) and its
//!   result is never shown.
//! - The cache holds the last [`CACHE_PAGES`] pages. Actions that change a page invalidate
//!   it ([`Nav::invalidate`]); a sign-in, a sign-out or an expired sign-in empties it. A shown
//!   page that is invalidated reloads in place: it stays on screen until the new one is there,
//!   except a page that needs the sign-in after a sign-out, which shows the sign-in error.
//! - Search hints load after [`HINT_DELAY`] without typing; an answer for an older term is
//!   dropped.
//!
//! The methods return at once and work from any thread: loads run on the tokio runtime given
//! to [`Nav::new`], their results arrive in [`Nav::state`] and [`Nav::hints`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rustet_ampapi::Api;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tracing::debug;

use crate::pages::catalog::{self, Hint};
use crate::pages::model::{List, Listing, Tile, TrackRow};
use crate::pages::more::LoadMore;
use crate::pages::{self, ErrorState, LoadError, Page};
use crate::route::Route;

/// Pages the cache keeps.
pub const CACHE_PAGES: usize = 32;
/// Routes Back can return to.
const MAX_HISTORY: usize = 100;
/// Search hints wait this long after the last key.
pub const HINT_DELAY: Duration = Duration::from_millis(150);

/// The page of the current route.
#[derive(Debug, Clone, PartialEq)]
pub enum PageState {
    Loading,
    Ready(Arc<Page>),
    /// The error state says what failed and what to do ([`pages::Recovery`]).
    Failed(ErrorState),
    /// Downloads and Settings: drawn from their own models, not loaded.
    Local,
}

/// "Load more" of the shown page.
#[derive(Debug, Clone, PartialEq)]
pub enum MoreState {
    Idle,
    Loading(MoreTarget),
    Failed {
        target: MoreTarget,
        error: ErrorState,
    },
}

/// Which list of a page continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoreTarget {
    /// The page's list: playlist tracks, a library section, a "see all" list, a curator's
    /// playlists.
    Main,
    /// Section `n` of search results.
    Section(usize),
}

/// What the page region draws.
#[derive(Debug, Clone, PartialEq)]
pub struct NavState {
    pub route: Route,
    pub page: PageState,
    pub more: MoreState,
    pub can_back: bool,
    pub can_forward: bool,
}

/// The hints under the search field, for `term`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hints {
    pub term: String,
    pub hints: Vec<Hint>,
}

/// The least recently used page goes first.
#[derive(Default)]
struct PageCache(VecDeque<(Route, Arc<Page>)>);

impl PageCache {
    fn get(&mut self, route: &Route) -> Option<Arc<Page>> {
        let i = self.0.iter().position(|(r, _)| r == route)?;
        let entry = self.0.remove(i)?;
        let page = entry.1.clone();
        self.0.push_back(entry);
        Some(page)
    }

    fn put(&mut self, route: Route, page: Arc<Page>) {
        self.0.retain(|(r, _)| *r != route);
        if self.0.len() >= CACHE_PAGES {
            self.0.pop_front();
        }
        self.0.push_back((route, page));
    }

    fn retain(&mut self, keep: impl Fn(&Route) -> bool) {
        self.0.retain(|(r, _)| keep(r));
    }
}

struct Stack {
    route: Route,
    back: Vec<Route>,
    forward: Vec<Route>,
    cache: PageCache,
    /// Bumped by every page change: a result of an older load is dropped.
    generation: u64,
    load: Option<AbortHandle>,
    more: Option<AbortHandle>,
    hint_generation: u64,
    hint: Option<AbortHandle>,
}

struct Inner {
    api: Api,
    runtime: Handle,
    stack: Mutex<Stack>,
    state: watch::Sender<NavState>,
    hints: watch::Sender<Hints>,
}

/// The navigation model. Cheap to clone.
#[derive(Clone)]
pub struct Nav(Arc<Inner>);

fn local(route: &Route) -> bool {
    matches!(route, Route::Downloads | Route::Settings { .. })
}

impl Nav {
    /// Starts at `start` and loads it. Loads run on `runtime`; every change of `session`
    /// ([`crate::account::AccountModel::session`]) empties the cache and reloads the page shown.
    pub fn new<S: Send + Sync + 'static>(
        api: Api,
        runtime: Handle,
        start: Route,
        session: watch::Receiver<S>,
    ) -> Self {
        let state = NavState {
            route: start.clone(),
            page: PageState::Loading,
            more: MoreState::Idle,
            can_back: false,
            can_forward: false,
        };
        let nav = Self(Arc::new(Inner {
            api,
            runtime,
            stack: Mutex::new(Stack {
                route: start,
                back: Vec::new(),
                forward: Vec::new(),
                cache: PageCache::default(),
                generation: 0,
                load: None,
                more: None,
                hint_generation: 0,
                hint: None,
            }),
            state: watch::channel(state).0,
            hints: watch::channel(Hints::default()).0,
        }));
        nav.show(&mut nav.stack(), Show::Fresh);
        let weak = Arc::downgrade(&nav.0);
        let mut session = session;
        nav.0.runtime.spawn(async move {
            session.borrow_and_update();
            while session.changed().await.is_ok() {
                let Some(inner) = weak.upgrade() else { return };
                Nav(inner).invalidate(|_| true);
            }
        });
        nav
    }

    pub fn state(&self) -> watch::Receiver<NavState> {
        self.0.state.subscribe()
    }

    pub fn hints(&self) -> watch::Receiver<Hints> {
        self.0.hints.subscribe()
    }

    fn stack(&self) -> MutexGuard<'_, Stack> {
        self.0.stack.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Opens `route` (a click, a link). The current page goes on the back stack; forward is
    /// cleared. The same route again does nothing.
    pub fn go(&self, route: Route) {
        let mut s = self.stack();
        if s.route == route {
            return;
        }
        let previous = std::mem::replace(&mut s.route, route);
        s.back.push(previous);
        if s.back.len() > MAX_HISTORY {
            s.back.remove(0);
        }
        s.forward.clear();
        self.show(&mut s, Show::Fresh);
    }

    /// Back (Alt+←, mouse button 4). Nothing when there is no page to go back to.
    pub fn back(&self) {
        let mut s = self.stack();
        let Some(route) = s.back.pop() else { return };
        let previous = std::mem::replace(&mut s.route, route);
        s.forward.push(previous);
        self.show(&mut s, Show::Fresh);
    }

    /// Forward (Alt+→, mouse button 5).
    pub fn forward(&self) {
        let mut s = self.stack();
        let Some(route) = s.forward.pop() else { return };
        let previous = std::mem::replace(&mut s.route, route);
        s.back.push(previous);
        self.show(&mut s, Show::Fresh);
    }

    /// Loads the current page again (Retry, "refresh"), past the cache.
    pub fn reload(&self) {
        let mut s = self.stack();
        let route = s.route.clone();
        s.cache.retain(|r| *r != route);
        self.show(&mut s, Show::Fresh);
    }

    /// Drops the cached pages `changed` selects (an action changed them). The shown page, if
    /// selected, reloads in place.
    pub fn invalidate(&self, changed: impl Fn(&Route) -> bool) {
        let mut s = self.stack();
        s.cache.retain(|r| !changed(r));
        if changed(&s.route) {
            self.show(&mut s, Show::InPlace);
        }
    }

    /// Loads the next part of a list of the shown page. Nothing while one loads or when the
    /// list is complete.
    pub fn load_more(&self, target: MoreTarget) {
        let mut s = self.stack();
        let state = self.0.state.borrow().clone();
        if matches!(state.more, MoreState::Loading(_)) {
            return;
        }
        let PageState::Ready(page) = &state.page else {
            return;
        };
        let Some(list) = more_of(page, target) else {
            return;
        };
        let generation = s.generation;
        let me = self.clone();
        let task = self.0.runtime.spawn(async move {
            let result = list.load(&me.0.api).await;
            me.more_loaded(generation, target, result);
        });
        s.more = Some(task.abort_handle());
        self.0
            .state
            .send_modify(|st| st.more = MoreState::Loading(target));
    }

    /// The search field changed: hints for `term` follow after [`HINT_DELAY`] without another
    /// change. An empty term clears them.
    pub fn search_input(&self, term: &str) {
        let mut s = self.stack();
        if let Some(task) = s.hint.take() {
            task.abort();
        }
        s.hint_generation += 1;
        let generation = s.hint_generation;
        let term = term.to_owned();
        if term.trim().is_empty() {
            self.0.hints.send_replace(Hints {
                term,
                hints: Vec::new(),
            });
            return;
        }
        let me = self.clone();
        let task = self.0.runtime.spawn(async move {
            tokio::time::sleep(HINT_DELAY).await;
            let hints = catalog::hints(&me.0.api, &term).await.unwrap_or_else(|e| {
                debug!("search hints: {e}");
                Vec::new()
            });
            // An answer for an older term is dropped.
            if me.stack().hint_generation == generation {
                me.0.hints.send_replace(Hints { term, hints });
            }
        });
        s.hint = Some(task.abort_handle());
    }

    fn show(&self, s: &mut Stack, how: Show) {
        for task in [s.load.take(), s.more.take()].into_iter().flatten() {
            task.abort();
        }
        s.generation += 1;
        let route = s.route.clone();
        let cached = match how {
            Show::Fresh => s.cache.get(&route),
            Show::InPlace => None,
        };
        let page = match (cached, local(&route)) {
            (Some(page), _) => PageState::Ready(page),
            (None, true) => PageState::Local,
            (None, false) => {
                let generation = s.generation;
                let me = self.clone();
                let load_route = route.clone();
                let task = self.0.runtime.spawn(async move {
                    let result = pages::load(&me.0.api, &load_route).await;
                    me.loaded(generation, load_route, result, how);
                });
                s.load = Some(task.abort_handle());
                match (how, &self.0.state.borrow().page) {
                    (Show::InPlace, shown @ PageState::Ready(_)) => shown.clone(),
                    _ => PageState::Loading,
                }
            }
        };
        self.0.state.send_replace(NavState {
            route,
            page,
            more: MoreState::Idle,
            can_back: !s.back.is_empty(),
            can_forward: !s.forward.is_empty(),
        });
    }

    fn loaded(&self, generation: u64, route: Route, result: Result<Page, LoadError>, how: Show) {
        let mut s = self.stack();
        if s.generation != generation {
            return;
        }
        s.load = None;
        let page = match result {
            Ok(page) => {
                let page = Arc::new(page);
                s.cache.put(route.clone(), page.clone());
                PageState::Ready(page)
            }
            // A failed reload in place keeps the page that is there, unless it is no longer
            // the user's to see (signed out).
            Err(e)
                if how == Show::InPlace
                    && e != LoadError::SignInRequired
                    && matches!(self.0.state.borrow().page, PageState::Ready(_)) =>
            {
                return debug!("reloading {route:?}: {e}");
            }
            Err(e) => PageState::Failed(e.state(&route)),
        };
        self.0.state.send_modify(|st| st.page = page);
    }

    fn more_loaded(
        &self,
        generation: u64,
        target: MoreTarget,
        result: Result<MoreItems, LoadError>,
    ) {
        let mut s = self.stack();
        if s.generation != generation {
            return;
        }
        s.more = None;
        let route = s.route.clone();
        let items = match result {
            Ok(items) => items,
            Err(e) => {
                let error = e.state(&route);
                return self
                    .0
                    .state
                    .send_modify(|st| st.more = MoreState::Failed { target, error });
            }
        };
        let shown = self.0.state.borrow().page.clone();
        let PageState::Ready(page) = shown else {
            return;
        };
        let mut page = Page::clone(&page);
        extend(&mut page, target, items);
        let page = Arc::new(page);
        s.cache.put(route, page.clone());
        self.0.state.send_modify(|st| {
            st.page = PageState::Ready(page);
            st.more = MoreState::Idle;
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Show {
    /// From the cache, else loading.
    Fresh,
    /// Past the cache, keeping a shown page until the new one is there.
    InPlace,
}

/// A list continuation of either item type.
enum MoreList {
    Tiles(LoadMore<Tile>),
    Tracks(LoadMore<TrackRow>),
}

enum MoreItems {
    Tiles(List<Tile>),
    Tracks(List<TrackRow>),
}

impl MoreList {
    async fn load(&self, api: &Api) -> Result<MoreItems, LoadError> {
        Ok(match self {
            Self::Tiles(more) => MoreItems::Tiles(more.load(api).await?),
            Self::Tracks(more) => MoreItems::Tracks(more.load(api).await?),
        })
    }
}

fn listing_more(listing: &Listing) -> Option<MoreList> {
    match listing {
        Listing::Tiles(l) => l.more.clone().map(MoreList::Tiles),
        Listing::Tracks(l) => l.more.clone().map(MoreList::Tracks),
    }
}

fn more_of(page: &Page, target: MoreTarget) -> Option<MoreList> {
    match (page, target) {
        (Page::Playlist(p), MoreTarget::Main) => p.tracks.more.clone().map(MoreList::Tracks),
        (Page::LibraryPlaylist(p), MoreTarget::Main) => p.tracks.more.clone().map(MoreList::Tracks),
        (Page::Curator(p), MoreTarget::Main) => p.playlists.more.clone().map(MoreList::Tiles),
        (Page::List(p), MoreTarget::Main) => listing_more(&p.listing),
        (Page::Library(p), MoreTarget::Main) => listing_more(&p.listing),
        (Page::Search(p) | Page::LibrarySearch(p), MoreTarget::Section(i)) => {
            listing_more(&p.sections.get(i)?.listing)
        }
        _ => None,
    }
}

fn extend_listing(listing: &mut Listing, items: MoreItems) {
    match (listing, items) {
        (Listing::Tiles(l), MoreItems::Tiles(more)) => l.extend(more),
        (Listing::Tracks(l), MoreItems::Tracks(more)) => l.extend(more),
        _ => {}
    }
}

fn extend(page: &mut Page, target: MoreTarget, items: MoreItems) {
    match (page, target, items) {
        (Page::Playlist(p), MoreTarget::Main, MoreItems::Tracks(more)) => p.tracks.extend(more),
        (Page::LibraryPlaylist(p), MoreTarget::Main, MoreItems::Tracks(more)) => {
            p.tracks.extend(more)
        }
        (Page::Curator(p), MoreTarget::Main, MoreItems::Tiles(more)) => p.playlists.extend(more),
        (Page::List(p), MoreTarget::Main, items) => extend_listing(&mut p.listing, items),
        (Page::Library(p), MoreTarget::Main, items) => extend_listing(&mut p.listing, items),
        (Page::Search(p) | Page::LibrarySearch(p), MoreTarget::Section(i), items) => {
            if let Some(section) = p.sections.get_mut(i) {
                extend_listing(&mut section.listing, items);
            }
        }
        _ => {}
    }
}
