//! The Settings and Downloads pages around a real, signed-out `App`, in a minimal window.
//!
//!     cargo run -p rustet-ui-panels --example panels -- [--state-dir DIR] [--light|--dark]
//!         [--route settings/<section>|downloads] [--scenario tour|preview|write|verify|a11y]
//!
//! One binary, two roles, as `crates/rustet` has them: `panels engine --api <port>
//! [--tls-identity-stdin]` is the engine the app starts as its child. Everything lives under
//! the state directory (a new temporary one by default): the process starts itself again with
//! `HOME` and `XDG_*` pointed there, so the user's `~/.config/rustet` and `~/.cache/rustet` are
//! never read. The engine listens on a free port (never 20025). No desktop integrations, no
//! web sign-in window, no FairPlay download. Run it inside `crates/rustet-ui/scripts/headless.sh`.
//!
//! `--scenario` drives the window (`scenario.rs`) and prints `REPORT {json}` lines.

mod scenario;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, AppContext as _, Bounds, Context, Entity, FocusHandle, KeyBinding, Render,
    Role, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, size,
};
use rustet_app::account::SignInProgram;
use rustet_app::route::Route;
use rustet_app::{App, AppConfig};
use rustet_desktop::engine::SupervisorTiming;
use rustet_desktop::platform::Env;
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::{Store, StoreCipher};
use rustet_desktop::theme::ThemeService;
use rustet_ui::components::{FocusRing, Popover, Toasts};
use rustet_ui::host::{Host, HostRequest};
use rustet_ui::icons::Assets;
use rustet_ui::theme::{ActiveTheme, Appearance, Theme};
use serde_json::json;

actions!(panels, [NextFocus, PreviousFocus, Quit]);

/// Set in the re-started process: its environment points at the state directory.
const CHILD: &str = "RUSTET_PANELS_EXAMPLE_CHILD";

pub fn report(kind: &str, value: serde_json::Value) {
    println!("REPORT {}", json!({ "kind": kind, "value": value }));
}

struct Args {
    state_dir: Option<PathBuf>,
    appearance: Option<Appearance>,
    route: Route,
    scenario: Option<String>,
}

fn parse_args(args: &[String]) -> Args {
    let mut out = Args {
        state_dir: None,
        appearance: None,
        route: Route::Settings { section: None },
        scenario: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--state-dir" => out.state_dir = it.next().map(PathBuf::from),
            "--light" => out.appearance = Some(Appearance::Light),
            "--dark" => out.appearance = Some(Appearance::Dark),
            "--scenario" => out.scenario = it.next().cloned(),
            "--route" => {
                out.route = match it.next().map(String::as_str) {
                    Some("downloads") => Route::Downloads,
                    Some(r) => Route::Settings {
                        section: r.strip_prefix("settings/").map(str::to_owned),
                    },
                    None => out.route,
                }
            }
            other => eprintln!("ignored argument {other}"),
        }
    }
    out
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("engine") {
        return run_engine(&args[1..]);
    }
    if std::env::var_os(CHILD).is_none() {
        return restart_inside_state_dir(&args);
    }
    run_window(parse_args(&args))
}

/// `engine --api <port> [--tls-identity-stdin]`, as `rustet engine`.
fn run_engine(args: &[String]) -> ExitCode {
    let port = args
        .iter()
        .position(|a| a == "--api")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u16>().ok());
    let Some(port) = port else {
        eprintln!("usage: panels engine --api <port> [--tls-identity-stdin]");
        return ExitCode::from(2);
    };
    let tls = if args.iter().any(|a| a == "--tls-identity-stdin") {
        rustet_engine::TlsSource::Stdin
    } else {
        rustet_engine::TlsSource::Generate
    };
    rustet_engine::prepare_process();
    let result = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(rustet_engine::run(rustet_engine::Options { port, tls })));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engine: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Starts this program again with `HOME` and the XDG directories inside the state directory.
