//! Play intents: what the engine's player gets when the user plays something (spec §4.2).
//!
//! Every function returns an [`rustet_player::model::Command`]: `SetQueue` (play now) or
//! `Insert` (play next, play later). The rules:
//!
//! - Unavailable tracks are left out; the start index counts only the tracks that stay.
//! - Play from a track keeps the player's shuffle mode (`shuffle: None`); Play turns shuffle
//!   off and starts at the first track; Shuffle turns it on and starts at a random track (the
//!   player keeps the start item first and shuffles the rest).
//! - Play next / play later insert the tracks with their album or playlist as `containerId`.
//! - A station starts with an empty queue and its container; the player fetches its tracks
//!   (needs the music user token). A live station is sent as `liveStation`, which the player
//!   refuses until live radio exists. Stations cannot be queued next or later.

use rustet_ampapi::Api;
use rustet_ampapi::model::AudioTrait;
use rustet_player::model::{
    CatalogId, Command, Container, ContainerKind, Insert, InsertPosition, ItemKind, QueueItemIn,
    SetQueue, TrackMeta,
};

use super::catalog::{self, AlbumPage, ArtistPage, PlaylistPage};
use super::error::LoadError;
use super::model::{TrackKind, TrackRow};
use super::user::{self, LibraryAlbumPage, LibraryPage, LibraryPlaylistPage};
use crate::route::LibrarySection;

/// A station a tile or page can start.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StationRef {
    pub id: String,
    pub name: String,
    /// A live broadcast (`playParams.format == "stream"`).
    pub live: bool,
}

/// What a tile's play button plays. Containers load their tracks first ([`play_target`]).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    Album { id: String },
    Playlist { id: String },
    LibraryAlbum { id: String },
    LibraryPlaylist { id: String },
    Station(StationRef),
    Track(Box<TrackRow>),
}

