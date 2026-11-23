//! Settings → Links: which of Apple Music's URL schemes open in Rustet. Claiming one takes the
//! link from any other app, so each is off until turned on (`rustet_desktop::links`).

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AnyElement, Context, IntoElement, Styled};
use rustet_desktop::links::deeplink::SchemeGroup;
use rustet_desktop::links::schemes::{LinkSchemes, MimeappsRegistrar};
use rustet_desktop::platform::{Env, Platform};
use rustet_ui::components::Toggle;
use rustet_ui::theme::ActiveTheme;

use super::SettingsPage;
use crate::DesktopPrefs;
use crate::ui::{group, muted};

impl SettingsPage {
    fn with_schemes<R>(
        &self,
        cx: &mut gpui::App,
        f: impl FnOnce(&LinkSchemes<'_, MimeappsRegistrar>) -> R,
    ) -> R {
        let env = Env::from_process();
        let home = env.get("HOME").map(PathBuf::from).unwrap_or_default();
        let registrar = MimeappsRegistrar::new(&env, &home);
        let prefs: Arc<_> = DesktopPrefs::of(&self.app, cx).prefs.clone();
        f(&LinkSchemes {
            prefs: &prefs,
            registrar: &registrar,
            platform: Platform::current(),
        })
    }

    pub(super) fn refresh_links(&mut self, cx: &mut Context<Self>) {
        if !self.frozen {
            self.status.links = self.with_schemes(cx, |s| s.list());
        }
    }

    pub(super) fn links(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // The lead paragraph lines up with the group labels.
        let mut groups = vec![
            muted(
                "Links of these kinds open in Rustet. Turning one on takes it from any other app that opens it.",
                cx,
            )
            .px(cx.theme().space.m)
            .into_any_element(),
        ];
        for g in SchemeGroup::ALL {
            let rows: Vec<AnyElement> = self
                .status
                .links
                .iter()
                .filter(|s| s.group == g)
                .map(|s| {
                    let scheme = s.scheme;
                    let page = cx.entity();
                    let description = if s.enabled && !s.active {
                        format!("{} Another app opens these links now.", s.note)
                    } else {
                        s.note.to_owned()
                    };
                    Toggle::new(scheme, format!("{scheme}://"), s.enabled)
                        .description(description)
                        .on_change(move |on, _, cx| {
                            page.update(cx, |p, cx| {
                                match p.with_schemes(cx, |s| s.set(scheme, on)) {
                                    Ok(list) => {
                                        p.refused.remove("links");
                                        p.status.links = list;
                                    }
                                    Err(e) => p.refuse(
                                        "links",
                                        format!("Rustet could not change the link ({e})."),
                                        cx,
                                    ),
                                }
                                cx.notify();
                            })
                        })
                        .into_any_element()
                })
                .collect();
            if !rows.is_empty() {
                groups.push(group(g.title(), rows, cx));
            }
        }
        groups.extend(self.refusal_of("links", cx));
        groups
    }
}
