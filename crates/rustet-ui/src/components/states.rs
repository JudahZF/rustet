//! What a page shows when it has nothing ([`EmptyStateView`]), when loading failed
//! ([`ErrorStateView`], with the recovery action), and while it loads ([`Placeholder`]).
//!
//! The texts come from `rustet_app` ([`EmptyState`], [`ErrorState`]): they say what failed and
//! what to do. The placeholder is still (no shimmer: nothing moves unless the user acts).

use std::rc::Rc;

use gpui::{
    App, IntoElement, ParentElement, RenderOnce, Role, SharedString, Styled, Window, div,
    prelude::*,
};
use rustet_app::pages::model::EmptyState;
use rustet_app::pages::{ErrorState, Recovery};

use super::Button;
use crate::icons::Icon;
use crate::theme::{ActiveTheme, BOLD};

/// A button's handler.
type Action = Rc<dyn Fn(&mut Window, &mut App)>;
/// What a recovery button runs.
type OnRecover = Rc<dyn Fn(Recovery, &mut Window, &mut App)>;

/// A page with nothing in it, optionally with an action ("Search Apple Music").
#[derive(IntoElement)]
pub struct EmptyStateView {
    title: SharedString,
    message: SharedString,
    action: Option<(SharedString, Action)>,
}

impl EmptyStateView {
    pub fn new(state: &EmptyState) -> Self {
        Self {
            title: state.title.clone().into(),
            message: state.message.clone().into(),
            action: None,
        }
    }

    pub fn action(
        mut self,
        label: impl Into<SharedString>,
        f: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.action = Some((label.into(), Rc::new(f)));
        self
    }
}

impl RenderOnce for EmptyStateView {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        message_block(
            "empty-state",
            None,
            self.title,
            self.message,
            self.action,
            cx,
        )
    }
}

/// A page that failed to load: the error's text and its recovery as a button.
#[derive(IntoElement)]
pub struct ErrorStateView {
    state: ErrorState,
    on_recover: Option<OnRecover>,
}

impl ErrorStateView {
    pub fn new(state: &ErrorState) -> Self {
        Self {
            state: state.clone(),
            on_recover: None,
        }
    }

    /// Called with the error's [`Recovery`] when its button is used.
    pub fn on_recover(mut self, f: impl Fn(Recovery, &mut Window, &mut App) + 'static) -> Self {
        self.on_recover = Some(Rc::new(f));
        self
    }
}

/// The button text of a recovery.
pub fn recovery_label(recovery: Recovery) -> &'static str {
    match recovery {
        Recovery::Retry => "Try again",
        Recovery::SignIn => "Sign in",
        Recovery::GoBack => "Go back",
    }
}

impl RenderOnce for ErrorStateView {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let recovery = self.state.action;
        let action = self.on_recover.map(|f| {
            let label: SharedString = recovery_label(recovery).into();
            let run: Action = Rc::new(move |w, cx| f(recovery, w, cx));
            (label, run)
        });
        message_block(
            "error-state",
            Some(Icon::Alert),
            self.state.title.into(),
            self.state.message.into(),
            action,
            cx,
        )
    }
}

fn message_block(
    id: &'static str,
    icon: Option<Icon>,
    title: SharedString,
    message: SharedString,
    action: Option<(SharedString, Action)>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let colors = theme.colors;
    div()
        .id(id)
        .role(if icon.is_some() {
            Role::Alert
        } else {
            Role::Status
        })
        .aria_label(title.clone())
        .aria_description(message.clone())
        .flex()
        .flex_col()
        .items_start()
        .gap(theme.space.s)
        .max_w(theme.size.state_max_width)
        .py(theme.space.xxl)
        .children(icon.map(|i| {
            i.element()
                .size(theme.text.large_title)
                .text_color(colors.text_secondary)
        }))
        .child(
            div()
                .text_size(theme.text.title2)
                .font_weight(BOLD)
                .text_color(colors.text)
                .child(title),
        )
        .child(
            div()
                .text_size(theme.text.body)
                .line_height(theme.text.body_line)
                .text_color(colors.text_secondary)
                .child(message),
        )
        .children(action.map(|(label, run)| {
            div().pt(theme.space.s).child(
                Button::new("state-action", label)
                    .primary()
                    .large()
                    .on_click(move |_, w, cx| run(w, cx)),
            )
        }))
}

/// Still blocks in the shape of the content that is loading.
#[derive(IntoElement)]
pub enum Placeholder {
    /// `n` track rows.
    Rows(usize),
    /// `n` tiles of `size` px.
    Tiles { count: usize, size: gpui::Pixels },
}

impl RenderOnce for Placeholder {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let block = theme.colors.placeholder;
        let bar = move |w: f32| {
            div()
                .h(theme.text.callout)
                .w(gpui::relative(w))
                .rounded(theme.radius.small)
                .bg(block)
        };
        match self {
            Placeholder::Rows(n) => div().flex().flex_col().children((0..n).map(|i| {
                div()
                    .h(theme.size.row)
                    .flex()
                    .items_center()
                    .gap(theme.space.m)
                    .child(
                        div()
                            .size(theme.size.art_row)
                            .rounded(theme.radius.small)
                            .bg(block),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.space.xs)
                            .w(gpui::relative(0.4))
                            .child(bar(0.9 - (i % 3) as f32 * 0.15))
                            .child(bar(0.5)),
                    )
            })),
            Placeholder::Tiles { count, size } => {
                div()
                    .flex()
                    .gap(theme.space.l)
                    .children((0..count).map(|_| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.space.s)
                            .w(size)
                            .child(div().size(size).rounded(theme.radius.tile).bg(block))
                            .child(bar(0.8))
                            .child(bar(0.5))
                    }))
            }
        }
    }
}

/// The region a page shows while it loads: one "Loading" node for assistive technology, with
/// the [`Placeholder`]s inside it (a placeholder is only drawing, so several can share one
/// region).
pub fn loading(id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(Role::ProgressIndicator)
        .aria_label("Loading")
}
