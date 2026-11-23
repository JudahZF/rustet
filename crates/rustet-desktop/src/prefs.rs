//! Preferences: `electron-prefs.json` in the config directory (`loadPrefs` / `savePrefs`).
//!
//! The keys the shell owns are typed fields. Every other key (the settings panel's generic
//! `view:tweak` keys such as `hideRadio`, the `pref:set` cache limits, anything a newer or older
//! version wrote) lives in [`Prefs::extra`] and is written back unchanged. A typed key whose
//! saved value has the wrong type loads as unset, as the Electron code's fallbacks treated it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::atomic::write_atomic;
use crate::theme::acrylic::AcrylicSettings;
use crate::theme::{Palette, Preset, ThemeMode};

/// The prefs file's name inside the config directory.
pub const PREFS_FILE_NAME: &str = "electron-prefs.json";

/// The saved preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prefs {
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub custom_css_path: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub theme_palette: Option<Palette>,
    #[serde(
        default,
        deserialize_with = "lenient_vec",
        skip_serializing_if = "Option::is_none"
    )]
    pub theme_presets: Option<Vec<Preset>>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub theme_mode: Option<ThemeMode>,
    #[serde(
        default,
        deserialize_with = "lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub theme_nav_bg_alpha: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win_acrylic: Option<AcrylicSettings>,
    #[serde(
        default,
        deserialize_with = "lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub zoom_factor: Option<f64>,
    #[serde(
        default,
        deserialize_with = "lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub glass_blur: Option<f64>,
    #[serde(
        default,
        deserialize_with = "lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub glass_opacity: Option<f64>,
    #[serde(
        default,
        deserialize_with = "lenient_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub bg_blur: Option<f64>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub link_schemes: Option<Vec<Value>>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub link_schemes_applied: Option<Vec<Value>>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub signed_in: Option<bool>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub theme_version: Option<u32>,
    /// The main window's size and position at the last quit (the Electron shell always opened
    /// 1200 × 800 and saved nothing).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub window_bounds: Option<WindowBounds>,
    /// Where the mini player was when it last closed, `[x, y]`.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub mini_player_position: Option<[i32; 2]>,
    /// Every other key, kept as saved.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The main window's saved geometry. `x`/`y` are absent where the window manager places
/// windows itself (Wayland).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds {
    pub width: i32,
    pub height: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default)]
    pub maximized: bool,
}

/// Keys only the shell writes (`_MAIN_OWNED_PREFS`): the page's generic `view:tweak` writer
/// may not touch them. Every typed field of [`Prefs`] is in this list.
pub const MAIN_OWNED_KEYS: [&str; 17] = [
    "customCssPath",
    "themePalette",
    "themePresets",
    "themeMode",
    "themeAppearance",
    "themeNavBgAlpha",
    "winAcrylic",
    "zoomFactor",
    "glassBlur",
    "glassOpacity",
    "bgBlur",
    "linkSchemes",
    "linkSchemesApplied",
    "signedIn",
    "themeVersion",
    "windowBounds",
    "miniPlayerPosition",
];

impl Prefs {
    /// The prefs as the page sees them (`prefs:get`).
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new()))
    }

    /// JavaScript truthiness of an untyped key (`if (p.hideRadio)`).
    pub fn extra_truthy(&self, key: &str) -> bool {
        self.extra.get(key).is_some_and(js_truthy)
    }

    /// Settings → Display → Reduce animations: only an explicit `true` turns it on.
    pub fn reduce_motion(&self) -> bool {
        self.extra.get("reduceMotion") == Some(&Value::Bool(true))
    }

    /// The saved blur strength for the wallpaper (`parseFloat(bgBlur) || 18`, at least 1).
    pub fn bg_blur_px(&self) -> f64 {
        self.bg_blur.filter(|b| *b != 0.0).unwrap_or(18.0).max(1.0)
    }
}

/// JavaScript truthiness of a JSON value.
pub(crate) fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `T` when the saved value has its shape, `None` otherwise.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}

/// The items of a saved list that have `T`'s shape; anything else is dropped.
fn lenient_vec<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    d: D,
) -> Result<Option<Vec<T>>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => Some(
            items
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect(),
        ),
        _ => None,
    })
}

/// A finite number, or a string holding one (prefs written by earlier versions held strings).
fn lenient_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(number_like(&Value::deserialize(d)?))
}

