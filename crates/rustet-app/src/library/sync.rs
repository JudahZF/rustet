//! Library sync: the Rust version of the page scripts' `_syncLibraryViaJS`.
//!
//! Same data as before: every library song (with its catalog and album IDs), album and
//! playlist, then each playlist's tracks, 100 per page, at most 50 000 per list; then one
//! [`Store::ingest`](rustet_library::Store::ingest), which replaces the cache.
//!
//! - **Resumable**: a [`LibrarySync`] keeps what it fetched and where each list stopped. After
//!   an error or a cancel (dropping the future), running the same value again continues at
//!   the page that failed. It resets after a successful save.
//! - **Incremental**: a playlist whose `lastModifiedDate` equals the one recorded at the last
//!   sync (in `library-sync.json` next to the cache) keeps its cached tracks instead of being
//!   fetched again. Playlist tracks are most of the requests of a sync.
//! - **Progress** is reported as [`SyncEvent`]s after each page.

use std::collections::BTreeMap;
use std::path::Path;

use rustet_ampapi::Api;
use rustet_ampapi::api::{LibraryContainer, LibraryKind};
use rustet_ampapi::model::{
    Cursor, LibraryAlbum, LibraryPlaylist, LibrarySong, Page, PlayParams, Resource,
};
use rustet_library::{IngestPayload, LibraryError, LibraryStats};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::Library;
use crate::pages::LoadError;
use crate::pages::more::with_params;

/// The page scripts stopped a list here.
const MAX_ITEMS: usize = 50_000;
const TRACK_PARAMS: &[(&str, &str)] = &[("include", "catalog,albums"), ("limit", "100")];
const LIST_PARAMS: &[(&str, &str)] = &[("include", "catalog"), ("limit", "100")];
const PLAYLIST_TRACK_PARAMS: &[(&str, &str)] = &[("include", "catalog"), ("limit", "100")];

/// The part of the library a sync is fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncPhase {
    Songs,
    Albums,
    Playlists,
    PlaylistTracks,
}

/// Progress of a sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    /// `fetched` items of a list so far.
    Fetched { phase: SyncPhase, fetched: usize },
    /// Tracks of playlist `index` (of `total`) are being fetched; `reused` when its cached
    /// tracks are still current.
    Playlist {
        index: usize,
        total: usize,
        name: String,
        reused: bool,
    },
    /// The cache was replaced.
    Saved(LibraryStats),
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("library sync: {0}")]
    Fetch(#[from] LoadError),
    #[error("library sync: {0}")]
    Store(#[from] LibraryError),
}

/// Where a paged list stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Next {
    #[default]
    First,
    More(Cursor),
    Done,
}

/// One list being fetched.
#[derive(Debug, Default)]
struct Stage {
    items: Vec<Item>,
    next: Next,
}

/// A sync in progress; see the module documentation. Create with `LibrarySync::default()`.
#[derive(Debug, Default)]
pub struct LibrarySync {
    songs: Stage,
    albums: Stage,
    playlists: Stage,
    /// Playlist ID → modification date, for the next incremental sync.
    modified: BTreeMap<String, String>,
    /// Tracks of the playlists done so far.
    playlist_tracks: BTreeMap<String, Vec<Item>>,
    /// The playlist being fetched (index into `playlists.items`) and its tracks so far.
    current: usize,
    current_tracks: Stage,
}

