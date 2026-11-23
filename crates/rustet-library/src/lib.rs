//! Encrypted local metadata cache for the user's Apple Music library: songs, albums,
//! playlists and playlist membership. It mirrors what Android's MediaLibrary and
//! Windows' AMPLibraryAgent do with their local SQLite stores.
//!
//! - Runtime: in-memory SQLite for fast indexed queries.
//! - Persistence: AES-256-GCM encrypted JSON at `<cache>/library.enc`.
//! - Key: 32 random bytes, generated on first use, at `<cache>/library.key`.
//!
//! The renderer fills the cache through [`Store::ingest`]: MusicKit JS fetches the
//! library from Apple's API (it owns authentication) and posts the result to the engine.
//!
//! SQLite calls block, so every [`Store`] method runs them on the blocking thread pool.
#![forbid(unsafe_code)]

mod api;
mod crypto;
mod disk;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub use api::{AmItem, FetchError, fetch_playlist_tracks_once};
use crypto::KeyFile;
use disk::{AlbumInfo, CACHE_FORMAT_VERSION, DiskCache, SongInfo, go_time, nullable};

/// A sync is due when the last one is older than this.
const SYNC_MAX_AGE: TimeDelta = TimeDelta::hours(24);

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS songs (
        lid TEXT PRIMARY KEY, cid TEXT, name TEXT, artist TEXT, album TEXT, ms INTEGER,
        album_id TEXT NOT NULL DEFAULT '', track_number INTEGER NOT NULL DEFAULT 0,
        disc_number INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_songs_album_id ON songs(album_id);
    CREATE TABLE IF NOT EXISTS playlists (
        lid TEXT PRIMARY KEY, name TEXT, track_count INTEGER
    );
    CREATE TABLE IF NOT EXISTS playlist_tracks (
        playlist_id TEXT, position INTEGER, lid TEXT, cid TEXT,
        PRIMARY KEY (playlist_id, position)
    );
    CREATE TABLE IF NOT EXISTS albums (
        lid TEXT PRIMARY KEY, cid TEXT, name TEXT, artist TEXT, track_count INTEGER
    );
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
";

const INSERT_SONG: &str = "INSERT OR REPLACE INTO songs(lid,cid,name,artist,album,album_id,ms,track_number,disc_number) VALUES(?,?,?,?,?,?,?,?,?)";
const INSERT_ALBUM: &str =
    "INSERT OR REPLACE INTO albums(lid,cid,name,artist,track_count) VALUES(?,?,?,?,?)";
const INSERT_PLAYLIST: &str =
    "INSERT OR REPLACE INTO playlists(lid,name,track_count) VALUES(?,?,?)";
const INSERT_META: &str = "INSERT OR REPLACE INTO meta(key,value) VALUES(?,?)";

/// Errors of the library cache.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        source: io::Error,
    },
    #[error("key file wrong length {len} (expected 32) — delete {} to reset", path.display())]
    KeyLength { len: usize, path: PathBuf },
    #[error("ciphertext too short")]
    CiphertextTooShort,
    /// Encryption failed, or the cache was tampered with or sealed with another key.
    #[error("AES-GCM seal or open failed")]
    Crypto,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cache format v{0} is newer than this build understands (v{CACHE_FORMAT_VERSION})")]
    NewerFormat(u32),
}

/// A cached library playlist. Serialised as `{lid, name, trackCount}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistInfo {
    #[serde(rename = "lid", deserialize_with = "nullable")]
    pub library_id: String,
    #[serde(deserialize_with = "nullable")]
    pub name: String,
    #[serde(rename = "trackCount", deserialize_with = "nullable")]
    pub track_count: u32,
}

/// One entry of an ordered track list. Serialised as `{lid, cid}`; `cid` is left out
/// when empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistTrack {
    /// Library song ID (`i.…`).
    #[serde(rename = "lid", deserialize_with = "nullable")]
    pub library_id: String,
    /// Catalog song ID.
    #[serde(
        rename = "cid",
        skip_serializing_if = "String::is_empty",
        deserialize_with = "nullable"
    )]
    pub catalog_id: String,
}