/// A finite number from a JSON number or a numeric string (`Number(v)` for non-blank strings).
pub(crate) fn number_like(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

/// The prefs file. Every read goes to disk, as `loadPrefs()` did; updates are serialised so two
/// callers never lose each other's change.
#[derive(Debug)]
pub struct PrefsFile {
    path: PathBuf,
    lock: Mutex<()>,
}

impl PrefsFile {
    /// The prefs file of the config directory `config_dir`.
    pub fn new(config_dir: &Path) -> Self {
        Self {
            path: config_dir.join(PREFS_FILE_NAME),
            lock: Mutex::new(()),
        }
    }

    /// The saved prefs; empty prefs when the file is missing or unreadable.
    pub fn load(&self) -> Prefs {
        fs::read(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    /// Writes `prefs` atomically (pretty JSON with two-space indentation, as before).
    pub fn save(&self, prefs: &Prefs) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.write(prefs)
    }

    fn write(&self, prefs: &Prefs) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(prefs).map_err(io::Error::other)?;
        write_atomic(&self.path, &json)
    }

    /// Loads, applies `f`, and saves, as one step. The result of `f` is returned once saved.
    pub fn update<R>(&self, f: impl FnOnce(&mut Prefs) -> R) -> io::Result<R> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut prefs = self.load();
        let out = f(&mut prefs);
        self.write(&prefs)?;
        Ok(out)
    }
}

// ── Page-writable prefs ──────────────────────────────────────────────────────

/// The cache limits the page sets through `pref:set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheLimit {
    PersistLimitMb,
    PersistTtlDays,
    PrewarmLimitMb,
}

impl CacheLimit {
    /// The allow-listed key, or `None`.
    pub fn parse(key: &str) -> Option<Self> {
        match key {
            "persistLimitMB" => Some(Self::PersistLimitMb),
            "persistTTLDays" => Some(Self::PersistTtlDays),
            "prewarmLimitMB" => Some(Self::PrewarmLimitMb),
            _ => None,
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Self::PersistLimitMb => "persistLimitMB",
            Self::PersistTtlDays => "persistTTLDays",
            Self::PrewarmLimitMb => "prewarmLimitMB",
        }
    }
}

/// `pref:set`: an allow-listed key and a finite number in `0..=1e7`; anything else is ignored.
/// Returns whether the value was saved.
pub fn set_cache_limit(prefs: &PrefsFile, key: &str, value: &Value) -> io::Result<bool> {
    let (Some(limit), Some(v)) = (CacheLimit::parse(key), value.as_f64()) else {
        return Ok(false);
    };
    if !(v.is_finite() && (0.0..=1e7).contains(&v)) {
        return Ok(false);
    }
    prefs.update(|p| p.extra.insert(limit.key().to_owned(), value.clone()))?;
    Ok(true)
}

/// A validated `view:tweak` write (`_validTweak`).
#[derive(Debug, Clone, PartialEq)]
pub struct Tweak {
    key: String,
    value: Value,
}

impl Tweak {
    /// A key of `[a-zA-Z][a-zA-Z0-9_-]{0,63}` that the shell does not own, with a null,
    /// boolean, finite number or string of at most 1024 characters.
    pub fn parse(key: &Value, value: &Value) -> Option<Self> {
        let key = key.as_str()?;
        let mut chars = key.chars();
        let key_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && key.len() <= 64
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !MAIN_OWNED_KEYS.contains(&key);
        let value_ok = match value {
            Value::Null | Value::Bool(_) => true,
            Value::Number(n) => n.as_f64().is_some_and(f64::is_finite),
            Value::String(s) => s.encode_utf16().count() <= 1024,
            Value::Array(_) | Value::Object(_) => false,
        };
        (key_ok && value_ok).then(|| Self {
            key: key.to_owned(),
            value: value.clone(),
        })
    }

