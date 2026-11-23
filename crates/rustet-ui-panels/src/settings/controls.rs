//! The page's stateful controls (selects, sliders, text fields) and how they follow the
//! stored settings. Toggles and buttons are stateless and built in each section.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{AppContext as _, Context, Entity, SharedString, Subscription};
use rustet_app::settings::{ArtworkSize, MvDownloadQuality, Settings};
use rustet_player::model::StreamingQuality;
use rustet_ui::components::{Select, SelectEvent, Slider, SliderEvent, TextInput, TextInputEvent};

use super::SettingsPage;

/// A select over typed values.
pub struct Choice<T> {
    pub select: Entity<Select>,
    values: Rc<RefCell<Vec<T>>>,
}

impl<T: Copy + PartialEq + 'static> Choice<T> {
    /// `label` is the select's accessible name; `on_change` gets the chosen value.
    pub fn new(
        label: &'static str,
        options: Vec<(T, SharedString)>,
        current: T,
        on_change: impl Fn(&mut SettingsPage, T, &mut Context<SettingsPage>) + 'static,
        subs: &mut Vec<Subscription>,
        cx: &mut Context<SettingsPage>,
    ) -> Self {
        let (values, labels): (Vec<T>, Vec<SharedString>) = options.into_iter().unzip();
        let selected = values.iter().position(|v| *v == current).unwrap_or(0);
        let select = cx.new(|cx| Select::new(label, labels, selected, cx));
        let values = Rc::new(RefCell::new(values));
        let chosen = values.clone();
        subs.push(
            cx.subscribe(&select, move |page, _, SelectEvent::Changed(ix), cx| {
                let value = chosen.borrow().get(*ix).copied();
                if let Some(v) = value {
                    on_change(page, v, cx);
                }
            }),
        );
        Self { select, values }
    }

    /// Replaces the options (a preset was saved or deleted) and shows `current`; emits nothing.
    pub fn set_options(&self, options: Vec<(T, SharedString)>, current: T, cx: &mut gpui::App) {
        let (values, labels): (Vec<T>, Vec<SharedString>) = options.into_iter().unzip();
        let selected = values.iter().position(|v| *v == current).unwrap_or(0);
        *self.values.borrow_mut() = values;
        self.select
            .update(cx, |s, cx| s.set_options(labels, selected, cx));
    }

    /// Shows `value` without emitting a change.
    pub fn show(&self, value: T, cx: &mut gpui::App) {
        let ix = self.values.borrow().iter().position(|v| *v == value);
        if let Some(ix) = ix {
            self.select.update(cx, |s, cx| {
                if s.selected() != ix {
                    s.set_selected(ix, cx);
                }
            });
        }
    }
}

/// A slider over a range of numbers, snapped to `step`. While it moves, `preview` holds the
/// value under the thumb (for the label beside it); on release the value is committed.
pub struct Range {
    pub slider: Entity<Slider>,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub preview: Option<f64>,
}

impl Range {
    pub fn new(
        label: &'static str,
        (min, max, step): (f64, f64, f64),
        current: f64,
        field: fn(&mut Controls) -> Option<&mut Range>,
        on_commit: impl Fn(&mut SettingsPage, f64, &mut Context<SettingsPage>) + 'static,
        subs: &mut Vec<Subscription>,
        cx: &mut Context<SettingsPage>,
    ) -> Self {
        let fraction = Self::fraction_of(min, max, current);
        let slider = cx.new(|cx| Slider::new(fraction, label, cx));
        subs.push(cx.subscribe(&slider, move |page, _, event, cx| {
            let Some(range) = field(&mut page.c) else {
                return;
            };
            match *event {
                SliderEvent::Changed(f) => {
                    range.preview = Some(range.value_at(f));
                    cx.notify();
                }
                SliderEvent::Committed(f) => {
                    let value = range.value_at(f);
                    range.preview = None;
                    on_commit(page, value, cx);
                }
            }
        }));
        Self {
            slider,
            min,
            max,
            step,
            preview: None,
        }
    }

    fn fraction_of(min: f64, max: f64, value: f64) -> f32 {
        (((value - min) / (max - min)).clamp(0., 1.)) as f32
    }

    /// The snapped value at slider position `f` (0–1).
    pub fn value_at(&self, f: f32) -> f64 {
        let raw = self.min + f64::from(f) * (self.max - self.min);
        ((raw - self.min) / self.step)
            .round()
            .mul_add(self.step, self.min)
            .clamp(self.min, self.max)
    }

