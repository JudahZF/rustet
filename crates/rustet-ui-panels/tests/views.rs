//! The Settings page on gpui's test platform over a real `App` whose engine never starts (its
//! program does not exist): every control writes through the settings model to the prefs file
//! or the encrypted store, under the keys the web shell used.
//!
//!     cargo test -p rustet-ui-panels --features test-support
#![cfg(feature = "test-support")]

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Context, Entity, IntoElement, KeyUpEvent, Keystroke, Render, TestAppContext, VisualTestContext,
    Window, div, prelude::*,
};
use rustet_app::account::SignInProgram;
use rustet_app::route::Route;
use rustet_app::settings::ShortcutAction;
use rustet_app::{App, AppConfig};
use rustet_desktop::engine::{DrmLayout, EngineProgram, RustetEngine, SupervisorTiming};
use rustet_desktop::platform::{EngineLibs, Env};
use rustet_desktop::prefs::PrefsFile;
use rustet_desktop::store::{Store, StoreCipher};
use rustet_ui::components;
use rustet_ui::theme::{Appearance, Theme};
use rustet_ui_panels::preview;
use rustet_ui_panels::{Section, SettingsPage};
use serde_json::{Value, json};

struct Fixture {
    runtime: tokio::runtime::Runtime,
    app: App,
    prefs: Arc<PrefsFile>,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let prefs = Arc::new(PrefsFile::new(&config_dir));
        let app = runtime.block_on(async {
            let config = AppConfig {
                store: Store::open(&config_dir, StoreCipher::Plain).await,
                prefs: prefs.clone(),
                config_dir: config_dir.clone(),
                cache_dir: dir.path().join("cache"),
                engine_port: 9,
                timing: SupervisorTiming::default(),
                drm_session_dir: dir.path().join("drm"),
                sign_in: SignInProgram {
                    program: "false".into(),
                    args: Vec::new(),
                },
                language: "en-US".into(),
                desktop: false,
                exe_dir: None,
                start: Route::Settings { section: None },
            };
            App::start_with(config, |_| {
                Ok(RustetEngine {
                    program: EngineProgram {
                        program: "/nonexistent/rustet".into(),
                        args: Vec::new(),
                    },
                    port: 9,
                    config_dir: config_dir.clone(),
                    drm: DrmLayout::Installed { legacy: None },
                    libs: EngineLibs::default(),
                    env: Env::from_process(),
                    proc_root: "/proc".into(),
                    tls_identity: None,
                })
            })
            .await
            .unwrap()
        });
        Self {
            runtime,
            app,
            prefs,
            dir,
        }
    }

    fn pref(&self, key: &str) -> Option<Value> {
        self.prefs.load().extra.get(key).cloned()
    }

    /// The encrypted store's value of `key`, after the app has stopped (and written it).
    fn stored_after_stop(self, key: &str) -> Option<Value> {
        self.runtime.block_on(self.app.stop());
        let config = self.dir.path().join("config");
        self.runtime
            .block_on(Store::open(&config, StoreCipher::Plain))
            .get(key)
    }
}

struct Root(gpui::AnyView);

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rustet_ui::focus::traverse(div())
            .size_full()
            .child(self.0.clone())
    }
}

fn open<'a>(
    f: &Fixture,
    cx: &'a mut TestAppContext,
) -> (Entity<SettingsPage>, &'a mut VisualTestContext) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        cx.set_global(Theme::new(Appearance::Dark));
        components::bind_keys(cx);
        rustet_ui::focus::bind_keys(cx);
    });
    let app = f.app.clone();
    let (root, cx) = cx.add_window_view(|window, cx| {
        let page = rustet_ui_panels::settings_page(&app, window, cx);
        Root(page)
    });
    let page = root.read_with(cx, |r, _| r.0.clone().downcast::<SettingsPage>().unwrap());
    (page, cx)
}

/// Lets the app's runtime and the page's watches catch up.
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(20));
        cx.run_until_parked();
    }
}

fn go(f: &Fixture, section: Section, page: &Entity<SettingsPage>, cx: &mut VisualTestContext) {
    f.app.nav().go(Route::Settings {
        section: Some(section.slug().to_owned()),
    });
    settle(cx);
    assert_eq!(cx.update(|_, cx| preview::section(page, cx)), section);
}

fn focus(page: &Entity<SettingsPage>, control: &str, cx: &mut VisualTestContext) {
    assert!(
        cx.update(|window, cx| preview::focus_control(page, control, window, cx)),
        "{control}"
    );
    cx.run_until_parked();
}

/// A key pressed and released: a focused element is clicked on release.
fn press(key: &'static str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(key);
    cx.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse(key).unwrap(),
    });
    cx.run_until_parked();
}

