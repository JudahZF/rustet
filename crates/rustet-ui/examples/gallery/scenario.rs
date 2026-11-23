//! Scripted runs of the gallery (`--scenario NAME`), for the headless scripts:
//!
//! - `tour`: screenshots of every page in dark and light, the focus ring, a menu with its
//!   sub-menu, the sheet, the dialog, toasts, search suggestions and the quality crossfade;
//! - `bench`: scrolls the 5,000-tile grid and the 10,000-row list and reports frame times,
//!   CPU and memory;
//! - `a11y`: holds the states `a11y-check.sh` dumps the AT-SPI tree in;
//! - `input`: opens the Components page and waits for `input-x11.sh` to drive it.
//!
//! Screenshots go to `RUSTET_UI_SHOT_DIR`, cropped to the window (Wayland: trimmed to the
//! compositor's magenta; X11: the window by name).

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, AsyncApp, Focusable as _, Keystroke, WindowHandle, point, px};
use rustet_ui::artwork::ArtworkStore;
use rustet_ui::theme::Appearance;
use serde_json::json;

use crate::{Gallery, Section, report};

pub fn run(name: String, window: WindowHandle<Gallery>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let s = Script { window, cx };
        // The pages load from the local mock; give them, and the first artwork, a moment.
        s.wait(2500).await;
        match name.as_str() {
            "tour" => tour(&s).await,
            "bench" => bench(&s).await,
            "a11y" => a11y(&s).await,
            "input" => {
                report("input.ready", json!(true));
                return;
            }
            other => report("error", json!(format!("unknown scenario {other}"))),
        }
        s.cx.update(|cx| cx.quit());
    })
    .detach();
}

struct Script<'a> {
    window: WindowHandle<Gallery>,
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
        f: impl FnOnce(&mut Gallery, &mut gpui::Window, &mut gpui::Context<Gallery>) -> R,
    ) -> Option<R> {
        let mut cx = self.cx.clone();
        self.window.update(&mut cx, f).ok()
    }

    /// Types `keys` (space-separated keystrokes) as keyboard input.
    async fn keys(&self, keys: &'static str) {
        for key in keys.split(' ') {
            let Ok(stroke) = Keystroke::parse(key) else {
                panic!("bad keystroke {key}")
            };
            // Through the bare window: the key's handlers update the gallery themselves.
            let mut cx = self.cx.clone();
            cx.update_window(self.window.into(), |_, window, cx| {
                window.dispatch_keystroke(stroke, cx)
            })
            .ok();
            self.wait(60).await;
        }
    }

    async fn section(&self, section: Section) {
        self.update(|g, _, cx| g.set_section(section, cx));
        self.wait(1200).await;
    }

    async fn scroll_to(&self, y: f32) {
        self.update(|g, _, cx| {
            g.scroll.set_offset(point(px(0.), px(-y)));
            cx.notify();
        });
        self.wait(700).await;
    }

    /// The page's scrollable height beyond the viewport.
    fn max_scroll(&self) -> f32 {
        self.update(|g, _, _| f32::from(g.scroll.max_offset().y))
            .unwrap_or(0.)
    }

    /// Takes a screenshot of the screen and crops it to the window (the server's background
    /// is magenta).
    async fn shot(&self, name: &str) {
        self.wait(350).await;
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
                    // The window itself, without the window manager's frame.
                    let id = Command::new("xdotool")
                        .args(["search", "--name", "^Rustet gallery$"])
                        .output()
                        .ok();
                    let id = id.and_then(|o| {
                        String::from_utf8(o.stdout)
                            .ok()?
                            .lines()
                            .next()
                            .map(str::to_owned)
                    });
                    Command::new("import")
                        .args(["-window", id.as_deref().unwrap_or("root")])
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
}

