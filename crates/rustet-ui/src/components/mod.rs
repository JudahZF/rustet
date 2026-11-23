//! Rustet's component set (native-UI spec §6.2), drawn from the tokens in [`crate::theme`] in
//! the look of `docs/native-ui-look.md`.
//!
//! Stateless pieces are `RenderOnce` builders ([`Button`], [`IconButton`], [`Toggle`],
//! [`Sheet`], [`Dialog`], [`EmptyStateView`], [`ErrorStateView`], [`Scrollbar`]). Pieces
//! with state of their own are entities that emit typed events ([`TextInput`],
//! [`SearchField`], [`CodeInput`], [`Select`], [`Slider`], [`ContextMenu`], [`Toasts`]).
//!
//! Every component is reachable and usable by keyboard (Tab, Enter or Space, arrows,
//! Escape) and shows the 3 px focus ring when focused from the keyboard. Every component is
//! a node for screen readers: an element gets one when it has an `id` and a `role`; it is
//! named with `aria_label` and reports its state with `aria_selected`, `aria_toggled`,
//! `aria_value` or `aria_numeric_value`. Text inside a named element is not repeated.

pub mod button;
pub mod code_input;
pub mod menu;
pub mod scrollbar;
pub mod search_field;
pub mod select;
pub mod sheet;
pub mod slider;
pub mod states;
pub mod text_input;
pub mod toast;
pub mod toggle;

use gpui::{
    AnyView, App, AppContext, Context, Hsla, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, div,
};

pub use button::{Button, ButtonVariant, Circle, IconButton};
pub use code_input::{CodeInput, CodeInputEvent};
pub use menu::{ContextMenu, MenuItem, Popover};
pub use scrollbar::Scrollbar;
pub use search_field::{SearchField, SearchFieldEvent, Suggestion};
pub use select::{Select, SelectEvent};
pub use sheet::{Dialog, Sheet};
pub use slider::{Slider, SliderEvent};
pub use states::{EmptyStateView, ErrorStateView, Placeholder, loading};
pub use text_input::{TextInput, TextInputEvent};
pub use toast::{Toast, Toasts};
pub use toggle::Toggle;

use crate::theme::ActiveTheme;

/// Binds the keys of every component. Call once at start-up.
pub fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    search_field::bind_keys(cx);
    select::bind_keys(cx);
    slider::bind_keys(cx);
    menu::bind_keys(cx);
    code_input::bind_keys(cx);
    sheet::bind_keys(cx);
}

/// The keyboard focus ring on any focusable element: 3 px of the focus colour, 2 px out.
///
/// The ring is drawn as shadows outside the element (no layout shift). The 2 px gap is
/// painted in the surface around the element, and the shadows also fill the element's own
/// area with it, so an element without a fill of its own looks unchanged while focused.
pub trait FocusRing: InteractiveElement + Sized {
    /// The ring, for an element on the page base.
    fn focus_ring(self, cx: &App) -> Self {
        let base = cx.theme().base();
        self.focus_ring_on(base, cx)
    }

    /// The ring, for an element on `surface` (opaque: the sidebar, a sheet, glass).
    fn focus_ring_on(self, surface: Hsla, cx: &App) -> Self {
        let ring = cx.theme().focus_ring(surface);
        self.focus_visible(move |s| s.shadow(ring))
    }
}

impl<E: InteractiveElement> FocusRing for E {}

/// Apple's glass: the near-opaque glass fill, a 1 px stroke and a soft shadow (player pill,
/// round page buttons, menus, toasts). gpui cannot blur what is behind a view, so the fill
/// stands in for the blur.
pub trait Glass: Styled + Sized {
    fn glass(self, cx: &App) -> Self {
        let theme = cx.theme();
        self.bg(theme.colors.glass)
            .border(theme.size.hairline)
            .border_color(theme.colors.glass_stroke)
            .shadow(theme.glass_shadow())
    }
}

impl<E: Styled> Glass for E {}

/// A text tooltip, shown by gpui after the pointer rests on an element.
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    /// For `.tooltip(Tooltip::text("Play"))`.
    pub fn text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let text = text.into();
        move |_, cx| cx.new(|_| Tooltip { text: text.clone() }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // gpui places the tooltip at the pointer; the offset keeps it clear of the cursor.
        div()
            .pl(theme.space.s)
            .pt(theme.space.m)
            .child(tooltip_box(self.text.clone(), cx))
    }
}

/// The tooltip's look, for gpui's tooltips and the ones the kit places itself
/// ([`IconButton::tooltip_above`]): a small glass label.
pub(crate) fn tooltip_box(text: SharedString, cx: &App) -> gpui::Div {
    let theme = cx.theme();
    div()
        .px(theme.space.s)
        .py(theme.space.xs)
        .rounded(theme.radius.medium)
        .glass(cx)
        .text_size(theme.text.callout)
        .text_color(theme.colors.text)
        .whitespace_nowrap()
        .child(text)
}
