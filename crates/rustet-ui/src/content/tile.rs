//! A tile: artwork, title, subtitle; every [`TileKind`] (album, playlist, station, artist,
//! video, song, curator, label, category), regular, large (editorial) or a card (search).
//!
//! The artwork has 7 px corners and an inset hairline; below it, 6 px down, the title and
//! subtitle (12 px, one line each). An editorial tile puts an uppercase eyebrow, its title and
//! subtitle (17 px) above a wide picture. A card ("Top Results") is a 118 px fill-coloured
//! box with a hairline: 90 px artwork (a circle for artists), then the title (15) and the
//! kind and subtitle (12, "Song · Paramore"). Under the pointer the artwork dims and shows a
//! play circle (lower left) and a "more" circle (lower right; a card's sits at its right).
//!
//! Click or Enter opens its route; the play circle plays it; "more", right click, the Menu
//! key or Shift+F10 opens its context menu. The arrow keys are
//! actions ([`MoveLeft`] …) that the tile leaves to its container: [`super::TileGrid`] and
//! [`super::ShelfView`] move the focus to the next tile.

use gpui::{
    App, FocusHandle, IntoElement, KeyBinding, ParentElement, Pixels, Point, RenderOnce, Role,
    SharedString, Styled, Window, actions, canvas, div, point, prelude::*,
};
use rustet_app::pages::model::{Tile, TileKind};

use super::artwork::{ArtworkView, Shape, shaped};
use super::{ContentEvent, OnContent};
use crate::components::{Circle, FocusRing, IconButton};
use crate::icons::Icon;
use crate::theme::{ActiveTheme, BOLD, SEMIBOLD, Theme};

actions!(tile, [OpenMenu, MoveLeft, MoveRight, MoveUp, MoveDown]);

pub(crate) const CONTEXT: &str = "Tile";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("menu", OpenMenu, Some(CONTEXT)),
        KeyBinding::new("shift-f10", OpenMenu, Some(CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(CONTEXT)),
        KeyBinding::new("up", MoveUp, Some(CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(CONTEXT)),
    ]);
}

#[derive(IntoElement)]
pub struct TileView {
    tile: Tile,
    index: usize,
    width: Pixels,
    form: Form,
    focus: Option<FocusHandle>,
    on_event: Option<OnContent>,
}

/// How a tile is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Regular,
    /// Editorial: text above a wide picture.
    Large,
    /// A search card: artwork and text side by side in a box.
    Card,
}

impl TileView {
    /// `index` keeps sibling tiles' ids apart; `width` is the artwork's width (a card's own
    /// width).
    pub fn new(tile: Tile, index: usize, width: Pixels) -> Self {
        Self {
            tile,
            index,
            width,
            form: Form::Regular,
            focus: None,
            on_event: None,
        }
    }

    /// The tile's focus handle, so its container can move the focus to it (arrow keys).
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// An editorial tile: Apple's label, title and subtitle above a wide picture.
    pub fn large(mut self) -> Self {
        self.form = Form::Large;
        self
    }

    /// A search card ("Top Results").
    pub fn card(mut self) -> Self {
        self.form = Form::Card;
        self
    }

    pub fn on_event(mut self, f: OnContent) -> Self {
        self.on_event = Some(f);
        self
    }
}

/// The explicit badge after a title: a small "E" in the base colour on text secondary.
pub fn explicit_mark(theme: &Theme) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .h(theme.size.badge)
        .min_w(theme.size.badge)
        .rounded(theme.radius.small)
        .bg(theme.colors.text_secondary)
        .text_size(theme.text.footnote)
        .line_height(theme.size.badge)
        .font_weight(BOLD)
        .text_color(theme.colors.base)
        .child("E")
}

