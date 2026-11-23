//! A labelled switch in Apple's shape: 51 × 31, green when on, a white knob. Enter, Space or
//! a click flips it.

use std::rc::Rc;

use gpui::{
    App, ElementId, IntoElement, ParentElement, RenderOnce, Role, SharedString, Styled, Toggled,
    Window, div, prelude::*,
};

use super::FocusRing;
use crate::theme::ActiveTheme;

type ChangeHandler = Rc<dyn Fn(bool, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct Toggle {
    id: ElementId,
    label: SharedString,
    description: Option<SharedString>,
    on: bool,
    disabled: bool,
    on_change: Option<ChangeHandler>,
}

impl Toggle {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            on,
            disabled: false,
            on_change: None,
        }
    }

    /// A second line under the label, also read by screen readers.
    pub fn description(mut self, text: impl Into<SharedString>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Called with the new state.
    pub fn on_change(mut self, f: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Toggle {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors;
        let (w, h) = (theme.size.switch_width, theme.size.switch_height);
        let inset = theme.space.xxs;
        let on = self.on;
        let mut row = div()
            .id(self.id)
            .role(Role::Switch)
            .aria_label(self.label.clone())
            .when_some(self.description.clone(), |d, t| d.aria_description(t))
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .flex()
            .items_center()
            .justify_between()
            .gap(theme.space.m)
            .min_h(theme.size.settings_row)
            // Room above and below a label that wraps to more lines than the row holds.
            .py(theme.space.s6)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(theme.text.body)
                            .text_color(colors.text)
                            .child(self.label),
                    )
                    .children(self.description.map(|t| {
                        div()
                            .text_size(theme.text.callout)
                            .text_color(colors.text_secondary)
                            .child(t)
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .w(w)
                    .h(h)
                    .rounded_full()
                    .p(inset)
                    .flex()
                    .when(on, |d| d.justify_end())
                    .bg(if on { colors.switch_on } else { colors.fill })
                    .child(
                        div()
                            .size(h - inset * 2.)
                            .rounded_full()
                            .bg(colors.switch_knob)
                            .shadow_sm(),
                    ),
            );
        if self.disabled {
            row = row.opacity(0.4);
        } else {
            row = row.tab_index(0).focus_ring(cx).cursor_pointer();
            if let Some(f) = self.on_change {
                row = row.on_click(move |_, w, cx| f(!on, w, cx));
            }
        }
        row
    }
}
