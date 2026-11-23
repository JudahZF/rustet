//! Theme state (`theme-mode.mjs`, `theme-presets.mjs`, the theme handlers of `main.mjs`, and the
//! sidebar glass rules of `src/engine/sidebar-glass.js`).
//!
//! Everything here produces data: which theme applies, the CSS variables of a palette, the tweak
//! CSS. Injecting it into the page is the shell's job.

pub mod acrylic;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::prefs::{Prefs, PrefsFile, js_truthy, number_like};
use acrylic::{ACRYLIC_MATERIALS, AcrylicMaterial, AcrylicSettings};

// ── Theme mode ───────────────────────────────────────────────────────────────

/// The theme in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// No theme CSS; the glass look from the base styles over the blurred wallpaper.
    Blur,
    /// The saved colour palette (the Rustet preset unless the user chose another).
    Accent,
    /// The user's own CSS file.
    Custom,
}

impl ThemeMode {
    /// One of the three names the page's buttons send, or `None`.
    pub fn parse(v: &Value) -> Option<Self> {
        serde_json::from_value(v.clone()).ok()
    }
}

/// Which theme mode is in effect: the saved choice, else the accent palette. Blur works on
/// every desktop, so a saved choice is always honoured. (The web shell forced blur until
/// sign-in and defaulted to it on Hyprland; Rustet's own UI is opaque unless the user asks.)
pub fn resolve_theme_mode(saved: Option<ThemeMode>) -> ThemeMode {
    saved.unwrap_or(ThemeMode::Accent)
}

// ── Palettes and presets ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Light,
    Dark,
}

/// A colour palette as saved. Values are colour strings; a saved entry of another type loads
/// as unset. Palettes are fixed colour sets, never derived from an accent colour.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Palette {
    #[serde(
        default,
        deserialize_with = "lenient_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub accent: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient_appearance",
        skip_serializing_if = "Option::is_none"
    )]
    pub appearance: Option<Appearance>,
    #[serde(
        default,
        deserialize_with = "lenient_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub bg_color: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub nav_bg: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub nav_border: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub accent_active: Option<String>,
}

fn lenient_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        _ => None,
    })
}

fn lenient_appearance<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Appearance>, D::Error> {
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}

/// A named palette. A preset saved while no palette was set has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_palette"
    )]
    pub palette: Option<Palette>,
}

fn lenient_palette<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Palette>, D::Error> {
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}

/// The name of the default preset: Rustet's own palette (native-UI spec §6.3).
pub const DEFAULT_PRESET: &str = "Rustet";
/// The web player's colours, kept as a preset a user can choose.
pub const APPLE_MUSIC_PRESET: &str = "Apple Music";
/// The built-in presets, in the order Settings lists them.
pub const BUILTIN_PRESETS: [&str; 2] = [DEFAULT_PRESET, APPLE_MUSIC_PRESET];

/// The default theme: the neutral Apple Music look, dark (`docs/native-ui-look.md`). The
/// accent is the sidebar icon tint; the native UI reads this accent as "Apple's tint", which
/// is `#007AFF` in light mode.
pub fn default_palette() -> Palette {
    Palette {
        accent: Some("#5ac8f5".into()),
        appearance: Some(Appearance::Dark),
        bg_color: Some("#1f1f1f".into()),
        nav_bg: Some("#232324".into()),
        nav_border: Some("#2e2e2f".into()),
        accent_active: Some("hsla(197, 88%, 66%, 0.28)".into()),
    }
}

/// The Apple Music preset.
pub fn apple_music_palette() -> Palette {
    Palette {
        accent: Some("#fc3c44".into()),
        appearance: Some(Appearance::Dark),
        bg_color: Some("#000000".into()),
        nav_bg: Some("#303030".into()),
        nav_border: Some("#3b3b3b".into()),
        accent_active: Some("hsla(358, 87%, 60%, 0.28)".into()),
    }
}

/// The built-in preset called `name`.
pub fn builtin_preset(name: &str) -> Option<Palette> {
    match name {
        DEFAULT_PRESET => Some(default_palette()),
        APPLE_MUSIC_PRESET => Some(apple_music_palette()),
        _ => None,
    }
}

/// The pref that makes light or dark follow the desktop's setting (Settings → Appearance,
/// "System"). It is not part of a palette: a preset keeps its own light or dark for when the
/// user stops following the system.
pub const FOLLOW_SYSTEM_PREF: &str = "appearanceFollowsSystem";

/// Whether light or dark follows the desktop.
pub fn follows_system(p: &Prefs) -> bool {
    p.extra.get(FOLLOW_SYSTEM_PREF) == Some(&Value::Bool(true))
}

/// Bump when saved theme data needs a one-time reset.
pub const THEME_VERSION: u32 = 3;

/// The one-time resets of saved theme data, from the profile's version. Returns whether prefs
/// changed. User presets and the chosen mode are always kept.
///
/// - Before 2: earlier versions generated a palette from an accent colour on first run and
///   saved it. It is dropped.
/// - Before 3: the Electron app saved its default palette (Apple Music's) on first run. A saved
///   palette equal to it is dropped, so the current default applies; any other palette was
///   chosen and stays.
pub fn migrate_theme_prefs(prefs: &mut Prefs) -> bool {
    let version = prefs.theme_version.unwrap_or(0);
    if version >= THEME_VERSION {
        return false;
    }
    if version < 2 {
        prefs.theme_palette = None;
        prefs.extra.remove("themeAppearance");
    }
    if prefs.theme_palette == Some(apple_music_palette()) {
        prefs.theme_palette = None;
    }
    prefs.theme_version = Some(THEME_VERSION);
    true
}

/// Colour syntax only (`^[#\w\s().,%/+-]{0,100}$`, no `url(` or `expression(`): no quotes,
/// semicolons, braces or backslashes can get through.
fn safe_color(v: &str) -> bool {
    let chars_ok = v
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c.is_whitespace() || "#_().,%/+-".contains(c));
    let lower = v.to_ascii_lowercase();
    let calls = |name: &str| {
        lower
            .match_indices(name)
            .any(|(i, _)| lower[i + name.len()..].trim_start().starts_with('('))
    };
    chars_ok && v.encode_utf16().count() <= 100 && !calls("url") && !calls("expression")
}

