//! Compositor detection and the desktop wallpaper the blur theme draws, blurred, behind the
//! interface (`wallpaper.mjs` and the wallpaper code of `main.mjs`).
//!
//! The blur is done here, once: a wallpaper reduced to a few dozen pixels and scaled back up by
//! the page is already soft, and costs nothing per frame, whereas a full-window CSS `blur()` made
//! the page crawl on machines without a GPU.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::Engine;
use image::{DynamicImage, ImageReader, RgbaImage, imageops::FilterType};

use crate::platform::Env;
use crate::system::CommandRunner;

/// Wallpapers larger than this are not read.
pub const WALLPAPER_MAX_BYTES: u64 = 60 * 1024 * 1024;

/// The desktop the shell runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desktop {
    /// `XDG_CURRENT_DESKTOP`, lower case (empty when unset).
    pub name: String,
    pub wayland: bool,
    pub kde: bool,
    pub hyprland: bool,
}

/// How the blur layer gets its picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlurSource {
    /// Hyprland: its wallpaper file ([`hyprland_wallpaper_path`]).
    HyprlandWallpaper,
    /// GNOME: its wallpaper file ([`gnome_wallpaper_path`]), with the screenshot as fallback
    /// when [`Desktop::software_blur`] holds.
    GnomeWallpaper,
    /// A blurred desktop screenshot (X11, Sway and other compositors without blur-behind).
    Screenshot,
    /// Nothing to draw: the compositor (KWin, the Windows backdrop) blurs behind the window.
    None,
}

