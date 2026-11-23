//! [`PageArea`]: the view the shell puts in the page region.
//!
//! It follows `nav.state()`: placeholders while a page loads, the page's [`PageView`] when it
//! is ready, the error state (or the sign-in state) when it failed. Each history entry keeps
//! its view ([`History`]), so Back shows the page as it was left: scroll position, selection,
//! shelf pages, and the focus. A new page slides in (the page-push motion, none with Reduce
//! animations).
//!
//! Everything a page asks for arrives here as a [`PageEvent`] and becomes an `App` call:
//! navigation, play intents, library actions, downloads, "load more". Menus, dialogs, toasts
//! and links go to the window through [`Host`].
//!
//! The area also sets the window's page tint ([`theme::set_page_tint`]) for the page shown:
//! the artwork's colour for album, playlist and artist pages, none for every other page and
//! while a page loads or fails. It sets it again when the theme changes (album-art colours
//! turned on), and Back, Forward and the page cache give the same tint as the first visit.

use std::future::Future;
use std::rc::Rc;

use gpui::{
    Animation, AnimationExt, AnyElement, AnyView, App, AppContext, ClipboardItem, Context, Entity,
    FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled, Subscription, Task, Window,
    div, ease_out_quint, prelude::*,
};
use rustet_ampapi::Api;
use rustet_app::App as RustetApp;
use rustet_app::downloads::DownloadTarget;
use rustet_app::nav::{Nav, NavState, PageState};
use rustet_app::pages::actions::Action;
use rustet_app::pages::{Page, Recovery};
use rustet_app::route::Route;
use rustet_ui::components::{EmptyStateView, ErrorStateView, Toast};
use rustet_ui::host::{Host, HostRequest};
use rustet_ui::theme::{self, ActiveTheme, Motion, Theme};
use tracing::debug;

use crate::context::{PageContext, Playing, PlaylistChoice};
use crate::event::{DownloadRequest, PageEvent, PlayRequest, Sink};
use crate::history::{History, Move};
use crate::modal::{self, ConfirmDialog, NameSheet, PlaylistPicker};
use crate::view::PageView;
use crate::{menu, placeholder};

/// History entries that keep their view. Older ones load again from Nav's cache.
const KEEP_VIEWS: usize = 12;

/// The page area for `app`, following `app.nav()`.
pub fn page_area(app: &RustetApp, window: &mut Window, cx: &mut App) -> AnyView {
    let app = app.clone();
    cx.new(|cx| {
        PageArea::new(
            app.clone(),
            app.nav().clone(),
            app.api().clone(),
            window,
            cx,
        )
    })
    .into()
}

/// The page keys are bound once per app, by the first area.
struct KeysBound;

impl gpui::Global for KeysBound {}

struct Kept {
    view: Entity<PageView>,
    /// What had the focus when the entry was left.
    focus: Option<FocusHandle>,
}

pub struct PageArea {
    app: RustetApp,
    nav: Nav,
    api: Api,
    state: NavState,
    history: History<Kept>,
    ctx: PageContext,
    /// Playlists set from outside (the example's fixtures): the cache is not read.
    fixed_playlists: bool,
    focus: FocusHandle,
    /// Bumped by every move: the id of the push motion.
    moves: u64,
    last_move: Move,
    _tasks: Vec<Task<()>>,
    _theme_changed: Subscription,
}