/// Reduces an untrusted palette (imported file, old prefs) to known keys with safe colour
/// values. Returns `None` if nothing usable remains.
pub fn sanitize_palette(raw: &Value) -> Option<Palette> {
    let obj = raw.as_object()?;
    let color = |k: &str| {
        obj.get(k)
            .and_then(Value::as_str)
            .filter(|v| safe_color(v))
            .map(|v| v.trim().to_owned())
    };
    let out = Palette {
        accent: color("accent"),
        appearance: obj
            .get("appearance")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        bg_color: color("bgColor"),
        nav_bg: color("navBg"),
        nav_border: color("navBorder"),
        accent_active: color("accentActive"),
    };
    (out != Palette::default()).then_some(out)
}

/// Validates an imported preset file. The name is trimmed, 1–64 characters, and not a
/// built-in preset's.
pub fn sanitize_preset(raw: &Value) -> Option<Preset> {
    let obj = raw.as_object()?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if name.is_empty() || name.encode_utf16().count() > 64 || builtin_preset(name).is_some() {
        return None;
    }
    let palette = sanitize_palette(obj.get("palette")?)?;
    Some(Preset {
        name: name.to_owned(),
        palette: Some(palette),
    })
}

/// One validated `theme:set-palette` entry (`_paletteEntryOk`).
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteEntry {
    Accent(String),
    BgColor(String),
    NavBg(String),
    NavBorder(String),
    AccentActive(String),
    Appearance(Appearance),
}

impl PaletteEntry {
    /// A known key with a colour-looking value (`^[#a-zA-Z0-9(),.%\s/-]{1,64}$`), or
    /// `appearance` with `light` / `dark`.
    pub fn parse(key: &Value, value: &Value) -> Option<Self> {
        let (key, value) = (key.as_str()?, value.as_str()?);
        if key == "appearance" {
            return serde_json::from_value(Value::from(value))
                .ok()
                .map(Self::Appearance);
        }
        let ok = (1..=64).contains(&value.encode_utf16().count())
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c.is_whitespace() || "#(),.%/-".contains(c));
        if !ok {
            return None;
        }
        let v = value.to_owned();
        Some(match key {
            "accent" => Self::Accent(v),
            "bgColor" => Self::BgColor(v),
            "navBg" => Self::NavBg(v),
            "navBorder" => Self::NavBorder(v),
            "accentActive" => Self::AccentActive(v),
            _ => return None,
        })
    }

    fn apply(self, p: &mut Palette) {
        match self {
            Self::Accent(v) => p.accent = Some(v),
            Self::BgColor(v) => p.bg_color = Some(v),
            Self::NavBg(v) => p.nav_bg = Some(v),
            Self::NavBorder(v) => p.nav_border = Some(v),
            Self::AccentActive(v) => p.accent_active = Some(v),
            Self::Appearance(v) => p.appearance = Some(v),
        }
    }
}

/// A non-empty string of at most `max` characters (`_str`).
pub fn valid_str(v: &Value, max: usize) -> Option<&str> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.encode_utf16().count() <= max)
}

/// A file-name-safe version of `v`: path separators and anything outside `[\w .()-]` become `_`,
/// at most 64 characters, `preset` when empty.
pub fn safe_name(v: &str) -> String {
    let mut out = String::new();
    for c in v.chars() {
        if c.is_ascii_alphanumeric() || " _.()-".contains(c) {
            out.push(c);
        } else {
            // JavaScript replaces each UTF-16 unit.
            out.extend(std::iter::repeat_n('_', c.len_utf16()));
        }
    }
    out.truncate(64);
    if out.is_empty() { "preset".into() } else { out }
}

// ── CSS variables ────────────────────────────────────────────────────────────

/// `color` with its alpha replaced: `hsl(a)(h, s, l[, a])` and `#rrggbb` are rewritten, other
/// forms are returned unchanged, a missing colour is empty.
pub fn with_alpha(color: &str, alpha: f64) -> String {
    if let Some(inner) = color
        .strip_prefix("hsla(")
        .or_else(|| color.strip_prefix("hsl("))
        .and_then(|r| r.strip_suffix(')'))
    {
        if inner.is_empty() {
            return color.to_owned();
        }
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() < 3 {
            return color.to_owned();
        }
        return format!("hsla({}, {}, {}, {alpha})", parts[0], parts[1], parts[2]);
    }
    if color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        let ch = |i: usize| u8::from_str_radix(&color[i..i + 2], 16).unwrap_or(0);
        return format!("rgba({},{},{},{alpha})", ch(1), ch(3), ch(5));
    }
    color.to_owned()
}

/// Every CSS variable an accent palette sets; clearing the theme removes them all.
pub const THEME_VAR_NAMES: [&str; 7] = [
    "--rustet-nav-bg",
    "--rustet-nav-border",
    "--rustet-accent",
    "--rustet-accent-active",
    "--rustet-nav-text",
    "--rustet-nav-text-sec",
    "--rustet-bg-color",
];

/// What applying a palette sets in the page (`_applyThemeVars`).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeVars {
    /// The native colour scheme to use.
    pub appearance: Appearance,
    /// Each variable of [`THEME_VAR_NAMES`] with its value. An empty value means remove the
    /// property: an empty custom property would defeat `var(--x, fallback)`.
    pub vars: Vec<(&'static str, String)>,
    /// Background of the accent backdrop layer.
    pub background: String,
}

