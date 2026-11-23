//! Scripted runs of the real window (`--scenario NAME`), for the headless compositor of
//! `crates/rustet-ui/scripts/headless.sh`. Development only. Screenshots go to
//! `RUSTET_UI_SHOT_DIR`, trimmed to the window; `REPORT {json}` lines go to standard output.
//!
//! - `tour`: the window at three widths, the player pill and the sign-in in every state
//!   reachable without an account (real), then the states that need one, from built
//!   [`PlayerState`] and [`SheetView`] values (marked `built` in the report): playing at four
//!   widths, paused, resume, failed, the volume slider, the queue and lyrics panels, Now
//!   Playing, the mini player; the same in the light appearance; the mini player over an
//!   album's artwork colour; the offline banner after the engine child is stopped; the search
//!   key opening the Search page with its field; hide and show.
//! - `pages`: every kind of page in the real window, signed out (New, Radio, search results,
//!   an album, an artist, a playlist, Charts, the library's sign-in state, every Settings
//!   section, Downloads), then frame times while an album grid and a long track list scroll
//!   (one wheel event per frame).
//! - `video`: a music video from a frame ring the scenario writes (a moving test pattern with a
//!   frame counter), in Now Playing: 1080p and 720p, windowed and full screen, with frame rate,
//!   CPU and memory measured; a size change, a pause, an ended ring, a new ring, audio only,
//!   and a two-minute run for memory growth.
//! - `startup`: the time from the process start to the first frame (every scenario reports
//!   it), then the CPU and memory of the idle window over 30 s.
//! - `a11y`: holds the window in states for the AT-SPI dump (`scripts/a11y-shell.sh`).
//! - `idle`: opens the window and waits (manual checks).

use std::path::PathBuf;
use std::process::Command as Process;
use std::time::Duration;

use gpui::{App, AppContext, AsyncApp, Entity, Focusable, px, size};
use rustet_app::account::lossless::{CodeWait, SheetError, SheetView};
use rustet_app::account::{AccountState, SigningIn};
use rustet_app::engine::Connection;
use rustet_app::pages::model::{Listing, TrackRow};
use rustet_app::pages::{self, Page};
use rustet_app::route::Route;
use rustet_player::model::{
    CatalogId, Codec, Command, HistoryEntry, ItemKind, ItemSource, PlayerState, Quality,
    QualityTier, QueueItem, QueueView, SetQueue, Status, TrackMeta, Volume,
};
use rustet_ui::theme::{ActiveTheme, Appearance};
use serde_json::json;

use crate::shell::Shell;

mod video;

/// Prints one machine-readable report line.
pub fn report(kind: &str, value: serde_json::Value) {
    println!("REPORT {}", json!({ "kind": kind, "value": value }));
}

/// Starts scenario `name` on the shell.
pub fn run(name: String, shell: Entity<Shell>, cx: &mut App) {
    if let Some(main) = shell.read(cx).main {
        main.update(cx, |_, window, _| {
            window.on_next_frame(|_, _| {
                report(
                    "startup.first_frame_ms",
                    json!(video::since_process_start_ms()),
                )
            })
        })
        .ok();
    }
    cx.spawn(async move |cx| {
        let s = Script { shell, cx };
        // The first page loads from Apple, the engine answers its first snapshot.
        s.wait(4000).await;
        match name.as_str() {
            "tour" => tour(&s).await,
            "pages" => pages(&s).await,
            "video" => video::run(&s).await,
            "a11y" => a11y(&s).await,
            "startup" => video::idle_cost(&s).await,
            "idle" => {
                report("idle.ready", json!(true));
                return;
            }
            other => report("error", json!(format!("unknown scenario {other}"))),
        }
        s.update(|sh, cx| sh.quit(cx));
    })
    .detach();
}

struct Script<'a> {
    shell: Entity<Shell>,
    cx: &'a AsyncApp,
}

