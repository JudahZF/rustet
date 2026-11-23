//! Which URL schemes Rustet claims as the system handler (`link-schemes.mjs`). `rustet://` is
//! always registered; the Apple Music schemes are opt-in (Settings → Links), because claiming one
//! takes the link away from any other app that handles it.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::deeplink::{APPLE_SCHEMES, OWN_SCHEME, SchemeDef, SchemeGroup};
use crate::platform::{Env, Platform};
use crate::prefs::PrefsFile;
use crate::system::CommandRunner;

/// Keeps only known optional schemes, once each, in order.
pub fn normalize_selection(list: Option<&[Value]>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    list.unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| APPLE_SCHEMES.iter().any(|d| d.scheme == *s) && seen.insert(s.to_string()))
        .map(str::to_owned)
        .collect()
}

/// The desktop file Linux associates with Rustet's schemes.
pub fn linux_desktop_id(env: &Env) -> String {
    env.get("FLATPAK_ID")
        .map_or_else(|| "rustet.desktop".to_owned(), |id| format!("{id}.desktop"))
}

const DEFAULTS_SECTION: &str = "[Default Applications]";

/// `key=value` of a mimeapps.list line (not a comment or section header).
fn split_entry(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    let first = key.chars().next()?;
    (!"#[".contains(first)).then_some((key, value))
}

fn handlers(value: &str) -> impl Iterator<Item = &str> {
    value.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// Drops `x-scheme-handler/<scheme>=…` associations that point at `desktop_id` from the text of a
/// mimeapps.list, leaving every other application's entry untouched.
pub fn remove_scheme_associations(text: &str, desktop_id: &str, schemes: &[&str]) -> String {
    let wanted: Vec<String> = schemes
        .iter()
        .map(|s| format!("x-scheme-handler/{s}"))
        .collect();
    let mut out = Vec::new();
    for line in text.split('\n') {
        match split_entry(line) {
            Some((key, value)) if wanted.iter().any(|w| w == key.trim()) => {
                let kept: Vec<&str> = handlers(value).filter(|h| *h != desktop_id).collect();
                if !kept.is_empty() {
                    out.push(format!("{key}={};", kept.join(";")));
                }
            }
            _ => out.push(line.to_owned()),
        }
    }
    out.join("\n")
}

fn is_section(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('[') && t.ends_with(']')
}

/// Makes `desktop_id` the first handler for each scheme in the `[Default Applications]` section,
/// keeping other handlers after it. Creates the section when it is missing.
pub fn add_scheme_associations(text: &str, desktop_id: &str, schemes: &[&str]) -> String {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let start = match lines.iter().position(|l| l.trim() == DEFAULTS_SECTION) {
        Some(i) => i,
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(DEFAULTS_SECTION.to_owned());
            lines.len() - 1
        }
    };
    let mut end = lines
        .iter()
        .skip(start + 1)
        .position(|l| is_section(l))
        .map_or(lines.len(), |i| i + start + 1);
    for scheme in schemes {
        let key = format!("x-scheme-handler/{scheme}");
        let existing = (start + 1..end)
            .find(|&i| lines[i].split('=').next().unwrap_or_default().trim() == key);
        match existing {
            Some(i) => {
                let value = lines[i].split_once('=').map_or("", |(_, v)| v);
                let others = handlers(value).filter(|h| *h != desktop_id);
                lines[i] = format!(
                    "{key}={};",
                    std::iter::once(desktop_id)
                        .chain(others)
                        .collect::<Vec<_>>()
                        .join(";")
                );
            }
            None => {
                lines.insert(end, format!("{key}={desktop_id};"));
                end += 1;
            }
        }
    }
    lines.join("\n") + "\n"
}

/// The handler a mimeapps.list names first for a scheme in `[Default Applications]`.
pub fn default_handler(text: &str, scheme: &str) -> Option<String> {
    let key = format!("x-scheme-handler/{scheme}");
    let mut in_defaults = false;
    for line in text.split('\n') {
        let t = line.trim();
        if is_section(t) {
            in_defaults = t == DEFAULTS_SECTION;
            continue;
        }
        if in_defaults && t.split('=').next().unwrap_or_default().trim() == key {
            return t
                .split_once('=')
                .and_then(|(_, v)| v.split(';').next())
                .map(|h| h.trim().to_owned());
        }
    }
    None
}