    /// Shows `value` (ignored while the user drags).
    pub fn show(&self, value: f64, cx: &mut gpui::App) {
        let f = Self::fraction_of(self.min, self.max, value);
        self.slider.update(cx, |s, cx| s.set_value(f, cx));
    }
}

/// A text field whose every edit goes to `on_change`.
pub fn field(
    name: &'static str,
    text: &str,
    on_change: impl Fn(&mut SettingsPage, SharedString, &mut Context<SettingsPage>) + 'static,
    subs: &mut Vec<Subscription>,
    cx: &mut Context<SettingsPage>,
) -> Entity<TextInput> {
    let input = cx.new(|cx| {
        let mut input = TextInput::new(name, cx);
        input.set_text(text.to_owned(), cx);
        input
    });
    subs.push(cx.subscribe(&input, move |page, _, event, cx| {
        if let TextInputEvent::Changed(text) = event {
            on_change(page, text.clone(), cx);
        }
    }));
    input
}

/// Shows `text` in `input` unless it already shows it. Not while the value the user typed is
/// refused: their text stays so they can fix it.
fn show_text(input: &Entity<TextInput>, text: &str, refused: bool, cx: &mut gpui::App) {
    if !refused && input.read(cx).text().as_ref() != text {
        input.update(cx, |i, cx| i.set_text(text.to_owned(), cx));
    }
}

pub const QUALITIES: [(StreamingQuality, &str); 3] = [
    (StreamingQuality::HighQuality, "AAC 256 kbps"),
    (StreamingQuality::Lossless, "Lossless, up to 24-bit 48 kHz"),
    (
        StreamingQuality::HiResLossless,
        "Hi-Res Lossless, up to 24-bit 192 kHz",
    ),
];

/// Song-cache expiry choices in days; 0 is never.
const TTL_DAYS: [u32; 9] = [0, 1, 3, 5, 7, 14, 30, 90, 365];
const ZOOM_PERCENT: [u32; 8] = [75, 90, 100, 110, 125, 150, 175, 200];
/// The song cache slider (MB), as the web panel had it.
pub const SONG_CACHE_MB: (f64, f64, f64) = (100., 10_000., 100.);
/// The engine's limit when none was set.
pub const SONG_CACHE_DEFAULT_MB: u32 = 500;
/// The music-video cache slider (MB).
pub const MV_CACHE_MB: (f64, f64, f64) = (512., 20_480., 512.);

fn ttl_label(days: u32) -> SharedString {
    match days {
        0 => "Never".into(),
        1 => "After 1 day".into(),
        7 => "After 1 week".into(),
        14 => "After 2 weeks".into(),
        30 => "After 1 month".into(),
        90 => "After 3 months".into(),
        365 => "After 1 year".into(),
        n => format!("After {n} days").into(),
    }
}

/// The zoom as whole percent.
pub fn zoom_percent(zoom: f64) -> u32 {
    (zoom * 100.).round() as u32
}

pub struct Controls {
    pub quality: Choice<StreamingQuality>,
    pub mv_height: Choice<u32>,
    pub dl_quality: Choice<StreamingQuality>,
    pub dl_mv: Choice<MvDownloadQuality>,
    pub dl_artwork: Choice<ArtworkSize>,
    pub retry_delay: Choice<u32>,
    pub ttl: Choice<u32>,
    pub zoom: Choice<u32>,
    pub song_cache: Range,
    /// The engine's music-video cache limit; made when its value first arrives.
    pub mv_cache: Option<Range>,
    pub track_template: Entity<TextInput>,
    pub album_template: Entity<TextInput>,
    pub folder: Entity<TextInput>,
    pub ffmpeg_path: Entity<TextInput>,
    pub explicit_text: Entity<TextInput>,
    pub clean_text: Entity<TextInput>,
    pub master_text: Entity<TextInput>,
    pub lastfm_key: Entity<TextInput>,
    pub lastfm_secret: Entity<TextInput>,
    pub listenbrainz_token: Entity<TextInput>,
    /// Light or dark, the preset and the sidebar opacity: rebuilt when the presets change.
    pub theme: Option<super::appearance::ThemeControls>,
}

