//! Settings → Appearance. Light, dark or the system's, the preset, window blur and the
//! sidebar opacity are the theme service's (prefs `themeMode`, `themePalette`, …); album-art
//! colours, zoom, Reduce animations and Hide Radio are settings. Every change is applied at
//! once: the kit's theme is rebuilt from the saved values and every window repaints.

use std::io;

use gpui::{
    AnyElement, Context, IntoElement, ParentElement, Styled, Subscription, div, prelude::*,
};
use rustet_desktop::theme::{
    Appearance, BUILTIN_PRESETS, SidebarOpacity, ThemeMode, builtin_preset, default_palette,
};
use rustet_ui::components::Toggle;
use rustet_ui::theme::Theme;
use serde_json::json;

use super::SettingsPage;
use super::controls::{Choice, Range};
use crate::DesktopPrefs;
use crate::ui::{self, group, row};

/// Light, dark, or whatever the desktop uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Dark,
    Light,
    System,
}

/// The theme service's controls, rebuilt when the list of presets or the theme mode changes.
pub struct ThemeControls {
    pub appearance: Choice<Look>,
    pub preset: Choice<usize>,
    pub sidebar: Range,
    /// The names behind the preset options, in their order.
    preset_names: Vec<String>,
    /// The mode they were built for: the sidebar slider's range depends on it.
    built_for: ThemeMode,
    _subs: Vec<Subscription>,
}

/// The preset option for a palette that matches no preset.
const OWN_COLOURS: usize = usize::MAX;

impl SettingsPage {
    /// Rebuilds the kit's theme from the saved values and repaints.
    pub(crate) fn apply_theme(&mut self, cx: &mut Context<Self>) {
        let tp = DesktopPrefs::of(&self.app, cx);
        let Ok(service) = &tp.service else { return };
        match service.render() {
            Ok(render) => {
                let theme = Theme::from_settings(&render, &tp.prefs.load(), cx.window_appearance());
                theme.apply(cx);
            }
            Err(e) => tracing::warn!("theme: {e}"),
        }
    }

    /// A theme service change, then the theme applied and the controls updated.
    fn theme_change<T>(
        &mut self,
        key: &'static str,
        f: impl FnOnce(&rustet_desktop::theme::ThemeService) -> io::Result<T>,
        cx: &mut Context<Self>,
    ) {
        let result = match &DesktopPrefs::of(&self.app, cx).service {
            Ok(s) => f(s).map(drop).map_err(|e| e.to_string()),
            Err(e) => Err(e.clone()),
        };
        match result {
            Ok(()) => {
                self.refused.remove(key);
                self.apply_theme(cx);
            }
            Err(e) => self.refuse(key, format!("Rustet could not save the theme ({e})."), cx),
        }
        self.refresh_theme(cx);
        cx.notify();
    }

