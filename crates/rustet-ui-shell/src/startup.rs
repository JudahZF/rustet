//! Start-up and shut-down of the UI role, in the web shell's order: runtime, config
//! directory (with the migration of older layouts), one instance per user, housekeeping, prefs,
//! theme and store, the app, then gpui with the kit, the artwork store and the main window.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use gpui::{AppContext, QuitMode};
use rustet_app::route::Route;
use rustet_app::{App as RustetApp, AppConfig};
use rustet_desktop::cleanup;
use rustet_desktop::platform::{Env, Platform, config_root};
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::{LinuxBackend, OsKeyring, Store, StoreCipher};
use rustet_desktop::theme::ThemeService;
use rustet_ui::artwork::{ArtworkStore, DiskCache, StoreConfig};
use rustet_ui::icons::Assets;
use rustet_ui::theme::Theme;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::shell::{Control, Shell, ShellConfig, install_host};
use crate::{Args, Views};

/// Logs to stderr: info by default; `RUSTET_LOG` (an `EnvFilter`) overrides it. gpui's `log`
/// records come too: at start-up it names the GPU adapters it found and the one it chose, with
/// its backend (Vulkan or OpenGL), and it warns when one fails. wgpu's own internals stay at
/// warnings.
fn init_logging(env: &Env) {
    use tracing_subscriber::EnvFilter;
    let filter = env
        .get("RUSTET_LOG")
        .and_then(|f| EnvFilter::try_new(f).ok())
        .unwrap_or_else(|| EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
}

fn home_dir(env: &Env) -> PathBuf {
    env.get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// The per-user cache root (`app.getPath('cache')`).
fn cache_root(env: &Env, home: &Path) -> PathBuf {
    if cfg!(windows) {
        return env
            .get("LOCALAPPDATA")
            .map_or_else(|| home.join("AppData/Local"), PathBuf::from);
    }
    env.get("XDG_CACHE_HOME")
        .map_or_else(|| home.join(".cache"), PathBuf::from)
}

/// Housekeeping before the caches open: at most once a day, never fatal.
fn housekeeping(config_dir: &Path, cache_dir: &Path) {
    let Some(report) =
        cleanup::run_cleanup_throttled(config_dir, SystemTime::now(), cleanup::CLEANUP_INTERVAL)
    else {
        return;
    };
    let cache = cleanup::cleanup_cache(cache_dir, false);
    let (removed, bytes) = (report.removed + cache.removed, report.bytes + cache.bytes);
    if removed > 0 {
        info!(
            "cleanup freed {:.1} MB ({removed} items)",
            bytes as f64 / 1_048_576.0
        );
    }
}

/// Ctrl+C, SIGTERM and SIGHUP quit like the tray's Quit.
#[cfg(unix)]
fn quit_on_signals(control: mpsc::UnboundedSender<Control>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return warn!("cannot install the signal handlers");
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
        info!("signal: quitting");
        let _ = control.send(Control::Quit);
    });
}

/// A new Rustet with the same arguments, after this one let go of its lock.
fn relaunch(args: &[String]) {
    match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(args).spawn())
    {
        Ok(child) => info!("restarted as pid {}", child.id()),
        Err(e) => error!("restart failed: {e}"),
    }
}

