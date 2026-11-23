//! The sidebar (`docs/native-ui-look.md`, "Sidebar"): the "Rustet" wordmark, the destinations
//! (Search, Home, New, Radio), the library, the playlists with their folder tree, and pinned
//! at the bottom Downloads, Settings and the account. In the narrow window it is a column of
//! icons (the rail); in the narrowest, a menu in the strip.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Role, SharedString, Stateful, div,
    prelude::*,
};
use rustet_app::account::{AccountState, Backend};
use rustet_app::library::{PlaylistEntry, playlist_folder};
use rustet_app::route::{LibrarySection, Route};
use rustet_ui::components::{Button, FocusRing, MenuItem, Scrollbar, Tooltip};
use rustet_ui::frame::Layout;
use rustet_ui::icons::Icon;
use rustet_ui::theme::{ActiveTheme, BOLD, SEMIBOLD};
use tracing::debug;

use crate::shell::{Confirm, Shell};

/// A destination of the rail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailItem {
    pub label: &'static str,
    pub icon: Icon,
    pub route: Route,
}

const fn item(label: &'static str, icon: Icon, route: Route) -> RailItem {
    RailItem { label, icon, route }
}

/// The destinations above the library, in Apple's order: Search, Home (signed in only), New,
/// Radio (unless hidden).
pub fn destinations(signed_in: bool, hide_radio: bool) -> Vec<RailItem> {
    let mut items = vec![item(
        "Search",
        Icon::Search,
        Route::Search {
            term: String::new(),
        },
    )];
    if signed_in {
        items.push(item("Home", Icon::Home, Route::Home));
    }
    items.push(item("New", Icon::New, Route::New));
    if !hide_radio {
        items.push(item("Radio", Icon::Radio, Route::Radio));
    }
    items
}

/// The library sections of the sidebar.
pub fn library() -> [RailItem; 4] {
    [
        item(
            "Recently Added",
            Icon::Clock,
            Route::Library(LibrarySection::RecentlyAdded),
        ),
        item(
            "Artists",
            Icon::Artist,
            Route::Library(LibrarySection::Artists),
        ),
        item(
            "Albums",
            Icon::Album,
            Route::Library(LibrarySection::Albums),
        ),
        item("Songs", Icon::Song, Route::Library(LibrarySection::Songs)),
    ]
}

/// The destinations pinned at the bottom, above the account.
pub fn footer() -> [RailItem; 2] {
    [
        item("Downloads", Icon::Download, Route::Downloads),
        item(
            "Settings",
            Icon::Settings,
            Route::Settings { section: None },
        ),
    ]
}

/// Whether the rail item for `item` is the page shown: any search is Search, any section of
/// Settings is Settings.
pub fn is_active(item: &Route, shown: &Route) -> bool {
    match (item, shown) {
        (Route::Search { .. }, Route::Search { .. })
        | (Route::Settings { .. }, Route::Settings { .. }) => true,
        (a, b) => a == b,
    }
}

/// A folder of the playlist tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Folder {
    Loading,
    Ready(Vec<PlaylistEntry>),
    Failed,
}

/// The playlist tree, loaded a folder at a time. Which folders are open is kept across
/// reloads (a sign-in, an edited playlist).
#[derive(Debug, Default)]
pub struct PlaylistTree {
    signed_in: bool,
    /// By folder ID; `None` is the top level.
    folders: HashMap<Option<String>, Folder>,
    open: HashSet<String>,
    /// Bumped by every reload: an answer for an older tree is dropped.
    generation: u64,
}

impl PlaylistTree {
    pub fn session_changed(&mut self, signed_in: bool) {
        self.signed_in = signed_in;
        self.invalidate();
    }

    pub fn invalidate(&mut self) {
        self.folders.clear();
        self.generation += 1;
    }

    pub fn is_open(&self, id: &str) -> bool {
        self.open.contains(id)
    }

    /// Opens or closes a folder; returns whether it is open now.
    pub fn toggle(&mut self, id: &str) -> bool {
        if !self.open.remove(id) {
            self.open.insert(id.to_owned());
            return true;
        }
        false
    }

    pub fn folder(&self, id: Option<&str>) -> Option<&Folder> {
        self.folders.get(&id.map(str::to_owned))
    }

