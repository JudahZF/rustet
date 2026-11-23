//! The user's settings (spec §6.4) as one typed model, stored where the web shell kept them.
//!
//! Every setting keeps its old key and its old place, so a user's choices carry over and an
//! older Rustet still reads them:
//!
//! - the settings panel's `setTweak` keys (`streaming-quality`, `dl-*`, …) and the cache limits
//!   (`persistLimitMB`, …) are untyped keys of `electron-prefs.json` ([`Prefs::extra`]);
//! - the switches the page kept in the encrypted store (`resumeEnabled`, `historyEnabled`,
//!   `discordEnabled`, `hotkeyBindings`, …) stay there, written as the page wrote them: the
//!   strings `"true"` / `"false"`, the bindings as a JSON string;
//! - `rustet-mv-quality` lived in the web profile's `localStorage`, which is gone; it is now a
//!   prefs key of the same name (default 1080; a saved choice stays). Measured with the shell's
//!   `video` scenario through Vulkan: 1080p at 30 fps took 12 % of a core in the UI, 2160p 42 %
//!   and 640 MiB more memory (through OpenGL, 1080p managed 20 fps at a full core, 2160p 5).
//!
//! Reads follow the renderer's rules exactly (`!== false` means on by default, `!!x` off by
//! default). A value of the wrong type reads as the default. Theme colours, presets and window
//! glass stay with `rustet_desktop::theme`; link schemes with `rustet_desktop::links`.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use rustet_desktop::prefs::{Prefs, PrefsFile};
use rustet_desktop::store::Store;
use rustet_player::model::{
    Command, Crossfade, CrossfadeMode, MV_HEIGHTS, PlayerSettings, PlayerState, Seek,
    SettingsPatch, StreamingQuality, Volume,
};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::engine::{EngineClient, EngineEvent};

// ── The model ────────────────────────────────────────────────────────────────

/// Every setting, by Settings section.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub playback: Playback,
    pub library: LibrarySettings,
    pub downloads: Downloads,
    pub storage: Storage,
    pub appearance: Appearance,
    pub history: History,
    pub shortcuts: Shortcuts,
    pub integrations: IntegrationSettings,
}

/// Settings → Playback (and the music-video resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playback {
    /// `streaming-quality`. What the user picked; [`Playback::effective_quality`] applies the
    /// lossless switch.
    pub quality: StreamingQuality,
    /// `lossless-enabled`. Off forces AAC; the picked quality is kept for when it is on again.
    pub lossless_enabled: bool,
    /// `rustet-mv-quality`: 480, 720, 1080 or 2160.
    pub mv_max_height: u32,
    /// `crossfade-mode`, `crossfade-sec`, `xf-preserve-gapless`, `xf-station`.
    pub crossfade: Crossfade,
    /// `soundCheckEnabled` (store).
    pub sound_check: bool,
    /// `resumeEnabled` (store).
    pub resume_on_launch: bool,
    /// `reportPlays` (new): share listening history with Apple Music.
    pub report_plays: bool,
}

impl Playback {
    /// The quality the player uses: AAC while lossless is off (`lossless-enabled=false
    /// overrides streaming-quality`).
    pub fn effective_quality(&self) -> StreamingQuality {
        if self.lossless_enabled {
            self.quality
        } else {
            StreamingQuality::HighQuality
        }
    }
}

/// Settings → Library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibrarySettings {
    /// `use-songinfo-for-playlist`: a playlist download uses the album's track number and name.
    pub use_song_info_for_playlist: bool,
}

/// Settings → Downloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloads {
    /// `dl-quality`.
    pub quality: StreamingQuality,
    /// `dl-filename-track`.
    pub track_template: String,
    /// `dl-dirname-album`.
    pub album_template: String,
    /// `download-dir`; empty: the engine's default (`~/Music/Rustet-Downloads`).
    pub folder: String,
    /// `dl-ffmpeg-enabled`: re-encode with FFmpeg.
    pub ffmpeg_enabled: bool,
    /// `ffmpeg-path`; empty: `ffmpeg` from `PATH`.
    pub ffmpeg_path: String,
    /// `dl-embed-art`.
    pub embed_artwork: bool,
    /// `dl-motion-art`: the animated cover as a separate video file.
    pub motion_artwork: bool,
    /// `dl-mv-quality`.
    pub mv_quality: MvDownloadQuality,
    /// `dl-artwork-size`.
    pub artwork_size: ArtworkSize,
    /// `dl-lyrics`.
    pub embed_lyrics: bool,
    /// `dl-timed-lyrics` (SYLT).
    pub timed_lyrics: bool,
    /// `explicit-enabled` / `explicit-marker`.
    pub explicit_marker: Marker,
    /// `clean-enabled` / `clean-marker`.
    pub clean_marker: Marker,
    /// `adm-enabled` / `adm-marker` (Apple Digital Masters).
    pub master_marker: Marker,
    /// `retry-on-fail`.
    pub retry_on_fail: bool,
    /// `retry-timeout` in seconds: 15, 30, 60 or 300.
    pub retry_after_secs: u32,
}

