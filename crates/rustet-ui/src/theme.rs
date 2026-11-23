//! The design tokens (`docs/native-ui-look.md`): colour roles, type, spacing, sizes, radii and
//! motion, as one gpui global every view reads through [`ActiveTheme`]. No view picks a colour,
//! a size or a weight of its own; the crate README lists the tokens and their uses.
//!
//! The tokens follow the user's appearance settings from `rustet_desktop`: light or dark from
//! the saved palette (or the desktop), the preset's accent as the sidebar icon tint, album-art
//! colours (`artTheme`), Reduce animations (`reduceMotion`) and zoom (`zoomFactor`).
//! [`Theme::apply`] swaps the global and repaints every window.
//!
//! A page with artwork colour tints the whole window ([`set_page_tint`]): the base, the
//! sidebar and the text set follow the artwork's `bgColor`. The tint survives
//! [`Theme::apply`], so a settings change does not drop it.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, BoxShadow, FontFeatures, FontWeight, Global, Hsla, Pixels, WindowBackgroundAppearance,
    hsla, point, px,
};
use rustet_app::pages::model::{Palette as ArtPalette, Rgb};
use rustet_desktop::prefs::Prefs;
pub use rustet_desktop::theme::Appearance;
use rustet_desktop::theme::ThemeRender;

/// The bundled family (Inter). One family for everything.
pub const FONT_FAMILY: &str = "Inter";

/// The three weights of the type system.
pub const REGULAR: FontWeight = FontWeight::NORMAL;
pub const SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;
pub const BOLD: FontWeight = FontWeight::BOLD;

/// A `0xRRGGBB` value as a colour.
fn rgb_hex(v: u32) -> Hsla {
    let [_, r, g, b] = v.to_be_bytes();
    rgba8(r, g, b, 1.)
}

fn white(a: f32) -> Hsla {
    hsla(0., 0., 1., a)
}

fn black(a: f32) -> Hsla {
    hsla(0., 0., 0., a)
}

/// Colour roles. Translucent roles (text, fills, hairlines) are white or black at an alpha,
/// so they read on the neutral base and on any page tint alike.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    /// The content area (`#1F1F1F` / white), or the page tint.
    pub base: Hsla,
    /// The sidebar, the right-side panels and sheets.
    pub sidebar: Hsla,
    /// The sidebar's right hairline and the panels' left hairline.
    pub sidebar_border: Hsla,
    /// The selected sidebar item, over the sidebar.
    pub selected: Hsla,
    /// Titles, row titles, active glyphs (white / black 85 %).
    pub text: Hsla,
    /// Artist lines, durations, track numbers, shelf chevrons, quality marks.
    pub text_secondary: Hsla,
    /// Disabled; the idle player's glyphs.
    pub text_tertiary: Hsla,
    /// Cards, fields, quiet button fills.
    pub fill: Hsla,
    /// Row hairlines.
    pub divider: Hsla,
    /// Track rows and tiles under the pointer.
    pub hover: Hsla,
    /// Primary pill buttons and links.
    pub key: Hsla,
    /// Text and glyphs on [`Colors::key`]: the base, so a key pill on a tinted page carries the
    /// page's colour, as Apple's do.
    pub on_key: Hsla,
    /// Sidebar icons only: the preset's accent, else Apple's tint.
    pub nav_icon: Hsla,
    /// The 3 px focus ring.
    pub focus: Hsla,
    /// Selected text in a field.
    pub text_selection: Hsla,
    /// Player pill, round page buttons, menus, toasts (near-opaque: gpui cannot blur behind).
    pub glass: Hsla,
    /// The 1 px stroke around glass.
    pub glass_stroke: Hsla,
    /// The shadow under glass.
    pub glass_shadow: Hsla,
    /// "LIVE" badges only.
    pub live: Hsla,
    /// A switch that is on.
    pub switch_on: Hsla,
    /// A switch's knob (white in both appearances).
    pub switch_knob: Hsla,
    /// Text and glyphs over a picture (white): the artist name, editorial captions.
    pub on_artwork: Hsla,
    /// Behind sheets and dialogs.
    pub scrim: Hsla,
    /// Over a tile's artwork under the pointer (black 20 %).
    pub artwork_dim: Hsla,
    /// An image's place before it loads, when the artwork gives no colour.
    pub placeholder: Hsla,
}

/// Which text set reads on a background: white text on dark colours, black on light ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSet {
    White,
    Black,
}

impl From<Appearance> for TextSet {
    fn from(a: Appearance) -> Self {
        match a {
            Appearance::Dark => Self::White,
            Appearance::Light => Self::Black,
        }
    }
}

