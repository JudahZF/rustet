//! What every page needs to know besides its model: the account, the playing track and the
//! user's playlists (the "Add to playlist" sub-menu).

use std::sync::Arc;

use rustet_app::pages::model::{TrackId, TrackRow};
use rustet_player::model::QueueItem;

/// A library playlist a song can be added to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistChoice {
    pub id: String,
    pub name: String,
}

/// The track the player has, as the rows of a page name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playing {
    pub catalog_id: String,
    pub library_id: Option<String>,
}

impl Playing {
    pub fn of(item: &QueueItem) -> Self {
        Self {
            catalog_id: item.id.0.clone(),
            library_id: item.library_id.clone(),
        }
    }

    /// Whether `row` is this track: the same library song, else the same catalog song.
    pub fn is(&self, row: &TrackRow) -> bool {
        match (&self.library_id, row.id.library_id()) {
            (Some(playing), Some(row)) => playing == row,
            _ => row.id.catalog_id() == Some(self.catalog_id.as_str()),
        }
    }

    /// The id of the row of `rows` that is playing, for `TrackListView::set_now_playing`.
    pub fn row_id(&self, rows: &[TrackRow]) -> Option<TrackId> {
        rows.iter().find(|r| self.is(r)).map(|r| r.id.clone())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageContext {
    pub signed_in: bool,
    /// For share links.
    pub storefront: String,
    /// The user's library playlists by name.
    pub playlists: Arc<[PlaylistChoice]>,
    pub playing: Option<Playing>,
}