/// The CSS variables of a saved palette. A palette that fails [`sanitize_palette`] falls back to
/// the default, so a poisoned saved palette cannot break startup. `nav_alpha` is the saved
/// `themeNavBgAlpha` (default 0.72).
pub fn theme_vars(palette: &Palette, nav_alpha: Option<f64>) -> ThemeVars {
    let p = serde_json::to_value(palette)
        .ok()
        .and_then(|v| sanitize_palette(&v))
        .unwrap_or_else(default_palette);
    let light = p.appearance == Some(Appearance::Light);
    let or_empty = |v: &Option<String>| v.clone().unwrap_or_default();
    let bg = p
        .bg_color
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            (if light {
                "hsl(0,0%,96%)"
            } else {
                "hsl(0,0%,10%)"
            })
            .into()
        });
    let nav_bg = {
        let raw = or_empty(&p.nav_bg);
        let tinted = if raw.is_empty() {
            String::new()
        } else {
            with_alpha(&raw, nav_alpha.unwrap_or(0.72))
        };
        if tinted.is_empty() { raw } else { tinted }
    };
    let text = |dark_value: &str| {
        if light {
            dark_value.to_owned()
        } else {
            String::new()
        }
    };
    ThemeVars {
        appearance: if light {
            Appearance::Light
        } else {
            Appearance::Dark
        },
        vars: vec![
            ("--rustet-nav-bg", nav_bg),
            ("--rustet-nav-border", or_empty(&p.nav_border)),
            ("--rustet-accent", or_empty(&p.accent)),
            ("--rustet-accent-active", or_empty(&p.accent_active)),
            ("--rustet-nav-text", text("rgba(0,0,0,0.85)")),
            ("--rustet-nav-text-sec", text("rgba(0,0,0,0.5)")),
            ("--rustet-bg-color", bg.clone()),
        ],
        background: bg,
    }
}

/// The settings panel's hide toggles as CSS (`buildTweakCSS`). The upsell and preview badge are
/// hidden unless explicitly turned off; the Radio item only when turned on.
pub fn build_tweak_css(p: &Prefs) -> String {
    let off = |k: &str| p.extra.get(k) == Some(&Value::Bool(false));
    [
        if off("hideUpsell") { "" } else { ".native-cta__button,.web-navigation__auth,cwc-upsell-banner,.locale-switcher-banner{display:none!important}" },
        if off("hidePreviewBadge") { "" } else { r#"cwc-badge[kind="preview"],.preview-badge,.web-chrome-playback-lcd__preview-badge{display:none!important}"# },
        if p.extra.get("hideRadio").is_some_and(js_truthy) { ".navigation-item__radio{display:none!important}" } else { "" },
    ]
    .join("\n")
}

// ── Sidebar glass (sidebar-glass.js) ─────────────────────────────────────────

/// A slider's range and default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlassRange {
    pub def: f64,
    pub min: f64,
    pub max: f64,
}

pub const GLASS_BLUR: GlassRange = GlassRange {
    def: 20.0,
    min: 0.0,
    max: 80.0,
};
pub const GLASS_OPACITY: GlassRange = GlassRange {
    def: 0.07,
    min: 0.0,
    max: 0.5,
};
pub const NAV_ALPHA: GlassRange = GlassRange {
    def: 0.72,
    min: 0.0,
    max: 1.0,
};
/// Album-art theming colours the sidebar with this alpha when the veil is at its default.
pub const ART_NAV_ALPHA: f64 = 0.6;

/// A finite number inside the range, else the default. Numeric strings are accepted (prefs are
/// JSON); 0 is a real value.
pub fn clamp_to(r: GlassRange, v: &Value) -> f64 {
    number_like(v).map_or(r.def, |n| n.clamp(r.min, r.max))
}

fn hundredths(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

fn opt_value(v: Option<f64>) -> Value {
    v.map_or(Value::Null, Value::from)
}

pub fn clean_blur(v: &Value) -> f64 {
    clamp_to(GLASS_BLUR, v).round()
}

pub fn clean_glass_opacity(v: &Value) -> f64 {
    hundredths(clamp_to(GLASS_OPACITY, v))
}

pub fn clean_nav_alpha(v: &Value) -> f64 {
    hundredths(clamp_to(NAV_ALPHA, v))
}

/// What the Sidebar opacity slider edits in a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarOpacity {
    /// Accent: the palette's sidebar colour at `themeNavBgAlpha`.
    NavBgAlpha,
    /// Blur and custom CSS: the white veil over the blur at `glassOpacity`.
    GlassOpacity,
}

impl SidebarOpacity {
    pub fn for_mode(mode: ThemeMode) -> Self {
        if mode == ThemeMode::Accent {
            Self::NavBgAlpha
        } else {
            Self::GlassOpacity
        }
    }

    pub const fn pref(self) -> &'static str {
        match self {
            Self::NavBgAlpha => "themeNavBgAlpha",
            Self::GlassOpacity => "glassOpacity",
        }
    }

    pub fn range(self) -> GlassRange {
        match self {
            Self::NavBgAlpha => NAV_ALPHA,
            Self::GlassOpacity => GLASS_OPACITY,
        }
    }

    pub fn clean(self, v: &Value) -> f64 {
        match self {
            Self::NavBgAlpha => clean_nav_alpha(v),
            Self::GlassOpacity => clean_glass_opacity(v),
        }
    }
}

/// Alpha for album-art theming's sidebar colour (`--rustet-nav-alpha`), following the same slider.
pub fn art_nav_alpha(mode: ThemeMode, p: &Prefs) -> f64 {
    if mode == ThemeMode::Accent {
        return clean_nav_alpha(&opt_value(p.theme_nav_bg_alpha));
    }
    let veil = clean_glass_opacity(&opt_value(p.glass_opacity));
    hundredths(ART_NAV_ALPHA * veil / GLASS_OPACITY.def).min(1.0)
}

// ── Theme service ────────────────────────────────────────────────────────────

/// What the shell applies for the theme in effect (`applyThemeMode`, `theme:set-mode`).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeRender {
    pub mode: ThemeMode,
    /// Accent: the palette's variables. Otherwise the variables are cleared.
    pub vars: Option<ThemeVars>,
    /// Custom: the user's CSS file, when it exists and is readable. The shell also shows the
    /// solid fallback background in custom mode.
    pub custom_css: Option<String>,
    /// `--rustet-nav-alpha`.
    pub nav_alpha: f64,
    /// Light or dark follows the desktop ([`FOLLOW_SYSTEM_PREF`]); the palette's own
    /// appearance applies otherwise.
    pub follows_system: bool,
}

