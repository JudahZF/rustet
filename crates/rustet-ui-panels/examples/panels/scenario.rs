//! Scripted runs (`--scenario NAME`) for the headless compositor:
//!
//! - `tour`: every Settings section and Downloads at 1360, 820 and 480 px wide (480 is the
//!   narrowest window; run once with `--dark`, once with `--light`);
//! - `preview`: the states that need an account or a running download, from built values;
//! - `write`: changes one control of each kind by keyboard (select, slider, text field,
//!   toggle, shortcut recorder, a refused value, a confirmation dialog, live zoom);
//! - `verify`: a second start on the same `--state-dir`: reports what is stored and shows it;
//! - `a11y`: holds each section for `scripts/a11y-check.sh`.
//!
//! Screenshots go to `RUSTET_UI_SHOT_DIR`, cropped to the window.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use gpui::{
    AppContext as _, AsyncApp, Entity, KeyUpEvent, Keystroke, PlatformInput, WindowHandle, px, size,
};
use rustet_app::App;
use rustet_app::account::{AccountState, Backend};
use rustet_app::downloads::DownloadList;
use rustet_app::engine::Connection;
use rustet_app::engine::types::{
    CacheStats, DrmCapabilities, DrmDimensions, DrmStatus, ExportJob, ExportPhase, FairPlayState,
    MvCache, PersistentCache, SessionState,
};
use rustet_app::library::{LibraryStats, LibraryStatus, SyncEvent, SyncStatus};
use rustet_app::route::Route;
use rustet_app::settings::ShortcutAction;
use rustet_integrations::lastfm::LastfmStatus;
use rustet_integrations::listenbrainz::LbStatus;
use rustet_ui_panels::preview::{self, SettingsPreview};
use rustet_ui_panels::{DownloadsPage, Section, SettingsPage};
use serde_json::json;

use crate::{Demo, report};

pub fn run(name: String, window: WindowHandle<Demo>, app: App, cx: &mut gpui::App) {
    cx.spawn(async move |cx| {
        let s = Script { window, cx, app };
        // The engine starts and the first snapshot arrives.
        s.wait(4000).await;
        match name.as_str() {
            "tour" => tour(&s).await,
            "preview" => preview_states(&s).await,
            "write" => write(&s).await,
            "verify" => verify(&s).await,
            "a11y" => a11y(&s).await,
            other => report("error", json!(format!("unknown scenario {other}"))),
        }
        s.cx.update(|cx| cx.quit());
    })
    .detach();
}

struct Script<'a> {
    window: WindowHandle<Demo>,
    cx: &'a AsyncApp,
    app: App,
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
        f: impl FnOnce(&mut Demo, &mut gpui::Window, &mut gpui::Context<Demo>) -> R,
    ) -> Option<R> {
        let mut cx = self.cx.clone();
        self.window.update(&mut cx, f).ok()
    }

    fn settings(&self) -> Option<Entity<SettingsPage>> {
        self.update(|d, _, _| d.page.clone()?.downcast::<SettingsPage>().ok())
            .flatten()
    }

    fn downloads(&self) -> Option<Entity<DownloadsPage>> {
        self.update(|d, _, _| d.page.clone()?.downcast::<DownloadsPage>().ok())
            .flatten()
    }

    async fn go(&self, route: Route) {
        self.app.nav().go(route);
        self.wait(900).await;
    }

    async fn section(&self, section: Section) {
        self.go(Route::Settings {
            section: Some(section.slug().to_owned()),
        })
        .await;
    }

    async fn keys(&self, keys: &'static str) {
        for key in keys.split(' ') {
            let stroke = Keystroke::parse(key).unwrap_or_else(|_| panic!("bad keystroke {key}"));
            let mut cx = self.cx.clone();
            // Down, then up: a focused element is clicked by Enter or Space on release.
            cx.update_window(self.window.into(), |_, window, cx| {
                window.dispatch_keystroke(stroke.clone(), cx);
                window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke: stroke }), cx);
            })
            .ok();
            self.wait(80).await;
        }
    }

    async fn focus(&self, control: &str) {
        let page = self.settings().expect("the settings page");
        let ok = self.update(|_, window, cx| preview::focus_control(&page, control, window, cx));
        report("focus", json!({ "control": control, "ok": ok }));
        self.wait(150).await;
    }

    async fn resize(&self, width: f32) {
        self.update(|_, window, _| window.resize(size(px(width), px(860.))));
        self.wait(700).await;
    }

    async fn shot(&self, name: &str) {
        self.wait(400).await;
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
                    Command::new("import")
                        .args(["-window", "root"])
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

    fn stored(&self) -> serde_json::Value {
        let s = self.app.settings().get();
        json!({
            "streaming_quality": format!("{:?}", s.playback.quality),
            "song_cache_limit_mb": s.storage.song_cache_limit_mb,
            "track_template": s.downloads.track_template,
            "explicit_marker": s.downloads.explicit_marker.enabled,
            "seek_back_key": s.shortcuts.bindings.get(&ShortcutAction::SeekBack).map(|k| k.0.clone()),
            "zoom": s.appearance.zoom,
        })
    }
}