    /// Marks `id` loading; `None` when it needs no load (signed out, loaded or loading).
    fn start(&mut self, id: Option<String>) -> Option<u64> {
        if !self.signed_in || self.folders.contains_key(&id) {
            return None;
        }
        self.folders.insert(id, Folder::Loading);
        Some(self.generation)
    }

    /// A folder's answer; returns the open subfolders still to load.
    fn loaded(&mut self, generation: u64, id: Option<String>, folder: Folder) -> Vec<String> {
        if generation != self.generation {
            return Vec::new();
        }
        let open = match &folder {
            Folder::Ready(entries) => entries
                .iter()
                .filter_map(|e| match e {
                    PlaylistEntry::Folder { id, .. } if self.open.contains(id) => Some(id.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        self.folders.insert(id, folder);
        open
    }
}

/// A sidebar row: 36 px high, radius 8, the 18 px icon in the nav tint and the label (title 3,
/// 600 when selected). In the icon rail only the icon shows; the label is the tooltip.
fn nav_row(
    id: impl Into<ElementId>,
    icon: Icon,
    label: SharedString,
    selected: bool,
    full: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let colors = theme.colors;
    let surface = if selected {
        colors.sidebar.blend(colors.selected)
    } else {
        colors.sidebar
    };
    div()
        .id(id)
        .aria_label(label.clone())
        .tab_index(0)
        .focus_ring_on(surface, cx)
        .h(theme.size.nav_item)
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.space.s)
        .px(theme.size.nav_icon_pad)
        .rounded(theme.radius.card)
        .text_size(theme.text.title3)
        .text_color(colors.text)
        .cursor_pointer()
        .when(selected, |d| d.bg(colors.selected).font_weight(SEMIBOLD))
        .when(!selected, |d| d.hover(|s| s.bg(colors.selected)))
        .when(!full, |d| {
            d.justify_center().tooltip(Tooltip::text(label.clone()))
        })
        .child(
            icon.element()
                .size(theme.size.icon)
                .text_color(colors.nav_icon),
        )
        .when(full, |d| {
            d.child(div().flex_1().min_w_0().truncate().child(label))
        })
}

impl Shell {
    /// Loads a folder of the playlist tree (`None`: the top level) unless it is there.
    pub(crate) fn load_playlists(&mut self, folder: Option<String>, cx: &mut Context<Self>) {
        let Some(generation) = self.playlists.start(folder.clone()) else {
            return;
        };
        let api = self.app.api().clone();
        let id = folder.clone();
        let job =
            rustet_ui::runtime::spawn(
                cx,
                async move { playlist_folder(&api, id.as_deref()).await },
            );
        cx.spawn(async move |this, cx| {
            let folder_state = match job.await {
                Ok(Ok(entries)) => Folder::Ready(entries),
                Ok(Err(e)) => {
                    debug!("playlist folder {folder:?}: {e}");
                    Folder::Failed
                }
                Err(_) => return,
            };
            this.update(cx, |s, cx| {
                for open in s.playlists.loaded(generation, folder, folder_state) {
                    s.load_playlists(Some(open), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Goes to an item's route. The Search item opens the Search page and focuses its field.
    pub(crate) fn open_item(&mut self, route: Route, cx: &mut Context<Self>) {
        if matches!(route, Route::Search { .. }) {
            self.open_search(cx);
        } else {
            self.go(route, cx);
        }
    }

    fn rail_item(&self, item: &RailItem, full: bool, cx: &mut Context<Self>) -> AnyElement {
        let route = item.route.clone();
        nav_row(
            SharedString::from(format!("rail-{}", item.label)),
            item.icon,
            item.label.into(),
            is_active(&item.route, &self.nav.route),
            full,
            cx,
        )
        .role(Role::Tab)
        .aria_selected(is_active(&item.route, &self.nav.route))
        .on_click(cx.listener(move |this, _, _, cx| this.open_item(route.clone(), cx)))
        .into_any_element()
    }

    /// A section head ("Library", "Playlists"): 11 / 600, secondary. The icon rail has no
    /// labels: a hairline marks the section there.
    fn rail_heading(&self, label: &'static str, full: bool, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if !full {
            return div()
                .my(theme.space.s)
                .mx(theme.space.s)
                .h(theme.size.hairline)
                .bg(theme.colors.sidebar_border)
                .into_any_element();
        }
        div()
            .pl(theme.size.nav_icon_pad)
            .pt(theme.space.l)
            .pb(theme.space.xs)
            .text_size(theme.text.subhead)
            .font_weight(SEMIBOLD)
            .text_color(theme.colors.text_secondary)
            .child(label)
            .into_any_element()
    }

    /// A line of the playlist tree that is not an entry ("Loading…", "No playlists yet").
    fn tree_note(indent: gpui::Pixels, text: &'static str, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        div()
            .h(theme.size.nav_item)
            .flex()
            .flex_none()
            .items_center()
            .pl(theme.size.nav_icon_pad + indent)
            .text_size(theme.text.callout)
            .text_color(theme.colors.text_tertiary)
            .child(text)
    }

    /// The entries of a folder, nested folders indented under their open parent.
    fn playlist_entries(
        &mut self,
        folder: Option<&str>,
        depth: usize,
        out: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let indent = theme.space.m * depth as f32;
        let entries = match self.playlists.folder(folder) {
            Some(Folder::Ready(entries)) => entries.clone(),
            Some(Folder::Loading) | None => {
                out.push(Self::tree_note(indent, "Loading…", cx).into_any_element());
                return;
            }
            Some(Folder::Failed) => {
                let id = folder.map(str::to_owned);
                out.push(
                    Self::tree_note(indent, "Playlists did not load. Try again.", cx)
                        .id(SharedString::from(format!(
                            "playlists-retry-{}",
                            id.as_deref().unwrap_or("top")
                        )))
                        .role(Role::Button)
                        .aria_label("Playlists did not load. Try again.")
                        .tab_index(0)
                        .focus_ring_on(colors.sidebar, cx)
                        .rounded(theme.radius.card)
                        .text_color(colors.text_secondary)
                        .cursor_pointer()
                        .hover(|s| s.bg(colors.selected))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.playlists.folders.remove(&id);
                            this.load_playlists(id.clone(), cx);
                        }))
                        .into_any_element(),
                );
                return;
            }
        };
        if entries.is_empty() && depth == 0 {
            out.push(Self::tree_note(indent, "No playlists yet", cx).into_any_element());
        }
        for entry in entries {
            match entry {
                PlaylistEntry::Folder { id, name } => {
                    let open = self.playlists.is_open(&id);
                    let toggle_id = id.clone();
                    out.push(
                        nav_row(
                            SharedString::from(format!("folder-{id}")),
                            Icon::Folder,
                            name.into(),
                            false,
                            true,
                            cx,
                        )
                        .role(Role::TreeItem)
                        .aria_expanded(open)
                        .pl(theme.size.nav_icon_pad + indent)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.playlists.toggle(&toggle_id) {
                                this.load_playlists(Some(toggle_id.clone()), cx);
                            }
                            cx.notify();
                        }))
                        .child(
                            if open {
                                Icon::ChevronDown
                            } else {
                                Icon::ChevronRight
                            }
                            .element()
                            .size(theme.size.icon_small)
                            .text_color(colors.text_secondary),
                        )
                        .into_any_element(),
                    );
                    if open {
                        self.playlist_entries(Some(&id), depth + 1, out, cx);
                    }
                }
                PlaylistEntry::Playlist { id, name, .. } => {
                    let route = Route::LibraryPlaylist { id: id.clone() };
                    let active = self.nav.route == route;
                    out.push(
                        nav_row(
                            SharedString::from(format!("playlist-{id}")),
                            Icon::Playlist,
                            name.into(),
                            active,
                            true,
                            cx,
                        )
                        .role(Role::TreeItem)
                        .aria_selected(active)
                        .pl(theme.size.nav_icon_pad + indent)
                        .on_click(cx.listener(move |this, _, _, cx| this.go(route.clone(), cx)))
                        .into_any_element(),
                    );
                }
            }
        }
    }

    fn account_menu(&self, cx: &mut Context<Self>) -> Vec<MenuItem> {
        let entity = cx.entity();
        let (a, b, c) = (entity.clone(), entity.clone(), entity);
        let mut items = vec![MenuItem::action("Account settings", move |_, cx| {
            a.update(cx, |s, cx| {
                s.go(
                    Route::Settings {
                        section: Some("account".into()),
                    },
                    cx,
                )
            })
        })];
        items.push(MenuItem::Separator);
        items.push(MenuItem::action("Sign out", move |_, cx| {
            b.update(cx, |s, cx| s.confirm(Confirm::SignOut, cx))
        }));
        if self.backend == Some(Backend::FairPlay) {
            items.push(MenuItem::action("Forget this device", move |_, cx| {
                c.update(cx, |s, cx| s.confirm(Confirm::ForgetDevice, cx))
            }));
        }
        items
    }

    /// The account control at the foot of the sidebar. Signed out it is the full-width key
    /// pill "Sign In" (in the icon rail, a person row); signed in, a row that opens the
    /// account menu. The account model has no user name: the row names the service and the
    /// storefront.
    fn account_entry(&self, full: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let muted = |text: &str| {
            div()
                .px(theme.size.nav_inset_left)
                .text_size(theme.text.callout)
                .text_color(colors.text_secondary)
                .child(text.to_owned())
                .into_any_element()
        };
        let sign_in = |cx: &mut Context<Self>| {
            let can = self.backend.is_some();
            if full {
                div()
                    .px(theme.space.m)
                    .child(
                        Button::new("account-sign-in", "Sign In")
                            .primary()
                            .large()
                            .full_width()
                            .icon(Icon::Person)
                            .disabled(!can)
                            .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx))),
                    )
                    .into_any_element()
            } else {
                let row = nav_row(
                    "account-sign-in",
                    Icon::Person,
                    "Sign In".into(),
                    false,
                    false,
                    cx,
                )
                .role(Role::Button);
                inset(false, div(), cx)
                    .child(if can {
                        row.on_click(cx.listener(|this, _, _, cx| this.sign_in(cx)))
                    } else {
                        row.opacity(0.4)
                    })
                    .into_any_element()
            }
        };
        match &self.account {
            AccountState::Starting if full => muted("Connecting to the engine…"),
            AccountState::SigningIn(_) if full => muted("Signing in…"),
            AccountState::Starting | AccountState::SigningIn(_) => div().into_any_element(),
            AccountState::SignedOut => sign_in(cx),
            AccountState::Error { message } => div()
                .flex()
                .flex_col()
                .gap(theme.space.s)
                .when(full, |d| d.child(muted(message)))
                .child(sign_in(cx))
                .into_any_element(),
            AccountState::SignedIn { storefront } => {
                let label: SharedString =
                    format!("Apple Music account, {}", storefront.to_uppercase()).into();
                let row = nav_row(
                    "account",
                    Icon::Person,
                    "Apple Music".into(),
                    false,
                    full,
                    cx,
                )
                .role(Role::Button)
                .aria_label(label)
                .on_click(cx.listener(|this, e: &ClickEvent, window, cx| {
                    let items = this.account_menu(cx);
                    this.open_menu(items, e.position(), window, cx);
                }))
                .when(full, |d| {
                    d.child(
                        div()
                            .text_size(theme.text.callout)
                            .text_color(colors.text_secondary)
                            .child(storefront.to_uppercase()),
                    )
                });
                inset(full, div(), cx).child(row).into_any_element()
            }
        }
    }

