//! A shelf: a titled horizontal row of tiles, editorial tiles, songs or links.
//!
//! The head is the title (17 / 700) with a chevron when Apple has more; the whole head is then
//! the "see all" link. Previous and next are 36 px glass circles that show over the shelf's
//! edges under the pointer, level with the artwork. Songs are the song grid: columns of
//! 55 px rows (three columns in 1100 px). A shelf of artists shows 120 px circles; a shelf of
//! cards ([`ShelfView::cards`], search "Top Results") shows two rows of 118 px cards.
//!
//! The shelf measures the width it gets and fits whole tiles into it (stretched to fill), so a
//! page shows only what fits; only the shown page's tiles are drawn, so only their artwork
//! loads. The shelf has no side margin of its own: the page gives it the same margin as its
//! title, so the first tile lines up with the page title.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Entity, FocusHandle, IntoElement, ParentElement, Pixels, RenderOnce, Role, SharedString,
    Styled, Window, canvas, div, prelude::*, px,
};
use rustet_app::pages::model::{LinkTarget, Shelf, ShelfItems, Tile, TileKind, TileSize, TrackRow};

use super::artwork::{ArtworkView, Shape};
use super::tile::{MoveLeft, MoveRight, TileView, explicit_mark};
use super::{ContentEvent, OnContent};
use crate::components::{Button, Circle, FocusRing, IconButton};
use crate::icons::Icon;
use crate::theme::{ActiveTheme, BOLD, Theme};

/// Songs per column of a songs shelf.
const SONG_ROWS: usize = 4;

/// Rows of a cards shelf.
const CARD_ROWS: usize = 2;

type OnSeeAll = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct ShelfView {
    shelf: Shelf,
    on_event: Option<OnContent>,
    on_see_all: Option<OnSeeAll>,
    cards: bool,
}

impl ShelfView {
    pub fn new(shelf: Shelf) -> Self {
        Self {
            shelf,
            on_event: None,
            on_see_all: None,
            cards: false,
        }
    }

    /// Tiles as search cards, in two rows of as many columns as fit ("Top Results").
    pub fn cards(mut self) -> Self {
        self.cards = true;
        self
    }

    pub fn on_event(mut self, f: OnContent) -> Self {
        self.on_event = Some(f);
        self
    }

    /// "See all" runs `f` instead of opening the shelf's `see_all` route, and shows even when
    /// the shelf has no route (a search section that expands in place).
    pub fn on_see_all(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_see_all = Some(Rc::new(f));
        self
    }
}

#[derive(Default)]
struct ShelfState {
    page: usize,
    width: Pixels,
    /// The tiles' focus handles (Left and Right move between them, across pages).
    focus: Vec<FocusHandle>,
}

/// Moves the keyboard focus to the next or previous tile, turning the page when it leaves the
/// one shown.
fn step_focus(
    state: &Entity<ShelfState>,
    forward: bool,
    per_page: usize,
    window: &mut Window,
    cx: &mut App,
) {
    let target = {
        let s = state.read(cx);
        let Some(from) = s.focus.iter().position(|h| h.is_focused(window)) else {
            return;
        };
        let to = if forward {
            from + 1
        } else {
            from.wrapping_sub(1)
        };
        let Some(handle) = s.focus.get(to) else {
            return;
        };
        (to, handle.clone())
    };
    state.update(cx, |s, cx| {
        s.page = target.0 / per_page.max(1);
        cx.notify();
    });
    window.focus(&target.1, cx);
}

/// How many items of `min` px fit in `width` with `gap` between them, and their width.
pub fn fit(width: Pixels, min: Pixels, gap: Pixels) -> (usize, Pixels) {
    let n = (((width + gap) / (min + gap)).floor() as usize).max(1);
    let item = ((width - gap * (n as f32 - 1.)) / n as f32).max(px(1.));
    (n, item)
}