/// A page's colour from its artwork (album-art colours on): it becomes the window's base.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageTint {
    /// The artwork's `bgColor`: the whole page background.
    pub background: Hsla,
    /// The sidebar: the background 8 % darker.
    pub sidebar: Hsla,
    /// The text set that reads on the background (by its luminance).
    pub text: TextSet,
}

/// The type scale: 10 / 11 / 12 / 13 / 15 / 17 / 22 / 26 / 34 px, weights 400 / 600 / 700.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeScale {
    /// 10: times in the player, the explicit badge.
    pub footnote: Pixels,
    /// 11: eyebrows (uppercase 600), sidebar section heads.
    pub subhead: Pixels,
    /// 12: tile titles and subtitles, artist under a row title, album meta.
    pub callout: Pixels,
    /// 13: track titles, descriptions, menus, the player's title.
    pub body: Pixels,
    /// 15: sidebar labels, buttons, card titles.
    pub title3: Pixels,
    /// 17: shelf heads (700); editorial hero titles (400).
    pub title2: Pixels,
    /// 22: sheet titles.
    pub title1: Pixels,
    /// 26: album and playlist titles (700) and their artist line (400).
    pub large_title: Pixels,
    /// 34: page titles ("New", "Radio", Settings).
    pub page_title: Pixels,
    /// 40: the artist name over the artist hero.
    pub hero: Pixels,
    /// Line height of body text set in paragraphs (descriptions, lyrics notes).
    pub body_line: Pixels,
    /// Line height of page titles.
    pub page_title_line: Pixels,
    /// Line height of large titles set in paragraphs (lyrics).
    pub large_title_line: Pixels,
    /// Line height as a multiple of the size, for every other line (`relative(leading)`).
    pub leading: f32,
}

/// The spacing scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    /// 2
    pub xxs: Pixels,
    /// 4
    pub xs: Pixels,
    /// 6: under tile artwork, menu padding.
    pub s6: Pixels,
    /// 8
    pub s: Pixels,
    /// 12
    pub m: Pixels,
    /// 16
    pub m16: Pixels,
    /// 20: between tiles and song-grid columns.
    pub l: Pixels,
    /// 28
    pub l28: Pixels,
    /// 40: the content's side gutter.
    pub xl: Pixels,
    /// 48: between page sections.
    pub xxl: Pixels,
}