/// The glass slider values to set as `--rustet-glass-blur` (px) and `--rustet-glass-opacity`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glass {
    pub blur: f64,
    pub opacity: f64,
}

/// Result of the Sidebar opacity slider.
#[derive(Debug, Clone, PartialEq)]
pub enum NavOpacityChange {
    /// Accent: re-apply these palette variables (`None` when no palette is saved).
    Palette(Option<ThemeVars>),
    /// Blur and custom: new glass values.
    Glass(Glass),
}

/// The view settings restored when the page loads (`applyPersistedViewSettings`).
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedView {
    /// Zoom factor to set, when not 1.
    pub zoom: Option<f64>,
    pub glass: Glass,
    /// `--rustet-bg-blur` in px, when saved.
    pub bg_blur: Option<f64>,
    pub tweak_css: String,
    pub theme: ThemeRender,
}

/// A preset file to save: the dialog's default name and the file's text.
#[derive(Debug, Clone, PartialEq)]
pub struct PresetExport {
    pub default_file_name: String,
    pub contents: String,
}

/// The Windows backdrop as `theme:get-info` reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AcrylicInfo {
    #[serde(flatten)]
    pub settings: AcrylicSettings,
    pub materials: Vec<AcrylicMaterial>,
    pub defaults: AcrylicSettings,
}

/// `theme:get-info`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeInfo {
    pub desktop: String,
    pub blur_available: bool,
    pub theme_mode: ThemeMode,
    pub theme_palette: Option<Palette>,
    pub default_palette: Palette,
    pub theme_presets: Vec<Value>,
    pub custom_css_path: Option<String>,
    pub acrylic: Option<AcrylicInfo>,
}

/// The theme state of the main window: the prefs, and whether the page reports a signed-in
/// account (remembered in prefs so a signed-in user gets no flash of the signed-out look).
#[derive(Debug)]
pub struct ThemeService {
    prefs: Arc<PrefsFile>,
    signed_in: AtomicBool,
}

impl ThemeService {
    /// Runs the one-time theme migration and reads the remembered sign-in state.
    pub fn new(prefs: Arc<PrefsFile>) -> io::Result<Self> {
        let mut p = prefs.load();
        if migrate_theme_prefs(&mut p) {
            prefs.save(&p)?;
        }
        Ok(Self {
            signed_in: AtomicBool::new(p.signed_in == Some(true)),
            prefs,
        })
    }

    pub fn signed_in(&self) -> bool {
        self.signed_in.load(Ordering::SeqCst)
    }

    /// The theme in effect for `p`.
    pub fn mode(&self, p: &Prefs) -> ThemeMode {
        resolve_theme_mode(p.theme_mode)
    }

    /// `app:auth-state`: only `true` counts as signed in. Returns the theme to apply when the
    /// state changed.
    pub fn set_signed_in(&self, signed_in: &Value) -> io::Result<Option<ThemeRender>> {
        let on = signed_in == &Value::Bool(true);
        if self.signed_in.swap(on, Ordering::SeqCst) == on {
            return Ok(None);
        }
        self.prefs.update(|p| p.signed_in = Some(on))?;
        self.render().map(Some)
    }

    /// The theme in effect (`applyThemeMode`). With no saved palette it is the default
    /// palette, which is not saved: a later default reaches users who never chose one.
    pub fn render(&self) -> io::Result<ThemeRender> {
        let p = self.prefs.load();
        Ok(self.render_mode(self.mode(&p), &p))
    }

    /// Settings → Appearance "System": light or dark follows the desktop.
    pub fn set_follows_system(&self, on: bool) -> io::Result<ThemeRender> {
        let p = self.prefs.update(|p| {
            p.extra.insert(FOLLOW_SYSTEM_PREF.into(), Value::Bool(on));
            p.clone()
        })?;
        Ok(self.render_mode(self.mode(&p), &p))
    }

    fn render_mode(&self, mode: ThemeMode, p: &Prefs) -> ThemeRender {
        ThemeRender {
            mode,
            vars: (mode == ThemeMode::Accent).then(|| {
                theme_vars(
                    &p.theme_palette.clone().unwrap_or_else(default_palette),
                    p.theme_nav_bg_alpha,
                )
            }),
            custom_css: (mode == ThemeMode::Custom)
                .then(|| {
                    p.custom_css_path
                        .as_deref()
                        .and_then(|f| read_custom_css(Path::new(f)))
                })
                .flatten(),
            nav_alpha: art_nav_alpha(self.mode(p), p),
            follows_system: follows_system(p),
        }
    }

    /// `theme:set-mode`: one of the three modes, else `None`. The chosen mode applies directly
    /// (the page only offers it once signed in).
    pub fn set_mode(&self, mode: &Value) -> io::Result<Option<ThemeRender>> {
        let Some(mode) = ThemeMode::parse(mode) else {
            return Ok(None);
        };
        let p = self.prefs.update(|p| {
            p.theme_mode = Some(mode);
            if mode == ThemeMode::Accent {
                p.theme_palette.get_or_insert_with(default_palette);
            }
            p.clone()
        })?;
        Ok(Some(self.render_mode(mode, &p)))
    }

    /// `theme:set-palette`: one validated entry. Returns the variables to apply.
    pub fn set_palette_entry(&self, key: &Value, value: &Value) -> io::Result<Option<ThemeVars>> {
        let Some(entry) = PaletteEntry::parse(key, value) else {
            return Ok(None);
        };
        let p = self.prefs.update(|p| {
            entry.apply(p.theme_palette.get_or_insert_with(default_palette));
            p.clone()
        })?;
        Ok(p.theme_palette
            .as_ref()
            .map(|pal| theme_vars(pal, p.theme_nav_bg_alpha)))
    }

    /// `theme:reset-palette`: back to the built-in palette.
    pub fn reset_palette(&self) -> io::Result<(Palette, ThemeVars)> {
        let p = self.prefs.update(|p| {
            p.theme_palette = Some(default_palette());
            p.clone()
        })?;
        Ok((
            default_palette(),
            theme_vars(&default_palette(), p.theme_nav_bg_alpha),
        ))
    }