impl Focusable for PageArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Runs `f` with every new value of `rx`, until the view is gone.
fn follow<T, V>(
    mut rx: tokio::sync::watch::Receiver<T>,
    window: &mut Window,
    cx: &mut Context<V>,
    f: impl Fn(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) -> Task<()>
where
    T: Clone + 'static,
    V: 'static,
{
    cx.spawn_in(window, async move |this, cx| {
        while rx.changed().await.is_ok() {
            let value = rx.borrow_and_update().clone();
            if this.update_in(cx, |v, w, cx| f(v, value, w, cx)).is_err() {
                return;
            }
        }
    })
}

impl PageArea {
    /// The area over `nav` and `api` (the app's own, or the example's fixtures); playing,
    /// actions and downloads go to `app`.
    pub fn new(
        app: RustetApp,
        nav: Nav,
        api: Api,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if !cx.has_global::<KeysBound>() {
            crate::view::bind_keys(cx);
            cx.set_global(KeysBound);
        }
        let state = nav.state().borrow().clone();
        let mut tasks = vec![
            follow(nav.state(), window, cx, |this, s, w, cx| {
                this.on_nav(s, w, cx)
            }),
            follow(app.player().state(), window, cx, |this, s, _, cx| {
                this.ctx.playing = s.and_then(|s| s.track.as_ref().map(Playing::of));
                this.push_context(cx);
            }),
            follow(app.account().state(), window, cx, |this, _, w, cx| {
                this.account_changed(w, cx);
            }),
        ];
        // A settings change (album-art colours on) may give the shown page its tint.
        let theme_changed = cx.observe_global::<Theme>(|this, cx| this.tint(cx));
        let mut outcomes = app.outcomes();
        tasks.push(cx.spawn_in(window, async move |this, cx| {
            while let Ok(outcome) = outcomes.recv().await {
                debug!("action finished: {}", outcome.toast());
                if this
                    .update_in(cx, |this, w, cx| this.load_playlists(w, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
        let mut area = Self {
            history: History::new(state.route.clone(), KEEP_VIEWS),
            state: NavState {
                page: PageState::Loading,
                ..state.clone()
            },
            app,
            nav,
            api,
            ctx: PageContext::default(),
            fixed_playlists: false,
            focus: cx.focus_handle(),
            moves: 0,
            last_move: Move::Stay,
            _tasks: std::mem::take(&mut tasks),
            _theme_changed: theme_changed,
        };
        area.account_changed(window, cx);
        area.on_nav(state, window, cx);
        area
    }

    /// Sets the "Add to playlist" choices and stops reading them from the library cache.
    pub fn set_playlists(&mut self, playlists: Vec<PlaylistChoice>, cx: &mut Context<Self>) {
        self.fixed_playlists = true;
        self.ctx.playlists = playlists.into();
        self.push_context(cx);
    }

    /// The view of the shown page, once it is ready.
    pub fn current(&self) -> Option<&Entity<PageView>> {
        match self.state.page {
            PageState::Ready(_) => self.history.kept().map(|k| &k.view),
            _ => None,
        }
    }

    pub fn state(&self) -> &NavState {
        &self.state
    }

    fn sink(cx: &mut Context<Self>) -> Sink {
        let this = cx.entity().downgrade();
        // Deferred: the event may come from inside an update of the page view it changes.
        Rc::new(move |event, window, cx| {
            let this = this.clone();
            window.defer(cx, move |w, cx| {
                this.update(cx, |area, cx| area.handle(event, w, cx)).ok();
            });
        })
    }

    fn account_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ctx.signed_in = self.api.is_signed_in();
        self.ctx.storefront = self.api.locale().storefront;
        self.push_context(cx);
        self.load_playlists(window, cx);
    }

    /// The playlist choices, from the library cache (sorted by name).
    fn load_playlists(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fixed_playlists {
            return;
        }
        let library = self.app.account().syncer().library().clone();
        self.run(
            async move { library.playlists().await },
            |this, result, _, cx| {
                let playlists: Vec<PlaylistChoice> = match result {
                    Ok(list) => list
                        .into_iter()
                        .map(|p| PlaylistChoice {
                            id: p.library_id,
                            name: p.name,
                        })
                        .collect(),
                    Err(e) => return debug!("playlists from the library cache: {e}"),
                };
                if !this.fixed_playlists {
                    this.ctx.playlists = playlists.into();
                    this.push_context(cx);
                }
            },
            window,
            cx,
        );
    }

    fn push_context(&mut self, cx: &mut Context<Self>) {
        if let Some(kept) = self.history.kept() {
            let ctx = self.ctx.clone();
            kept.view.update(cx, |v, cx| v.set_context(ctx, cx));
        }
        cx.notify();
    }

    fn on_nav(&mut self, state: NavState, window: &mut Window, cx: &mut Context<Self>) {
        let focus_inside = self.focus.contains_focused(window, cx) || window.focused(cx).is_none();
        if state.route != *self.history.route()
            && let Some(kept) = self.history.kept_mut()
        {
            kept.focus = window.focused(cx).filter(|_| focus_inside);
        }
        let step = self
            .history
            .follow(&state.route, state.can_back, state.can_forward);
        if step != Move::Stay {
            self.moves += 1;
            self.last_move = step;
        }
        if let PageState::Ready(page) = &state.page {
            let ctx = self.ctx.clone();
            let more = state.more.clone();
            match self.history.kept() {
                Some(kept) if kept.view.read(cx).route() == &state.route => {
                    let page = page.clone();
                    kept.view.update(cx, |v, cx| {
                        v.set_page(page, window, cx);
                        v.set_more(more, cx);
                        v.set_context(ctx, cx);
                    });
                }
                _ => {
                    let sink = Self::sink(cx);
                    let (route, page) = (state.route.clone(), page.clone());
                    let view =
                        cx.new(|cx| PageView::new(route, page.clone(), ctx, sink, window, cx));
                    view.update(cx, |v, cx| v.set_more(more, cx));
                    *self.history.kept_mut() = Some(Kept {
                        view: view.clone(),
                        focus: None,
                    });
                    if matches!(&*page, Page::Library(p) if p.section == rustet_app::route::LibrarySection::Playlists)
                        && self.ctx.signed_in
                    {
                        self.load_folder(view, None, window, cx);
                    }
                }
            }
            // A move gives the focus to the page (or back to what had it), unless the user is
            // elsewhere (the search field).
            if step != Move::Stay
                && focus_inside
                && let Some(kept) = self.history.kept()
            {
                let handle = kept
                    .focus
                    .clone()
                    .unwrap_or_else(|| kept.view.focus_handle(cx));
                window.focus(&handle, cx);
            }
        }
        self.state = state;
        self.tint(cx);
        cx.notify();
    }

    /// Gives the window the shown page's artwork colour, or the neutral look.
    fn tint(&self, cx: &mut App) {
        let palette = match &self.state.page {
            PageState::Ready(page) => match &**page {
                Page::Album(p) => p.header.palette(),
                Page::Playlist(p) => p.header.palette(),
                Page::Artist(p) => p.header.palette(),
                Page::LibraryAlbum(p) => p.header.palette(),
                Page::LibraryPlaylist(p) => p.header.palette(),
                _ => None,
            },
            _ => None,
        };
        theme::set_page_tint(palette.as_ref(), cx);
    }

    /// Runs `future` on the app's runtime, then `done` here with its output.
    fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = self.app.runtime().spawn(future);
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(value) = job.await {
                this.update_in(cx, |this, w, cx| done(this, value, w, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn toast(text: impl Into<String>, failure: bool, window: &mut Window, cx: &mut App) {
        let text: String = text.into();
        let toast = if failure {
            Toast::failure(text)
        } else {
            Toast::new(text)
        };
        Host::request(HostRequest::Toast(toast), window, cx);
    }

    fn load_folder(
        &self,
        view: Entity<PageView>,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let api = self.api.clone();
        let folder = id.clone();
        self.run(
            async move { rustet_app::library::playlist_folder(&api, folder.as_deref()).await },
            move |_, result, w, cx| match (result, id) {
                (Ok(entries), None) => view.update(cx, |v, cx| v.set_root_folders(entries, cx)),
                (Ok(entries), Some(id)) => {
                    view.update(cx, |v, cx| v.set_folder_entries(&id, entries, cx))
                }
                (Err(e), _) => {
                    debug!("playlist folder: {e}");
                    Self::toast("Rustet could not open this folder. Try again.", true, w, cx);
                }
            },
            window,
            cx,
        );
    }

    fn act(
        &mut self,
        actions: Vec<Action>,
        then_back: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app = self.app.clone();
        self.run(
            async move {
                let mut ok = true;
                for action in actions {
                    ok &= app.act(action).await.is_ok();
                }
                ok
            },
            move |this, ok, _, _| {
                if ok && then_back {
                    this.nav.back();
                }
            },
            window,
            cx,
        );
    }

    /// Does what a page asked for.
    pub fn handle(&mut self, event: PageEvent, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        match event {
            PageEvent::Navigate(route) => self.nav.go(route),
            PageEvent::Back => self.nav.back(),
            PageEvent::Retry => self.nav.reload(),
            PageEvent::SignIn => {
                let account = self.app.account().clone();
                self.run(
                    async move { account.sign_in().await },
                    |_, _, _, _| {},
                    window,
                    cx,
                );
            }
            PageEvent::OpenExternal(url) => {
                Host::request(HostRequest::OpenExternal(url), window, cx)
            }
            PageEvent::CopyLink(link) => {
                cx.write_to_clipboard(ClipboardItem::new_string(link));
                Self::toast("Link copied", false, window, cx);
            }
            PageEvent::LoadMore(target) => self.nav.load_more(target),
            PageEvent::Play(request) => {
                if request.starts_music_video() {
                    Host::request(HostRequest::ShowNowPlaying, window, cx);
                }
                let (player, api) = (self.app.player().clone(), self.api.clone());
                self.run(
                    async move {
                        match request {
                            PlayRequest::Intent(intent) => player.apply(intent).await,
                            PlayRequest::Target(target, how) => {
                                player.play_target(&api, &target, how).await
                            }
                        }
                    },
                    |_, result, w, cx| {
                        if let Some(text) = result.err().and_then(|e| e.toast()) {
                            Self::toast(text, true, w, cx);
                        }
                    },
                    window,
                    cx,
                );
            }
            PageEvent::Act(action) => self.act(vec![action], false, window, cx),
            PageEvent::ActAll(actions) => self.act(actions, false, window, cx),
            PageEvent::Confirm {
                title,
                message,
                answer,
                action,
                then_back,
            } => {
                let dialog = cx.new(|_| {
                    ConfirmDialog::new(title, message, answer, move |w, cx| {
                        let action = action.clone();
                        this.update(cx, |a, cx| a.act(vec![action], then_back, w, cx))
                            .ok();
                    })
                });
                modal::open(dialog.into(), window, cx);
            }
            PageEvent::Download(request) => {
                let downloads = self.app.downloads().clone();
                self.run(
                    async move {
                        let one = |target| downloads.download(target);
                        match &request {
                            DownloadRequest::Page(page) => {
                                let target = match &**page {
                                    Page::Album(p) => DownloadTarget::Album(p),
                                    Page::Playlist(p) => DownloadTarget::Playlist(p),
                                    Page::LibraryAlbum(p) => DownloadTarget::LibraryAlbum(p),
                                    Page::LibraryPlaylist(p) => DownloadTarget::LibraryPlaylist(p),
                                    Page::Song(p) => DownloadTarget::Track(&p.track),
                                    Page::MusicVideo(p) => DownloadTarget::Track(&p.video),
                                    _ => {
                                        return Err(
                                            rustet_app::downloads::DownloadError::NotDownloadable,
                                        );
                                    }
                                };
                                one(target).await.map(|_| 1)
                            }
                            DownloadRequest::Item(item) => {
                                one(DownloadTarget::Item(item)).await.map(|_| 1)
                            }
                            DownloadRequest::Tracks(rows) => {
                                let mut queued = 0;
                                for row in rows {
                                    one(DownloadTarget::Track(row)).await?;
                                    queued += 1;
                                }
                                Ok(queued)
                            }
                        }
                    },
                    |_, result, w, cx| match result {
                        Ok(1) => Self::toast("Added to Downloads", false, w, cx),
                        Ok(n) => Self::toast(format!("Added {n} songs to Downloads"), false, w, cx),
                        Err(e) => Self::toast(e.to_string(), true, w, cx),
                    },
                    window,
                    cx,
                );
            }
            PageEvent::NewPlaylist { tracks } => {
                let sheet = cx.new(|cx| {
                    NameSheet::new(
                        "New playlist",
                        "Create",
                        "",
                        move |name, w, cx| {
                            let action = Action::CreatePlaylist {
                                name,
                                description: None,
                                tracks: tracks.clone(),
                            };
                            this.update(cx, |a, cx| a.act(vec![action], false, w, cx))
                                .ok();
                        },
                        window,
                        cx,
                    )
                });
                modal::open(sheet.into(), window, cx);
            }
            PageEvent::RenamePlaylist { id, name } => {
                let sheet = cx.new(|cx| {
                    NameSheet::new(
                        "Rename playlist",
                        "Rename",
                        &name,
                        move |name, w, cx| {
                            let action = Action::EditPlaylist {
                                id: id.clone(),
                                name: Some(name),
                                description: None,
                            };
                            this.update(cx, |a, cx| a.act(vec![action], false, w, cx))
                                .ok();
                        },
                        window,
                        cx,
                    )
                });
                modal::open(sheet.into(), window, cx);
            }
            PageEvent::PickPlaylist { tracks } => {
                let playlists = self.ctx.playlists.clone();
                let picker = cx.new(|cx| {
                    PlaylistPicker::new(
                        playlists,
                        move |choice, w, cx| {
                            let action = Action::AddToPlaylist {
                                playlist_id: choice.id.clone(),
                                tracks: tracks.clone(),
                            };
                            this.update(cx, |a, cx| a.act(vec![action], false, w, cx))
                                .ok();
                        },
                        cx,
                    )
                });
                modal::open(picker.into(), window, cx);
            }
            PageEvent::OpenFolder { id, name } => {
                if let Some(view) = self.current().cloned() {
                    view.update(cx, |v, cx| v.open_folder(id.clone(), name, cx));
                    if id.is_some() {
                        self.load_folder(view, id, window, cx);
                    }
                }
            }
            PageEvent::Menu { entries, position } => {
                let sink = Self::sink(cx);
                let items = menu::to_kit(entries, &sink);
                Host::request(HostRequest::Menu { items, position }, window, cx);
            }
        }
    }

    fn recover(&mut self, recovery: Recovery, window: &mut Window, cx: &mut Context<Self>) {
        let event = match recovery {
            Recovery::Retry => PageEvent::Retry,
            Recovery::SignIn => PageEvent::SignIn,
            Recovery::GoBack if self.state.can_back => PageEvent::Back,
            Recovery::GoBack => PageEvent::Navigate(Route::New),
        };
        self.handle(event, window, cx);
    }
}

/// What a page that needs the account says while signed out: its title, and the state's
/// heading and message.
fn sign_in_text(route: &Route) -> (&'static str, &'static str, &'static str) {
    match route {
        Route::Home => (
            "Home",
            "Sign in to see Home",
            "Home has recommendations from your Apple Music listening. Sign in with your Apple ID to see them.",
        ),
        Route::Library(section) => (
            crate::view::section_title(*section),
            "Sign in to see your library",
            "Your albums, songs and playlists come from your Apple Music account. Sign in with your Apple ID to see them.",
        ),
        _ => (
            "Library",
            "Sign in to see your library",
            "Your albums, songs and playlists come from your Apple Music account. Sign in with your Apple ID to see them.",
        ),
    }
}

impl Render for PageArea {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let route = self.state.route.clone();
        let body: AnyElement = match &self.state.page {
            PageState::Loading => placeholder::render(&route, &theme),
            PageState::Ready(_) => match self.history.kept() {
                Some(kept) => kept.view.clone().into_any_element(),
                None => placeholder::render(&route, &theme),
            },
            PageState::Failed(error) if error.action == Recovery::SignIn => {
                let (page, title, message) = sign_in_text(&route);
                let state = rustet_app::pages::model::EmptyState {
                    title: title.to_owned(),
                    message: message.to_owned(),
                };
                let this = cx.entity().downgrade();
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.l)
                    .px(theme.space.xl)
                    .pt(theme.size.strip)
                    .child(crate::view::page_title(&theme, page))
                    .child(EmptyStateView::new(&state).action("Sign in", move |w, cx| {
                        this.update(cx, |a, cx| a.handle(PageEvent::SignIn, w, cx))
                            .ok();
                    }))
                    .into_any_element()
            }
            PageState::Failed(error) => {
                let this = cx.entity().downgrade();
                div()
                    .px(theme.space.xl)
                    .pt(theme.size.strip)
                    .child(ErrorStateView::new(error).on_recover(move |r, w, cx| {
                        this.update(cx, |a, cx| a.recover(r, w, cx)).ok();
                    }))
                    .into_any_element()
            }
            PageState::Local => div().into_any_element(),
        };
        let push = theme.motion(Motion::PagePush);
        let from = match self.last_move {
            Move::Back => -1.,
            Move::Push | Move::Forward => 1.,
            Move::Stay => 0.,
        };
        let slide = theme.space.l;
        let page = div().relative().size_full().child(body);
        let page: AnyElement = if push.is_zero() || self.moves == 0 || from == 0. {
            page.into_any_element()
        } else {
            page.with_animation(
                ("page-push", self.moves as usize),
                Animation::new(push).with_easing(ease_out_quint()),
                move |el, t| el.left(slide * (from * (1. - t))).opacity(t),
            )
            .into_any_element()
        };
        div()
            .id("page-area")
            .track_focus(&self.focus)
            .size_full()
            .overflow_hidden()
            .child(page)
    }
}
