//! What a page shows while it loads: the kit's still placeholders, in the shape of the page
//! that is coming (a header and rows for an album, shelves for New, a grid for library albums).

use gpui::{AnyElement, IntoElement, ParentElement, Pixels, Styled, div};
use rustet_app::route::{LibrarySection, Route};
use rustet_ui::components::{Placeholder, loading};
use rustet_ui::theme::Theme;

/// The shape of a loading page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Header with artwork, then track rows (album, playlist).
    HeaderRows,
    /// Header, then shelves (artist, station, curator, song, music video).
    HeaderShelves,
    /// A title and shelves (New, Radio, Home, charts, search).
    Shelves,
    /// A title and a grid of tiles (library albums, "see all").
    Grid,
    /// A title and rows (library songs).
    Rows,
}

pub fn shape(route: &Route) -> Shape {
    match route {
        Route::Album { .. }
        | Route::Playlist { .. }
        | Route::LibraryAlbum { .. }
        | Route::LibraryPlaylist { .. } => Shape::HeaderRows,
        Route::Artist { .. }
        | Route::Song { .. }
        | Route::MusicVideo { .. }
        | Route::Station { .. }
        | Route::Curator { .. } => Shape::HeaderShelves,
        Route::Library(LibrarySection::Songs | LibrarySection::MusicVideos) => Shape::Rows,
        Route::Library(_) | Route::ArtistView { .. } | Route::Room { .. } => Shape::Grid,
        Route::Search { .. }
        | Route::LibrarySearch { .. }
        | Route::New
        | Route::Radio
        | Route::Home
        | Route::Charts { .. }
        | Route::Downloads
        | Route::Settings { .. } => Shape::Shelves,
    }
}

/// A bar in place of a line of text `width` wide and as tall as a page title.
fn title_bar(theme: &Theme, width: Pixels) -> gpui::Div {
    div()
        .h(theme.text.page_title_line)
        .w(width)
        .rounded(theme.radius.small)
        .bg(theme.colors.placeholder)
}

/// The album header's shape: the 270 px artwork, then the title, artist and pills.
fn header(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(theme.size.header_gap)
        .child(
            div()
                .flex_none()
                .size(theme.size.art_header)
                .rounded(theme.radius.card)
                .bg(theme.colors.placeholder),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(theme.space.s)
                .child(title_bar(theme, theme.size.art_header))
                .child(title_bar(theme, theme.size.tile_min).h(theme.text.large_title))
                .child(div().h(theme.size.button)),
        )
}

pub fn render(route: &Route, theme: &Theme) -> AnyElement {
    let tiles = |count| Placeholder::Tiles {
        count,
        size: theme.size.tile_min,
    };
    let body = loading("page-loading")
        .flex()
        .flex_col()
        .gap(theme.space.xl)
        .px(theme.space.xl)
        .pt(theme.size.strip)
        .overflow_hidden();
    let title = || title_bar(theme, theme.size.tile_min);
    match shape(route) {
        Shape::HeaderRows => body.child(header(theme)).child(Placeholder::Rows(8)),
        Shape::HeaderShelves => body.child(header(theme)).child(tiles(6)),
        Shape::Shelves => body.child(title()).child(tiles(6)).child(tiles(6)),
        Shape::Grid => body
            .child(title())
            .child(tiles(6))
            .child(tiles(6))
            .child(tiles(6)),
        Shape::Rows => body.child(title()).child(Placeholder::Rows(12)),
    }
    .into_any_element()
}
