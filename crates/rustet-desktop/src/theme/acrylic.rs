//! Windows host backdrop settings (`acrylic.mjs`). The backdrop itself and the page overlay
//! script are the shell's; this is the saved setting, its validation and the values the overlay
//! uses.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::platform::Platform;

/// The one backdrop material: the per-window Windows Composition Gaussian effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcrylicMaterial {
    Gaussian,
}

/// Materials the backend offers.
pub const ACRYLIC_MATERIALS: [AcrylicMaterial; 1] = [AcrylicMaterial::Gaussian];

/// Height of the strip the page reserves for dragging; the caption height Windows draws its
/// buttons in.
pub const ACRYLIC_TITLEBAR_HEIGHT: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcrylicTheme {
    System,
    Light,
    Dark,
}

/// The saved `winAcrylic` preference. Deserialising never fails: anything stored or received
/// from the page becomes valid settings, bad fields falling back to the default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "Value")]
pub struct AcrylicSettings {
    pub material: AcrylicMaterial,
    pub theme: AcrylicTheme,
    /// `#rrggbb`, lower case.
    pub tint_color: String,
    pub tint_opacity: f64,
    pub lum_opacity: f64,
    pub blur_radius: f64,
    pub saturation: f64,
    pub noise: f64,
}

impl Default for AcrylicSettings {
    /// Dark by default because Rustet's interface is dark-styled. The tint and luminosity layers
    /// start at 0, so what the user sees is the compositor's own blurred, saturated backdrop.
    fn default() -> Self {
        Self {
            material: AcrylicMaterial::Gaussian,
            theme: AcrylicTheme::Dark,
            tint_color: "#000000".into(),
            tint_opacity: 0.0,
            lum_opacity: 0.0,
            blur_radius: 0.6792668,
            saturation: 1.25,
            noise: 0.025,
        }
    }
}

