//! The shell: one entity that follows `rustet_app`'s models and draws the main window around the
//! page. It outlives its window: closing the window hides Rustet (the tray keeps it), and
//! showing it again opens a new window on the same entity.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, AnyView, App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable,
    KeyBinding, KeyDownEvent, MouseButton, NavigationDirection, Pixels, Point, Subscription, Task,
    TitlebarOptions, Window, WindowDecorations, WindowHandle, WindowOptions, actions, div,
    prelude::*, px, size,
};
use rustet_ampapi::api::{Rating, ResourceRef, ResourceType};
use rustet_app::App as RustetApp;
use rustet_app::account::{AccountState, Backend, SigningIn};
use rustet_app::engine::Connection;
use rustet_app::integrations::Intent;
use rustet_app::nav::{Hints, NavState};
use rustet_app::pages::actions::Action;
use rustet_app::pages::catalog::Hint;
use rustet_app::player::unix_ms;
use rustet_app::route::Route;
use rustet_app::settings::{Settings, ShortcutAction};
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::theme::ThemeService;
use rustet_player::model::{Command, HistoryEntry, ItemKind, PlayerState, Volume};
use rustet_ui::components::{
    Circle, Dialog, Glass, IconButton, MenuItem, Popover, SearchField, SearchFieldEvent, Slider,
    SliderEvent, Suggestion, Toast, Toasts,
};
use rustet_ui::content::QualityStrip;
use rustet_ui::frame::{FrameParts, Layout, app_frame, client_inset};
use rustet_ui::host::{Host, HostRequest};
use rustet_ui::icons::Icon;
use rustet_ui::theme::{ActiveTheme, Theme};
use rustet_ui::window_state::WindowState;
use tokio::sync::{broadcast, watch};
use tracing::{info, warn};

use crate::deck::{Room, Scrubber};
use crate::mini::MiniPlayer;
use crate::now_playing::{NowPlaying, VideoRequest};
use crate::rail::PlaylistTree;
use crate::signin::{SignInRequest, SignInSheet};
use crate::{Region, Views, keys, region};

actions!(
    shell,
    [
        FocusSearch,
        Back,
        Forward,
        NextTrack,
        PreviousTrack,
        CloseOverlay,
        ToggleVideoFullscreen,
        Quit
    ]
);

/// The shell's key context.
pub const CONTEXT: &str = "Shell";

/// Context-free key bindings of the main window. `/` and the track keys stay out of fields.
pub fn bind_keys(cx: &mut App) {
    let not_in_field = "Shell && !TextInput && !PasswordInput && !CodeInput";
    cx.bind_keys([
        KeyBinding::new("ctrl-k", FocusSearch, Some(CONTEXT)),
        KeyBinding::new("/", FocusSearch, Some(not_in_field)),
        KeyBinding::new("alt-left", Back, Some(CONTEXT)),
        KeyBinding::new("alt-right", Forward, Some(CONTEXT)),
        KeyBinding::new("ctrl-right", NextTrack, Some(not_in_field)),
        KeyBinding::new("ctrl-left", PreviousTrack, Some(not_in_field)),
        KeyBinding::new("escape", CloseOverlay, Some(CONTEXT)),
        KeyBinding::new("f", ToggleVideoFullscreen, Some(not_in_field)),
        KeyBinding::new("ctrl-q", Quit, Some(CONTEXT)),
    ]);
}

/// What start-up hands the shell besides the app.
pub struct ShellConfig {
    pub prefs: Arc<PrefsFile>,
    pub theme: Arc<ThemeService>,
    pub http: reqwest::Client,
    /// Where backdrops and artwork are cached.
    pub cache_dir: PathBuf,
    /// The platform tells windows where they are (X11, not Wayland).
    pub position_known: bool,
    /// Set by the tray's "Restart App": start again after quitting.
    pub restart: Arc<AtomicBool>,
}

/// What the process asks of the shell from outside gpui.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// A second instance's arguments: open its link, or show the window.
    Args(Vec<String>),
    /// A signal: quit as the tray's Quit does.
    Quit,
}

/// A modal a page or panel handed over, and the focus to give back when it closes.
struct Modal {
    view: AnyView,
    restore: Option<FocusHandle>,
}

/// A dialog of the shell's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    SignOut,
    ForgetDevice,
}

/// The right-side panel that is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// Playing Next, and the history when Settings shows it there.
    Queue,
    Lyrics,
}

/// The views made for the window that is open; made again with the next window.
#[derive(Default)]
struct RegionViews {
    page: Option<AnyView>,
    settings: Option<AnyView>,
    downloads: Option<AnyView>,
}

pub struct Shell {
    pub(crate) app: RustetApp,
    pub(crate) views: Views,
    pub(crate) config: ShellConfig,
    pub(crate) focus: FocusHandle,