/// Cache size. Serialised as `{songs, albums, playlists, syncedAt}`, the shape of the
/// `/api/v1/library/ingest` response; `syncedAt` is `0001-01-01T00:00:00Z` when the
/// library was never synced (the renderer checks for a year after 2000).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStats {
    pub songs: u64,
    pub albums: u64,
    pub playlists: u64,
    #[serde(serialize_with = "go_time::serialize")]
    pub synced_at: Option<DateTime<Utc>>,
}

/// Library data posted by the renderer's sync. Items follow the Apple Music API schema.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IngestPayload {
    #[serde(deserialize_with = "nullable")]
    pub songs: Vec<AmItem>,
    #[serde(deserialize_with = "nullable")]
    pub albums: Vec<AmItem>,
    #[serde(deserialize_with = "nullable")]
    pub playlists: Vec<AmItem>,
    /// Playlist ID → its entries, in order.
    #[serde(deserialize_with = "nullable")]
    pub playlist_tracks: BTreeMap<String, Vec<AmItem>>,
    /// Opaque library revision token, kept for a future delta sync.
    #[serde(deserialize_with = "nullable")]
    pub revision: String,
}

/// The library cache. Cheap to clone; clones share one database.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    /// One connection: every `:memory:` connection is its own empty database. The mutex
    /// also makes each dump a consistent snapshot.
    conn: Mutex<Connection>,
    /// Serialises saves, so an older snapshot never overwrites a newer one.
    save_lock: Mutex<()>,
    key: KeyFile,
    enc_path: PathBuf,
}

impl Store {
    /// Opens the cache in `cache_dir` (which must exist) and loads `library.enc` if it is
    /// there. An unreadable, tampered or newer-format dump is logged and ignored: the store
    /// starts empty and the next sync rebuilds it.
    pub async fn open(cache_dir: impl Into<PathBuf>) -> Result<Self, LibraryError> {
        let cache_dir = cache_dir.into();
        run_blocking(move || Inner::open(&cache_dir))
            .await
            .map(|inner| Self {
                inner: Arc::new(inner),
            })
    }

    /// Whether a sync should run: no sync ever completed, or the last one is older than
    /// 24 hours. A time gate, not a count gate, so a genuinely empty library does not
    /// trigger an endless retry loop.
    pub async fn needs_sync(&self) -> bool {
        let synced_at = self.with(Inner::synced_at).await.unwrap_or_else(|err| {
            warn!("[library] read synced_at: {err}");
            None
        });
        synced_at.is_none_or(|t| Utc::now() - t > SYNC_MAX_AGE)
    }

    /// The cached playlists.
    pub async fn playlists(&self) -> Result<Vec<PlaylistInfo>, LibraryError> {
        self.with(|inner| Ok(query_playlists(&inner.conn())?)).await
    }

