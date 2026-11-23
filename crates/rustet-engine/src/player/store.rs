//! The player's files in the engine data directory, written atomically.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rustet_player::ports::{Store, StoreError, StoreKey};

/// `mkdir -p` with mode 0700 on Unix: the files hold the user's queue and history.
pub(crate) fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// Stores each [`StoreKey`] as `<dir>/<file name>`.
pub(crate) struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

fn store_error(key: StoreKey, e: impl std::fmt::Display) -> StoreError {
    StoreError(format!("{}: {e}", key.file_name()))
}

/// Writes `bytes` next to `path`, syncs, then renames over it: a crash leaves the old file or the
/// new one, never half of one.
fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    create_private_dir(dir)?;
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[async_trait]
impl Store for FileStore {
    async fn load(&self, key: StoreKey) -> Result<Option<Vec<u8>>, StoreError> {
        match tokio::fs::read(self.dir.join(key.file_name())).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(store_error(key, e)),
        }
    }

    async fn save(&self, key: StoreKey, bytes: Vec<u8>) -> Result<(), StoreError> {
        let (dir, path) = (self.dir.clone(), self.dir.join(key.file_name()));
        tokio::task::spawn_blocking(move || write_atomic(&dir, &path, &bytes))
            .await
            .map_err(|e| store_error(key, e))?
            .map_err(|e| store_error(key, e))
    }

    async fn delete(&self, key: StoreKey) -> Result<(), StoreError> {
        match tokio::fs::remove_file(self.dir.join(key.file_name())).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(store_error(key, e)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn files_round_trip_under_the_crate_names() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("engine-data"));
        assert_eq!(
            store.load(StoreKey::Snapshot).await.unwrap(),
            None,
            "a missing file is no data"
        );
        store.delete(StoreKey::Snapshot).await.unwrap();
        store
            .save(StoreKey::Snapshot, b"{\"v\":3}".to_vec())
            .await
            .unwrap();
        store
            .save(StoreKey::Snapshot, b"{\"v\":4}".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.load(StoreKey::Snapshot).await.unwrap().as_deref(),
            Some(&b"{\"v\":4}"[..])
        );
        let names: Vec<_> = std::fs::read_dir(dir.path().join("engine-data"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["player-state.json"], "no temp file left behind");
        store.delete(StoreKey::Snapshot).await.unwrap();
        assert_eq!(store.load(StoreKey::Snapshot).await.unwrap(), None);
    }
}