/// A filename marker for a kind of track (`[E]`, `[C]`, `[M]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub enabled: bool,
    pub text: String,
}

impl Marker {
    /// The text the export puts in `{tag}`: empty when off.
    pub fn choice(&self) -> &str {
        if self.enabled { &self.text } else { "" }
    }
}

/// Music-video download resolution (`dl-mv-quality`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MvDownloadQuality {
    #[default]
    Best,
    P2160,
    P1080,
    P720,
    P480,
}

impl MvDownloadQuality {
    pub const ALL: [Self; 5] = [Self::Best, Self::P2160, Self::P1080, Self::P720, Self::P480];

    /// The saved value.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Best => "best",
            Self::P2160 => "4k",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
        }
    }

    /// The export's `mvMaxHeight`: 0 lets the engine choose the best.
    pub const fn max_height(self) -> u32 {
        match self {
            Self::Best => 0,
            Self::P2160 => 2160,
            Self::P1080 => 1080,
            Self::P720 => 720,
            Self::P480 => 480,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.key() == s)
    }
}

/// Square cover size of a download (`dl-artwork-size`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArtworkSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl ArtworkSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    pub const fn pixels(self) -> u32 {
        match self {
            Self::Small => 600,
            Self::Medium => 1200,
            Self::Large => 3000,
        }
    }

    fn parse(v: &Value) -> Option<Self> {
        let px = match v {
            Value::String(s) => s.trim().parse::<u32>().ok()?,
            Value::Number(n) => u32::try_from(n.as_u64()?).ok()?,
            _ => return None,
        };
        Self::ALL.into_iter().find(|s| s.pixels() == px)
    }
}

/// Settings → Storage. `None` is "never set": the engine's defaults apply (500 MB, no expiry).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Storage {
    /// `persistLimitMB`.
    pub song_cache_limit_mb: Option<u32>,
    /// `persistTTLDays`: songs unused longer than this are removed.
    pub song_cache_ttl_days: Option<u32>,
    /// `prewarmLimitMB`.
    pub prewarm_limit_mb: Option<u32>,
}

impl Storage {
    /// The body of `PUT /api/v1/cache/config`: only the limits that were set, as the renderer
    /// pushed them. `None` when nothing was set.
    pub fn cache_config(&self) -> Option<Value> {
        let mut body = Map::new();
        for (key, v) in [
            ("prewarmLimitMB", self.prewarm_limit_mb),
            ("persistLimitMB", self.song_cache_limit_mb),
            ("persistTTLDays", self.song_cache_ttl_days),
        ] {
            if let Some(v) = v {
                body.insert(key.into(), v.into());
            }
        }
        (!body.is_empty()).then_some(Value::Object(body))
    }
}

/// Settings → Appearance (the parts that are not theme colours).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Appearance {
    /// `artTheme`: pages take their colours from the artwork.
    pub album_art_colours: bool,
    /// `reduceMotion`.
    pub reduce_motion: bool,
    /// `hideRadio`.
    pub hide_radio: bool,
    /// `zoomFactor`; 1.0 when unset.
    pub zoom: f64,
}

/// Settings → History.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct History {
    /// `historyEnabled` (store): show history in Up Next. History is recorded either way.
    pub show_in_up_next: bool,
}

/// Settings → Integrations (Last.fm and ListenBrainz keep their own state: connected or not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrationSettings {
    /// `discordEnabled` (store).
    pub discord: bool,
}

// ── Shortcuts ────────────────────────────────────────────────────────────────

/// An in-app keyboard shortcut action (`_HOTKEY_ACTIONS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShortcutAction {
    PlayPause,
    SeekForward,
    SeekBack,
    VolumeUp,
    VolumeDown,
    Love,
}

impl ShortcutAction {
    pub const ALL: [Self; 6] = [
        Self::PlayPause,
        Self::SeekForward,
        Self::SeekBack,
        Self::VolumeUp,
        Self::VolumeDown,
        Self::Love,
    ];