fn kind_name(kind: TileKind) -> &'static str {
    match kind {
        TileKind::Album => "Album",
        TileKind::Playlist => "Playlist",
        TileKind::Station { live: true } => "Live radio",
        TileKind::Station { live: false } => "Station",
        TileKind::Artist => "Artist",
        TileKind::MusicVideo => "Music video",
        TileKind::Song => "Song",
        TileKind::Curator => "Curator",
        TileKind::RecordLabel => "Record label",
        TileKind::Category => "Category",
    }
}

impl RenderOnce for TileView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = theme.colors;
        let tile = self.tile;
        let form = self.form;
        let large = form == Form::Large;
        let card = form == Form::Card;
        let artist = tile.kind == TileKind::Artist;
        let (aspect, shape) = match (tile.kind, form) {
            (TileKind::Artist, _) => (1., Shape::Round),
            (_, Form::Large) => (16. / 9., Shape::Editorial),
            (_, Form::Card) => (1., Shape::Small),
            (TileKind::MusicVideo, _) => (16. / 9., Shape::Rounded),
            _ => (1., Shape::Rounded),
        };
        let art_width = if card {
            theme.size.art_card
        } else {
            self.width
        };
        let name: SharedString = match &tile.subtitle {
            Some(sub) => format!("{}, {}, {}", tile.title, kind_name(tile.kind), sub).into(),
            None => format!("{}, {}", tile.title, kind_name(tile.kind)).into(),
        };
        let on_event = self.on_event;
        let route = tile.route.clone();
        let menu_tile = tile.clone();
        let key_tile = tile.clone();
        let more_tile = tile.clone();
        let (open, menu, key_menu, more_menu) = (
            on_event.clone(),
            on_event.clone(),
            on_event.clone(),
            on_event.clone(),
        );
        // Where a menu opened from the keyboard appears: the tile's lower left, measured while
        // painting.
        let anchor = window.use_keyed_state(("tile-anchor", self.index), cx, |_, _| {
            Point::<Pixels>::default()
        });
        let measure = anchor.clone();
        // Shown on hover: play over the artwork's lower left, "more" at the lower right (a
        // card's at its right edge).
        let hover_button = |button: IconButton| {
            div()
                .invisible()
                .group_hover("tile", |s| s.visible())
                .child(button)
        };
        let play = tile.play.clone().filter(|_| !large).map(|target| {
            let on_event = on_event.clone();
            hover_button(
                IconButton::new("play", Icon::Play, format!("Play {}", tile.title))
                    .glass(Circle::Small)
                    .on_click(move |_, w, cx| {
                        cx.stop_propagation();
                        if let Some(f) = &on_event {
                            f(&ContentEvent::Play(target.clone()), w, cx);
                        }
                    }),
            )
        });
        let more = (!large && !(artist && !card)).then(|| {
            hover_button(
                IconButton::new("more", Icon::More, "More")
                    .glass(Circle::Small)
                    .on_click(move |e, w, cx| {
                        cx.stop_propagation();
                        if let Some(f) = &more_menu {
                            f(
                                &ContentEvent::TileMenu {
                                    tile: Box::new(more_tile.clone()),
                                    position: e.position(),
                                },
                                w,
                                cx,
                            );
                        }
                    }),
            )
        });
        let (art_more, card_more) = if card { (None, more) } else { (more, None) };
        let artwork = div()
            .relative()
            .flex_none()
            .child(
                ArtworkView::new(tile.artwork.clone(), art_width)
                    .aspect(aspect)
                    .shape(shape),
            )
            // The artwork dims under the pointer.
            .child(
                shaped(div().absolute().inset_0(), shape, &theme)
                    .group_hover("tile", |s| s.bg(colors.artwork_dim)),
            )
            .when(play.is_some() || art_more.is_some(), |d| {
                d.child(
                    div()
                        .absolute()
                        .left(theme.space.s)
                        .right(theme.space.s)
                        .bottom(theme.space.s)
                        .flex()
                        .justify_between()
                        .child(div().children(play))
                        .children(art_more),
                )
            })
            .child(
                canvas(
                    move |bounds, _, cx| {
                        measure.update(cx, |p, _| {
                            *p = bounds.origin
                                + point(bounds.size.width / 4., bounds.size.height / 2.)
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );
        let (title_size, sub_size) = match form {
            Form::Regular => (theme.text.callout, theme.text.callout),
            Form::Large => (theme.text.title2, theme.text.title2),
            Form::Card => (theme.text.title3, theme.text.callout),
        };
        let title_line = div()
            .flex()
            .items_center()
            .gap(theme.space.xs)
            .min_w_0()
            .max_w_full()
            .child(
                div()
                    .truncate()
                    .text_size(title_size)
                    .text_color(colors.text)
                    .child(tile.title.clone()),
            )
            .when(tile.explicit, |d| d.child(explicit_mark(&theme)));
        // A card names the kind before the subtitle, as Apple's do ("Song · Paramore").
        let subtitle = match (card, tile.subtitle.clone()) {
            (true, Some(s)) => Some(format!("{} · {s}", kind_name(tile.kind))),
            (true, None) => Some(kind_name(tile.kind).to_owned()),
            (false, s) => s,
        };
        let subtitle = subtitle.map(|s| {
            div()
                .max_w_full()
                .truncate()
                .text_size(sub_size)
                .text_color(colors.text_secondary)
                .child(s)
        });
        let text = div()
            .flex()
            .flex_col()
            .min_w_0()
            .when(artist && form == Form::Regular, |d| d.items_center())
            .when(large, |d| {
                // Editorial: eyebrow, title and subtitle above the picture.
                d.pb(theme.space.s).children(tile.badge.as_deref().map(|b| {
                    div()
                        .truncate()
                        .text_size(theme.text.subhead)
                        .font_weight(SEMIBOLD)
                        .text_color(colors.text_secondary)
                        .child(b.to_uppercase())
                }))
            })
            .when(form == Form::Regular, |d| d.pt(theme.space.s6))
            .when(card, |d| d.flex_1())
            .child(title_line)
            .children(subtitle);
        // A card's artwork sits as far from its edges as from its top and bottom.
        let card_pad = (theme.size.card - theme.size.art_card) / 2.;
        div()
            .id(("tile", self.index))
            .role(Role::Link)
            .aria_label(name)
            .key_context(CONTEXT)
            .when_some(self.focus, |d, h| d.track_focus(&h))
            .tab_index(0)
            .on_action(move |_: &OpenMenu, w, cx| {
                if let Some(f) = &key_menu {
                    let position = *anchor.read(cx);
                    f(
                        &ContentEvent::TileMenu {
                            tile: Box::new(key_tile.clone()),
                            position,
                        },
                        w,
                        cx,
                    );
                }
            })
            .focus_ring(cx)
            .group("tile")
            .flex()
            .flex_none()
            .w(self.width)
            .cursor_pointer()
            .on_click(move |e, w, cx| {
                if e.is_right_click() {
                    return;
                }
                if let (Some(f), Some(route)) = (&open, &route) {
                    f(&ContentEvent::Navigate(route.clone()), w, cx);
                }
            })
            .on_mouse_down(gpui::MouseButton::Right, move |e, w, cx| {
                if let Some(f) = &menu {
                    f(
                        &ContentEvent::TileMenu {
                            tile: Box::new(menu_tile.clone()),
                            position: e.position,
                        },
                        w,
                        cx,
                    );
                }
            })
            .map(|d| match form {
                Form::Regular => d
                    .flex_col()
                    .rounded(theme.radius.tile)
                    .child(artwork)
                    .child(text),
                Form::Large => d
                    .flex_col()
                    .rounded(theme.radius.tile)
                    .child(text)
                    .child(artwork),
                Form::Card => d
                    .h(theme.size.card)
                    .items_center()
                    .gap(card_pad)
                    .px(card_pad)
                    .rounded(theme.radius.card)
                    .bg(colors.fill)
                    .border(theme.size.hairline)
                    .border_color(colors.divider)
                    .child(artwork)
                    .child(text)
                    .children(card_more),
            })
    }
}