async fn tour(s: &Script<'_>) {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let tag = if appearance == Appearance::Dark {
            "dark"
        } else {
            "light"
        };
        s.update(|g, _, cx| g.set_appearance(appearance, cx));
        for section in [
            Section::Components,
            Section::Album,
            Section::Artist,
            Section::Search,
            Section::New,
            Section::Lyrics,
        ] {
            s.section(section).await;
            let name = format!("{tag}-{}", section.label().to_lowercase().replace(' ', "-"));
            s.shot(&name).await;
            // The long pages, further down.
            let max = s.max_scroll();
            let steps = match section {
                Section::Components => 3,
                Section::Album | Section::Artist | Section::New => 1,
                _ => 0,
            };
            for step in 1..=steps {
                s.scroll_to(max * step as f32 / steps as f32).await;
                s.shot(&format!("{name}-{step}")).await;
            }
        }
    }

    s.update(|g, _, cx| g.set_appearance(Appearance::Dark, cx));
    s.section(Section::Components).await;

    // Keyboard focus: Tab into the sidebar, then into the page's buttons.
    s.keys("tab tab").await;
    s.shot("focus-rail").await;
    s.keys("tab tab tab tab tab tab tab tab tab tab").await;
    s.shot("focus-page").await;

    // The track menu, by keyboard: down to "Add to playlist", Right opens the sub-menu.
    s.update(|g, window, cx| g.open_menu(point(px(520.), px(240.)), None, window, cx));
    s.wait(200).await;
    s.keys("down down down down right").await;
    s.shot("menu-submenu").await;
    s.keys("down enter").await;
    s.shot("toast-from-menu").await;

    s.update(|g, _, cx| {
        g.sheet_open = true;
        g.code.update(cx, |c, cx| c.insert("1234", cx));
        cx.notify();
    });
    s.wait(400).await;
    s.shot("sheet").await;
    s.keys("escape").await;

    s.update(|g, _, cx| {
        g.dialog_open = true;
        cx.notify();
    });
    s.wait(400).await;
    s.shot("dialog").await;
    s.keys("escape").await;

    // Search: type into the field; the mock answers with the hints for "phoebe".
    s.section(Section::Search).await;
    s.keys("ctrl-k").await;
    let focused = s.update(|g, window, cx| g.search.focus_handle(cx).is_focused(window));
    report("search.focused_by_ctrl_k", json!(focused));
    s.keys("p h o e b e").await;
    s.wait(700).await;
    s.keys("down").await;
    s.shot("search-suggestions").await;
    s.keys("escape escape").await;

    // The quality strip: mid-crossfade, then settled.
    s.section(Section::Lyrics).await;
    s.wait(1500).await;
    s.shot("lyrics-playing").await;
    s.update(|g, _, cx| g.next_quality(cx));
    s.wait(60).await;
    s.shot("quality-crossfade").await;
    s.update(|g, _, cx| g.next_quality(cx));
    s.wait(600).await;
    s.shot("quality-aac").await;

    // Light, keyboard focus and a toast.
    s.update(|g, _, cx| g.set_appearance(Appearance::Light, cx));
    s.section(Section::Album).await;
    s.keys("tab tab tab tab tab tab tab tab tab tab tab").await;
    s.shot("light-focus-album").await;
    report("tour.done", json!(true));
}

/// CPU time of this process (user + system), from /proc.
fn cpu_time() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let ticks: u64 = [11, 12]
        .iter()
        .filter_map(|&i| fields.get(i)?.parse::<u64>().ok())
        .sum();
    Duration::from_millis(ticks * 10)
}

fn rss_mib() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        .map_or(0., |kb| kb / 1024.)
}

async fn bench(s: &Script<'_>) {
    s.section(Section::Lists).await;
    for (rows, name, speed) in [
        (false, "grid_5000_tiles", 30.),
        (true, "list_10000_rows", 40.),
    ] {
        s.update(|g, _, cx| {
            g.lists_tab_rows = rows;
            cx.notify();
        });
        s.wait(2500).await;
        s.update(|g, _, cx| {
            g.frame_stats.reset();
            g.autoscroll = Some(speed);
            cx.notify();
        });
        let (cpu, wall) = (cpu_time(), Instant::now());
        s.wait(12_000).await;
        let stats = s.update(|g, _, cx| {
            g.autoscroll = None;
            let artwork = ArtworkStore::global(cx).read(cx).stats();
            (
                g.frame_stats.intervals(),
                g.frame_stats.work(),
                g.frame_stats.late(Duration::from_micros(25_000)),
                format!("{artwork:?}"),
            )
        });
        let cpu_share = (cpu_time() - cpu).as_secs_f64() / wall.elapsed().as_secs_f64();
        if let Some((intervals, work, late, artwork)) = stats {
            report(
                name,
                json!({
                    "speed_px_per_frame": speed,
                    "interval_ms": intervals.map(|s| json!({"count": s.count, "p50": s.p50, "p95": s.p95, "p99": s.p99, "max": s.max})),
                    "cpu_work_ms": work.map(|s| json!({"p50": s.p50, "p95": s.p95, "p99": s.p99, "max": s.max})),
                    "frames_over_25ms": late,
                    "cpu_cores": cpu_share,
                    "rss_mib": rss_mib(),
                    "artwork": artwork,
                }),
            );
        }
    }
}

async fn a11y(s: &Script<'_>) {
    s.section(Section::Components).await;
    report("a11y.ready", json!("components"));
    s.wait(5000).await;
    s.section(Section::Album).await;
    report("a11y.ready", json!("album"));
    s.wait(5000).await;
    s.update(|g, window, cx| g.open_menu(point(px(520.), px(240.)), None, window, cx));
    s.keys("down down down down right").await;
    report("a11y.ready", json!("menu"));
    s.wait(5000).await;
}
