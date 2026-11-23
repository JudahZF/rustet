//! Rustet's own user interface, built with gpui (pinned Zed revision, see the README).
//!
//! The foundations the shell and the pages are built on:
//!
//! - [`theme`]: the design tokens (colour roles, type scale, spacing, radii, motion) and how
//!   the user's appearance settings drive them. Every view reads them; none invents its own.
//! - [`fonts`]: the bundled Inter.
//! - [`icons`]: the icon set and the asset source.
//! - [`components`]: buttons, toggle, select, slider, text input, search field, code input,
//!   sheet, dialog, toasts, tooltip, context menu, scroll bar, empty, error and loading states.
//! - [`content`]: views over `rustet_app`'s page models: artwork, tiles, shelves, grid, track
//!   list, page header, quality strip, lyrics.
//! - [`frame`]: the main window's frame (regions, client-side decorations, header).
//! - [`focus`]: Tab order, focus traps in modals, one tab stop per list.
//! - [`host`]: how a page asks the window for a menu, a modal or a toast.
//! - [`artwork`], [`backdrop`], [`video`]: images, the blurred page background, a music video
//!   from the engine's frame ring.
//! - [`runtime`]: tokio inside gpui.
//! - [`window_state`], [`platform`]: window persistence and always-on-top on X11.
//! - [`format`], [`grid`], [`reorder`], [`frame_stats`]: text formats, grid geometry,
//!   drag-to-reorder, frame timing.
#![forbid(unsafe_code)]

pub mod artwork;
pub mod backdrop;
pub mod components;
pub mod content;
pub mod focus;
pub mod fonts;
pub mod format;
pub mod frame;
pub mod frame_stats;
pub mod grid;
pub mod host;
pub mod icons;
#[cfg(target_os = "linux")]
pub mod platform;
pub mod reorder;
pub mod runtime;
pub mod theme;
pub mod video;
pub mod window_state;

/// The key contexts of the kit's components that use keys themselves (typing, arrows, Enter,
/// Space): an app-wide shortcut must not fire while focus is inside one of them.
pub const KEY_CONTEXTS: &[&str] = &[
    components::text_input::CONTEXT,
    components::search_field::CONTEXT,
    components::code_input::CONTEXT,
    components::select::CONTEXT,
    components::slider::CONTEXT,
    components::menu::CONTEXT,
    content::track_list::CONTEXT,
    content::tile::CONTEXT,
];

/// Everything the app needs before its first window: the tokio runtime, the fonts, the
/// component key bindings, and the theme. The artwork store is installed separately
/// ([`artwork::ArtworkStore::install`]) because it needs the HTTP client and cache directory.
///
/// `runtime`: the app's own tokio runtime, to share it; `None` starts one for the kit.
pub fn init(
    theme: theme::Theme,
    runtime: Option<tokio::runtime::Handle>,
    cx: &mut gpui::App,
) -> std::io::Result<()> {
    match runtime {
        Some(handle) => runtime::Tokio::install_handle(cx, handle),
        None => runtime::Tokio::install(cx)?,
    }
    fonts::register(cx).map_err(std::io::Error::other)?;
    cx.set_global(theme);
    fonts::warm_up(cx);
    components::bind_keys(cx);
    focus::bind_keys(cx);
    content::track_list::bind_keys(cx);
    content::tile::bind_keys(cx);
    Ok(())
}