impl Desktop {
    pub fn from_env(env: &Env) -> Self {
        let name = env
            .get("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_lowercase();
        Self {
            wayland: env.get("WAYLAND_DISPLAY").is_some(),
            kde: name.contains("kde") || name.contains("plasma"),
            hyprland: name.contains("hyprland") || env.get("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
            name,
        }
    }

    /// Whether the software (screenshot) blur is used: on Linux off KDE, on X11 or on Wayland
    /// compositors without blur-behind. A GPU crash turns it on everywhere (the shell's call).
    pub fn software_blur(&self) -> bool {
        cfg!(target_os = "linux")
            && !self.kde
            && (!self.wayland
                || ["gnome", "sway", "unity", "cosmic", "budgie"]
                    .iter()
                    .any(|d| self.name.contains(d)))
    }

    /// Where the blur picture comes from. `acrylic` is the Windows backdrop.
    pub fn blur_source(&self, acrylic: bool, software_blur: bool) -> BlurSource {
        if self.hyprland {
            BlurSource::HyprlandWallpaper
        } else if acrylic {
            BlurSource::None
        } else if self.name.contains("gnome") {
            BlurSource::GnomeWallpaper
        } else if software_blur {
            BlurSource::Screenshot
        } else {
            BlurSource::None
        }
    }
}

// ── Wallpaper lookup ─────────────────────────────────────────────────────────

/// `gsettings get org.gnome.desktop.background picture-uri` output → a file path (`None` when
/// not a `file://` URI).
pub fn parse_gsettings_uri(out: &str) -> Option<PathBuf> {
    let s = out.trim();
    let s = s.strip_prefix('\'').unwrap_or(s);
    let s = s.strip_suffix('\'').unwrap_or(s);
    if !s.starts_with("file://") {
        return None;
    }
    url::Url::parse(s).ok()?.to_file_path().ok()
}

/// GNOME's wallpaper: the dark variant first, and only an existing file.
pub async fn gnome_wallpaper_path(runner: &impl CommandRunner) -> Option<PathBuf> {
    for key in ["picture-uri-dark", "picture-uri"] {
        let out = runner
            .run("gsettings", &["get", "org.gnome.desktop.background", key])
            .await
            .unwrap_or_default();
        if let Some(p) = parse_gsettings_uri(&out).filter(|p| p.exists()) {
            return Some(p);
        }
    }
    None
}

/// KDE Plasma's wallpaper: the first `Image=` of `~/.config/plasma-org.kde.plasma.desktop-appletsrc`
/// (the Electron shell had this lookup but no caller; KWin blurs behind the window itself).
pub fn kde_wallpaper_path(home: &Path) -> Option<PathBuf> {
    let text =
        fs::read_to_string(home.join(".config/plasma-org.kde.plasma.desktop-appletsrc")).ok()?;
    text.lines().find_map(|line| {
        let rest = line
            .strip_prefix("Image")?
            .trim_start()
            .strip_prefix('=')?
            .trim();
        (!rest.is_empty()).then(|| PathBuf::from(rest.strip_prefix("file://").unwrap_or(rest)))
    })
}

/// Hyprland's wallpaper: hyprpaper's config (`wallpaper = <monitor>, <path>`), then swww's
/// cache, then the current-background symlink of Omarchy (and Nixarchy, its NixOS port).
pub fn hyprland_wallpaper_path(home: &Path) -> Option<PathBuf> {
    if let Ok(conf) = fs::read_to_string(home.join(".config/hypr/hyprpaper.conf")) {
        let found = conf.lines().find_map(|line| {
            let rest = line
                .strip_prefix("wallpaper")?
                .trim_start()
                .strip_prefix('=')?;
            let (_, path) = rest.split_once(',')?;
            let path = path.trim();
            (!path.is_empty()).then(|| PathBuf::from(path))
        });
        if found.is_some() {
            return found;
        }
    }
    if let Ok(entries) = fs::read_dir(home.join(".cache/swww")) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for f in files {
            let Ok(raw) = fs::read(&f) else { continue };
            let first = raw.split(|b| *b == 0).next().unwrap_or_default();
            let content = String::from_utf8_lossy(first);
            let content = content.trim();
            if !content.is_empty() && Path::new(content).exists() {
                return Some(PathBuf::from(content));
            }
        }
    }
    [
        ".config/omarchy/current/background",
        ".local/state/omarchy/current/background",
    ]
    .iter()
    .map(|rel| home.join(rel))
    .find(|p| p.exists())
}

/// MIME subtype for a `data:` URL from a file name (Windows' cached wallpaper has no extension
/// and is a JPEG).
pub fn image_mime(file: &Path) -> String {
    match file.extension().map(|e| e.to_string_lossy().to_lowercase()) {
        None => "jpeg".into(),
        Some(e) if e == "jpg" || e.is_empty() => "jpeg".into(),
        Some(e) => e,
    }
}

// ── Blur ─────────────────────────────────────────────────────────────────────

/// Why a wallpaper could not be blurred.
#[derive(Debug, thiserror::Error)]
pub enum WallpaperError {
    #[error("reading the wallpaper: {0}")]
    Io(#[from] std::io::Error),
    #[error("the wallpaper is larger than 60 MiB")]
    TooLarge,
    #[error("the wallpaper could not be decoded: {0}")]
    Decode(#[from] image::ImageError),
    #[error("the image is empty")]
    Empty,
}

/// The width the picture is reduced to: the reference width over 1.2 × the Background blur
/// slider (`bgBlur`, px), between 48 and 960.
pub fn blur_width(ref_width: u32, bg_blur_px: f64) -> u32 {
    (f64::from(ref_width) / (bg_blur_px.max(1.0) * 1.2))
        .round()
        .clamp(48.0, 960.0) as u32
}

/// A small JPEG (quality 80) of `img` as a `data:` URL.
fn blur_data_url(
    img: &DynamicImage,
    ref_width: u32,
    bg_blur_px: f64,
) -> Result<String, WallpaperError> {
    if img.width() == 0 || img.height() == 0 {
        return Err(WallpaperError::Empty);
    }
    let width = blur_width(ref_width, bg_blur_px);
    let height = ((f64::from(img.height()) * f64::from(width) / f64::from(img.width())).round()
        as u32)
        .max(1);
    let small = img
        .resize_exact(width, height, FilterType::Triangle)
        .to_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&small)?;
    Ok(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    ))
}

/// The blurred wallpaper the shell injects as the blur layer's background (`_blurredWallpaperUrl`).
/// Decodes PNG, JPEG, WebP, GIF (first frame) and BMP. CPU-bound: call it from
/// `spawn_blocking`. `display_width` is the primary display's width (1920 when unknown).
pub fn blurred_wallpaper(
    file: &Path,
    display_width: u32,
    bg_blur_px: f64,
) -> Result<String, WallpaperError> {
    if fs::metadata(file)?.len() > WALLPAPER_MAX_BYTES {
        return Err(WallpaperError::TooLarge);
    }
    let img = ImageReader::new(Cursor::new(fs::read(file)?))
        .with_guessed_format()?
        .decode()?;
    blur_data_url(&img, display_width, bg_blur_px)
}

/// The same for a desktop screenshot (RGBA pixels) in the software blur. The screenshot's own
/// width is the reference.
pub fn blurred_screenshot(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    bg_blur_px: f64,
) -> Result<String, WallpaperError> {
    let img = RgbaImage::from_raw(width, height, rgba).ok_or(WallpaperError::Empty)?;
    blur_data_url(&DynamicImage::ImageRgba8(img), width, bg_blur_px)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeGsettings(String, String);

    impl CommandRunner for FakeGsettings {
        async fn run(&self, _cmd: &str, args: &[&str]) -> Option<String> {
            Some(if args[2] == "picture-uri-dark" {
                self.0.clone()
            } else {
                self.1.clone()
            })
        }
    }

    #[test]
    fn desktop_detection() {
        let d = |pairs: &[(&str, &str)]| Desktop::from_env(&pairs.iter().copied().collect());
        let h = d(&[
            ("XDG_CURRENT_DESKTOP", "Hyprland"),
            ("WAYLAND_DISPLAY", "wayland-1"),
        ]);
        assert!(h.hyprland && h.wayland && !h.kde);
        assert_eq!(
            h.blur_source(false, h.software_blur()),
            BlurSource::HyprlandWallpaper
        );
        assert!(d(&[("HYPRLAND_INSTANCE_SIGNATURE", "x")]).hyprland);
        let k = d(&[("XDG_CURRENT_DESKTOP", "KDE"), ("WAYLAND_DISPLAY", "w")]);
        assert!(k.kde && !k.software_blur());
        assert_eq!(k.blur_source(false, k.software_blur()), BlurSource::None);
        let g = d(&[
            ("XDG_CURRENT_DESKTOP", "ubuntu:GNOME"),
            ("WAYLAND_DISPLAY", "w"),
        ]);
        assert!(g.software_blur());
        assert_eq!(g.blur_source(false, true), BlurSource::GnomeWallpaper);
        let x11 = d(&[("XDG_CURRENT_DESKTOP", "i3")]);
        assert_eq!(
            x11.blur_source(false, x11.software_blur()),
            BlurSource::Screenshot
        );
        assert!(!d(&[("XDG_CURRENT_DESKTOP", "river"), ("WAYLAND_DISPLAY", "w")]).software_blur());
        assert_eq!(
            g.blur_source(true, true),
            BlurSource::None,
            "the Windows backdrop is the blur"
        );
    }

    #[test]
    fn gsettings_uris() {
        assert_eq!(
            parse_gsettings_uri("'file:///home/a/Pictures/my%20wall.jpg'\n"),
            Some(PathBuf::from("/home/a/Pictures/my wall.jpg"))
        );
        assert_eq!(parse_gsettings_uri("'none'"), None);
        assert_eq!(parse_gsettings_uri("'https://example.com/x.jpg'"), None);
        assert_eq!(parse_gsettings_uri(""), None);
    }

    #[tokio::test]
    async fn gnome_prefers_the_dark_variant_and_needs_an_existing_file() {
        let d = tempfile::tempdir().unwrap();
        let (dark, light) = (d.path().join("dark.png"), d.path().join("light.png"));
        let uri = |p: &Path| format!("'file://{}'", p.display());
        let runner = FakeGsettings(uri(&dark), uri(&light));
        assert_eq!(gnome_wallpaper_path(&runner).await, None);
        fs::write(&light, b"").unwrap();
        assert_eq!(gnome_wallpaper_path(&runner).await, Some(light.clone()));
        fs::write(&dark, b"").unwrap();
        assert_eq!(gnome_wallpaper_path(&runner).await, Some(dark));
    }

    #[test]
    fn kde_and_hyprland_lookups() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        assert_eq!(kde_wallpaper_path(h), None);
        fs::create_dir_all(h.join(".config")).unwrap();
        fs::write(
            h.join(".config/plasma-org.kde.plasma.desktop-appletsrc"),
            "[Containments][1]\nImage=file:///w/kde.jpg\nImage=/second\n",
        )
        .unwrap();
        assert_eq!(kde_wallpaper_path(h), Some(PathBuf::from("/w/kde.jpg")));

        assert_eq!(hyprland_wallpaper_path(h), None);
        let omarchy = h.join(".local/state/omarchy/current");
        fs::create_dir_all(&omarchy).unwrap();
        fs::write(omarchy.join("background"), b"").unwrap();
        assert_eq!(
            hyprland_wallpaper_path(h),
            Some(omarchy.join("background")),
            "Omarchy's link is the last resort"
        );
        let swww = h.join(".cache/swww");
        fs::create_dir_all(&swww).unwrap();
        let wall = h.join("wall.png");
        fs::write(&wall, b"").unwrap();
        fs::write(swww.join("eDP-1"), format!("{}\0lanczos", wall.display())).unwrap();
        assert_eq!(hyprland_wallpaper_path(h), Some(wall));
        fs::create_dir_all(h.join(".config/hypr")).unwrap();
        fs::write(
            h.join(".config/hypr/hyprpaper.conf"),
            "preload = /x.png\nwallpaper = DP-1, /w/hypr.png\n",
        )
        .unwrap();
        assert_eq!(
            hyprland_wallpaper_path(h),
            Some(PathBuf::from("/w/hypr.png"))
        );
    }