    pub(crate) nav: NavState,
    pub(crate) hints: Hints,
    pub(crate) account: AccountState,
    pub(crate) backend: Option<Backend>,
    pub(crate) player: Option<PlayerState>,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) connection: Connection,
    pub(crate) settings: Settings,

    pub(crate) main: Option<WindowHandle<Shell>>,
    pub(crate) mini: Option<WindowHandle<MiniPlayer>>,
    /// The mini player is being opened.
    pub(crate) opening_mini: bool,
    region_views: RegionViews,
    window_subscriptions: Vec<Subscription>,
    bounds_observer: Option<Subscription>,
    save_bounds: Option<Task<()>>,

    /// The search field, drawn at the top of the content on Search pages only.
    pub(crate) search: Entity<SearchField>,
    /// Focus the search field once a Search page shows (the route changes asynchronously).
    focus_search: bool,
    pub(crate) scrubber: Entity<Scrubber>,
    /// The playing stream's format, in the player's artist line.
    pub(crate) quality: Entity<QualityStrip>,
    pub(crate) volume: Entity<Slider>,
    /// The pointer is on the player's speaker or its slider: the slider shows.
    pub(crate) volume_hover: bool,
    /// The pointer is on the player's LCD: the times and the stream's format show.
    pub(crate) lcd_hover: bool,
    pub(crate) sign_in: Entity<SignInSheet>,
    pub(crate) toasts: Entity<Toasts>,
    /// The rail's playlist list scrolls on its own.
    pub(crate) rail_scroll: gpui::ScrollHandle,
    pub(crate) popover: Popover,
    modals: Vec<Modal>,
    pub(crate) confirm: Option<Confirm>,
    /// Whether a sheet or dialog of the shell's own was drawn last frame, and the focus before
    /// it opened.
    own_modal: Option<Option<FocusHandle>>,

    pub(crate) panel: Option<Panel>,
    pub(crate) history_tab: bool,
    pub(crate) now_playing: Entity<NowPlaying>,
    pub(crate) now_playing_open: bool,
    /// A music video fills the window (Now Playing in full screen).
    pub(crate) video_fullscreen: bool,
    /// What the window was last asked: full screen or not.
    fullscreen_applied: bool,
    /// The window has been seen in full screen since it was asked (the change is not
    /// immediate): leaving it after that is the window manager's doing.
    fullscreen_seen: bool,
    pub(crate) playlists: PlaylistTree,
    quitting: bool,
    /// The player failure last said in a toast.
    last_failure: Option<String>,
    /// The main window is being opened.
    opening: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AsMut<Popover> for Shell {
    fn as_mut(&mut self) -> &mut Popover {
        &mut self.popover
    }
}

