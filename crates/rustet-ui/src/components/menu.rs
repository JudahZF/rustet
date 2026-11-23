//! Context menus with sub-menus ("Add to playlist ▸"): glass, radius 10, 30 px rows with the
//! item's glyph at the right.
//!
//! Keyboard: Down and Up move, Enter runs the item or opens its sub-menu, Right opens a
//! sub-menu, Left and Escape close it (Escape on the top menu closes the menu). A click outside
//! dismisses. The owner keeps the open menu in a [`Popover`] and draws [`Popover::render`] last.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    KeyBinding, MouseButton, Pixels, Point, Role, SharedString, Subscription, Window, actions,
    anchored, deferred, div, prelude::*,
};

use super::Glass;
use crate::icons::Icon;
use crate::theme::ActiveTheme;

actions!(
    menu,
    [
        Cancel,
        SelectNext,
        SelectPrevious,
        Confirm,
        OpenSubmenu,
        CloseSubmenu
    ]
);

pub(crate) const CONTEXT: &str = "Menu";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(CONTEXT)),
        KeyBinding::new("up", SelectPrevious, Some(CONTEXT)),
        KeyBinding::new("enter", Confirm, Some(CONTEXT)),
        KeyBinding::new("space", Confirm, Some(CONTEXT)),
        KeyBinding::new("right", OpenSubmenu, Some(CONTEXT)),
        KeyBinding::new("left", CloseSubmenu, Some(CONTEXT)),
    ]);
}

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// An entry of a menu.
#[derive(Clone)]
pub enum MenuItem {
    Action {
        label: SharedString,
        /// Drawn at the row's right edge.
        icon: Option<Icon>,
        disabled: bool,
        handler: Handler,
    },
    Submenu {
        label: SharedString,
        items: Vec<MenuItem>,
    },
    Separator,
}

impl MenuItem {
    pub fn action(
        label: impl Into<SharedString>,
        f: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self::Action {
            label: label.into(),
            icon: None,
            disabled: false,
            handler: Rc::new(f),
        }
    }

    /// The action's glyph, at the right of its row ("Play Next" with its glyph).
    pub fn icon(mut self, glyph: Icon) -> Self {
        if let Self::Action { icon, .. } = &mut self {
            *icon = Some(glyph);
        }
        self
    }

    pub fn submenu(label: impl Into<SharedString>, items: Vec<MenuItem>) -> Self {
        Self::Submenu {
            label: label.into(),
            items,
        }
    }

    /// An action shown but not available ("Download" while signed out).
    pub fn disabled(mut self) -> Self {
        if let Self::Action { disabled, .. } = &mut self {
            *disabled = true;
        }
        self
    }

    fn label(&self) -> Option<&SharedString> {
        match self {
            Self::Action { label, .. } | Self::Submenu { label, .. } => Some(label),
            Self::Separator => None,
        }
    }

    fn glyph(&self) -> Option<Icon> {
        match self {
            Self::Action { icon, .. } => *icon,
            Self::Submenu { .. } => Some(Icon::ChevronRight),
            Self::Separator => None,
        }
    }

    fn enabled(&self) -> bool {
        match self {
            Self::Action { disabled, .. } => !disabled,
            Self::Submenu { items, .. } => !items.is_empty(),
            Self::Separator => false,
        }
    }
}

/// A sub-menu gave the focus back (Left or Escape).
pub struct MenuBack;

pub struct ContextMenu {
    focus_handle: FocusHandle,
    items: Vec<MenuItem>,
    selected: Option<usize>,
    /// The open sub-menu: the item it belongs to.
    child: Option<(usize, Entity<ContextMenu>, [Subscription; 2])>,
    is_submenu: bool,
    /// The menu's accessible name.
    label: SharedString,
}

/// The name of a top-level menu unless its owner gives one ([`ContextMenu::set_label`]).
const DEFAULT_LABEL: &str = "Actions";

impl EventEmitter<DismissEvent> for ContextMenu {}
impl EventEmitter<MenuBack> for ContextMenu {}

impl ContextMenu {
    /// A menu of `items`, focused.
    pub fn new(items: Vec<MenuItem>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::create(items, false, DEFAULT_LABEL.into(), window, cx)
    }

    /// Names the menu for assistive technology ("Song actions"). A sub-menu is named after
    /// its item ("Add to playlist").
    pub fn set_label(&mut self, label: impl Into<SharedString>) {
        self.label = label.into();
    }