impl Script<'_> {
    async fn wait(&self, ms: u64) {
        self.cx
            .background_executor()
            .timer(Duration::from_millis(ms))
            .await;
    }

    fn update<R>(&self, f: impl FnOnce(&mut Shell, &mut gpui::Context<Shell>) -> R) -> R {
        self.shell.update(&mut self.cx.clone(), f)
    }

    /// Sets the main window's content size.
    async fn resize(&self, width: f32, height: f32) {
        let main = self.update(|s, _| s.main);
        if let Some(main) = main {
            let mut cx = self.cx.clone();
            cx.update_window(main.into(), |_, window, _| {
                window.resize(size(px(width), px(height)))
            })
            .ok();
        }
        self.wait(900).await;
    }

    /// A screenshot of the screen, trimmed to the windows (the server's background is magenta).
    async fn shot(&self, name: &str) {
        self.wait(400).await;
        let dir =
            PathBuf::from(std::env::var_os("RUSTET_UI_SHOT_DIR").unwrap_or_else(|| ".".into()));
        let path = dir.join(format!("{name}.png"));
        let ok = self
            .cx
            .background_executor()
            .spawn(async move {
                let captured = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                    Process::new("grim").arg(&path).status()
                } else {
                    // The main window without the window manager's frame (or the screen,
                    // when it is hidden).
                    let id = Process::new("xdotool")
                        .args(["search", "--name", "^Rustet$"])
                        .output()
                        .ok()
                        .and_then(|o| {
                            String::from_utf8(o.stdout)
                                .ok()?
                                .lines()
                                .next()
                                .map(str::to_owned)
                        });
                    Process::new("import")
                        .args(["-window", id.as_deref().unwrap_or("root")])
                        .arg(&path)
                        .status()
                };
                captured.is_ok_and(|s| s.success())
                    && Process::new("magick")
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

    /// Moves the pointer in the main window to where `at` says.
    fn pointer(
        &self,
        at: impl FnOnce(&rustet_ui::theme::Theme, &gpui::Window) -> gpui::Point<gpui::Pixels>,
    ) {
        let Some(main) = self.update(|sh, _| sh.main) else {
            return;
        };
        let mut cx = self.cx.clone();
        cx.update_window(main.into(), |_, window, cx| {
            let position = at(cx.theme(), window);
            window.dispatch_event(
                gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers: gpui::Modifiers::default(),
                }),
                cx,
            );
        })
        .ok();
    }

    /// Shows a built account state, as the model would publish it.
    fn account(&self, state: AccountState) {
        self.update(|s, cx| {
            if let AccountState::SigningIn(SigningIn::Sheet(view)) = &state {
                let view = view.clone();
                s.sign_in.update(cx, |sheet, cx| sheet.set_view(view, cx));
            }
            s.account = state;
            cx.notify();
        });
    }

    fn player(&self, state: PlayerState) {
        self.update(|s, cx| s.show_player(Some(state), cx));
    }
}

