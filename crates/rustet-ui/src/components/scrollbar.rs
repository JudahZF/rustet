//! A vertical scroll bar for a scrolling element (gpui scrolls but draws no bar).
//!
//! Place it as the last child of a `relative()` container whose other child scrolls with
//! `handle`: `div().relative().child(list.track_scroll(&h)).child(Scrollbar::new(h.0.borrow().base_handle.clone()))`.
//! The thumb is drawn only when there is something to scroll; drag it to scroll, or click the
//! track to jump. Keyboard and wheel scrolling belong to the list itself.

use gpui::{
    App, Bounds, DispatchPhase, ElementId, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, RenderOnce, ScrollHandle, Styled, Window, canvas, fill,
    point, px, size,
};

use crate::theme::ActiveTheme;

#[derive(IntoElement)]
pub struct Scrollbar {
    id: ElementId,
    handle: ScrollHandle,
}

impl Scrollbar {
    pub fn new(id: impl Into<ElementId>, handle: ScrollHandle) -> Self {
        Self {
            id: id.into(),
            handle,
        }
    }
}

/// The pointer's offset into the thumb while dragging.
#[derive(Default)]
struct Drag(Option<Pixels>);

/// Where the thumb is: (top, height), relative to the track.
fn thumb(track: Pixels, content: Pixels, offset: Pixels) -> Option<(Pixels, Pixels)> {
    if content <= track || track <= px(0.) {
        return None;
    }
    let height = (track * (track / content)).max(px(24.)).min(track);
    let max_offset = content - track;
    let top = (track - height) * ((-offset) / max_offset).clamp(0., 1.);
    Some((top, height))
}

impl RenderOnce for Scrollbar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let color = theme.colors.text_tertiary;
        let active = theme.colors.text_secondary;
        let width = px(10. * theme.zoom);
        let drag = window.use_keyed_state(self.id.clone(), cx, |_, _| Drag::default());
        let handle = self.handle;
        canvas(
            |bounds, _, _| bounds,
            move |bounds: Bounds<Pixels>, _, window, cx| {
                let viewport = handle.bounds().size.height;
                let content = viewport + handle.max_offset().y;
                let offset = handle.offset().y;
                let Some((top, height)) = thumb(viewport, content, offset) else {
                    return;
                };
                let dragging = drag.read(cx).0.is_some();
                let thumb_bounds = Bounds::new(
                    point(bounds.right() - px(7.), bounds.top() + top),
                    size(px(4.), height),
                );
                window.paint_quad(
                    fill(thumb_bounds, if dragging { active } else { color }).corner_radii(px(2.)),
                );

                let scroll_to = {
                    let handle = handle.clone();
                    move |y: Pixels| {
                        let max = handle.max_offset().y;
                        let track = (viewport - height).max(px(1.));
                        let ratio = ((y - bounds.top()) / track).clamp(0., 1.);
                        handle.set_offset(point(handle.offset().x, -(max * ratio)));
                    }
                };
                {
                    let (drag, scroll_to) = (drag.clone(), scroll_to.clone());
                    window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble
                            || e.button != MouseButton::Left
                            || !bounds.contains(&e.position)
                        {
                            return;
                        }
                        let grab = if (thumb_bounds.top()..=thumb_bounds.bottom())
                            .contains(&e.position.y)
                        {
                            e.position.y - thumb_bounds.top()
                        } else {
                            // A click on the track centres the thumb there.
                            scroll_to(e.position.y - height / 2.);
                            height / 2.
                        };
                        drag.update(cx, |d, _| d.0 = Some(grab));
                        cx.stop_propagation();
                        window.refresh();
                    });
                }
                if let Some(grab) = drag.read(cx).0 {
                    let drag_move = drag.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, _| {
                        if phase == DispatchPhase::Capture
                            && e.pressed_button == Some(MouseButton::Left)
                        {
                            scroll_to(e.position.y - grab);
                            window.refresh();
                        }
                    });
                    window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
                        if phase == DispatchPhase::Capture && e.button == MouseButton::Left {
                            drag_move.update(cx, |d, _| d.0 = None);
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .top_0()
        .right_0()
        .h_full()
        .w(width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_tracks_the_offset() {
        assert_eq!(thumb(px(100.), px(80.), px(0.)), None);
        let (top, height) = thumb(px(100.), px(400.), px(0.)).unwrap();
        assert_eq!((top, height), (px(0.), px(25.)));
        let (top, _) = thumb(px(100.), px(400.), px(-300.)).unwrap();
        assert_eq!(top, px(75.));
        // Very long content: the thumb keeps a usable size.
        assert_eq!(thumb(px(100.), px(100_000.), px(0.)).unwrap().1, px(24.));
    }
}
