//! A select (combo box): one choice of a short list ("Streaming quality"); a fill-coloured
//! pill 28 px high whose list opens as a glass menu.
//!
//! Closed, Up and Down change the choice directly; Enter or Space opens the list, where Up and
//! Down move, Enter takes the highlighted option and Escape closes it.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, Role, SharedString, Window,
    actions, anchored, deferred, div, prelude::*,
};

use super::{FocusRing, Glass};
use crate::icons::Icon;
use crate::theme::ActiveTheme;

actions!(select, [Open, Next, Previous, Close]);

pub(crate) const CONTEXT: &str = "Select";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Open, Some(CONTEXT)),
        KeyBinding::new("space", Open, Some(CONTEXT)),
        KeyBinding::new("down", Next, Some(CONTEXT)),
        KeyBinding::new("up", Previous, Some(CONTEXT)),
        KeyBinding::new("escape", Close, Some(CONTEXT)),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectEvent {
    /// The choice changed to this index.
    Changed(usize),
}

pub struct Select {
    focus_handle: FocusHandle,
    label: SharedString,
    options: Vec<SharedString>,
    selected: usize,
    open: bool,
    highlighted: usize,
}

impl EventEmitter<SelectEvent> for Select {}

impl Select {
    /// `label` names the select for screen readers ("Streaming quality").
    pub fn new(
        label: impl Into<SharedString>,
        options: Vec<SharedString>,
        selected: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected = selected.min(options.len().saturating_sub(1));
        Self {
            focus_handle: cx.focus_handle().tab_stop(true),
            label: label.into(),
            options,
            selected,
            open: false,
            highlighted: selected,
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Replaces the options (a list that changed: presets, output devices) and the choice;
    /// emits nothing. An open list closes.
    pub fn set_options(
        &mut self,
        options: Vec<SharedString>,
        selected: usize,
        cx: &mut Context<Self>,
    ) {
        self.selected = selected.min(options.len().saturating_sub(1));
        self.highlighted = self.selected;
        self.options = options;
        self.open = false;
        cx.notify();
    }

    /// Sets the choice from outside; emits nothing.
    pub fn set_selected(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.options.len() {
            self.selected = ix;
            self.highlighted = ix;
            cx.notify();
        }
    }

    fn choose(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.open = false;
        if ix != self.selected {
            self.selected = ix;
            cx.emit(SelectEvent::Changed(ix));
        }
        cx.notify();
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let n = self.options.len();
        if n == 0 {
            return;
        }
        let from = if self.open {
            self.highlighted
        } else {
            self.selected
        };
        let to = if forward {
            (from + 1).min(n - 1)
        } else {
            from.saturating_sub(1)
        };
        if self.open {
            self.highlighted = to;
            cx.notify();
        } else {
            self.choose(to, cx);
        }
    }
}

impl Render for Select {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let current = self.options.get(self.selected).cloned().unwrap_or_default();
        let list = self.open.then(|| {
            deferred(
                anchored().snap_to_window_with_margin(theme.space.s).child(
                    div()
                        .id("options")
                        .role(Role::ListBox)
                        .aria_label(self.label.clone())
                        .occlude()
                        .mt(theme.space.xs)
                        .min_w(theme.size.menu_min_width)
                        .p(theme.space.s6)
                        .rounded(theme.radius.menu)
                        .glass(cx)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.open = false;
                            cx.notify();
                        }))
                        .children(self.options.iter().enumerate().map(|(ix, option)| {
                            let highlighted = ix == self.highlighted;
                            div()
                                .id(ix)
                                .role(Role::ListBoxOption)
                                .aria_label(option.clone())
                                .aria_selected(ix == self.selected)
                                .when(highlighted, |d| d.aria_active_descendant().bg(colors.hover))
                                .hover(|d| d.bg(colors.hover))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| this.choose(ix, cx)))
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(theme.space.m)
                                .h(theme.size.menu_row)
                                .px(theme.space.s)
                                .rounded(theme.radius.medium)
                                .text_size(theme.text.body)
                                .text_color(colors.text)
                                .child(option.clone())
                                .when(ix == self.selected, |d| {
                                    d.child(
                                        Icon::Check
                                            .element()
                                            .size(theme.size.icon_small)
                                            .text_color(colors.text),
                                    )
                                })
                        })),
                ),
            )
            .with_priority(1)
        });
        let field = div()
            .id("select")
            .role(Role::ComboBox)
            .aria_label(self.label.clone())
            .aria_value(current.clone())
            .aria_expanded(self.open)
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .focus_ring(cx)
            .on_action(cx.listener(|this, _: &Open, _, cx| {
                if this.open {
                    let ix = this.highlighted;
                    this.choose(ix, cx);
                } else {
                    this.open = true;
                    this.highlighted = this.selected;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &Next, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &Previous, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &Close, _, cx| {
                if this.open {
                    this.open = false;
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.open = !this.open;
                this.highlighted = this.selected;
                cx.notify();
            }))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(theme.space.s)
            .h(theme.size.control)
            .min_w(theme.size.menu_min_width)
            .pl(theme.space.m)
            .pr(theme.space.s)
            .rounded_full()
            .bg(colors.fill)
            .hover(|s| s.bg(colors.hover.blend(colors.fill)))
            .cursor_pointer()
            .max_w_full()
            .text_size(theme.text.body)
            .text_color(colors.text)
            // In a space narrower than the choice, the choice is cut with an ellipsis.
            .child(div().min_w_0().truncate().child(current))
            .child(
                Icon::ChevronDown
                    .element()
                    .flex_none()
                    .size(theme.size.icon_small)
                    .text_color(colors.text_secondary),
            );
        // The list is anchored where it is laid out: under the field.
        div()
            .flex()
            .flex_col()
            .items_start()
            .max_w_full()
            .child(field)
            .children(list)
    }
}

impl Focusable for Select {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