    fn create(
        items: Vec<MenuItem>,
        is_submenu: bool,
        label: SharedString,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let menu = cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            items,
            selected: None,
            child: None,
            is_submenu,
            label,
        });
        window.focus(&menu.focus_handle(cx), cx);
        menu
    }

    /// The menu's items, for tests and the gallery.
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    fn run(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.items.get(ix) {
            Some(MenuItem::Action {
                handler,
                disabled: false,
                ..
            }) => {
                let handler = handler.clone();
                cx.emit(DismissEvent);
                handler(window, cx);
            }
            Some(MenuItem::Submenu { .. }) => self.open_child(ix, true, window, cx),
            _ => {}
        }
    }

    fn open_child(&mut self, ix: usize, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.child.as_ref().is_some_and(|(open, ..)| *open == ix) {
            if focus && let Some((_, child, _)) = &self.child {
                let handle = child.read(cx).focus_handle.clone();
                window.focus(&handle, cx);
                child.update(cx, |c, cx| c.select(true, cx));
            }
            return;
        }
        let Some(MenuItem::Submenu { items, label }) = self.items.get(ix) else {
            return;
        };
        let child = Self::create(items.clone(), true, label.clone(), window, cx);
        if focus {
            child.update(cx, |c, cx| c.select(true, cx));
        } else {
            // Opened by hover: the pointer stays in charge, the keyboard focus stays here.
            window.focus(&self.focus_handle, cx);
        }
        let subs = [
            // An item ran in the sub-menu: the whole menu closes.
            cx.subscribe(&child, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent)),
            cx.subscribe_in(&child, window, |this, _, _: &MenuBack, window, cx| {
                this.child = None;
                window.focus(&this.focus_handle, cx);
                cx.notify();
            }),
        ];
        self.selected = Some(ix);
        self.child = Some((ix, child, subs));
        cx.notify();
    }

    fn select(&mut self, forward: bool, cx: &mut Context<Self>) {
        let enabled: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.enabled())
            .map(|(ix, _)| ix)
            .collect();
        if enabled.is_empty() {
            return;
        }
        let pos = self
            .selected
            .and_then(|s| enabled.iter().position(|&a| a == s));
        let next = match (pos, forward) {
            (None, true) => 0,
            (None, false) => enabled.len() - 1,
            (Some(p), true) => (p + 1) % enabled.len(),
            (Some(p), false) => (p + enabled.len() - 1) % enabled.len(),
        };
        self.selected = Some(enabled[next]);
        if self
            .child
            .as_ref()
            .is_some_and(|(open, ..)| Some(*open) != self.selected)
        {
            self.child = None;
        }
        cx.notify();
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let is_submenu = self.is_submenu;
        div()
            .id("menu")
            .role(Role::Menu)
            .aria_label(self.label.clone())
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(move |_, _: &Cancel, _, cx| {
                if is_submenu {
                    cx.emit(MenuBack);
                } else {
                    cx.emit(DismissEvent);
                }
            }))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select(true, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.select(false, cx)))
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                if let Some(ix) = this.selected {
                    this.run(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSubmenu, window, cx| {
                if let Some(ix) = this
                    .selected
                    .filter(|&ix| matches!(this.items[ix], MenuItem::Submenu { .. }))
                {
                    this.open_child(ix, true, window, cx);
                }
            }))
            .on_action(cx.listener(move |_, _: &CloseSubmenu, _, cx| {
                if is_submenu {
                    cx.emit(MenuBack);
                }
            }))
            .when(!is_submenu, |d| {
                d.on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            })
            .occlude()
            .min_w(theme.size.menu_min_width)
            .p(theme.space.s6)
            .rounded(theme.radius.menu)
            .glass(cx)
            .text_size(theme.text.body)
            .text_color(colors.text)
            .children(self.items.iter().enumerate().map(|(ix, item)| {
                let Some(label) = item.label().cloned() else {
                    return div()
                        .my(theme.space.xs)
                        .mx(theme.space.s)
                        .h(theme.size.hairline)
                        .bg(colors.divider)
                        .into_any_element();
                };
                let enabled = item.enabled();
                let selected = self.selected == Some(ix);
                let submenu = matches!(item, MenuItem::Submenu { .. });
                let glyph = item.glyph();
                let child = self
                    .child
                    .as_ref()
                    .filter(|(open, ..)| *open == ix)
                    .map(|(_, c, _)| c.clone());
                div()
                    .id(ix)
                    .role(Role::MenuItem)
                    .aria_label(label.clone())
                    .when(submenu, |d| d.aria_expanded(child.is_some()))
                    .when(selected, |d| d.aria_active_descendant().bg(colors.hover))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(theme.space.l)
                    .h(theme.size.menu_row)
                    .px(theme.space.s)
                    .rounded(theme.radius.medium)
                    .when(!enabled, |d| d.text_color(colors.text_tertiary))
                    .when(enabled, |d| {
                        d.hover(|s| s.bg(colors.hover))
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.run(ix, window, cx)),
                            )
                            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                                if !*hovered {
                                    return;
                                }
                                this.selected = Some(ix);
                                if submenu {
                                    this.open_child(ix, false, window, cx);
                                } else if this.child.is_some() {
                                    this.child = None;
                                }
                                cx.notify();
                            }))
                    })
                    .child(label)
                    .children(glyph.map(|g| {
                        g.element()
                            .size(theme.size.icon_small)
                            .text_color(if enabled {
                                colors.text
                            } else {
                                colors.text_tertiary
                            })
                    }))
                    .children(child.map(|c| {
                        // Anchored at the item's right edge, drawn above the parent menu.
                        div().absolute().top(-theme.space.xs).left_full().child(
                            deferred(
                                anchored()
                                    .snap_to_window_with_margin(theme.space.s)
                                    .child(div().ml(theme.space.xs).child(c)),
                            )
                            .with_priority(3),
                        )
                    }))
                    .into_any_element()
            }))
    }
}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// An open context menu and where it is anchored. Owned by the view that opened it.
#[derive(Default)]
pub struct Popover {
    open: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
}

impl Popover {
    /// Shows `menu` at `position` (window coordinates); it closes itself on dismiss.
    pub fn show<V>(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        cx: &mut Context<V>,
    ) where
        V: AsMut<Popover> + 'static,
    {
        let sub = cx.subscribe(&menu, |this: &mut V, _, _: &DismissEvent, cx| {
            this.as_mut().open = None;
            cx.notify();
        });
        self.open = Some((menu, position, sub));
        cx.notify();
    }

    pub fn close(&mut self) {
        self.open = None;
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The popover element, drawn above everything else, or nothing.
    pub fn render(&self, cx: &App) -> Option<AnyElement> {
        let (menu, position, _) = self.open.as_ref()?;
        Some(
            deferred(
                anchored()
                    .position(*position)
                    .snap_to_window_with_margin(cx.theme().space.s)
                    .child(menu.clone()),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }
}