    /// The key in `hotkeyBindings`.
    pub const fn id(self) -> &'static str {
        match self {
            Self::PlayPause => "playPause",
            Self::SeekForward => "seekForward",
            Self::SeekBack => "seekBack",
            Self::VolumeUp => "volUp",
            Self::VolumeDown => "volDown",
            Self::Love => "love",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::PlayPause => "Play / Pause",
            Self::SeekForward => "Seek forward 10s",
            Self::SeekBack => "Seek back 10s",
            Self::VolumeUp => "Volume up",
            Self::VolumeDown => "Volume down",
            Self::Love => "Love / Favourite",
        }
    }

    /// The default key, as a DOM `KeyboardEvent.key` value.
    pub fn default_key(self) -> Key {
        Key(match self {
            Self::PlayPause => " ",
            Self::SeekForward => "ArrowRight",
            Self::SeekBack => "ArrowLeft",
            Self::VolumeUp => "ArrowUp",
            Self::VolumeDown => "ArrowDown",
            Self::Love => "l",
        }
        .to_owned())
    }
}

/// A key as the web shell stored it: a DOM `KeyboardEvent.key` value (`" "`, `"ArrowLeft"`,
/// `"l"`). The UI maps its key events to these names. Letters match case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key(pub String);

impl Key {
    fn normalized(&self) -> String {
        if self.0.chars().count() == 1 {
            self.0.to_lowercase()
        } else {
            self.0.clone()
        }
    }

    /// The label on the binding's button (`_hotkeyKeyLabel`).
    pub fn label(&self) -> String {
        match self.0.as_str() {
            " " => "Space".into(),
            "ArrowRight" => "→".into(),
            "ArrowLeft" => "←".into(),
            "ArrowUp" => "↑".into(),
            "ArrowDown" => "↓".into(),
            "Escape" => "Esc".into(),
            k if k.chars().count() == 1 => k.to_uppercase(),
            k => k.into(),
        }
    }
}

/// Settings → Keyboard Shortcuts (`hotkeysEnabled`, `hotkeyBindings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcuts {
    pub enabled: bool,
    pub bindings: BTreeMap<ShortcutAction, Key>,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            enabled: true,
            bindings: ShortcutAction::ALL
                .into_iter()
                .map(|a| (a, a.default_key()))
                .collect(),
        }
    }
}

impl Shortcuts {
    /// The action bound to `key`, when shortcuts are on. The UI does not call this while a text
    /// field has focus or a modifier (Ctrl, Alt, Meta) is held, as the web shell did not.
    pub fn action_for(&self, key: &Key) -> Option<ShortcutAction> {
        if !self.enabled {
            return None;
        }
        let key = key.normalized();
        self.bindings
            .iter()
            .find(|(_, k)| k.normalized() == key)
            .map(|(a, _)| *a)
    }

    /// The binding JSON the page stored: `{"playPause":" ", …}`.
    fn to_json(&self) -> String {
        let map: Map<String, Value> = self
            .bindings
            .iter()
            .map(|(a, k)| (a.id().to_owned(), Value::String(k.0.clone())))
            .collect();
        Value::Object(map).to_string()
    }
}

// ── Reading and writing ──────────────────────────────────────────────────────

/// Where a setting lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Pref(&'static str),
    /// The encrypted store.
    Store(&'static str),
}

/// The saved values the settings are read from.
struct Saved<'a> {
    prefs: &'a Prefs,
    store: &'a Store,
}

impl Saved<'_> {
    fn pref(&self, key: &str) -> Option<&Value> {
        self.prefs.extra.get(key).filter(|v| !v.is_null())
    }

    /// `prefs[key] !== false`.
    fn pref_on_unless_false(&self, key: &str) -> bool {
        self.pref(key) != Some(&Value::Bool(false))
    }

    /// `!!prefs[key]`.
    fn pref_truthy(&self, key: &str) -> bool {
        self.prefs.extra_truthy(key)
    }

    fn pref_str(&self, key: &str) -> Option<&str> {
        self.pref(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    fn pref_u32(&self, key: &str) -> Option<u32> {
        let n = match self.pref(key)? {
            Value::Number(n) => n.as_f64()?,
            Value::String(s) => s.trim().parse().ok()?,
            _ => return None,
        };
        (n.is_finite() && n >= 0.0 && n <= f64::from(u32::MAX)).then_some(n as u32)
    }

    /// The page wrote `String(v)`; an older version wrote booleans.
    fn store_bool(&self, key: &str) -> Option<bool> {
        match self.store.get(key)? {
            Value::Bool(b) => Some(b),
            Value::String(s) if s == "true" => Some(true),
            Value::String(s) if s == "false" => Some(false),
            _ => None,
        }
    }
}

fn quality_key(q: StreamingQuality) -> &'static str {
    match q {
        StreamingQuality::HighQuality => "high-quality",
        StreamingQuality::Lossless => "lossless",
        StreamingQuality::HiResLossless => "hi-res-lossless",
    }
}