/// Follows a watch: `apply` gets the current value now and every new one.
fn follow<T>(
    mut rx: watch::Receiver<T>,
    cx: &mut Context<Shell>,
    apply: fn(&mut Shell, T, &mut Context<Shell>),
) -> Task<()>
where
    T: Clone + Send + Sync + 'static,
{
    cx.spawn(async move |this, cx| {
        loop {
            let value = rx.borrow_and_update().clone();
            if this
                .update(cx, |shell, cx| apply(shell, value, cx))
                .is_err()
            {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    })
}

/// Follows a broadcast stream; a lagging receiver skips what it missed.
fn listen<T>(
    mut rx: broadcast::Receiver<T>,
    cx: &mut Context<Shell>,
    apply: fn(&mut Shell, T, &mut Context<Shell>),
) -> Task<()>
where
    T: Clone + Send + 'static,
{
    cx.spawn(async move |this, cx| {
        loop {
            match rx.recv().await {
                Ok(value) => {
                    if this
                        .update(cx, |shell, cx| apply(shell, value, cx))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

pub(crate) fn hint_suggestions(hints: &[Hint]) -> Vec<Suggestion> {
    hints
        .iter()
        .map(|h| match h {
            Hint::Term { display, .. } => Suggestion {
                title: display.clone().into(),
                subtitle: None,
                artwork: None,
            },
            Hint::Item(tile) => Suggestion {
                title: tile.title.clone().into(),
                subtitle: tile.subtitle.clone().map(Into::into),
                artwork: tile.artwork.clone(),
            },
        })
        .collect()
}

impl Shell {
    pub fn new(
        app: RustetApp,
        views: Views,
        config: ShellConfig,
        control: Option<tokio::sync::mpsc::UnboundedReceiver<Control>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| SearchField::new("Artists, songs, lyrics and more", cx));
        let scrubber = cx.new(Scrubber::new);
        let sign_in = cx.new(SignInSheet::new);
        let now_playing =
            cx.new(|cx| NowPlaying::new(app.clone(), config.http.clone(), &config.cache_dir, cx));
        let volume = cx.new(|cx| Slider::new(1., "Volume", cx));
        let mut subscriptions = vec![
            cx.subscribe(&search, Self::on_search),
            cx.subscribe(&sign_in, |this, _, request: &SignInRequest, cx| {
                request.clone().run(this.app.account(), cx)
            }),
            cx.subscribe(
                &now_playing,
                |this, _, request: &VideoRequest, cx| match *request {
                    VideoRequest::ToggleFullscreen => this.toggle_video_fullscreen(cx),
                    VideoRequest::AudioOnly(on) => this.set_mv_audio_only(on, cx),
                },
            ),
            cx.subscribe(&scrubber, |this, _, command: &Command, cx| {
                this.send(command.clone(), cx)
            }),
            cx.subscribe(&volume, |this, _, e: &SliderEvent, cx| {
                let (SliderEvent::Changed(v) | SliderEvent::Committed(v)) = *e;
                let level = (v * 100.).round().clamp(0., 100.) as u8;
                if this.player.as_ref().is_some_and(|s| s.volume != level) {
                    this.send(Command::Volume(Volume { level, muted: None }), cx);
                }
            }),
        ];
        subscriptions.push(
            cx.subscribe(&now_playing, |this, _, command: &Command, cx| {
                this.send(command.clone(), cx)
            }),
        );

        let mut tasks = vec![
            follow(app.nav().state(), cx, Self::apply_nav),
            follow(app.nav().hints(), cx, Self::apply_hints),
            follow(app.account().state(), cx, Self::apply_account),
            follow(app.account().backend(), cx, |s, b, cx| {
                s.backend = b;
                cx.notify();
            }),
            follow(app.account().session(), cx, |s, session, cx| {
                s.playlists.session_changed(session.is_some());
                s.load_playlists(None, cx);
            }),
            follow(app.player().state(), cx, Self::show_player),
            follow(app.player().history(), cx, |s, h, cx| {
                s.history = h;
                cx.notify();
            }),
            follow(app.connection(), cx, |s, c, cx| {
                s.connection = c;
                cx.notify();
            }),
            follow(app.settings().subscribe(), cx, Self::apply_settings),
            listen(app.outcomes(), cx, |s, outcome, cx| {
                let toast = match &outcome.result {
                    Ok(_) => Toast::new(outcome.toast().to_owned()),
                    Err(_) => Toast::failure(outcome.toast().to_owned()),
                };
                s.toasts.update(cx, |t, cx| t.push(toast, cx));
                if matches!(
                    outcome.action,
                    Action::CreatePlaylist { .. }
                        | Action::DeletePlaylist { .. }
                        | Action::EditPlaylist { .. }
                ) {
                    s.playlists.invalidate();
                    s.load_playlists(None, cx);
                }
            }),
            listen(app.intents(), cx, Self::on_intent),
        ];
        if let Some(mut rx) = control {
            tasks.push(cx.spawn(async move |this, cx| {
                while let Some(control) = rx.recv().await {
                    let alive = this.update(cx, |shell, cx| match control {
                        Control::Args(args) => {
                            info!("second instance: {args:?}");
                            match crate::find_link(&args) {
                                Some(link) => shell.open_link(link, cx),
                                None => shell.show_main(cx),
                            }
                        }
                        Control::Quit => shell.quit(cx),
                    });
                    if alive.is_err() {
                        return;
                    }
                }
            }));
        }

        let settings = app.settings().get();
        Self {
            nav: app.nav().state().borrow().clone(),
            hints: Hints::default(),
            account: app.account().state().borrow().clone(),
            backend: *app.account().backend().borrow(),
            player: app.player().state().borrow().clone(),
            history: Vec::new(),
            connection: app.connection().borrow().clone(),
            settings,
            app,
            views,
            config,
            focus: cx.focus_handle(),
            main: None,
            mini: None,
            opening_mini: false,
            region_views: RegionViews::default(),
            window_subscriptions: Vec::new(),
            bounds_observer: None,
            save_bounds: None,
            search,
            focus_search: false,
            scrubber,
            quality: cx.new(|_| QualityStrip::new()),
            volume,
            volume_hover: false,
            lcd_hover: false,
            sign_in,
            toasts: cx.new(|_| Toasts::default()),
            rail_scroll: gpui::ScrollHandle::new(),
            popover: Popover::default(),
            modals: Vec::new(),
            confirm: None,
            own_modal: None,
            panel: None,
            history_tab: false,
            now_playing,
            now_playing_open: false,
            video_fullscreen: false,
            fullscreen_applied: false,
            fullscreen_seen: false,
            playlists: PlaylistTree::default(),
            quitting: false,
            last_failure: None,
            opening: false,
            _tasks: tasks,
            _subscriptions: subscriptions,
        }
    }

    // ── Model updates ───────────────────────────────────────────────────────────────────

    fn apply_nav(&mut self, nav: NavState, cx: &mut Context<Self>) {
        if nav.route != self.nav.route {
            self.now_playing_open = false;
        }
        self.nav = nav;
        cx.notify();
    }

    fn apply_hints(&mut self, hints: Hints, cx: &mut Context<Self>) {
        let suggestions = hint_suggestions(&hints.hints);
        let term = hints.term.clone();
        self.search
            .update(cx, |s, cx| s.set_suggestions(&term, suggestions, cx));
        self.hints = hints;
    }

    fn apply_account(&mut self, state: AccountState, cx: &mut Context<Self>) {
        let signed_in = matches!(state, AccountState::SignedIn { .. });
        if let AccountState::Error { message } = &state
            && self.account != state
        {
            let message = message.clone();
            self.toasts
                .update(cx, |t, cx| t.push(Toast::failure(message), cx));
        }
        if let AccountState::SigningIn(SigningIn::Sheet(view)) = &state {
            let view = view.clone();
            self.sign_in.update(cx, |s, cx| s.set_view(view, cx));
        }
        match self
            .config
            .theme
            .set_signed_in(&serde_json::Value::Bool(signed_in))
        {
            Ok(Some(_)) => self.apply_theme(cx),
            Ok(None) => {}
            Err(e) => warn!("theme: {e}"),
        }
        self.account = state;
        cx.notify();
    }

    pub(crate) fn show_player(&mut self, state: Option<PlayerState>, cx: &mut Context<Self>) {
        // A new player error is also a toast: the deck says it only while it is current.
        // Said once: failures in a row often read the same.
        let message = state
            .as_ref()
            .filter(|s| s.error.is_some())
            .map(|s| crate::deck::failure_message(s, self.signed_in()));
        if message.is_some() && message != self.last_failure {
            let toast = Toast::failure(message.clone().unwrap_or_default());
            self.toasts.update(cx, |t, cx| t.push(toast, cx));
        }
        self.last_failure = message;
        let signed_in = self.signed_in();
        self.scrubber.update(cx, |s, cx| {
            s.signed_in = signed_in;
            s.set_state(state.clone(), cx)
        });
        if let Some(level) = state.as_ref().map(|s| f32::from(s.volume) / 100.) {
            self.volume.update(cx, |v, cx| v.set_value(level, cx));
        }
        let quality = state.as_ref().and_then(|s| s.quality);
        self.quality.update(cx, |q, cx| q.set(quality, cx));
        let follow = self.now_playing_open || self.panel == Some(Panel::Lyrics);
        self.now_playing
            .update(cx, |n, cx| n.set_state(state.clone(), follow, cx));
        self.player = state;
        cx.notify();
    }

    fn apply_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        self.settings = settings;
        self.apply_theme(cx);
        cx.notify();
    }

    /// The theme of the settings in effect, applied when it changed.
    fn apply_theme(&mut self, cx: &mut Context<Self>) {
        let render = match self.config.theme.render() {
            Ok(r) => r,
            Err(e) => return warn!("theme: {e}"),
        };
        let theme =
            Theme::from_settings(&render, &self.config.prefs.load(), cx.window_appearance());
        if *cx.theme() != theme {
            theme.apply(cx);
        }
    }

    pub(crate) fn signed_in(&self) -> bool {
        matches!(self.account, AccountState::SignedIn { .. })
    }

    // ── Commands ─────────────────────────────────────────────────────────────────────────

    /// Sends a player command; a failure that needs words becomes a toast.
    pub(crate) fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        let player = self.app.player().clone();
        let job = rustet_ui::runtime::spawn(cx, async move { player.send(&command).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(e)) = job.await
                && let Some(message) = e.toast()
            {
                this.update(cx, |s, cx| {
                    s.toasts
                        .update(cx, |t, cx| t.push(Toast::failure(message), cx))
                })
                .ok();
            }
        })
        .detach();
    }

    pub(crate) fn go(&mut self, route: Route, cx: &mut Context<Self>) {
        self.now_playing_open = false;
        self.app.nav().go(route);
        cx.notify();
    }

    /// Opens the Search page (unless a search shows) and focuses its field: the sidebar's
    /// Search item and the search shortcut.
    pub(crate) fn open_search(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.nav.route, Route::Search { .. }) {
            self.go(
                Route::Search {
                    term: String::new(),
                },
                cx,
            );
        }
        self.now_playing_open = false;
        self.focus_search = true;
        cx.notify();
    }

    /// Opens a right-side panel, or closes it when it is open. Now Playing gives way to it.
    pub(crate) fn toggle_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        let open = self.panel == Some(panel) && !self.now_playing_open;
        self.now_playing_open = false;
        self.panel = (!open).then_some(panel);
        cx.notify();
    }

    /// Loves the playing song, or takes the love back.
    fn toggle_love(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.player.as_ref().and_then(|s| s.track.clone()) else {
            return;
        };
        if !matches!(self.account, AccountState::SignedIn { .. }) {
            let toast = Toast::failure("Sign in to Apple Music to love songs.");
            return self.toasts.update(cx, |t, cx| t.push(toast, cx));
        }
        let kind = match track.kind {
            ItemKind::Song => ResourceType::Songs,
            ItemKind::MusicVideo => ResourceType::MusicVideos,
        };
        let item = ResourceRef::new(kind, track.id.0);
        let app = self.app.clone();
        let job = rustet_ui::runtime::spawn(cx, async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            let loved = app.api().rating(&cancel, &item).await.ok().flatten() == Some(Rating::Love);
            let rating = (!loved).then_some(Rating::Love);
            // The outcome arrives as a toast through `App::outcomes`.
            let _ = app.act(Action::Rate { item, rating }).await;
        });
        cx.background_spawn(async move { drop(job.await) }).detach();
    }

    pub(crate) fn sign_in(&mut self, cx: &mut Context<Self>) {
        let account = self.app.account().clone();
        let job = rustet_ui::runtime::spawn(cx, async move { account.sign_in().await });
        cx.background_spawn(async move { drop(job.await) }).detach();
    }

    fn sign_out(&mut self, forget_device: bool, cx: &mut Context<Self>) {
        use rustet_app::account::lossless::SignOutResult;
        let account = self.app.account().clone();
        let job =
            rustet_ui::runtime::spawn(cx, async move { account.sign_out(forget_device).await });
        cx.spawn(async move |this, cx| {
            let toast = match job.await {
                Ok(SignOutResult::Done) => Toast::new("Signed out"),
                Ok(SignOutResult::DoneWithNote(note)) => Toast::new(note),
                Ok(SignOutResult::Failed(message)) => Toast::failure(message),
                Err(_) => return,
            };
            this.update(cx, |s, cx| s.toasts.update(cx, |t, cx| t.push(toast, cx)))
                .ok();
        })
        .detach();
    }

    pub(crate) fn retry_engine(&mut self, cx: &mut Context<Self>) {
        let app = self.app.clone();
        let job = rustet_ui::runtime::spawn(cx, async move { app.retry_engine().await });
        cx.background_spawn(async move { drop(job.await) }).detach();
    }

    pub(crate) fn confirm(&mut self, confirm: Confirm, cx: &mut Context<Self>) {
        self.confirm = Some(confirm);
        cx.notify();
    }

    // ── Links, intents, windows ──────────────────────────────────────────────────────────

    /// An `rustet://` or music.apple.com link (command line, a second instance, MPRIS).
    pub(crate) fn open_link(&mut self, link: &str, cx: &mut Context<Self>) {
        match Route::from_link(link) {
            Some(route) => {
                self.go(route, cx);
                self.show_main(cx);
            }
            None => {
                warn!("no page for the link {link}");
                self.show_main(cx);
                let toast = Toast::failure("Rustet has no page for this link.");
                self.toasts.update(cx, |t, cx| t.push(toast, cx));
            }
        }
    }

    fn on_intent(&mut self, intent: Intent, cx: &mut Context<Self>) {
        match intent {
            Intent::ShowWindow => self.show_main(cx),
            Intent::ToggleWindow => match self.main {
                Some(_) => self.hide_main(cx),
                None => self.show_main(cx),
            },
            Intent::ToggleMaximize => {
                if let Some(main) = self.main {
                    cx.update_window(main.into(), |_, window, _| window.zoom_window())
                        .ok();
                }
            }
            Intent::ToggleMiniPlayer => self.toggle_mini(cx),
            Intent::Restart => {
                self.config.restart.store(true, Ordering::SeqCst);
                self.quit(cx);
            }
            Intent::Quit => self.quit(cx),
            Intent::OpenUri(link) => self.open_link(&link, cx),
            Intent::ToggleLove => self.toggle_love(cx),
        }
    }

    /// Opens the main window, or brings it to the front.
    pub fn show_main(&mut self, cx: &mut Context<Self>) {
        if let Some(main) = self.main {
            cx.update_window(main.into(), |_, window, _| window.activate_window())
                .ok();
            return;
        }
        if std::mem::replace(&mut self.opening, true) {
            return;
        }
        let entity = cx.entity();
        let saved = self.config.prefs.load().window_bounds.map(|b| WindowState {
            x: b.x.map(|x| x as f32),
            y: b.y.map(|y| y as f32),
            width: b.width as f32,
            height: b.height as f32,
            maximized: b.maximized,
        });
        let min = size(px(480.), px(400.));
        let state = saved.unwrap_or(WindowState {
            x: None,
            y: None,
            width: 1200.,
            height: 800.,
            maximized: false,
        });
        let bounds = state.to_bounds(min, |sz| Bounds::centered(None, sz, cx));
        let options = WindowOptions {
            window_bounds: Some(bounds),
            titlebar: Some(TitlebarOptions {
                title: Some("Rustet".into()),
                ..Default::default()
            }),
            window_min_size: Some(min),
            window_decorations: Some(WindowDecorations::Client),
            window_background: cx.theme().window_background(true),
            app_id: Some("rustet".into()),
            // Lyrics follow the music in Now Playing, also when the window is in the back.
            inactive_frame_interval: None,
            ..Default::default()
        };
        // Opening draws the window, which draws this entity: not while it is being updated.
        cx.defer(move |cx| {
            let root = entity.clone();
            let opened = cx.open_window(options, move |window, cx| {
                window.on_window_should_close(cx, |_, _| true);
                rustet_ui::theme::follow_system_appearance(window);
                root
            });
            entity.update(cx, |s, cx| {
                s.opening = false;
                match opened {
                    Ok(handle) => s.main_opened(handle, cx),
                    Err(e) => warn!("cannot open the main window: {e}"),
                }
            });
        });
    }

    fn main_opened(&mut self, handle: WindowHandle<Shell>, cx: &mut Context<Self>) {
        self.main = Some(handle);
        let id = handle.window_id();
        cx.update_window(handle.into(), |_, window, cx| {
            window.focus(&self.focus, cx);
        })
        .ok();
        let closed = cx.on_window_closed(move |cx, closed| {
            if closed == id
                && let Some(shell) = cx.try_global::<ShellGlobal>().map(|g| g.0.clone())
            {
                // Window callbacks can run inside an update of the shell.
                cx.defer(move |cx| shell.update(cx, |s, cx| s.main_closed(cx)));
            }
        });
        self.window_subscriptions.push(closed);
        info!("main window shown");
    }

    /// Records the window's size for the next start (debounced: resizing sends many).
    pub(crate) fn bounds_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = WindowState::from_bounds(
            window.window_bounds(),
            client_inset(window),
            self.config.position_known,
        );
        let prefs = self.config.prefs.clone();
        self.save_bounds = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            save_bounds(&prefs, state);
        }));
    }

    fn main_closed(&mut self, cx: &mut Context<Self>) {
        self.main = None;
        self.region_views = RegionViews::default();
        self.window_subscriptions.clear();
        self.bounds_observer = None;
        self.popover.close();
        info!("main window hidden");
        cx.notify();
    }

    /// Hides Rustet to the tray: the window closes, the app runs on.
    pub fn hide_main(&mut self, cx: &mut Context<Self>) {
        if let Some(main) = self.main.take() {
            cx.update_window(main.into(), |_, window, _| window.remove_window())
                .ok();
        }
    }

    /// Quits for good: integrations, tasks and the engine stop first.
    pub fn quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        info!("quitting");
        if let Some(task) = self.save_bounds.take() {
            drop(task);
        }
        if let Some(main) = self.main {
            cx.update_window(main.into(), |_, window, _| {
                let state = WindowState::from_bounds(
                    window.window_bounds(),
                    client_inset(window),
                    self.config.position_known,
                );
                save_bounds(&self.config.prefs, state);
            })
            .ok();
        }
        let app = self.app.clone();
        let job = rustet_ui::runtime::spawn(cx, async move { app.stop().await });
        cx.spawn(async move |_, cx| {
            let _ = job.await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    // ── Host requests ────────────────────────────────────────────────────────────────────

    /// A music video fills the window, or leaves full screen. Now Playing opens with it.
    pub(crate) fn toggle_video_fullscreen(&mut self, cx: &mut Context<Self>) {
        self.video_fullscreen = !self.video_fullscreen;
        if self.video_fullscreen {
            self.now_playing_open = true;
        }
        cx.notify();
    }

    fn set_mv_audio_only(&mut self, on: bool, cx: &mut Context<Self>) {
        if on {
            self.video_fullscreen = false;
        }
        let player = self.app.player().clone();
        let job = rustet_ui::runtime::spawn(cx, async move { player.set_mv_audio_only(on).await });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(e)) = job.await {
                this.update(cx, |s, cx| {
                    let toast = Toast::failure(format!("The video setting did not change: {e}"));
                    s.toasts.update(cx, |t, cx| t.push(toast, cx));
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// Keeps the window's full screen, Now Playing and the video ring reader in step with
    /// `video_fullscreen`. Full screen ends when the video does, or when the window manager
    /// takes the window out of it.
    fn sync_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shows_video = self.now_playing.read(cx).shows_video();
        if self.fullscreen_applied {
            if window.is_fullscreen() {
                self.fullscreen_seen = true;
            } else if self.fullscreen_seen {
                self.video_fullscreen = false;
                self.fullscreen_applied = false;
            }
        }
        if !shows_video || !self.now_playing_open {
            self.video_fullscreen = false;
        }
        if self.video_fullscreen != self.fullscreen_applied {
            if self.video_fullscreen != window.is_fullscreen() {
                window.toggle_fullscreen();
            }
            self.fullscreen_applied = self.video_fullscreen;
            self.fullscreen_seen = false;
        }
        let (open, fullscreen) = (self.now_playing_open, self.video_fullscreen);
        self.now_playing
            .update(cx, |n, cx| n.set_shown(open, fullscreen, cx));
    }

    pub(crate) fn host_request(
        &mut self,
        request: HostRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match request {
            HostRequest::Menu { items, position } => self.open_menu(items, position, window, cx),
            HostRequest::Modal(view) => {
                self.modals.push(Modal {
                    view,
                    restore: window.focused(cx),
                });
                cx.notify();
            }
            HostRequest::CloseModal => {
                if let Some(modal) = self.modals.pop() {
                    let restore = modal.restore.unwrap_or_else(|| self.focus.clone());
                    window.focus(&restore, cx);
                }
                cx.notify();
            }
            HostRequest::Toast(toast) => self.toasts.update(cx, |t, cx| t.push(toast, cx)),
            HostRequest::OpenExternal(url) => cx.open_url(&url),
            HostRequest::ShowNowPlaying => {
                self.now_playing_open = true;
                cx.notify();
            }
        }
    }

    pub(crate) fn open_menu(
        &mut self,
        items: Vec<MenuItem>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = rustet_ui::components::ContextMenu::new(items, window, cx);
        self.popover.show(menu, position, cx);
    }

    // ── Input ───────────────────────────────────────────────────────────────────────────

    fn on_search(
        &mut self,
        _: Entity<SearchField>,
        event: &SearchFieldEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            SearchFieldEvent::Query(q) => self.app.nav().search_input(q),
            SearchFieldEvent::Submit(term) if !term.trim().is_empty() => {
                self.go(
                    Route::Search {
                        term: term.trim().to_owned(),
                    },
                    cx,
                );
            }
            SearchFieldEvent::Submit(_) => {}
            SearchFieldEvent::Choose(ix) => match self.hints.hints.get(*ix).cloned() {
                Some(Hint::Term { term, .. }) => self.go(Route::Search { term }, cx),
                Some(Hint::Item(tile)) => {
                    if let Some(route) = tile.route.clone() {
                        self.go(route, cx);
                    }
                }
                None => {}
            },
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = keys::dom_key(&event.keystroke) else {
            return;
        };
        let on_page = window.focused(cx).is_none_or(|f| f == self.focus);
        let stack = window.context_stack();
        if !keys::may_fire(&key, |name| stack.iter().any(|c| c.contains(name)), on_page) {
            return;
        }
        let Some(action) = self.settings.shortcuts.action_for(&key) else {
            return;
        };
        cx.stop_propagation();
        if action == ShortcutAction::Love {
            return self.toggle_love(cx);
        }
        if let Some(command) = self
            .player
            .as_ref()
            .and_then(|s| action.command(s, unix_ms()))
        {
            self.send(command, cx);
        }
    }

    // ── Drawing ─────────────────────────────────────────────────────────────────────────

    /// The view that draws the page region now, made for this window on first use.
    fn region_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyView {
        let (slot, make) = match region(&self.nav) {
            Region::Page => (&mut self.region_views.page, self.views.page),
            Region::Settings => (&mut self.region_views.settings, self.views.settings),
            Region::Downloads => (&mut self.region_views.downloads, self.views.downloads),
        };
        slot.get_or_insert_with(|| make(&self.app, window, cx))
            .clone()
    }

    /// The page region. Pages start under the strip themselves (`FrameParts::page`).
    fn page_region(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let region = self.region_view(window, cx);
        div()
            .size_full()
            .text_size(cx.theme().text.body)
            .child(region)
            .into_any_element()
    }

    /// The strip's left: back and forward as 28 px glass circles, after the menu of the
    /// sidebar's destinations when the window is too narrow for a sidebar.
    fn title_leading(&mut self, layout: Layout, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (can_back, can_forward) = (self.nav.can_back, self.nav.can_forward);
        let nav = self.app.nav().clone();
        let nav2 = nav.clone();
        div()
            .flex()
            .items_center()
            .gap(theme.space.s)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when(layout == Layout::Compact, |d| {
                d.child(
                    IconButton::new("rail-menu", Icon::Menu, "Menu")
                        .glass(Circle::Small)
                        .on_click(cx.listener(|this, e: &gpui::ClickEvent, window, cx| {
                            let items = this.compact_menu(cx);
                            this.open_menu(items, e.position(), window, cx);
                        })),
                )
            })
            .child(
                IconButton::new("back", Icon::ChevronLeft, "Back")
                    .glass(Circle::Small)
                    .disabled(!can_back)
                    .on_click(move |_, _, _| nav.back()),
            )
            .child(
                IconButton::new("forward", Icon::ChevronRight, "Forward")
                    .glass(Circle::Small)
                    .disabled(!can_forward)
                    .on_click(move |_, _, _| nav2.forward()),
            )
            .into_any_element()
    }

    /// The search field at the top centre of the content, inside the strip: a 38 px pill at
    /// most 500 px wide. Drawn on Search pages only.
    fn search_bar(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let Route::Search { .. } = self.nav.route else {
            return None;
        };
        let theme = cx.theme();
        // Back and forward stay clear at the left: the field keeps that much from both edges.
        let clear = theme.space.m + theme.size.circle_small * 2. + theme.space.l;
        Some(
            div()
                .absolute()
                .top_0()
                .left(clear)
                .right(clear)
                .h(theme.size.strip)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w_full()
                        .max_w(theme.size.search_max_width)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(self.search.clone()),
                )
                .into_any_element(),
        )
    }

    /// The engine's link when it is not up: restarting, or failed with what to do. A glass
    /// card centred just above the player pill, over the content: it is about playback.
    fn connection_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use rustet_ui::components::Button;
        let theme = cx.theme().clone();
        let (message, retry): (String, bool) = match &self.connection {
            Connection::Ready | Connection::Starting | Connection::Stopped => return None,
            Connection::Restarting { retry_in: Some(d) } => (
                format!(
                    "Rustet's engine stopped. It starts again in {} s; playback continues then.",
                    d.as_secs().max(1)
                ),
                false,
            ),
            Connection::Restarting { retry_in: None } => (
                "Rustet's engine stopped. It starts again in a moment.".to_owned(),
                false,
            ),
            Connection::Failed { reason } => {
                (format!("Rustet's engine cannot start. {reason}"), true)
            }
        };
        let banner = div()
            .id("engine-banner")
            .role(gpui::Role::Alert)
            .aria_label(message.clone())
            .max_w(theme.size.player_max_width)
            .min_h(theme.size.settings_row)
            .pl(theme.space.m16)
            .pr(theme.space.s)
            .py(theme.space.s)
            .flex()
            .items_center()
            .gap(theme.space.m)
            .rounded(theme.radius.menu)
            .glass(cx)
            .child(
                Icon::Alert
                    .element()
                    .size(theme.size.icon_small)
                    .text_color(theme.colors.text_secondary),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme.text.body)
                    .text_color(theme.colors.text)
                    .child(message),
            )
            .when(retry, |d| {
                d.child(
                    Button::new("engine-retry", "Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx))),
                )
            });
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(theme.size.player_bottom + theme.size.player_height + theme.space.m)
                .px(theme.space.l)
                .flex()
                .justify_center()
                .child(banner)
                .into_any_element(),
        )
    }

    fn confirm_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.confirm?;
        let (title, message, answer) = match confirm {
            Confirm::SignOut => (
                "Sign out of Apple Music?",
                "Rustet removes your library and play history from this computer. Settings stay.",
                "Sign out",
            ),
            Confirm::ForgetDevice => (
                "Forget this device?",
                "Rustet signs out and Apple forgets this computer. The next sign-in asks for a verification code.",
                "Forget this device",
            ),
        };
        let entity = cx.entity();
        let close = move |e: Entity<Shell>, act: bool| {
            move |_: &mut Window, cx: &mut App| {
                e.update(cx, |s, cx| {
                    s.confirm = None;
                    if act {
                        s.sign_out(confirm == Confirm::ForgetDevice, cx);
                    }
                    cx.notify();
                })
            }
        };
        Some(
            Dialog::new("confirm", title, message)
                .answer(answer, close(entity.clone(), true))
                .answer("Cancel", close(entity.clone(), false))
                .on_dismiss(close(entity, false))
                .into_any_element(),
        )
    }

    /// The account's own modal: the sign-in sheet or the web sign-in's note.
    fn account_modal(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        match &self.account {
            AccountState::SigningIn(SigningIn::Sheet(_)) => {
                Some(self.sign_in.clone().into_any_element())
            }
            AccountState::SigningIn(SigningIn::WebWindow) => Some(crate::signin::web_window_sheet(
                self.app.account().clone(),
                cx,
            )),
            _ => None,
        }
    }
}