/// Runs the UI role until Quit. `views` draw the page region.
pub fn run(args: Args, views: Views) -> ExitCode {
    let env = Env::from_process();
    init_logging(&env);
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .thread_name("rustet-rt")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _entered = rt.enter();
    let home = home_dir(&env);
    let config_dir = config_root(Platform::current(), &env, &home).join("rustet");
    let (control_tx, control_rx) = mpsc::unbounded_channel();

    // One Rustet per user: a second `rustet <link>` hands its arguments to this one and exits.
    #[cfg(unix)]
    let primary = {
        use rustet_desktop::links::single_instance::{self, Instance};
        let acquired = single_instance::runtime_dir(&env)
            .and_then(|dir| rt.block_on(single_instance::acquire(&dir, &args.raw)));
        match acquired {
            Ok(Instance::Forwarded) => {
                info!("Rustet is already running; handed the arguments to it");
                return ExitCode::SUCCESS;
            }
            Ok(Instance::Primary(mut p)) => {
                let mut forwarded = std::mem::replace(&mut p.args, mpsc::unbounded_channel().1);
                let tx = control_tx.clone();
                rt.spawn(async move {
                    while let Some(args) = forwarded.recv().await {
                        if tx.send(Control::Args(args)).is_err() {
                            return;
                        }
                    }
                });
                Some(p)
            }
            Err(e) => {
                warn!("single-instance lock unavailable, continuing without it: {e}");
                None
            }
        }
    };
    #[cfg(unix)]
    quit_on_signals(control_tx.clone());

    housekeeping(&config_dir, &cache_root(&env, &home).join("rustet"));
    let prefs = Arc::new(PrefsFile::new(&config_dir));
    let theme = match ThemeService::new(prefs.clone()) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            error!(
                "cannot write the preferences in {}: {e}",
                config_dir.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let store = rt.block_on(async {
        let keyring = OsKeyring {
            backend: LinuxBackend::from_desktop(&env),
        };
        Store::open(&config_dir, StoreCipher::from_keyring(&keyring).await).await
    });
    let mut config = match AppConfig::installed(&config_dir, store, prefs.clone(), &env) {
        Ok(c) => c,
        Err(e) => {
            error!("cannot find the rustet executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    config.desktop = !args.no_desktop;
    if let Some(link) = &args.link {
        match Route::from_link(link) {
            Some(route) => config.start = route,
            None => warn!("no page for the link {link}"),
        }
    }
    let cache_dir = config.cache_dir.clone();
    let app = match rt.block_on(RustetApp::start(config, env.clone())) {
        Ok(app) => app,
        Err(e) => {
            error!("Rustet cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let http = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            error!("cannot build the HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let restart = Arc::new(AtomicBool::new(false));
    let shell_config = ShellConfig {
        prefs: prefs.clone(),
        theme: theme.clone(),
        http: http.clone(),
        cache_dir: cache_dir.join("ui"),
        // gpui talks Wayland when it can; only X11 tells a window where it is.
        position_known: cfg!(windows) || env.get("WAYLAND_DISPLAY").is_none(),
        restart: restart.clone(),
    };
    let handle = rt.handle().clone();
    let ui_app = app.clone();
    let initial = theme.render();

    gpui_platform::application()
        .with_assets(Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut gpui::App| {
            // The desktop's light or dark is known once gpui runs.
            let initial = initial
                .map(|r| Theme::from_settings(&r, &prefs.load(), cx.window_appearance()))
                .unwrap_or_else(|e| {
                    warn!("theme: {e}");
                    Theme::new(rustet_ui::theme::Appearance::Dark)
                });
            // The app's runtime serves the views too.
            if let Err(e) = rustet_ui::init(initial, Some(handle), cx) {
                warn!("fonts: {e}");
            }
            crate::shell::bind_keys(cx);
            let disk = DiskCache::open(shell_config.cache_dir.join("artwork"), 256 << 20)
                .map_err(|e| warn!("artwork cache: {e}"))
                .ok()
                .map(Arc::new);
            ArtworkStore::install(http, disk, StoreConfig::default(), cx);

            let shell = cx.new(|cx| Shell::new(ui_app, views, shell_config, Some(control_rx), cx));
            install_host(shell.clone(), cx);
            if !args.hidden {
                shell.update(cx, |s, cx| s.show_main(cx));
            }
            #[cfg(feature = "scenario")]
            if let Some(name) = args.scenario.clone() {
                crate::scenario::run(name, shell, cx);
            }
        });

    // The event loop has ended; whatever Quit did not finish, finish now.
    rt.block_on(app.stop());
    #[cfg(unix)]
    drop(primary); // releases the lock before a restarted Rustet asks for it
    if restart.load(Ordering::SeqCst) {
        relaunch(&std::env::args().skip(1).collect::<Vec<_>>());
    }
    rt.shutdown_timeout(Duration::from_secs(1));
    info!("exited");
    ExitCode::SUCCESS
}