fn restart_inside_state_dir(args: &[String]) -> ExitCode {
    let parsed = parse_args(args);
    let (dir, _temp) = match parsed.state_dir {
        Some(d) => (d, None),
        None => {
            let t = tempfile::tempdir().expect("a temporary directory");
            (t.path().to_owned(), Some(t))
        }
    };
    let sub = |p: &str| {
        let d = dir.join(p);
        std::fs::create_dir_all(&d).expect("state directory");
        d
    };
    let mut cmd = std::process::Command::new(std::env::current_exe().expect("own path"));
    cmd.args(args)
        .env(CHILD, "1")
        .env("HOME", sub("home"))
        .env("XDG_CONFIG_HOME", sub("config"))
        .env("XDG_CACHE_HOME", sub("cache"))
        .env("XDG_DATA_HOME", sub("data"))
        .env("XDG_STATE_HOME", sub("state"))
        .env("TMPDIR", sub("tmp"))
        .env("RUSTET_EXPORT_ROOTS", sub("home"))
        .env_remove("RUSTET_ENGINE_PORT");
    match cmd.status() {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => ExitCode::from(u8::try_from(s.code().unwrap_or(1)).unwrap_or(1)),
        Err(e) => {
            eprintln!("could not start: {e}");
            ExitCode::FAILURE
        }
    }
}

fn free_port() -> u16 {
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port());
        match port {
            Ok(p) if p != 20025 => return p,
            Ok(_) => {}
            Err(e) => panic!("no free port: {e}"),
        }
    }
}

/// The app, signed out, on the engine role of this program.
fn start_app(runtime: &tokio::runtime::Runtime, route: Route) -> (App, Arc<PrefsFile>) {
    let config_dir = rustet_core::appdir::config_dir().to_owned();
    let prefs = Arc::new(PrefsFile::new(&config_dir));
    let app = runtime.block_on(async {
        let store = Store::open(&config_dir, StoreCipher::Plain).await;
        let config = AppConfig {
            cache_dir: rustet_core::appdir::cache_dir().to_owned(),
            store,
            prefs: prefs.clone(),
            engine_port: free_port(),
            timing: SupervisorTiming::default(),
            drm_session_dir: config_dir.join("drm/files"),
            // No web sign-in window: a click on "Sign in" without FairPlay ends in an error.
            sign_in: SignInProgram {
                program: "false".into(),
                args: Vec::new(),
            },
            language: "en-US".into(),
            desktop: false,
            exe_dir: None,
            start: route,
            config_dir: config_dir.clone(),
        };
        App::start(config, Env::from_process())
            .await
            .expect("the app starts")
    });
    (app, prefs)
}

