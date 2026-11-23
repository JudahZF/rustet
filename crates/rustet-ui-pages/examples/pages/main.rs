//! The page area in a minimal window, around a real `App`, signed out.
//!
//!     cargo run --release -p rustet-ui-pages --example pages -- [--light] [--fixtures | --account]
//!         [--songs N] [--route LINK] [--size WxH] [--scenario live|fixtures|bench]
//!
//! The binary plays both roles of `rustet`: started as `<exe> engine --api PORT
//! --tls-identity-stdin` it is the engine the `App` supervises. It first starts itself again
//! inside a temporary home (`HOME`, `XDG_*`, `TMPDIR`), so neither it nor its engine touches
//! `~/.config/rustet` or `~/.cache/rustet`; the engine gets a free port (never 20025), and the
//! desktop integrations stay off.
//!
//! - Without `--fixtures` the pages come from Apple's live catalog, signed out.
//! - With `--fixtures` they come from a local mock of Apple's API (`fixtures.rs`) through a
//!   `Nav` over a signed-in mock `Api`: Home and the library, which need an account. Playing
//!   and actions still go to the real `App` (signed out: they fail with their toasts).
//! - With `--account` (for the user, never an agent) there is no temporary home: the `App`
//!   opens the user's own `~/.config/rustet` and `~/.cache/rustet` with the keyring, as the shell
//!   does, so the pages are signed in. Quit the installed Rustet first: both would use the same
//!   FairPlay session. The engine still gets a free port.
//!
//! A route field (Ctrl+L) takes an `rustet://` or music.apple.com link, or a search term.
//! Alt+Left and Alt+Right go back and forward. Menus, modals and toasts are drawn by the
//! window's `Host` handler. `--scenario` drives the window for screenshots (`scenario.rs`);
//! run it inside `crates/rustet-ui/scripts/headless.sh`, never on a desktop session.

mod fixtures;
mod scenario;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, size,
};
use rustet_app::account::SignInProgram;
use rustet_app::nav::Nav;
use rustet_app::route::Route;
use rustet_app::{App as RustetApp, AppConfig};
use rustet_desktop::engine::SupervisorTiming;
use rustet_desktop::platform::Env;
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::{Store, StoreCipher};
use rustet_ui::artwork::{ArtworkStore, DiskCache, StoreConfig};
use rustet_ui::components::{
    ContextMenu, IconButton, Popover, TextInput, TextInputEvent, Toast, Toasts,
};
use rustet_ui::frame_stats::FrameStats;
use rustet_ui::host::{Host, HostRequest};
use rustet_ui::icons::{Assets, Icon};
use rustet_ui::theme::{ActiveTheme, Appearance, Theme};
use rustet_ui_pages::PageArea;
use serde_json::json;

actions!(
    pages_demo,
    [Back, Forward, FocusRoute, NextFocus, PreviousFocus, Quit]
);

/// Prints one machine-readable report line.
pub fn report(kind: &str, value: serde_json::Value) {
    println!("REPORT {}", json!({ "kind": kind, "value": value }));
}

struct Args {
    light: bool,
    fixtures: bool,
    /// Library songs the fixtures have (the frame-time run).
    songs: Option<usize>,
    start: Route,
    size: (f32, f32),
    scenario: Option<String>,
    /// The user's own config, cache and session (`--account`).
    account: bool,
}

fn parse_args(args: &[String]) -> Args {
    let mut a = Args {
        light: false,
        fixtures: false,
        songs: None,
        start: Route::New,
        size: (1360., 860.),
        scenario: None,
        account: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--light" => a.light = true,
            "--fixtures" => a.fixtures = true,
            "--songs" => a.songs = it.next().and_then(|n| n.parse().ok()),
            "--route" => {
                if let Some(r) = it.next().and_then(|l| Route::from_link(l)) {
                    a.start = r;
                }
            }
            "--size" => {
                if let Some((w, h)) = it.next().and_then(|s| s.split_once('x')) {
                    a.size = (w.parse().unwrap_or(a.size.0), h.parse().unwrap_or(a.size.1));
                }
            }
            "--scenario" => a.scenario = it.next().cloned(),
            "--account" => a.account = true,
            other => eprintln!("pages: ignoring {other}"),
        }
    }
    a
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("engine") {
        return engine(&args[1..]);
    }
    if args.iter().any(|a| a == "--account") {
        // Artwork goes to a temporary directory; everything else is the user's.
        let Ok(scratch) = tempfile::Builder::new().prefix("rustet-pages.").tempdir() else {
            return ExitCode::FAILURE;
        };
        return window(parse_args(&args), scratch.path().to_owned());
    }
    match std::env::var_os("RUSTET_PAGES_ROOT") {
        None => sandboxed(&args),
        Some(root) => window(parse_args(&args), PathBuf::from(root)),
    }
}