impl From<Value> for AcrylicSettings {
    fn from(raw: Value) -> Self {
        normalize_acrylic(&raw)
    }
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn enum_field<T: serde::de::DeserializeOwned>(v: Option<&Value>) -> Option<T> {
    v.and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Validates untrusted settings field by field (`normalizeAcrylic`). A material saved by the
/// DWM-acrylic version (`acrylic`, `mica`, …) migrates to `gaussian`, keeping the rest.
pub fn normalize_acrylic(raw: &Value) -> AcrylicSettings {
    let d = AcrylicSettings::default();
    let get = |k: &str| raw.as_object().and_then(|o| o.get(k));
    let unit = |k: &str, fallback: f64| finite(get(k)).map_or(fallback, |n| n.clamp(0.0, 1.0));
    let tint_color = get("tintColor")
        .and_then(Value::as_str)
        .filter(|s| {
            s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase);
    AcrylicSettings {
        material: enum_field(get("material")).unwrap_or(d.material),
        theme: enum_field(get("theme")).unwrap_or(d.theme),
        tint_color: tint_color.unwrap_or(d.tint_color),
        tint_opacity: unit("tintOpacity", d.tint_opacity),
        lum_opacity: unit("lumOpacity", d.lum_opacity),
        blur_radius: unit("blurRadius", d.blur_radius),
        saturation: finite(get("saturation")).map_or(d.saturation, |n| n.clamp(0.0, 2.0)),
        noise: unit("noise", d.noise).min(0.1),
    }
}

impl AcrylicSettings {
    /// The tint layer's colour, `rgba(r,g,b,opacity)`.
    pub fn tint_rgba(&self) -> String {
        let n = u32::from_str_radix(&self.tint_color[1..], 16).unwrap_or(0);
        format!(
            "rgba({},{},{},{})",
            (n >> 16) & 255,
            (n >> 8) & 255,
            n & 255,
            self.tint_opacity
        )
    }

    /// Opacity of the grain layer: the noise setting, faded out as the blur goes to zero.
    pub fn noise_opacity(&self) -> f64 {
        let radius = 30.0 * self.blur_radius.powf(1.25);
        self.noise * (1.0 - (-radius / 4.0).exp())
    }
}

/// Which Windows this is, from the OS release string (`10.0.<build>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsVersion {
    Win10,
    Win11,
}

/// `Win11` from build 22000, `Win10` below it, `None` for anything else or another platform.
pub fn windows_version(release: &str, platform: Platform) -> Option<WindowsVersion> {
    if platform != Platform::Windows {
        return None;
    }
    let rest = release.strip_prefix("10.0.")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let build: u64 = digits.parse().ok()?;
    Some(if build >= 22000 {
        WindowsVersion::Win11
    } else {
        WindowsVersion::Win10
    })
}

/// Whether the blur theme uses the Composition backdrop: Windows 10 and 11, nothing else.
pub fn has_acrylic_backend(release: &str, platform: Platform) -> bool {
    windows_version(release, platform).is_some()
}

/// The overlay layers (luminosity, tint, noise, drag strip) behind the interface. Hidden in the
/// accent and custom-CSS themes, which paint their own opaque background.
pub const ACRYLIC_CSS: &str = r#"
:root { --rustet-acrylic-lum: #202020; --rustet-acrylic-lum-opacity: 0; --rustet-acrylic-tint: rgba(0,0,0,0); --rustet-acrylic-noise: 0; }
@media (prefers-color-scheme: light) { :root { --rustet-acrylic-lum: #f3f3f3; } }
#_rustetAcrylicLum, #_rustetAcrylicTint { position: fixed; inset: 0; pointer-events: none; z-index: -1; }
#_rustetAcrylicLum { background: var(--rustet-acrylic-lum); opacity: var(--rustet-acrylic-lum-opacity); }
#_rustetAcrylicNoise { position: fixed; inset: 0; pointer-events: none; z-index: 0; opacity: var(--rustet-acrylic-noise); mix-blend-mode: soft-light;
    background-image: url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='120' height='120'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.9' numOctaves='2' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)'/%3E%3C/svg%3E"); }
/* Windows hit-tests this transparent strip as HTCAPTION so DWM handles move, restore, and snap. */
#_rustetAcrylicDrag { position: fixed; top: 0; left: 0; width: 100%;
    height: 32px; -webkit-app-region: drag; z-index: 2147483000; }
#_rustetAcrylicTint { background: var(--rustet-acrylic-tint); }
html[data-rustet-acrylic-material="none"] #_rustetAcrylicLum { opacity: 1 !important; }
body[data-rustet-mode="accent"] #_rustetAcrylicLum, body[data-rustet-mode="accent"] #_rustetAcrylicTint,
body[data-rustet-mode="custom"] #_rustetAcrylicLum, body[data-rustet-mode="custom"] #_rustetAcrylicTint { display: none !important; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults() {
        let d = AcrylicSettings::default();
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({
                "material": "gaussian", "theme": "dark", "tintColor": "#000000", "tintOpacity": 0.0, "lumOpacity": 0.0,
                "blurRadius": 0.6792668, "saturation": 1.25, "noise": 0.025,
            })
        );
        for nothing in [Value::Null, json!("gaussian"), json!(7)] {
            assert_eq!(normalize_acrylic(&nothing), d);
        }
    }

    #[test]
    fn old_materials_migrate_keeping_the_rest() {
        for old in ["acrylic", "mica", "tabbed", "auto", "none"] {
            let got = normalize_acrylic(
                &json!({ "material": old, "theme": "light", "tintColor": "#ff0000", "tintOpacity": 0.3 }),
            );
            assert_eq!(got.material, AcrylicMaterial::Gaussian);
            assert_eq!(
                (got.theme, got.tint_color.as_str(), got.tint_opacity),
                (AcrylicTheme::Light, "#ff0000", 0.3)
            );
        }
    }

    #[test]
    fn every_field_is_validated_and_clamped() {
        assert_eq!(
            normalize_acrylic(&json!({ "theme": "sepia" })).theme,
            AcrylicTheme::Dark
        );
        let hi = normalize_acrylic(
            &json!({ "tintOpacity": 5, "lumOpacity": 5, "blurRadius": 5, "saturation": 9, "noise": 5 }),
        );
        assert_eq!(
            [
                hi.tint_opacity,
                hi.lum_opacity,
                hi.blur_radius,
                hi.saturation,
                hi.noise
            ],
            [1.0, 1.0, 1.0, 2.0, 0.1]
        );
        let lo = normalize_acrylic(
            &json!({ "tintOpacity": -1, "lumOpacity": -1, "blurRadius": -1, "saturation": -1, "noise": -1 }),
        );
        assert_eq!(
            [
                lo.tint_opacity,
                lo.lum_opacity,
                lo.blur_radius,
                lo.saturation,
                lo.noise
            ],
            [0.0; 5]
        );
        assert_eq!(
            normalize_acrylic(&json!({ "blurRadius": 0 })).blur_radius,
            0.0,
            "zero blur is a real value"
        );
        let d = AcrylicSettings::default();
        for bad in [json!("0.5"), Value::Null, json!({})] {
            let got = normalize_acrylic(
                &json!({ "tintOpacity": bad, "blurRadius": bad, "saturation": bad, "noise": bad }),
            );
            assert_eq!(
                [got.tint_opacity, got.blur_radius, got.saturation, got.noise],
                [d.tint_opacity, d.blur_radius, d.saturation, d.noise]
            );
        }
    }

    #[test]
    fn tint_colour_must_be_rrggbb_and_is_lower_cased() {
        assert_eq!(
            normalize_acrylic(&json!({ "tintColor": "#AABBCC" })).tint_color,
            "#aabbcc"
        );
        for bad in [
            json!("red"),
            json!("#fff"),
            json!("#12345g"),
            json!("rgb(1,2,3)"),
            json!("#aabbcc;}</style>"),
            json!(123),
        ] {
            assert_eq!(
                normalize_acrylic(&json!({ "tintColor": bad })).tint_color,
                "#000000"
            );
        }
    }

    #[test]
    fn overlay_values() {
        let s = normalize_acrylic(&json!({ "tintColor": "#102030", "tintOpacity": 0.5 }));
        assert_eq!(s.tint_rgba(), "rgba(16,32,48,0.5)");
        let n = s.noise_opacity();
        assert!(
            n > 0.0 && n <= s.noise,
            "noise follows the blur and never exceeds the setting"
        );
        assert_eq!(
            normalize_acrylic(&json!({ "blurRadius": 0, "noise": 0.1 })).noise_opacity(),
            0.0
        );
        assert!(ACRYLIC_CSS.contains(&format!(
            "height: {ACRYLIC_TITLEBAR_HEIGHT}px; -webkit-app-region: drag"
        )));
    }

    #[test]
    fn windows_10_and_11_only() {
        use WindowsVersion::*;
        assert_eq!(
            windows_version("10.0.26200", Platform::Windows),
            Some(Win11)
        );
        assert_eq!(
            windows_version("10.0.22000", Platform::Windows),
            Some(Win11)
        );
        assert_eq!(
            windows_version("10.0.21996", Platform::Windows),
            Some(Win10)
        );
        assert_eq!(
            windows_version("10.0.19045", Platform::Windows),
            Some(Win10)
        );
        for old in ["6.3.9600", "6.1.7601", "5.1.2600", ""] {
            assert_eq!(windows_version(old, Platform::Windows), None);
        }
        assert_eq!(windows_version("10.0.26200", Platform::Linux), None);
        assert!(has_acrylic_backend("10.0.17134", Platform::Windows));
        assert!(!has_acrylic_backend("6.1.7601", Platform::Windows));
    }
}