/// Real songs for the built states: the first song results for a public search (signed out
/// works), so the artwork is real. Built ones without artwork when Apple does not answer.
async fn songs(s: &Script<'_>) -> Vec<QueueItem> {
    let api = s.update(|sh, _| sh.app.api().clone());
    let job = s.update(|_, cx| {
        rustet_ui::runtime::spawn(cx, async move {
            pages::load(
                &api,
                &Route::Search {
                    term: "Phoebe Bridgers".into(),
                },
            )
            .await
        })
    });
    let rows: Vec<TrackRow> = match job.await {
        Ok(Ok(Page::Search(p))) => p
            .sections
            .iter()
            .find_map(|sec| match &sec.listing {
                Listing::Tracks(l) => Some(l.items.iter().take(8).cloned().collect()),
                Listing::Tiles(_) => None,
            })
            .unwrap_or_default(),
        other => {
            report(
                "songs",
                json!(format!("search failed: {:?}", other.map(|r| r.map(|_| ())))),
            );
            Vec::new()
        }
    };
    let mut items: Vec<QueueItem> = rows
        .iter()
        .filter_map(|r| rustet_app::pages::play::queue_item(r, None))
        .enumerate()
        .map(|(i, q)| QueueItem {
            uid: i as u64 + 1,
            id: q.id,
            library_id: q.library_id,
            kind: q.kind,
            source: ItemSource::User,
            meta: q.meta,
            container_id: q.container_id,
        })
        .collect();
    if items.is_empty() {
        items = [
            "Motion Sickness",
            "Kyoto",
            "Garden Song",
            "Funeral",
            "Moon Song",
            "I Know the End",
        ]
        .iter()
        .enumerate()
        .map(|(i, name)| QueueItem {
            uid: i as u64 + 1,
            id: CatalogId(format!("10{i}")),
            library_id: None,
            kind: ItemKind::Song,
            source: ItemSource::User,
            meta: TrackMeta {
                name: (*name).into(),
                artist_name: "Phoebe Bridgers".into(),
                album_name: "Punisher".into(),
                artwork_url: String::new(),
                duration_ms: 229_000,
                audio_traits: vec![],
                content_rating: None,
            },
            container_id: None,
        })
        .collect();
    }
    report("songs", json!(items.len()));
    items
}

fn playing(items: &[QueueItem], status: Status) -> PlayerState {
    let now = rustet_app::player::unix_ms();
    PlayerState {
        generation: 3,
        status,
        track: items.first().cloned(),
        position_ms: 74_000,
        position_at: now,
        duration_ms: items.first().map_or(229_000, |t| t.meta.duration_ms.max(1)),
        quality: Some(Quality {
            codec: Codec::Alac,
            tier: QualityTier::HiResLossless,
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            bitrate: None,
            channels: Some(2),
            spatial: None,
            video_height: None,
            fell_back: false,
        }),
        volume: 70,
        shuffle: true,
        queue: QueueView {
            items: items.to_vec(),
            current_index: Some(0),
        },
        can_previous: true,
        can_next: items.len() > 1,
        ..PlayerState::default()
    }
}

fn sign_in(view: SheetView) -> AccountState {
    AccountState::SigningIn(SigningIn::Sheet(view))
}

