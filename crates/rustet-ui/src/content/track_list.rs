//! The virtualised track list (album, playlist, library songs, search songs, charts, queue).
//!
//! Rows are 46 px with a hairline above each, from the number column: number (or artwork),
//! title with the explicit badge and the quality marks (neutral, text secondary), the artist
//! under it when it differs from the album's, album (when wide enough), duration in tabular
//! figures, and a "more" glyph. The playing row shows moving bars; under the pointer a play
//! glyph takes the number's place. Unavailable rows are dimmed.
//!
//! Mouse: click selects, Ctrl+click adds or removes, Shift+click selects a range, double click
//! plays, right click opens the menu, drag carries the selection ([`DraggedTracks`]).
//! Keyboard (Tab into the list): Up and Down move (Shift extends), Home and End, Ctrl+A,
//! Enter plays, the Menu key or Shift+F10 opens the menu, Escape clears the selection.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, Modifiers,
    MouseButton, Pixels, Point, Role, ScrollStrategy, SharedString, UniformListScrollHandle,
    Window, actions, canvas, div, point, prelude::*, px, uniform_list,
};
use rustet_app::pages::model::{Availability, Lossless, TrackId, TrackRow};
use rustet_app::route::Route;

use super::artwork::{ArtworkView, Shape};
use super::quality::quality_mark;
use super::tile::explicit_mark;
use super::{ContentEvent, OnContent};
use crate::components::{Glass, Scrollbar};
use crate::format::duration as format_duration;
use crate::icons::Icon;
use crate::theme::{ActiveTheme, Theme};

actions!(
    track_list,
    [
        Down,
        Up,
        ExtendDown,
        ExtendUp,
        First,
        Last,
        SelectAll,
        Play,
        Menu,
        ClearSelection
    ]
);

pub(crate) const CONTEXT: &str = "TrackList";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("shift-down", ExtendDown, Some(CONTEXT)),
        KeyBinding::new("shift-up", ExtendUp, Some(CONTEXT)),
        KeyBinding::new("home", First, Some(CONTEXT)),
        KeyBinding::new("end", Last, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("enter", Play, Some(CONTEXT)),
        KeyBinding::new("menu", Menu, Some(CONTEXT)),
        KeyBinding::new("shift-f10", Menu, Some(CONTEXT)),
        KeyBinding::new("escape", ClearSelection, Some(CONTEXT)),
    ]);
}

/// What a list shows.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackListOptions {
    /// Names the list for screen readers ("Songs").
    pub label: SharedString,
    /// Artwork in the first column (playlists, search); `false` shows track numbers (albums).
    pub artwork: bool,
    /// The album column (playlists, library songs), shown when the list is wide enough.
    pub album: bool,
    /// As tall as its rows, inside a page that scrolls (an album's tracks); otherwise the list
    /// fills its parent and scrolls itself (library songs).
    pub fit_content: bool,
    /// The album's artist: rows by this artist show no artist line (an album's tracks).
    pub album_artist: Option<SharedString>,
}

impl Default for TrackListOptions {
    fn default() -> Self {
        Self {
            label: "Songs".into(),
            artwork: true,
            album: true,
            fit_content: false,
            album_artist: None,
        }
    }
}

/// The drag payload: the rows being dragged (to the queue or a playlist).
#[derive(Debug, Clone)]
pub struct DraggedTracks {
    pub rows: Vec<TrackRow>,
}

struct DragPreview(SharedString);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px(theme.space.m)
            .py(theme.space.xs)
            .rounded_full()
            .glass(cx)
            .text_size(theme.text.body)
            .text_color(theme.colors.text)
            .child(self.0.clone())
    }
}

pub struct TrackListView {
    rows: Arc<[TrackRow]>,
    options: TrackListOptions,
    now_playing: Option<TrackId>,
    selection: BTreeSet<usize>,
    /// The keyboard position and the anchor of Shift selections.
    cursor: Option<usize>,
    anchor: Option<usize>,
    focus_handle: FocusHandle,
    width: Pixels,
    pub scroll: UniformListScrollHandle,
}

impl EventEmitter<ContentEvent> for TrackListView {}