    pub(crate) fn rail(&mut self, layout: Layout, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let full = layout == Layout::Full;
        let signed_in = self.signed_in();
        let mut nav: Vec<AnyElement> = destinations(signed_in, self.settings.appearance.hide_radio)
            .iter()
            .map(|i| self.rail_item(i, full, cx))
            .collect();
        nav.push(self.rail_heading("Library", full, cx));
        nav.extend(library().iter().map(|i| self.rail_item(i, full, cx)));
        if signed_in {
            nav.push(self.rail_heading("Playlists", full, cx));
            if full {
                self.load_playlists(None, cx);
                self.playlist_entries(None, 0, &mut nav, cx);
            } else {
                let item = item(
                    "Playlists",
                    Icon::Playlist,
                    Route::Library(LibrarySection::Playlists),
                );
                nav.push(self.rail_item(&item, false, cx));
            }
        }
        let pinned: Vec<AnyElement> = footer()
            .iter()
            .map(|i| self.rail_item(i, full, cx))
            .collect();
        div()
            .id("rail")
            .role(Role::Navigation)
            .aria_label("Rustet")
            .w(layout.sidebar_width(&theme))
            .h_full()
            .flex()
            .flex_col()
            // The wordmark at 28, 30; navigation from 80. The icon rail starts under the strip.
            .child(if full {
                div()
                    .flex_none()
                    .h(theme.size.nav_top)
                    .pt(theme.size.wordmark_top)
                    .pl(theme.size.wordmark_left)
                    .text_size(theme.text.title1)
                    .font_weight(BOLD)
                    .text_color(colors.text)
                    .child("Rustet")
            } else {
                div().flex_none().h(theme.size.strip)
            })
            // Everything between the wordmark and the pinned foot scrolls as one.
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        inset(full, div(), cx)
                            .id("rail-nav")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.rail_scroll)
                            .flex()
                            .flex_col()
                            .gap(theme.size.nav_gap)
                            .pb(theme.space.m)
                            .children(nav),
                    )
                    .child(Scrollbar::new(
                        "rail-nav-scrollbar",
                        self.rail_scroll.clone(),
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .mx(theme.space.m)
                    .h(theme.size.hairline)
                    .bg(colors.sidebar_border),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(theme.size.nav_gap)
                    .pt(theme.space.s)
                    .pb(theme.space.m)
                    .child(
                        inset(full, div(), cx)
                            .flex()
                            .flex_col()
                            .gap(theme.size.nav_gap)
                            .children(pinned),
                    )
                    .child(self.account_entry(full, cx)),
            )
            .into_any_element()
    }

    /// The sidebar's destinations as a menu: the narrowest window has no sidebar.
    pub(crate) fn compact_menu(&self, cx: &mut Context<Self>) -> Vec<MenuItem> {
        let signed_in = self.signed_in();
        let entity = cx.entity();
        let to_menu = |items: &[RailItem]| -> Vec<MenuItem> {
            items
                .iter()
                .map(|i| {
                    let (e, route) = (entity.clone(), i.route.clone());
                    MenuItem::action(i.label, move |_, cx| {
                        e.update(cx, |s, cx| s.open_item(route.clone(), cx))
                    })
                    .icon(i.icon)
                })
                .collect()
        };
        let mut items = to_menu(&destinations(
            signed_in,
            self.settings.appearance.hide_radio,
        ));
        items.push(MenuItem::Separator);
        let mut lib = library().to_vec();
        if signed_in {
            lib.push(item(
                "Playlists",
                Icon::Playlist,
                Route::Library(LibrarySection::Playlists),
            ));
        }
        items.extend(to_menu(&lib));
        items.push(MenuItem::Separator);
        items.extend(to_menu(&footer()));
        items
    }
}