/// `<exe> engine --api PORT [--tls-identity-stdin]`, as `crates/rustet/src/main.rs` runs it.
fn engine(args: &[String]) -> ExitCode {
    let mut port = None;
    let mut tls = rustet_engine::TlsSource::Generate;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--api" => port = it.next().and_then(|p| p.parse::<u16>().ok()),
            "--tls-identity-stdin" => tls = rustet_engine::TlsSource::Stdin,
            _ => {}
        }
    }
    let Some(port) = port else {
        eprintln!("usage: pages engine --api <port> [--tls-identity-stdin]");
        return ExitCode::from(2);
    };
    rustet_engine::prepare_process();
    let result = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(rustet_engine::run(rustet_engine::Options { port, tls })));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pages engine: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Starts this program again in a temporary home and removes it afterwards.
fn sandboxed(args: &[String]) -> ExitCode {
    let run = || -> anyhow::Result<ExitCode> {
        let root = tempfile::Builder::new().prefix("rustet-pages.").tempdir()?;
        let dir = |p: &str| root.path().join(p);
        for d in ["home", "config", "cache", "data", "tmp"] {
            std::fs::create_dir_all(dir(d))?;
        }
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(args)
            .env("RUSTET_PAGES_ROOT", root.path())
            .env("HOME", dir("home"))
            .env("XDG_CONFIG_HOME", dir("config"))
            .env("XDG_CACHE_HOME", dir("cache"))
            .env("XDG_DATA_HOME", dir("data"))
            .env("TMPDIR", dir("tmp"))
            .env("RUSTET_EXPORT_ROOTS", dir("home"))
            .status()?;
        Ok(ExitCode::from(
            status.code().unwrap_or(1).clamp(0, 255) as u8
        ))
    };
    run().unwrap_or_else(|e| {
        eprintln!("pages: {e:#}");
        ExitCode::FAILURE
    })
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(20125)
}

/// The app over the user's own config directory, as the shell starts it (`--account`).
async fn start_account_app(start: Route) -> anyhow::Result<RustetApp> {
    use rustet_desktop::platform::{Platform, config_root};
    use rustet_desktop::store::{LinuxBackend, OsKeyring};
    let env = Env::from_process();
    let home = PathBuf::from(env.get("HOME").unwrap_or("/"));
    let config_dir = config_root(Platform::current(), &env, &home).join("rustet");
    let keyring = OsKeyring {
        backend: LinuxBackend::from_desktop(&env),
    };
    let store = Store::open(&config_dir, StoreCipher::from_keyring(&keyring).await).await;
    let prefs = Arc::new(PrefsFile::new(&config_dir));
    let mut config = AppConfig::installed(&config_dir, store, prefs, &env)?;
    config.engine_port = free_port();
    config.desktop = false;
    config.exe_dir = None;
    config.start = start;
    Ok(RustetApp::start(config, env).await?)
}

async fn start_app(root: PathBuf, start: Route) -> anyhow::Result<RustetApp> {
    let config_dir = root.join("config/rustet");
    std::fs::create_dir_all(&config_dir)?;
    let config = AppConfig {
        store: Store::open(&config_dir, StoreCipher::Plain).await,
        prefs: Arc::new(PrefsFile::new(&config_dir)),
        cache_dir: root.join("cache/rustet"),
        engine_port: free_port(),
        timing: SupervisorTiming::default(),
        drm_session_dir: config_dir.join("drm/files"),
        sign_in: SignInProgram {
            program: "false".into(),
            args: Vec::new(),
        },
        language: "en-US".into(),
        desktop: false,
        exe_dir: None,
        start,
        config_dir,
    };
    Ok(RustetApp::start(config, Env::from_process()).await?)
}