/// How to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum How {
    /// From the track at this index of the list (a click on a row).
    From(usize),
    /// Play from the start, in order.
    InOrder,
    /// Play shuffled.
    Shuffled,
    /// Insert after the current track.
    Next,
    /// Insert after the user's queued tracks.
    Later,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayError {
    #[error("nothing here can be played")]
    NothingPlayable,
    /// The chosen track is not available in this storefront.
    #[error("this track is not available")]
    Unavailable,
    #[error("a station cannot be queued")]
    NotQueueable,
    /// Stations need the music user token.
    #[error("sign in to play stations")]
    SignInRequired,
    #[error("loading the tracks failed: {0}")]
    Load(#[from] LoadError),
}

fn trait_name(t: &AudioTrait) -> &str {
    match t {
        AudioTrait::Lossless => "lossless",
        AudioTrait::HiResLossless => "hi-res-lossless",
        AudioTrait::LossyStereo => "lossy-stereo",
        AudioTrait::Atmos => "atmos",
        AudioTrait::Spatial => "spatial",
        AudioTrait::Other(name) => name,
    }
}

/// The queue item of a playable row; `None` for an unavailable one.
pub fn queue_item(row: &TrackRow, container_id: Option<&str>) -> Option<QueueItemIn> {
    if !row.is_playable() {
        return None;
    }
    Some(QueueItemIn {
        id: CatalogId(row.id.catalog_id()?.to_owned()),
        library_id: row.id.library_id().map(str::to_owned),
        kind: match row.kind {
            TrackKind::Song => ItemKind::Song,
            TrackKind::MusicVideo => ItemKind::MusicVideo,
        },
        meta: TrackMeta {
            name: row.title.clone(),
            artist_name: row.artist.text.clone(),
            album_name: row
                .album
                .as_ref()
                .map(|a| a.text.clone())
                .unwrap_or_default(),
            artwork_url: row
                .artwork
                .as_ref()
                .map(|a| a.template.clone())
                .unwrap_or_default(),
            duration_ms: row.duration.map_or(0, |d| d.as_millis() as u64),
            audio_traits: row
                .traits
                .iter()
                .map(|t| trait_name(t).to_owned())
                .collect(),
            content_rating: row.explicit.then(|| "explicit".to_owned()),
        },
        container_id: container_id.map(str::to_owned),
    })
}

/// The command that plays `tracks` (with `context` as what started it) the way `how` says.
pub fn play(
    tracks: &[TrackRow],
    context: Option<Container>,
    how: How,
) -> Result<Command, PlayError> {
    if let How::From(index) = how
        && tracks.get(index).is_none_or(|row| !row.is_playable())
    {
        return Err(PlayError::Unavailable);
    }
    let insert = matches!(how, How::Next | How::Later);
    let container_id = context.as_ref().filter(|_| insert).map(|c| c.id.as_str());
    let items: Vec<QueueItemIn> = tracks
        .iter()
        .filter_map(|row| queue_item(row, container_id))
        .collect();
    if items.is_empty() {
        return Err(PlayError::NothingPlayable);
    }
    let (start_index, shuffle) = match how {
        // Playable rows before `index`: its position once the unavailable ones are gone.
        How::From(index) => (
            tracks[..index].iter().filter(|r| r.is_playable()).count(),
            None,
        ),
        How::InOrder => (0, Some(false)),
        How::Shuffled => (fastrand::usize(..items.len()), Some(true)),
        How::Next | How::Later => {
            let position = if how == How::Next {
                InsertPosition::Next
            } else {
                InsertPosition::Later
            };
            return Ok(Command::Insert(Insert { items, position }));
        }
    };
    Ok(Command::SetQueue(SetQueue {
        items,
        start_index,
        context,
        shuffle,
        start_ms: None,
        play: true,
    }))
}

/// The command that starts a station. Stations cannot be queued next or later.
pub fn station(station: &StationRef, how: How) -> Result<Command, PlayError> {
    if matches!(how, How::Next | How::Later) {
        return Err(PlayError::NotQueueable);
    }
    let kind = if station.live {
        ContainerKind::LiveStation
    } else {
        ContainerKind::Station
    };
    Ok(Command::SetQueue(SetQueue {
        items: Vec::new(),
        start_index: 0,
        context: Some(container(kind, &station.id, Some(&station.name))),
        shuffle: None,
        start_ms: None,
        play: true,
    }))
}

fn container(kind: ContainerKind, id: &str, name: Option<&str>) -> Container {
    Container {
        kind,
        id: id.to_owned(),
        name: name.map(str::to_owned),
    }
}

/// Plays a tile's target, loading an album's or playlist's tracks first (one request; a
/// playlist longer than 300 tracks takes one more per 100). Library targets and stations need
/// the music user token.
pub async fn play_target(api: &Api, target: &PlayTarget, how: How) -> Result<Command, PlayError> {
    match target {
        PlayTarget::Album { id } => catalog::album(api, id).await?.play(how),
        PlayTarget::Playlist { id } => {
            let mut page = catalog::playlist(api, id).await?;
            while let Some(more) = page.tracks.more.take() {
                let next = more.load(api).await?;
                page.tracks.extend(next);
            }
            page.play(how)
        }
        PlayTarget::LibraryAlbum { id } => user::library_album(api, id).await?.play(how),
        PlayTarget::LibraryPlaylist { id } => {
            let tracks = user::library_playlist_tracks(api, id).await?;
            play(
                &tracks,
                Some(container(ContainerKind::LibraryPlaylist, id, None)),
                how,
            )
        }
        PlayTarget::Station(s) => {
            if !api.is_signed_in() {
                return Err(PlayError::SignInRequired);
            }
            station(s, how)
        }
        PlayTarget::Track(row) => play(std::slice::from_ref(row.as_ref()), None, how),
    }
}

impl AlbumPage {
    pub fn play(&self, how: How) -> Result<Command, PlayError> {
        let tracks: Vec<TrackRow> = self.tracks().cloned().collect();
        let context = container(ContainerKind::Album, &self.id, Some(&self.header.title));
        play(&tracks, Some(context), how)
    }
}

impl PlaylistPage {
    /// Plays the loaded tracks (load `tracks.more` first for all of a long playlist, or use
    /// [`play_target`]).
    pub fn play(&self, how: How) -> Result<Command, PlayError> {
        let context = container(ContainerKind::Playlist, &self.id, Some(&self.header.title));
        play(&self.tracks.items, Some(context), how)
    }
}

impl ArtistPage {
    /// Plays the artist's top songs.
    pub fn play_top_songs(&self, how: How) -> Result<Command, PlayError> {
        let context = container(ContainerKind::Artist, &self.id, Some(&self.header.title));
        play(self.top_songs(), Some(context), how)
    }
}

impl LibraryAlbumPage {
    pub fn play(&self, how: How) -> Result<Command, PlayError> {
        let tracks: Vec<TrackRow> = self.tracks().cloned().collect();
        let context = container(
            ContainerKind::LibraryAlbum,
            &self.id,
            Some(&self.header.title),
        );
        play(&tracks, Some(context), how)
    }
}

impl LibraryPlaylistPage {
    pub fn play(&self, how: How) -> Result<Command, PlayError> {
        let context = container(
            ContainerKind::LibraryPlaylist,
            &self.id,
            Some(&self.header.title),
        );
        play(&self.tracks.items, Some(context), how)
    }
}

impl LibraryPage {
    /// Plays the loaded songs or videos of a track section. Tile sections have nothing to play
    /// directly ([`PlayError::NothingPlayable`]).
    pub fn play(&self, how: How) -> Result<Command, PlayError> {
        let super::model::Listing::Tracks(list) = &self.listing else {
            return Err(PlayError::NothingPlayable);
        };
        let id = match self.section {
            LibrarySection::MusicVideos => "library-music-videos",
            _ => "library-songs",
        };
        play(
            &list.items,
            Some(container(ContainerKind::LibrarySongs, id, None)),
            how,
        )
    }
}
