//! The UI's icons, Rustet's own drawings in Apple's shape language: filled transport glyphs
//! (play, pause, the double triangles of previous and next) and 2-unit rounded strokes on a
//! 24-unit grid for the rest (1.5 px at the 18 px sidebar size). They are compiled into the
//! binary and served to gpui as SVG assets; `svg().text_color()` tints them, so they follow
//! the colour tokens.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString, Styled, Svg, svg};

/// One layer of an icon: a filled path or a stroked one.
#[derive(Debug, Clone, Copy)]
enum Layer {
    Fill(&'static str),
    Stroke(&'static str),
}

use Layer::{Fill, Stroke};

macro_rules! icons {
    ($($variant:ident => $name:literal, [$($layer:expr),+ $(,)?];)*) => {
        /// An icon of the set.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Icon {
            $($variant,)*
        }

        impl Icon {
            /// Every icon, for the gallery.
            pub const ALL: &[Icon] = &[$(Icon::$variant,)*];

            pub fn name(self) -> &'static str {
                match self {
                    $(Icon::$variant => $name,)*
                }
            }

            fn layers(self) -> &'static [Layer] {
                match self {
                    $(Icon::$variant => &[$($layer),+],)*
                }
            }
        }
    };
}

/// The loudspeaker of the volume glyphs.
const SPEAKER: Layer = Fill(
    "M3.5 9.2h3l4.3-3.6c.6-.5 1.5-.1 1.5.7v11.4c0 .8-.9 1.2-1.5.7L6.5 14.8h-3a1 1 0 0 1-1-1v-3.6a1 1 0 0 1 1-1z",
);

