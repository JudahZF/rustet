//! Modal surfaces over a scrim: [`Sheet`] (a panel with its own content: sign-in, lossless
//! account) and [`Dialog`] (a question with up to two answers: confirm a removal). The panel
//! is sidebar-coloured with radius 14 and a 22 / 700 title.
//!
//! The panel rises into place (the sheet-rise motion), takes the keyboard focus when it
//! appears, keeps Tab and Shift+Tab inside itself ([`crate::focus::trap`]), and closes on
//! Escape or a click on the scrim. Draw it as the window's last child.

use std::rc::Rc;

use gpui::{
    Animation, AnimationExt, AnyElement, App, ElementId, FocusHandle, IntoElement, ParentElement,
    RenderOnce, Role, SharedString, Styled, Window, deferred, div, ease_out_quint, prelude::*, px,
};

use super::Button;
use crate::theme::{ActiveTheme, BOLD, Motion};

type OnDismiss = Rc<dyn Fn(&mut Window, &mut App)>;

const CONTEXT: &str = "Sheet";

gpui::actions!(sheet, [Dismiss]);

/// Escape closes the sheet. An action in the sheet's own key context: it comes before the
/// window's Escape (a field inside that does not use Escape passes it on to the sheet).
pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

/// A modal panel with a title and content.
#[derive(IntoElement)]
pub struct Sheet {
    id: ElementId,
    title: SharedString,
    role: Role,
    width: f32,
    body: Vec<AnyElement>,
    on_dismiss: Option<OnDismiss>,
    focus_on_open: Option<FocusHandle>,
}

impl Sheet {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            role: Role::Dialog,
            width: 420.,
            body: Vec::new(),
            on_dismiss: None,
            focus_on_open: None,
        }
    }

    /// The control that takes the focus when the sheet appears (a form's first field);
    /// without it, the panel takes it and Tab goes to its first control.
    pub fn focus_on_open(mut self, handle: FocusHandle) -> Self {
        self.focus_on_open = Some(handle);
        self
    }

    /// Escape and a click on the scrim call this.
    pub fn on_dismiss(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_dismiss = Some(Rc::new(f));
        self
    }

    /// The panel width in px at zoom 1 (default 420).
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }
}

impl ParentElement for Sheet {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.body.extend(elements);
    }
}

/// Focus moves into the panel once, when it first appears.
struct SheetFocus {
    handle: FocusHandle,
    focused: bool,
}

impl RenderOnce for Sheet {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let focus = window.use_keyed_state(self.id.clone(), cx, |_, cx| SheetFocus {
            handle: cx.focus_handle(),
            focused: false,
        });
        let open_on = self.focus_on_open;
        let handle = focus.update(cx, |f, _| {
            let first = !f.focused;
            f.focused = true;
            first.then(|| open_on.unwrap_or_else(|| f.handle.clone()))
        });
        if let Some(handle) = handle {
            window.focus(&handle, cx);
        }
        let handle = focus.read(cx).handle.clone();
        let dismiss = self.on_dismiss.clone();
        let dismiss_key = self.on_dismiss;
        let rise = theme.motion(Motion::SheetRise);
        let lift = theme.space.m16;
        let panel = crate::focus::trap(div().id("sheet-panel"), &handle)
            .role(self.role)
            .aria_label(self.title.clone())
            .track_focus(&handle)
            .key_context(CONTEXT)
            .on_action(move |_: &Dismiss, window, cx| match &dismiss_key {
                Some(f) => f(window, cx),
                None => cx.propagate(),
            })
            .on_click(|_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .gap(theme.space.m)
            .w(px(self.width * theme.zoom))
            .max_w_full()
            .p(theme.space.l28)
            .rounded(theme.radius.sheet)
            .bg(colors.sidebar)
            .border(theme.size.hairline)
            .border_color(colors.glass_stroke)
            .shadow(theme.glass_shadow())
            .text_color(colors.text)
            .child(
                div()
                    .text_size(theme.text.title1)
                    .font_weight(BOLD)
                    .child(self.title),
            )
            .children(self.body);
        let panel = if rise.is_zero() {
            panel.into_any_element()
        } else {
            panel
                .with_animation(
                    self.id.clone(),
                    Animation::new(rise).with_easing(ease_out_quint()),
                    move |el, t| el.mt(lift * (1. - t)).opacity(t),
                )
                .into_any_element()
        };
        deferred(
            div()
                .id(self.id)
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p(theme.space.xl)
                .bg(colors.scrim)
                .occlude()
                .on_click(move |_, window, cx| {
                    if let Some(f) = &dismiss {
                        f(window, cx);
                    }
                })
                .child(panel),
        )
        .with_priority(4)
    }
}

/// A question with one or two answers. The first answer is the primary one.
#[derive(IntoElement)]
pub struct Dialog {
    id: ElementId,
    title: SharedString,
    message: SharedString,
    answers: Vec<(SharedString, OnDismiss)>,
    on_dismiss: Option<OnDismiss>,
}

impl Dialog {
    pub fn new(
        id: impl Into<ElementId>,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            message: message.into(),
            answers: Vec::new(),
            on_dismiss: None,
        }
    }

    /// Adds an answer ("Remove", then "Cancel"). Answers say what they do.
    pub fn answer(
        mut self,
        label: impl Into<SharedString>,
        f: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.answers.push((label.into(), Rc::new(f)));
        self
    }

    pub fn on_dismiss(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_dismiss = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Dialog {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut sheet = Sheet {
            role: Role::AlertDialog,
            width: 380.,
            ..Sheet::new(self.id, self.title)
        };
        if let Some(f) = self.on_dismiss {
            sheet = sheet.on_dismiss(move |w, cx| f(w, cx));
        }
        sheet
            .child(
                div()
                    .text_size(theme.text.body)
                    .line_height(theme.text.body_line)
                    .text_color(theme.colors.text_secondary)
                    .child(self.message),
            )
            .child(
                div().pt(theme.space.s).flex().gap(theme.space.s).children(
                    self.answers
                        .into_iter()
                        .enumerate()
                        .map(|(ix, (label, f))| {
                            let button = Button::new(("answer", ix), label)
                                .large()
                                .on_click(move |_, w, cx| f(w, cx));
                            if ix == 0 { button.primary() } else { button }
                        }),
                ),
            )
    }
}