    /// Saves the tweak; returns the prefs after it, for [`crate::theme::build_tweak_css`].
    pub fn apply(self, prefs: &PrefsFile) -> io::Result<Prefs> {
        prefs.update(|p| {
            p.extra.insert(self.key, self.value);
            p.clone()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn existing_files_load_and_unknown_keys_survive_a_rewrite() {
        let d = tempfile::tempdir().unwrap();
        let raw = json!({
            "zoomFactor": 1.1, "glassBlur": "24", "bgBlur": null, "themeMode": "accent",
            "themePalette": { "accent": "#fc3c44", "appearance": "dark", "junk": 1 },
            "themePresets": [{ "name": "mine", "palette": { "accent": "#123456" } }, { "name": 5 }, { "name": "bare" }],
            "linkSchemes": ["music"], "signedIn": true, "themeVersion": 2,
            "winAcrylic": { "material": "mica", "theme": "light" },
            "hideRadio": true, "persistLimitMB": 512, "somethingNew": { "nested": [1, 2] },
        });
        fs::write(
            d.path().join(PREFS_FILE_NAME),
            serde_json::to_vec(&raw).unwrap(),
        )
        .unwrap();
        let file = PrefsFile::new(d.path());
        let p = file.load();
        assert_eq!(p.zoom_factor, Some(1.1));
        assert_eq!(p.glass_blur, Some(24.0));
        assert_eq!(p.bg_blur, None);
        assert_eq!(p.theme_mode, Some(ThemeMode::Accent));
        assert_eq!(
            p.theme_presets.as_ref().unwrap().len(),
            2,
            "the malformed preset is dropped"
        );
        assert!(p.extra_truthy("hideRadio"));
        assert_eq!(
            p.win_acrylic.as_ref().unwrap().theme,
            crate::theme::acrylic::AcrylicTheme::Light
        );
        file.save(&p).unwrap();
        let back: Value =
            serde_json::from_slice(&fs::read(d.path().join(PREFS_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(back["somethingNew"], json!({ "nested": [1, 2] }));
        assert_eq!(back["persistLimitMB"], 512);
        assert_eq!(
            back["themePalette"],
            json!({ "accent": "#fc3c44", "appearance": "dark" })
        );
        assert_eq!(back["glassBlur"], 24.0);
    }

    #[test]
    fn missing_or_corrupt_file_is_empty_prefs() {
        let d = tempfile::tempdir().unwrap();
        let file = PrefsFile::new(d.path());
        assert_eq!(file.load(), Prefs::default());
        fs::write(d.path().join(PREFS_FILE_NAME), b"{torn").unwrap();
        assert_eq!(file.load(), Prefs::default());
        assert_eq!(
            fs::read_to_string(d.path().join(PREFS_FILE_NAME)).unwrap(),
            "{torn",
            "loading never rewrites"
        );
    }

    #[test]
    fn pref_set_allows_only_the_cache_limits_with_sane_numbers() {
        let d = tempfile::tempdir().unwrap();
        let file = PrefsFile::new(d.path());
        assert!(set_cache_limit(&file, "persistLimitMB", &json!(256)).unwrap());
        for (k, v) in [
            ("zoomFactor", json!(2)),
            ("persistLimitMB", json!(-1)),
            ("persistLimitMB", json!(2e7)),
            ("prewarmLimitMB", json!("5")),
        ] {
            assert!(!set_cache_limit(&file, k, &v).unwrap(), "{k} {v}");
        }
        assert_eq!(file.load().extra.get("persistLimitMB"), Some(&json!(256)));
        assert_eq!(file.load().zoom_factor, None);
    }

    #[test]
    fn tweaks_cannot_reach_shell_owned_keys() {
        for key in MAIN_OWNED_KEYS {
            assert_eq!(Tweak::parse(&json!(key), &json!(true)), None, "{key}");
        }
        assert!(Tweak::parse(&json!("hideRadio"), &json!(true)).is_some());
        assert!(Tweak::parse(&json!("x"), &Value::Null).is_some());
        for bad_key in [
            json!("_x"),
            json!("1x"),
            json!("a.b"),
            json!("x".repeat(65)),
            json!(5),
            json!("__proto__"),
        ] {
            assert_eq!(Tweak::parse(&bad_key, &json!(true)), None, "{bad_key}");
        }
        for bad_value in [json!({ "a": 1 }), json!([1]), json!("x".repeat(1025))] {
            assert_eq!(Tweak::parse(&json!("k"), &bad_value), None);
        }
        // A typed key in the prefs cannot be shadowed by an extra key of the same name.
        let d = tempfile::tempdir().unwrap();
        let file = PrefsFile::new(d.path());
        let p = Tweak::parse(&json!("hideUpsell"), &json!(false))
            .unwrap()
            .apply(&file)
            .unwrap();
        assert_eq!(p.extra.get("hideUpsell"), Some(&json!(false)));
    }

    #[test]
    fn js_truthiness() {
        for v in [json!(null), json!(false), json!(0), json!(""), json!(0.0)] {
            assert!(!js_truthy(&v), "{v}");
        }
        for v in [json!(true), json!(1), json!("0"), json!([]), json!({})] {
            assert!(js_truthy(&v), "{v}");
        }
    }
}
