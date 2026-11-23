//! The main window's frame (`docs/native-ui-look.md`, "Layout"): sidebar, content, the
//! right-side panel, and the floating player pill, with client-side decorations when the
//! compositor asks for them.
//!
//! ```text
//! ┌──────────┬───────────────────────────────────────┬──────────┐
//! │ sidebar  │ ( < )( > )   strip (52, drag)   – □ × │ panel    │
//! │ 260      │ page (from the top, under the strip)  │ 320      │
//! │          │                                       │ (queue,  │
//! │          │      ╭──── player pill (glass) ────╮  │ lyrics)  │
//! │          │      ╰─────────────────────────────╯  │          │
//! └──────────┴───────────────────────────────────────┴──────────┘
//! ```
//!
//! There is no top bar: the strip is transparent and the page scrolls under it. A page starts
//! its content `theme.size.strip` down and ends `theme.size.player_clearance` above its
//! bottom, so its first and last rows clear the strip and the pill.
//!
//! Wayland compositors without server-side decorations (GNOME) get our own shadow, rounded
//! corners, resize edges and window buttons; tiled edges drop the inset. KDE, Hyprland, sway
//! and X11 window managers draw their own frame, and we draw only the content.

use gpui::{
    AnyElement, App, Bounds, CursorStyle, Decorations, Hitbox, HitboxBehavior, Hsla, MouseButton,
    Pixels, Point, ResizeEdge, Size, Window, WindowControlArea, canvas, div, point, prelude::*, px,
    relative,
};

use crate::components::{Glass, IconButton};
use crate::icons::Icon;
use crate::theme::{ActiveTheme, FONT_FAMILY, Theme};

/// Width bands of the main window, in px at zoom 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The labelled sidebar.
    Full,
    /// The sidebar as a column of icons.
    Rail,
    /// No sidebar; the shell puts a menu button in the strip.
    Compact,
}

impl Layout {
    pub fn for_width(width: Pixels) -> Self {
        if width >= px(900.) {
            Self::Full
        } else if width >= px(620.) {
            Self::Rail
        } else {
            Self::Compact
        }
    }

    /// The sidebar's width (zoom included).
    pub fn sidebar_width(self, theme: &Theme) -> Pixels {
        match self {
            Self::Full => theme.size.sidebar,
            Self::Rail => theme.size.rail,
            Self::Compact => px(0.),
        }
    }
}

/// The client-side decoration inset (shadow and resize band).
const INSET: Pixels = px(10.);
/// The corner radius of a client-decorated window.
const RADIUS: Pixels = px(10.);

/// The transparent band around the window's content: [`INSET`] with client-side decorations.
/// gpui reports window bounds including it.
pub fn client_inset(window: &Window) -> Pixels {
    match window.window_decorations() {
        Decorations::Client { .. } => INSET,
        Decorations::Server => px(0.),
    }
}

/// The regions of the main window.
pub struct FrameParts {
    /// The sidebar (navigation, library, account). `None` in [`Layout::Compact`].
    pub sidebar: Option<AnyElement>,
    /// The page. It fills the content area from its top edge; the strip floats over it.
    pub page: AnyElement,
    /// Back and forward (28 px glass circles), at the strip's left.
    pub leading: Option<AnyElement>,
    /// Controls at the strip's right, before the window buttons.
    pub trailing: Option<AnyElement>,
    /// The right-side panel (queue, lyrics), when open.
    pub panel: Option<AnyElement>,
    /// What the player pill holds; the frame draws the glass pill around it. `None` hides it.
    pub player: Option<AnyElement>,
    /// Drawn behind the regions (the blurred artwork of Now Playing).
    pub backdrop: Option<AnyElement>,
    /// Drawn over the content area only, after the page (the `Toasts` view: it centres on the
    /// content).
    pub content_overlays: Vec<AnyElement>,
    /// Menus, sheets and dialogs, drawn last over the whole window.
    pub overlays: Vec<AnyElement>,
}