fn parse_quality(s: &str) -> Option<StreamingQuality> {
    [
        StreamingQuality::HighQuality,
        StreamingQuality::Lossless,
        StreamingQuality::HiResLossless,
    ]
    .into_iter()
    .find(|q| quality_key(*q) == s)
}

fn crossfade_key(m: CrossfadeMode) -> &'static str {
    match m {
        CrossfadeMode::Off => "off",
        CrossfadeMode::Automatic => "auto",
        CrossfadeMode::Adaptive => "adaptive",
        CrossfadeMode::Manual => "manual",
    }
}

const DEFAULT_TRACK_TEMPLATE: &str = "{track_number:02d} - {title}";
const DEFAULT_ALBUM_TEMPLATE: &str = "{album_artist}/{year} - {album}";
const RETRY_AFTER_SECS: [u32; 4] = [15, 30, 60, 300];

impl Settings {
    fn read(saved: &Saved<'_>) -> Self {
        // `_xfSetup`: a saved manual length (capped at 12 s) decides the mode when no mode was
        // saved; the slider shows 1–12 s, 6 s when nothing usable was saved.
        let xf_secs = saved
            .pref(XF_SECS)
            .and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            })
            .filter(|s: &f64| s.is_finite() && *s >= 0.0)
            .map(|s| s.min(12.0));
        let xf_mode = match saved.pref_str(XF_MODE) {
            Some("off") => CrossfadeMode::Off,
            Some("auto") => CrossfadeMode::Automatic,
            Some("adaptive") => CrossfadeMode::Adaptive,
            Some("manual") => CrossfadeMode::Manual,
            _ if xf_secs.is_some_and(|s| s > 0.0) => CrossfadeMode::Manual,
            _ => CrossfadeMode::Off,
        };
        let manual_seconds = xf_secs
            .filter(|s| *s > 0.0)
            .map_or(6, |s| s.clamp(1.0, 12.0).round() as u8);
        let marker = |enabled: bool, key: &str, default: &str| Marker {
            enabled,
            text: saved.pref_str(key).unwrap_or(default).to_owned(),
        };
        let mut shortcuts = Shortcuts {
            enabled: saved.store_bool(HOTKEYS_ENABLED) != Some(false),
            ..Shortcuts::default()
        };
        // `hotkeyBindings` is a JSON string; known actions with a non-empty key are taken.
        if let Some(Value::Object(saved)) = saved.store.get(HOTKEY_BINDINGS).and_then(|v| match v {
            Value::String(s) => serde_json::from_str(&s).ok(),
            other => Some(other),
        }) {
            for action in ShortcutAction::ALL {
                if let Some(key) = saved
                    .get(action.id())
                    .and_then(Value::as_str)
                    .filter(|k| !k.is_empty())
                {
                    shortcuts.bindings.insert(action, Key(key.to_owned()));
                }
            }
        }
        Self {
            playback: Playback {
                quality: saved
                    .pref_str(QUALITY)
                    .and_then(parse_quality)
                    .unwrap_or_default(),
                lossless_enabled: saved.pref_on_unless_false(LOSSLESS),
                mv_max_height: saved
                    .pref_u32(MV_QUALITY)
                    .filter(|h| MV_HEIGHTS.contains(h))
                    .unwrap_or(DEFAULT_MV_HEIGHT),
                crossfade: Crossfade {
                    mode: xf_mode,
                    manual_seconds,
                    keep_albums_gapless: saved.pref_on_unless_false(XF_GAPLESS),
                    stations: saved.pref_on_unless_false(XF_STATION),
                },
                sound_check: saved.store_bool(SOUND_CHECK) == Some(true),
                resume_on_launch: saved.store_bool(RESUME) != Some(false),
                report_plays: saved.pref_on_unless_false(REPORT_PLAYS),
            },
            library: LibrarySettings {
                use_song_info_for_playlist: saved.pref_truthy(SONGINFO),
            },
            downloads: Downloads {
                quality: saved
                    .pref_str(DL_QUALITY)
                    .and_then(parse_quality)
                    .unwrap_or_default(),
                track_template: saved
                    .pref_str(DL_TRACK)
                    .unwrap_or(DEFAULT_TRACK_TEMPLATE)
                    .to_owned(),
                album_template: saved
                    .pref_str(DL_ALBUM)
                    .unwrap_or(DEFAULT_ALBUM_TEMPLATE)
                    .to_owned(),
                folder: saved.pref_str(DL_FOLDER).unwrap_or_default().to_owned(),
                ffmpeg_enabled: saved.pref_truthy(DL_FFMPEG),
                ffmpeg_path: saved.pref_str(FFMPEG_PATH).unwrap_or_default().to_owned(),
                embed_artwork: saved.pref_on_unless_false(DL_EMBED_ART),
                motion_artwork: saved.pref_truthy(DL_MOTION_ART),
                mv_quality: saved
                    .pref_str(DL_MV_QUALITY)
                    .and_then(MvDownloadQuality::parse)
                    .unwrap_or_default(),
                artwork_size: saved
                    .pref(DL_ART_SIZE)
                    .and_then(ArtworkSize::parse)
                    .unwrap_or_default(),
                embed_lyrics: saved.pref_on_unless_false(DL_LYRICS),
                timed_lyrics: saved.pref_on_unless_false(DL_TIMED_LYRICS),
                explicit_marker: marker(
                    saved.pref_on_unless_false(EXPLICIT_ON),
                    EXPLICIT_TEXT,
                    "[E]",
                ),
                clean_marker: marker(saved.pref_truthy(CLEAN_ON), CLEAN_TEXT, "[C]"),
                master_marker: marker(saved.pref_truthy(ADM_ON), ADM_TEXT, "[M]"),
                retry_on_fail: saved.pref_on_unless_false(RETRY_ON_FAIL),
                retry_after_secs: saved
                    .pref_u32(RETRY_TIMEOUT)
                    .filter(|s| *s > 0)
                    .unwrap_or(30),
            },
            storage: Storage {
                song_cache_limit_mb: saved.pref_u32(PERSIST_LIMIT),
                song_cache_ttl_days: saved.pref_u32(PERSIST_TTL),
                prewarm_limit_mb: saved.pref_u32(PREWARM_LIMIT),
            },
            appearance: Appearance {
                album_art_colours: saved.pref_on_unless_false(ART_THEME),
                reduce_motion: saved.prefs.reduce_motion(),
                hide_radio: saved.pref_truthy(HIDE_RADIO),
                zoom: saved.prefs.zoom_factor.filter(|z| *z > 0.0).unwrap_or(1.0),
            },
            history: History {
                show_in_up_next: saved.store_bool(HISTORY) != Some(false),
            },
            shortcuts,
            integrations: IntegrationSettings {
                discord: saved.store_bool(DISCORD) == Some(true),
            },
        }
    }

    /// Every saved value, in the format the web shell wrote. Zoom is a typed prefs field and is
    /// written separately.
    fn entries(&self) -> Vec<(Slot, Value)> {
        use Slot::{Pref, Store};
        let p = &self.playback;
        let d = &self.downloads;
        let s = &self.storage;
        let flag = |b: bool| Value::String(b.to_string());
        let opt = |v: Option<u32>| v.map_or(Value::Null, Value::from);
        vec![
            (Pref(QUALITY), json!(quality_key(p.quality))),
            (Pref(LOSSLESS), json!(p.lossless_enabled)),
            (Pref(MV_QUALITY), json!(p.mv_max_height)),
            (Pref(XF_MODE), json!(crossfade_key(p.crossfade.mode))),
            (Pref(XF_SECS), json!(p.crossfade.manual_seconds)),
            (Pref(XF_GAPLESS), json!(p.crossfade.keep_albums_gapless)),
            (Pref(XF_STATION), json!(p.crossfade.stations)),
            (Store(SOUND_CHECK), flag(p.sound_check)),
            (Store(RESUME), flag(p.resume_on_launch)),
            (Pref(REPORT_PLAYS), json!(p.report_plays)),
            (
                Pref(SONGINFO),
                json!(self.library.use_song_info_for_playlist),
            ),
            (Pref(DL_QUALITY), json!(quality_key(d.quality))),
            (Pref(DL_TRACK), json!(d.track_template)),
            (Pref(DL_ALBUM), json!(d.album_template)),
            (Pref(DL_FOLDER), json!(d.folder)),
            (Pref(DL_FFMPEG), json!(d.ffmpeg_enabled)),
            (Pref(FFMPEG_PATH), json!(d.ffmpeg_path)),
            (Pref(DL_EMBED_ART), json!(d.embed_artwork)),
            (Pref(DL_MOTION_ART), json!(d.motion_artwork)),
            (Pref(DL_MV_QUALITY), json!(d.mv_quality.key())),
            (
                Pref(DL_ART_SIZE),
                json!(d.artwork_size.pixels().to_string()),
            ),
            (Pref(DL_LYRICS), json!(d.embed_lyrics)),
            (Pref(DL_TIMED_LYRICS), json!(d.timed_lyrics)),
            (Pref(EXPLICIT_ON), json!(d.explicit_marker.enabled)),
            (Pref(EXPLICIT_TEXT), json!(d.explicit_marker.text)),
            (Pref(CLEAN_ON), json!(d.clean_marker.enabled)),
            (Pref(CLEAN_TEXT), json!(d.clean_marker.text)),
            (Pref(ADM_ON), json!(d.master_marker.enabled)),
            (Pref(ADM_TEXT), json!(d.master_marker.text)),
            (Pref(RETRY_ON_FAIL), json!(d.retry_on_fail)),
            (Pref(RETRY_TIMEOUT), json!(d.retry_after_secs)),
            (Pref(PERSIST_LIMIT), opt(s.song_cache_limit_mb)),
            (Pref(PERSIST_TTL), opt(s.song_cache_ttl_days)),
            (Pref(PREWARM_LIMIT), opt(s.prewarm_limit_mb)),
            (Pref(ART_THEME), json!(self.appearance.album_art_colours)),
            (Pref(REDUCE_MOTION), json!(self.appearance.reduce_motion)),
            (Pref(HIDE_RADIO), json!(self.appearance.hide_radio)),
            (Store(HISTORY), flag(self.history.show_in_up_next)),
            (Store(HOTKEYS_ENABLED), flag(self.shortcuts.enabled)),
            (Store(HOTKEY_BINDINGS), json!(self.shortcuts.to_json())),
            (Store(DISCORD), flag(self.integrations.discord)),
        ]
    }

    /// What the player gets (`POST /api/v1/player/settings`).
    pub fn player(&self) -> PlayerSettings {
        let p = &self.playback;
        PlayerSettings {
            quality: p.effective_quality(),
            lossless_enabled: p.lossless_enabled,
            mv_max_height: p.mv_max_height,
            crossfade: p.crossfade,
            sound_check: p.sound_check,
            history_enabled: self.history.show_in_up_next,
            report_plays: p.report_plays,
            resume_on_launch: p.resume_on_launch,
        }
    }
}