/// Claims and releases schemes with the system.
pub trait SchemeRegistrar {
    fn register(&self, scheme: &str) -> bool;
    fn unregister(&self, scheme: &str) -> bool;
    fn is_active(&self, scheme: &str) -> bool;
}

/// Linux: writes the associations into mimeapps.list directly. Electron's own registration
/// depends on `xdg-settings` and fails quietly without it, and cannot unregister at all.
#[derive(Debug, Clone)]
pub struct MimeappsRegistrar {
    /// `$XDG_CONFIG_HOME/mimeapps.list` (or `~/.config/mimeapps.list`): written.
    primary: PathBuf,
    /// Also cleaned when unregistering: `~/.local/share/applications/mimeapps.list`.
    legacy: PathBuf,
    desktop_id: String,
}

impl MimeappsRegistrar {
    pub fn new(env: &Env, home: &Path) -> Self {
        let config = env
            .get("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from);
        Self {
            primary: config.join("mimeapps.list"),
            legacy: home.join(".local/share/applications/mimeapps.list"),
            desktop_id: linux_desktop_id(env),
        }
    }

    fn replace(file: &Path, text: &str) -> io::Result<()> {
        let mut tmp = file.as_os_str().to_owned();
        tmp.push(".rustet-tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, file)
    }
}

impl SchemeRegistrar for MimeappsRegistrar {
    fn register(&self, scheme: &str) -> bool {
        let before = fs::read_to_string(&self.primary).unwrap_or_default();
        let after = add_scheme_associations(&before, &self.desktop_id, &[scheme]);
        if after == before {
            return true;
        }
        self.primary
            .parent()
            .is_none_or(|d| fs::create_dir_all(d).is_ok())
            && Self::replace(&self.primary, &after).is_ok()
    }

    fn unregister(&self, scheme: &str) -> bool {
        for file in [&self.primary, &self.legacy] {
            // An unreadable or unwritable file is left alone.
            if let Ok(before) = fs::read_to_string(file) {
                let after = remove_scheme_associations(&before, &self.desktop_id, &[scheme]);
                if after != before {
                    let _ = Self::replace(file, &after);
                }
            }
        }
        true
    }

    fn is_active(&self, scheme: &str) -> bool {
        fs::read_to_string(&self.primary)
            .is_ok_and(|t| default_handler(&t, scheme).as_deref() == Some(self.desktop_id.as_str()))
    }
}

/// Registers `rustet://` (`app.setAsDefaultProtocolClient('rustet')`): on Linux Electron runs
/// `xdg-settings set default-url-scheme-handler rustet <desktop file>`. Returns whether it worked.
pub async fn register_own_scheme(runner: &impl CommandRunner, env: &Env) -> bool {
    let id = linux_desktop_id(env);
    runner
        .run(
            "xdg-settings",
            &["set", "default-url-scheme-handler", OWN_SCHEME, &id],
        )
        .await
        .is_some()
}

/// One row of Settings → Links (`links:get`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemeStatus {
    pub scheme: &'static str,
    pub note: &'static str,
    pub group: SchemeGroup,
    pub group_title: &'static str,
    pub kind: &'static str,
    pub enabled: bool,
    pub active: bool,
}

/// `links:set` with a scheme this platform does not offer.
#[derive(Debug, thiserror::Error)]
pub enum LinkSchemeError {
    #[error("unknown scheme")]
    UnknownScheme,
    #[error("saving prefs: {0}")]
    Prefs(#[from] io::Error),
}

/// Keeps the system's scheme claims in line with the saved choices.
pub struct LinkSchemes<'a, R: SchemeRegistrar> {
    pub prefs: &'a PrefsFile,
    pub registrar: &'a R,
    pub platform: Platform,
}

impl<R: SchemeRegistrar> LinkSchemes<'_, R> {
    /// Schemes offered on this platform (the Windows aliases mean nothing elsewhere).
    fn offered(&self) -> impl Iterator<Item = &'static SchemeDef> {
        let windows = self.platform == Platform::Windows;
        APPLE_SCHEMES
            .iter()
            .filter(move |s| !s.windows_only || windows)
    }

    /// The saved choice.
    pub fn selection(&self) -> Vec<String> {
        normalize_selection(self.prefs.load().link_schemes.as_deref())
    }

    /// Makes the system match the saved choices. Only schemes Rustet registered itself are ever
    /// unregistered, so another app's claim is never removed.
    pub fn apply(&self) -> io::Result<()> {
        let p = self.prefs.load();
        let want = normalize_selection(p.link_schemes.as_deref());
        let mut applied = normalize_selection(p.link_schemes_applied.as_deref());
        for s in &want {
            if self.registrar.register(s) && !applied.contains(s) {
                applied.push(s.clone());
            }
        }
        applied.retain(|s| want.contains(s) || !self.registrar.unregister(s));
        self.prefs.update(|p| {
            p.link_schemes_applied = Some(applied.into_iter().map(Value::from).collect())
        })
    }

    /// `links:get`.
    pub fn list(&self) -> Vec<SchemeStatus> {
        let want = self.selection();
        self.offered()
            .map(|s| SchemeStatus {
                scheme: s.scheme,
                note: s.note,
                group: s.group,
                group_title: s.group.title(),
                kind: s.kind.as_str(),
                enabled: want.iter().any(|w| w == s.scheme),
                active: self.registrar.is_active(s.scheme),
            })
            .collect()
    }

    /// `links:set`: turns a scheme on or off, applies, and returns the new list.
    pub fn set(&self, scheme: &str, enabled: bool) -> Result<Vec<SchemeStatus>, LinkSchemeError> {
        if !self.offered().any(|s| s.scheme == scheme) {
            return Err(LinkSchemeError::UnknownScheme);
        }
        self.prefs.update(|p| {
            let mut want = normalize_selection(p.link_schemes.as_deref());
            want.retain(|s| s != scheme);
            if enabled {
                want.push(scheme.to_owned());
            }
            p.link_schemes = Some(want.into_iter().map(Value::from).collect());
        })?;
        self.apply()?;
        Ok(self.list())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeRegistrar(RefCell<BTreeSet<String>>);

    impl SchemeRegistrar for FakeRegistrar {
        fn register(&self, s: &str) -> bool {
            self.0.borrow_mut().insert(s.into());
            true
        }
        fn unregister(&self, s: &str) -> bool {
            self.0.borrow_mut().remove(s)
        }
        fn is_active(&self, s: &str) -> bool {
            self.0.borrow().contains(s)
        }
    }

    fn prefs_with(v: Value) -> (tempfile::TempDir, PrefsFile) {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(crate::prefs::PREFS_FILE_NAME), v.to_string()).unwrap();
        let p = PrefsFile::new(d.path());
        (d, p)
    }

    #[test]
    fn normalize_keeps_known_schemes_once() {
        let list = [
            json!("music"),
            json!("music"),
            json!("evil"),
            json!(5),
            json!("itms"),
        ];
        assert_eq!(normalize_selection(Some(&list)), ["music", "itms"]);
        assert!(normalize_selection(None).is_empty());
    }

    #[test]
    fn nothing_optional_is_claimed_until_turned_on() {
        let (_d, prefs) = prefs_with(json!({}));
        let reg = FakeRegistrar::default();
        let m = LinkSchemes {
            prefs: &prefs,
            registrar: &reg,
            platform: Platform::Windows,
        };
        m.apply().unwrap();
        assert!(reg.0.borrow().is_empty());
        assert!(m.list().iter().all(|s| !s.enabled && !s.active));
    }

    #[test]
    fn windows_aliases_only_on_windows() {
        let (_d, prefs) = prefs_with(json!({}));
        let reg = FakeRegistrar::default();
        let win = LinkSchemes {
            prefs: &prefs,
            registrar: &reg,
            platform: Platform::Windows,
        };
        let lin = LinkSchemes {
            prefs: &prefs,
            registrar: &reg,
            platform: Platform::Linux,
        };
        assert_eq!(win.list().len(), 24);
        assert_eq!(lin.list().len(), 13);
        assert!(matches!(
            lin.set("itunes.assocprotocol.itms", true),
            Err(LinkSchemeError::UnknownScheme)
        ));
        assert!(win.set("itunes.assocprotocol.itms", true).is_ok());
    }

    #[test]
    fn enabling_and_disabling_registers_unregisters_and_remembers() {
        let (_d, prefs) = prefs_with(json!({}));
        let reg = FakeRegistrar::default();
        let m = LinkSchemes {
            prefs: &prefs,
            registrar: &reg,
            platform: Platform::Windows,
        };
        let list = m.set("music", true).unwrap();
        assert!(reg.0.borrow().contains("music"));
        assert_eq!(prefs.load().link_schemes, Some(vec![json!("music")]));
        assert_eq!(
            prefs.load().link_schemes_applied,
            Some(vec![json!("music")])
        );
        assert!(list.iter().find(|s| s.scheme == "music").unwrap().active);
        m.set("music", false).unwrap();
        assert!(!reg.0.borrow().contains("music"));
        assert_eq!(prefs.load().link_schemes, Some(vec![]));
        assert_eq!(prefs.load().link_schemes_applied, Some(vec![]));
        assert!(m.set("https", true).is_err());
        assert!(m.set("rustet", false).is_err());
    }

    #[test]
    fn apply_never_unregisters_what_rustet_did_not_register() {
        let (_d, prefs) = prefs_with(json!({ "linkSchemes": ["music"], "linkSchemesApplied": [] }));
        let reg = FakeRegistrar::default();
        reg.0.borrow_mut().insert("itunes".into());
        LinkSchemes {
            prefs: &prefs,
            registrar: &reg,
            platform: Platform::Windows,
        }
        .apply()
        .unwrap();
        assert!(reg.0.borrow().contains("music"));
        assert!(
            reg.0.borrow().contains("itunes"),
            "a foreign registration is left alone"
        );
    }

    #[test]
    fn remove_edits_only_our_entries() {
        let text = "[Default Applications]\nx-scheme-handler/music=rustet.desktop\nx-scheme-handler/itms=other.desktop;rustet.desktop;\nx-scheme-handler/itmss=other.desktop\nx-scheme-handler/https=firefox.desktop\n[Added Associations]\nx-scheme-handler/music=rustet.desktop;\n";
        let out = remove_scheme_associations(text, "rustet.desktop", &["music", "itms", "itmss"]);
        assert!(!out.contains("x-scheme-handler/music="));
        assert!(out.contains("x-scheme-handler/itms=other.desktop;"));
        assert!(out.contains("x-scheme-handler/itmss=other.desktop"));
        assert!(out.contains("x-scheme-handler/https=firefox.desktop"));
        assert!(out.contains("[Default Applications]") && out.contains("[Added Associations]"));
        assert_eq!(
            remove_scheme_associations(text, "rustet.desktop", &["itunes"]),
            text
        );
    }

    #[test]
    fn add_puts_rustet_first_keeps_others_and_is_idempotent() {
        let text = "[Default Applications]\nx-scheme-handler/https=firefox.desktop\nx-scheme-handler/itms=other.desktop;\n[Added Associations]\nfoo/bar=x.desktop;\n";
        let once = add_scheme_associations(text, "rustet.desktop", &["music", "itms"]);
        assert!(once.contains("x-scheme-handler/music=rustet.desktop;"));
        assert!(once.contains("x-scheme-handler/itms=rustet.desktop;other.desktop;"));
        assert!(once.contains("x-scheme-handler/https=firefox.desktop"));
        assert!(once.contains("foo/bar=x.desktop;"));
        assert!(
            once.find("x-scheme-handler/music").unwrap()
                < once.find("[Added Associations]").unwrap()
        );
        assert_eq!(
            add_scheme_associations(&once, "rustet.desktop", &["music", "itms"]),
            once
        );
        assert_eq!(
            default_handler(&once, "music").as_deref(),
            Some("rustet.desktop")
        );
        assert_eq!(
            default_handler(&once, "https").as_deref(),
            Some("firefox.desktop")
        );
        assert_eq!(default_handler(&once, "itunes"), None);
        let back = remove_scheme_associations(&once, "rustet.desktop", &["music", "itms"]);
        assert_eq!(
            default_handler(&back, "itms").as_deref(),
            Some("other.desktop")
        );
        assert_eq!(default_handler(&back, "music"), None);
    }

    #[test]
    fn add_creates_the_section() {
        assert_eq!(
            add_scheme_associations("", "rustet.desktop", &["music"]),
            "[Default Applications]\nx-scheme-handler/music=rustet.desktop;\n"
        );
        assert_eq!(
            add_scheme_associations(
                "[Added Associations]\na/b=c.desktop;\n",
                "rustet.desktop",
                &["music"]
            ),
            "[Added Associations]\na/b=c.desktop;\n\n[Default Applications]\nx-scheme-handler/music=rustet.desktop;\n"
        );
    }

    #[test]
    fn desktop_id_follows_flatpak() {
        assert_eq!(linux_desktop_id(&Env::default()), "rustet.desktop");
        let env: Env = [("FLATPAK_ID", "io.github.judahzf.Rustet")]
            .into_iter()
            .collect();
        assert_eq!(linux_desktop_id(&env), "io.github.judahzf.Rustet.desktop");
    }

    #[test]
    fn linux_full_cycle_on_mimeapps_list() {
        let home = tempfile::tempdir().unwrap();
        let f = home.path().join(".config/mimeapps.list");
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(&f, "[Default Applications]\nx-scheme-handler/music=other.desktop;\nx-scheme-handler/https=firefox.desktop\n").unwrap();
        let (_d, prefs) = prefs_with(json!({}));
        let reg = MimeappsRegistrar::new(&Env::default(), home.path());
        let m = LinkSchemes {
            prefs: &prefs,
            registrar: &reg,
            platform: Platform::Linux,
        };
        let l = m.set("music", true).unwrap();
        assert!(l.iter().find(|s| s.scheme == "music").unwrap().active);
        let text = fs::read_to_string(&f).unwrap();
        assert_eq!(
            default_handler(&text, "music").as_deref(),
            Some("rustet.desktop")
        );
        assert!(
            text.contains("other.desktop"),
            "the previous handler stays as a fallback"
        );
        let l = m.set("music", false).unwrap();
        assert!(!l.iter().find(|s| s.scheme == "music").unwrap().active);
        let text = fs::read_to_string(&f).unwrap();
        assert_eq!(
            default_handler(&text, "music").as_deref(),
            Some("other.desktop")
        );
        assert!(text.contains("x-scheme-handler/https=firefox.desktop"));
        assert!(
            !home
                .path()
                .join(".config/mimeapps.list.rustet-tmp")
                .exists()
        );
        m.set("itms", true).unwrap();
        fs::write(&f, "").unwrap();
        LinkSchemes {
            prefs: &prefs,
            registrar: &reg,
            platform: Platform::Linux,
        }
        .apply()
        .unwrap();
        assert_eq!(
            default_handler(&fs::read_to_string(&f).unwrap(), "itms").as_deref(),
            Some("rustet.desktop")
        );
    }
}

/// Every scheme Settings → Links can turn on must be declared by the Linux desktop entry, or
/// the system cannot offer Rustet as a handler for it.
#[cfg(test)]
mod packaging_tests {
    use super::*;

    #[test]
    fn the_desktop_entry_declares_every_linux_scheme() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/rustet.desktop");
        let text = fs::read_to_string(&file).unwrap();
        let linux = APPLE_SCHEMES
            .iter()
            .filter(|s| !s.windows_only)
            .map(|s| s.scheme);
        for scheme in std::iter::once(OWN_SCHEME).chain(linux) {
            assert!(
                text.contains(&format!("x-scheme-handler/{scheme};")),
                "{} is missing {scheme}",
                file.display()
            );
        }
    }
}
