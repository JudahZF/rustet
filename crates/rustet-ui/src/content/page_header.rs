//! The header of an album, playlist, station, curator or artist page.
//!
//! The album and playlist header: a 270 px artwork (8 px corners, a soft shadow), then, 34 px
//! to its right and centred on it, the title (26 / 700) with the explicit badge, the artist
//! line (26 / 400, a link), the facts (12 / 600, secondary, "Rock · 2000 · Lossless"), the
//! notes (13, three lines, "MORE" shows all) and the Play and Shuffle key pills. Add, download
//! and "more" are 36 px glass circles at the header's top right.
//!
//! The artist hero ([`PageHeader::hero`]): the picture across the whole content width,
//! fading into the page colour, the name (40 / 700) over its lower edge, and below it the
//! actions as circles: play as a 72 px key circle between 48 px glass circles.
//!
//! The page decides which actions apply and what they do; the header draws them and reports
//! the one used. The page colour comes from the window's page tint
//! ([`crate::theme::set_page_tint`]), not from the header.

use std::rc::Rc;

use gpui::{
    App, IntoElement, ParentElement, RenderOnce, Role, SharedString, Styled, Window, canvas, div,
    linear_color_stop, linear_gradient, prelude::*, px,
};
use rustet_app::pages::model::{Header, Lossless};

use super::artwork::{ArtworkView, Shape};
use super::{ContentEvent, OnContent};
use crate::components::{Button, Circle, FocusRing, IconButton};
use crate::icons::Icon;
use crate::theme::{ActiveTheme, BOLD, REGULAR, SEMIBOLD, Theme};

/// An action of the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaderAction {
    Play,
    Shuffle,
    AddToLibrary,
    Download,
    /// The page's context menu.
    More,
}

impl HeaderAction {
    fn label(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::Shuffle => "Shuffle",
            Self::AddToLibrary => "Add to library",
            Self::Download => "Download",
            Self::More => "More",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Play => Icon::Play,
            Self::Shuffle => Icon::Shuffle,
            Self::AddToLibrary => Icon::Plus,
            Self::Download => Icon::Download,
            Self::More => Icon::More,
        }
    }

    /// Play and Shuffle are pills under the text; the others are round.
    fn is_pill(self) -> bool {
        matches!(self, Self::Play | Self::Shuffle)
    }
}

type OnAction = Rc<dyn Fn(HeaderAction, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct PageHeader {
    header: Header,
    facts: Vec<SharedString>,
    actions: Vec<HeaderAction>,
    round: bool,
    hero: bool,
    on_action: Option<OnAction>,
    on_event: Option<OnContent>,
}

impl PageHeader {
    pub fn new(header: Header) -> Self {
        Self {
            header,
            facts: Vec::new(),
            actions: Vec::new(),
            round: false,
            hero: false,
            on_action: None,
            on_event: None,
        }
    }

    /// Facts under the artist line ("Rock", "2000"), joined with " · ".
    pub fn facts(mut self, facts: impl IntoIterator<Item = impl Into<SharedString>>) -> Self {
        self.facts = facts.into_iter().map(Into::into).collect();
        self
    }

    /// The actions this page offers.
    pub fn actions(mut self, actions: impl IntoIterator<Item = HeaderAction>) -> Self {
        self.actions = actions.into_iter().collect();
        self
    }

    /// A round artwork (stations, curators).
    pub fn round(mut self) -> Self {
        self.round = true;
        self
    }

    /// The artist hero instead of the album header.
    pub fn hero(mut self) -> Self {
        self.hero = true;
        self
    }

    pub fn on_action(mut self, f: impl Fn(HeaderAction, &mut Window, &mut App) + 'static) -> Self {
        self.on_action = Some(Rc::new(f));
        self
    }

    /// For the artist link.
    pub fn on_event(mut self, f: OnContent) -> Self {
        self.on_event = Some(f);
        self
    }

    fn act(&self, a: HeaderAction) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        let f = self.on_action.clone();
        move |_, w, cx| {
            if let Some(f) = &f {
                f(a, w, cx);
            }
        }
    }

    fn round_button(&self, ix: usize, a: HeaderAction, size: Circle) -> IconButton {
        IconButton::new(("action", ix), a.icon(), a.label())
            .glass(size)
            .on_click(self.act(a))
    }
}

