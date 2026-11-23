//! Buttons: [`Button`] (a pill with text and an optional icon) and [`IconButton`] (a glyph,
//! a glass circle or a key circle).
//!
//! A primary button is a key pill (white or black 85 %, text in the opposite colour); a quiet
//! one is a fill-coloured pill. Enter and Space click a focused button (gpui turns them into a
//! click).

use std::rc::Rc;

use gpui::{
    App, ClickEvent, ElementId, IntoElement, ParentElement, Pixels, RenderOnce, Role, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::*,
};

use super::{FocusRing, Tooltip, tooltip_box};
use crate::icons::Icon;
use crate::theme::{ActiveTheme, SEMIBOLD, Theme};

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    /// A key pill: the page's main actions (Play, Shuffle, Sign In).
    Primary,
    /// A fill-coloured pill: every other action.
    Quiet,
}

/// A pill button.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: ButtonVariant,
    large: bool,
    full_width: bool,
    icon: Option<Icon>,
    disabled: bool,
    on_click: Option<ClickHandler>,
}

impl Button {
    /// A quiet pill, 28 px high.
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            variant: ButtonVariant::Quiet,
            large: false,
            full_width: false,
            icon: None,
            disabled: false,
            on_click: None,
        }
    }

    /// A key pill.
    pub fn primary(mut self) -> Self {
        self.variant = ButtonVariant::Primary;
        self
    }

    /// 36 px high with 15 px text (a header's Play and Shuffle, the sidebar's Sign In).
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }

    /// As wide as its container, label centred.
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors;
        let (bg, fg, hover) = match self.variant {
            ButtonVariant::Primary => (colors.key, colors.on_key, colors.text),
            ButtonVariant::Quiet => (colors.fill, colors.text, colors.hover.blend(colors.fill)),
        };
        let (height, text, icon_size) = if self.large {
            (theme.size.button, theme.text.title3, theme.size.icon_small)
        } else {
            (theme.size.control, theme.text.body, theme.text.body)
        };
        let mut button = div()
            .id(self.id)
            .role(Role::Button)
            .aria_label(self.label.clone())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(theme.space.s6)
            .h(height)
            .px(if self.large {
                theme.space.l28
            } else {
                theme.space.m16
            })
            .when(self.full_width, |d| d.w_full())
            .rounded_full()
            .bg(bg)
            .text_color(fg)
            .text_size(text)
            .font_weight(SEMIBOLD)
            .children(
                self.icon
                    .map(|i| i.element().size(icon_size).text_color(fg)),
            )
            .child(self.label);
        if self.disabled {
            button = button.opacity(0.4).aria_description(UNAVAILABLE);
        } else {
            button = button
                .tab_index(0)
                .focus_ring(cx)
                .cursor_pointer()
                .hover(move |s| s.bg(hover));
            if let Some(f) = self.on_click {
                button = button.on_click(move |e, w, cx| f(e, w, cx));
            }
        }
        button
    }
}

/// The sizes of a round button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Circle {
    /// 28: back and forward, a tile's play and more.
    Small,
    /// 36: shelf paddles; a header's add, download and more.
    Regular,
    /// 48: beside the artist hero's play.
    Large,
    /// 72: the artist hero's play.
    Key,
}

impl Circle {
    fn diameter(self, theme: &Theme) -> Pixels {
        match self {
            Self::Small => theme.size.circle_small,
            Self::Regular => theme.size.circle,
            Self::Large => theme.size.circle_large,
            Self::Key => theme.size.key_circle,
        }
    }
}

/// How an [`IconButton`] is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    /// The glyph alone (the player's controls, rows).
    Plain,
    /// A glass circle.
    Glass(Circle),
    /// A key circle (the artist hero's play).
    Key(Circle),
}

/// An icon button. Its label is the tooltip and the screen-reader name.
#[derive(IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon: Icon,
    label: SharedString,
    look: Look,
    large: bool,
    selected: Option<bool>,
    disabled: bool,
    tooltip_above: bool,
    on_click: Option<ClickHandler>,
}

/// How long the pointer rests on a button before its tooltip shows (gpui's own delay).
const TOOLTIP_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether a placed tooltip shows, and the timer that will show it.
#[derive(Default)]
struct TipState {
    shown: bool,
    _timer: Option<gpui::Task<()>>,
}

/// What a disabled button tells assistive technology. gpui (Zed 1.29.2) cannot set
/// AccessKit's disabled flag, so a disabled button also loses its click and focus actions.
const UNAVAILABLE: &str = "Unavailable";