/// Fixed sizes of the layout and the controls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sizes {
    /// 1: hairlines.
    pub hairline: Pixels,
    /// 260: the sidebar.
    pub sidebar: Pixels,
    /// 64: the sidebar as a column of icons (narrow windows).
    pub rail: Pixels,
    /// 52: the transparent strip over the content (back, forward, window buttons).
    pub strip: Pixels,
    /// 320: the right-side panel (queue, lyrics).
    pub panel: Pixels,
    /// 56: the player pill's height.
    pub player_height: Pixels,
    /// 668: the player pill's maximum width.
    pub player_max_width: Pixels,
    /// 18: the player pill's distance from the window's bottom edge.
    pub player_bottom: Pixels,
    /// Bottom padding a page adds so its last row clears the player pill.
    pub player_clearance: Pixels,
    /// 220: the narrowest the player's "LCD" (artwork, title, artist line) gets; the pill
    /// drops controls before the LCD gets narrower.
    pub player_lcd_min: Pixels,
    /// 96: the player's volume slider, beside the speaker.
    pub player_volume: Pixels,
    /// 36: a sidebar item.
    pub nav_item: Pixels,
    /// 4: between sidebar items (40 px pitch).
    pub nav_gap: Pixels,
    /// 80: where the sidebar's navigation starts.
    pub nav_top: Pixels,
    /// 20 and 13: a sidebar item's left and right inset in the sidebar.
    pub nav_inset_left: Pixels,
    pub nav_inset_right: Pixels,
    /// 10: the padding before a sidebar item's icon.
    pub nav_icon_pad: Pixels,
    /// 28 and 30: the "Rustet" wordmark's left and top in the sidebar.
    pub wordmark_left: Pixels,
    pub wordmark_top: Pixels,
    /// 28: a quiet pill, a select.
    pub control: Pixels,
    /// 36: a large pill (a header's Play), a sidebar Sign In.
    pub button: Pixels,
    /// 36: a text field.
    pub field: Pixels,
    /// 38: the search field.
    pub search: Pixels,
    /// 500: the search field's maximum width.
    pub search_max_width: Pixels,
    /// 46: a track-list row.
    pub row: Pixels,
    /// 55: a song-grid row.
    pub song_row: Pixels,
    /// 44: a settings row.
    pub settings_row: Pixels,
    /// 30: a menu row.
    pub menu_row: Pixels,
    /// 200: the narrowest menu or select.
    pub menu_min_width: Pixels,
    /// 118: a search "Top Results" card.
    pub card: Pixels,
    /// 28: back, forward, a tile's play and more circles.
    pub circle_small: Pixels,
    /// 36: shelf paddles, a header's add, download and more.
    pub circle: Pixels,
    /// 48: the artist hero's side circles.
    pub circle_large: Pixels,
    /// 72: the artist hero's play circle.
    pub key_circle: Pixels,
    /// 16: transport glyphs other than play, row glyphs.
    pub icon_small: Pixels,
    /// 18: sidebar glyphs.
    pub icon: Pixels,
    /// 20: the play glyph.
    pub icon_large: Pixels,
    /// 40: artwork in rows, the song grid and the player.
    pub art_row: Pixels,
    /// 90: artwork in a search card.
    pub art_card: Pixels,
    /// 120: an artist circle in search results.
    pub art_artist: Pixels,
    /// 270: an album or playlist header's artwork.
    pub art_header: Pixels,
    /// 34: between a header's artwork and its text.
    pub header_gap: Pixels,
    /// 20: a track list's number column.
    pub number_column: Pixels,
    /// 52: a track list's duration column.
    pub duration_column: Pixels,
    /// 720: the narrowest track list that shows the album column.
    pub album_column_min: Pixels,
    /// 320: the tallest list inside a sheet (the playlist picker).
    pub sheet_list_max_height: Pixels,
    /// 180: the narrowest regular tile (five fit 1100 px).
    pub tile_min: Pixels,
    /// 400: the narrowest editorial tile (two fit 1100 px).
    pub tile_large_min: Pixels,
    /// 300: the narrowest song-grid column (three fit 1100 px).
    pub song_column_min: Pixels,
    /// 51 × 31: a switch.
    pub switch_width: Pixels,
    pub switch_height: Pixels,
    /// 14: the explicit badge's height.
    pub badge: Pixels,
    /// 3: the focus ring's width; 2: its gap from the element.
    pub focus_ring: Pixels,
    pub focus_gap: Pixels,
    /// 3: the player's progress line; 4: a slider's track; 12: a slider's knob.
    pub progress: Pixels,
    pub slider_track: Pixels,
    pub slider_knob: Pixels,
    /// 42 × 52: a digit of the code input.
    pub code_width: Pixels,
    pub code_height: Pixels,
    /// 520: the widest empty or error state.
    pub state_max_width: Pixels,
    /// 440: the widest toast.
    pub toast_max_width: Pixels,
    /// 640: the widest block of notes under a header.
    pub notes_max_width: Pixels,
    /// 820: the widest lyric line.
    pub lyrics_max_width: Pixels,
    /// 120 and 320: the space above the first lyric line and below the last.
    pub lyrics_lead: Pixels,
    pub lyrics_tail: Pixels,
    /// 760: the widest column of settings groups.
    pub settings_max_width: Pixels,
    /// 180: the narrowest a settings row's label gets before its control moves under it.
    pub settings_label: Pixels,
    /// 280: a text field, or a slider with its value, in a settings row.
    pub settings_field: Pixels,
    /// 64: the value beside a settings slider, the narrowest shortcut-key pill.
    pub settings_value: Pixels,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radii {
    /// 3: the explicit badge, small artwork in rows.
    pub small: Pixels,
    /// 4: the player's artwork.
    pub thumb: Pixels,
    /// 6: hover fills of rows and menu rows.
    pub medium: Pixels,
    /// 7: tile artwork.
    pub tile: Pixels,
    /// 8: cards, fields, sidebar items, header artwork.
    pub card: Pixels,
    /// 10: menus, settings groups, editorial pictures.
    pub menu: Pixels,
    /// 14: sheets and dialogs.
    pub sheet: Pixels,
}

/// What a motion answers. Only actions animate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Page push: slide and fade.
    PagePush,
    /// Sheet and dialog rise.
    SheetRise,
    /// The quality strip's crossfade.
    QualityChange,
    /// Artwork fading in on its first load.
    ArtworkFade,
    /// A toast arriving.
    Toast,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub appearance: Appearance,
    /// Window blur (theme mode Blur): the base is translucent.
    pub translucent: bool,
    pub colors: Colors,
    pub text: TypeScale,
    pub space: Spacing,
    pub size: Sizes,
    pub radius: Radii,
    /// The preset's accent, when it is not the default palette's: it tints the sidebar icons.
    pub accent: Option<Hsla>,
    /// The page tint in effect ([`set_page_tint`]).
    pub tint: Option<PageTint>,
    /// Album-art colours: pages take their background from the artwork.
    pub art_colors: bool,
    /// Reduce animations: every [`Motion`] is instant.
    pub reduce_motion: bool,
    /// The zoom every size was multiplied by.
    pub zoom: f32,
    /// Light or dark follows the desktop (Settings → Appearance "System"); see
    /// [`follow_system_appearance`].
    pub follows_system: bool,
}

