//! The Settings page (spec §6.4): the title, a row of section tabs, and the groups of one
//! section (`docs/native-ui-look.md`, "Settings and Downloads").
//!
//! Every control shows the stored value and writes through [`SettingsStore::update`]; a value
//! the model refuses leaves the stored one in place and a line under the control says why.
//! Settings that live elsewhere (the engine's music-video cache, autoplay, the theme
//! service, Last.fm and ListenBrainz, link schemes) are read from and written to their own
//! owners, never mirrored.

mod account;
mod appearance;
mod controls;
mod downloads;
mod integrations;
mod library;
mod links;
mod playback;
mod shortcuts;
mod storage;

#[cfg(feature = "preview")]
pub mod preview;

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement, Render, Role, ScrollHandle,
    SharedString, Styled, Subscription, Task, Window, div, prelude::*, px,
};
use rustet_app::App;
use rustet_app::account::{AccountState, Backend};
use rustet_app::engine::Connection;
use rustet_app::engine::types::{CacheStats, DrmStatus, FfmpegInfo, MvCache};
use rustet_app::library::{LibraryStatus, SyncStatus};
use rustet_app::route::Route;
use rustet_app::settings::{Settings, SettingsError};
use rustet_desktop::links::schemes::SchemeStatus;
use rustet_integrations::lastfm::LastfmStatus;
use rustet_integrations::listenbrainz::LbStatus;
use rustet_ui::components::{FocusRing, Scrollbar, Toast, Toggle};
use rustet_ui::host::{Host, HostRequest};
use rustet_ui::theme::{ActiveTheme, SEMIBOLD};

use crate::ui::{self, follow};
use controls::Controls;
use shortcuts::Recording;

/// A section of the page, in the order of the section tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Account,
    Playback,
    Appearance,
    Library,
    Downloads,
    Storage,
    Integrations,
    Links,
    Shortcuts,
    History,
}

impl Section {
    pub const ALL: [Self; 10] = [
        Self::Account,
        Self::Playback,
        Self::Appearance,
        Self::Library,
        Self::Downloads,
        Self::Storage,
        Self::Integrations,
        Self::Links,
        Self::Shortcuts,
        Self::History,
    ];

    /// The route's section (`rustet://settings/<slug>`).
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Playback => "playback",
            Self::Library => "library",
            Self::Downloads => "downloads",
            Self::Storage => "storage",
            Self::Appearance => "appearance",
            Self::History => "history",
            Self::Shortcuts => "shortcuts",
            Self::Integrations => "integrations",
            Self::Links => "links",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Account => "Account",
            Self::Playback => "Playback",
            Self::Library => "Library",
            Self::Downloads => "Downloads",
            Self::Storage => "Storage",
            Self::Appearance => "Appearance",
            Self::History => "History",
            Self::Shortcuts => "Shortcuts",
            Self::Integrations => "Integrations",
            Self::Links => "Links",
        }
    }

    /// The section a route names; Account for none or an unknown one.
    pub fn from_slug(slug: Option<&str>) -> Self {
        Self::ALL
            .into_iter()
            .find(|s| Some(s.slug()) == slug)
            .unwrap_or(Self::Account)
    }
}

/// What the page shows besides the settings: the state of the models it drives.
pub(crate) struct Status {
    pub account: AccountState,
    pub backend: Option<Backend>,
    pub drm: Option<DrmStatus>,
    pub connection: Connection,
    pub sync: SyncStatus,
    pub library: Option<LibraryStatus>,
    pub autoplay: Option<bool>,
    /// Music videos play as audio only (the engine keeps the choice).
    pub mv_audio_only: Option<bool>,
    pub cache: Option<CacheStats>,
    pub mv_cache: Option<MvCache>,
    /// The FFmpeg check of the path in the field; `Err` when the engine could not be asked.
    pub ffmpeg: Option<Result<FfmpegInfo, String>>,
    pub lastfm: Option<LastfmStatus>,
    /// Last.fm's authorize page was opened; "Finish connecting" trades the token.
    pub lastfm_waiting: bool,
    pub listenbrainz: Option<LbStatus>,
    pub links: Vec<SchemeStatus>,
}