/// Writes the window's geometry to the prefs (`windowBounds`, as the web shell kept it).
fn save_bounds(prefs: &PrefsFile, state: WindowState) {
    let bounds = rustet_desktop::prefs::WindowBounds {
        width: state.width.round() as i32,
        height: state.height.round() as i32,
        x: state.x.map(|x| x.round() as i32),
        y: state.y.map(|y| y.round() as i32),
        maximized: state.maximized,
    };
    if let Err(e) = prefs.update(|p| p.window_bounds = Some(bounds)) {
        warn!("cannot save the window size: {e}");
    }
}

/// The shell entity, for the host handler and window callbacks.
pub struct ShellGlobal(pub Entity<Shell>);

impl gpui::Global for ShellGlobal {}

/// Installs the shell as the window host: pages' menus, modals, toasts and links come here.
pub fn install_host(shell: Entity<Shell>, cx: &mut App) {
    cx.set_global(ShellGlobal(shell.clone()));
    Host::install(
        move |request, window, cx| shell.update(cx, |s, cx| s.host_request(request, window, cx)),
        cx,
    );
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        rustet_ui::artwork::ArtworkStore::global(cx).update(cx, |a, _| a.release(window));
        if self.bounds_observer.is_none() {
            // Once per window: its size is kept for the next start.
            self.bounds_observer =
                Some(cx.observe_window_bounds(window, |this, window, cx| {
                    this.bounds_changed(window, cx)
                }));
        }
        let theme = cx.theme().clone();
        let layout = Layout::for_width(window.viewport_size().width / theme.zoom);

        let mut overlays: Vec<AnyElement> = Vec::new();
        let own = self.account_modal(cx).or_else(|| self.confirm_dialog(cx));
        // Focus goes back where it was when the shell's own sheet or dialog closes.
        match (&self.own_modal, own.is_some()) {
            (None, true) => self.own_modal = Some(window.focused(cx)),
            (Some(before), false) => {
                let restore = before.clone().unwrap_or_else(|| self.focus.clone());
                window.focus(&restore, cx);
                self.own_modal = None;
            }
            _ => {}
        }
        overlays.extend(
            self.modals
                .iter()
                .map(|m| m.view.clone().into_any_element()),
        );
        overlays.extend(own);
        overlays.extend(self.popover.render(cx));

        self.sync_fullscreen(window, cx);
        if self.focus_search && matches!(self.nav.route, Route::Search { .. }) {
            self.focus_search = false;
            let handle = self.search.focus_handle(cx);
            window.focus(&handle, cx);
        }
        let (rail, page, leading, backdrop, drawer) = if self.video_fullscreen {
            let page = div()
                .size_full()
                .child(self.now_playing.clone())
                .into_any_element();
            (None, page, None, None, None)
        } else if self.now_playing_open {
            let page = div()
                .size_full()
                .flex()
                .flex_col()
                .pt(theme.size.strip)
                .child(div().flex_1().min_h_0().child(self.now_playing.clone()))
                .into_any_element();
            let leading = Some(self.now_playing_leading(cx));
            let backdrop = div()
                .absolute()
                .inset_0()
                .child(self.now_playing.read(cx).backdrop())
                .child(div().absolute().inset_0().bg(theme.colors.scrim))
                .into_any_element();
            (None, page, leading, Some(backdrop), None)
        } else {
            let rail = (layout != Layout::Compact).then(|| self.rail(layout, cx));
            let drawer = self.panel.map(|panel| match panel {
                Panel::Queue => self.drawer(cx),
                Panel::Lyrics => self.lyrics_panel(cx),
            });
            let leading = Some(self.title_leading(layout, cx));
            (rail, self.page_region(window, cx), leading, None, drawer)
        };
        // The pill's width: the content area (the window less its decoration band, the
        // sidebar and the panel), less the frame's side margins, at most the pill's own.
        let mut content = window.viewport_size().width - client_inset(window) * 2.;
        if rail.is_some() {
            content -= layout.sidebar_width(&theme);
        }
        if drawer.is_some() {
            content -= theme.size.panel;
        }
        let pill = (content - theme.space.l * 2.).min(theme.size.player_max_width);
        let deck = self.deck(Room::for_width(pill, &theme), window, cx);
        let mut content_overlays: Vec<AnyElement> = self.search_bar(cx).into_iter().collect();
        content_overlays.extend(self.connection_banner(cx));
        content_overlays.push(self.toasts.clone().into_any_element());

        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|this, _: &FocusSearch, _, cx| this.open_search(cx)))
            .on_action(cx.listener(|this, _: &Back, _, _| this.app.nav().back()))
            .on_action(cx.listener(|this, _: &Forward, _, _| this.app.nav().forward()))
            .on_action(cx.listener(|this, _: &NextTrack, _, cx| this.send(Command::Next, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousTrack, _, cx| this.send(Command::Previous, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleVideoFullscreen, _, cx| {
                if this.now_playing.read(cx).shows_video() {
                    this.toggle_video_fullscreen(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &CloseOverlay, _, cx| {
                if this.video_fullscreen {
                    this.video_fullscreen = false;
                } else if this.now_playing_open {
                    this.now_playing_open = false;
                } else {
                    this.panel = None;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Quit, _, cx| this.quit(cx)))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, _, _| this.app.nav().back()),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, _, _| this.app.nav().forward()),
            )
            .child(app_frame(
                FrameParts {
                    sidebar: rail,
                    page,
                    leading,
                    trailing: None,
                    panel: drawer,
                    player: Some(deck),
                    backdrop,
                    content_overlays,
                    overlays,
                },
                window,
                cx,
            ))
    }
}