/// Lays out the frame. Call from the root view's `render`.
pub fn app_frame(parts: FrameParts, window: &mut Window, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let colors = theme.colors;
    let decorations = window.window_decorations();
    let csd = matches!(decorations, Decorations::Client { .. });
    window.set_client_inset(if csd { INSET } else { px(0.) });
    let tiling = match decorations {
        Decorations::Client { tiling } => tiling,
        Decorations::Server => gpui::Tiling::tiled(),
    };

    let content = div()
        .relative()
        .flex_1()
        .min_w_0()
        .h_full()
        // First, so it is behind the page: a click on the page's empty top band drags the
        // window. Controls a page places in that band stop the mouse-down from reaching it.
        .child(drag_strip(theme))
        .child(div().size_full().child(parts.page))
        .child(strip_controls(
            parts.leading,
            parts.trailing,
            csd,
            window,
            cx,
        ))
        .children(parts.player.map(|player| {
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(theme.size.player_bottom)
                .flex()
                .justify_center()
                .px(theme.space.l)
                .child(
                    div()
                        .id("player")
                        .occlude()
                        .w_full()
                        .max_w(theme.size.player_max_width)
                        .h(theme.size.player_height)
                        .rounded_full()
                        .glass(cx)
                        .child(player),
                )
        }))
        .children(parts.content_overlays);

    let body = crate::focus::traverse(div())
        .relative()
        .size_full()
        .flex()
        .overflow_hidden()
        .bg(theme.base())
        .text_color(colors.text)
        .font_family(FONT_FAMILY)
        .text_size(theme.text.body)
        .line_height(relative(theme.text.leading))
        .when(csd, |d| {
            d.border(theme.size.hairline)
                .border_color(colors.sidebar_border)
                .when(!(tiling.top || tiling.left), |d| d.rounded_tl(RADIUS))
                .when(!(tiling.top || tiling.right), |d| d.rounded_tr(RADIUS))
                .when(!(tiling.bottom || tiling.left), |d| d.rounded_bl(RADIUS))
                .when(!(tiling.bottom || tiling.right), |d| d.rounded_br(RADIUS))
                .when(!tiling.is_tiled(), |d| {
                    d.shadow(vec![gpui::BoxShadow {
                        color: Hsla {
                            a: 0.45,
                            ..gpui::black()
                        },
                        blur_radius: INSET / 2.,
                        spread_radius: px(0.),
                        inset: false,
                        offset: point(px(0.), px(1.)),
                    }])
                })
        })
        .children(parts.backdrop)
        .children(parts.sidebar.map(|s| {
            div()
                .h_full()
                .flex_none()
                .bg(colors.sidebar)
                .border_r(theme.size.hairline)
                .border_color(colors.sidebar_border)
                .child(s)
        }))
        .child(content)
        .children(parts.panel.map(|p| {
            div()
                .h_full()
                .flex_none()
                .w(theme.size.panel)
                .bg(colors.sidebar)
                .border_l(theme.size.hairline)
                .border_color(colors.sidebar_border)
                .child(p)
        }))
        .children(parts.overlays);

    if !csd {
        return body.into_any_element();
    }
    // The transparent band around the body: shadow, and resize handles where not tiled.
    div()
        .id("frame")
        .size_full()
        .when(!tiling.top, |d| d.pt(INSET))
        .when(!tiling.bottom, |d| d.pb(INSET))
        .when(!tiling.left, |d| d.pl(INSET))
        .when(!tiling.right, |d| d.pr(INSET))
        .child(
            canvas(
                |_, window, _| {
                    window.insert_hitbox(
                        Bounds::new(Point::default(), window.window_bounds().get_bounds().size),
                        HitboxBehavior::Normal,
                    )
                },
                |_, hitbox: Hitbox, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    if let Some(edge) = resize_edge(window.mouse_position(), size) {
                        window.set_cursor_style(resize_cursor(edge), &hitbox);
                    }
                },
            )
            .absolute()
            .size_full(),
        )
        .on_mouse_move(|_, window, _| window.refresh())
        .on_mouse_down(MouseButton::Left, |e, window, _| {
            if let Some(edge) = resize_edge(e.position, window.window_bounds().get_bounds().size) {
                window.start_window_resize(edge);
            }
        })
        .child(
            body.on_mouse_move(|_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
        )
        .into_any_element()
}

/// The strip's drag area, behind the page: drag moves the window, a double click maximises
/// it, a right click opens the window menu.
fn drag_strip(theme: &Theme) -> impl IntoElement {
    div()
        .id("drag-strip")
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(theme.size.strip)
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down(MouseButton::Left, |e, window, _| {
            if e.click_count == 2 {
                window.zoom_window();
            } else {
                window.start_window_move();
            }
        })
        .on_mouse_down(MouseButton::Right, |e, window, _| {
            window.show_window_menu(e.position)
        })
}

/// The strip's controls, over the page: `leading` at the left; `trailing`, then the window
/// buttons when Rustet draws the decorations, at the right.
fn strip_controls(
    leading: Option<AnyElement>,
    trailing: Option<AnyElement>,
    csd: bool,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let controls = window.window_controls();
    // Each group stops the mouse-down, so a click on a control does not drag the window.
    let group = || {
        div()
            .absolute()
            .top_0()
            .h(theme.size.strip)
            .flex()
            .items_center()
            .gap(theme.space.s)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    };
    let window_buttons = csd.then(|| {
        div()
            .flex()
            .gap(theme.space.xs)
            .when(controls.minimize, |d| {
                d.child(
                    IconButton::new("win-min", Icon::Minimize, "Minimize")
                        .on_click(|_, w, _| w.minimize_window()),
                )
            })
            .when(controls.maximize, |d| {
                d.child(
                    IconButton::new("win-max", Icon::Maximize, "Maximize")
                        .on_click(|_, w, _| w.zoom_window()),
                )
            })
            .child(
                IconButton::new("win-close", Icon::Close, "Close")
                    .on_click(|_, w, _| w.remove_window()),
            )
    });
    let has_trailing = trailing.is_some() || window_buttons.is_some();
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(theme.size.strip)
        .children(leading.map(|l| group().left(theme.space.m).child(l)))
        .when(has_trailing, |d| {
            d.child(
                group()
                    .right(theme.space.m)
                    .children(trailing)
                    .children(window_buttons),
            )
        })
}

fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let (left, right) = (pos.x < INSET, pos.x > size.width - INSET);
    let (top, bottom) = (pos.y < INSET, pos.y > size.height - INSET);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

fn resize_cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}