    /// Shows the saved theme values; rebuilds the controls when the presets or the mode changed.
    pub(crate) fn refresh_theme(&mut self, cx: &mut Context<Self>) {
        let tp = DesktopPrefs::of(&self.app, cx);
        let Ok(service) = &tp.service else { return };
        let p = tp.prefs.load();
        let mode = service.mode(&p);
        let palette = p.theme_palette.clone().unwrap_or_else(default_palette);
        let appearance = if rustet_desktop::theme::follows_system(&p) {
            Look::System
        } else if palette.appearance == Some(Appearance::Light) {
            Look::Light
        } else {
            Look::Dark
        };
        let presets: Vec<(String, Option<rustet_desktop::theme::Palette>)> = BUILTIN_PRESETS
            .iter()
            .map(|&name| (name.to_owned(), builtin_preset(name)))
            .chain(
                p.theme_presets
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|pr| (pr.name, pr.palette)),
            )
            .collect();
        let current = presets
            .iter()
            .position(|(_, pal)| pal.as_ref() == Some(&palette))
            .unwrap_or(OWN_COLOURS);
        let sidebar = SidebarOpacity::for_mode(mode);
        let range = sidebar.range();
        let opacity = match sidebar {
            SidebarOpacity::NavBgAlpha => p.theme_nav_bg_alpha,
            SidebarOpacity::GlassOpacity => p.glass_opacity,
        }
        .unwrap_or(range.def);
        let names: Vec<String> = presets.iter().map(|(n, _)| n.clone()).collect();
        let mut options: Vec<(usize, gpui::SharedString)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (i, n.clone().into()))
            .collect();
        if current == OWN_COLOURS {
            options.push((OWN_COLOURS, "Your own colours".into()));
        }

        if let Some(c) = &mut self.c.theme
            && c.built_for == mode
        {
            c.appearance.show(appearance, cx);
            c.preset_names = names;
            c.preset.set_options(options, current, cx);
            c.sidebar.show(opacity, cx);
            return;
        }
        let mut subs = Vec::new();
        self.c.theme = Some(ThemeControls {
            appearance: Choice::new(
                "Light or dark",
                vec![
                    (Look::Dark, "Dark".into()),
                    (Look::Light, "Light".into()),
                    (Look::System, "System".into()),
                ],
                appearance,
                |p, look, cx| {
                    let value = match look {
                        Look::System => {
                            return p.theme_change(
                                "appearance",
                                |s| s.set_follows_system(true),
                                cx,
                            );
                        }
                        Look::Light => "light",
                        Look::Dark => "dark",
                    };
                    p.theme_change(
                        "appearance",
                        |s| {
                            s.set_follows_system(false)?;
                            s.set_palette_entry(&json!("appearance"), &json!(value))
                        },
                        cx,
                    );
                },
                &mut subs,
                cx,
            ),
            preset: Choice::new(
                "Theme preset",
                options,
                current,
                |p, ix, cx| {
                    let name =
                        p.c.theme
                            .as_ref()
                            .and_then(|c| c.preset_names.get(ix).cloned());
                    if let Some(name) = name {
                        let name = json!(name);
                        p.theme_change("preset", |s| s.apply_preset(&name), cx);
                    }
                },
                &mut subs,
                cx,
            ),
            sidebar: Range::new(
                "Sidebar opacity",
                (range.min, range.max, 0.01),
                opacity,
                |c| c.theme.as_mut().map(|t| &mut t.sidebar),
                |p, v, cx| p.theme_change("sidebar", |s| s.set_nav_opacity(&json!(v)), cx),
                &mut subs,
                cx,
            ),
            preset_names: names,
            built_for: mode,
            _subs: subs,
        });
    }

    pub(super) fn appearance(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let a = self.settings.appearance;
        let tp = DesktopPrefs::of(&self.app, cx);
        let (mode, broken) = match &tp.service {
            Ok(s) => (Some(s.mode(&tp.prefs.load())), None),
            Err(e) => (None, Some(e.clone())),
        };
        if self.c.theme.is_none() {
            self.refresh_theme(cx);
        }

        let mut theme_rows = Vec::new();
        if let Some(e) = broken {
            theme_rows.push(ui::note(
                format!("Rustet could not open the theme settings ({e})."),
                cx,
            ));
        }
        if let (Some(mode), Some(c)) = (mode, &self.c.theme) {
            let blur = mode == ThemeMode::Blur;
            let page = cx.entity();
            theme_rows.push(
                Toggle::new("blur", "Window blur", blur)
                    .description("The window lets the desktop show through, blurred. Needs a compositor that blurs windows.")
                    .on_change(move |on, _, cx| {
                        page.update(cx, |p, cx| {
                            let mode = if on { "blur" } else { "accent" };
                            p.theme_change("blur", |s| s.set_mode(&json!(mode)), cx);
                        })
                    })
                    .into_any_element(),
            );
            theme_rows.extend(self.refusal_of("blur", cx));
            let colours_note =
                (mode != ThemeMode::Accent).then(|| "Turn off window blur to use these.".into());
            theme_rows.push(row(
                "Light or dark",
                colours_note.clone(),
                dim(c.appearance.select.clone(), mode),
                cx,
            ));
            theme_rows.extend(self.refusal_of("appearance", cx));
            theme_rows.push(row(
                "Theme preset",
                colours_note,
                dim(c.preset.select.clone(), mode),
                cx,
            ));
            theme_rows.extend(self.refusal_of("preset", cx));
            let value = c
                .sidebar
                .preview
                .unwrap_or_else(|| c.sidebar.value_at(c.sidebar.slider.read(cx).value()));
            theme_rows.push(row(
                "Sidebar opacity",
                None,
                ui::slider(&c.sidebar.slider, format!("{:.0} %", value * 100.), cx),
                cx,
            ));
            theme_rows.extend(self.refusal_of("sidebar", cx));
        }

        let mut colour_rows = vec![
            self.toggle(
                "art-colours",
                "Album-art colours",
                a.album_art_colours,
                |s, on| s.appearance.album_art_colours = on,
                cx,
            )
            .description("Pages take their colours from the artwork.")
            .into_any_element(),
        ];
        colour_rows.extend(self.refusal_of("art-colours", cx));

        let mut display = vec![row("Zoom", None, self.c.zoom.select.clone(), cx)];
        display.extend(self.refusal_of("zoom", cx));
        display.push(
            self.toggle(
                "reduce-motion",
                "Reduce animations",
                a.reduce_motion,
                |s, on| s.appearance.reduce_motion = on,
                cx,
            )
            .description("Pages, sheets and artwork appear without motion.")
            .into_any_element(),
        );
        display.extend(self.refusal_of("reduce-motion", cx));
        display.push(
            self.toggle(
                "hide-radio",
                "Hide Radio",
                a.hide_radio,
                |s, on| s.appearance.hide_radio = on,
                cx,
            )
            .description("Removes Radio from the sidebar.")
            .into_any_element(),
        );
        display.extend(self.refusal_of("hide-radio", cx));

        vec![
            group("Theme", theme_rows, cx),
            group("Colours", colour_rows, cx),
            group("Display", display, cx),
        ]
    }
}

/// A theme control that does not apply in the current mode is drawn faded (it still works:
/// the choice is kept for when it applies).
fn dim(control: impl IntoElement, mode: ThemeMode) -> impl IntoElement {
    div()
        .when(mode != ThemeMode::Accent, |d| d.opacity(0.5))
        .child(control)
}