async fn tour(s: &Script<'_>) {
    let real_account = s.update(|sh, _| sh.account.clone());
    report("account", json!(format!("{real_account:?}")));
    report(
        "connection",
        json!(format!("{:?}", s.update(|sh, _| sh.connection.clone()))),
    );

    // The window at three widths: labelled rail, icon rail, no rail.
    for (name, w) in [
        ("width-1360", 1360.),
        ("width-760", 760.),
        ("width-560", 560.),
    ] {
        s.resize(w, 860.).await;
        s.shot(name).await;
    }
    s.resize(1360., 860.).await;

    // Real: the deck before anything plays, then a queue without an account.
    s.shot("deck-idle").await;
    let items = songs(s).await;
    s.update(|sh, cx| {
        // Silent first: the headless session has no sound server, and the volume is down too.
        sh.send(
            Command::Volume(Volume {
                level: 0,
                muted: Some(true),
            }),
            cx,
        );
    });
    s.wait(300).await;
    s.update(|sh, cx| {
        let queue = items
            .iter()
            .map(|q| rustet_player::model::QueueItemIn {
                id: q.id.clone(),
                library_id: None,
                kind: q.kind,
                meta: q.meta.clone(),
                container_id: None,
            })
            .collect();
        sh.send(
            Command::SetQueue(SetQueue {
                items: queue,
                start_index: 0,
                context: None,
                shuffle: None,
                start_ms: None,
                play: true,
            }),
            cx,
        );
    });
    for i in 0..12 {
        s.wait(500).await;
        let status = s.update(|sh, _| sh.player.as_ref().map(|p| p.status));
        report(
            "deck.real",
            json!({ "t": i, "status": format!("{status:?}") }),
        );
        if i == 1 {
            s.shot("deck-real-opening").await;
        }
        if status == Some(Status::Failed) {
            break;
        }
    }
    s.shot("deck-real-after-queue").await;
    s.update(|sh, cx| sh.send(Command::Stop, cx));

    // Real: the sign-in as this engine offers it (the web window without FairPlay here).
    let backend = s.update(|sh, _| sh.backend);
    report("backend", json!(format!("{backend:?}")));
    if backend == Some(rustet_app::account::Backend::FairPlay) {
        s.update(|sh, cx| sh.sign_in(cx));
        s.wait(1500).await;
        s.shot("signin-real-open").await;
        s.update(|sh, cx| {
            let account = sh.app.account().clone();
            let job =
                rustet_ui::runtime::spawn(cx, async move { account.lossless().cancel().await });
            cx.background_spawn(async move { drop(job.await) }).detach();
        });
        s.wait(800).await;
    }

    // Built: every page of the sign-in sheet.
    let error = |m: &str, url: Option<&str>| {
        Some(SheetError {
            message: m.into(),
            help_url: url.map(Into::into),
        })
    };
    let sheets = [
        (
            "signin-form",
            SheetView::SignIn {
                email: String::new(),
                known_account: false,
                busy: false,
                error: None,
            },
        ),
        (
            "signin-known",
            SheetView::SignIn {
                email: "listener@example.com".into(),
                known_account: true,
                busy: false,
                error: None,
            },
        ),
        (
            "signin-busy",
            SheetView::SignIn {
                email: "listener@example.com".into(),
                known_account: false,
                busy: true,
                error: None,
            },
        ),
        (
            "signin-error",
            SheetView::SignIn {
                email: "listener@example.com".into(),
                known_account: false,
                busy: false,
                error: error(
                    "This Apple Account is locked for security reasons. Unlock it at iforgot.apple.com.",
                    Some("https://iforgot.apple.com"),
                ),
            },
        ),
        (
            "signin-code",
            SheetView::Code {
                busy: None,
                error: None,
                wrong_code: false,
                phone_options: false,
            },
        ),
        (
            "signin-code-verifying",
            SheetView::Code {
                busy: Some(CodeWait::Verifying),
                error: None,
                wrong_code: false,
                phone_options: false,
            },
        ),
        (
            "signin-code-wrong",
            SheetView::Code {
                busy: None,
                error: error(
                    "Incorrect code. Enter the new code sent to your devices.",
                    None,
                ),
                wrong_code: true,
                phone_options: false,
            },
        ),
        (
            "signin-code-phone",
            SheetView::Code {
                busy: None,
                error: None,
                wrong_code: false,
                phone_options: true,
            },
        ),
        (
            "signin-choice",
            SheetView::Choice {
                title: "Recovery Options".into(),
                options: vec!["Text code to •••• 42".into(), "Call •••• 42".into()],
                busy: false,
            },
        ),
    ];
    for (name, view) in sheets {
        s.account(sign_in(view));
        s.wait(500).await;
        s.shot(&format!("built-{name}")).await;
    }
    s.account(AccountState::SigningIn(SigningIn::WebWindow));
    s.shot("built-signin-web-window").await;
    s.account(AccountState::Error {
        message: "Apple Music no longer accepts your sign-in. Sign in again.".into(),
    });
    s.shot("built-account-error").await;
    s.account(AccountState::Starting);
    s.shot("built-account-starting").await;
    s.account(AccountState::SignedIn {
        storefront: "gb".into(),
    });
    s.shot("built-account-signed-in").await;
    s.account(real_account.clone());

    // Built: the deck in every state that needs an account.
    for (name, status, restored) in [
        ("deck-opening", Status::Opening, false),
        ("deck-loading", Status::Loading, false),
        ("deck-playing", Status::Playing, false),
        ("deck-paused", Status::Paused, false),
        ("deck-resume", Status::Paused, true),
    ] {
        let mut state = playing(&items, status);
        state.restored = restored;
        s.player(state);
        s.shot(&format!("built-{name}")).await;
    }
    let mut failed = playing(&items, Status::Failed);
    failed.quality = None;
    failed.error = Some(rustet_player::model::PlayerError {
        code: rustet_player::model::ErrorCode::OpenFailed,
        message:
            "Lossless isn't available for this track. Playing AAC 256 kbps failed too. Try again."
                .into(),
    });
    s.player(failed);
    s.shot("built-deck-failed").await;

    // Built: what the pill keeps as the window narrows (`deck::Room`), and the volume slider.
    s.player(playing(&items, Status::Playing));
    for (name, w) in [("1000", 1000.), ("760", 760.), ("560", 560.)] {
        s.resize(w, 860.).await;
        s.shot(&format!("built-deck-playing-{name}")).await;
    }
    s.resize(1360., 860.).await;
    // The pointer on the LCD: the times and the stream's format.
    s.pointer(|theme, window| {
        let inset = rustet_ui::frame::client_inset(window);
        let size = window.viewport_size();
        let left = inset + theme.size.sidebar;
        gpui::point(
            left + (size.width - inset - left) / 2.,
            size.height - inset - theme.size.player_bottom - theme.size.player_height / 2.,
        )
    });
    s.shot("built-deck-hover").await;
    s.pointer(|_, window| gpui::point(window.viewport_size().width / 2., px(200.)));
    s.update(|sh, cx| {
        sh.volume_hover = true;
        cx.notify();
    });
    s.shot("built-deck-volume").await;
    s.update(|sh, cx| {
        sh.volume_hover = false;
        cx.notify();
    });

    // Built: Up Next with its queue and history, Now Playing, the mini player.
    let mut state = playing(&items, Status::Playing);
    state.repeat = rustet_player::model::Repeat::One;
    s.player(state);
    s.update(|sh, cx| {
        sh.history = items
            .iter()
            .rev()
            .map(|q| HistoryEntry {
                id: q.id.clone(),
                kind: q.kind,
                meta: q.meta.clone(),
                played_at: 0,
            })
            .collect();
        sh.panel = Some(crate::shell::Panel::Queue);
        cx.notify();
    });
    s.shot("built-drawer-queue").await;
    s.update(|sh, cx| {
        sh.history_tab = true;
        cx.notify();
    });
    s.shot("built-drawer-history").await;
    s.update(|sh, cx| {
        sh.history_tab = false;
        sh.toggle_panel(crate::shell::Panel::Lyrics, cx);
    });
    s.wait(2000).await; // the lyrics load (signed out: they ask for the sign-in)
    s.shot("built-panel-lyrics").await;
    s.update(|sh, cx| {
        sh.panel = None;
        sh.now_playing_open = true;
        cx.notify();
    });
    s.wait(2500).await; // lyrics (real: signed out asks for the sign-in) and the backdrop
    s.shot("built-now-playing").await;
    s.resize(760., 860.).await;
    s.shot("built-now-playing-narrow").await;
    s.resize(1360., 860.).await;
    s.update(|sh, cx| {
        sh.now_playing_open = false;
        sh.toggle_mini(cx);
    });
    s.wait(1000).await;
    s.shot("built-mini-player").await;
    s.update(|sh, cx| sh.toggle_mini(cx));

    // Built: the light appearance of the same states.
    let appearance = |a: Appearance| s.update(|_, cx| cx.theme().with_appearance(a).apply(cx));
    appearance(Appearance::Light);
    s.update(|sh, cx| {
        sh.panel = Some(crate::shell::Panel::Queue);
        cx.notify();
    });
    s.shot("light-deck-queue").await;
    s.update(|sh, cx| sh.toggle_panel(crate::shell::Panel::Lyrics, cx));
    s.shot("light-panel-lyrics").await;
    s.update(|sh, cx| {
        sh.panel = None;
        sh.now_playing_open = true;
        cx.notify();
    });
    s.shot("light-now-playing").await;
    s.update(|sh, cx| {
        sh.now_playing_open = false;
        cx.notify();
    });
    s.account(sign_in(SheetView::SignIn {
        email: String::new(),
        known_account: false,
        busy: false,
        error: None,
    }));
    s.shot("light-signin-form").await;
    s.account(real_account.clone());
    s.update(|sh, cx| sh.toggle_mini(cx));
    s.wait(1000).await;
    s.shot("light-mini-player").await;
    s.update(|sh, cx| sh.toggle_mini(cx));
    appearance(Appearance::Dark);

    // Real: an album page takes its artwork's colour; the mini player stays neutral.
    s.update(|sh, cx| {
        sh.go(
            Route::Album {
                id: "1440833098".into(),
            },
            cx,
        )
    });
    let state = settled(s, 12_000).await;
    report("tinted_page", json!(state));
    s.wait(1500).await;
    s.update(|sh, cx| sh.toggle_mini(cx));
    s.wait(1000).await;
    s.shot("built-mini-on-tinted-page").await;
    s.update(|sh, cx| {
        sh.toggle_mini(cx);
        sh.go(Route::New, cx);
    });

    // Real: the engine child stopped from outside: the offline banner, then the restart.
    s.update(|sh, cx| sh.show_player(None, cx));
    let port = std::env::var("RUSTET_ENGINE_PORT").unwrap_or_else(|_| "20025".into());
    let killed = Process::new("pkill")
        .args(["-f", &format!("engine --api {port}")])
        .status()
        .is_ok_and(|s| s.success());
    report("engine.killed", json!(killed));
    for i in 0..10 {
        s.wait(400).await;
        let c = s.update(|sh, _| sh.connection.clone());
        if matches!(c, Connection::Restarting { .. }) {
            report(
                "engine.offline",
                json!({ "t": i, "state": format!("{c:?}") }),
            );
            s.shot("offline-restarting").await;
            break;
        }
    }
    s.update(|sh, cx| {
        sh.connection = Connection::Failed {
            reason: "Port 20125 is in use by another program. Close it, then try again.".into(),
        };
        cx.notify();
    });
    s.shot("built-offline-failed").await;
    for _ in 0..30 {
        s.wait(500).await;
        let c = s.update(|sh, cx| {
            sh.connection = sh.app.connection().borrow().clone();
            cx.notify();
            sh.connection.clone()
        });
        if c == Connection::Ready {
            report("engine.back", json!(true));
            break;
        }
    }
    s.shot("engine-back").await;

    // Real: keys. `/` focuses the search field; Alt+Left goes back.
    let main = s.update(|sh, _| sh.main);
    let key = |stroke: &'static str| {
        let mut cx = s.cx.clone();
        if let Some(main) = main {
            cx.update_window(main.into(), |_, window, cx| {
                window.dispatch_keystroke(gpui::Keystroke::parse(stroke).expect("keystroke"), cx)
            })
            .ok();
        }
    };
    key("/");
    s.wait(300).await;
    let focused = {
        let mut cx = s.cx.clone();
        main.and_then(|m| {
            cx.update_window(m.into(), |_, window, cx| {
                let handle = s.shell.read(cx).search.focus_handle(cx);
                handle.is_focused(window)
            })
            .ok()
        })
    };
    report("keys.slash_focuses_search", json!(focused));
    report(
        "keys.slash_opens_search",
        json!(s.update(|sh, _| sh.nav.route.to_link())),
    );
    s.wait(1500).await;
    s.shot("search-field-focused").await;
    s.update(|sh, cx| sh.go(Route::Radio, cx));
    s.wait(500).await;
    if let Some(m) = main {
        let mut cx = s.cx.clone();
        cx.update_window(m.into(), |_, window, cx| {
            let handle = s.shell.read(cx).focus.clone();
            window.focus(&handle, cx)
        })
        .ok();
    }
    key("alt-left");
    s.wait(500).await;
    report(
        "keys.alt_left",
        json!(s.update(|sh, _| sh.nav.route.to_link())),
    );

    // Real: a second instance hands its link over.
    let exe = std::env::current_exe().expect("exe");
    let second =
        s.cx.background_executor()
            .spawn(async move {
                Process::new(exe)
                    .arg("rustet://search?term=boygenius")
                    .status()
            })
            .await;
    report("second_instance.exit", json!(second.map(|s| s.code()).ok()));
    s.wait(1500).await;
    report(
        "second_instance.route",
        json!(s.update(|sh, _| sh.nav.route.to_link())),
    );

    // Real: a setting changes the theme while the window is open (zoom 125 %).
    s.update(|sh, _| {
        sh.app
            .settings()
            .update(|st| st.appearance.zoom = 1.25)
            .is_ok()
    });
    s.wait(800).await;
    s.shot("theme-zoom-125").await;
    s.update(|sh, _| {
        sh.app
            .settings()
            .update(|st| st.appearance.zoom = 1.0)
            .is_ok()
    });
    s.wait(500).await;

    // Real: hide to the tray and show again.
    s.update(|sh, cx| sh.hide_main(cx));
    s.wait(800).await;
    report("hidden", json!(s.update(|sh, _| sh.main.is_none())));
    s.shot("hidden").await;
    s.update(|sh, cx| sh.show_main(cx));
    s.wait(1200).await;
    report("shown", json!(s.update(|sh, _| sh.main.is_some())));
    s.shot("shown-again").await;
}