/// What the sync keeps of a resource: the fields `rustet-library`'s ingest reads, in the Apple
/// Music JSON shape it decodes.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    id: String,
    attributes: Attributes,
    relationships: Relationships,
    #[serde(skip)]
    modified: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Attributes {
    name: String,
    artist_name: String,
    album_name: String,
    duration_in_millis: u64,
    track_count: u32,
    track_number: u32,
    disc_number: u32,
    play_params: ItemPlayParams,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemPlayParams {
    id: String,
    catalog_id: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct Relationships {
    albums: Relation,
    catalog: Relation,
}

#[derive(Debug, Clone, Default, Serialize)]
struct Relation {
    data: Vec<Ref>,
}

#[derive(Debug, Clone, Serialize)]
struct Ref {
    id: String,
}

fn relation(id: Option<&str>) -> Relation {
    Relation {
        data: id.map(|id| Ref { id: id.to_owned() }).into_iter().collect(),
    }
}

fn play_params(p: Option<&PlayParams>) -> ItemPlayParams {
    p.map(|p| ItemPlayParams {
        id: p.id.clone(),
        catalog_id: p.stream_catalog_id().unwrap_or_default().to_owned(),
    })
    .unwrap_or_default()
}

fn song_item(song: &LibrarySong) -> Option<Item> {
    let a = song.attributes.as_ref()?;
    let r = &song.relationships;
    Some(Item {
        id: song.id.clone(),
        attributes: Attributes {
            name: a.name.clone(),
            artist_name: a.artist_name.clone().unwrap_or_default(),
            album_name: a.album_name.clone().unwrap_or_default(),
            duration_in_millis: a.duration_in_millis.unwrap_or_default(),
            track_number: a.track_number.unwrap_or_default(),
            disc_number: a.disc_number.unwrap_or_default(),
            play_params: play_params(a.play_params.as_ref()),
            ..Attributes::default()
        },
        relationships: Relationships {
            albums: relation(
                r.albums
                    .as_ref()
                    .and_then(|p| p.data.first())
                    .map(Resource::id),
            ),
            catalog: relation(
                r.catalog
                    .as_ref()
                    .and_then(|p| p.data.first())
                    .map(|c| c.id.as_str()),
            ),
        },
        modified: None,
    })
}

fn album_item(album: &LibraryAlbum) -> Option<Item> {
    let a = album.attributes.as_ref()?;
    let catalog = album
        .relationships
        .catalog
        .as_ref()
        .and_then(|p| p.data.first());
    Some(Item {
        id: album.id.clone(),
        attributes: Attributes {
            name: a.name.clone(),
            artist_name: a.artist_name.clone().unwrap_or_default(),
            track_count: a.track_count.unwrap_or_default(),
            play_params: play_params(a.play_params.as_ref()),
            ..Attributes::default()
        },
        relationships: Relationships {
            catalog: relation(catalog.map(|c| c.id.as_str())),
            ..Relationships::default()
        },
        modified: None,
    })
}

fn playlist_item(playlist: &LibraryPlaylist) -> Option<Item> {
    let a = playlist.attributes.as_ref()?;
    Some(Item {
        id: playlist.id.clone(),
        attributes: Attributes {
            name: a.name.clone(),
            ..Attributes::default()
        },
        relationships: Relationships::default(),
        modified: a.last_modified_date.clone(),
    })
}

/// A playlist entry: its library ID and `playParams` (the catalog ID the cache keeps).
fn entry_item(resource: &Resource) -> Option<Item> {
    let (id, params) = match resource {
        Resource::LibrarySong(s) => (&s.id, s.attributes.as_ref()?.play_params.as_ref()),
        Resource::LibraryMusicVideo(v) => (&v.id, v.attributes.as_ref()?.play_params.as_ref()),
        _ => return None,
    };
    Some(Item {
        id: id.clone(),
        attributes: Attributes {
            play_params: play_params(params),
            ..Attributes::default()
        },
        ..Item::default()
    })
}

/// The sync state file: modification dates of the playlists of the last sync.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// [`SYNC_STATE_VERSION`] of the sync that wrote the file.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    playlists: BTreeMap<String, String>,
}

/// Raise this when the stored playlist tracks change shape, so the next sync fetches every
/// playlist again. 1: tracks carry the replacement catalog ID (`reportingId`).
const SYNC_STATE_VERSION: u32 = 1;

/// The saved state; an older version reads as empty, so no cached playlist is reused.
fn read_state(path: &Path) -> SyncState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SyncState>(&bytes).ok())
        .filter(|state| state.version == SYNC_STATE_VERSION)
        .unwrap_or_default()
}

fn write_state(path: &Path, state: &SyncState) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(state)?)?;
    std::fs::rename(&tmp, path)
}

/// One page of `stage`'s list; `None` when the list is complete. 404 is an empty list
/// (Apple's answer for the tracks of an empty playlist).
async fn fetch_page<F>(
    api: &Api,
    stage: &Stage,
    first: F,
    params: &[(&str, &str)],
) -> Result<Option<Page<Resource>>, LoadError>
where
    F: Future<Output = Result<Page<Resource>, rustet_ampapi::Error>>,
{
    let cancel = CancellationToken::new();
    let result = match &stage.next {
        Next::Done => return Ok(None),
        _ if stage.items.len() >= MAX_ITEMS => return Ok(None),
        Next::First => first.await,
        Next::More(cursor) => api.next_page(&cancel, &with_params(cursor, params)).await,
    };
    match result {
        Ok(page) => Ok(Some(page)),
        Err(rustet_ampapi::Error::NotFound) => Ok(Some(Page::default())),
        Err(err) => Err(err.into()),
    }
}

/// Adds a fetched page to `stage`. Called only after the page arrived, so a dropped future
/// leaves the stage where it was.
fn take(stage: &mut Stage, page: Page<Resource>, item: impl Fn(&Resource) -> Option<Item>) {
    stage.items.extend(page.data.iter().filter_map(item));
    stage.next = page.next.map_or(Next::Done, Next::More);
}

