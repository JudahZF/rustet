//! The window: a tao event loop with one wry web view, polled for the token.

use std::io;
use std::panic;
use std::thread;
use std::time::{Duration, Instant};

use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Window, WindowBuilder};
use tracing::{debug, info, warn};
use wry::{
    NewWindowResponse, PageLoadEvent, PermissionResponse, WebContext, WebView, WebViewBuilder,
};

use crate::capture::{self, LocalStorage};
use crate::{CancelReason, Error, Options, SignInResult, TokenSource};

/// How often the cookies and `localStorage` are read.
const POLL: Duration = Duration::from_secs(1);

enum UserEvent {
    StdinClosed,
    Signal,
    LocalStorage(String),
    PageLoaded(String),
}

pub(crate) fn sign_in(options: &Options) -> Result<SignInResult, Error> {
    // tao panics instead of returning an error when GTK cannot open a display.
    let mut event_loop =
        panic::catch_unwind(|| EventLoopBuilder::<UserEvent>::with_user_event().build()).map_err(
            |panic| {
                let message = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "no display".to_owned());
                Error::Display(message)
            },
        )?;
    let proxy = event_loop.create_proxy();
    watch_stdin(proxy.clone());
    #[cfg(unix)]
    watch_signals(proxy.clone())?;

    let window = WindowBuilder::new()
        .with_title("Sign in to Apple Music")
        .with_inner_size(LogicalSize::new(560.0, 760.0))
        .build(&event_loop)?;
    let mut context = WebContext::new(Some(options.profile_dir.clone()));
    let storefront = options
        .storefront
        .as_deref()
        .and_then(capture::storefront)
        .unwrap_or_else(|| "us".to_owned());
    let start = format!("{}/{storefront}/login", capture::MUSIC_ORIGIN);
    let webview = build_webview(&mut context, &window, &start, proxy.clone())?;
    info!(url = %start, profile = %options.profile_dir.display(), "sign-in window open");

    Ok(run_loop(&mut event_loop, &webview, &proxy))
}

