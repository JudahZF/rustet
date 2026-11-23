//! Toasts: short notices as glass pills at the top centre of the content ("Added to Up
//! Next", "Lossless isn't available for this track. Playing AAC 256 kbps."), with an optional
//! action ("Undo").
//!
//! The shell owns one [`Toasts`] entity and draws it in `FrameParts::content_overlays`, so
//! it centres on the content area; anything that has the entity pushes to it. A toast leaves after 4 s, or 8 s when it has an action. Screen readers
//! announce each one (role `Status`; `Alert` for failures).

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, App, Context, ElementId, Role, SharedString, Task, Window, deferred,
    div, prelude::*,
};

use super::{Button, Glass};
use crate::theme::{ActiveTheme, Motion};

type Action = Rc<dyn Fn(&mut Window, &mut App)>;

/// One notice.
#[derive(Clone)]
pub struct Toast {
    pub message: SharedString,
    /// A failure: announced at once (role `Alert`).
    pub failure: bool,
    action: Option<(SharedString, Action)>,
}

impl Toast {
    pub fn new(message: impl Into<SharedString>) -> Self {
        Self {
            message: message.into(),
            failure: false,
            action: None,
        }
    }

    pub fn failure(message: impl Into<SharedString>) -> Self {
        Self {
            failure: true,
            ..Self::new(message)
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

struct Shown {
    id: u64,
    toast: Toast,
    _expire: Task<()>,
}

/// The stack of toasts. At most three show; older ones leave first.
#[derive(Default)]
pub struct Toasts {
    shown: Vec<Shown>,
    next_id: u64,
}

const MAX_SHOWN: usize = 3;

impl Toasts {
    pub fn push(&mut self, toast: Toast, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        let life = Duration::from_secs(if toast.action.is_some() { 8 } else { 4 });
        let expire = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(life).await;
            this.update(cx, |this, cx| this.dismiss(id, cx)).ok();
        });
        self.shown.push(Shown {
            id,
            toast,
            _expire: expire,
        });
        if self.shown.len() > MAX_SHOWN {
            self.shown.remove(0);
        }
        cx.notify();
    }

    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.shown.retain(|s| s.id != id);
        cx.notify();
    }

    pub fn len(&self) -> usize {
        self.shown.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }
}

impl Render for Toasts {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let arrive = theme.motion(Motion::Toast);
        let drop = theme.space.s;
        let entity = cx.entity();
        deferred(
            div()
                .absolute()
                .left_0()
                .right_0()
                // Under the strip, clear of back, forward and the window buttons.
                .top(theme.size.strip)
                .flex()
                .flex_col()
                .items_center()
                .gap(theme.space.s)
                .children(self.shown.iter().map(|s| {
                    let id = s.id;
                    let toast = div()
                        .id(ElementId::NamedInteger("toast".into(), id))
                        .role(if s.toast.failure {
                            Role::Alert
                        } else {
                            Role::Status
                        })
                        .aria_label(s.toast.message.clone())
                        .flex()
                        .items_center()
                        .gap(theme.space.m)
                        .max_w(theme.size.toast_max_width)
                        .min_h(theme.size.button)
                        .pl(theme.space.l)
                        .pr(theme.space.s6)
                        .py(theme.space.s6)
                        .rounded_full()
                        .glass(cx)
                        .text_size(theme.text.body)
                        .text_color(colors.text)
                        .child(
                            div()
                                .flex_1()
                                .py(theme.space.xs)
                                .child(s.toast.message.clone()),
                        )
                        .children(s.toast.action.clone().map(|(label, f)| {
                            let entity = entity.clone();
                            Button::new(("toast-action", id), label).on_click(move |_, w, cx| {
                                f(w, cx);
                                entity.update(cx, |t, cx| t.dismiss(id, cx));
                            })
                        }));
                    if arrive.is_zero() {
                        toast.into_any_element()
                    } else {
                        toast
                            .with_animation(
                                ElementId::NamedInteger("toast-in".into(), id),
                                Animation::new(arrive),
                                move |el, t| el.opacity(t).mt(-drop * (1. - t)),
                            )
                            .into_any_element()
                    }
                })),
        )
        .with_priority(5)
    }
}
