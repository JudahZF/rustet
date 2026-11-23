//! Content components: they draw `rustet_app` page models and report what the user asked for as
//! [`ContentEvent`]s. They never load page data; the page that owns them does.
//!
//! - [`ArtworkView`]: an artwork at a size, its own colour until the image arrives.
//! - [`TileView`], [`ShelfView`], [`TileGrid`]: tiles, horizontal shelves, virtualised grids.
//! - [`TrackListView`]: the virtualised track list.
//! - [`PageHeader`]: the album and playlist header, and the artist hero.
//! - [`QualityStrip`]: the playing stream's format, set as type.
//! - [`LyricsView`]: line-synced lyrics.
//!
//! Stateless ones take an [`OnContent`] callback; entities emit [`ContentEvent`].

pub mod artwork;
pub mod grid;
pub mod lyrics;
pub mod page_header;
pub mod quality;
pub mod shelf;
pub mod tile;
pub mod track_list;

use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Pixels, Point, Window};
use rustet_app::pages::model::{Tile, TrackRow};
use rustet_app::pages::play::PlayTarget;
use rustet_app::route::Route;

pub use artwork::ArtworkView;
pub use grid::TileGrid;
pub use lyrics::{LyricsEvent, LyricsView};
pub use page_header::{HeaderAction, PageHeader};
pub use quality::{QualityStrip, quality_badge, quality_mark, quality_text};
pub use shelf::ShelfView;
pub use tile::TileView;
pub use track_list::{DraggedTracks, TrackListOptions, TrackListView};

/// What the user asked for in a content component.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentEvent {
    /// Open a page (a tile, an artist name, "See all").
    Navigate(Route),
    /// Open a link Rustet has no page for, in the browser.
    OpenExternal(String),
    /// Play this (a tile's play button).
    Play(PlayTarget),
    /// Play `rows` from `start` (Enter or a double click on a row).
    PlayTracks { rows: Arc<[TrackRow]>, start: usize },
    /// The context menu of a tile at the pointer (or the element, from the Menu key).
    TileMenu {
        tile: Box<Tile>,
        position: Point<Pixels>,
    },
    /// The context menu of the selected rows.
    TracksMenu {
        rows: Vec<TrackRow>,
        position: Point<Pixels>,
    },
}

/// The callback stateless content components report through.
pub type OnContent = Rc<dyn Fn(&ContentEvent, &mut Window, &mut App)>;