impl IconButton {
    /// A plain glyph button, 28 px square, 16 px glyph.
    pub fn new(id: impl Into<ElementId>, icon: Icon, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon,
            label: label.into(),
            look: Look::Plain,
            large: false,
            selected: None,
            disabled: false,
            tooltip_above: false,
            on_click: None,
        }
    }

    /// A glass circle (back and forward, shelf paddles, a header's round actions).
    pub fn glass(mut self, size: Circle) -> Self {
        self.look = Look::Glass(size);
        self
    }

    /// A key circle (the artist hero's play).
    pub fn key(mut self, size: Circle) -> Self {
        self.look = Look::Key(size);
        self
    }

    /// Shows the tooltip above the button, clear of what is around it, instead of at the
    /// pointer (buttons at the window's bottom edge: the player).
    pub fn tooltip_above(mut self) -> Self {
        self.tooltip_above = true;
        self
    }

    /// Drawn faint, out of the Tab order, not clickable, and announced as unavailable.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// A plain button with the 20 px glyph (play and pause), 36 px square.
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }

    /// A toggle button (shuffle, repeat, lyrics, queue): drawn and announced as on or off.
    /// On, the glyph is text primary with a dot below it; off, text secondary.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let tip = self.tooltip_above.then(|| {
            window.use_keyed_state(ElementId::from((self.id.clone(), "tooltip")), cx, |_, _| {
                TipState::default()
            })
        });
        let theme = cx.theme();
        let colors = theme.colors;
        let on = self.selected == Some(true);
        let (box_size, icon_size) = match self.look {
            Look::Plain if self.large => (theme.size.circle, theme.size.icon_large),
            Look::Plain => (theme.size.control, theme.size.icon_small),
            Look::Glass(c) => (c.diameter(theme), c.diameter(theme) * 0.5),
            Look::Key(c) => (c.diameter(theme), c.diameter(theme) * 0.45),
        };
        let glyph = match (self.look, self.selected) {
            (Look::Key(_), _) => colors.on_key,
            (_, Some(false)) => colors.text_secondary,
            _ => colors.text,
        };
        let mut button = div()
            .id(self.id)
            .role(Role::Button)
            .aria_label(self.label.clone())
            .when_some(self.selected, |d, on| {
                d.aria_toggled(if on {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
            })
            .focus_ring(cx)
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(box_size)
            .rounded_full()
            // A glass circle has the glass fill and stroke, but no shadow: on a pale page
            // the 40 px shadow of a small circle reads as a smudge.
            .map(|d| match self.look {
                Look::Plain => d,
                Look::Glass(_) => d
                    .bg(colors.glass)
                    .border(theme.size.hairline)
                    .border_color(colors.glass_stroke),
                Look::Key(_) => d.bg(colors.key),
            })
            .map(|d| match &tip {
                None => d.tooltip(Tooltip::text(self.label.clone())),
                Some(tip) => {
                    let (hover, down, shown) = (tip.clone(), tip.clone(), tip.read(cx).shown);
                    let label = self.label.clone();
                    d.on_hover(move |hovered, _, cx| {
                        let hovered = *hovered;
                        hover.update(cx, |t, cx| {
                            t.shown = false;
                            t._timer = hovered.then(|| {
                                cx.spawn(async move |t, cx| {
                                    cx.background_executor().timer(TOOLTIP_DELAY).await;
                                    t.update(cx, |t, cx| {
                                        t.shown = true;
                                        cx.notify();
                                    })
                                    .ok();
                                })
                            });
                            cx.notify();
                        })
                    })
                    .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                        down.update(cx, |t, cx| {
                            t.shown = false;
                            t._timer = None;
                            cx.notify();
                        })
                    })
                    .when(shown, |d| {
                        d.child(
                            div().absolute().top_0().left(gpui::relative(0.5)).child(
                                gpui::deferred(
                                    gpui::anchored()
                                        .anchor(gpui::Anchor::BottomCenter)
                                        .snap_to_window_with_margin(theme.space.xs)
                                        .child(
                                            div().mb(theme.space.s).child(tooltip_box(label, cx)),
                                        ),
                                )
                                .with_priority(4),
                            ),
                        )
                    })
                }
            })
            .child(self.icon.element().size(icon_size).text_color(glyph))
            // The "on" dot of a toggle, under the glyph.
            .when(on, |d| {
                d.child(
                    div()
                        .absolute()
                        .bottom(-theme.space.xxs)
                        .size(theme.space.xs)
                        .rounded_full()
                        .bg(colors.text),
                )
            });
        if self.disabled {
            return button.opacity(0.4).aria_description(UNAVAILABLE);
        }
        let hover = match self.look {
            Look::Plain => colors.hover,
            Look::Glass(_) => colors.hover.blend(colors.glass),
            Look::Key(_) => colors.text,
        };
        button = button
            .tab_index(0)
            .hover(move |s| s.bg(hover))
            .cursor_pointer();
        if let Some(f) = self.on_click {
            button = button.on_click(move |e, w, cx| f(e, w, cx));
        }
        button
    }
}
