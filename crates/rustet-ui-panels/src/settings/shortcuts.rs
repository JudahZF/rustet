//! Settings → Shortcuts: the in-app keys, each reassigned by pressing the new key.
//!
//! While a binding records, a keystroke interceptor takes every key before the window's key
//! bindings, so Space or an arrow lands in the recorder and does not play or seek. Escape
//! stops recording; Tab stops it and moves on.

use gpui::{
    AnyElement, Context, IntoElement, Keystroke, ParentElement, Role, Styled, Subscription, Window,
    div, prelude::*,
};
use rustet_app::settings::{Key, ShortcutAction, Shortcuts};
use rustet_ui::components::{Button, FocusRing};
use rustet_ui::theme::{ActiveTheme, SEMIBOLD};

use super::SettingsPage;
use crate::ui::{self, group, row};

/// A binding being recorded.
pub struct Recording {
    pub action: ShortcutAction,
    _intercept: Subscription,
}

/// Why a pressed key cannot be a shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRefusal {
    /// Ctrl, Alt or Super was held.
    Modifier,
    /// A key with no name the shortcuts store (media keys, …).
    Unknown,
}

impl KeyRefusal {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Modifier => "Shortcuts are single keys. Press a key without Ctrl, Alt or Super.",
            Self::Unknown => {
                "Rustet cannot use that key. Press a letter, a number, Space or an arrow key."
            }
        }
    }
}

/// The DOM `KeyboardEvent.key` name of a gpui keystroke, as the bindings are stored.
pub fn dom_key(k: &Keystroke) -> Result<Key, KeyRefusal> {
    let m = k.modifiers;
    if m.control || m.alt || m.platform || m.function {
        return Err(KeyRefusal::Modifier);
    }
    let named = match k.key.as_str() {
        "space" => " ",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "enter" => "Enter",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
            return Ok(Key(f.to_uppercase()));
        }
        single if single.chars().count() == 1 => {
            // What the key types (Shift+1 is "!"), as a browser reports it.
            return Ok(Key(k
                .key_char
                .clone()
                .filter(|c| c.chars().count() == 1)
                .unwrap_or_else(|| single.to_owned())));
        }
        _ => return Err(KeyRefusal::Unknown),
    };
    Ok(Key(named.to_owned()))
}

/// The other action already bound to `key`, if any (letters match regardless of case).
pub fn conflict(
    shortcuts: &Shortcuts,
    action: ShortcutAction,
    key: &Key,
) -> Option<ShortcutAction> {
    let same = |a: &Key| {
        let norm = |k: &Key| {
            if k.0.chars().count() == 1 {
                k.0.to_lowercase()
            } else {
                k.0.clone()
            }
        };
        norm(a) == norm(key)
    };
    shortcuts
        .bindings
        .iter()
        .find(|(a, k)| **a != action && same(k))
        .map(|(a, _)| *a)
}

/// A key as a screen reader should say it ("Left arrow", not "←").
fn spoken(key: &Key) -> String {
    match key.0.as_str() {
        "ArrowLeft" => "Left arrow".to_owned(),
        "ArrowRight" => "Right arrow".to_owned(),
        "ArrowUp" => "Up arrow".to_owned(),
        "ArrowDown" => "Down arrow".to_owned(),
        _ => key.label(),
    }
}

fn key_name(action: ShortcutAction) -> &'static str {
    match action {
        ShortcutAction::PlayPause => "key-playPause",
        ShortcutAction::SeekForward => "key-seekForward",
        ShortcutAction::SeekBack => "key-seekBack",
        ShortcutAction::VolumeUp => "key-volUp",
        ShortcutAction::VolumeDown => "key-volDown",
        ShortcutAction::Love => "key-love",
    }
}