pub struct Demo {
    pub app: Option<RustetApp>,
    pub area: Option<Entity<PageArea>>,
    pub nav: Option<Nav>,
    route_field: Entity<TextInput>,
    popover: Popover,
    modal: Option<(AnyView, Option<FocusHandle>)>,
    /// Focus to give back once the menu closes.
    menu_focus: Option<FocusHandle>,
    toasts: Entity<Toasts>,
    focus: FocusHandle,
    pub frame_stats: FrameStats,
    /// Pixels per frame the bench scrolls the library list.
    pub autoscroll: Option<f32>,
    status: String,
    _keep: Vec<Box<dyn std::any::Any>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl AsMut<Popover> for Demo {
    fn as_mut(&mut self) -> &mut Popover {
        &mut self.popover
    }
}

impl Demo {
    fn new(args: &Args, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let route_field = cx.new(|cx| TextInput::new("Link or search, then Enter", cx));
        let subscriptions = vec![cx.subscribe_in(
            &route_field,
            window,
            |this, field, e: &TextInputEvent, _, cx| {
                if let TextInputEvent::Submitted(text) = e {
                    let text = text.trim();
                    let route = Route::from_link(text).unwrap_or_else(|| Route::Search {
                        term: text.to_owned(),
                    });
                    if let Some(nav) = &this.nav {
                        nav.go(route);
                    }
                    field.update(cx, |f, cx| f.set_text("", cx));
                }
            },
        )];
        let me = cx.entity().downgrade();
        Host::install(
            move |request, window, cx| {
                me.update(cx, |d, cx| d.host(request, window, cx)).ok();
            },
            cx,
        );

        let (fixtures, songs, start, account) =
            (args.fixtures, args.songs, args.start.clone(), args.account);
        let job = rustet_ui::runtime::spawn(cx, async move {
            let app = if account {
                start_account_app(start.clone()).await?
            } else {
                start_app(root, start.clone()).await?
            };
            let api = if fixtures {
                Some(fixtures::api(songs).await?)
            } else {
                None
            };
            anyhow::Ok((app, api, start))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await.map_err(anyhow::Error::from).and_then(|r| r);
            this.update_in(cx, |this, window, cx| match result {
                Ok((app, api, start)) => this.started(app, api, start, window, cx),
                Err(e) => {
                    report("error", json!(format!("{e:#}")));
                    this.status = format!("Rustet could not start: {e:#}");
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            app: None,
            area: None,
            nav: None,
            route_field,
            popover: Popover::default(),
            modal: None,
            menu_focus: None,
            toasts: cx.new(|_| Toasts::default()),
            focus,
            frame_stats: FrameStats::default(),
            autoscroll: None,
            status: "Starting the engine…".to_owned(),
            _keep: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    fn started(
        &mut self,
        app: RustetApp,
        fixture_api: Option<rustet_ampapi::Api>,
        start: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fixtures = fixture_api.is_some();
        let (nav, api) = match fixture_api {
            Some(api) => {
                let (session, rx) = tokio::sync::watch::channel(());
                self._keep.push(Box::new(session));
                let nav = Nav::new(api.clone(), app.runtime().clone(), start, rx);
                (nav, api)
            }
            None => (app.nav().clone(), app.api().clone()),
        };
        let area = cx.new(|cx| PageArea::new(app.clone(), nav.clone(), api, window, cx));
        if fixtures {
            area.update(cx, |a, cx| {
                a.set_playlists(fixtures::playlist_choices(), cx)
            });
        }
        report("started", json!({ "fixtures": fixtures }));
        self.app = Some(app);
        self.nav = Some(nav);
        self.area = Some(area);
        self.status.clear();
        cx.notify();
    }

    fn host(&mut self, request: HostRequest, window: &mut Window, cx: &mut Context<Self>) {
        match request {
            HostRequest::Menu { items, position } => {
                self.menu_focus = window.focused(cx);
                let menu = ContextMenu::new(items, window, cx);
                self.popover.show(menu, position, cx);
            }
            HostRequest::Modal(view) => {
                self.popover.close();
                self.modal = Some((view, window.focused(cx)));
                cx.notify();
            }
            HostRequest::CloseModal => {
                if let Some((_, Some(focus))) = self.modal.take() {
                    window.focus(&focus, cx);
                }
                cx.notify();
            }
            HostRequest::Toast(toast) => self.toasts.update(cx, |t, cx| t.push(toast, cx)),
            HostRequest::ShowNowPlaying => report("show-now-playing", json!(true)),
            HostRequest::OpenExternal(url) => {
                report("open-external", json!(url));
                self.toasts.update(cx, |t, cx| {
                    t.push(Toast::new(format!("Opens in the browser: {url}")), cx)
                });
            }
        }
    }

    pub fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        Theme::new(appearance).apply(cx);
    }
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame_stats.begin();
        if let Some(speed) = self.autoscroll {
            scenario::autoscroll_step(self, speed, cx);
            window.request_animation_frame();
        }
        ArtworkStore::global(cx).update(cx, |a, _| a.release(window));
        if !self.popover.is_open()
            && let Some(focus) = self.menu_focus.take()
        {
            window.focus(&focus, cx);
        }
        let theme = cx.theme().clone();
        let appearance = theme.appearance;
        let nav = self.nav.clone();
        let (back, forward) = (nav.clone(), nav);
        let me = cx.entity();
        let bar = div()
            .flex()
            .items_center()
            .gap(theme.space.s)
            .px(theme.space.m)
            .py(theme.space.s)
            .border_b_1()
            .border_color(theme.colors.divider)
            .child(
                IconButton::new("back", Icon::ChevronLeft, "Back").on_click(move |_, _, _| {
                    if let Some(n) = &back {
                        n.back();
                    }
                }),
            )
            .child(
                IconButton::new("forward", Icon::ChevronRight, "Forward").on_click(
                    move |_, _, _| {
                        if let Some(n) = &forward {
                            n.forward();
                        }
                    },
                ),
            )
            .child(div().flex_1().child(self.route_field.clone()))
            .child(
                IconButton::new("appearance", Icon::Settings, "Switch appearance").on_click(
                    move |_, _, cx| {
                        let next = if appearance == Appearance::Dark {
                            Appearance::Light
                        } else {
                            Appearance::Dark
                        };
                        me.update(cx, |d, cx| d.set_appearance(next, cx));
                    },
                ),
            );
        let body: AnyElement = match &self.area {
            Some(area) => area.clone().into_any_element(),
            None => div()
                .p(theme.space.xl)
                .text_color(theme.colors.text_secondary)
                .child(self.status.clone())
                .into_any_element(),
        };
        div()
            .key_context("PagesDemo")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Back, _, _| {
                if let Some(n) = &this.nav {
                    n.back();
                }
            }))
            .on_action(cx.listener(|this, _: &Forward, _, _| {
                if let Some(n) = &this.nav {
                    n.forward();
                }
            }))
            .on_action(cx.listener(|this, _: &FocusRoute, window, cx| {
                let handle = this.route_field.focus_handle(cx);
                window.focus(&handle, cx);
            }))
            .on_action(|_: &NextFocus, window, cx| window.focus_next(cx))
            .on_action(|_: &PreviousFocus, window, cx| window.focus_prev(cx))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.base())
            .text_color(theme.colors.text)
            .font_family(rustet_ui::theme::FONT_FAMILY)
            .child(bar)
            .child(div().flex_1().min_h_0().child(body))
            .children(self.popover.render(cx))
            .children(self.modal.as_ref().map(|(v, _)| v.clone()))
            .child(self.toasts.clone())
            .child(self.frame_stats.probe())
    }
}

fn window(args: Args, root: PathBuf) -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let scenario = args.scenario.clone();
    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            let theme = Theme::new(if args.light {
                Appearance::Light
            } else {
                Appearance::Dark
            });
            rustet_ui::init(theme, None, cx).expect("init");
            cx.bind_keys([
                KeyBinding::new("alt-left", Back, None),
                KeyBinding::new("alt-right", Forward, None),
                KeyBinding::new("ctrl-l", FocusRoute, None),
                KeyBinding::new("tab", NextFocus, None),
                KeyBinding::new("shift-tab", PreviousFocus, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);
            let http = reqwest::Client::builder()
                .user_agent("rustet-ui-pages-example")
                .build()
                .expect("http client");
            let disk = DiskCache::open(root.join("artwork"), 256 << 20)
                .ok()
                .map(Arc::new);
            ArtworkStore::install(http, disk, StoreConfig::default(), cx);
            let bounds = Bounds::centered(None, size(px(args.size.0), px(args.size.1)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Rustet pages".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(480.), px(400.))),
                window_decorations: Some(gpui::WindowDecorations::Server),
                app_id: Some("rustet-pages".into()),
                inactive_frame_interval: None,
                ..Default::default()
            };
            let window = cx
                .open_window(options, |window, cx| {
                    cx.new(|cx| Demo::new(&args, root.clone(), window, cx))
                })
                .expect("open window");
            if let Some(name) = scenario.clone() {
                scenario::run(name, window, cx);
            }
        });
    ExitCode::SUCCESS
}
