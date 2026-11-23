//! Keyboard focus traversal. Tab and Shift+Tab move through a window's tab stops in visual
//! order ([`traverse`] on the window's root); an open modal keeps them inside itself
//! ([`trap`]); a list of items is one tab stop ([`rove`]), and the arrow keys move inside it.
//!
//! A control is a tab stop when its element has `tab_index(0)`, or when its own focus handle
//! was made with `tab_stop(true)` (a handle given to `track_focus` keeps its own setting).

use gpui::{App, FocusHandle, InteractiveElement, KeyBinding, Window, actions};

actions!(focus, [Next, Previous]);

/// Binds Tab and Shift+Tab ([`crate::init`] does it; view tests call it themselves).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", Next, None),
        KeyBinding::new("shift-tab", Previous, None),
    ]);
}

/// Lets Tab and Shift+Tab move the focus through the window. Put it on the window's root.
pub fn traverse<E: InteractiveElement>(root: E) -> E {
    root.on_action(|_: &Next, window, cx| window.focus_next(cx))
        .on_action(|_: &Previous, window, cx| window.focus_prev(cx))
}

/// Keeps Tab and Shift+Tab inside `panel` (an open modal), wrapping at its ends. `scope` is
/// the focus handle the panel tracks.
pub fn trap<E: InteractiveElement>(panel: E, scope: &FocusHandle) -> E {
    let (next, previous) = (scope.clone(), scope.clone());
    panel
        .on_action(move |_: &Next, window, cx| cycle_within(&next, true, window, cx))
        .on_action(move |_: &Previous, window, cx| cycle_within(&previous, false, window, cx))
}

/// Steps through the window's tab order until a stop inside `scope`. With no stop inside, the
/// focus goes back to `scope` itself.
fn cycle_within(scope: &FocusHandle, forward: bool, window: &mut Window, cx: &mut App) {
    let start = window.focused(cx);
    // The order has one entry per tab stop: a full turn visits each once.
    for _ in 0..1024 {
        if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
        let now = window.focused(cx);
        if scope.contains_focused(window, cx) && !scope.is_focused(window) {
            return;
        }
        if now.is_none() || now == start {
            break;
        }
    }
    window.focus(scope, cx);
}

/// Makes one of `shown` a tab stop and the others not (a roving tab stop): the focused item,
/// else the first. Call it with the items drawn this frame.
pub fn rove<'a>(shown: impl IntoIterator<Item = &'a FocusHandle>, window: &Window) {
    let shown: Vec<&FocusHandle> = shown.into_iter().collect();
    let stop = shown.iter().position(|h| h.is_focused(window)).unwrap_or(0);
    for (ix, handle) in shown.into_iter().enumerate() {
        handle.clone().tab_stop(ix == stop);
    }
}