impl Global for Theme {}

/// Light or dark from the desktop's setting, as gpui reports it (on Linux the XDG desktop
/// portal's `color-scheme`; no preference reads as light).
pub fn system_appearance(appearance: gpui::WindowAppearance) -> Appearance {
    match appearance {
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => Appearance::Dark,
        gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => Appearance::Light,
    }
}

/// Keeps a theme that follows the desktop in step with it while `window` is open: when the
/// desktop switches between light and dark, the theme switches and every window repaints.
pub fn follow_system_appearance(window: &mut gpui::Window) {
    window
        .observe_window_appearance(|window, cx| {
            let theme = cx.theme();
            let wanted = system_appearance(window.appearance());
            if theme.follows_system && theme.appearance != wanted {
                theme.with_appearance(wanted).apply(cx);
            }
        })
        .detach();
}

/// Gives the window the page's artwork colour (`palette`), or the neutral look for `None`.
/// Pages call it when they show (album, playlist, artist: their artwork's palette; every
/// other page: `None`). Nothing happens with album-art colours off or when the tint is
/// already in effect.
pub fn set_page_tint(palette: Option<&ArtPalette>, cx: &mut App) {
    let theme = cx.theme();
    let tint = theme.page_tint(palette);
    if theme.tint != tint {
        let next = theme.with_tint(tint);
        next.install(cx);
    }
}

impl Theme {
    /// The tokens for an appearance, at zoom 1, with Apple's tint and no page tint.
    pub fn new(appearance: Appearance) -> Self {
        Self::build(appearance, 1., None, None)
    }

    fn build(
        appearance: Appearance,
        zoom: f32,
        accent: Option<Hsla>,
        tint: Option<PageTint>,
    ) -> Self {
        let z = |v: f32| px(v * zoom);
        let set = tint.map_or(appearance.into(), |t| t.text);
        let (base, sidebar, sidebar_border) = match (tint, appearance) {
            (Some(t), _) => (t.background, t.sidebar, overlay(set, 0.06)),
            (None, Appearance::Dark) => (rgb_hex(0x1F1F1F), rgb_hex(0x232324), rgb_hex(0x2E2E2F)),
            (None, Appearance::Light) => (rgb_hex(0xFFFFFF), rgb_hex(0xFAFAFB), black(0.10)),
        };
        // Glass is the base a step lighter: `#2D2D2D` on `#1F1F1F`, white on white, and the
        // same step on a page tint.
        let glass = Hsla {
            l: (base.l + 0.055).min(1.),
            a: 0.97,
            ..base
        };
        let colors = match set {
            TextSet::White => Colors {
                base,
                sidebar,
                sidebar_border,
                selected: white(0.055),
                text: white(0.85),
                text_secondary: white(0.55),
                text_tertiary: white(0.25),
                fill: white(0.10),
                divider: white(0.10),
                hover: white(0.10),
                key: white(0.85),
                on_key: base,
                nav_icon: accent.unwrap_or_else(|| rgb_hex(0x5AC8F5)),
                focus: Hsla {
                    a: 0.6,
                    ..rgb_hex(0x0A84FF)
                },
                text_selection: Hsla {
                    a: 0.4,
                    ..rgb_hex(0x0A84FF)
                },
                glass,
                glass_stroke: white(0.20),
                glass_shadow: black(0.35),
                live: rgb_hex(0xFF2D55),
                switch_on: rgb_hex(0x30D158),
                switch_knob: white(1.),
                on_artwork: white(1.),
                scrim: black(0.60),
                artwork_dim: black(0.20),
                placeholder: white(0.08),
            },
            TextSet::Black => Colors {
                base,
                sidebar,
                sidebar_border,
                selected: black(0.036),
                text: black(0.85),
                text_secondary: black(0.50),
                text_tertiary: black(0.25),
                fill: black(0.10),
                divider: black(0.15),
                hover: black(0.10),
                key: black(0.85),
                on_key: base,
                nav_icon: accent.unwrap_or_else(|| rgb_hex(0x007AFF)),
                focus: Hsla {
                    a: 0.6,
                    ..rgb_hex(0x007AFF)
                },
                text_selection: Hsla {
                    a: 0.25,
                    ..rgb_hex(0x007AFF)
                },
                glass,
                glass_stroke: black(0.05),
                glass_shadow: black(0.10),
                live: rgb_hex(0xFF2D55),
                switch_on: rgb_hex(0x34C759),
                switch_knob: white(1.),
                on_artwork: white(1.),
                scrim: black(0.40),
                artwork_dim: black(0.20),
                placeholder: black(0.06),
            },
        };
        let player_height = 56.;
        let player_bottom = 18.;
        Self {
            appearance,
            translucent: false,
            colors,
            text: TypeScale {
                footnote: z(10.),
                subhead: z(11.),
                callout: z(12.),
                body: z(13.),
                title3: z(15.),
                title2: z(17.),
                title1: z(22.),
                large_title: z(26.),
                page_title: z(34.),
                hero: z(40.),
                body_line: z(18.),
                page_title_line: z(41.),
                large_title_line: z(32.),
                leading: 1.25,
            },
            space: Spacing {
                xxs: z(2.),
                xs: z(4.),
                s6: z(6.),
                s: z(8.),
                m: z(12.),
                m16: z(16.),
                l: z(20.),
                l28: z(28.),
                xl: z(40.),
                xxl: z(48.),
            },
            size: Sizes {
                hairline: z(1.),
                sidebar: z(260.),
                rail: z(64.),
                strip: z(52.),
                panel: z(320.),
                player_height: z(player_height),
                player_max_width: z(668.),
                player_bottom: z(player_bottom),
                player_clearance: z(player_height + player_bottom + 26.),
                player_lcd_min: z(220.),
                player_volume: z(96.),
                nav_item: z(36.),
                nav_gap: z(4.),
                nav_top: z(80.),
                nav_inset_left: z(20.),
                nav_inset_right: z(13.),
                nav_icon_pad: z(10.),
                wordmark_left: z(28.),
                wordmark_top: z(30.),
                control: z(28.),
                button: z(36.),
                field: z(36.),
                search: z(38.),
                search_max_width: z(500.),
                row: z(46.),
                song_row: z(55.),
                settings_row: z(44.),
                menu_row: z(30.),
                menu_min_width: z(200.),
                card: z(118.),
                circle_small: z(28.),
                circle: z(36.),
                circle_large: z(48.),
                key_circle: z(72.),
                icon_small: z(16.),
                icon: z(18.),
                icon_large: z(20.),
                art_row: z(40.),
                art_card: z(90.),
                art_artist: z(120.),
                art_header: z(270.),
                header_gap: z(34.),
                number_column: z(20.),
                duration_column: z(52.),
                album_column_min: z(720.),
                sheet_list_max_height: z(320.),
                tile_min: z(180.),
                tile_large_min: z(400.),
                song_column_min: z(300.),
                switch_width: z(51.),
                switch_height: z(31.),
                badge: z(14.),
                focus_ring: z(3.),
                focus_gap: z(2.),
                progress: z(3.),
                slider_track: z(4.),
                slider_knob: z(12.),
                code_width: z(42.),
                code_height: z(52.),
                state_max_width: z(520.),
                toast_max_width: z(440.),
                notes_max_width: z(640.),
                lyrics_max_width: z(820.),
                lyrics_lead: z(120.),
                lyrics_tail: z(320.),
                settings_max_width: z(760.),
                settings_label: z(180.),
                settings_field: z(280.),
                settings_value: z(64.),
            },
            radius: Radii {
                small: z(3.),
                thumb: z(4.),
                medium: z(6.),
                tile: z(7.),
                card: z(8.),
                menu: z(10.),
                sheet: z(14.),
            },
            accent,
            tint,
            art_colors: true,
            reduce_motion: false,
            zoom,
            follows_system: false,
        }
    }