    #[test]
    fn mime_from_extension() {
        assert_eq!(image_mime(Path::new(r"C:\x\TranscodedWallpaper")), "jpeg");
        assert_eq!(image_mime(Path::new("/a/b.JPG")), "jpeg");
        assert_eq!(image_mime(Path::new("/a/b.png")), "png");
        assert_eq!(image_mime(Path::new("/a/b.bmp")), "bmp");
    }

    #[test]
    fn blur_width_follows_the_slider() {
        assert_eq!(blur_width(1920, 18.0), 89);
        assert_eq!(blur_width(1920, 1.0), 960);
        assert_eq!(blur_width(1920, 0.0), 960, "at least 1px");
        assert_eq!(blur_width(1920, 80.0), 48);
    }

    fn sample(format: image::ImageFormat, ext: &str, dir: &Path) -> PathBuf {
        let img = RgbaImage::from_fn(400, 200, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let p = dir.join(format!("w.{ext}"));
        DynamicImage::ImageRgba8(img)
            .to_rgb8()
            .save_with_format(&p, format)
            .unwrap();
        p
    }

    #[test]
    fn wallpapers_of_every_format_become_a_small_jpeg() {
        use image::ImageFormat::*;
        let d = tempfile::tempdir().unwrap();
        for (format, ext) in [
            (Png, "png"),
            (Jpeg, "jpg"),
            (WebP, "webp"),
            (Gif, "gif"),
            (Bmp, "bmp"),
        ] {
            let p = sample(format, ext, d.path());
            let url = blurred_wallpaper(&p, 1920, 18.0).unwrap_or_else(|e| panic!("{ext}: {e}"));
            let b64 = url
                .strip_prefix("data:image/jpeg;base64,")
                .expect("a JPEG data URL");
            let jpeg = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap();
            let back = image::load_from_memory_with_format(&jpeg, Jpeg).unwrap();
            assert_eq!(
                (back.width(), back.height()),
                (89, 45),
                "{ext}: display width / (18 × 1.2), aspect kept"
            );
        }
        fs::write(d.path().join("bad.png"), b"not an image").unwrap();
        assert!(matches!(
            blurred_wallpaper(&d.path().join("bad.png"), 1920, 18.0),
            Err(WallpaperError::Decode(_))
        ));
        assert!(matches!(
            blurred_wallpaper(&d.path().join("missing.png"), 1920, 18.0),
            Err(WallpaperError::Io(_))
        ));
        let big = d.path().join("big.png");
        fs::File::create(&big)
            .unwrap()
            .set_len(WALLPAPER_MAX_BYTES + 1)
            .unwrap();
        assert!(matches!(
            blurred_wallpaper(&big, 1920, 18.0),
            Err(WallpaperError::TooLarge)
        ));
    }

    #[test]
    fn screenshots_blur_at_their_own_width() {
        let url = blurred_screenshot(960, 540, vec![255; 960 * 540 * 4], 18.0).unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"));
        assert!(matches!(
            blurred_screenshot(10, 10, vec![0; 3], 18.0),
            Err(WallpaperError::Empty)
        ));
    }
}
