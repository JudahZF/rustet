//! Layout pieces both pages share, built from the kit's tokens, and the glue between the
//! app's watches and runtime and gpui views.

use std::future::Future;

use gpui::{
    AnyElement, App, AsyncApp, AsyncWindowContext, Context, Div, Entity, IntoElement,
    ParentElement, Role, SharedString, Styled, Task, WeakEntity, Window, div, prelude::*,
};
use rustet_ui::components::{Slider, TextInput};
use rustet_ui::icons::Icon;
use rustet_ui::theme::{ActiveTheme, BOLD, SEMIBOLD};
use tokio::runtime::Handle;
use tokio::sync::watch;

/// Calls `apply` with the current value of `rx` and then with every change, until the view
/// is gone or the sender is dropped.
pub fn follow<T, V>(
    mut rx: watch::Receiver<T>,
    cx: &mut Context<V>,
    apply: impl Fn(&mut V, T, &mut Context<V>) + 'static,
) -> Task<()>
where
    T: Clone + Send + Sync + 'static,
    V: 'static,
{
    cx.spawn(async move |this: WeakEntity<V>, cx: &mut AsyncApp| {
        loop {
            let value = rx.borrow_and_update().clone();
            let alive = this.update(cx, |view, cx| {
                apply(view, value, cx);
                cx.notify();
            });
            if alive.is_err() || rx.changed().await.is_err() {
                return;
            }
        }
    })
}

/// Runs `job` on the app's runtime (the models' async methods need it) and hands the result
/// to the view.
pub fn run<T, V>(
    runtime: &Handle,
    job: impl Future<Output = T> + Send + 'static,
    cx: &mut Context<V>,
    then: impl FnOnce(&mut V, T, &mut Context<V>) + 'static,
) where
    T: Send + 'static,
    V: 'static,
{
    let handle = runtime.spawn(job);
    cx.spawn(async move |this: WeakEntity<V>, cx: &mut AsyncApp| {
        let Ok(value) = handle.await else { return };
        this.update(cx, |view, cx| {
            then(view, value, cx);
            cx.notify();
        })
        .ok();
    })
    .detach();
}

/// [`run`], for a result that needs the window (a toast, a modal).
pub fn run_in<T, V>(
    runtime: &Handle,
    job: impl Future<Output = T> + Send + 'static,
    window: &Window,
    cx: &mut Context<V>,
    then: impl FnOnce(&mut V, T, &mut Window, &mut Context<V>) + 'static,
) where
    T: Send + 'static,
    V: 'static,
{
    let handle = runtime.spawn(job);
    cx.spawn_in(
        window,
        async move |this: WeakEntity<V>, cx: &mut AsyncWindowContext| {
            let Ok(value) = handle.await else { return };
            this.update_in(cx, |view, window, cx| {
                then(view, value, window, cx);
                cx.notify();
            })
            .ok();
        },
    )
    .detach();
}

/// The page title, 34 / 700 as every page's.
pub fn page_title(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .text_size(theme.text.page_title)
        .line_height(theme.text.page_title_line)
        .font_weight(BOLD)
        .text_color(theme.colors.text)
        .child(text.into())
}

/// A settings group: a 13 / 600 secondary label over a card (fill colour, radius 10) whose
/// rows are split by hairlines.
pub fn group(
    title: impl Into<SharedString>,
    rows: impl IntoIterator<Item = AnyElement>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let colors = theme.colors;
    let mut card = div()
        .flex()
        .flex_col()
        .px(theme.space.m)
        .rounded(theme.radius.menu)
        .bg(colors.fill);
    for (ix, row) in rows.into_iter().enumerate() {
        if ix > 0 {
            card = card.child(div().h(theme.size.hairline).bg(colors.divider));
        }
        card = card.child(row);
    }
    div()
        .flex()
        .flex_col()
        .gap(theme.space.s6)
        .child(
            div()
                .px(theme.space.m)
                .text_size(theme.text.body)
                .font_weight(SEMIBOLD)
                .text_color(colors.text_secondary)
                .child(title.into()),
        )
        .child(card)
        .into_any_element()
}

/// A setting: its label and an optional secondary line on the left, the control on the
/// right. When the row is too narrow for both, the control moves under the label.
pub fn row(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x(theme.space.m)
        .gap_y(theme.space.s6)
        .min_h(theme.size.settings_row)
        .py(theme.space.s6)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(theme.size.settings_label)
                .child(
                    div()
                        .text_size(theme.text.body)
                        .text_color(theme.colors.text)
                        .child(label.into()),
                )
                .children(description.map(|d| muted(d, cx))),
        )
        .child(
            div()
                .flex_none()
                .max_w_full()
                .flex()
                .items_center()
                .gap(theme.space.s)
                .child(control),
        )
        .into_any_element()
}

/// A read-only fact: label on the left, value on the right in secondary tabular figures. A
/// long value wraps, right-aligned.
pub fn fact(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(theme.space.m)
        .min_h(theme.size.settings_row)
        .py(theme.space.s6)
        .text_size(theme.text.body)
        .child(
            div()
                .flex_none()
                .text_color(theme.colors.text)
                .child(label.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_right()
                .text_color(theme.colors.text_secondary)
                .font_features(theme.tabular())
                .child(value.into()),
        )
        .into_any_element()
}

/// A paragraph that is a row of its own in a group (why lossless is not available).
pub fn note(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .flex()
        .items_center()
        .min_h(theme.size.settings_row)
        .py(theme.space.s)
        .text_size(theme.text.body)
        .line_height(theme.text.body_line)
        .text_color(theme.colors.text_secondary)
        .child(text.into())
        .into_any_element()
}

/// A text field at the width of a settings row's control.
pub fn field(input: &Entity<TextInput>, cx: &App) -> Div {
    div()
        .w(cx.theme().size.settings_field)
        .max_w_full()
        .child(input.clone())
}

/// A slider with its value beside it, at the width of a settings row's control.
pub fn slider(slider: &Entity<Slider>, value: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex()
        .items_center()
        .gap(theme.space.m)
        .w(theme.size.settings_field)
        .max_w_full()
        .child(div().flex_1().child(slider.clone()))
        .child(
            div()
                .flex_none()
                .w(theme.size.settings_value)
                .text_right()
                .text_size(theme.text.callout)
                .text_color(theme.colors.text_secondary)
                .font_features(theme.tabular())
                .child(value.into()),
        )
}

/// Secondary text.
pub fn muted(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .text_size(theme.text.callout)
        .text_color(theme.colors.text_secondary)
        .child(text.into())
}

/// Why a value was not taken: a row of its own under its control's. Read out when it appears.
pub fn refusal(
    id: impl Into<gpui::ElementId>,
    text: impl Into<SharedString>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let text = text.into();
    div()
        .id(id)
        .role(Role::Alert)
        .aria_label(text.clone())
        .flex()
        .items_center()
        .gap(theme.space.s6)
        .py(theme.space.s)
        .text_size(theme.text.callout)
        .text_color(theme.colors.text)
        .child(
            Icon::Alert
                .element()
                .flex_none()
                .size(theme.size.icon_small)
                .text_color(theme.colors.text),
        )
        .child(div().flex_1().min_w_0().child(text))
        .into_any_element()
}