async fn tour(s: &Script<'_>) {
    let tag = if std::env::args().any(|a| a == "--light") {
        "light"
    } else {
        "dark"
    };
    for width in [1360., 820., 480.] {
        s.resize(width).await;
        for section in Section::ALL {
            s.section(section).await;
            let name = format!("{tag}-{width}-{}", section.slug());
            s.shot(&name).await;
            let page = s.settings().expect("the settings page");
            let overflow = s
                .update(|_, _, cx| preview::settings_overflow(&page, cx))
                .unwrap_or(0.);
            if overflow > 40. {
                s.update(|_, _, cx| preview::scroll_settings(&page, true, cx));
                s.shot(&format!("{name}-end")).await;
                s.update(|_, _, cx| preview::scroll_settings(&page, false, cx));
            }
        }
        s.go(Route::Downloads).await;
        s.shot(&format!("{tag}-{width}-page-downloads")).await;
    }
    report("tour.done", json!(true));
}

fn drm_ready() -> DrmStatus {
    DrmStatus {
        state: DrmDimensions {
            fairplay: FairPlayState::Ready,
            session: SessionState::Valid,
            ..DrmDimensions::default()
        },
        capabilities: DrmCapabilities {
            cbcs: true,
            alac: true,
            atmos: false,
            hi_res: true,
        },
        available: true,
        ready_since_ms: Some(0),
        session_age_ms: Some(2 * 3600 * 1000),
        session_ttl_ms: Some((5 * 3600 + 20 * 60) * 1000),
        ..DrmStatus::default()
    }
}

/// A list with one job in every state the page draws.
pub fn sample_jobs() -> DownloadList {
    let job = |id: &str, pos: i64, title: &str, artist: &str, phase: ExportPhase| ExportJob {
        job_id: id.into(),
        asset_id: format!("asset-{id}"),
        phase,
        queue_pos: pos,
        title: title.into(),
        artist_name: artist.into(),
        ..ExportJob::default()
    };
    DownloadList {
        jobs: vec![
            ExportJob {
                output: "/home/listener/Music/Rustet-Downloads/Phoebe Bridgers/2020 - Punisher/01 - DVD Menu.m4a".into(),
                percent: 100,
                ..job("1", 1, "DVD Menu", "Phoebe Bridgers", ExportPhase::Done)
            },
            ExportJob {
                error: "Apple refused the stream for this storefront. Try again later.".into(),
                ..job("2", 2, "Kyoto", "Phoebe Bridgers", ExportPhase::Failed)
            },
            ExportJob {
                percent: 42,
                bytes_done: 18_400_000,
                bytes_total: 43_800_000,
                source: Some("network".into()),
                ..job("3", 3, "Garden Song", "Phoebe Bridgers", ExportPhase::Downloading)
            },
            ExportJob { queue_index: 1, ..job("4", 4, "Halloween", "Phoebe Bridgers", ExportPhase::Queued) },
            ExportJob { queue_index: 2, ..job("5", 5, "Chinese Satellite", "Phoebe Bridgers", ExportPhase::Queued) },
        ],
    }
}