    /// `theme:import-css` after the file dialog: remembers the file and returns its text.
    pub fn set_custom_css(&self, file: &Path) -> io::Result<Option<String>> {
        self.prefs
            .update(|p| p.custom_css_path = Some(file.to_string_lossy().into_owned()))?;
        Ok(read_custom_css(file))
    }

    /// `theme:save-preset`: saves the current palette under `name` (1–64 characters, replacing
    /// a preset of that name). Returns the user presets.
    pub fn save_preset(&self, name: &Value) -> io::Result<Vec<Preset>> {
        let Some(name) = valid_str(name, 64) else {
            return Ok(self.prefs.load().theme_presets.unwrap_or_default());
        };
        self.prefs.update(|p| {
            let palette = p.theme_palette.clone();
            let presets = p.theme_presets.get_or_insert_with(Vec::new);
            presets.retain(|x| x.name != name);
            presets.push(Preset {
                name: name.to_owned(),
                palette,
            });
            presets.clone()
        })
    }

    /// `theme:delete-preset`.
    pub fn delete_preset(&self, name: &Value) -> io::Result<()> {
        let Some(name) = valid_str(name, 64) else {
            return Ok(());
        };
        self.prefs.update(|p| {
            p.theme_presets
                .get_or_insert_with(Vec::new)
                .retain(|x| x.name != name)
        })
    }

    /// `theme:apply-preset`: a built-in or user preset. Returns the variables to apply.
    pub fn apply_preset(&self, name: &Value) -> io::Result<Option<ThemeVars>> {
        let Some(name) = valid_str(name, 64) else {
            return Ok(None);
        };
        let p = self.prefs.load();
        let palette = builtin_preset(name).or_else(|| {
            p.theme_presets
                .iter()
                .flatten()
                .find(|x| x.name == name)
                .and_then(|x| x.palette.clone())
        });
        let Some(palette) = palette else {
            return Ok(None);
        };
        let p = self.prefs.update(|p| {
            p.theme_palette = Some(palette.clone());
            p.clone()
        })?;
        Ok(Some(theme_vars(&palette, p.theme_nav_bg_alpha)))
    }

    /// `theme:export-preset`: the named user preset, or the current palette as `current`.
    pub fn export_preset(&self, name: &Value) -> Option<PresetExport> {
        let name = valid_str(name, 64)?;
        let p = self.prefs.load();
        let preset = p
            .theme_presets
            .iter()
            .flatten()
            .find(|x| x.name == name)
            .cloned()
            .unwrap_or_else(|| Preset {
                name: "current".into(),
                palette: p.theme_palette.clone(),
            });
        Some(PresetExport {
            default_file_name: format!("rustet-theme-{}.json", safe_name(&preset.name)),
            contents: serde_json::to_string_pretty(&preset).ok()?,
        })
    }