icons! {
    Play => "play", [Fill("M7 4.8v14.4c0 .9 1 1.4 1.7.9l11-7.2c.7-.4.7-1.4 0-1.8l-11-7.2C8 3.4 7 3.9 7 4.8z")];
    Pause => "pause", [Fill("M6.5 4h3a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1zm8 0h3a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z")];
    Previous => "previous", [Fill("M21.5 6.6v10.8c0 .7-.8 1.1-1.4.7L12 12.7c-.5-.4-.5-1.1 0-1.4l8.1-5.4c.6-.4 1.4 0 1.4.7zm-10 0v10.8c0 .7-.8 1.1-1.4.7L2 12.7c-.5-.4-.5-1.1 0-1.4l8.1-5.4c.6-.4 1.4 0 1.4.7z")];
    Next => "next", [Fill("M2.5 6.6v10.8c0 .7.8 1.1 1.4.7L12 12.7c.5-.4.5-1.1 0-1.4L3.9 5.9c-.6-.4-1.4 0-1.4.7zm10 0v10.8c0 .7.8 1.1 1.4.7l8.1-5.4c.5-.4.5-1.1 0-1.4l-8.1-5.4c-.6-.4-1.4 0-1.4.7z")];
    Shuffle => "shuffle", [Stroke("M3 6.5h2.6c1.9 0 3.6 1 4.6 2.6l3.6 5.8c1 1.6 2.7 2.6 4.6 2.6H21M3 17.5h2.6c1.9 0 3.6-1 4.6-2.6l.5-.8m2.6-4.2.5-.8c1-1.6 2.7-2.6 4.6-2.6H21M18.5 4 21 6.5 18.5 9M18.5 15l2.5 2.5-2.5 2.5")];
    Repeat => "repeat", [Stroke("M4 11.5v-1.5A4 4 0 0 1 8 6h12M17.5 3.5 20 6l-2.5 2.5M20 12.5V14a4 4 0 0 1-4 4H4M6.5 20.5 4 18l2.5-2.5")];
    Volume => "volume", [SPEAKER, Stroke("M15.5 9a4.2 4.2 0 0 1 0 6M18.3 6.3a8 8 0 0 1 0 11.4")];
    VolumeMuted => "volume-muted", [SPEAKER, Stroke("M16 9.5l5 5M21 9.5l-5 5")];
    Autoplay => "autoplay", [Stroke("M12 12c-1.6-2-3-3.3-4.8-3.3a3.3 3.3 0 0 0 0 6.6c1.8 0 3.2-1.3 4.8-3.3zm0 0c1.6 2 3 3.3 4.8 3.3a3.3 3.3 0 0 0 0-6.6c-1.8 0-3.2 1.3-4.8 3.3z")];
    Folder => "folder", [Stroke("M3.5 7.5A1.5 1.5 0 0 1 5 6h4l2 2h8a1.5 1.5 0 0 1 1.5 1.5v8A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5z")];
    Search => "search", [Stroke("M10.5 4a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13zM15.3 15.3 20 20")];
    Close => "close", [Stroke("M6 6l12 12M18 6 6 18")];
    Minimize => "minimize", [Stroke("M6 12h12")];
    Maximize => "maximize", [Stroke("M8 6.5h8A1.5 1.5 0 0 1 17.5 8v8a1.5 1.5 0 0 1-1.5 1.5H8A1.5 1.5 0 0 1 6.5 16V8A1.5 1.5 0 0 1 8 6.5z")];
    Menu => "menu", [Stroke("M4 7h16M4 12h16M4 17h16")];
    Grip => "grip", [Fill("M9 5.5a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3zm6 0a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3zm-6 5a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3zm6 0a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3zm-6 5a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3zm6 0a1.5 1.5 0 1 1 0 3 1.5 1.5 0 0 1 0-3z")];
    ChevronLeft => "chevron-left", [Stroke("M15 5l-7 7 7 7")];
    ChevronRight => "chevron-right", [Stroke("M9 5l7 7-7 7")];
    ChevronDown => "chevron-down", [Stroke("M5 9l7 7 7-7")];
    Plus => "plus", [Stroke("M12 5v14M5 12h14")];
    Check => "check", [Stroke("M5 12.5l4.5 4.5L19 7.5")];
    Download => "download", [Stroke("M12 4v11M7.5 10.5 12 15l4.5-4.5M5 19.5h14")];
    More => "more", [Fill("M5 10.2a1.8 1.8 0 1 1 0 3.6 1.8 1.8 0 0 1 0-3.6zm7 0a1.8 1.8 0 1 1 0 3.6 1.8 1.8 0 0 1 0-3.6zm7 0a1.8 1.8 0 1 1 0 3.6 1.8 1.8 0 0 1 0-3.6z")];
    Heart => "heart", [Stroke("M12 19.5s-7.5-4.6-7.5-9.7A4.3 4.3 0 0 1 12 7.2a4.3 4.3 0 0 1 7.5 2.6c0 5.1-7.5 9.7-7.5 9.7z")];
    NowPlaying => "now-playing", [Fill("M4.5 10h3v10h-3zM10.5 4h3v16h-3zM16.5 13h3v7h-3z")];
    Lyrics => "lyrics", [
        Stroke("M5 5h14a1.5 1.5 0 0 1 1.5 1.5v9A1.5 1.5 0 0 1 19 17h-8l-4.5 3.5V17H5a1.5 1.5 0 0 1-1.5-1.5v-9A1.5 1.5 0 0 1 5 5z"),
        Fill("M8.3 8.8h2.4v2.4c0 1.3-.6 2.2-1.9 2.5l-.4-.9c.6-.2 1-.6 1-1.2H8.3zm5 0h2.4v2.4c0 1.3-.6 2.2-1.9 2.5l-.4-.9c.6-.2 1-.6 1-1.2h-1.1z"),
    ];
    Queue => "queue", [
        Stroke("M8.5 6.5H20M8.5 12H20M8.5 17.5H20"),
        Fill("M4.5 5.2a1.3 1.3 0 1 1 0 2.6 1.3 1.3 0 0 1 0-2.6zm0 5.5a1.3 1.3 0 1 1 0 2.6 1.3 1.3 0 0 1 0-2.6zm0 5.5a1.3 1.3 0 1 1 0 2.6 1.3 1.3 0 0 1 0-2.6z"),
    ];
    Radio => "radio", [
        Fill("M12 10.2a1.8 1.8 0 1 1 0 3.6 1.8 1.8 0 0 1 0-3.6z"),
        Stroke("M8.6 8.6a4.8 4.8 0 0 0 0 6.8M15.4 8.6a4.8 4.8 0 0 1 0 6.8M5.8 5.8a8.8 8.8 0 0 0 0 12.4M18.2 5.8a8.8 8.8 0 0 1 0 12.4"),
    ];
    Album => "album", [Stroke("M6.5 8.5h11A1.5 1.5 0 0 1 19 10v8.5a1.5 1.5 0 0 1-1.5 1.5h-11A1.5 1.5 0 0 1 5 18.5V10a1.5 1.5 0 0 1 1.5-1.5zM7 5h10")];
    Song => "song", [Stroke("M9 18V6.5l11-2V16M9 18a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0zM20 16a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0z")];
    Artist => "artist", [Stroke("M12 3.5a3 3 0 0 1 3 3V11a3 3 0 0 1-6 0V6.5a3 3 0 0 1 3-3zM6.5 10.5a5.5 5.5 0 0 0 11 0M12 16v4.5M9 20.5h6")];
    Person => "person", [Stroke("M12 4a3.5 3.5 0 1 1 0 7 3.5 3.5 0 0 1 0-7zM5 20c.6-3.4 3.4-5.5 7-5.5s6.4 2.1 7 5.5")];
    Playlist => "playlist", [Stroke("M4 6.5h11M4 11h11M4 15.5h6M18 17.5V8.5l3-1M18 17.5a2 2 0 1 1-4 0 2 2 0 0 1 4 0z")];
    Library => "library", [Stroke("M5 4.5v15M9.5 4.5v15M13.5 5.2l4.6 14")];
    Clock => "clock", [Stroke("M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16zM12 7.5V12l3 2")];
    Home => "home", [Stroke("M4.5 10.5 12 4.5l7.5 6V19a1 1 0 0 1-1 1h-4v-5.5h-5V20h-4a1 1 0 0 1-1-1z")];
    New => "new", [Stroke("M6 5h3.5a1 1 0 0 1 1 1v3.5a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zm8.5 0H18a1 1 0 0 1 1 1v3.5a1 1 0 0 1-1 1h-3.5a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zM6 13.5h3.5a1 1 0 0 1 1 1V18a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1v-3.5a1 1 0 0 1 1-1zm8.5 0H18a1 1 0 0 1 1 1V18a1 1 0 0 1-1 1h-3.5a1 1 0 0 1-1-1v-3.5a1 1 0 0 1 1-1z")];
    Settings => "settings", [Stroke("M18.38 10.33 20.54 10.99 20.54 13.01 18.38 13.67 17.7 15.33 18.75 17.32 17.32 18.75 15.33 17.7 13.67 18.38 13.01 20.54 10.99 20.54 10.33 18.38 8.67 17.7 6.68 18.75 5.25 17.32 6.3 15.33 5.62 13.67 3.46 13.01 3.46 10.99 5.62 10.33 6.3 8.67 5.25 6.68 6.68 5.25 8.67 6.3 10.33 5.62 10.99 3.46 13.01 3.46 13.67 5.62 15.33 6.3 17.32 5.25 18.75 6.68 17.7 8.67zM12 9.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5z")];
    Alert => "alert", [
        Stroke("M10.7 5.2a1.5 1.5 0 0 1 2.6 0l7 12.3a1.5 1.5 0 0 1-1.3 2.2H5a1.5 1.5 0 0 1-1.3-2.2zM12 9.5v4"),
        Fill("M12 15.6a1.1 1.1 0 1 1 0 2.2 1.1 1.1 0 0 1 0-2.2z"),
    ];
    MiniPlayer => "mini-player", [
        Stroke("M5 5h14a1.5 1.5 0 0 1 1.5 1.5v11A1.5 1.5 0 0 1 19 19H5a1.5 1.5 0 0 1-1.5-1.5v-11A1.5 1.5 0 0 1 5 5z"),
        Fill("M12.5 12h5.5a.5.5 0 0 1 .5.5v4a.5.5 0 0 1-.5.5h-5.5a.5.5 0 0 1-.5-.5v-4a.5.5 0 0 1 .5-.5z"),
    ];
}

impl Icon {
    fn asset_path(self) -> SharedString {
        format!("icons/{}.svg", self.name()).into()
    }

    /// The icon as an element; size and colour from the caller (`.size(..).text_color(..)`).
    pub fn element(self) -> Svg {
        svg().path(self.asset_path()).flex_none()
    }

    fn svg(self) -> String {
        let body: String = self
            .layers()
            .iter()
            .map(|layer| match layer {
                Fill(d) => format!(r#"<path fill="black" fill-rule="evenodd" d="{d}"/>"#),
                Stroke(d) => format!(
                    r#"<path fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" d="{d}"/>"#
                ),
            })
            .collect();
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">{body}</svg>"#)
    }
}

/// The asset source to pass to `Application::with_assets`: the icons.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(Icon::ALL
            .iter()
            .find(|i| i.asset_path() == path)
            .map(|icon| Cow::Owned(icon.svg().into_bytes())))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Icon::ALL
            .iter()
            .map(|i| i.asset_path())
            .filter(|p| p.starts_with(path))
            .collect())
    }
}
