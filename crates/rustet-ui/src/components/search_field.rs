//! The search field, a 38 px pill, with its suggestion list in a glass menu (spec §6.2:
//! debounced 150 ms, stale queries cancelled). Its owner gives it at most
//! `theme.size.search_max_width`.
//!
//! The field does not fetch. It emits [`SearchFieldEvent::Query`] 150 ms after the last
//! keystroke (a newer keystroke cancels the pending one); the owner loads hints
//! (`rustet_app::pages::catalog::hints`) and hands them back with
//! [`SearchField::set_suggestions`], passing the query they answer so a late answer to an old
//! query is dropped. Down and Up move through the suggestions, Enter opens the highlighted one
//! ([`SearchFieldEvent::Choose`]) or searches the text ([`SearchFieldEvent::Submit`]), Escape
//! clears the text, then closes the list.

use std::time::Duration;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, Role, SharedString,
    Subscription, Task, Window, actions, anchored, deferred, div, prelude::*,
};
use rustet_app::pages::model::Artwork;

use super::{Glass, TextInput, TextInputEvent};
use crate::content::ArtworkView;
use crate::icons::Icon;
use crate::theme::ActiveTheme;

actions!(search_field, [Down, Up, Close]);

pub(crate) const CONTEXT: &str = "SearchField";
/// The pause after typing before the field asks for suggestions.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("escape", Close, Some(CONTEXT)),
    ]);
}

/// One line of the suggestion list.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub title: SharedString,
    /// The kind and artist ("Album by Phoebe Bridgers"); a search term has none.
    pub subtitle: Option<SharedString>,
    pub artwork: Option<Artwork>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchFieldEvent {
    /// Load suggestions for this text (debounced).
    Query(String),
    /// Enter with no suggestion highlighted: search for the text.
    Submit(String),
    /// A suggestion was chosen (Enter or click): its index.
    Choose(usize),
}

pub struct SearchField {
    input: Entity<TextInput>,
    suggestions: Vec<Suggestion>,
    /// The query the shown suggestions answer.
    answered: String,
    highlighted: Option<usize>,
    open: bool,
    pending: Option<Task<()>>,
    /// The field's width, measured while painting: the list is as wide as the field.
    width: gpui::Pixels,
    _subscription: Subscription,
}

impl EventEmitter<SearchFieldEvent> for SearchField {}

impl SearchField {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let placeholder = placeholder.into();
        let input = cx.new(|cx| {
            TextInput::new(placeholder, cx)
                .with_leading_icon(Icon::Search)
                .pill()
                .with_role(Role::SearchInput)
                .clear_on_escape()
        });
        let subscription = cx.subscribe(&input, |this, _, e: &TextInputEvent, cx| match e {
            TextInputEvent::Changed(text) => this.changed(text.to_string(), cx),
            TextInputEvent::Submitted(text) => {
                let event = match this.highlighted.filter(|_| this.open) {
                    Some(ix) => SearchFieldEvent::Choose(ix),
                    None => SearchFieldEvent::Submit(text.to_string()),
                };
                this.open = false;
                cx.emit(event);
                cx.notify();
            }
        });
        Self {
            input,
            suggestions: Vec::new(),
            answered: String::new(),
            highlighted: None,
            open: false,
            pending: None,
            width: gpui::px(0.),
            _subscription: subscription,
        }
    }

    pub fn text(&self, cx: &App) -> SharedString {
        self.input.read(cx).text().clone()
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.set_text(text, cx));
    }

    /// Shows `suggestions` if they answer the current text (`query` is the text they were
    /// loaded for). Answers to older text are dropped.
    pub fn set_suggestions(
        &mut self,
        query: &str,
        suggestions: Vec<Suggestion>,
        cx: &mut Context<Self>,
    ) {
        if query != self.text(cx).as_ref() {
            return;
        }
        self.answered = query.to_owned();
        self.open = !suggestions.is_empty();
        self.suggestions = suggestions;
        self.highlighted = None;
        cx.notify();
    }

    pub fn suggestions(&self) -> &[Suggestion] {
        &self.suggestions
    }

    pub fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    fn changed(&mut self, text: String, cx: &mut Context<Self>) {
        self.highlighted = None;
        if text.trim().is_empty() {
            self.pending = None;
            self.open = false;
            self.suggestions.clear();
            cx.notify();
            return;
        }
        // Replacing the task drops (cancels) the previous one.
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |_, cx| cx.emit(SearchFieldEvent::Query(text)))
                .ok();
        }));
        cx.notify();
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let n = self.suggestions.len();
        if n == 0 {
            return;
        }
        self.open = true;
        self.highlighted = Some(match (self.highlighted, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        });
        cx.notify();
    }
}

impl Render for SearchField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let list = self.open.then(|| {
            let art = theme.size.art_row;
            let rows = self.suggestions.iter().enumerate().map(|(ix, s)| {
                let highlighted = self.highlighted == Some(ix);
                let label: SharedString = match &s.subtitle {
                    Some(sub) => format!("{}, {}", s.title, sub).into(),
                    None => s.title.clone(),
                };
                div()
                    .id(ix)
                    .role(Role::ListBoxOption)
                    .aria_label(label)
                    .aria_selected(highlighted)
                    .when(highlighted, |d| {
                        d.aria_active_descendant().bg(colors.selected)
                    })
                    .hover(|d| d.bg(colors.hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open = false;
                        cx.emit(SearchFieldEvent::Choose(ix));
                        cx.notify();
                    }))
                    .flex()
                    .items_center()
                    .gap(theme.space.s)
                    .min_h(theme.size.song_row)
                    .px(theme.space.s)
                    .rounded(theme.radius.medium)
                    .child(match &s.artwork {
                        Some(a) => ArtworkView::new(Some(a.clone()), art).into_any_element(),
                        None => div()
                            .size(art)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::Search
                                    .element()
                                    .size(theme.size.icon_small)
                                    .text_color(colors.text_secondary),
                            )
                            .into_any_element(),
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.text.body)
                                    .text_color(colors.text)
                                    .child(s.title.clone()),
                            )
                            .children(s.subtitle.clone().map(|t| {
                                div()
                                    .truncate()
                                    .text_size(theme.text.callout)
                                    .text_color(colors.text_secondary)
                                    .child(t)
                            })),
                    )
            });
            deferred(
                anchored().snap_to_window_with_margin(theme.space.s).child(
                    div()
                        .id("suggestions")
                        .role(Role::ListBox)
                        .aria_label("Suggestions")
                        .occlude()
                        .mt(theme.space.xs)
                        .w(self.width)
                        .p(theme.space.s6)
                        .rounded(theme.radius.menu)
                        .glass(cx)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.open = false;
                            cx.notify();
                        }))
                        .children(rows),
                ),
            )
            .with_priority(1)
        });
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step(true, cx)))
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step(false, cx)))
            .on_action(cx.listener(|this, _: &Close, _, cx| {
                if this.open {
                    this.open = false;
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .relative()
            .w_full()
            .child(self.input.clone())
            .children(list)
            .child({
                let entity = cx.entity();
                gpui::canvas(
                    move |bounds, window, cx| {
                        entity.update(cx, |this, cx| {
                            if this.width != bounds.size.width {
                                this.width = bounds.size.width;
                                cx.notify();
                                window.request_animation_frame();
                            }
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
    }
}

impl Focusable for SearchField {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}