    /// The ordered track list of a playlist. Empty means "not cached".
    pub async fn playlist_tracks(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<PlaylistTrack>, LibraryError> {
        let playlist_id = playlist_id.to_owned();
        self.with(move |inner| {
            let conn = inner.conn();
            let mut stmt = conn.prepare(
                "SELECT lid,cid FROM playlist_tracks WHERE playlist_id=? ORDER BY position",
            )?;
            stmt.query_map([playlist_id], track_row)?
                .collect::<Result<_, _>>()
                .map_err(Into::into)
        })
        .await
    }

    /// The ordered tracks of a library album (for example `l.1OBwMCC`), by disc, then
    /// track number. Empty when the album is not cached or `album_id` is empty.
    pub async fn songs_by_album(&self, album_id: &str) -> Result<Vec<PlaylistTrack>, LibraryError> {
        if album_id.is_empty() {
            return Ok(Vec::new());
        }
        let album_id = album_id.to_owned();
        self.with(move |inner| {
            let conn = inner.conn();
            let mut stmt = conn.prepare(
                "SELECT lid, cid FROM songs WHERE album_id = ? ORDER BY disc_number ASC, track_number ASC, rowid ASC",
            )?;
            stmt.query_map([album_id], track_row)?.collect::<Result<_, _>>().map_err(Into::into)
        })
        .await
    }

    /// The cache size and the time of the last sync.
    pub async fn stats(&self) -> Result<LibraryStats, LibraryError> {
        self.with(|inner| {
            let count = |table: &str| -> rusqlite::Result<u64> {
                inner
                    .conn()
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            };
            Ok(LibraryStats {
                songs: count("songs")?,
                albums: count("albums")?,
                playlists: count("playlists")?,
                synced_at: inner.synced_at()?,
            })
        })
        .await
    }

    /// Caches a fetched track list for a playlist, replacing the old one, then saves.
    pub async fn set_playlist_tracks(
        &self,
        playlist_id: &str,
        tracks: Vec<PlaylistTrack>,
    ) -> Result<(), LibraryError> {
        let playlist_id = playlist_id.to_owned();
        self.with(move |inner| {
            {
                let mut conn = inner.conn();
                let tx = conn.transaction()?;
                tx.execute(
                    "DELETE FROM playlist_tracks WHERE playlist_id=?",
                    [&playlist_id],
                )?;
                insert_playlist_tracks(
                    &tx,
                    &playlist_id,
                    tracks.iter().map(|t| (&*t.library_id, &*t.catalog_id)),
                )?;
                tx.commit()?;
            }
            inner.save();
            Ok(())
        })
        .await
    }

    /// Replaces the whole cache with `payload`, then saves. On error nothing changes.
    pub async fn ingest(&self, payload: IngestPayload) -> Result<(), LibraryError> {
        self.with(move |inner| {
            inner.ingest(&payload)?;
            info!(
                "[library] ingested: {} songs, {} albums, {} playlists",
                payload.songs.len(),
                payload.albums.len(),
                payload.playlists.len()
            );
            inner.save();
            Ok(())
        })
        .await
    }

    /// Runs `f` on the blocking pool.
    async fn with<T: Send + 'static>(&self, f: impl FnOnce(&Inner) -> T + Send + 'static) -> T {
        let inner = Arc::clone(&self.inner);
        run_blocking(move || f(&inner)).await
    }
}

/// Runs `f` on tokio's blocking pool, re-raising its panic here.
async fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(err) => std::panic::resume_unwind(err.into_panic()),
    }
}

impl Inner {
    fn open(cache_dir: &Path) -> Result<Self, LibraryError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let inner = Self {
            conn: Mutex::new(conn),
            save_lock: Mutex::new(()),
            key: KeyFile::new(cache_dir.join("library.key")),
            enc_path: cache_dir.join("library.enc"),
        };
        inner.load();
        Ok(inner)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn synced_at(&self) -> rusqlite::Result<Option<DateTime<Utc>>> {
        let value: Option<String> = self
            .conn()
            .query_row("SELECT value FROM meta WHERE key='synced_at'", [], |r| {
                r.get(0)
            })
            .optional()?;
        // An unparsable value counts as "never synced".
        Ok(value.and_then(|v| go_time::parse(&v).ok().flatten()))
    }

    /// Loads `library.enc` into the empty database. A missing file is the normal first
    /// start; any other problem is logged and the store stays empty.
    fn load(&self) {
        let Ok(data) = fs::read(&self.enc_path) else {
            return;
        };
        let dc = match self.decode(&data) {
            Ok(dc) => dc,
            Err(err) => return warn!("[library] load: {err} — ignoring the cache"),
        };
        if let Err(err) = self.load_dump(&dc) {
            return warn!("[library] load: {err}");
        }
        let ago = dc.synced_at.map(|t| (Utc::now() - t).num_seconds());
        info!(
            "[library] cache loaded: {} songs, {} playlists (synced {}s ago)",
            dc.songs.len(),
            dc.playlists.len(),
            ago.map_or_else(|| "never".into(), |s| s.to_string())
        );
    }