pub struct SettingsPage {
    pub(crate) app: App,
    pub(crate) section: Section,
    pub(crate) settings: Settings,
    /// Why the last value of a control (by its key) was refused.
    pub(crate) refused: HashMap<&'static str, SharedString>,
    pub(crate) status: Status,
    pub(crate) c: Controls,
    pub(crate) recording: Option<Recording>,
    /// A view of built values (`preview`): the models' watches no longer change the status.
    pub(crate) frozen: bool,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl SettingsPage {
    pub fn new(app: App, _: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = app.settings().get();
        let mut subscriptions = Vec::new();
        let c = Controls::new(&settings, &mut subscriptions, cx);
        let account = app.account();
        let tasks = vec![
            follow(app.settings().subscribe(), cx, |this: &mut Self, s, cx| {
                this.settings = s;
                this.sync_controls(cx);
            }),
            follow(app.nav().state(), cx, |this: &mut Self, nav, cx| {
                if let Route::Settings { section } = nav.route {
                    let section = Section::from_slug(section.as_deref());
                    if section != this.section {
                        this.section = section;
                        this.cancel_recording();
                        this.scroll.set_offset(gpui::point(px(0.), px(0.)));
                        this.refresh(cx);
                    }
                }
            }),
            follow(account.state(), cx, |this: &mut Self, s, _| {
                if !this.frozen {
                    this.status.account = s;
                }
            }),
            follow(account.backend(), cx, |this: &mut Self, b, _| {
                if !this.frozen {
                    this.status.backend = b;
                }
            }),
            follow(account.lossless().status(), cx, |this: &mut Self, d, _| {
                if !this.frozen {
                    this.status.drm = d;
                }
            }),
            follow(app.connection(), cx, |this: &mut Self, c, cx| {
                if !this.frozen {
                    let ready =
                        c == Connection::Ready && this.status.connection != Connection::Ready;
                    this.status.connection = c;
                    // What the engine reports was unknown while it was away.
                    if ready {
                        this.refresh(cx);
                    }
                }
            }),
            follow(account.syncer().status(), cx, |this: &mut Self, s, cx| {
                if !this.frozen {
                    let done = matches!(s, SyncStatus::Done(_));
                    this.status.sync = s;
                    if done {
                        this.refresh_library(cx);
                    }
                }
            }),
            follow(app.player().state(), cx, |this: &mut Self, p, _| {
                if !this.frozen {
                    this.status.autoplay = p.as_ref().map(|p| p.autoplay);
                    this.status.mv_audio_only = p.map(|p| p.mv_audio_only);
                }
            }),
        ];
        let section = match &app.nav().state().borrow().route {
            Route::Settings { section } => Section::from_slug(section.as_deref()),
            _ => Section::Account,
        };
        let mut page = Self {
            status: Status {
                account: AccountState::Starting,
                backend: None,
                drm: None,
                connection: Connection::Starting,
                sync: SyncStatus::Idle,
                library: None,
                autoplay: None,
                mv_audio_only: None,
                cache: None,
                mv_cache: None,
                ffmpeg: None,
                lastfm: None,
                lastfm_waiting: false,
                listenbrainz: None,
                links: Vec::new(),
            },
            app,
            section,
            settings,
            refused: HashMap::new(),
            c,
            recording: None,
            frozen: false,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
            _tasks: tasks,
        };
        page.refresh(cx);
        page
    }

    /// Changes settings through the model. `key` names the control, for its refusal line.
    /// Returns whether the change was taken.
    pub(crate) fn write(
        &mut self,
        key: &'static str,
        change: impl FnOnce(&mut Settings),
        cx: &mut Context<Self>,
    ) -> bool {
        let result = self.app.settings().update(change);
        let taken = match result {
            Ok(new) => {
                self.refused.remove(key);
                let theme = new.appearance != self.settings.appearance;
                self.settings = new;
                if theme {
                    self.apply_theme(cx);
                }
                true
            }
            Err(e) => {
                self.refused.insert(key, refusal(&e).into());
                false
            }
        };
        // A refused value goes back to the stored one; a taken one may be normalised.
        self.sync_controls(cx);
        cx.notify();
        taken
    }

    /// Shows `message` as the refusal of control `key` (a check of this page, not the model's).
    pub(crate) fn refuse(
        &mut self,
        key: &'static str,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.refused.insert(key, message.into());
        cx.notify();
    }

    /// The refusal line of control `key`, when its last value was refused.
    pub(crate) fn refusal_of(&self, key: &'static str, cx: &gpui::App) -> Option<AnyElement> {
        self.refused.get(key).map(|text| {
            ui::refusal(
                SharedString::from(format!("{key}-refused")),
                text.clone(),
                cx,
            )
        })
    }

    /// Loads what the current section shows from the engine and the integrations.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.frozen {
            return;
        }
        match self.section {
            Section::Library => self.refresh_library(cx),
            Section::Storage => self.refresh_storage(cx),
            Section::Downloads => self.check_ffmpeg(cx),
            Section::Integrations => self.refresh_integrations(cx),
            Section::Links => self.refresh_links(cx),
            Section::Appearance => self.refresh_theme(cx),
            _ => {}
        }
    }

    fn go(&self, section: Section) {
        self.app.nav().go(Route::Settings {
            section: Some(section.slug().to_owned()),
        });
    }

    /// The section tabs: quiet pills in a row that wraps; the current one is filled.
    fn section_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        div()
            .id("settings-sections")
            .role(Role::TabList)
            .aria_label("Settings sections")
            .flex()
            .flex_wrap()
            .gap(theme.space.xs)
            .children(Section::ALL.into_iter().map(|section| {
                let current = section == self.section;
                let page = cx.entity();
                div()
                    .id(section.slug())
                    .role(Role::Tab)
                    .aria_label(section.label())
                    .aria_selected(current)
                    .tab_index(0)
                    .focus_ring(cx)
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .h(theme.size.control)
                    .px(theme.space.s)
                    .rounded_full()
                    .text_size(theme.text.body)
                    .font_weight(SEMIBOLD)
                    .when(current, |d| d.bg(colors.fill).text_color(colors.text))
                    .when(!current, |d| {
                        d.text_color(colors.text_secondary)
                            .hover(|d| d.text_color(colors.text))
                    })
                    .on_click(move |_, _, cx| page.read(cx).go(section))
                    .child(section.label())
            }))
    }

    fn content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        match self.section {
            Section::Account => self.account(cx),
            Section::Playback => self.playback(cx),
            Section::Library => self.library(cx),
            Section::Downloads => self.downloads(window, cx),
            Section::Storage => self.storage(cx),
            Section::Appearance => self.appearance(cx),
            Section::History => self.history(cx),
            Section::Shortcuts => self.shortcuts(cx),
            Section::Integrations => self.integrations(cx),
            Section::Links => self.links(cx),
        }
    }
}