/// The sidebar's side insets: 20 left and 13 right in the full sidebar (Apple's), 12 each
/// side in the icon rail.
fn inset(full: bool, d: Div, cx: &App) -> Div {
    let theme = cx.theme();
    if full {
        d.pl(theme.size.nav_inset_left)
            .pr(theme.size.nav_inset_right)
    } else {
        d.px(theme.space.m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_needs_the_sign_in_and_radio_can_be_hidden() {
        let labels = |signed_in, hide| {
            destinations(signed_in, hide)
                .iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(false, false), ["Search", "New", "Radio"]);
        assert_eq!(labels(true, false), ["Search", "Home", "New", "Radio"]);
        assert_eq!(labels(true, true), ["Search", "Home", "New"]);
    }

    #[test]
    fn any_search_and_any_settings_section_light_their_item() {
        let search = Route::Search {
            term: String::new(),
        };
        assert!(is_active(
            &search,
            &Route::Search {
                term: "phoebe".into()
            }
        ));
        assert!(is_active(
            &Route::Settings { section: None },
            &Route::Settings {
                section: Some("account".into())
            }
        ));
        assert!(!is_active(&Route::New, &Route::Radio));
    }

    #[test]
    fn open_folders_stay_open_across_a_reload() {
        let mut tree = PlaylistTree::default();
        tree.session_changed(true);
        let g = tree.start(None).unwrap();
        assert_eq!(tree.start(None), None, "one load at a time");
        assert!(tree.toggle("f1"));
        let top = vec![
            PlaylistEntry::Folder {
                id: "f1".into(),
                name: "Running".into(),
            },
            PlaylistEntry::Folder {
                id: "f2".into(),
                name: "Old".into(),
            },
        ];
        assert_eq!(
            tree.loaded(g, None, Folder::Ready(top.clone())),
            ["f1"],
            "the open folder loads too"
        );
        tree.invalidate();
        assert!(tree.is_open("f1"));
        assert_eq!(
            tree.loaded(g, None, Folder::Ready(top)),
            Vec::<String>::new(),
            "an old answer is dropped"
        );
        assert_eq!(tree.folder(None), None);
        tree.session_changed(false);
        assert_eq!(tree.start(None), None, "signed out: nothing to load");
    }
}