impl TrackListView {
    pub fn new(rows: Arc<[TrackRow]>, options: TrackListOptions, cx: &mut Context<Self>) -> Self {
        Self {
            rows,
            options,
            now_playing: None,
            selection: BTreeSet::new(),
            cursor: None,
            anchor: None,
            // One tab stop; Up and Down move inside.
            focus_handle: cx.focus_handle().tab_stop(true),
            width: px(0.),
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Replaces the rows. When the new rows only add to the end of the old ones ("load
    /// more"), the selection, cursor and anchor stay; otherwise they clear.
    pub fn set_rows(&mut self, rows: Arc<[TrackRow]>, cx: &mut Context<Self>) {
        let appended = rows.len() >= self.rows.len()
            && rows
                .iter()
                .zip(self.rows.iter())
                .all(|(new, old)| new.id == old.id);
        self.rows = rows;
        if !appended {
            self.selection.clear();
            self.cursor = None;
            self.anchor = None;
        }
        cx.notify();
    }

    /// The index of the playing row ([`Self::set_now_playing`]), if it is in the list.
    pub fn now_playing_row(&self) -> Option<usize> {
        let id = self.now_playing.as_ref()?;
        self.rows.iter().position(|r| &r.id == id)
    }

    pub fn rows(&self) -> &Arc<[TrackRow]> {
        &self.rows
    }

    /// Marks the playing track (the player's current item).
    pub fn set_now_playing(&mut self, id: Option<TrackId>, cx: &mut Context<Self>) {
        self.now_playing = id;
        cx.notify();
    }

    pub fn selection(&self) -> Vec<usize> {
        self.selection.iter().copied().collect()
    }

    /// Selects one row and scrolls it into view (as a click would).
    pub fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.rows.len() {
            self.selection = BTreeSet::from([ix]);
            self.cursor = Some(ix);
            self.anchor = Some(ix);
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn move_cursor(&mut self, to: usize, extend: bool, cx: &mut Context<Self>) {
        let to = to.min(self.rows.len().saturating_sub(1));
        if self.rows.is_empty() {
            return;
        }
        if extend {
            let anchor = *self.anchor.get_or_insert(self.cursor.unwrap_or(to));
            self.selection = (anchor.min(to)..=anchor.max(to)).collect();
            self.cursor = Some(to);
            self.scroll.scroll_to_item(to, ScrollStrategy::Nearest);
            cx.notify();
        } else {
            self.select(to, cx);
        }
    }

    fn click(&mut self, ix: usize, e: &ClickEvent, cx: &mut Context<Self>) {
        let m: Modifiers = e.modifiers();
        if m.shift {
            let anchor = self.anchor.unwrap_or(ix);
            self.selection = (anchor.min(ix)..=anchor.max(ix)).collect();
        } else if m.control || m.platform {
            if !self.selection.remove(&ix) {
                self.selection.insert(ix);
            }
            self.anchor = Some(ix);
        } else {
            self.selection = BTreeSet::from([ix]);
            self.anchor = Some(ix);
        }
        self.cursor = Some(ix);
        if e.click_count() == 2 {
            self.play(ix, cx);
        }
        cx.notify();
    }

    fn play(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.rows.get(ix).is_some_and(TrackRow::is_playable) {
            cx.emit(ContentEvent::PlayTracks {
                rows: self.rows.clone(),
                start: ix,
            });
        }
    }

    /// The rows a menu or a drag acts on: the selection when `ix` is in it, else `ix` alone.
    fn targets(&self, ix: usize) -> Vec<TrackRow> {
        if self.selection.contains(&ix) {
            self.selection
                .iter()
                .map(|&i| self.rows[i].clone())
                .collect()
        } else {
            vec![self.rows[ix].clone()]
        }
    }

    fn open_menu(&mut self, ix: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selection.contains(&ix) {
            self.selection = BTreeSet::from([ix]);
            self.cursor = Some(ix);
            self.anchor = Some(ix);
        }
        cx.emit(ContentEvent::TracksMenu {
            rows: self.targets(ix),
            position,
        });
        cx.notify();
    }
}

/// "Hi-Res Lossless", "Lossless"; nothing for lossy.
fn lossless_label(l: Lossless) -> &'static str {
    match l {
        Lossless::HiRes => "Hi-Res Lossless",
        Lossless::Lossless => "Lossless",
    }
}

fn row_label(row: &TrackRow) -> SharedString {
    let mut s = format!("{}, {}", row.title, row.artist.text);
    if let Some(d) = row.duration {
        s.push_str(", ");
        s.push_str(&format_duration(d));
    }
    if row.explicit {
        s.push_str(", explicit");
    }
    if let Some(l) = row.quality().lossless {
        s.push_str(", ");
        s.push_str(lossless_label(l));
    }
    if row.availability == Availability::Unavailable {
        s.push_str(", not available");
    }
    s.into()
}

/// A link inside a row (artist, album): opens its route and does not select the row.
fn linked(
    theme: &Theme,
    id: &'static str,
    text: String,
    route: Option<Route>,
    on_event: OnContent,
) -> impl IntoElement {
    let colors = theme.colors;
    div()
        .id(id)
        .min_w_0()
        .truncate()
        .text_size(theme.text.body)
        .text_color(colors.text_secondary)
        .when_some(route, |d, route| {
            d.cursor_pointer()
                .hover(|s| s.text_color(colors.text).underline())
                .on_click(move |e, w, cx| {
                    if e.click_count() == 1 {
                        cx.stop_propagation();
                        on_event(&ContentEvent::Navigate(route.clone()), w, cx);
                    }
                })
        })
        .child(text)
}

impl Render for TrackListView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let side = theme.space.xl;
        let clearance = theme.size.player_clearance;
        let wide = self.options.album && self.width > theme.size.album_column_min;
        let keyboard = self.focus_handle.is_focused(window) && window.last_input_was_keyboard();
        let total = self.rows.len();
        let fit = self.options.fit_content;
        let entity = cx.entity();
        let on_event: OnContent = {
            let entity = entity.clone();
            Rc::new(move |e, _, cx| entity.update(cx, |_, cx| cx.emit(e.clone())))
        };
        div()
            .id("track-list")
            .role(Role::List)
            .aria_label(self.options.label.clone())
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .tab_index(0)
            .on_action(cx.listener(|this, _: &Down, _, cx| {
                this.move_cursor(this.cursor.map_or(0, |c| c + 1), false, cx)
            }))
            .on_action(cx.listener(|this, _: &Up, _, cx| {
                this.move_cursor(this.cursor.map_or(0, |c| c.saturating_sub(1)), false, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendDown, _, cx| {
                this.move_cursor(this.cursor.map_or(0, |c| c + 1), true, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendUp, _, cx| {
                this.move_cursor(this.cursor.map_or(0, |c| c.saturating_sub(1)), true, cx)
            }))
            .on_action(cx.listener(|this, _: &First, _, cx| this.move_cursor(0, false, cx)))
            .on_action(cx.listener(|this, _: &Last, _, cx| this.move_cursor(usize::MAX, false, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.selection = (0..this.rows.len()).collect();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Play, _, cx| {
                if let Some(ix) = this.cursor {
                    this.play(ix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Menu, _, cx| {
                if let Some(ix) = this.cursor {
                    // From the keyboard there is no pointer: open near the list's top left.
                    let inset = cx.theme().space.xl;
                    let at =
                        this.scroll.0.borrow().base_handle.bounds().origin + point(inset, inset);
                    this.open_menu(ix, at, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                if this.selection.is_empty() {
                    cx.propagate();
                } else {
                    this.selection.clear();
                    cx.notify();
                }
            }))
            .relative()
            .w_full()
            .map(|d| {
                if fit {
                    d.h(theme.size.row * total as f32)
                } else {
                    d.h_full()
                }
            })
            .child(
                uniform_list(
                    "track-rows",
                    total,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|ix| {
                                this.row(ix, wide, keyboard, total, &theme, on_event.clone(), cx)
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.scroll)
                .px(side)
                .when(!fit, |d| d.pb(clearance))
                .size_full(),
            )
            .when(!fit, |d| {
                d.child(Scrollbar::new(
                    "track-list-scrollbar",
                    self.scroll.0.borrow().base_handle.clone(),
                ))
            })
            .child(
                canvas(
                    move |bounds, window, cx| {
                        entity.update(cx, |this, cx| {
                            if this.width != bounds.size.width {
                                this.width = bounds.size.width;
                                cx.notify();
                                // Drawn with the old width: draw again now (a notify
                                // during a paint schedules no frame).
                                window.request_animation_frame();
                            }
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}

impl TrackListView {
    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        ix: usize,
        wide: bool,
        keyboard: bool,
        total: usize,
        theme: &Theme,
        on_event: OnContent,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = theme.colors;
        let row = &self.rows[ix];
        let selected = self.selection.contains(&ix);
        let is_cursor = self.cursor == Some(ix);
        let playing = self.now_playing.as_ref() == Some(&row.id);
        let quality = row.quality();
        let group: SharedString = format!("track-{ix}").into();
        let glyph = |icon: Icon, color| {
            icon.element()
                .size(theme.size.icon_small)
                .text_color(color)
                .into_any_element()
        };
        // The lead cell: the playing mark, else the artwork or the number; under the pointer
        // a play glyph takes the number's place.
        let lead_cell = if self.options.artwork {
            div()
                .relative()
                .child(
                    ArtworkView::new(row.artwork.clone(), theme.size.art_row).shape(Shape::Small),
                )
                .when(playing, |d| {
                    d.child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme.radius.small)
                            .bg(colors.artwork_dim)
                            .child(glyph(Icon::NowPlaying, colors.switch_knob)),
                    )
                })
                .into_any_element()
        } else {
            div()
                .w(theme.size.number_column)
                .flex()
                .justify_center()
                .text_size(theme.text.body)
                .font_features(theme.tabular())
                .text_color(colors.text_secondary)
                .map(|d| {
                    if playing {
                        d.child(glyph(Icon::NowPlaying, colors.text))
                    } else {
                        d.child(
                            div()
                                .group_hover(group.clone(), |s| s.invisible())
                                .child(row.number.map(|n| n.to_string()).unwrap_or_default()),
                        )
                        .child(
                            div()
                                .absolute()
                                .invisible()
                                .group_hover(group.clone(), |s| s.visible())
                                .child(glyph(Icon::Play, colors.text)),
                        )
                    }
                })
                .relative()
                .into_any_element()
        };
        // The artist line shows only when it differs from the album's artist.
        let artist =
            (self.options.album_artist.as_deref() != Some(row.artist.text.as_str())).then(|| {
                linked(
                    theme,
                    "artist",
                    row.artist.text.clone(),
                    row.artist.route.clone(),
                    on_event.clone(),
                )
            });
        let drag = DraggedTracks {
            rows: self.targets(ix),
        };
        let drag_label: SharedString = if drag.rows.len() == 1 {
            drag.rows[0].title.clone().into()
        } else {
            format!("{} songs", drag.rows.len()).into()
        };
        div()
            .id(ix)
            .role(Role::ListItem)
            .aria_label(row_label(row))
            .aria_selected(selected)
            .aria_position_in_set(ix + 1)
            .aria_size_of_set(total)
            .when(is_cursor, |d| d.aria_active_descendant())
            .group(group)
            .relative()
            .w_full()
            .h(theme.size.row)
            .flex()
            .items_center()
            .gap(theme.space.m)
            .px(theme.space.s)
            .rounded(theme.radius.medium)
            .when(selected, |d| d.bg(colors.fill))
            .when(!selected, |d| d.hover(|s| s.bg(colors.hover)))
            .when(is_cursor && keyboard, |d| {
                d.shadow(theme.focus_ring(theme.base()))
            })
            .when(row.availability == Availability::Unavailable, |d| {
                d.opacity(0.45)
            })
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.click(ix, e, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                    this.open_menu(ix, e.position, cx);
                }),
            )
            .on_drag(drag, move |_, _, _, cx| {
                cx.new(|_| DragPreview(drag_label.clone()))
            })
            // The hairline above the row, from the number column to the right edge.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left(theme.space.s)
                    .right_0()
                    .h(theme.size.hairline)
                    .bg(colors.divider),
            )
            .child(lead_cell)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.space.s6)
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.text.body)
                                    .text_color(colors.text)
                                    .child(row.title.clone()),
                            )
                            .when(row.explicit, |d| d.child(explicit_mark(theme)))
                            .children(
                                quality
                                    .lossless
                                    .map(|l| quality_mark(theme, lossless_label(l))),
                            )
                            .when(quality.dolby_atmos, |d| {
                                d.child(quality_mark(theme, "Dolby Atmos"))
                            }),
                    )
                    .children(artist),
            )
            .when(wide, |d| {
                d.child(
                    div().w(gpui::relative(0.3)).flex_none().min_w_0().children(
                        row.album
                            .clone()
                            .map(|a| linked(theme, "album", a.text, a.route, on_event.clone())),
                    ),
                )
            })
            .child(
                div()
                    .w(theme.size.duration_column)
                    .flex_none()
                    .text_right()
                    .text_size(theme.text.body)
                    .font_features(theme.tabular())
                    .text_color(colors.text_secondary)
                    .child(row.duration.map(format_duration).unwrap_or_default()),
            )
            .child(
                div()
                    .id("more")
                    .role(Role::Button)
                    .aria_label("More")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(theme.size.control)
                    .rounded_full()
                    .hover(|s| s.bg(colors.hover))
                    .on_click(cx.listener(move |this, e: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.open_menu(ix, e.position(), cx);
                    }))
                    .child(glyph(Icon::More, colors.text_secondary)),
            )
            .into_any_element()
    }
}

impl Focusable for TrackListView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
