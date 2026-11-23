//! A horizontal slider (seek, volume). Click or drag anywhere on the track; arrow keys step
//! when focused. Emits `Changed` while moving and `Committed` on release, so a seek bar can
//! preview while dragging and seek once.

use gpui::{
    AccessibleAction, App, Bounds, Context, DispatchPhase, EventEmitter, FocusHandle, Focusable,
    KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Role,
    SharedString, Window, actions, canvas, div, prelude::*, px, relative,
};

use crate::theme::ActiveTheme;

actions!(slider, [StepDown, StepUp]);

pub(crate) const CONTEXT: &str = "Slider";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("left", StepDown, Some(CONTEXT)),
        KeyBinding::new("down", StepDown, Some(CONTEXT)),
        KeyBinding::new("right", StepUp, Some(CONTEXT)),
        KeyBinding::new("up", StepUp, Some(CONTEXT)),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SliderEvent {
    Changed(f32),
    Committed(f32),
}

pub struct Slider {
    label: SharedString,
    /// 0–1: how much is buffered (drawn behind the value), for the player's scrubber.
    buffered: f32,
    focus_handle: FocusHandle,
    /// 0–1.
    value: f32,
    step: f32,
    dragging: bool,
    track: Bounds<Pixels>,
    /// The player's progress line: a 3 px track instead of 4 px.
    progress: bool,
}

impl EventEmitter<SliderEvent> for Slider {}

impl Slider {
    /// A slider at `value` (0–1). `label` names it for screen readers ("Volume").
    pub fn new(value: f32, label: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            label: label.into(),
            buffered: 0.,
            focus_handle: cx.focus_handle().tab_stop(true),
            value: value.clamp(0., 1.),
            step: 0.05,
            dragging: false,
            track: Bounds::default(),
            progress: false,
        }
    }

    /// Draws the track as the player's 3 px progress line (`size.progress`).
    pub fn progress(mut self) -> Self {
        self.progress = true;
        self
    }

    /// Sets the buffered range (0–1).
    pub fn set_buffered(&mut self, buffered: f32, cx: &mut Context<Self>) {
        self.buffered = buffered.clamp(0., 1.);
        cx.notify();
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Sets the value from outside (playback progress). Ignored while the user drags.
    pub fn set_value(&mut self, value: f32, cx: &mut Context<Self>) {
        if !self.dragging {
            self.value = value.clamp(0., 1.);
            cx.notify();
        }
    }

    fn value_at(&self, x: Pixels) -> f32 {
        if self.track.size.width <= px(0.) {
            return self.value;
        }
        ((x - self.track.left()) / self.track.size.width).clamp(0., 1.)
    }

    fn move_to(&mut self, value: f32, cx: &mut Context<Self>) {
        if value != self.value {
            self.value = value;
            cx.emit(SliderEvent::Changed(value));
            cx.notify();
        }
    }

    fn release(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.dragging) {
            cx.emit(SliderEvent::Committed(self.value));
            cx.notify();
        }
    }

    fn step(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.move_to((self.value + delta).clamp(0., 1.), cx);
        cx.emit(SliderEvent::Committed(self.value));
    }
}

impl Render for Slider {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let track = if self.progress {
            theme.size.progress
        } else {
            theme.size.slider_track
        };
        let knob = theme.size.slider_knob;
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();
        let fill = relative(self.value);
        div()
            .id("slider")
            .role(Role::Slider)
            .aria_label(self.label.clone())
            .aria_numeric_value(f64::from(self.value))
            .aria_min_numeric_value(0.)
            .aria_max_numeric_value(1.)
            .aria_numeric_value_step(f64::from(self.step))
            .on_a11y_action(AccessibleAction::Increment, {
                let entity = entity.clone();
                move |_, _, cx| entity.update(cx, |this, cx| this.step(this.step, cx))
            })
            .on_a11y_action(AccessibleAction::Decrement, {
                let entity = entity.clone();
                move |_, _, cx| entity.update(cx, |this, cx| this.step(-this.step, cx))
            })
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &StepDown, _, cx| this.step(-this.step, cx)))
            .on_action(cx.listener(|this, _: &StepUp, _, cx| this.step(this.step, cx)))
            .group("slider")
            .relative()
            .w_full()
            .h(theme.space.m16)
            .flex()
            .items_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    this.dragging = true;
                    let v = this.value_at(e.position.x);
                    this.move_to(v, cx);
                }),
            )
            // Release ends the drag wherever the pointer is. Element listeners, not window ones:
            // a click's release can arrive before the next frame registers anything.
            .on_mouse_up(MouseButton::Left, cx.listener(Self::release))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::release))
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(track)
                    .rounded_full()
                    .bg(colors.fill)
                    // The buffered range (the player's scrubber).
                    .when(self.buffered > self.value, |d| {
                        d.child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .h_full()
                                .w(relative(self.buffered))
                                .rounded_full()
                                .bg(colors.text_tertiary),
                        )
                    })
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .h_full()
                            .w(fill)
                            .rounded_full()
                            .bg(if focused || self.dragging {
                                colors.text
                            } else {
                                colors.text_secondary
                            }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top((track - knob) / 2.)
                            .left(fill)
                            .ml(-knob / 2.)
                            .size(knob)
                            .rounded_full()
                            .bg(colors.text)
                            .when(focused && window.last_input_was_keyboard(), |d| {
                                d.shadow(theme.focus_ring(theme.base()))
                            })
                            .when(!self.dragging && !focused, |d| {
                                d.invisible().group_hover("slider", |s| s.visible())
                            }),
                    ),
            )
            // Records the track bounds and, while dragging, follows the pointer anywhere in the
            // window (an element's own move listener stops at its edge).
            .child(
                canvas(
                    move |bounds, _, _| bounds,
                    move |bounds, _, window, cx| {
                        let dragging = entity.update(cx, |this, _| {
                            this.track = bounds;
                            this.dragging
                        });
                        if !dragging {
                            return;
                        }
                        let e = entity.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture
                                && ev.pressed_button == Some(MouseButton::Left)
                            {
                                e.update(cx, |this, cx| {
                                    let v = this.value_at(ev.position.x);
                                    this.move_to(v, cx);
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

impl Focusable for Slider {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
