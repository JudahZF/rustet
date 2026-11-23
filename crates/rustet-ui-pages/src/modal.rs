//! The modals pages open through the window ([`HostRequest::Modal`]): the playlist name sheet
//! (new playlist, rename), the confirm dialog, and the playlist picker.
//!
//! Each closes itself with [`HostRequest::CloseModal`]; the host gives the focus back.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Role, SharedString, Styled, Subscription, Window, div, prelude::*,
};
use rustet_ui::components::{Button, Dialog, FocusRing, Sheet, TextInput, TextInputEvent};
use rustet_ui::host::{Host, HostRequest};
use rustet_ui::theme::ActiveTheme;

use crate::context::PlaylistChoice;

type OnText = Rc<dyn Fn(String, &mut Window, &mut App)>;
type OnChoice = Rc<dyn Fn(&PlaylistChoice, &mut Window, &mut App)>;
type OnConfirm = Rc<dyn Fn(&mut Window, &mut App)>;

fn close(window: &mut Window, cx: &mut App) {
    Host::request(HostRequest::CloseModal, window, cx);
}

/// Opens `view` as the window's modal.
pub fn open(view: gpui::AnyView, window: &mut Window, cx: &mut App) {
    Host::request(HostRequest::Modal(view), window, cx);
}

/// Asks for a playlist name: "New playlist" (empty) or "Rename playlist" (the current name).
pub struct NameSheet {
    title: SharedString,
    answer: SharedString,
    input: Entity<TextInput>,
    on_submit: OnText,
    _subscription: Subscription,
}

impl NameSheet {
    pub fn new(
        title: &str,
        answer: &str,
        name: &str,
        on_submit: impl Fn(String, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Playlist name", cx);
            input.set_text(name.to_owned(), cx);
            input
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, e: &TextInputEvent, window, cx| match e {
                TextInputEvent::Submitted(_) => this.submit(window, cx),
                TextInputEvent::Changed(_) => cx.notify(),
            },
        );
        Self {
            title: title.to_owned().into(),
            answer: answer.to_owned().into(),
            input,
            on_submit: Rc::new(on_submit),
            _subscription: subscription,
        }
    }

    /// The trimmed name, `None` while empty.
    fn name(&self, cx: &App) -> Option<String> {
        let name = self.input.read(cx).text().trim().to_owned();
        (!name.is_empty()).then_some(name)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.name(cx) {
            close(window, cx);
            (self.on_submit)(name, window, cx);
        }
    }
}

impl Render for NameSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let submit = cx.listener(|this, _, window, cx| this.submit(window, cx));
        let empty = self.name(cx).is_none();
        Sheet::new("playlist-name", self.title.clone())
            .focus_on_open(self.input.focus_handle(cx))
            .on_dismiss(close)
            .child(self.input.clone())
            .child(
                div()
                    .flex()
                    .gap(theme.space.s)
                    .pt(theme.space.s)
                    .child(
                        Button::new("name-submit", self.answer.clone())
                            .primary()
                            .large()
                            .disabled(empty)
                            .on_click(submit),
                    )
                    .child(
                        Button::new("name-cancel", "Cancel")
                            .large()
                            .on_click(|_, w, cx| close(w, cx)),
                    ),
            )
    }
}

/// A question with a primary answer and Cancel ("Delete", "Remove").
pub struct ConfirmDialog {
    title: SharedString,
    message: SharedString,
    answer: SharedString,
    on_confirm: OnConfirm,
}

impl ConfirmDialog {
    pub fn new(
        title: String,
        message: String,
        answer: String,
        on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            answer: answer.into(),
            on_confirm: Rc::new(on_confirm),
        }
    }
}

impl Render for ConfirmDialog {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let confirm = self.on_confirm.clone();
        Dialog::new("confirm", self.title.clone(), self.message.clone())
            .answer(self.answer.clone(), move |w, cx| {
                close(w, cx);
                confirm(w, cx);
            })
            .answer("Cancel", close)
            .on_dismiss(close)
    }
}

/// Every library playlist, filtered by a search field: where "All playlists…" goes.
pub struct PlaylistPicker {
    playlists: Arc<[PlaylistChoice]>,
    filter: Entity<TextInput>,
    on_choice: OnChoice,
    focus: FocusHandle,
    _subscription: Subscription,
}

impl PlaylistPicker {
    pub fn new(
        playlists: Arc<[PlaylistChoice]>,
        on_choice: impl Fn(&PlaylistChoice, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| TextInput::new("Find a playlist", cx));
        let subscription = cx.subscribe(&filter, |_, _, _: &TextInputEvent, cx| cx.notify());
        Self {
            playlists,
            filter,
            on_choice: Rc::new(on_choice),
            focus: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    /// The playlists whose name contains the filter, ignoring case.
    pub fn shown(&self, cx: &App) -> Vec<PlaylistChoice> {
        let filter = self.filter.read(cx).text().to_lowercase();
        self.playlists
            .iter()
            .filter(|p| p.name.to_lowercase().contains(filter.trim()))
            .cloned()
            .collect()
    }
}

impl Focusable for PlaylistPicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PlaylistPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let shown = self.shown(cx);
        let list = div()
            .id("playlist-choices")
            .role(Role::List)
            .aria_label("Playlists")
            .flex()
            .flex_col()
            .max_h(theme.size.sheet_list_max_height)
            .overflow_y_scroll()
            .children(shown.into_iter().enumerate().map(|(ix, choice)| {
                let pick = self.on_choice.clone();
                div()
                    .id(ix)
                    .role(Role::ListItem)
                    .aria_label(SharedString::from(choice.name.clone()))
                    .tab_index(0)
                    .focus_ring_on(theme.colors.sidebar.blend(theme.colors.hover), cx)
                    .flex_none()
                    .h(theme.size.menu_row)
                    .px(theme.space.s)
                    .flex()
                    .items_center()
                    .rounded(theme.radius.medium)
                    .hover(|s| s.bg(theme.colors.hover))
                    .cursor_pointer()
                    .text_size(theme.text.body)
                    .child(div().truncate().child(choice.name.clone()))
                    .on_click(move |_, w, cx| {
                        close(w, cx);
                        pick(&choice, w, cx);
                    })
            }));
        Sheet::new("playlist-picker", "Add to playlist")
            .focus_on_open(self.filter.focus_handle(cx))
            .on_dismiss(close)
            .child(self.filter.clone())
            .child(list)
            .child(
                div().flex().child(
                    Button::new("picker-cancel", "Cancel")
                        .large()
                        .on_click(|_, w, cx| close(w, cx)),
                ),
            )
    }
}