/// Every field of `settings`, as the patch that sets them all.
pub fn full_patch(settings: &PlayerSettings) -> SettingsPatch {
    SettingsPatch {
        quality: Some(settings.quality),
        lossless_enabled: Some(settings.lossless_enabled),
        mv_max_height: Some(settings.mv_max_height),
        crossfade: Some(settings.crossfade),
        sound_check: Some(settings.sound_check),
        history_enabled: Some(settings.history_enabled),
        report_plays: Some(settings.report_plays),
        resume_on_launch: Some(settings.resume_on_launch),
    }
}

const QUALITY: &str = "streaming-quality";
const LOSSLESS: &str = "lossless-enabled";
const MV_QUALITY: &str = "rustet-mv-quality";
/// The music-video height limit until the user picks one (see the module notes).
pub const DEFAULT_MV_HEIGHT: u32 = 1080;
const XF_MODE: &str = "crossfade-mode";
const XF_SECS: &str = "crossfade-sec";
const XF_GAPLESS: &str = "xf-preserve-gapless";
const XF_STATION: &str = "xf-station";
const SOUND_CHECK: &str = "soundCheckEnabled";
const RESUME: &str = "resumeEnabled";
const REPORT_PLAYS: &str = "reportPlays";
const SONGINFO: &str = "use-songinfo-for-playlist";
const DL_QUALITY: &str = "dl-quality";
const DL_TRACK: &str = "dl-filename-track";
const DL_ALBUM: &str = "dl-dirname-album";
const DL_FOLDER: &str = "download-dir";
const DL_FFMPEG: &str = "dl-ffmpeg-enabled";
const FFMPEG_PATH: &str = "ffmpeg-path";
const DL_EMBED_ART: &str = "dl-embed-art";
const DL_MOTION_ART: &str = "dl-motion-art";
const DL_MV_QUALITY: &str = "dl-mv-quality";
const DL_ART_SIZE: &str = "dl-artwork-size";
const DL_LYRICS: &str = "dl-lyrics";
const DL_TIMED_LYRICS: &str = "dl-timed-lyrics";
const EXPLICIT_ON: &str = "explicit-enabled";
const EXPLICIT_TEXT: &str = "explicit-marker";
const CLEAN_ON: &str = "clean-enabled";
const CLEAN_TEXT: &str = "clean-marker";
const ADM_ON: &str = "adm-enabled";
const ADM_TEXT: &str = "adm-marker";
const RETRY_ON_FAIL: &str = "retry-on-fail";
const RETRY_TIMEOUT: &str = "retry-timeout";
const PERSIST_LIMIT: &str = "persistLimitMB";
const PERSIST_TTL: &str = "persistTTLDays";
const PREWARM_LIMIT: &str = "prewarmLimitMB";
const ART_THEME: &str = "artTheme";
const REDUCE_MOTION: &str = "reduceMotion";
const HIDE_RADIO: &str = "hideRadio";
const HISTORY: &str = "historyEnabled";
const HOTKEYS_ENABLED: &str = "hotkeysEnabled";
const HOTKEY_BINDINGS: &str = "hotkeyBindings";
const DISCORD: &str = "discordEnabled";