fn build_webview(
    context: &mut WebContext,
    window: &Window,
    start: &str,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<WebView, wry::Error> {
    // WebKit's own user agent: Apple's sign-in accepts it, no Safari disguise needed.
    let builder = WebViewBuilder::new_with_web_context(context)
        .with_navigation_handler(|url| {
            let allowed = capture::allow_navigation(&url);
            if allowed {
                debug!(url = %capture::loggable(&url), "navigation");
            } else {
                info!(url = %capture::loggable(&url), "navigation blocked");
            }
            allowed
        })
        .with_new_window_req_handler(|url, _| {
            info!(url = %capture::loggable(&url), "pop-up blocked");
            NewWindowResponse::Deny
        })
        .with_download_started_handler(|url, _| {
            info!(url = %capture::loggable(&url), "download blocked");
            false
        })
        .with_permission_handler(|kind| {
            info!(?kind, "permission denied");
            PermissionResponse::Deny
        })
        .with_on_page_load_handler(move |event, url| {
            if let PageLoadEvent::Finished = event {
                let _ = proxy.send_event(UserEvent::PageLoaded(url));
            }
        });

    #[cfg(target_os = "linux")]
    {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        // tao gives every window a GTK box; the web view goes into it (works on X11 and Wayland).
        let vbox = window
            .default_vbox()
            .expect("tao creates a default vbox for every window");
        let webview = builder.build_gtk(vbox)?;
        // Software rendering. With WebKit's GPU (DMA-BUF) path the cross-origin Apple ID frame
        // stayed blank where no GPU renders (headless wlroots / Xvfb), while the page around it
        // drew; the same class of bug hits some GPU drivers. One small form does not need the GPU.
        // Set before the first load, so wry is built without a URL.
        {
            use webkit2gtk::{HardwareAccelerationPolicy, SettingsExt, WebViewExt};
            use wry::WebViewExtUnix;
            if let Some(settings) = webview.webview().settings() {
                settings.set_hardware_acceleration_policy(HardwareAccelerationPolicy::Never);
            }
        }
        webview.load_url(start)?;
        Ok(webview)
    }
    #[cfg(not(target_os = "linux"))]
    builder.with_url(start).build(window)
}

fn run_loop(
    event_loop: &mut EventLoop<UserEvent>,
    webview: &WebView,
    proxy: &EventLoopProxy<UserEvent>,
) -> SignInResult {
    let mut result = None;
    let mut cookie_log = CookieLog::default();
    let mut storage_keys_seen = 0;
    event_loop.run_return(|event, _, control_flow| {
        match event {
            Event::NewEvents(StartCause::Init | StartCause::ResumeTimeReached { .. }) => {
                *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
                result = read_cookies(webview, &mut cookie_log);
                if result.is_none() {
                    let proxy = proxy.clone();
                    let evaluated = webview.evaluate_script_with_callback(
                        capture::LOCAL_STORAGE_SCRIPT,
                        move |json| {
                            let _ = proxy.send_event(UserEvent::LocalStorage(json));
                        },
                    );
                    if let Err(error) = evaluated {
                        debug!(%error, "localStorage read failed");
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                result = Some(SignInResult::Cancelled {
                    reason: CancelReason::WindowClosed,
                })
            }
            Event::UserEvent(UserEvent::StdinClosed) => {
                result = Some(SignInResult::Cancelled {
                    reason: CancelReason::StdinClosed,
                });
            }
            Event::UserEvent(UserEvent::Signal) => {
                result = Some(SignInResult::Cancelled {
                    reason: CancelReason::Signal,
                })
            }
            Event::UserEvent(UserEvent::PageLoaded(url)) => {
                info!(url = %capture::loggable(&url), "page loaded");
            }
            Event::UserEvent(UserEvent::LocalStorage(json)) => {
                let storage = LocalStorage::parse(&json);
                if storage.keys.len() != storage_keys_seen {
                    storage_keys_seen = storage.keys.len();
                    debug!(keys = ?storage.keys, "music.apple.com localStorage keys");
                }
                result = storage.token().map(|token| SignInResult::Token {
                    music_user_token: token,
                    // MusicKit keeps the storefront elsewhere; the cookie hint is read again.
                    storefront: webview
                        .cookies_for_url(capture::MUSIC_ORIGIN)
                        .ok()
                        .and_then(|cookies| {
                            capture::from_cookies(cookies.iter().map(|c| (c.name(), c.value()))).1
                        }),
                    source: TokenSource::LocalStorage,
                });
            }
            _ => {}
        }
        if result.is_some() {
            *control_flow = ControlFlow::Exit;
        }
    });
    result.unwrap_or(SignInResult::Cancelled {
        reason: CancelReason::WindowClosed,
    })
}

/// What the cookie poll has logged, so each fact is logged once and not every second.
#[derive(Default)]
struct CookieLog {
    error_logged: bool,
    names: Vec<String>,
}

/// Reads the `music.apple.com` cookies: a result when the token is there.
fn read_cookies(webview: &WebView, log: &mut CookieLog) -> Option<SignInResult> {
    let cookies = match webview.cookies_for_url(capture::MUSIC_ORIGIN) {
        Ok(cookies) => cookies,
        Err(error) => {
            if !std::mem::replace(&mut log.error_logged, true) {
                warn!(%error, "cookies unreadable, relying on localStorage");
            }
            return None;
        }
    };
    // Names only (with the HttpOnly flag): values are session secrets.
    let names: Vec<String> = cookies
        .iter()
        .map(|c| match c.http_only() {
            Some(true) => format!("{} (HttpOnly)", c.name()),
            _ => c.name().to_owned(),
        })
        .collect();
    if names != log.names {
        debug!(cookies = ?names, "music.apple.com cookies");
        log.names = names;
    }
    let (token, storefront) = capture::from_cookies(cookies.iter().map(|c| (c.name(), c.value())));
    token.map(|music_user_token| SignInResult::Token {
        music_user_token,
        storefront,
        source: TokenSource::Cookie,
    })
}

/// End of file on stdin means the parent is gone or wants the window closed.
fn watch_stdin(proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
        let _ = proxy.send_event(UserEvent::StdinClosed);
    });
}

#[cfg(unix)]
fn watch_signals(proxy: EventLoopProxy<UserEvent>) -> Result<(), Error> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals =
        signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]).map_err(Error::Signals)?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = proxy.send_event(UserEvent::Signal);
        }
    });
    Ok(())
}