impl RenderOnce for ShelfView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let shelf = self.shelf;
        let state = window.use_keyed_state(
            SharedString::from(format!("shelf-{}", shelf.id)),
            cx,
            |_, _| ShelfState::default(),
        );
        let (page, width) = {
            let s = state.read(cx);
            (s.page, s.width)
        };
        let gap = theme.space.l;
        let cards = self.cards;
        let (min, columns_per_item) = match &shelf.items {
            ShelfItems::Tiles { .. } if cards => (theme.size.song_column_min, CARD_ROWS),
            ShelfItems::Tiles {
                size: TileSize::Large,
                ..
            }
            | ShelfItems::Featured(_) => (theme.size.tile_large_min, 1),
            ShelfItems::Tracks(_) => (theme.size.song_column_min, SONG_ROWS),
            ShelfItems::Tiles { tiles, .. } if tiles.iter().all(|t| t.kind == TileKind::Artist) => {
                (theme.size.art_artist, 1)
            }
            ShelfItems::Tiles { .. } => (theme.size.tile_min, 1),
            ShelfItems::Links(_) => (px(0.), 1),
        };
        let (per_page, item_width) = fit(width.max(min), min, gap);
        let per_page_items = per_page * columns_per_item;
        let len = shelf.items.len();
        let pages = len.div_ceil(per_page_items.max(1)).max(1);
        let page = page.min(pages - 1);
        let range = page * per_page_items..((page + 1) * per_page_items).min(len);
        let on_event = self.on_event;
        let tile_count = match &shelf.items {
            ShelfItems::Tiles { tiles, .. } | ShelfItems::Featured(tiles) => tiles.len(),
            _ => 0,
        };
        if state.read(cx).focus.len() != tile_count {
            state.update(cx, |s, cx| {
                s.focus.resize_with(tile_count, || cx.focus_handle())
            });
        }
        let focus = state.read(cx).focus.clone();
        // One tab stop for the tiles shown; Left and Right move inside the shelf.
        if let Some(shown) = focus.get(range.clone()) {
            crate::focus::rove(shown, window);
        }
        let tile_view = |tiles: &[Tile], ix: usize, large: bool| {
            let mut tile =
                TileView::new(tiles[ix].clone(), ix, item_width).focus_handle(focus[ix].clone());
            if large {
                tile = tile.large();
            }
            if cards {
                tile = tile.card();
            }
            if let Some(f) = &on_event {
                tile = tile.on_event(f.clone());
            }
            tile
        };
        // Tiles in one row, or cards in rows of `per_page`.
        let tiles_row = |tiles: &[Tile], large: bool| {
            let (back, forward) = (state.clone(), state.clone());
            div()
                .on_action(move |_: &MoveLeft, w, cx| {
                    step_focus(&back, false, per_page_items, w, cx)
                })
                .on_action(move |_: &MoveRight, w, cx| {
                    step_focus(&forward, true, per_page_items, w, cx)
                })
                .flex()
                .flex_col()
                .gap(gap)
                .children(range.clone().step_by(per_page).map(|start| {
                    let end = (start + per_page).min(range.end);
                    div()
                        .flex()
                        .gap(gap)
                        .children((start..end).map(|ix| tile_view(tiles, ix, large)))
                }))
                .into_any_element()
        };

        // The paddles centre on the artwork (or on the song grid).
        let (body, art_height) = match &shelf.items {
            ShelfItems::Tiles { tiles, size } => {
                let large = *size == TileSize::Large;
                let art = if cards {
                    let rows = range.len().div_ceil(per_page).max(1) as f32;
                    theme.size.card * rows + gap * (rows - 1.)
                } else if large {
                    item_width * 9. / 16.
                } else {
                    item_width
                };
                (tiles_row(tiles, large), art)
            }
            ShelfItems::Featured(tiles) => (tiles_row(tiles, true), item_width * 9. / 16.),
            ShelfItems::Tracks(rows) => {
                let all: Arc<[TrackRow]> = rows.clone().into();
                let body = div()
                    .flex()
                    .gap(gap)
                    .children(range.clone().step_by(SONG_ROWS).map(|start| {
                        let end = (start + SONG_ROWS).min(range.end);
                        div().flex().flex_col().w(item_width).children(
                            (start..end)
                                .map(|ix| song_row(&theme, all.clone(), ix, on_event.clone(), cx)),
                        )
                    }))
                    .into_any_element();
                let shown = range.len().min(SONG_ROWS) as f32;
                (body, theme.size.song_row * shown)
            }
            ShelfItems::Links(links) => (
                div()
                    .flex()
                    .flex_wrap()
                    .gap(theme.space.s)
                    .children(links.iter().enumerate().map(|(i, link)| {
                        let target = link.target.clone();
                        let on_event = on_event.clone();
                        Button::new(("link", i), link.label.clone()).on_click(move |_, w, cx| {
                            let event = match &target {
                                LinkTarget::Route(r) => ContentEvent::Navigate(r.clone()),
                                LinkTarget::External(url) => {
                                    ContentEvent::OpenExternal(url.clone())
                                }
                            };
                            if let Some(f) = &on_event {
                                f(&event, w, cx);
                            }
                        })
                    }))
                    .into_any_element(),
                px(0.),
            ),
        };

        let paged = pages > 1 && !matches!(shelf.items, ShelfItems::Links(_));
        let see_all: Option<OnSeeAll> = match (self.on_see_all, shelf.see_all.clone()) {
            (Some(f), _) => Some(f),
            (None, Some(route)) => {
                let on_event = on_event.clone();
                Some(Rc::new(move |w: &mut Window, cx: &mut App| {
                    if let Some(f) = &on_event {
                        f(&ContentEvent::Navigate(route.clone()), w, cx);
                    }
                }))
            }
            (None, None) => None,
        };
        let title: SharedString = shelf.title.clone().unwrap_or_default().into();
        let measure = state.clone();
        // A glass circle over the shelf's edge, centred in the page's side gutter, level with
        // the artwork's middle; it shows under the pointer.
        let paddle_top = (art_height - theme.size.circle) / 2.;
        let paddle_out = -(theme.space.xl + theme.size.circle) / 2.;
        let paddle = |id: &'static str, icon: Icon, label: &'static str, forward: bool| {
            let state = state.clone();
            div()
                .absolute()
                .top(paddle_top)
                .map(|d| {
                    if forward {
                        d.right(paddle_out)
                    } else {
                        d.left(paddle_out)
                    }
                })
                .invisible()
                .group_hover("shelf", |s| s.visible())
                .child(
                    IconButton::new(id, icon, label)
                        .glass(Circle::Regular)
                        .on_click(move |_, _, cx| {
                            state.update(cx, |s, cx| {
                                s.page = if forward {
                                    (s.page + 1).min(pages - 1)
                                } else {
                                    s.page.saturating_sub(1)
                                };
                                cx.notify();
                            })
                        }),
                )
        };
        div()
            .id(SharedString::from(format!("shelf-{}", shelf.id)))
            .role(Role::Group)
            .aria_label(title.clone())
            .group("shelf")
            .relative()
            .flex()
            .flex_col()
            .gap(theme.space.m)
            .w_full()
            .when(shelf.title.is_some(), |d| {
                d.child(head(&theme, title.clone(), see_all, cx))
            })
            .child(
                div()
                    .relative()
                    .child(body)
                    .when(paged && page > 0, |d| {
                        d.child(paddle(
                            "previous-page",
                            Icon::ChevronLeft,
                            "Previous page",
                            false,
                        ))
                    })
                    .when(paged && page + 1 < pages, |d| {
                        d.child(paddle("next-page", Icon::ChevronRight, "Next page", true))
                    }),
            )
            // Measures the width the shelf gets, to fit whole tiles.
            .child(
                canvas(
                    move |bounds, window, cx| {
                        measure.update(cx, |s, cx| {
                            if s.width != bounds.size.width {
                                s.width = bounds.size.width;
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
                .top_0()
                .left_0()
                .w_full()
                .h(theme.size.hairline),
            )
    }
}

/// The shelf's head: its title (17 / 700) and, when there is more, a chevron; the whole head
/// is then the "see all" link.
fn head(
    theme: &Theme,
    title: SharedString,
    see_all: Option<OnSeeAll>,
    cx: &App,
) -> impl IntoElement {
    let colors = theme.colors;
    let linked = see_all.is_some();
    div()
        .id("shelf-head")
        .flex()
        .flex_none()
        .self_start()
        .items_center()
        .gap(theme.space.xs)
        .text_size(theme.text.title2)
        .font_weight(BOLD)
        .text_color(colors.text)
        .child(div().truncate().child(title.clone()))
        .when_some(see_all, |d, f| {
            d.role(Role::Link)
                .aria_label(SharedString::from(format!("{title}, see all")))
                .tab_index(0)
                .focus_ring(cx)
                .rounded(theme.radius.small)
                .cursor_pointer()
                .on_click(move |_, w, cx| f(w, cx))
                .child(
                    Icon::ChevronRight
                        .element()
                        .size(theme.size.icon_small)
                        .text_color(colors.text_secondary),
                )
        })
        .when(!linked, |d| d.role(Role::Heading).aria_level(2))
}

/// A row of the song grid: a hairline above, 40 px artwork, title, artist, and a "more"
/// glyph at the right. Under the pointer the row fills and a play glyph covers the artwork.
fn song_row(
    theme: &Theme,
    rows: Arc<[TrackRow]>,
    ix: usize,
    on_event: Option<OnContent>,
    cx: &App,
) -> impl IntoElement {
    let row = rows[ix].clone();
    let colors = theme.colors;
    let label: SharedString = format!("{}, {}", row.title, row.artist.text).into();
    let (play, menu) = (on_event.clone(), on_event);
    let menu_row = row.clone();
    let group: SharedString = format!("song-{ix}").into();
    div()
        .id(("song", ix))
        .role(Role::ListItem)
        .aria_label(label)
        .group(group.clone())
        .tab_index(0)
        .focus_ring(cx)
        .flex()
        .flex_col()
        .h(theme.size.song_row)
        .cursor_pointer()
        .on_click(move |_, w, cx| {
            if let Some(f) = &play {
                f(
                    &ContentEvent::PlayTracks {
                        rows: rows.clone(),
                        start: ix,
                    },
                    w,
                    cx,
                );
            }
        })
        .child(div().h(theme.size.hairline).bg(colors.divider))
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .gap(theme.space.m)
                .rounded(theme.radius.medium)
                .group_hover(group.clone(), |s| s.bg(colors.hover))
                .child(
                    div()
                        .relative()
                        .child(
                            ArtworkView::new(row.artwork.clone(), theme.size.art_row)
                                .shape(Shape::Small),
                        )
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(theme.radius.small)
                                .bg(colors.artwork_dim)
                                .invisible()
                                .group_hover(group, |s| s.visible())
                                .child(
                                    Icon::Play
                                        .element()
                                        .size(theme.size.icon_small)
                                        .text_color(colors.switch_knob),
                                ),
                        ),
                )
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
                                .gap(theme.space.xs)
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(theme.text.body)
                                        .text_color(colors.text)
                                        .child(row.title.clone()),
                                )
                                .when(row.explicit, |d| d.child(explicit_mark(theme))),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(theme.text.callout)
                                .text_color(colors.text_secondary)
                                .child(row.artist.text.clone()),
                        ),
                )
                .child(
                    div()
                        .id("song-more")
                        .role(Role::Button)
                        .aria_label("More")
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(theme.size.control)
                        .rounded_full()
                        .hover(|s| s.bg(colors.hover))
                        .on_click(move |e, w, cx| {
                            cx.stop_propagation();
                            if let Some(f) = &menu {
                                f(
                                    &ContentEvent::TracksMenu {
                                        rows: vec![menu_row.clone()],
                                        position: e.position(),
                                    },
                                    w,
                                    cx,
                                );
                            }
                        })
                        .child(
                            Icon::More
                                .element()
                                .size(theme.size.icon_small)
                                .text_color(colors.text_secondary),
                        ),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_tiles_fill_the_width() {
        assert_eq!(fit(px(1000.), px(168.), px(20.)), (5, px(184.)));
        assert_eq!(fit(px(100.), px(168.), px(20.)), (1, px(100.)));
    }

    /// The brief's widths: in 1100 px, five album tiles, two editorial tiles, three song-grid
    /// columns, eight artist circles of 120 px.
    #[test]
    fn the_content_width_holds_the_briefs_columns() {
        let theme = crate::theme::Theme::new(crate::theme::Appearance::Dark);
        let (s, gap) = (theme.size, theme.space.l);
        let width = px(1100.);
        assert_eq!(fit(width, s.tile_min, gap).0, 5);
        assert_eq!(fit(width, s.tile_large_min, gap).0, 2);
        assert_eq!(fit(width, s.song_column_min, gap).0, 3);
        assert_eq!(fit(width, s.art_artist, gap), (8, px(120.)));
        assert_eq!(fit(px(800.), s.song_column_min, gap).0, 2);
        assert_eq!(fit(px(600.), s.song_column_min, gap).0, 1);
    }
}