/// Why a settings change was refused.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("music-video height {0} is not one of 480, 720, 1080, 2160")]
    MvHeight(u32),
    #[error("crossfade length {0} s is outside 1–12 s")]
    CrossfadeSeconds(u8),
    #[error("retry delay {0} s is not one of 15, 30, 60, 300")]
    RetryDelay(u32),
    #[error("zoom {0} is not a positive number")]
    Zoom(f64),
    #[error("the same key is bound twice")]
    DuplicateKey,
    #[error("cannot write the preferences: {0}")]
    Io(#[from] io::Error),
}

impl Settings {
    fn validate(&self) -> Result<(), SettingsError> {
        let p = &self.playback;
        if !MV_HEIGHTS.contains(&p.mv_max_height) {
            return Err(SettingsError::MvHeight(p.mv_max_height));
        }
        if !(1..=12).contains(&p.crossfade.manual_seconds) {
            return Err(SettingsError::CrossfadeSeconds(p.crossfade.manual_seconds));
        }
        if !RETRY_AFTER_SECS.contains(&self.downloads.retry_after_secs) {
            return Err(SettingsError::RetryDelay(self.downloads.retry_after_secs));
        }
        let zoom = self.appearance.zoom;
        if !(zoom.is_finite() && zoom > 0.0) {
            return Err(SettingsError::Zoom(zoom));
        }
        let mut keys: Vec<String> = self
            .shortcuts
            .bindings
            .values()
            .map(Key::normalized)
            .collect();
        keys.sort();
        if keys.windows(2).any(|w| w[0] == w[1]) {
            return Err(SettingsError::DuplicateKey);
        }
        Ok(())
    }
}