fn run_window(args: Args) -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (app, prefs) = start_app(&runtime, args.route.clone());
    report(
        "state_dir",
        json!(std::env::var("HOME").unwrap_or_default()),
    );
    let stop = app.clone();
    let scenario = args.scenario.clone();
    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut gpui::App| {
            // The theme as the shell builds it, unless the run asks for one appearance.
            let theme = match args.appearance {
                Some(a) => Theme::new(a),
                None => {
                    let render = ThemeService::new(prefs.clone()).and_then(|s| s.render());
                    match render {
                        Ok(r) => Theme::from_settings(&r, &prefs.load(), cx.window_appearance()),
                        Err(_) => Theme::new(Appearance::Dark),
                    }
                }
            };
            rustet_ui::init(theme, None, cx).expect("init");
            cx.bind_keys([
                KeyBinding::new("tab", NextFocus, None),
                KeyBinding::new("shift-tab", PreviousFocus, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);
            let bounds = Bounds::centered(None, size(px(1360.), px(860.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Rustet panels".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(480.), px(400.))),
                window_background: cx.theme().window_background(false),
                app_id: Some("rustet-panels".into()),
                ..Default::default()
            };
            let demo_app = app.clone();
            let window = cx
                .open_window(options, |window, cx| {
                    cx.new(|cx| Demo::new(demo_app, window, cx))
                })
                .expect("open window");
            let root = window
                .update(cx, |_, _, cx| cx.entity())
                .expect("the root view");
            install_host(root, cx);
            if let Some(name) = scenario.clone() {
                scenario::run(name, window, app.clone(), cx);
            }
        });
    runtime.block_on(stop.stop());
    ExitCode::SUCCESS
}

/// The window's handler: a modal and toasts drawn over the page; links are reported, never
/// opened. Requests come from inside the window's own event handling, so the root view is
/// updated as an entity (with the window it is given), never through its window handle.
fn install_host(demo: Entity<Demo>, cx: &mut gpui::App) {
    Host::install(
        move |request, w, cx| {
            demo.update(cx, |demo, cx| {
                match request {
                    HostRequest::Modal(view) => {
                        report("modal", json!(true));
                        demo.return_focus = w.focused(cx);
                        demo.modal = Some(view);
                    }
                    HostRequest::CloseModal => {
                        demo.modal = None;
                        if let Some(f) = demo.return_focus.take() {
                            w.focus(&f, cx);
                        }
                    }
                    HostRequest::Toast(t) => {
                        report(
                            "toast",
                            json!({ "message": t.message.to_string(), "failure": t.failure }),
                        );
                        demo.toasts.update(cx, |ts, cx| ts.push(t, cx));
                    }
                    HostRequest::OpenExternal(url) => report("open_external", json!(url)),
                    HostRequest::ShowNowPlaying => report("show_now_playing", json!(true)),
                    HostRequest::Menu { .. } => report("menu", json!(true)),
                }
                cx.notify();
            })
        },
        cx,
    );
}

pub struct Demo {
    pub app: App,
    pub modal: Option<AnyView>,
    pub return_focus: Option<FocusHandle>,
    pub toasts: Entity<Toasts>,
    pub popover: Popover,
    pub page: Option<AnyView>,
    focus: FocusHandle,
    _nav: gpui::Task<()>,
}

impl Demo {
    fn new(app: App, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut nav = app.nav().state();
        let task = cx.spawn(async move |this, cx| {
            while nav.changed().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            app,
            modal: None,
            return_focus: None,
            toasts: cx.new(|_| Toasts::default()),
            popover: Popover::default(),
            page: None,
            focus,
            _nav: task,
        }
    }

    fn rail_item(&self, label: &'static str, route: Route, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let current = std::mem::discriminant(&self.app.nav().state().borrow().route)
            == std::mem::discriminant(&route);
        let app = self.app.clone();
        div()
            .id(label)
            .role(Role::Link)
            .aria_label(label)
            .aria_selected(current)
            .tab_index(0)
            .focus_ring_on(theme.colors.sidebar, cx)
            .px(theme.space.s)
            .h(theme.size.control)
            .flex()
            .items_center()
            .rounded(theme.radius.medium)
            .cursor_pointer()
            .text_size(theme.text.body)
            .text_color(if current {
                theme.colors.text
            } else {
                theme.colors.text_secondary
            })
            .when(current, |d| d.bg(theme.colors.selected))
            .on_click(move |_, _, _| app.nav().go(route.clone()))
            .child(label)
            .into_any_element()
    }
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let route = self.app.nav().state().borrow().route.clone();
        let page = match route {
            Route::Downloads => rustet_ui_panels::downloads_page(&self.app, window, cx),
            _ => rustet_ui_panels::settings_page(&self.app, window, cx),
        };
        self.page = Some(page.clone());
        let rail = div()
            .flex()
            .flex_col()
            .gap(theme.space.xs)
            .w(px(170. * theme.zoom))
            .flex_none()
            .p(theme.space.m)
            .bg(theme.colors.sidebar)
            .child(self.rail_item("Settings", Route::Settings { section: None }, cx))
            .child(self.rail_item("Downloads", Route::Downloads, cx));
        let label: SharedString = "Rustet panels".into();
        div()
            .id("root")
            .aria_label(label)
            .track_focus(&self.focus)
            .on_action(|_: &NextFocus, window, cx| window.focus_next(cx))
            .on_action(|_: &PreviousFocus, window, cx| window.focus_prev(cx))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .relative()
            .size_full()
            .flex()
            .bg(theme.base())
            .font_family(rustet_ui::theme::FONT_FAMILY)
            .child(rail)
            .child(div().flex_1().min_w_0().h_full().child(page))
            .children(self.popover.render(cx))
            .children(self.modal.clone())
            .child(self.toasts.clone())
    }
}