/// Waits until the shown page has loaded or failed (at most `ms`).
async fn settled(s: &Script<'_>, ms: u64) -> &'static str {
    // The route changes on the app's runtime: let it land before reading the state.
    s.wait(500).await;
    for _ in 0..ms / 250 {
        let state = s.update(|sh, _| match &sh.nav.page {
            rustet_app::nav::PageState::Loading => None,
            rustet_app::nav::PageState::Ready(_) => Some("ready"),
            rustet_app::nav::PageState::Failed(_) => Some("failed"),
            rustet_app::nav::PageState::Local => Some("local"),
        });
        if let Some(state) = state {
            return state;
        }
        s.wait(250).await;
    }
    "loading"
}

async fn pages(s: &Script<'_>) {
    use rustet_ampapi::ArtistView;
    use rustet_app::route::LibrarySection;
    let routes = [
        ("page-new", Route::New),
        ("page-radio", Route::Radio),
        (
            "page-search",
            Route::Search {
                term: "phoebe bridgers".into(),
            },
        ),
        (
            "page-album",
            Route::Album {
                id: "1440833098".into(),
            },
        ),
        (
            "page-artist",
            Route::Artist {
                id: "697833299".into(),
            },
        ),
        (
            "page-playlist",
            Route::Playlist {
                id: "pl.f4d106fed2bd41149aaacabb233eb5eb".into(),
            },
        ),
        ("page-charts", Route::Charts { genre: None }),
        (
            "page-library-signed-out",
            Route::Library(LibrarySection::Albums),
        ),
        ("page-downloads", Route::Downloads),
    ];
    for (name, route) in routes {
        s.update(|sh, cx| sh.go(route.clone(), cx));
        let state = settled(s, 12_000).await;
        report(
            "page",
            json!({ "name": name, "route": route.to_link(), "state": state }),
        );
        s.wait(1800).await;
        s.shot(name).await;
    }
    for section in [
        "account",
        "playback",
        "library",
        "downloads",
        "storage",
        "appearance",
        "history",
        "shortcuts",
        "integrations",
        "links",
    ] {
        s.update(|sh, cx| {
            sh.go(
                Route::Settings {
                    section: Some(section.into()),
                },
                cx,
            )
        });
        s.wait(900).await;
        s.shot(&format!("settings-{section}")).await;
    }

    // Frame times while a grid of albums and a long track list scroll.
    let lists = [
        (
            "bench_album_grid",
            Route::ArtistView {
                id: "159260351".into(),
                view: ArtistView::FullAlbums,
            },
        ),
        (
            "bench_track_list",
            Route::Playlist {
                id: "pl.f4d106fed2bd41149aaacabb233eb5eb".into(),
            },
        ),
    ];
    for (name, route) in lists {
        s.update(|sh, cx| sh.go(route.clone(), cx));
        let state = settled(s, 12_000).await;
        s.wait(2500).await;
        s.shot(&format!("{name}-before")).await;
        let frames = scroll_frames(s, 600).await;
        s.shot(&format!("{name}-after")).await;
        report(
            name,
            json!({ "route": route.to_link(), "state": state, "intervals_ms": summary(&frames) }),
        );
    }
}

/// Scrolls the page under the window's centre by one wheel event per frame for `count` frames
/// (down, then back up halfway) and returns the intervals between frames.
async fn scroll_frames(s: &Script<'_>, count: usize) -> Vec<Duration> {
    let Some(main) = s.update(|sh, _| sh.main) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(count);
    let mut last = std::time::Instant::now();
    for i in 0..count {
        let (tx, rx) = futures::channel::oneshot::channel();
        let dy = if i < count * 2 / 3 { -40. } else { 40. };
        let mut cx = s.cx.clone();
        cx.update_window(main.into(), |_, window, cx| {
            let centre = gpui::point(
                window.viewport_size().width * 0.6,
                window.viewport_size().height * 0.5,
            );
            window.dispatch_event(
                gpui::PlatformInput::Scroll(gpui::ScrollEvent {
                    position: centre,
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(dy))),
                    modifiers: gpui::Modifiers::default(),
                    touch_phase: gpui::TouchPhase::Moved,
                    source: gpui::ScrollSource::Pointer,
                }),
                cx,
            );
            window.on_next_frame(move |_, _| tx.send(()).unwrap_or_default());
            window.refresh();
        })
        .ok();
        if rx.await.is_err() {
            break;
        }
        let now = std::time::Instant::now();
        if i > 0 {
            out.push(now - last);
        }
        last = now;
    }
    out
}

