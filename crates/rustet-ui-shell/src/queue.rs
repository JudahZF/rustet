//! The right-side panels (`docs/native-ui-look.md`, "Queue and lyrics"): Playing Next (the
//! tracks after the current one, which a drag reorders, a click jumps to and a button removes;
//! "Clear"; Autoplay), the play history, and the lyrics. The frame draws the 320 px
//! sidebar-coloured panel with its left hairline; this module draws what is in it.

use gpui::{
    AnyElement, Context, Render, Role, SharedString, Window, div, prelude::*, uniform_list,
};
use rustet_app::pages::model::Artwork;
use rustet_app::player::up_next;
use rustet_player::model::{
    Command, HistoryEntry, Jump, Mode, Move, QueueItem, QueueItemIn, Remove, SetQueue, TrackMeta,
};
use rustet_ui::components::{Button, EmptyStateView, FocusRing, Glass, IconButton};
use rustet_ui::content::ArtworkView;
use rustet_ui::content::artwork::Shape;
use rustet_ui::icons::Icon;
use rustet_ui::reorder::DraggedRow;
use rustet_ui::theme::{ActiveTheme, BOLD, SEMIBOLD};

use crate::shell::Shell;

/// The queue index (`POST /queue/move`'s `toIndex`) of row `row` of Up Next: Up Next starts
/// after the current track.
pub fn queue_index(current: Option<usize>, row: usize) -> usize {
    current.map_or(0, |c| c + 1) + row
}

fn artwork(meta: &TrackMeta) -> Option<Artwork> {
    (!meta.artwork_url.is_empty()).then(|| Artwork {
        template: meta.artwork_url.clone(),
        width: None,
        height: None,
        palette: None,
    })
}

/// The label under the pointer while a row is dragged: a glass pill.
struct DragLabel(SharedString);

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px(theme.space.m)
            .py(theme.space.s6)
            .rounded_full()
            .glass(cx)
            .text_size(theme.text.body)
            .text_color(theme.colors.text)
            .child(self.0.clone())
    }
}