    /// The same settings in the other appearance.
    pub fn with_appearance(&self, appearance: Appearance) -> Self {
        Self {
            translucent: self.translucent,
            art_colors: self.art_colors,
            reduce_motion: self.reduce_motion,
            follows_system: self.follows_system,
            ..Self::build(appearance, self.zoom, self.accent, self.tint)
        }
    }

    /// The same settings with `tint` as the page tint (`None`: the neutral look).
    pub fn with_tint(&self, tint: Option<PageTint>) -> Self {
        Self {
            translucent: self.translucent,
            art_colors: self.art_colors,
            reduce_motion: self.reduce_motion,
            follows_system: self.follows_system,
            ..Self::build(self.appearance, self.zoom, self.accent, tint)
        }
    }

    /// The tokens for the user's settings: `render` from `ThemeService::render` (appearance,
    /// accent, Blur mode, follow the system) and the prefs (`artTheme`, `reduceMotion`,
    /// `zoomFactor`). `system` is the desktop's appearance (`cx.window_appearance()`), used
    /// when the user follows it. The default palette's accent means "Apple's tint", which
    /// differs between light and dark.
    pub fn from_settings(
        render: &ThemeRender,
        prefs: &Prefs,
        system: gpui::WindowAppearance,
    ) -> Self {
        let vars = render.vars.as_ref();
        let appearance = if render.follows_system {
            system_appearance(system)
        } else {
            vars.map_or(Appearance::Dark, |v| v.appearance)
        };
        let default_accent = rustet_desktop::theme::default_palette()
            .accent
            .as_deref()
            .and_then(parse_color);
        let accent = vars
            .and_then(|v| v.vars.iter().find(|(n, _)| *n == "--rustet-accent"))
            .and_then(|(_, v)| parse_color(v))
            .filter(|a| Some(*a) != default_accent);
        let zoom = prefs
            .zoom_factor
            .filter(|z| z.is_finite())
            .unwrap_or(1.)
            .clamp(0.5, 3.) as f32;
        Self {
            translucent: render.vars.is_none(),
            // Missing means on: the web player's default.
            art_colors: prefs.extra.get("artTheme") != Some(&serde_json::Value::Bool(false)),
            reduce_motion: prefs.reduce_motion(),
            follows_system: render.follows_system,
            ..Self::build(appearance, zoom, accent, None)
        }
    }