impl LibrarySync {
    /// A sync stopped part-way that the next [`Library::sync`] continues.
    pub fn is_partial(&self) -> bool {
        self.songs.next != Next::First
    }
}

impl Library {
    /// Fetches the user's library and replaces the cache with it, reporting progress to
    /// `on_event`. Needs the music user token. On error `sync` keeps its place: call again with
    /// the same value to resume.
    pub async fn sync(
        &self,
        api: &Api,
        sync: &mut LibrarySync,
        mut on_event: impl FnMut(SyncEvent),
    ) -> Result<LibraryStats, SyncError> {
        if !api.is_signed_in() {
            return Err(LoadError::SignInRequired.into());
        }
        let cancel = CancellationToken::new();
        let lists = [
            (SyncPhase::Songs, LibraryKind::Songs, TRACK_PARAMS),
            (SyncPhase::Albums, LibraryKind::Albums, LIST_PARAMS),
            (SyncPhase::Playlists, LibraryKind::Playlists, LIST_PARAMS),
        ];
        for (phase, kind, params) in lists {
            let stage = match phase {
                SyncPhase::Songs => &mut sync.songs,
                SyncPhase::Albums => &mut sync.albums,
                _ => &mut sync.playlists,
            };
            while let Some(page) =
                fetch_page(api, stage, api.library(&cancel, kind), params).await?
            {
                take(stage, page, |r| match r {
                    Resource::LibrarySong(s) => song_item(s),
                    Resource::LibraryAlbum(a) => album_item(a),
                    Resource::LibraryPlaylist(p) => playlist_item(p),
                    _ => None,
                });
                on_event(SyncEvent::Fetched {
                    phase,
                    fetched: stage.items.len(),
                });
            }
        }

        let previous = read_state(&self.sync_state_path());
        let total = sync.playlists.items.len();
        while sync.current < total {
            let playlist = &sync.playlists.items[sync.current];
            let id = playlist.id.clone();
            let modified = playlist.modified.clone();
            let unchanged = modified.is_some() && previous.playlists.get(&id) == modified.as_ref();
            let cached = if unchanged && sync.current_tracks.next == Next::First {
                self.store.playlist_tracks(&id).await?
            } else {
                Vec::new()
            };
            on_event(SyncEvent::Playlist {
                index: sync.current,
                total,
                name: playlist.attributes.name.clone(),
                reused: !cached.is_empty(),
            });
            if cached.is_empty() {
                let stage = &mut sync.current_tracks;
                let first = || api.library_tracks(&cancel, LibraryContainer::Playlist, &id);
                while let Some(page) =
                    fetch_page(api, stage, first(), PLAYLIST_TRACK_PARAMS).await?
                {
                    take(stage, page, entry_item);
                    on_event(SyncEvent::Fetched {
                        phase: SyncPhase::PlaylistTracks,
                        fetched: stage.items.len(),
                    });
                }
            } else {
                sync.current_tracks.items = cached
                    .into_iter()
                    .map(|t| Item {
                        id: t.library_id,
                        attributes: Attributes {
                            play_params: ItemPlayParams {
                                id: String::new(),
                                catalog_id: t.catalog_id,
                            },
                            ..Attributes::default()
                        },
                        ..Item::default()
                    })
                    .collect();
            }
            let tracks = std::mem::take(&mut sync.current_tracks).items;
            if let Some(modified) = modified {
                sync.modified.insert(id.clone(), modified);
            }
            sync.playlist_tracks.insert(id, tracks);
            sync.current += 1;
        }

        // Build from a copy: if the save fails, the next run saves without fetching again.
        let mut playlists = sync.playlists.items.clone();
        // The fetched count is the true size; library playlists carry no `trackCount`.
        for p in &mut playlists {
            p.attributes.track_count = sync
                .playlist_tracks
                .get(&p.id)
                .map_or(0, |t| t.len() as u32);
        }
        let payload: IngestPayload = serde_json::from_value(serde_json::json!({
            "songs": sync.songs.items,
            "albums": sync.albums.items,
            "playlists": playlists,
            "playlistTracks": sync.playlist_tracks,
        }))
        .map_err(LibraryError::Json)?;
        self.store.ingest(payload).await?;
        let done = std::mem::take(sync);
        let state = SyncState {
            version: SYNC_STATE_VERSION,
            playlists: done.modified,
        };
        if let Err(err) = write_state(&self.sync_state_path(), &state) {
            tracing::warn!("[library] save sync state: {err}");
        }
        let stats = self.store.stats().await?;
        on_event(SyncEvent::Saved(stats));
        Ok(stats)
    }
}