/// The settings over the prefs file and the store: read once, changed through
/// [`SettingsStore::update`], watched by whoever needs them. Cheap to clone.
#[derive(Clone)]
pub struct SettingsStore {
    prefs: Arc<PrefsFile>,
    store: Store,
    tx: Arc<watch::Sender<Settings>>,
}

impl SettingsStore {
    /// Reads the saved settings. Nothing is written until the first change.
    pub fn load(prefs: Arc<PrefsFile>, store: Store) -> Self {
        let settings = Settings::read(&Saved {
            prefs: &prefs.load(),
            store: &store,
        });
        Self {
            prefs,
            store,
            tx: Arc::new(watch::channel(settings).0),
        }
    }

    /// The current settings.
    pub fn get(&self) -> Settings {
        self.tx.borrow().clone()
    }

    /// Change notifications: the receiver sees every new value.
    pub fn subscribe(&self) -> watch::Receiver<Settings> {
        self.tx.subscribe()
    }

    /// Changes settings: `f` edits a copy, which is validated, then only the values that differ
    /// are written. Watchers are told when anything changed. Store values are written by the
    /// store's own delayed flush.
    pub fn update(&self, f: impl FnOnce(&mut Settings)) -> Result<Settings, SettingsError> {
        let old = self.get();
        let mut new = old.clone();
        f(&mut new);
        if new == old {
            return Ok(new);
        }
        new.validate()?;
        let changed: Vec<(Slot, Value)> = new
            .entries()
            .into_iter()
            .zip(old.entries())
            .filter(|(n, o)| n != o)
            .map(|(n, _)| n)
            .collect();
        let zoom = (new.appearance.zoom != old.appearance.zoom).then_some(new.appearance.zoom);
        let (prefs, store): (Vec<_>, Vec<_>) = changed
            .into_iter()
            .partition(|(slot, _)| matches!(slot, Slot::Pref(_)));
        if !prefs.is_empty() || zoom.is_some() {
            self.prefs.update(|p| {
                for (slot, value) in prefs {
                    if let Slot::Pref(key) = slot {
                        match value {
                            Value::Null => p.extra.remove(key),
                            v => p.extra.insert(key.to_owned(), v),
                        };
                    }
                }
                if let Some(z) = zoom {
                    p.zoom_factor = Some(z);
                }
            })?;
        }
        if !store.is_empty() {
            self.store.update(|m| {
                for (slot, value) in store {
                    if let Slot::Store(key) = slot {
                        m.insert(key.to_owned(), value);
                    }
                }
            });
        }
        self.tx.send_replace(new.clone());
        Ok(new)
    }
}