/// The description of a stored setting that no part of Rustet reads yet: it must not look as if
/// it worked.
pub(crate) const NOT_USED_YET: &str = "Saved, but downloads do not use it yet.";

/// The model's refusal, as a sentence that says what to do.
pub(crate) fn refusal(e: &SettingsError) -> String {
    match e {
        SettingsError::MvHeight(_) => "Choose 480p, 720p, 1080p or 4K.".to_owned(),
        SettingsError::CrossfadeSeconds(_) => "Choose a crossfade from 1 to 12 seconds.".to_owned(),
        SettingsError::RetryDelay(_) => {
            "Choose 15 seconds, 30 seconds, 1 minute or 5 minutes.".to_owned()
        }
        SettingsError::Zoom(_) => "Choose a zoom above 0 %.".to_owned(),
        SettingsError::DuplicateKey => {
            "That key is already used by another shortcut. Choose another key.".to_owned()
        }
        SettingsError::Io(e) => format!(
            "Rustet could not save this setting ({e}). Check that the config folder can be written."
        ),
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // The page starts under the floating strip and ends above the player pill.
        let body = div()
            .flex()
            .flex_col()
            .gap(theme.space.l28)
            .px(theme.space.xl)
            .pt(theme.size.strip)
            .pb(theme.size.player_clearance)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.space.m16)
                    .child(ui::page_title("Settings", cx))
                    .child(self.section_tabs(cx)),
            )
            .child(
                div()
                    .id("settings-section")
                    .role(Role::TabPanel)
                    .aria_label(self.section.label())
                    .flex()
                    .flex_col()
                    .gap(theme.space.l28)
                    .w_full()
                    .max_w(theme.size.settings_max_width)
                    .children(self.content(window, cx)),
            );
        div()
            .id("settings-page")
            .key_context("Settings")
            .relative()
            .size_full()
            .text_color(theme.colors.text)
            .child(
                div()
                    .id("settings-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(body),
            )
            .child(Scrollbar::new("settings-scrollbar", self.scroll.clone()))
    }
}

impl SettingsPage {
    /// A switch over one stored setting.
    pub(crate) fn toggle(
        &self,
        key: &'static str,
        label: &'static str,
        on: bool,
        set: fn(&mut Settings, bool),
        cx: &mut Context<Self>,
    ) -> Toggle {
        let page = cx.entity();
        Toggle::new(key, label, on).on_change(move |on, _, cx| {
            page.update(cx, |p, cx| {
                p.write(key, |s| set(s, on), cx);
            })
        })
    }

    /// A click handler that runs `f` on the page.
    pub(crate) fn on(
        &self,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
        let page: Entity<Self> = cx.entity();
        move |_, window, cx| page.update(cx, |p, cx| f(p, window, cx))
    }

    /// A toast from the window.
    pub(crate) fn toast(toast: Toast, window: &mut Window, cx: &mut gpui::App) {
        Host::request(HostRequest::Toast(toast), window, cx);
    }
}