fn summary(samples: &[Duration]) -> serde_json::Value {
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.).collect();
    if ms.is_empty() {
        return json!(null);
    }
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    json!({
        "frames": ms.len(), "p50": at(0.5), "p95": at(0.95), "p99": at(0.99), "max": ms[ms.len() - 1],
        "over_25ms": ms.iter().filter(|v| **v > 25.).count(),
    })
}

async fn a11y(s: &Script<'_>) {
    let items = songs(s).await;
    s.player(playing(&items, Status::Playing));
    s.update(|sh, cx| {
        sh.panel = Some(crate::shell::Panel::Queue);
        cx.notify();
    });
    s.wait(1500).await;
    report("a11y.ready", json!("main"));
    s.wait(6000).await;
    s.account(sign_in(SheetView::SignIn {
        email: String::new(),
        known_account: false,
        busy: false,
        error: None,
    }));
    s.wait(1000).await;
    report("a11y.ready", json!("signin"));
    s.wait(6000).await;
    s.account(sign_in(SheetView::Code {
        busy: None,
        error: None,
        wrong_code: false,
        phone_options: true,
    }));
    s.wait(1000).await;
    report("a11y.ready", json!("code"));
    s.wait(6000).await;
    s.account(AccountState::SignedOut);
    s.update(|sh, cx| {
        sh.panel = None;
        sh.now_playing_open = true;
        cx.notify();
    });
    s.wait(2000).await;
    report("a11y.ready", json!("nowplaying"));
    s.wait(6000).await;
}