impl RenderOnce for PageHeader {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        if self.hero {
            self.render_hero(window, cx, &theme).into_any_element()
        } else {
            self.render_album(window, cx, &theme).into_any_element()
        }
    }
}

impl PageHeader {
    fn render_album(self, window: &mut Window, cx: &mut App, theme: &Theme) -> impl IntoElement {
        let colors = theme.colors;
        let header = &self.header;
        let expanded = window.use_keyed_state("header-notes", cx, |_, _| false);
        let notes_open = *expanded.read(cx);
        let quality = match header.quality.lossless {
            Some(Lossless::HiRes) => Some("Hi-Res Lossless"),
            Some(Lossless::Lossless) => Some("Lossless"),
            None => None,
        };
        let meta: Vec<&str> = self
            .facts
            .iter()
            .map(|f| f.as_ref())
            .chain(quality)
            .chain(header.quality.dolby_atmos.then_some("Dolby Atmos"))
            .collect();
        let notes = header.notes.clone().and_then(|n| {
            let short = n.short.clone().or(n.full.clone())?;
            let more = n.full.filter(|f| *f != short);
            Some((short, more))
        });

        let pills = self
            .actions
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, a)| a.is_pill())
            .map(|(ix, a)| {
                Button::new(("action", ix), a.label())
                    .primary()
                    .large()
                    .icon(a.icon())
                    .on_click(self.act(a))
            })
            .collect::<Vec<_>>();
        let circles = self
            .actions
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, a)| !a.is_pill())
            .map(|(ix, a)| self.round_button(ix, a, Circle::Regular))
            .collect::<Vec<_>>();

        let subtitle = header
            .subtitle
            .clone()
            .map(|s| link(theme, s.text, s.route, self.on_event.clone(), cx));

        div()
            .id("page-header")
            .role(Role::Group)
            .aria_label(SharedString::from(header.title.clone()))
            .relative()
            .w_full()
            .px(theme.space.xl)
            .pt(theme.size.strip)
            .pb(theme.space.xl)
            .when(!circles.is_empty(), |d| {
                d.child(
                    div()
                        .absolute()
                        .top(theme.size.strip)
                        .right(theme.space.xl)
                        .flex()
                        .gap(theme.space.s)
                        .children(circles),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.size.header_gap)
                    .child(
                        div()
                            .flex_none()
                            .rounded(theme.radius.card)
                            .when(self.round, |d| d.rounded_full())
                            .shadow(theme.artwork_shadow())
                            .child(
                                ArtworkView::new(header.artwork.clone(), theme.size.art_header)
                                    .shape(if self.round {
                                        Shape::Round
                                    } else {
                                        Shape::Card
                                    }),
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
                                    .gap(theme.space.s)
                                    .child(
                                        div()
                                            .id("page-title")
                                            .role(Role::Heading)
                                            .aria_level(1)
                                            .aria_label(SharedString::from(header.title.clone()))
                                            .line_clamp(2)
                                            .text_size(theme.text.large_title)
                                            .font_weight(BOLD)
                                            .text_color(colors.text)
                                            .child(header.title.clone()),
                                    )
                                    .when(header.explicit, |d| {
                                        d.child(super::tile::explicit_mark(theme))
                                    }),
                            )
                            .children(subtitle)
                            .when(!meta.is_empty(), |d| {
                                d.child(
                                    div()
                                        .pt(theme.space.xxs)
                                        .text_size(theme.text.callout)
                                        .font_weight(SEMIBOLD)
                                        .text_color(colors.text_secondary)
                                        .child(meta.join(" · ")),
                                )
                            })
                            .children(notes.map(|(short, more)| {
                                let text = if notes_open {
                                    more.clone().unwrap_or(short.clone())
                                } else {
                                    short
                                };
                                let toggle = expanded.clone();
                                div()
                                    .pt(theme.space.l)
                                    .max_w(theme.size.notes_max_width)
                                    .flex()
                                    .items_end()
                                    .gap(theme.space.xs)
                                    // As wide as its text, so "MORE" follows a short note.
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_size(theme.text.body)
                                            .line_height(theme.text.body_line)
                                            .text_color(colors.text_secondary)
                                            .when(!notes_open, |d| d.line_clamp(3))
                                            .child(text),
                                    )
                                    .when(more.is_some(), |d| {
                                        d.child(
                                            div()
                                                .id("notes-more")
                                                .role(Role::Button)
                                                .aria_label(if notes_open {
                                                    "Less"
                                                } else {
                                                    "More"
                                                })
                                                .aria_expanded(notes_open)
                                                .tab_index(0)
                                                .focus_ring(cx)
                                                .flex_none()
                                                .cursor_pointer()
                                                .text_size(theme.text.subhead)
                                                .font_weight(BOLD)
                                                .text_color(colors.text)
                                                .on_click(move |_, _, cx| {
                                                    toggle.update(cx, |open, cx| {
                                                        *open = !*open;
                                                        cx.notify();
                                                    })
                                                })
                                                .child(if notes_open { "LESS" } else { "MORE" }),
                                        )
                                    })
                            }))
                            .when(!pills.is_empty(), |d| {
                                d.child(
                                    div()
                                        .pt(theme.space.l)
                                        .flex()
                                        .gap(theme.space.m)
                                        .children(pills),
                                )
                            }),
                    ),
            )
    }

    fn render_hero(self, window: &mut Window, cx: &mut App, theme: &Theme) -> impl IntoElement {
        let colors = theme.colors;
        let header = &self.header;
        // The picture is as wide as the content: measured while painting.
        let width = window.use_keyed_state("hero-width", cx, |_, _| px(0.));
        let measured = *width.read(cx);
        let picture_width = measured.max(theme.size.art_header);
        let picture_height = picture_width / HERO_ASPECT;
        let fade = theme.base();
        let mut buttons = Vec::new();
        for (ix, a) in self.actions.iter().copied().enumerate() {
            buttons.push(if a == HeaderAction::Play {
                IconButton::new(("action", ix), a.icon(), a.label())
                    .key(Circle::Key)
                    .on_click(self.act(a))
            } else {
                self.round_button(ix, a, Circle::Large)
            });
        }
        div()
            .id("page-header")
            .role(Role::Group)
            .aria_label(SharedString::from(header.title.clone()))
            .relative()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(picture_height)
                    .overflow_hidden()
                    .child(
                        ArtworkView::new(header.artwork.clone(), picture_width)
                            .aspect(HERO_ASPECT)
                            .shape(Shape::Square),
                    )
                    // The fade into the page colour.
                    .child(div().absolute().inset_0().bg(linear_gradient(
                        180.,
                        linear_color_stop(gpui::Hsla { a: 0., ..fade }, 0.45),
                        linear_color_stop(fade, 1.),
                    )))
                    .child(
                        div()
                            .id("page-title")
                            .role(Role::Heading)
                            .aria_level(1)
                            .aria_label(SharedString::from(header.title.clone()))
                            .absolute()
                            .left(theme.space.xl)
                            .right(theme.space.xl)
                            .bottom(theme.space.l)
                            .flex()
                            .justify_center()
                            .text_size(theme.text.hero)
                            .font_weight(BOLD)
                            .text_color(colors.on_artwork)
                            .truncate()
                            .child(header.title.clone()),
                    )
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                width.update(cx, |w, cx| {
                                    if *w != bounds.size.width {
                                        *w = bounds.size.width;
                                        cx.notify();
                                        window.request_animation_frame();
                                    }
                                })
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .when(!buttons.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.space.l)
                        .children(buttons),
                )
            })
            .child(div().h(theme.space.xl))
    }
}

/// Width over height of the artist hero's picture.
const HERO_ASPECT: f32 = 2.;

/// The artist line under a title: 26 / 400, a link when it has a route.
fn link(
    theme: &Theme,
    text: String,
    route: Option<rustet_app::route::Route>,
    on_event: Option<OnContent>,
    cx: &App,
) -> impl IntoElement {
    div()
        .id("header-subtitle")
        .when(route.is_some(), |d| {
            d.role(Role::Link)
                .aria_label(SharedString::from(text.clone()))
                .tab_index(0)
                .focus_ring(cx)
        })
        .self_start()
        .text_size(theme.text.large_title)
        .font_weight(REGULAR)
        .text_color(theme.colors.text)
        .when_some(route, |d, route| {
            d.cursor_pointer()
                .hover(|st| st.underline())
                .on_click(move |_, w, cx| {
                    if let Some(f) = &on_event {
                        f(&ContentEvent::Navigate(route.clone()), w, cx);
                    }
                })
        })
        .child(text)
}