    /// `theme:import-preset` after the file dialog: a valid preset file is added (replacing one
    /// of the same name). `None` for anything unreadable or invalid.
    pub fn import_preset_file(&self, file: &Path) -> io::Result<Option<Preset>> {
        let Some(preset) = std::fs::read(file)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|v| sanitize_preset(&v))
        else {
            return Ok(None);
        };
        self.prefs.update(|p| {
            let presets = p.theme_presets.get_or_insert_with(Vec::new);
            presets.retain(|x| x.name != preset.name);
            presets.push(preset.clone());
        })?;
        Ok(Some(preset))
    }

    /// `theme:get-info`. `acrylic` is `Some` when the Windows backdrop exists.
    pub fn info(&self, desktop: &str, acrylic: bool) -> ThemeInfo {
        let p = self.prefs.load();
        let builtin = BUILTIN_PRESETS
            .iter()
            .map(|&name| json!({ "name": name, "palette": builtin_preset(name), "builtin": true }));
        let presets = builtin.chain(
            p.theme_presets
                .iter()
                .flatten()
                .filter_map(|x| serde_json::to_value(x).ok()),
        );
        ThemeInfo {
            desktop: if desktop.is_empty() {
                "unknown".into()
            } else {
                desktop.into()
            },
            blur_available: true,
            theme_mode: resolve_theme_mode(p.theme_mode),
            theme_palette: p.theme_palette.clone(),
            default_palette: default_palette(),
            theme_presets: presets.collect(),
            custom_css_path: p.custom_css_path.clone(),
            acrylic: acrylic.then(|| AcrylicInfo {
                settings: p.win_acrylic.clone().unwrap_or_default(),
                materials: ACRYLIC_MATERIALS.to_vec(),
                defaults: AcrylicSettings::default(),
            }),
        }
    }

    /// `acrylic:set`: merges `patch` over the saved settings and validates the result.
    pub fn set_acrylic(&self, patch: &Value) -> io::Result<AcrylicSettings> {
        self.prefs.update(|p| {
            let mut merged =
                serde_json::to_value(p.win_acrylic.clone().unwrap_or_default()).unwrap_or_default();
            if let (Some(base), Some(patch)) = (merged.as_object_mut(), patch.as_object()) {
                base.extend(patch.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            let s = acrylic::normalize_acrylic(&merged);
            p.win_acrylic = Some(s.clone());
            s
        })
    }

    /// `view:glass-blur` and the glass half of the opacity slider (`applyGlassEffect`): cleans,
    /// saves and returns the values.
    pub fn set_glass(&self, blur: &Value, opacity: &Value) -> io::Result<Glass> {
        let glass = Glass {
            blur: clean_blur(blur),
            opacity: clean_glass_opacity(opacity),
        };
        self.prefs.update(|p| {
            p.glass_blur = Some(glass.blur);
            p.glass_opacity = Some(glass.opacity);
        })?;
        Ok(glass)
    }

    /// `view:glass-blur`: a new blur, keeping the saved opacity.
    pub fn set_glass_blur(&self, blur: &Value) -> io::Result<Glass> {
        let opacity = opt_value(self.prefs.load().glass_opacity);
        self.set_glass(blur, &opacity)
    }

    /// `view:nav-opacity`: edits what the current theme's sidebar uses. Also returns the new
    /// `--rustet-nav-alpha`.
    pub fn set_nav_opacity(&self, v: &Value) -> io::Result<(NavOpacityChange, f64)> {
        let p = self.prefs.load();
        let change = match SidebarOpacity::for_mode(self.mode(&p)) {
            SidebarOpacity::NavBgAlpha => {
                let alpha = clean_nav_alpha(v);
                let p = self.prefs.update(|p| {
                    p.theme_nav_bg_alpha = Some(alpha);
                    p.clone()
                })?;
                NavOpacityChange::Palette(
                    p.theme_palette
                        .as_ref()
                        .map(|pal| theme_vars(pal, Some(alpha))),
                )
            }
            SidebarOpacity::GlassOpacity => {
                NavOpacityChange::Glass(self.set_glass(&opt_value(p.glass_blur), v)?)
            }
        };
        let p = self.prefs.load();
        Ok((change, art_nav_alpha(self.mode(&p), &p)))
    }

    /// `view:zoom`: a positive finite factor (number or numeric string) is saved and returned.
    pub fn set_zoom(&self, factor: &Value) -> io::Result<Option<f64>> {
        let Some(f) = number_like(factor).filter(|f| *f > 0.0) else {
            return Ok(None);
        };
        self.prefs.update(|p| p.zoom_factor = Some(f))?;
        Ok(Some(f))
    }

    /// `view:bg-blur`: the integer part of the value (`parseInt`); unparseable clears it. Returns
    /// the px value for `--rustet-bg-blur`. The shell then re-renders the wallpaper blur.
    pub fn set_bg_blur(&self, v: &Value) -> io::Result<Option<f64>> {
        let px = parse_int(v);
        self.prefs.update(|p| p.bg_blur = px)?;
        Ok(px)
    }

    /// Everything restored when the page loads. Saves the cleaned glass values, as before.
    pub fn persisted_view(&self) -> io::Result<PersistedView> {
        let p = self.prefs.load();
        let glass = self.set_glass(
            &opt_value(p.glass_blur.or(Some(GLASS_BLUR.def))),
            &opt_value(p.glass_opacity.or(Some(GLASS_OPACITY.def))),
        )?;
        Ok(PersistedView {
            zoom: p.zoom_factor.filter(|z| *z != 1.0 && *z != 0.0),
            glass,
            bg_blur: p.bg_blur.map(|_| p.bg_blur_px()),
            tweak_css: build_tweak_css(&p),
            theme: self.render()?,
        })
    }
}

/// `parseInt` of a number or a string: the leading integer, or `None`.
fn parse_int(v: &Value) -> Option<f64> {
    let s = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<f64>().ok().map(|n| sign * n)
}

/// The text of a custom CSS file, when it exists and is readable as UTF-8.
pub fn read_custom_css(file: &Path) -> Option<String> {
    std::fs::read_to_string(file).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(prefs: Value) -> (tempfile::TempDir, Arc<PrefsFile>, ThemeService) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(crate::prefs::PREFS_FILE_NAME),
            serde_json::to_vec(&prefs).unwrap(),
        )
        .unwrap();
        let file = Arc::new(PrefsFile::new(d.path()));
        let svc = ThemeService::new(file.clone()).unwrap();
        (d, file, svc)
    }

    #[test]
    fn the_saved_theme_wins_and_the_default_is_the_accent_palette() {
        use ThemeMode::*;
        assert_eq!(resolve_theme_mode(None), Accent);
        for saved in [Accent, Blur, Custom] {
            assert_eq!(resolve_theme_mode(Some(saved)), saved);
        }
    }

    #[test]
    fn the_builtin_presets_are_fixed() {
        let p = default_palette();
        assert_eq!(p.accent.as_deref(), Some("#5ac8f5"));
        assert_eq!(p.bg_color.as_deref(), Some("#1f1f1f"));
        assert_eq!(p.nav_bg.as_deref(), Some("#232324"));
        assert_eq!(p.nav_border.as_deref(), Some("#2e2e2f"));
        assert_eq!(p.appearance, Some(Appearance::Dark));
        assert_eq!(builtin_preset("Rustet"), Some(default_palette()));
        let apple = builtin_preset("Apple Music").unwrap();
        assert_eq!(apple.accent.as_deref(), Some("#fc3c44"));
        assert_eq!(apple.bg_color.as_deref(), Some("#000000"));
        assert!(builtin_preset("apple music").is_none());
    }

    #[test]
    fn migrate_replaces_the_saved_electron_default_and_keeps_a_chosen_palette() {
        let electron_default = json!({
            "accent": "#fc3c44", "appearance": "dark", "bgColor": "#000000", "navBg": "#303030",
            "navBorder": "#3b3b3b", "accentActive": "hsla(358, 87%, 60%, 0.28)",
        });
        let mut prefs: Prefs = serde_json::from_value(json!({
            "themeVersion": 2, "themeMode": "accent", "themePalette": electron_default,
            "themePresets": [{ "name": "mine" }],
        }))
        .unwrap();
        assert!(migrate_theme_prefs(&mut prefs));
        assert_eq!(prefs.theme_palette, None, "the current default applies");
        assert_eq!(prefs.theme_version, Some(THEME_VERSION));
        assert_eq!(prefs.theme_presets.as_ref().unwrap()[0].name, "mine");

        let chosen = Palette {
            accent: Some("#00a0ff".into()),
            ..apple_music_palette()
        };
        let mut prefs = Prefs {
            theme_version: Some(2),
            theme_palette: Some(chosen.clone()),
            ..Prefs::default()
        };
        assert!(migrate_theme_prefs(&mut prefs));
        assert_eq!(
            prefs.theme_palette,
            Some(chosen),
            "one changed colour: the user chose it"
        );
        assert_eq!(prefs.theme_version, Some(THEME_VERSION));
    }

    #[test]
    fn migrate_drops_the_old_generated_palette_once() {
        let mut prefs: Prefs = serde_json::from_value(json!({
            "themeMode": "accent", "themePalette": { "accent": "#00ff00" }, "themeAppearance": "dark",
            "themePresets": [{ "name": "mine" }], "zoomFactor": 1.1,
        }))
        .unwrap();
        assert!(migrate_theme_prefs(&mut prefs));
        assert_eq!(prefs.theme_palette, None);
        assert!(!prefs.extra.contains_key("themeAppearance"));
        assert_eq!(prefs.theme_version, Some(THEME_VERSION));
        assert_eq!(prefs.theme_mode, Some(ThemeMode::Accent));
        assert_eq!(prefs.theme_presets.as_ref().unwrap()[0].name, "mine");
        assert_eq!(prefs.zoom_factor, Some(1.1));
        prefs.theme_palette = Some(Palette {
            accent: Some("#123456".into()),
            ..Palette::default()
        });
        assert!(!migrate_theme_prefs(&mut prefs));
        assert_eq!(
            prefs.theme_palette.unwrap().accent.as_deref(),
            Some("#123456")
        );
    }

    #[test]
    fn well_formed_preset_passes_trimmed_to_known_keys() {
        let p = sanitize_preset(&json!({ "name": " Ocean ", "palette": { "accent": "#0af", "navBg": "hsla(200, 50%, 20%, 0.7)", "appearance": "light", "evil": "x" } })).unwrap();
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({ "name": "Ocean", "palette": { "accent": "#0af", "appearance": "light", "navBg": "hsla(200, 50%, 20%, 0.7)" } })
        );
    }

    #[test]
    fn hostile_palette_values_are_dropped() {
        let pal = sanitize_palette(
            &json!({ "accent": 123, "navBg": { "x": 1 }, "bgColor": "red; background:url(http://evil)", "navBorder": "url(http://evil)", "accentActive": "#123456" }),
        );
        assert_eq!(
            serde_json::to_value(pal).unwrap(),
            json!({ "accentActive": "#123456" })
        );
        assert_eq!(sanitize_palette(&json!({ "accent": "URL (x)" })), None);
        assert_eq!(
            sanitize_palette(&json!({ "accent": "expression(1)" })),
            None
        );
        assert_eq!(
            sanitize_palette(&json!({ "accent": format!("#{}", "a".repeat(200)) })),
            None
        );
    }

    #[test]
    fn unusable_presets_are_rejected() {
        for bad in [
            Value::Null,
            json!([]),
            json!("x"),
            json!({}),
            json!({ "name": "", "palette": { "accent": "#fff" } }),
            json!({ "name": "n" }),
            json!({ "name": "n", "palette": {} }),
            json!({ "name": "n", "palette": { "accent": 5 } }),
            json!({ "name": "x".repeat(65), "palette": { "accent": "#fff" } }),
            json!({ "name": "Apple Music", "palette": { "accent": "#fff" } }),
            json!({ "name": "__proto__", "palette": [] }),
        ] {
            assert_eq!(sanitize_preset(&bad), None, "{bad}");
        }
    }

    #[test]
    fn palette_entries_known_keys_with_colour_values_only() {
        let ok = |k: &str, v: Value| PaletteEntry::parse(&json!(k), &v).is_some();
        assert!(ok("accent", json!("#fc3c44")));
        assert!(ok("accentActive", json!("hsla(358, 87%, 60%, 0.28)")));
        assert!(ok("appearance", json!("dark")));
        assert!(!ok("__proto__", json!("#fff")));
        assert!(!ok("accent", json!("red; background:url(//evil)")));
        assert!(!ok("accent", json!("#fff".repeat(30))));
        assert!(!ok("accent", json!(42)));
        assert!(!ok("appearance", json!("dark; x")));
    }

    #[test]
    fn valid_str_and_safe_name() {
        assert_eq!(valid_str(&json!("x"), 256), Some("x"));
        assert_eq!(valid_str(&json!(""), 256), None);
        assert_eq!(valid_str(&json!("x".repeat(65)), 64), None);
        assert_eq!(valid_str(&json!(42), 256), None);
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_name("My Theme (v2).json"), "My Theme (v2).json");
        assert_eq!(safe_name("a/b\\c"), "a_b_c");
        assert_eq!(safe_name(&"x".repeat(200)).len(), 64);
        assert_eq!(safe_name(""), "preset");
        assert_eq!(safe_name("a😀"), "a__");
    }

    #[test]
    fn with_alpha_rewrites_hsl_and_hex_only() {
        assert_eq!(
            with_alpha("hsla(358, 87%, 60%, 0.28)", 0.72),
            "hsla(358, 87%, 60%, 0.72)"
        );
        assert_eq!(with_alpha("hsl(1,2%,3%)", 1.0), "hsla(1, 2%, 3%, 1)");
        assert_eq!(with_alpha("#303030", 0.72), "rgba(48,48,48,0.72)");
        assert_eq!(with_alpha("#fff", 0.5), "#fff");
        assert_eq!(with_alpha("hsl(1,2)", 0.5), "hsl(1,2)");
        assert_eq!(with_alpha("", 0.5), "");
    }

    #[test]
    fn theme_vars_of_the_default_and_of_a_poisoned_palette() {
        let v = theme_vars(&default_palette(), None);
        assert_eq!(v.appearance, Appearance::Dark);
        assert_eq!(v.background, "#1f1f1f");
        assert_eq!(v.vars[0], ("--rustet-nav-bg", "rgba(35,35,36,0.72)".into()));
        assert_eq!(v.vars[4], ("--rustet-nav-text", String::new()));
        let poisoned = Palette {
            accent: Some("x;}body{".into()),
            ..Palette::default()
        };
        assert_eq!(
            theme_vars(&poisoned, None),
            v,
            "a poisoned palette falls back to the default"
        );
        let light = Palette {
            appearance: Some(Appearance::Light),
            accent: Some("#111111".into()),
            ..Palette::default()
        };
        let v = theme_vars(&light, Some(0.5));
        assert_eq!(v.background, "hsl(0,0%,96%)");
        assert_eq!(v.vars[4].1, "rgba(0,0,0,0.85)");
        assert_eq!(v.vars[0].1, "", "no sidebar colour: removed");
    }

    #[test]
    fn tweak_css() {
        let p: Prefs = serde_json::from_value(json!({})).unwrap();
        let css = build_tweak_css(&p);
        assert!(
            css.contains("cwc-upsell-banner")
                && css.contains("preview-badge")
                && !css.contains("radio")
        );
        let p: Prefs = serde_json::from_value(
            json!({ "hideUpsell": false, "hidePreviewBadge": 0, "hideRadio": "yes" }),
        )
        .unwrap();
        let css = build_tweak_css(&p);
        assert!(!css.contains("cwc-upsell-banner"));
        assert!(
            css.contains("preview-badge"),
            "only an explicit false shows the badge"
        );
        assert!(css.contains("navigation-item__radio"));
    }

    #[test]
    fn sidebar_glass_rules() {
        assert_eq!(clean_blur(&json!(0)), 0.0, "0 is a real value");
        assert_eq!(clean_blur(&json!("33.6")), 34.0);
        assert_eq!(clean_blur(&json!(500)), 80.0);
        assert_eq!(clean_blur(&json!("")), 20.0);
        assert_eq!(clean_glass_opacity(&json!(0.123)), 0.12);
        assert_eq!(clean_nav_alpha(&Value::Null), 0.72);
        let p: Prefs =
            serde_json::from_value(json!({ "glassOpacity": 0.07, "themeNavBgAlpha": 0.5 }))
                .unwrap();
        assert_eq!(art_nav_alpha(ThemeMode::Accent, &p), 0.5);
        assert_eq!(art_nav_alpha(ThemeMode::Blur, &p), 0.6);
        let p: Prefs = serde_json::from_value(json!({ "glassOpacity": 0.5 })).unwrap();
        assert_eq!(art_nav_alpha(ThemeMode::Blur, &p), 1.0);
        assert_eq!(
            SidebarOpacity::for_mode(ThemeMode::Custom),
            SidebarOpacity::GlassOpacity
        );
    }

    #[test]
    fn service_presets_round_trip() {
        let (d, file, svc) =
            service(json!({ "themeVersion": 2, "themePalette": { "accent": "#111111" } }));
        assert_eq!(svc.save_preset(&json!("Mine")).unwrap().len(), 1);
        assert_eq!(
            svc.save_preset(&json!("Mine")).unwrap().len(),
            1,
            "same name replaces"
        );
        assert_eq!(
            svc.save_preset(&json!("")).unwrap().len(),
            1,
            "invalid name changes nothing"
        );
        let exp = svc.export_preset(&json!("Mine")).unwrap();
        assert_eq!(exp.default_file_name, "rustet-theme-Mine.json");
        let f = d.path().join("p.json");
        std::fs::write(&f, exp.contents.replace("Mine", "Imported")).unwrap();
        assert_eq!(
            svc.import_preset_file(&f).unwrap().unwrap().name,
            "Imported"
        );
        assert!(svc.apply_preset(&json!("Apple Music")).unwrap().is_some());
        assert_eq!(file.load().theme_palette, Some(apple_music_palette()));
        assert!(svc.apply_preset(&json!("Imported")).unwrap().is_some());
        assert_eq!(
            file.load().theme_palette.unwrap().accent.as_deref(),
            Some("#111111")
        );
        svc.delete_preset(&json!("Mine")).unwrap();
        let info = svc.info("hyprland", false);
        assert_eq!(info.theme_presets.len(), 3);
        assert_eq!(info.theme_presets[0]["name"], "Rustet");
        assert_eq!(info.theme_presets[1]["builtin"], true);
        assert_eq!(
            svc.export_preset(&json!("nope")).unwrap().default_file_name,
            "rustet-theme-current.json"
        );
    }

    #[test]
    fn service_modes_and_sign_in() {
        let (_d, file, svc) = service(json!({}));
        assert_eq!(
            file.load().theme_version,
            Some(THEME_VERSION),
            "migration ran"
        );
        let r = svc.render().unwrap();
        assert_eq!(r.mode, ThemeMode::Accent, "the default, signed in or not");
        assert_eq!(
            r.vars,
            Some(theme_vars(&default_palette(), None)),
            "Rustet's palette"
        );
        assert!(!r.follows_system);
        assert!(svc.set_signed_in(&json!(true)).unwrap().is_some());
        assert_eq!(file.load().theme_palette, None, "the default is not saved");
        assert!(svc.set_follows_system(true).unwrap().follows_system);
        assert!(svc.render().unwrap().follows_system);
        assert_eq!(svc.set_signed_in(&json!(true)).unwrap(), None, "unchanged");
        assert_eq!(svc.set_mode(&json!("sepia")).unwrap(), None);
        assert_eq!(svc.set_mode(&json!("blur")).unwrap().unwrap().vars, None);
        assert!(
            svc.set_palette_entry(&json!("accent"), &json!("#222222"))
                .unwrap()
                .is_some()
        );
        assert!(
            svc.set_palette_entry(&json!("accent"), &json!("x;y"))
                .unwrap()
                .is_none()
        );
        let (change, _) = svc.set_nav_opacity(&json!(0.3)).unwrap();
        assert_eq!(
            change,
            NavOpacityChange::Glass(Glass {
                blur: 20.0,
                opacity: 0.3
            })
        );
        assert_eq!(svc.set_zoom(&json!("abc")).unwrap(), None);
        assert_eq!(svc.set_bg_blur(&json!("24px")).unwrap(), Some(24.0));
        assert_eq!(
            svc.set_acrylic(&json!({ "theme": "light", "tintColor": "bad" }))
                .unwrap()
                .theme,
            acrylic::AcrylicTheme::Light
        );
        let view = svc.persisted_view().unwrap();
        assert_eq!(view.bg_blur, Some(24.0));
        assert_eq!(
            view.glass,
            Glass {
                blur: 20.0,
                opacity: 0.3
            }
        );
    }
}