impl Shell {
    fn track_text(meta: &TrackMeta, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .truncate()
                    .text_size(theme.text.body)
                    .text_color(theme.colors.text)
                    .child(meta.name.clone()),
            )
            .when(!meta.artist_name.is_empty(), |d| {
                d.child(
                    div()
                        .truncate()
                        .text_size(theme.text.callout)
                        .text_color(theme.colors.text_secondary)
                        .child(meta.artist_name.clone()),
                )
            })
            .into_any_element()
    }

    /// A row of the panel: 55 px, the 40 px artwork, title and artist, the hover fill.
    fn panel_row(
        id: gpui::ElementId,
        label: SharedString,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let colors = theme.colors;
        div()
            .id(id)
            .role(Role::ListItem)
            .aria_label(label)
            .tab_index(0)
            .focus_ring_on(colors.sidebar, cx)
            .w_full()
            .h(theme.size.song_row)
            .px(theme.space.s)
            .flex()
            .items_center()
            .gap(theme.space.m)
            .rounded(theme.radius.medium)
            .cursor_pointer()
            .hover(|s| s.bg(colors.hover))
    }

    fn queue_row(&self, row: usize, item: &QueueItem, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let uid = item.uid;
        let label: SharedString = track_label(&item.meta);
        let current = self.player.as_ref().and_then(|s| s.queue.current_index);
        Self::panel_row(("queue-row", uid).into(), label.clone(), cx)
            .group("queue-row")
            .on_click(cx.listener(move |this, _, _, cx| this.send(Command::Jump(Jump { uid }), cx)))
            .on_drag(
                DraggedRow {
                    index: row,
                    label: label.clone(),
                },
                |row, _, _, cx| {
                    let label = row.label.clone();
                    cx.new(|_| DragLabel(label))
                },
            )
            .drag_over::<DraggedRow>(move |s, _, _, _| s.bg(colors.selected))
            .on_drop(cx.listener(move |this, dragged: &DraggedRow, _, cx| {
                if dragged.index == row {
                    return;
                }
                let items = this
                    .player
                    .as_ref()
                    .map(|s| up_next(s).to_vec())
                    .unwrap_or_default();
                if let Some(moving) = items.get(dragged.index) {
                    let to_index = queue_index(current, row);
                    this.send(
                        Command::Move(Move {
                            uid: moving.uid,
                            to_index,
                        }),
                        cx,
                    );
                }
            }))
            .child(ArtworkView::new(artwork(&item.meta), theme.size.art_row).shape(Shape::Small))
            .child(Self::track_text(&item.meta, cx))
            // Remove and the grip show on the row under the pointer.
            .child(
                div()
                    .flex()
                    .items_center()
                    .invisible()
                    .group_hover("queue-row", |s| s.visible())
                    .child(
                        IconButton::new(
                            ("queue-remove", uid),
                            Icon::Close,
                            "Remove from Playing Next",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.send(Command::Remove(Remove { uids: vec![uid] }), cx)
                        })),
                    )
                    .child(
                        Icon::Grip
                            .element()
                            .size(theme.size.icon_small)
                            .text_color(colors.text_tertiary)
                            .cursor_grab(),
                    ),
            )
            .into_any_element()
    }

    fn history_row(&self, row: usize, entry: &HistoryEntry, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let item = QueueItemIn {
            id: entry.id.clone(),
            library_id: None,
            kind: entry.kind,
            meta: entry.meta.clone(),
            container_id: None,
        };
        Self::panel_row(("history-row", row).into(), track_label(&entry.meta), cx)
            .on_click(cx.listener(move |this, _, _, cx| {
                let command = Command::SetQueue(SetQueue {
                    items: vec![item.clone()],
                    start_index: 0,
                    context: None,
                    shuffle: None,
                    start_ms: None,
                    play: true,
                });
                this.send(command, cx)
            }))
            .child(ArtworkView::new(artwork(&entry.meta), theme.size.art_row).shape(Shape::Small))
            .child(Self::track_text(&entry.meta, cx))
            .into_any_element()
    }

    /// A head of the queue panel ("Playing Next", "History"): 17 / 700. With the history
    /// shown, the two heads are tabs; the other one is secondary.
    fn drawer_tab(
        &self,
        id: &'static str,
        label: &'static str,
        history: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let on = self.history_tab == history;
        div()
            .id(id)
            .role(Role::Tab)
            .aria_label(label)
            .aria_selected(on)
            .tab_index(0)
            .focus_ring_on(theme.colors.sidebar, cx)
            .flex_none()
            .rounded(theme.radius.medium)
            .cursor_pointer()
            .text_color(if on {
                theme.colors.text
            } else {
                theme.colors.text_secondary
            })
            .when(!on, |d| d.hover(|s| s.text_color(theme.colors.text)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.history_tab = history;
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    /// The head row of a panel: its title (or tabs) at the left, `actions` and Close at the
    /// right, as high as the strip beside it.
    fn panel_head(
        &self,
        title: AnyElement,
        actions: Vec<AnyElement>,
        close_label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        div()
            .flex_none()
            .h(theme.size.strip)
            .pl(theme.space.l)
            .pr(theme.space.m)
            .flex()
            .items_center()
            .gap(theme.space.s)
            .text_size(theme.text.title2)
            .font_weight(BOLD)
            .child(title)
            .child(div().flex_1())
            .children(actions)
            .child(
                IconButton::new("drawer-close", Icon::Close, close_label).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.panel = None;
                        cx.notify();
                    },
                )),
            )
            .into_any_element()
    }

    /// A small section label inside a panel ("Now Playing"): 11 / 600, secondary.
    fn panel_label(label: &'static str, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        div()
            .px(theme.space.l)
            .pt(theme.space.s)
            .pb(theme.space.xs)
            .text_size(theme.text.subhead)
            .font_weight(SEMIBOLD)
            .text_color(theme.colors.text_secondary)
            .child(label)
            .into_any_element()
    }

    /// The queue panel: Playing Next (or the history), the track playing now, Clear and
    /// Autoplay (∞) in the head.
    pub(crate) fn drawer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let show_history = self.settings.history.show_in_up_next;
        let history = show_history && self.history_tab;
        let upcoming = self.player.as_ref().map(|s| up_next(s).len()).unwrap_or(0);
        let current = self.player.as_ref().and_then(|s| s.track.clone());
        let ready = self.connection == rustet_app::engine::Connection::Ready;
        let autoplay = self.player.as_ref().is_some_and(|s| s.autoplay);

        let title = if show_history {
            div()
                .id("drawer-tabs")
                .role(Role::TabList)
                .aria_label("Playing Next or History")
                .flex()
                .gap(theme.space.m)
                .child(self.drawer_tab("tab-up-next", "Playing Next", false, cx))
                .child(self.drawer_tab("tab-history", "History", true, cx))
                .into_any_element()
        } else {
            div().child("Playing Next").into_any_element()
        };
        let mut actions = Vec::new();
        if !history {
            actions.push(
                IconButton::new("queue-autoplay", Icon::Autoplay, "Autoplay")
                    .selected(autoplay)
                    .disabled(!ready)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let mode = Mode {
                            autoplay: Some(!autoplay),
                            ..Mode::default()
                        };
                        this.send(Command::Mode(mode), cx)
                    }))
                    .into_any_element(),
            );
        }
        let header = self.panel_head(title, actions, "Close Playing Next", cx);

        let now = (!history).then_some(current).flatten().map(|t| {
            div()
                .flex()
                .flex_col()
                .child(Self::panel_label("Now Playing", cx))
                .child(
                    div()
                        .px(theme.space.m)
                        .h(theme.size.song_row)
                        .flex()
                        .items_center()
                        .gap(theme.space.m)
                        .child(
                            div().pl(theme.space.s).child(
                                ArtworkView::new(artwork(&t.meta), theme.size.art_row)
                                    .shape(Shape::Small),
                            ),
                        )
                        .child(Self::track_text(&t.meta, cx)),
                )
        });

        let count = if history {
            self.history.len()
        } else {
            upcoming
        };
        let list: AnyElement = if count == 0 {
            let (title, message) = if history {
                ("No history yet", "Songs you play show up here.")
            } else {
                (
                    "Nothing up next",
                    "Play an album or a playlist, or choose Play next on a song.",
                )
            };
            div()
                .p(theme.space.l)
                .child(EmptyStateView::new(&rustet_app::pages::model::EmptyState {
                    title: title.into(),
                    message: message.into(),
                }))
                .into_any_element()
        } else {
            uniform_list(
                if history {
                    "history-list"
                } else {
                    "queue-list"
                },
                count,
                cx.processor(
                    move |this: &mut Shell, range: std::ops::Range<usize>, _, cx| {
                        if history {
                            let entries: Vec<_> = this.history[range.clone()].to_vec();
                            range
                                .zip(entries)
                                .map(|(i, e)| this.history_row(i, &e, cx))
                                .collect::<Vec<_>>()
                        } else {
                            let items: Vec<QueueItem> = this
                                .player
                                .as_ref()
                                .map(|s| up_next(s).get(range.clone()).unwrap_or_default().to_vec())
                                .unwrap_or_default();
                            range
                                .zip(items)
                                .map(|(i, item)| this.queue_row(i, &item, cx))
                                .collect::<Vec<_>>()
                        }
                    },
                ),
            )
            .flex_1()
            .px(theme.space.m)
            .pb(theme.size.player_clearance)
            .into_any_element()
        };

        div()
            .id("drawer")
            .role(Role::Complementary)
            .aria_label(if history { "History" } else { "Playing Next" })
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .children(now)
            // "Next", with Clear at the right.
            .when(!history && upcoming > 0, |d| {
                d.child(
                    div()
                        .flex()
                        .items_end()
                        .justify_between()
                        .pr(theme.space.l)
                        .child(Self::panel_label("Next", cx))
                        .child(Button::new("queue-clear", "Clear").on_click(
                            cx.listener(|this, _, _, cx| this.send(Command::ClearUpcoming, cx)),
                        )),
                )
            })
            .child(list)
            .into_any_element()
    }

    /// The lyrics panel: the lyrics of the track playing, following the position.
    pub(crate) fn lyrics_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let header = self.panel_head(
            div().child("Lyrics").into_any_element(),
            Vec::new(),
            "Close Lyrics",
            cx,
        );
        let lyrics = self.now_playing.update(cx, |n, cx| n.lyrics_panel(cx));
        let theme = cx.theme();
        div()
            .id("lyrics-panel")
            .role(Role::Complementary)
            .aria_label("Lyrics")
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().px(theme.space.l).child(lyrics))
            .into_any_element()
    }
}

/// A row's accessible name: the title, and the artist when the track has one.
fn track_label(meta: &rustet_player::model::TrackMeta) -> SharedString {
    if meta.artist_name.is_empty() {
        meta.name.clone().into()
    } else {
        format!("{}, {}", meta.name, meta.artist_name).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_next_rows_map_to_queue_indices() {
        assert_eq!(
            queue_index(Some(3), 0),
            4,
            "the first row follows the current track"
        );
        assert_eq!(queue_index(Some(3), 2), 6);
        assert_eq!(
            queue_index(None, 2),
            2,
            "nothing current: Up Next is the whole queue"
        );
    }
}
