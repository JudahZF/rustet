//! The local library cache (`rustet-library`) in the UI process (spec D7): where it lives, how it
//! is filled from the user's Apple Music library, and what the library screens read from it.
//!
//! The cache is the one the engine kept: `library.enc` and `library.key` in Rustet's cache
//! directory (`~/.cache/rustet`), so an existing cache keeps working. It holds songs, albums,
//! playlists and playlist order: enough to list playlists offline and to build a queue without
//! a request. The library pages themselves load from Apple ([`crate::pages::user`]).

mod folders;
mod sync;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustet_ampapi::Api;
use rustet_library::{IngestPayload, LibraryError, Store};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub use folders::{PlaylistEntry, playlist_folder};
pub use rustet_library::{LibraryStats, PlaylistInfo, PlaylistTrack};
pub use sync::{LibrarySync, SyncError, SyncEvent, SyncPhase};

/// The file next to the cache that makes a sync incremental (playlist modification dates).
const SYNC_STATE_FILE: &str = "library-sync.json";

/// The library cache.
#[derive(Clone)]
pub struct Library {
    store: Store,
    dir: PathBuf,
}

/// What Settings → Library shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryStatus {
    pub stats: LibraryStats,
    /// No sync ever completed, or the last one is older than 24 hours.
    pub needs_sync: bool,
}

impl Library {
    /// Opens the cache in Rustet's cache directory, where the engine kept it.
    pub async fn open() -> Result<Self, LibraryError> {
        Self::open_in(rustet_core::appdir::cache_dir()).await
    }

    /// Opens the cache in `dir`, creating the directory (private: it holds the key).
    pub async fn open_in(dir: impl AsRef<Path>) -> Result<Self, LibraryError> {
        let dir = dir.as_ref().to_owned();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir).map_err(|source| LibraryError::Io {
            context: "create the cache directory",
            source,
        })?;
        Ok(Self {
            store: Store::open(&dir).await?,
            dir,
        })
    }

    /// The store, for its queries (album and playlist track order).
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub async fn status(&self) -> Result<LibraryStatus, LibraryError> {
        Ok(LibraryStatus {
            stats: self.store.stats().await?,
            needs_sync: self.store.needs_sync().await,
        })
    }

    /// The cached playlists by name (the sidebar's list, offline).
    pub async fn playlists(&self) -> Result<Vec<PlaylistInfo>, LibraryError> {
        let mut playlists = self.store.playlists().await?;
        playlists.sort_by_cached_key(|p| (p.name.to_lowercase(), p.library_id.clone()));
        Ok(playlists)
    }

    fn sync_state_path(&self) -> PathBuf {
        self.dir.join(SYNC_STATE_FILE)
    }

    /// Empties the cache (sign-out): an empty library is saved over the old one and the
    /// incremental sync state is removed. The store records the time as a sync, so the next
    /// sign-in syncs by itself ([`Syncer::start`]), not by age.
    pub async fn clear(&self) -> Result<(), LibraryError> {
        self.store.ingest(IngestPayload::default()).await?;
        match std::fs::remove_file(self.sync_state_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                warn!("[library] remove the sync state: {e}");
            }
            _ => {}
        }
        Ok(())
    }
}

/// Where the library sync is, for Settings → Library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    /// No sync ran in this session.
    Idle,
    /// A sync runs; the latest progress, `None` before the first page.
    Running(Option<SyncEvent>),
    Done(LibraryStats),
    /// The sync stopped; "Sync now" continues where it stopped.
    Failed(String),
}

struct SyncerInner {
    library: Library,
    api: Api,
    status: watch::Sender<SyncStatus>,
    /// What a stopped sync fetched so far, for the next run.
    progress: Arc<tokio::sync::Mutex<LibrarySync>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// Runs library syncs in the background, one at a time. Cheap to clone.
#[derive(Clone)]
pub struct Syncer(Arc<SyncerInner>);

impl Syncer {
    pub fn new(library: Library, api: Api) -> Self {
        Self(Arc::new(SyncerInner {
            library,
            api,
            status: watch::channel(SyncStatus::Idle).0,
            progress: Arc::default(),
            task: Mutex::new(None),
        }))
    }

    pub fn library(&self) -> &Library {
        &self.0.library
    }

    pub fn status(&self) -> watch::Receiver<SyncStatus> {
        self.0.status.subscribe()
    }

    /// Starts a sync unless one runs. Must run inside the tokio runtime.
    pub fn start(&self) {
        let mut task = self.0.task.lock().unwrap_or_else(|e| e.into_inner());
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let me = self.0.clone();
        *task = Some(tokio::spawn(async move {
            let mut progress = me.progress.lock().await;
            me.status.send_replace(SyncStatus::Running(None));
            let status = &me.status;
            let result = me
                .library
                .sync(&me.api, &mut progress, |event| {
                    status.send_replace(SyncStatus::Running(Some(event)));
                })
                .await;
            status.send_replace(match result {
                Ok(stats) => {
                    info!(
                        "[library] synced: {} songs, {} playlists",
                        stats.songs, stats.playlists
                    );
                    SyncStatus::Done(stats)
                }
                Err(e) => {
                    warn!("{e}");
                    SyncStatus::Failed(e.to_string())
                }
            });
        }));
    }

    /// Starts a sync when the cache is older than a day or was never filled.
    pub async fn start_if_due(&self) {
        if self.0.library.store.needs_sync().await {
            self.start();
        }
    }

    /// Stops a running sync, forgets its progress and empties the cache (sign-out).
    pub async fn stop_and_clear(&self) {
        let task = self.0.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        *self.0.progress.lock().await = LibrarySync::default();
        if let Err(e) = self.0.library.clear().await {
            warn!("[library] clear: {e}");
        }
        self.0.status.send_replace(SyncStatus::Idle);
    }
}
