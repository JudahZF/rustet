//! The bundled typeface: Inter 400, 600 and 700 (SIL Open Font License 1.1, the licence text
//! is `assets/fonts/OFL.txt` and ships with the binary's notices). Inter is the closest free
//! face to Apple's SF Pro, which Rustet may not ship.
//!
//! Scripts it does not cover (CJK, Arabic, Hebrew, Devanagari, Thai, emoji) fall back to the
//! system's fonts through gpui's text system.

use std::borrow::Cow;

use gpui::{App, IndentAdjustment, LineFragment, px};

use crate::theme::{ActiveTheme, BOLD, FONT_FAMILY, REGULAR, SEMIBOLD};

const FACES: [&[u8]; 3] = [
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
];

/// The font licence, for an "Open-source notices" page.
pub const LICENSE: &str = include_str!("../assets/fonts/OFL.txt");

/// Registers the bundled faces. Call once at start-up, before the first window.
pub fn register(cx: &App) -> gpui::Result<()> {
    cx.text_system()
        .add_fonts(FACES.iter().map(|f| Cow::Borrowed(*f)).collect())
}

/// Text in every script music metadata uses, for [`warm_up`].
const WARM_UP_TEXT: &str =
    "Ágætis 0123456789 宇多田ヒカル 周杰倫 아이유 فيروز עומר अरिजीत ลิซ่า Βασίλης Кино 🌍👩‍🎤";

/// Shapes a line in every script on a background thread at start-up. The first use of a
/// script makes the text system find, read and parse its fallback font (the CJK faces are
/// tens of megabytes); done lazily, that stalls a frame the first time such a title scrolls
/// into view.
pub fn warm_up(cx: &App) {
    let text_system = cx.text_system().clone();
    let size = cx.theme().text.body;
    cx.background_executor()
        .spawn(async move {
            for weight in [REGULAR, SEMIBOLD, BOLD] {
                let font = gpui::Font {
                    weight,
                    ..gpui::font(FONT_FAMILY)
                };
                let mut wrapper = text_system.line_wrapper(font, size);
                let fragments = [LineFragment::text(WARM_UP_TEXT)];
                wrapper
                    .wrap_line(&fragments, px(1.), IndentAdjustment::NoIndent)
                    .for_each(drop);
            }
        })
        .detach();
}