#[gpui::test]
fn each_kind_of_control_writes_its_key(cx: &mut TestAppContext) {
    let f = Fixture::new();
    let (page, cx) = open(&f, cx);
    settle(cx);

    // A select: closed, Down takes the next choice.
    go(&f, Section::Playback, &page, cx);
    focus(&page, "quality", cx);
    cx.simulate_keystrokes("down");
    assert_eq!(f.pref("streaming-quality"), Some(json!("hi-res-lossless")));

    // A slider: one step right from the 500 MB default, snapped to 100 MB.
    go(&f, Section::Storage, &page, cx);
    focus(&page, "song-cache", cx);
    cx.simulate_keystrokes("right");
    assert_eq!(f.pref("persistLimitMB"), Some(json!(1000)));

    // A text field: every edit is written.
    go(&f, Section::Downloads, &page, cx);
    focus(&page, "track-template", cx);
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("{title}");
    cx.run_until_parked();
    assert_eq!(f.pref("dl-filename-track"), Some(json!("{title}")));

    // A toggle: the next control after the field, flipped by Space.
    cx.update(|window, cx| window.focus_next(cx));
    press("space", cx);
    assert_eq!(f.pref("explicit-enabled"), Some(json!(false)));

    // A select that saves seconds.
    focus(&page, "retry-delay", cx);
    cx.simulate_keystrokes("down");
    assert_eq!(f.pref("retry-timeout"), Some(json!(60)));

    // Zoom is a typed prefs field.
    go(&f, Section::Appearance, &page, cx);
    focus(&page, "zoom", cx);
    cx.simulate_keystrokes("down");
    assert_eq!(f.prefs.load().zoom_factor, Some(1.1));
    assert_eq!(
        cx.update(|_, cx| rustet_ui::theme::ActiveTheme::theme(cx).zoom),
        1.1,
        "applied at once"
    );
}

#[gpui::test]
fn a_refused_value_says_why_and_keeps_the_stored_one(cx: &mut TestAppContext) {
    let f = Fixture::new();
    let (page, cx) = open(&f, cx);
    go(&f, Section::Downloads, &page, cx);
    focus(&page, "album-template", cx);
    cx.simulate_keystrokes("ctrl-a backspace");
    let why = cx.update(|_, cx| preview::refusal(&page, "album-template", cx));
    assert!(why.is_some_and(|w| w.starts_with("Enter a folder pattern")));
    assert_eq!(f.pref("dl-dirname-album"), None, "nothing was written");
    assert_eq!(
        f.app.settings().get().downloads.album_template,
        "{album_artist}/{year} - {album}"
    );

    // Typing a value clears the refusal.
    cx.simulate_input("{album}");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_, cx| preview::refusal(&page, "album-template", cx)),
        None
    );
    assert_eq!(f.pref("dl-dirname-album"), Some(json!("{album}")));
}

#[gpui::test]
fn the_shortcut_recorder_takes_the_next_key(cx: &mut TestAppContext) {
    let f = Fixture::new();
    let (page, cx) = open(&f, cx);
    go(&f, Section::Shortcuts, &page, cx);
    let record = |action, cx: &mut VisualTestContext| {
        cx.update(|window, cx| preview::record_shortcut(&page, action, window, cx));
        cx.run_until_parked();
    };

    // Escape stops recording and changes nothing.
    record(ShortcutAction::SeekBack, cx);
    cx.simulate_keystrokes("escape");
    assert_eq!(cx.update(|_, cx| preview::recording(&page, cx)), None);

    // Space is Play / Pause's: refused with the conflict named, still recording.
    record(ShortcutAction::SeekBack, cx);
    cx.simulate_keystrokes("space");
    let why = cx
        .update(|_, cx| preview::refusal(&page, "key-seekBack", cx))
        .unwrap();
    assert_eq!(
        why.as_ref(),
        "Space is already used by Play / Pause. Press another key."
    );
    assert_eq!(
        cx.update(|_, cx| preview::recording(&page, cx)),
        Some(ShortcutAction::SeekBack)
    );

    // A modifier is refused too.
    cx.simulate_keystrokes("ctrl-j");
    let why = cx
        .update(|_, cx| preview::refusal(&page, "key-seekBack", cx))
        .unwrap();
    assert!(why.starts_with("Shortcuts are single keys"));

    // A free key is taken and recording ends.
    cx.simulate_keystrokes("j");
    assert_eq!(cx.update(|_, cx| preview::recording(&page, cx)), None);
    assert_eq!(
        cx.update(|_, cx| preview::refusal(&page, "key-seekBack", cx)),
        None
    );
    let bindings = f.app.settings().get().shortcuts.bindings;
    assert_eq!(bindings[&ShortcutAction::SeekBack].0, "j");

    // Saved in the encrypted store as the page did: a JSON string under `hotkeyBindings`.
    let saved = f.stored_after_stop("hotkeyBindings").expect("saved");
    let saved: Value = serde_json::from_str(saved.as_str().unwrap()).unwrap();
    assert_eq!(saved["seekBack"], json!("j"));
    assert_eq!(saved["playPause"], json!(" "));
}

#[gpui::test]
fn tab_moves_through_a_settings_form_in_visual_order(cx: &mut TestAppContext) {
    let f = Fixture::new();
    let (page, cx) = open(&f, cx);
    go(&f, Section::Downloads, &page, cx);
    let focused = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| preview::focused_control(&page, window, cx))
    };
    focus(&page, "folder", cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(
        focused(cx),
        None,
        "the Choose button is between the folder and the album folder"
    );
    cx.simulate_keystrokes("tab");
    assert_eq!(focused(cx), Some("album-template"));
    cx.simulate_keystrokes("tab");
    assert_eq!(focused(cx), Some("track-template"));
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(focused(cx), Some("album-template"));
    // Back over the Choose button to the folder.
    cx.simulate_keystrokes("shift-tab shift-tab");
    assert_eq!(focused(cx), Some("folder"));
}