impl Controls {
    pub fn new(s: &Settings, subs: &mut Vec<Subscription>, cx: &mut Context<SettingsPage>) -> Self {
        let qualities = || QUALITIES.into_iter().map(|(q, l)| (q, l.into())).collect();
        let mut ttl: Vec<(u32, SharedString)> =
            TTL_DAYS.into_iter().map(|d| (d, ttl_label(d))).collect();
        let stored_ttl = s.storage.song_cache_ttl_days.unwrap_or(0);
        if !TTL_DAYS.contains(&stored_ttl) {
            ttl.push((stored_ttl, ttl_label(stored_ttl)));
        }
        let mut zoom: Vec<(u32, SharedString)> = ZOOM_PERCENT
            .into_iter()
            .map(|z| (z, format!("{z} %").into()))
            .collect();
        let stored_zoom = zoom_percent(s.appearance.zoom);
        if !ZOOM_PERCENT.contains(&stored_zoom) {
            zoom.push((stored_zoom, format!("{stored_zoom} %").into()));
        }
        let d = &s.downloads;
        Self {
            quality: Choice::new(
                "Streaming quality",
                qualities(),
                s.playback.quality,
                |p, q, cx| {
                    p.write("quality", |s| s.playback.quality = q, cx);
                },
                subs,
                cx,
            ),
            mv_height: Choice::new(
                "Music-video resolution",
                vec![
                    (2160, "4K (2160p)".into()),
                    (1080, "1080p".into()),
                    (720, "720p".into()),
                    (480, "480p".into()),
                ],
                s.playback.mv_max_height,
                |p, h, cx| {
                    p.write("mv-height", |s| s.playback.mv_max_height = h, cx);
                },
                subs,
                cx,
            ),
            dl_quality: Choice::new(
                "Download quality",
                qualities(),
                d.quality,
                |p, q, cx| {
                    p.write("dl-quality", |s| s.downloads.quality = q, cx);
                },
                subs,
                cx,
            ),
            dl_mv: Choice::new(
                "Music-video download resolution",
                MvDownloadQuality::ALL
                    .into_iter()
                    .map(|q| {
                        let label = match q {
                            MvDownloadQuality::Best => "Best available",
                            MvDownloadQuality::P2160 => "4K (2160p)",
                            MvDownloadQuality::P1080 => "1080p",
                            MvDownloadQuality::P720 => "720p",
                            MvDownloadQuality::P480 => "480p",
                        };
                        (q, label.into())
                    })
                    .collect(),
                d.mv_quality,
                |p, q, cx| {
                    p.write("dl-mv", |s| s.downloads.mv_quality = q, cx);
                },
                subs,
                cx,
            ),
            dl_artwork: Choice::new(
                "Artwork size",
                ArtworkSize::ALL
                    .into_iter()
                    .map(|a| (a, format!("{0} × {0} pixels", a.pixels()).into()))
                    .collect(),
                d.artwork_size,
                |p, a, cx| {
                    p.write("dl-artwork", |s| s.downloads.artwork_size = a, cx);
                },
                subs,
                cx,
            ),
            retry_delay: Choice::new(
                "Retry after",
                vec![
                    (15, "15 seconds".into()),
                    (30, "30 seconds".into()),
                    (60, "1 minute".into()),
                    (300, "5 minutes".into()),
                ],
                d.retry_after_secs,
                |p, secs, cx| {
                    p.write("retry-delay", |s| s.downloads.retry_after_secs = secs, cx);
                },
                subs,
                cx,
            ),
            ttl: Choice::new(
                "Remove unused songs",
                ttl,
                stored_ttl,
                |p, days, cx| {
                    p.write("ttl", |s| s.storage.song_cache_ttl_days = Some(days), cx);
                },
                subs,
                cx,
            ),
            zoom: Choice::new(
                "Zoom",
                zoom,
                stored_zoom,
                |p, z, cx| {
                    p.write("zoom", |s| s.appearance.zoom = f64::from(z) / 100., cx);
                },
                subs,
                cx,
            ),
            song_cache: Range::new(
                "Song cache limit",
                SONG_CACHE_MB,
                f64::from(
                    s.storage
                        .song_cache_limit_mb
                        .unwrap_or(SONG_CACHE_DEFAULT_MB),
                ),
                |c| Some(&mut c.song_cache),
                |p, mb, cx| {
                    p.write(
                        "song-cache",
                        |s| s.storage.song_cache_limit_mb = Some(mb as u32),
                        cx,
                    );
                },
                subs,
                cx,
            ),
            mv_cache: None,
            track_template: field(
                "Track file name",
                &d.track_template,
                |p, t, cx| {
                    p.write_text(
                        "track-template",
                        t,
                        "Enter a file name pattern, for example {track_number:02d} - {title}.",
                        |s, t| s.downloads.track_template = t,
                        cx,
                    );
                },
                subs,
                cx,
            ),
            album_template: field(
                "Album folder name",
                &d.album_template,
                |p, t, cx| {
                    p.write_text(
                        "album-template",
                        t,
                        "Enter a folder pattern, for example {album_artist}/{year} - {album}.",
                        |s, t| s.downloads.album_template = t,
                        cx,
                    );
                },
                subs,
                cx,
            ),
            folder: field(
                "Download folder",
                &d.folder,
                |p, t, cx| {
                    p.write("folder", |s| s.downloads.folder = t.trim().to_owned(), cx);
                },
                subs,
                cx,
            ),
            ffmpeg_path: field(
                "FFmpeg path",
                &d.ffmpeg_path,
                |p, t, cx| {
                    if p.write(
                        "ffmpeg-path",
                        |s| s.downloads.ffmpeg_path = t.trim().to_owned(),
                        cx,
                    ) {
                        p.check_ffmpeg(cx);
                    }
                },
                subs,
                cx,
            ),
            explicit_text: field(
                "Explicit marker",
                &d.explicit_marker.text,
                |p, t, cx| {
                    p.write_text(
                        "explicit-text",
                        t,
                        "Enter the marker text, or turn the marker off.",
                        |s, t| s.downloads.explicit_marker.text = t,
                        cx,
                    );
                },
                subs,
                cx,
            ),
            clean_text: field(
                "Clean marker",
                &d.clean_marker.text,
                |p, t, cx| {
                    p.write_text(
                        "clean-text",
                        t,
                        "Enter the marker text, or turn the marker off.",
                        |s, t| s.downloads.clean_marker.text = t,
                        cx,
                    );
                },
                subs,
                cx,
            ),
            master_text: field(
                "Apple Digital Master marker",
                &d.master_marker.text,
                |p, t, cx| {
                    p.write_text(
                        "master-text",
                        t,
                        "Enter the marker text, or turn the marker off.",
                        |s, t| s.downloads.master_marker.text = t,
                        cx,
                    );
                },
                subs,
                cx,
            ),
            lastfm_key: field("Last.fm API key", "", |_, _, _| {}, subs, cx),
            lastfm_secret: field("Last.fm shared secret", "", |_, _, _| {}, subs, cx),
            listenbrainz_token: field("ListenBrainz user token", "", |_, _, _| {}, subs, cx),
            theme: None,
        }
    }
}

