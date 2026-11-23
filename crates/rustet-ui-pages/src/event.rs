//! What a page asks the area to do.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Pixels, Point, Window};
use rustet_ampapi::api::ResourceRef;
use rustet_app::nav::MoreTarget;
use rustet_app::pages::Page;
use rustet_app::pages::actions::Action;
use rustet_app::pages::model::{TrackKind, TrackRow};
use rustet_app::pages::play::{How, PlayError, PlayTarget};
use rustet_app::route::Route;
use rustet_player::model::{Command, ItemKind};

use crate::menu::Entry;

/// Something to play.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayRequest {
    /// A command built from rows the page has (a row, Play, Shuffle, Play next).
    Intent(Result<Command, PlayError>),
    /// A tile's target: an album's or playlist's tracks load first.
    Target(PlayTarget, How),
}

impl PlayRequest {
    /// It starts playing a music video now (not one queued next or later): the window shows it.
    pub fn starts_music_video(&self) -> bool {
        match self {
            Self::Intent(Ok(Command::SetQueue(q))) => q
                .items
                .get(q.start_index)
                .is_some_and(|item| item.kind == ItemKind::MusicVideo),
            Self::Target(PlayTarget::Track(row), how) => {
                row.kind == TrackKind::MusicVideo && !matches!(how, How::Next | How::Later)
            }
            _ => false,
        }
    }
}

/// Something to download. The page is kept whole: the request needs its header.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadRequest {
    /// The page's own item (album, playlist, library album or playlist).
    Page(Arc<Page>),
    Tracks(Vec<TrackRow>),
    /// A tile's item.
    Item(ResourceRef),
}

/// A request of a page view or a menu entry.
#[derive(Debug, Clone, PartialEq)]
pub enum PageEvent {
    Navigate(Route),
    Back,
    /// Load the shown page again.
    Retry,
    /// Open the sign-in sheet.
    SignIn,
    OpenExternal(String),
    /// Copy a music.apple.com link ("Share link").
    CopyLink(String),
    LoadMore(MoreTarget),
    Play(PlayRequest),
    Act(Action),
    /// Several actions in order (multi-select "Remove from playlist").
    ActAll(Vec<Action>),
    /// Ask first: a dialog with `answer` and Cancel. `then_back` leaves the page once done
    /// (a deleted playlist).
    Confirm {
        title: String,
        message: String,
        answer: String,
        action: Action,
        then_back: bool,
    },
    Download(DownloadRequest),
    /// Ask for a name, then create a playlist with `tracks`.
    NewPlaylist {
        tracks: Vec<ResourceRef>,
    },
    RenamePlaylist {
        id: String,
        name: String,
    },
    /// Choose a playlist from every library playlist for `tracks`.
    PickPlaylist {
        tracks: Vec<ResourceRef>,
    },
    /// Open a playlist folder (`None`: the top level) on the Playlists page.
    OpenFolder {
        id: Option<String>,
        name: String,
    },
    Menu {
        entries: Vec<Entry>,
        position: Point<Pixels>,
    },
}

/// Where a view sends its events. The area's sink runs them; a test's records them.
pub type Sink = Rc<dyn Fn(PageEvent, &mut Window, &mut App)>;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rustet_app::pages::model::{Availability, Linked, TrackId, TrackKind, TrackRow};
    use rustet_app::pages::play::{self, How, PlayTarget};

    use super::PlayRequest;

    fn row(id: &str, kind: TrackKind) -> TrackRow {
        TrackRow {
            id: TrackId::Catalog(id.to_owned()),
            kind,
            number: None,
            disc: None,
            title: id.to_owned(),
            explicit: false,
            artist: Linked {
                text: "Artist".into(),
                route: None,
            },
            album: None,
            duration: Some(Duration::from_secs(200)),
            traits: Vec::new(),
            apple_digital_master: false,
            availability: Availability::Playable,
            artwork: None,
            popularity: None,
        }
    }

    #[test]
    fn only_a_music_video_played_now_opens_now_playing() {
        let rows = [
            row("song", TrackKind::Song),
            row("video", TrackKind::MusicVideo),
        ];
        let intent = |how| PlayRequest::Intent(play::play(&rows, None, how));
        assert!(intent(How::From(1)).starts_music_video());
        assert!(!intent(How::From(0)).starts_music_video(), "a song first");
        assert!(
            !intent(How::Next).starts_music_video(),
            "queued, not played"
        );

        let tile =
            |kind, how| PlayRequest::Target(PlayTarget::Track(Box::new(row("t", kind))), how);
        assert!(tile(TrackKind::MusicVideo, How::InOrder).starts_music_video());
        assert!(!tile(TrackKind::MusicVideo, How::Later).starts_music_video());
        assert!(!tile(TrackKind::Song, How::InOrder).starts_music_video());
    }
}