impl SettingsPage {
    /// Starts recording a new key for `action`.
    pub(crate) fn record(
        &mut self,
        action: ShortcutAction,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            let Some(page) = page.upgrade() else { return };
            let key = event.keystroke.key.as_str();
            if matches!(
                key,
                "shift" | "control" | "alt" | "platform" | "function" | "super" | "fn"
            ) {
                return;
            }
            if key == "tab" {
                page.update(cx, |p, cx| {
                    p.cancel_recording();
                    cx.notify();
                });
                return;
            }
            cx.stop_propagation();
            let stroke = event.keystroke.clone();
            page.update(cx, |p, cx| p.recorded(&stroke, window, cx));
        });
        self.refused.remove(key_name(action));
        self.recording = Some(Recording {
            action,
            _intercept: intercept,
        });
        cx.notify();
    }

    pub(crate) fn cancel_recording(&mut self) {
        self.recording = None;
    }

    /// A key pressed while recording.
    pub(crate) fn recorded(&mut self, k: &Keystroke, _: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.recording.as_ref().map(|r| r.action) else {
            return;
        };
        if k.key == "escape" {
            self.cancel_recording();
            cx.notify();
            return;
        }
        let name = key_name(action);
        match dom_key(k) {
            Err(why) => self.refuse(name, why.message(), cx),
            Ok(key) => {
                if let Some(other) = conflict(&self.settings.shortcuts, action, &key) {
                    self.refuse(
                        name,
                        format!(
                            "{} is already used by {}. Press another key.",
                            key.label(),
                            other.label()
                        ),
                        cx,
                    );
                } else if self.write(
                    name,
                    |s| {
                        s.shortcuts.bindings.insert(action, key);
                    },
                    cx,
                ) {
                    self.cancel_recording();
                }
            }
        }
        cx.notify();
    }

    pub(super) fn shortcuts(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let sc = self.settings.shortcuts.clone();
        let mut rows = vec![
            self.toggle(
                "shortcuts-on",
                "Keyboard shortcuts",
                sc.enabled,
                |s, on| s.shortcuts.enabled = on,
                cx,
            )
            .description("Single keys that work anywhere except in text fields.")
            .into_any_element(),
        ];
        rows.extend(self.refusal_of("shortcuts-on", cx));

        let mut keys = Vec::new();
        for action in ShortcutAction::ALL {
            let key = sc
                .bindings
                .get(&action)
                .cloned()
                .unwrap_or_else(|| action.default_key());
            let recording = self.recording.as_ref().is_some_and(|r| r.action == action);
            let label = if recording {
                "Press a key".to_owned()
            } else {
                key.label()
            };
            // The card's fill under the pill's own: the surface its focus ring is drawn on.
            let surface = theme.colors.base.blend(theme.colors.fill);
            let page = cx.entity();
            let button = div()
                .id(key_name(action))
                .role(Role::Button)
                .aria_label(format!(
                    "{}: {}",
                    action.label(),
                    if recording {
                        "press the new key".to_owned()
                    } else {
                        spoken(&key)
                    }
                ))
                .tab_index(0)
                .focus_ring_on(surface, cx)
                .cursor_pointer()
                .flex()
                .items_center()
                .justify_center()
                .min_w(theme.size.settings_value)
                .h(theme.size.control)
                .px(theme.space.m)
                .rounded_full()
                .bg(theme.colors.fill)
                .hover(|d| d.bg(theme.colors.hover.blend(theme.colors.fill)))
                .when(recording, |d| d.border_1().border_color(theme.colors.focus))
                .text_size(theme.text.body)
                .font_weight(SEMIBOLD)
                .font_features(theme.tabular())
                .text_color(if recording {
                    theme.colors.text_secondary
                } else {
                    theme.colors.text
                })
                .on_click(move |_, window, cx| {
                    page.update(cx, |p, cx| {
                        if p.recording.as_ref().is_some_and(|r| r.action == action) {
                            p.cancel_recording();
                            cx.notify();
                        } else {
                            p.record(action, window, cx);
                        }
                    })
                })
                .child(label);
            // A saved file may already bind one key twice; both rows say so.
            let clash = conflict(&sc, action, &key)
                .map(|other| format!("Also used by {}", other.label()).into());
            keys.push(row(action.label(), clash, button, cx));
            keys.extend(self.refusal_of(key_name(action), cx));
        }
        keys.push(row(
            "Default keys",
            Some("Space, the arrow keys and L.".into()),
            Button::new("reset-keys", "Reset to defaults").on_click(self.on(
                |p, _, cx| {
                    p.cancel_recording();
                    p.write(
                        "reset-keys",
                        |s| s.shortcuts.bindings = Shortcuts::default().bindings,
                        cx,
                    );
                },
                cx,
            )),
            cx,
        ));
        keys.extend(self.refusal_of("reset-keys", cx));
        if self.recording.is_some() {
            keys.push(ui::note("Escape stops recording.", cx));
        }
        vec![group("Keyboard", rows, cx), group("Keys", keys, cx)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn stroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    #[test]
    fn keys_are_stored_by_their_dom_names() {
        let none = Modifiers::default();
        assert_eq!(
            dom_key(&stroke("space", Some(" "), none)),
            Ok(Key(" ".into()))
        );
        assert_eq!(
            dom_key(&stroke("left", None, none)),
            Ok(Key("ArrowLeft".into()))
        );
        assert_eq!(dom_key(&stroke("k", Some("k"), none)), Ok(Key("k".into())));
        assert_eq!(
            dom_key(&stroke("1", Some("!"), Modifiers::shift())),
            Ok(Key("!".into()))
        );
        assert_eq!(dom_key(&stroke("f5", None, none)), Ok(Key("F5".into())));
        assert_eq!(
            dom_key(&stroke("k", None, Modifiers::control())),
            Err(KeyRefusal::Modifier)
        );
        assert_eq!(
            dom_key(&stroke("audioplay", None, none)),
            Err(KeyRefusal::Unknown)
        );
    }

    #[test]
    fn a_conflict_names_the_other_action_and_ignores_case() {
        let s = Shortcuts::default();
        assert_eq!(
            conflict(&s, ShortcutAction::PlayPause, &Key("L".into())),
            Some(ShortcutAction::Love)
        );
        assert_eq!(
            conflict(&s, ShortcutAction::Love, &Key("l".into())),
            None,
            "its own key"
        );
        assert_eq!(conflict(&s, ShortcutAction::Love, &Key("k".into())), None);
    }
}