impl SettingsPage {
    /// A text setting that must not be empty: an empty field is refused with `why` (the
    /// model reads an empty value as the default, so it would not stay as typed).
    fn write_text(
        &mut self,
        key: &'static str,
        text: SharedString,
        why: &'static str,
        set: fn(&mut Settings, String),
        cx: &mut Context<Self>,
    ) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            self.refuse(key, why, cx);
        } else {
            self.write(key, |s| set(s, text), cx);
        }
    }

    /// Every control shows the stored value.
    pub(crate) fn sync_controls(&mut self, cx: &mut Context<Self>) {
        let s = self.settings.clone();
        let c = &self.c;
        c.quality.show(s.playback.quality, cx);
        c.mv_height.show(s.playback.mv_max_height, cx);
        c.dl_quality.show(s.downloads.quality, cx);
        c.dl_mv.show(s.downloads.mv_quality, cx);
        c.dl_artwork.show(s.downloads.artwork_size, cx);
        c.retry_delay.show(s.downloads.retry_after_secs, cx);
        c.ttl.show(s.storage.song_cache_ttl_days.unwrap_or(0), cx);
        c.zoom.show(zoom_percent(s.appearance.zoom), cx);
        c.song_cache.show(
            f64::from(
                s.storage
                    .song_cache_limit_mb
                    .unwrap_or(SONG_CACHE_DEFAULT_MB),
            ),
            cx,
        );
        let d = &s.downloads;
        for (input, key, text) in [
            (
                &c.track_template,
                "track-template",
                d.track_template.as_str(),
            ),
            (
                &c.album_template,
                "album-template",
                d.album_template.as_str(),
            ),
            (&c.folder, "folder", d.folder.as_str()),
            (&c.ffmpeg_path, "ffmpeg-path", d.ffmpeg_path.as_str()),
            (
                &c.explicit_text,
                "explicit-text",
                d.explicit_marker.text.as_str(),
            ),
            (&c.clean_text, "clean-text", d.clean_marker.text.as_str()),
            (&c.master_text, "master-text", d.master_marker.text.as_str()),
        ] {
            show_text(input, text, self.refused.contains_key(key), cx);
        }
    }
}
