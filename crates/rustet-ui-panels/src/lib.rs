//! The pages that have no loader: Settings and Downloads (spec §6.4, §1.5).
//!
//! The shell puts [`settings_page`] and [`downloads_page`] in the page region for the routes
//! `Settings { section }` and `Downloads`. Both draw `rustet_app` models and change them through
//! their methods: every Settings control writes through `App::settings().update` and shows the
//! stored value, or the reason the value was refused. Dialogs, menus and external links go to
//! the window through [`rustet_ui::host::Host::request`].
//!
//! The theme settings (light or dark, preset, window blur, sidebar opacity) and the link
//! schemes live in the app's prefs file (`App::prefs`) through
//! `rustet_desktop::theme::ThemeService` and `rustet_desktop::links`.
#![forbid(unsafe_code)]

mod confirm;
mod downloads;
mod format;
mod settings;
mod ui;

use std::sync::Arc;

use gpui::{AnyView, AppContext as _, Global, WeakEntity, Window};
use rustet_app::App;
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::theme::ThemeService;

pub use downloads::{DownloadsPage, JobAction, JobRow, JobStatus};
pub use settings::{Section, SettingsPage};

/// The open pages: the shell may call the page functions on every route change (or every
/// frame); the same view comes back while it is alive, so its state (a running shortcut
/// recording, the scroll position) survives.
#[derive(Default)]
struct Open {
    settings: Option<WeakEntity<SettingsPage>>,
    downloads: Option<WeakEntity<DownloadsPage>>,
}

impl Global for Open {}

/// The Settings page. The section comes from `app.nav().state()`; choosing a section
/// navigates to `Route::Settings { section }`.
pub fn settings_page(app: &App, window: &mut Window, cx: &mut gpui::App) -> AnyView {
    if let Some(view) = cx
        .try_global::<Open>()
        .and_then(|o| o.settings.as_ref()?.upgrade())
    {
        return view.into();
    }
    let view = cx.new(|cx| SettingsPage::new(app.clone(), window, cx));
    cx.default_global::<Open>().settings = Some(view.downgrade());
    view.into()
}

/// The Downloads page.
pub fn downloads_page(app: &App, window: &mut Window, cx: &mut gpui::App) -> AnyView {
    if let Some(view) = cx
        .try_global::<Open>()
        .and_then(|o| o.downloads.as_ref()?.upgrade())
    {
        return view.into();
    }
    let view = cx.new(|cx| DownloadsPage::new(app.clone(), window, cx));
    cx.default_global::<Open>().downloads = Some(view.downgrade());
    view.into()
}

/// The prefs file the theme settings and link schemes are saved in, and its theme service.
pub(crate) struct DesktopPrefs {
    pub prefs: Arc<PrefsFile>,
    /// `Err`: the one-time theme migration could not be saved (the folder is not writable).
    pub service: Result<ThemeService, String>,
}

impl Global for DesktopPrefs {}

impl DesktopPrefs {
    /// The app's prefs file and its theme service, opened on first use.
    pub fn of<'a>(app: &App, cx: &'a mut gpui::App) -> &'a DesktopPrefs {
        if !cx.has_global::<Self>() {
            let prefs = app.prefs().clone();
            let service = ThemeService::new(prefs.clone()).map_err(|e| {
                tracing::warn!("theme prefs: {e}");
                e.to_string()
            });
            cx.set_global(DesktopPrefs { prefs, service });
        }
        cx.global::<Self>()
    }
}

/// Built model values for screenshots and view tests: states that need an Apple account or a
/// running download, drawn without one. Feature `preview`; not in the app.
#[cfg(feature = "preview")]
pub mod preview {
    pub use crate::downloads::preview::*;
    pub use crate::settings::preview::*;
}