    /// The tint a page with artwork `palette` gives the window; `None` with album-art colours
    /// off or without a palette.
    pub fn page_tint(&self, palette: Option<&ArtPalette>) -> Option<PageTint> {
        let p = palette.filter(|_| self.art_colors)?;
        let background = rgb(p.background);
        // Darkened per channel: lowering HSL lightness keeps the saturation, which turns a
        // near-white sleeve's sidebar lavender.
        let darker = |v: u8| (f32::from(v) * 0.92) as u8;
        let Rgb { r, g, b } = p.background;
        Some(PageTint {
            background,
            sidebar: rgba8(darker(r), darker(g), darker(b), 1.),
            text: if relative_luminance(background) > 0.4 {
                TextSet::Black
            } else {
                TextSet::White
            },
        })
    }

    /// Text on a fill: white or black, whichever reads.
    pub fn on(&self, fill: Hsla) -> Hsla {
        if relative_luminance(fill) > 0.4 {
            black(0.85)
        } else {
            white(1.)
        }
    }

    /// An artwork's place before the image arrives: its own colour when Apple gives one.
    pub fn placeholder(&self, palette: Option<&ArtPalette>) -> Hsla {
        palette.map_or(self.colors.placeholder, |p| rgb(p.background))
    }

    /// How long `motion` takes; zero with Reduce animations.
    pub fn motion(&self, motion: Motion) -> Duration {
        if self.reduce_motion {
            return Duration::ZERO;
        }
        Duration::from_millis(match motion {
            Motion::PagePush => 120,
            Motion::SheetRise => 160,
            Motion::QualityChange => 200,
            Motion::ArtworkFade => 240,
            Motion::Toast => 160,
        })
    }

    /// Tabular figures, for times, track numbers and sample rates.
    pub fn tabular(&self) -> FontFeatures {
        FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
    }

    /// The focus ring: 3 px of the focus colour, 2 px out from the element, drawn as shadows
    /// (no layout shift). The gap is painted in `surface`, the colour around the element; the
    /// shadows also cover the element's own area, so the element shows `surface` unless it
    /// has a fill of its own.
    pub fn focus_ring(&self, surface: Hsla) -> Vec<BoxShadow> {
        let ring = |color: Hsla, spread: Pixels| BoxShadow {
            color,
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: spread,
            inset: false,
        };
        let gap = self.size.focus_gap;
        vec![
            ring(surface.blend(self.colors.focus), gap + self.size.focus_ring),
            ring(surface, gap),
        ]
    }

    /// The shadow under glass: `0 10px 40px`.
    pub fn glass_shadow(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: self.colors.glass_shadow,
            offset: point(px(0.), px(10. * self.zoom)),
            blur_radius: px(40. * self.zoom),
            spread_radius: px(0.),
            inset: false,
        }]
    }

    /// The shadow under a header's artwork: `0 10px 30px` black 30 %.
    pub fn artwork_shadow(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: black(0.30),
            offset: point(px(0.), px(10. * self.zoom)),
            blur_radius: px(30. * self.zoom),
            spread_radius: px(0.),
            inset: false,
        }]
    }

    /// The window background: blurred in Blur mode, transparent behind client-side
    /// decorations (their shadow band), opaque otherwise.
    pub fn window_background(&self, client_decorated: bool) -> WindowBackgroundAppearance {
        match (self.translucent, client_decorated) {
            (true, _) => WindowBackgroundAppearance::Blurred,
            (false, true) => WindowBackgroundAppearance::Transparent,
            (false, false) => WindowBackgroundAppearance::Opaque,
        }
    }

    /// The window base, translucent in Blur mode.
    pub fn base(&self) -> Hsla {
        if self.translucent {
            Hsla {
                a: 0.72,
                ..self.colors.base
            }
        } else {
            self.colors.base
        }
    }

    /// Replaces the theme and repaints every window. The page tint in effect stays (while
    /// album-art colours are on), so a settings change keeps the page's colour.
    pub fn apply(self, cx: &mut App) {
        let tint = cx
            .try_global::<Theme>()
            .and_then(|t| t.tint)
            .filter(|_| self.art_colors);
        let next = if tint == self.tint {
            self
        } else {
            self.with_tint(tint)
        };
        next.install(cx);
    }

    fn install(self, cx: &mut App) {
        cx.set_global(self);
        for window in cx.windows() {
            window
                .update(cx, |_, window, cx| {
                    let csd = matches!(
                        window.window_decorations(),
                        gpui::Decorations::Client { .. }
                    );
                    window.set_background_appearance(cx.theme().window_background(csd));
                    window.refresh();
                })
                .ok();
        }
    }
}