    fn decode(&self, data: &[u8]) -> Result<DiskCache, LibraryError> {
        let dc: DiskCache = serde_json::from_slice(&self.key.decrypt(data)?)?;
        if dc.version > CACHE_FORMAT_VERSION {
            return Err(LibraryError::NewerFormat(dc.version));
        }
        Ok(dc)
    }

    fn load_dump(&self, dc: &DiskCache) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(INSERT_SONG)?;
            for s in &dc.songs {
                stmt.execute(params![
                    s.lid, s.cid, s.name, s.artist, s.album, s.album_id, s.ms, s.track, s.disc
                ])?;
            }
            let mut stmt = tx.prepare(INSERT_ALBUM)?;
            for a in &dc.albums {
                stmt.execute(params![a.lid, a.cid, a.name, a.artist, a.track_count])?;
            }
            let mut stmt = tx.prepare(INSERT_PLAYLIST)?;
            for p in &dc.playlists {
                stmt.execute(params![p.library_id, p.name, p.track_count])?;
            }
        }
        for (playlist_id, tracks) in &dc.playlist_tracks {
            insert_playlist_tracks(
                &tx,
                playlist_id,
                tracks.iter().map(|t| (&*t.library_id, &*t.catalog_id)),
            )?;
        }
        set_meta(&tx, dc.synced_at, &dc.revision)?;
        tx.commit()
    }

    fn ingest(&self, p: &IngestPayload) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM songs; DELETE FROM albums; DELETE FROM playlists; DELETE FROM playlist_tracks;",
        )?;
        {
            let mut stmt = tx.prepare(INSERT_SONG)?;
            for item in &p.songs {
                let a = &item.attributes;
                let album_id = item
                    .relationships
                    .albums
                    .data
                    .first()
                    .map_or("", |r| r.id.as_str());
                stmt.execute(params![
                    item.id,
                    item.catalog_id(),
                    a.name,
                    a.artist_name,
                    a.album_name,
                    album_id,
                    a.duration_in_millis,
                    a.track_number,
                    a.disc_number
                ])?;
            }
            let mut stmt = tx.prepare(INSERT_ALBUM)?;
            for item in &p.albums {
                let a = &item.attributes;
                stmt.execute(params![
                    item.id,
                    item.catalog_id(),
                    a.name,
                    a.artist_name,
                    a.track_count
                ])?;
            }
            let mut stmt = tx.prepare(INSERT_PLAYLIST)?;
            for item in &p.playlists {
                stmt.execute(params![
                    item.id,
                    item.attributes.name,
                    item.attributes.track_count
                ])?;
            }
        }
        for (playlist_id, items) in &p.playlist_tracks {
            insert_playlist_tracks(
                &tx,
                playlist_id,
                items.iter().map(|i| (&*i.id, i.play_params_catalog_id())),
            )?;
        }
        set_meta(&tx, Some(Utc::now()), &p.revision)?;
        tx.commit()
    }

    /// Writes the encrypted dump. Failures are logged: the in-memory cache stays valid
    /// and the next change saves again.
    fn save(&self) {
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Err(err) = self.try_save() {
            warn!("[library] save: {err}");
        }
    }

    fn try_save(&self) -> Result<(), LibraryError> {
        let plain = serde_json::to_vec(&self.snapshot()?)?;
        let sealed = self.key.encrypt(&plain)?;
        // Write-then-rename so a crash mid-save never leaves a truncated library.enc.
        let tmp = self.enc_path.with_extension("enc.tmp");
        write_private(&tmp, &sealed).map_err(|source| LibraryError::Io {
            context: "write",
            source,
        })?;
        fs::rename(&tmp, &self.enc_path).map_err(|source| {
            let _ = fs::remove_file(&tmp);
            LibraryError::Io {
                context: "rename",
                source,
            }
        })
    }

    /// Dumps every table. The connection lock is held throughout, so a concurrent ingest
    /// cannot interleave and the dump is one consistent snapshot.
    fn snapshot(&self) -> rusqlite::Result<DiskCache> {
        let conn = self.conn();
        let songs = conn
            .prepare(
                "SELECT lid,cid,name,artist,album,album_id,ms,track_number,disc_number FROM songs",
            )?
            .query_map([], |r| {
                Ok(SongInfo {
                    lid: r.get(0)?,
                    cid: r.get(1)?,
                    name: r.get(2)?,
                    artist: r.get(3)?,
                    album: r.get(4)?,
                    album_id: r.get(5)?,
                    ms: r.get(6)?,
                    track: r.get(7)?,
                    disc: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let albums = conn
            .prepare("SELECT lid,cid,name,artist,track_count FROM albums")?
            .query_map([], |r| {
                Ok(AlbumInfo {
                    lid: r.get(0)?,
                    cid: r.get(1)?,
                    name: r.get(2)?,
                    artist: r.get(3)?,
                    track_count: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let playlists = query_playlists(&conn)?;
        let mut playlist_tracks = BTreeMap::<String, Vec<PlaylistTrack>>::new();
        let mut stmt = conn.prepare(
            "SELECT playlist_id,lid,cid FROM playlist_tracks ORDER BY playlist_id,position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            playlist_tracks
                .entry(r.get(0)?)
                .or_default()
                .push(PlaylistTrack {
                    library_id: r.get(1)?,
                    catalog_id: r.get(2)?,
                });
        }
        let meta = |key: &str| -> rusqlite::Result<Option<String>> {
            conn.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
                .optional()
        };
        let synced_at = meta("synced_at")?.and_then(|v| go_time::parse(&v).ok().flatten());
        let revision = meta("revision")?.unwrap_or_default();
        Ok(DiskCache {
            version: CACHE_FORMAT_VERSION,
            songs,
            albums,
            playlists,
            playlist_tracks,
            synced_at,
            revision,
        })
    }
}

fn track_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PlaylistTrack> {
    Ok(PlaylistTrack {
        library_id: r.get(0)?,
        catalog_id: r.get(1)?,
    })
}

fn query_playlists(conn: &Connection) -> rusqlite::Result<Vec<PlaylistInfo>> {
    conn.prepare("SELECT lid,name,track_count FROM playlists")?
        .query_map([], |r| {
            Ok(PlaylistInfo {
                library_id: r.get(0)?,
                name: r.get(1)?,
                track_count: r.get(2)?,
            })
        })?
        .collect()
}

/// Inserts `(lid, cid)` pairs at positions 0, 1, 2, …
fn insert_playlist_tracks<'a>(
    conn: &Connection,
    playlist_id: &str,
    tracks: impl Iterator<Item = (&'a str, &'a str)>,
) -> rusqlite::Result<()> {
    let mut stmt =
        conn.prepare("INSERT INTO playlist_tracks(playlist_id,position,lid,cid) VALUES(?,?,?,?)")?;
    for (position, (lid, cid)) in tracks.enumerate() {
        stmt.execute(params![playlist_id, position, lid, cid])?;
    }
    Ok(())
}

/// Records the sync time (RFC 3339, whole seconds) and, when not empty, the revision.
fn set_meta(
    conn: &Connection,
    synced_at: Option<DateTime<Utc>>,
    revision: &str,
) -> rusqlite::Result<()> {
    if let Some(t) = synced_at {
        conn.execute(
            INSERT_META,
            ["synced_at", &t.to_rfc3339_opts(SecondsFormat::Secs, true)],
        )?;
    }
    if !revision.is_empty() {
        conn.execute(INSERT_META, ["revision", revision])?;
    }
    Ok(())
}

/// Writes `data` to `path`, creating it with mode 0600 on Unix.
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(data)
}

#[cfg(test)]
mod tests;
