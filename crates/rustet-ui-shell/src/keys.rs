//! The in-app shortcuts of Settings → Keyboard Shortcuts: gpui key events to the DOM key names
//! the bindings store (`" "`, `"ArrowLeft"`, `"l"`), and when a shortcut may fire.

use gpui::Keystroke;
use rustet_app::settings::Key;

/// The DOM `KeyboardEvent.key` name of `stroke`, or `None` while Ctrl, Alt or Super is held
/// (the web shell ignored those, so Ctrl+→ stays "next track").
pub fn dom_key(stroke: &Keystroke) -> Option<Key> {
    let m = &stroke.modifiers;
    if m.control || m.alt || m.platform {
        return None;
    }
    let name = match stroke.key.as_str() {
        "space" => " ",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "escape" => "Escape",
        "enter" => "Enter",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
            return Some(Key(f.to_uppercase()));
        }
        // A printable key: what it types (Shift+1 is "!"), as the browser reports it.
        k => {
            let typed = stroke.key_char.as_deref().unwrap_or(k);
            if typed.chars().count() != 1 {
                return None;
            }
            return Some(Key(typed.to_owned()));
        }
    };
    Some(Key(name.to_owned()))
}

/// Whether a shortcut may take `key`. `in_context` says whether a key context is on the focus path;
/// `focus_on_page` says the focus is on the window's own base (nothing in particular). Space
/// and Enter press a focused button, so they are shortcuts only with nothing focused.
pub fn may_fire(key: &Key, in_context: impl Fn(&str) -> bool, focus_on_page: bool) -> bool {
    if matches!(key.0.as_str(), " " | "Enter") && !focus_on_page {
        return false;
    }
    !rustet_ui::KEY_CONTEXTS.iter().any(|c| in_context(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &'static str) -> Option<String> {
        dom_key(&Keystroke::parse(s).unwrap()).map(|k| k.0)
    }

    #[test]
    fn gpui_keys_become_the_names_the_bindings_store() {
        assert_eq!(key("space").as_deref(), Some(" "));
        assert_eq!(key("left").as_deref(), Some("ArrowLeft"));
        assert_eq!(key("right").as_deref(), Some("ArrowRight"));
        assert_eq!(key("up").as_deref(), Some("ArrowUp"));
        assert_eq!(key("down").as_deref(), Some("ArrowDown"));
        assert_eq!(key("escape").as_deref(), Some("Escape"));
        assert_eq!(key("f5").as_deref(), Some("F5"));
        assert_eq!(key("l").as_deref(), Some("l"));
        assert_eq!(key("ctrl-l"), None, "a held modifier is not a shortcut");
        assert_eq!(key("alt-left"), None);
        let mut shifted = Keystroke::parse("shift-l").unwrap();
        shifted.key_char = Some("L".into());
        assert_eq!(dom_key(&shifted).map(|k| k.0).as_deref(), Some("L"));
    }

    #[test]
    fn the_default_bindings_answer_converted_keys() {
        let shortcuts = rustet_app::settings::Shortcuts::default();
        let action =
            |s: &'static str| shortcuts.action_for(&dom_key(&Keystroke::parse(s).unwrap())?);
        use rustet_app::settings::ShortcutAction as A;
        assert_eq!(action("space"), Some(A::PlayPause));
        assert_eq!(action("right"), Some(A::SeekForward));
        assert_eq!(action("down"), Some(A::VolumeDown));
        assert_eq!(action("l"), Some(A::Love));
    }

    fn ctx<'a>(names: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |c| names.contains(&c)
    }

    #[test]
    fn space_stays_with_text_fields_and_focused_controls() {
        let space = Key(" ".into());
        assert!(may_fire(&space, ctx(&["Shell"]), true));
        assert!(
            !may_fire(&space, ctx(&["Shell"]), false),
            "a focused button takes Space"
        );
        assert!(!may_fire(&space, ctx(&["Shell", "TextInput"]), true));
        let right = Key("ArrowRight".into());
        assert!(may_fire(&right, ctx(&["Shell"]), false));
        assert!(
            !may_fire(&right, ctx(&["Shell", "Slider"]), false),
            "the slider steps"
        );
        assert!(!may_fire(&right, ctx(&["Shell", "TrackList"]), false));
        assert!(!may_fire(
            &Key("l".into()),
            ctx(&["Shell", "SearchField", "TextInput"]),
            false
        ));
    }
}
