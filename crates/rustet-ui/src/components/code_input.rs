//! A six-digit verification code field (Apple's two-factor code). Digits only; a pasted code
//! fills every box; the event fires once the sixth digit is in.
//!
//! It takes text through the platform input handler, so on-screen keyboards and input methods
//! that send full-width digits (０–９, common with CJK input) work too.

use std::ops::Range;

use gpui::{
    App, Bounds, Context, ElementInputHandler, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, KeyBinding, Pixels, Point, Role, UTF16Selection, Window, actions, canvas, div,
    prelude::*,
};

use crate::theme::ActiveTheme;

actions!(code_input, [Backspace, Paste]);

pub(crate) const CONTEXT: &str = "CodeInput";
pub const CODE_LEN: usize = 6;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
    ]);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeInputEvent {
    /// All six digits are in.
    Complete(String),
}

pub struct CodeInput {
    focus_handle: FocusHandle,
    digits: String,
}

impl EventEmitter<CodeInputEvent> for CodeInput {}

/// The ASCII digits of `text`, with full-width digits folded to ASCII.
pub fn digits_of(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().filter_map(|c| match c {
        '0'..='9' => Some(c),
        '０'..='９' => char::from_u32(c as u32 - '０' as u32 + '0' as u32),
        _ => None,
    })
}

impl CodeInput {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle().tab_stop(true),
            digits: String::new(),
        }
    }

    pub fn code(&self) -> &str {
        &self.digits
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.digits.clear();
        cx.notify();
    }

    /// Appends the digits of `text`, up to six in total.
    pub fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let was_complete = self.digits.len() == CODE_LEN;
        let room = CODE_LEN - self.digits.len();
        self.digits.extend(digits_of(text).take(room));
        if !was_complete && self.digits.len() == CODE_LEN {
            cx.emit(CodeInputEvent::Complete(self.digits.clone()));
        }
        cx.notify();
    }
}

impl EntityInputHandler for CodeInput {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.digits.len()..self.digits.len(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        // No composition for digits: the input method commits them.
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for CodeInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors;
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();
        let focus = self.focus_handle.clone();
        let current = self.digits.len();
        div()
            .id("code-input")
            .role(Role::TextInput)
            .aria_label("Verification code")
            .aria_value(self.digits.clone())
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                this.digits.pop();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    this.insert(&text, cx);
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| window.focus(&this.focus_handle, cx)))
            .relative()
            .flex()
            .gap(theme.space.s)
            .children((0..CODE_LEN).map(|i| {
                let digit = self.digits.chars().nth(i);
                let active =
                    focused && (i == current || (current == CODE_LEN && i == CODE_LEN - 1));
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(theme.size.code_width)
                    .h(theme.size.code_height)
                    .rounded(theme.radius.card)
                    .bg(colors.fill)
                    .when(active, |d| d.shadow(theme.focus_ring(colors.sidebar)))
                    .text_size(theme.text.large_title)
                    .font_weight(crate::theme::SEMIBOLD)
                    .font_features(theme.tabular())
                    .text_color(colors.text)
                    .when(i == 3, |d| d.ml(theme.space.s))
                    .children(digit.map(|d| d.to_string()))
            }))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

impl Focusable for CodeInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_full_width_digits_and_drops_the_rest() {
        assert_eq!(digits_of("12-34 ５６x").collect::<String>(), "123456");
        assert_eq!(digits_of("abc").count(), 0);
    }
}