async fn preview_states(s: &Script<'_>) {
    let tag = if std::env::args().any(|a| a == "--light") {
        "light"
    } else {
        "dark"
    };
    s.section(Section::Account).await;
    let page = s.settings().expect("the settings page");
    let stats = LibraryStats {
        songs: 4182,
        albums: 391,
        playlists: 27,
        synced_at: Some(chrono::Utc::now() - chrono::Duration::hours(3)),
    };
    s.update(|_, window, cx| {
        preview::show_settings(
            &page,
            SettingsPreview {
                account: Some(AccountState::SignedIn {
                    storefront: "gb".into(),
                }),
                backend: Some(Backend::FairPlay),
                drm: Some(drm_ready()),
                connection: Some(Connection::Ready),
                sync: Some(SyncStatus::Running(Some(SyncEvent::Playlist {
                    index: 11,
                    total: 27,
                    name: "Late trains".into(),
                    reused: false,
                }))),
                library: Some(LibraryStatus {
                    stats,
                    needs_sync: false,
                }),
                autoplay: Some(true),
                cache: Some(CacheStats {
                    persistent: PersistentCache {
                        available: true,
                        size_bytes: 312_000_000,
                        limit_bytes: 500_000_000,
                        ttl_days: 0,
                        count: 64,
                    },
                    ..CacheStats::default()
                }),
                mv_cache: Some(MvCache {
                    enabled: true,
                    max_bytes: 2 << 30,
                    quality: "1080p".into(),
                    size_bytes: 640_000_000,
                }),
                lastfm: Some(LastfmStatus {
                    configured: true,
                    api_key: "0123abcd".into(),
                    secret_set: true,
                    connected: true,
                    username: Some("slowlistener".into()),
                }),
                listenbrainz: Some(LbStatus {
                    connected: true,
                    username: Some("slowlistener".into()),
                }),
            },
            window,
            cx,
        )
    });
    for section in [
        Section::Account,
        Section::Playback,
        Section::Library,
        Section::Storage,
        Section::Integrations,
    ] {
        s.section(section).await;
        s.shot(&format!("preview-{tag}-{}", section.slug())).await;
    }
    s.go(Route::Downloads).await;
    let downloads = s.downloads().expect("the downloads page");
    s.update(|_, _, cx| preview::show_downloads(&downloads, sample_jobs(), Some(3_100_000.), cx));
    s.shot(&format!("preview-{tag}-downloads")).await;
    s.resize(820.).await;
    s.shot(&format!("preview-{tag}-downloads-820")).await;
    report("preview.done", json!(true));
}

async fn write(s: &Script<'_>) {
    report("stored.before", s.stored());

    // A select, by keyboard: closed, Down takes the next choice.
    s.section(Section::Playback).await;
    s.focus("quality").await;
    s.keys("down").await;
    s.shot("write-select").await;

    // A slider: two steps right, committed on each key.
    s.section(Section::Storage).await;
    s.focus("song-cache").await;
    s.keys("right right").await;
    s.shot("write-slider").await;

    // A confirmation: Tab from the expiry select reaches "Clear song cache".
    s.focus("ttl").await;
    s.keys("tab enter").await;
    s.shot("write-confirm-dialog").await;
    s.keys("escape").await;
    s.wait(300).await;

    // A text field: emptied (refused, with the reason), then typed.
    s.section(Section::Downloads).await;
    s.focus("track-template").await;
    s.keys("ctrl-a backspace").await;
    s.shot("write-text-refused").await;
    s.keys("s o n g").await;
    s.shot("write-text").await;

    // A toggle: Tab from the field reaches "Mark explicit tracks"; Space flips it.
    s.keys("tab space").await;
    s.shot("write-toggle").await;

    // The shortcut recorder: a conflict is refused with its reason, then a free key is taken.
    s.section(Section::Shortcuts).await;
    let page = s.settings().expect("the settings page");
    s.update(|_, window, cx| preview::record_shortcut(&page, ShortcutAction::SeekBack, window, cx));
    s.wait(200).await;
    s.shot("write-recorder-recording").await;
    s.keys("l").await;
    s.shot("write-recorder-conflict").await;
    s.keys("j").await;
    s.shot("write-recorder-done").await;

    // Zoom applies at once.
    s.section(Section::Appearance).await;
    s.focus("zoom").await;
    s.keys("down").await;
    s.wait(400).await;
    s.shot("write-zoom-live").await;

    report("stored.after", s.stored());
}

async fn verify(s: &Script<'_>) {
    report("stored.after_restart", s.stored());
    for section in [
        Section::Playback,
        Section::Storage,
        Section::Downloads,
        Section::Shortcuts,
        Section::Appearance,
    ] {
        s.section(section).await;
        s.shot(&format!("verify-{}", section.slug())).await;
    }
}

async fn a11y(s: &Script<'_>) {
    for section in Section::ALL {
        s.section(section).await;
        report("a11y.ready", json!(section.slug()));
        s.wait(3500).await;
    }
    s.go(Route::Downloads).await;
    let downloads = s.downloads().expect("the downloads page");
    s.update(|_, _, cx| preview::show_downloads(&downloads, sample_jobs(), Some(3_100_000.), cx));
    report("a11y.ready", json!("page-downloads"));
    s.wait(3500).await;
}
