//! Built model values for the Settings page: the states that need an Apple account (signed
//! in, FairPlay ready, a sync running, a connected scrobbler), for screenshots and tests.

use gpui::{Entity, Window};
use rustet_app::account::{AccountState, Backend};
use rustet_app::engine::Connection;
use rustet_app::engine::types::{CacheStats, DrmStatus, MvCache};
use rustet_app::library::{LibraryStatus, SyncStatus};
use rustet_integrations::lastfm::LastfmStatus;
use rustet_integrations::listenbrainz::LbStatus;

use super::SettingsPage;

/// What to show instead of the models' values; `None` keeps the current one.
#[derive(Default)]
pub struct SettingsPreview {
    pub account: Option<AccountState>,
    pub backend: Option<Backend>,
    pub drm: Option<DrmStatus>,
    pub connection: Option<Connection>,
    pub sync: Option<SyncStatus>,
    pub library: Option<LibraryStatus>,
    pub autoplay: Option<bool>,
    pub cache: Option<CacheStats>,
    pub mv_cache: Option<MvCache>,
    pub lastfm: Option<LastfmStatus>,
    pub listenbrainz: Option<LbStatus>,
}

/// Draws `page` from `preview` from now on: the models' watches stop changing what it shows
/// (the settings themselves still follow the store).
pub fn show_settings(
    page: &Entity<SettingsPage>,
    preview: SettingsPreview,
    _: &mut Window,
    cx: &mut gpui::App,
) {
    page.update(cx, |p, cx| {
        p.frozen = true;
        let s = &mut p.status;
        if let Some(a) = preview.account {
            s.account = a;
        }
        if preview.backend.is_some() {
            s.backend = preview.backend;
        }
        if preview.drm.is_some() {
            s.drm = preview.drm;
        }
        if let Some(c) = preview.connection {
            s.connection = c;
        }
        if let Some(sync) = preview.sync {
            s.sync = sync;
        }
        if preview.library.is_some() {
            s.library = preview.library;
        }
        if preview.autoplay.is_some() {
            s.autoplay = preview.autoplay;
        }
        if preview.cache.is_some() {
            s.cache = preview.cache;
        }
        if preview.mv_cache.is_some() {
            s.mv_cache = preview.mv_cache;
        }
        if preview.lastfm.is_some() {
            s.lastfm = preview.lastfm;
        }
        if preview.listenbrainz.is_some() {
            s.listenbrainz = preview.listenbrainz;
        }
        p.show_mv_cache(cx);
        cx.notify();
    });
}

/// Gives the keyboard focus to a stateful control by its key (`quality`, `song-cache`,
/// `track-template`, …), so a run can drive the page by keys from there. `false`: no such
/// control in this build of the page.
pub fn focus_control(
    page: &Entity<SettingsPage>,
    name: &str,
    window: &mut Window,
    cx: &mut gpui::App,
) -> bool {
    let Some((_, handle)) = controls(page, cx).into_iter().find(|(n, _)| *n == name) else {
        return false;
    };
    window.focus(&handle, cx);
    true
}

/// The key of the stateful control that has the keyboard focus (see [`focus_control`]).
pub fn focused_control(
    page: &Entity<SettingsPage>,
    window: &Window,
    cx: &gpui::App,
) -> Option<&'static str> {
    controls(page, cx)
        .into_iter()
        .find(|(_, h)| h.is_focused(window))
        .map(|(name, _)| name)
}

fn controls(page: &Entity<SettingsPage>, cx: &gpui::App) -> Vec<(&'static str, gpui::FocusHandle)> {
    use gpui::Focusable as _;
    let c = &page.read(cx).c;
    vec![
        ("quality", c.quality.select.focus_handle(cx)),
        ("mv-height", c.mv_height.select.focus_handle(cx)),
        ("dl-quality", c.dl_quality.select.focus_handle(cx)),
        ("retry-delay", c.retry_delay.select.focus_handle(cx)),
        ("ttl", c.ttl.select.focus_handle(cx)),
        ("zoom", c.zoom.select.focus_handle(cx)),
        ("song-cache", c.song_cache.slider.focus_handle(cx)),
        ("track-template", c.track_template.focus_handle(cx)),
        ("album-template", c.album_template.focus_handle(cx)),
        ("folder", c.folder.focus_handle(cx)),
    ]
}

/// Starts recording a new key for `action`, as a click on its key button does.
pub fn record_shortcut(
    page: &Entity<SettingsPage>,
    action: rustet_app::settings::ShortcutAction,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    page.update(cx, |p, cx| p.record(action, window, cx));
}

/// Scrolls the section to its top or its end.
pub fn scroll_settings(page: &Entity<SettingsPage>, to_end: bool, cx: &mut gpui::App) {
    page.update(cx, |p, cx| {
        let y = if to_end {
            -p.scroll.max_offset().y
        } else {
            gpui::px(0.)
        };
        p.scroll.set_offset(gpui::point(gpui::px(0.), y));
        cx.notify();
    });
}

/// How far the section scrolls, in pixels.
pub fn settings_overflow(page: &Entity<SettingsPage>, cx: &gpui::App) -> f32 {
    f32::from(page.read(cx).scroll.max_offset().y)
}

/// Why the last value of control `key` was refused, when it was.
pub fn refusal(
    page: &Entity<SettingsPage>,
    key: &str,
    cx: &gpui::App,
) -> Option<gpui::SharedString> {
    page.read(cx).refused.get(key).cloned()
}

/// The shortcut being recorded.
pub fn recording(
    page: &Entity<SettingsPage>,
    cx: &gpui::App,
) -> Option<rustet_app::settings::ShortcutAction> {
    page.read(cx).recording.as_ref().map(|r| r.action)
}

/// The section shown.
pub fn section(page: &Entity<SettingsPage>, cx: &gpui::App) -> super::Section {
    page.read(cx).section
}
