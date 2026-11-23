//! An artwork at a drawn size. It asks the [`ArtworkStore`] for the image at the device-pixel
//! size (Apple scales the template), shows the artwork's own colour (`bgColor`) until the
//! image arrives, and fades the image in the first time it is drawn (never again, and not with
//! Reduce animations). Only drawn artwork is requested, so lists and shelves load what is on
//! screen.

use gpui::{App, IntoElement, ObjectFit, Pixels, RenderOnce, Styled, Window, div, img, prelude::*};
use rustet_app::pages::model::Artwork;

use crate::artwork::ArtworkStore;
use crate::theme::{ActiveTheme, Motion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Tile corners, 7 px (albums, playlists, stations).
    Rounded,
    /// A circle (artists).
    Round,
    /// 3 px corners (track rows, the song grid).
    Small,
    /// 4 px corners (the player's artwork).
    Thumb,
    /// 8 px corners (a header's artwork, search cards).
    Card,
    /// 10 px corners (an editorial picture).
    Editorial,
    /// No corners (the artist hero, edge to edge).
    Square,
}

#[derive(IntoElement)]
pub struct ArtworkView {
    artwork: Option<Artwork>,
    width: Pixels,
    /// Width over height; 1 for square.
    aspect: f32,
    shape: Shape,
}

impl ArtworkView {
    /// A square artwork `size` wide.
    pub fn new(artwork: Option<Artwork>, size: Pixels) -> Self {
        Self {
            artwork,
            width: size,
            aspect: 1.,
            shape: Shape::Rounded,
        }
    }

    /// Width over height (16/9 for videos).
    pub fn aspect(mut self, aspect: f32) -> Self {
        self.aspect = aspect.max(0.1);
        self
    }

    pub fn shape(mut self, shape: Shape) -> Self {
        self.shape = shape;
        self
    }
}

impl RenderOnce for ArtworkView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let palette = self.artwork.as_ref().and_then(|a| a.palette);
        let height = self.width / self.aspect;
        let edge = f32::from(self.width.max(height)) * window.scale_factor();
        let view = window.current_view();
        let image = self.artwork.as_ref().and_then(|a| {
            ArtworkStore::global(cx).update(cx, |s, cx| s.image(&a.template, edge, view, cx))
        });
        let frame = shaped(
            div()
                .relative()
                .flex_none()
                .w(self.width)
                .h(height)
                .overflow_hidden()
                .bg(theme.placeholder(palette.as_ref())),
            self.shape,
            &theme,
        );
        // The inset hairline that keeps a pale sleeve's edge on a pale page.
        let hairline = shaped(
            div()
                .absolute()
                .inset_0()
                .border(theme.size.hairline)
                .border_color(theme.colors.fill),
            self.shape,
            &theme,
        );
        let Some((image, since)) = image else {
            // No artwork at all (a track from a link, a restored queue): a note marks the place.
            return frame
                .when(self.artwork.is_none(), |d| {
                    d.flex().items_center().justify_center().child(
                        crate::icons::Icon::Song
                            .element()
                            .size(self.width.min(height) * 0.4)
                            .text_color(theme.colors.text_tertiary),
                    )
                })
                .child(hairline);
        };
        let fade = theme.motion(Motion::ArtworkFade);
        let opacity = if fade.is_zero() {
            1.
        } else {
            (since.as_secs_f32() / fade.as_secs_f32()).min(1.)
        };
        if opacity < 1. {
            window.request_animation_frame();
        }
        // gpui clips an image by its own corner radii, not its parent's: both get the shape.
        frame
            .child(shaped(
                img(image)
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .opacity(opacity),
                self.shape,
                &theme,
            ))
            .child(hairline)
    }
}

/// `e` with the corners of `shape`.
pub(crate) fn shaped<E: Styled>(e: E, shape: Shape, theme: &crate::theme::Theme) -> E {
    match shape {
        Shape::Rounded => e.rounded(theme.radius.tile),
        Shape::Round => e.rounded_full(),
        Shape::Small => e.rounded(theme.radius.small),
        Shape::Thumb => e.rounded(theme.radius.thumb),
        Shape::Card => e.rounded(theme.radius.card),
        Shape::Editorial => e.rounded(theme.radius.menu),
        Shape::Square => e,
    }
}