/// White or black (by text set) at `alpha`.
fn overlay(set: TextSet, alpha: f32) -> Hsla {
    match set {
        TextSet::White => white(alpha),
        TextSet::Black => black(alpha),
    }
}

/// `cx.theme()` from render code.
pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

/// An `rustet_app` colour as a gpui colour.
pub fn rgb(c: Rgb) -> Hsla {
    rgba8(c.r, c.g, c.b, 1.)
}

fn relative_luminance(c: Hsla) -> f32 {
    let c = gpui::Rgba::from(c);
    let lin = |v: f32| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// Parses the CSS colours the theme model stores: `#rgb`, `#rrggbb`, `#rrggbbaa`,
/// `rgb()`/`rgba()`, `hsl()`/`hsla()`, `transparent`, `black`, `white`.
pub fn parse_color(css: &str) -> Option<Hsla> {
    let s = css.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => return None,
        "transparent" => return Some(hsla(0., 0., 0., 0.)),
        "black" => return Some(hsla(0., 0., 0., 1.)),
        "white" => return Some(hsla(0., 0., 1., 1.)),
        _ => {}
    }
    if let Some(hex) = s.strip_prefix('#') {
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<_>>()?;
        let (r, g, b, a) = match digits[..] {
            [r, g, b] => (r * 17, g * 17, b * 17, 255),
            [r1, r2, g1, g2, b1, b2] => (r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, 255),
            [r1, r2, g1, g2, b1, b2, a1, a2] => {
                (r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, a1 * 16 + a2)
            }
            _ => return None,
        };
        return Some(rgba8(r, g, b, f32::from(a) / 255.));
    }
    let (func, args) = s.strip_suffix(')')?.split_once('(')?;
    let parts: Vec<&str> = args
        .split([',', ' ', '/'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let (channels, alpha) = match parts.len() {
        3 => (&parts[..3], 1.),
        4 => (&parts[..3], unit(parts[3])?),
        _ => return None,
    };
    match func {
        "rgb" | "rgba" => {
            let c = |p: &str| -> Option<u8> {
                let v = match p.strip_suffix('%') {
                    Some(pct) => pct.parse::<f32>().ok()? * 2.55,
                    None => p.parse::<f32>().ok()?,
                };
                Some(v.round().clamp(0., 255.) as u8)
            };
            Some(rgba8(
                c(channels[0])?,
                c(channels[1])?,
                c(channels[2])?,
                alpha,
            ))
        }
        "hsl" | "hsla" => {
            let h = channels[0]
                .trim_end_matches("deg")
                .parse::<f32>()
                .ok()?
                .rem_euclid(360.)
                / 360.;
            let pct =
                |p: &str| Some(p.strip_suffix('%')?.parse::<f32>().ok()?.clamp(0., 100.) / 100.);
            Some(hsla(h, pct(channels[1])?, pct(channels[2])?, alpha))
        }
        _ => None,
    }
}

fn unit(p: &str) -> Option<f32> {
    let v = match p.strip_suffix('%') {
        Some(pct) => pct.parse::<f32>().ok()? / 100.,
        None => p.parse::<f32>().ok()?,
    };
    Some(v.clamp(0., 1.))
}

fn rgba8(r: u8, g: u8, b: u8, a: f32) -> Hsla {
    gpui::Rgba {
        r: f32::from(r) / 255.,
        g: f32::from(g) / 255.,
        b: f32::from(b) / 255.,
        a,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustet_desktop::theme::{Palette, ThemeMode, default_palette, theme_vars};

    fn close(a: Hsla, b: Hsla) -> bool {
        let (a, b) = (gpui::Rgba::from(a), gpui::Rgba::from(b));
        [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)]
            .iter()
            .all(|(x, y)| (x - y).abs() < 0.01)
    }

    fn render(palette: &Palette) -> ThemeRender {
        ThemeRender {
            mode: ThemeMode::Accent,
            vars: Some(theme_vars(palette, None)),
            custom_css: None,
            nav_alpha: 0.72,
            follows_system: false,
        }
    }

    fn art(r: u8, g: u8, b: u8) -> ArtPalette {
        ArtPalette {
            background: Rgb { r, g, b },
            text: [Rgb { r: 0, g: 0, b: 0 }; 4],
        }
    }

    #[test]
    fn parses_the_colours_the_theme_model_stores() {
        let red = rgba8(0xfc, 0x3c, 0x44, 1.);
        assert!(close(parse_color("#fc3c44").unwrap(), red));
        assert!(close(
            parse_color("rgba(252,60,68,0.72)").unwrap(),
            Hsla { a: 0.72, ..red }
        ));
        assert!(close(
            parse_color("hsla(358, 87%, 60%, 0.28)").unwrap(),
            hsla(358. / 360., 0.87, 0.6, 0.28)
        ));
        for bad in ["", "#12", "url(x)", "rgb(1,2)", "red"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn settings_drive_appearance_accent_motion_and_zoom() {
        let palette = Palette {
            appearance: Some(Appearance::Light),
            accent: Some("#fc3c44".into()),
            ..default_palette()
        };
        let mut prefs = Prefs {
            zoom_factor: Some(1.25),
            ..Prefs::default()
        };
        prefs
            .extra
            .insert("reduceMotion".into(), serde_json::Value::Bool(true));
        prefs
            .extra
            .insert("artTheme".into(), serde_json::Value::Bool(false));
        let theme = Theme::from_settings(&render(&palette), &prefs, gpui::WindowAppearance::Dark);
        assert_eq!(theme.appearance, Appearance::Light);
        assert!(close(theme.colors.base, rgb_hex(0xFFFFFF)));
        // A preset's own accent tints the sidebar icons.
        assert!(close(
            theme.colors.nav_icon,
            parse_color("#fc3c44").unwrap()
        ));
        assert_eq!(theme.text.body, px(13. * 1.25));
        assert_eq!(theme.size.row, px(46. * 1.25));
        assert_eq!(theme.motion(Motion::QualityChange), Duration::ZERO);
        // Album-art colours off: no page tint.
        assert_eq!(theme.page_tint(Some(&art(200, 40, 40))), None);
    }

    #[test]
    fn the_default_palette_takes_apples_tint_for_each_appearance() {
        let dark = Theme::from_settings(
            &render(&default_palette()),
            &Prefs::default(),
            gpui::WindowAppearance::Dark,
        );
        assert_eq!(dark.accent, None);
        assert!(close(dark.colors.nav_icon, rgb_hex(0x5AC8F5)));
        assert!(close(dark.colors.base, rgb_hex(0x1F1F1F)));
        let light = dark.with_appearance(Appearance::Light);
        assert!(close(light.colors.nav_icon, rgb_hex(0x007AFF)));
    }

    #[test]
    fn system_appearance_wins_when_followed() {
        let dark_palette = default_palette();
        let mut followed = render(&dark_palette);
        followed.follows_system = true;
        let theme =
            Theme::from_settings(&followed, &Prefs::default(), gpui::WindowAppearance::Light);
        assert_eq!(theme.appearance, Appearance::Light);
        assert!(theme.follows_system);
        // Switching keeps every other setting.
        let back = theme.with_appearance(Appearance::Dark);
        assert_eq!(
            (back.appearance, back.accent, back.follows_system),
            (Appearance::Dark, theme.accent, true)
        );
        // Not followed: the palette's own appearance.
        let own = Theme::from_settings(
            &render(&dark_palette),
            &Prefs::default(),
            gpui::WindowAppearance::Light,
        );
        assert_eq!(own.appearance, Appearance::Dark);
    }

    #[test]
    fn a_page_tint_sets_the_base_sidebar_and_text_set() {
        let light = Theme::new(Appearance::Light);
        // A dark navy sleeve in light mode: the window turns navy with the white text set.
        let navy = light.page_tint(Some(&art(30, 40, 80))).unwrap();
        assert_eq!(navy.text, TextSet::White);
        assert!(navy.sidebar.l < navy.background.l);
        let tinted = light.with_tint(Some(navy));
        assert!(close(
            tinted.colors.base,
            rgb(Rgb {
                r: 30,
                g: 40,
                b: 80
            })
        ));
        assert!(close(tinted.colors.text, white(0.85)));
        assert_eq!(tinted.appearance, Appearance::Light);
        // A pale sleeve takes the black set.
        let pale = light.page_tint(Some(&art(240, 230, 200))).unwrap();
        assert_eq!(pale.text, TextSet::Black);
        // A near-white sleeve keeps a near-white sidebar (not a saturated tint of its hue).
        let white_sleeve = light.page_tint(Some(&art(250, 250, 255))).unwrap();
        assert!(white_sleeve.sidebar.s < 0.2);
        // Back to neutral.
        assert!(close(tinted.with_tint(None).colors.base, rgb_hex(0xFFFFFF)));
    }
}