impl ShortcutAction {
    /// The player command of the shortcut against `state` (seek ±10 s, volume ±5). `None` for
    /// [`ShortcutAction::Love`], which is the library's, and for a seek with nothing loaded.
    pub fn command(self, state: &PlayerState, now_ms: u64) -> Option<Command> {
        let seek = |delta: i64| {
            state.track.as_ref()?;
            let pos = crate::integrations::position_now(state, now_ms) as i64;
            Some(Command::Seek(Seek {
                position_ms: (pos + delta).max(0) as u64,
                expect_generation: Some(state.generation),
            }))
        };
        let volume = |delta: i16| {
            Some(Command::Volume(Volume {
                level: (i16::from(state.volume) + delta).clamp(0, 100) as u8,
                muted: None,
            }))
        };
        match self {
            Self::PlayPause => Some(Command::Toggle),
            Self::SeekForward => seek(10_000),
            Self::SeekBack => seek(-10_000),
            Self::VolumeUp => volume(5),
            Self::VolumeDown => volume(-5),
            Self::Love => None,
        }
    }
}

/// Keeps the engine in step with the settings: after every engine start (each event-stream
/// snapshot) and on every change, the player settings go to `POST /api/v1/player/settings`
/// and the cache limits that were set to `PUT /api/v1/cache/config`. Runs until the event
/// stream closes.
pub async fn sync_engine(
    client: EngineClient,
    mut settings: watch::Receiver<Settings>,
    mut events: broadcast::Receiver<EngineEvent>,
) {
    let push = async |s: &Settings, player: bool, cache: bool| {
        if player {
            let patch = full_patch(&s.player());
            if let Err(e) = client.command(&Command::Settings(patch)).await {
                warn!("the engine did not take the player settings: {e}");
            }
        }
        if cache
            && let Some(config) = s.storage.cache_config()
            && let Err(e) = client.set_cache_config(&config).await
        {
            warn!("the engine did not take the cache limits: {e}");
        }
    };
    let mut last = settings.borrow_and_update().clone();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(EngineEvent::Snapshot(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    push(&last, true, true).await;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            Ok(()) = settings.changed() => {
                let new = settings.borrow_and_update().clone();
                push(&new, new.player() != last.player(), new.storage != last.storage).await;
                last = new;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_keys_match_like_the_web_shell() {
        let mut s = Shortcuts::default();
        assert_eq!(
            s.action_for(&Key(" ".into())),
            Some(ShortcutAction::PlayPause)
        );
        assert_eq!(
            s.action_for(&Key("L".into())),
            Some(ShortcutAction::Love),
            "letters ignore case"
        );
        assert_eq!(s.action_for(&Key("x".into())), None);
        assert_eq!(Key(" ".into()).label(), "Space");
        assert_eq!(Key("l".into()).label(), "L");
        assert_eq!(Key("ArrowUp".into()).label(), "↑");
        s.enabled = false;
        assert_eq!(s.action_for(&Key(" ".into())), None);
    }

    #[test]
    fn storage_pushes_only_the_limits_that_were_set() {
        assert_eq!(Storage::default().cache_config(), None);
        let s = Storage {
            song_cache_limit_mb: Some(800),
            ..Storage::default()
        };
        assert_eq!(s.cache_config(), Some(json!({"persistLimitMB": 800})));
    }
}
