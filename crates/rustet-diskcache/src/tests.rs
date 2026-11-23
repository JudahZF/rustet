use std::path::{Component, Path};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::TryStreamExt;
use tokio::time::timeout;

use super::*;

fn new_cache() -> (tempfile::TempDir, Cache) {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::new(dir.path()).unwrap();
    (dir, cache)
}

async fn read_all(reader: StreamingReader) -> Result<Vec<u8>, Error> {
    let chunks: Vec<Bytes> = reader.into_stream().try_collect().await?;
    Ok(chunks.concat())
}

fn set_mtime(path: &Path, ago: Duration) {
    let t = SystemTime::now() - ago;
    std::fs::set_times(
        path,
        std::fs::FileTimes::new().set_accessed(t).set_modified(t),
    )
    .unwrap();
}

// A cache key must never escape the cache directory.
#[test]
fn filename_stays_inside_dir() {
    let dir = Path::new("/cache");
    for id in [
        "../../etc/passwd",
        "..",
        ".",
        "a/b",
        r"a\b",
        "a\0b",
        "../x",
        "/abs",
        "",
        "ünï",
    ] {
        for q in ["alac", "../q", ""] {
            let name = filename(id, q);
            assert!(
                !name.contains(['/', '\\', '\0']),
                "filename({id:?}, {q:?}) = {name:?} has a separator or NUL"
            );
            let path = dir.join(&name);
            assert_eq!(path.parent(), Some(dir), "{name:?} escapes the cache dir");
            assert!(matches!(
                path.components().next_back(),
                Some(Component::Normal(_))
            ));
        }
    }
    // Same names as the Go engine, so its cache directories keep working.
    assert_eq!(
        filename("1440818839", "alac_raw"),
        "1440818839-alac_raw.m4a"
    );
    assert_eq!(
        filename("ünï", "mv-vseg-seek-12.000"),
        "_n_-mv-vseg-seek-12_000.m4a"
    );
}

#[tokio::test]
async fn streaming_basic_read_write() {
    let (dir, cache) = new_cache();
    let mut put = cache
        .begin_streaming_put("asset1", "alac")
        .await
        .unwrap()
        .unwrap();
    let want = b"hello from the writer side of the streaming cache";

    let reader = tokio::spawn(read_all(put.entry().reader_at(0)));
    let half = want.len() / 2;
    put.write_all(&want[..half]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    put.write_all(&want[half..]).await.unwrap();
    put.commit().await.unwrap();

    assert_eq!(reader.await.unwrap().unwrap(), want);
    assert!(dir.path().join(filename("asset1", "alac")).is_file());
}

#[tokio::test]
async fn streaming_drop_without_commit_fails_readers() {
    let (dir, cache) = new_cache();
    let mut put = cache
        .begin_streaming_put("asset2", "alac")
        .await
        .unwrap()
        .unwrap();
    let reader = tokio::spawn(read_all(put.entry().reader_at(0)));
    put.write_all(b"partial data").await.unwrap();
    drop(put);

    assert!(matches!(reader.await.unwrap(), Err(Error::Abandoned)));
    let tmp = dir.path().join(filename("asset2", "alac") + TMP_SUFFIX);
    assert!(!tmp.exists(), "temp file still exists after discard");
}

#[tokio::test]
async fn streaming_multiple_readers() {
    let (_dir, cache) = new_cache();
    let mut put = cache
        .begin_streaming_put("asset3", "alac")
        .await
        .unwrap()
        .unwrap();
    let want = vec![b'x'; 4096];
    let readers: Vec<_> = (0..4)
        .map(|_| tokio::spawn(read_all(put.entry().reader_at(0))))
        .collect();
    put.write_all(&want).await.unwrap();
    put.commit().await.unwrap();
    for reader in readers {
        assert_eq!(reader.await.unwrap().unwrap(), want);
    }
}

#[tokio::test]
async fn streaming_reader_at_offset() {
    let (_dir, cache) = new_cache();
    let mut put = cache
        .begin_streaming_put("asset5", "alac")
        .await
        .unwrap()
        .unwrap();
    let data = b"0123456789abcdef";
    let reader = tokio::spawn(read_all(put.entry().reader_at(8)));
    put.write_all(&data[..8]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    put.write_all(&data[8..]).await.unwrap();
    put.commit().await.unwrap();
    assert_eq!(reader.await.unwrap().unwrap(), &data[8..]);
}

#[tokio::test]
async fn get_streaming_tracks_in_progress_writer() {
    let (_dir, cache) = new_cache();
    assert!(cache.get_streaming("asset6", "alac").is_none());

    let mut put = cache
        .begin_streaming_put("asset6", "alac")
        .await
        .unwrap()
        .unwrap();
    put.write_all(b"abc").await.unwrap();
    let entry = cache
        .get_streaming("asset6", "alac")
        .expect("in-progress writer");
    assert_eq!(entry.written(), 3);

    drop(put);
    assert!(cache.get_streaming("asset6", "alac").is_none());
}

#[tokio::test]
async fn in_flight_key_blocks_second_put() {
    let (_dir, cache) = new_cache();
    let first = cache.begin_streaming_put("asset4", "alac").await.unwrap();
    assert!(first.is_some());
    assert!(
        cache
            .begin_streaming_put("asset4", "alac")
            .await
            .unwrap()
            .is_none()
    );
    assert!(cache.begin_put("asset4", "alac").await.unwrap().is_none());
    drop(first);
    assert!(
        cache
            .begin_streaming_put("asset4", "alac")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn seek_past_written_waits_for_data() {
    let (_dir, cache) = new_cache();
    let mut put = cache
        .begin_streaming_put("seek", "alac")
        .await
        .unwrap()
        .unwrap();
    put.write_all(b"0123").await.unwrap();
    let mut reader = put.entry().reader_at(0);
    reader.seek(6);
    let read = tokio::spawn(async move { reader.read(4).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !read.is_finished(),
        "read returned before the offset was written"
    );

    put.write_all(b"456789").await.unwrap();
    let got = timeout(Duration::from_secs(2), read)
        .await
        .expect("read did not wake");
    assert_eq!(got.unwrap().unwrap().unwrap(), &b"6789"[..]);
    put.commit().await.unwrap();
}

#[tokio::test]
async fn reader_if_active_is_none_after_commit() {
    let (_dir, cache) = new_cache();
    let put = cache
        .begin_streaming_put("active", "alac")
        .await
        .unwrap()
        .unwrap();
    let entry = put.entry().clone();
    assert!(entry.reader_if_active().is_some());
    put.commit().await.unwrap();
    assert!(entry.reader_if_active().is_none());
}

#[tokio::test]
async fn committed_entry_reads_through_streaming_path() {
    let (_dir, cache) = new_cache();
    let mut put = cache
        .begin_streaming_put("c", "mv-vseg-seek-0.000")
        .await
        .unwrap()
        .unwrap();
    put.write_all(b"fragments").await.unwrap();
    put.commit().await.unwrap();

    let entry = cache
        .open_committed("c", "mv-vseg-seek-0.000")
        .await
        .unwrap();
    assert_eq!(entry.written(), 9);
    let reader = entry.reader_at(0);
    assert_eq!(reader.size(), Some(9));
    assert_eq!(read_all(reader).await.unwrap(), b"fragments");
}

// While a writer is committing, the key must stay claimed until the rename is done;
// otherwise a new writer can truncate the same .tmp and corrupt the entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_keeps_key_claimed_until_renamed() {
    let want = b"ALAC".repeat(4096);
    for _ in 0..200 {
        let (_dir, cache) = new_cache();
        let mut put = cache
            .begin_streaming_put("a", "alac")
            .await
            .unwrap()
            .unwrap();
        put.write_all(&want).await.unwrap();

        let racer = {
            let cache = cache.clone();
            tokio::spawn(async move {
                loop {
                    if cache.get("a", "alac").await.is_some() {
                        return;
                    }
                    // Claimed the key while the first writer still owned it: dropped at once.
                    if cache
                        .begin_streaming_put("a", "alac")
                        .await
                        .unwrap()
                        .is_some()
                    {
                        return;
                    }
                }
            })
        };
        put.commit().await.unwrap();
        racer.await.unwrap();

        let mut file = cache
            .get("a", "alac")
            .await
            .expect("committed entry missing");
        let mut got = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut got)
            .await
            .unwrap();
        assert_eq!(got, want, "committed entry corrupted: {} bytes", got.len());
    }
}

// Path is how players that open the file themselves use the cache; a hit must count as a
// use, or their favourite tracks look the oldest and are evicted first.
#[tokio::test]
async fn path_hit_refreshes_recency() {
    let (_dir, cache) = new_cache();
    let mut put = cache.begin_put("a", "q").await.unwrap().unwrap();
    put.write_all(b"data").await.unwrap();
    put.commit().await.unwrap();

    let path = cache.path("a", "q").await.unwrap();
    set_mtime(&path, Duration::from_secs(3 * 3600));
    assert!(cache.path("a", "q").await.is_some(), "miss");
    let age = std::fs::metadata(&path)
        .unwrap()
        .modified()
        .unwrap()
        .elapsed()
        .unwrap();
    assert!(
        age < Duration::from_secs(60),
        "a path hit left the entry {age:?} old"
    );
}

// A burst of commits requests many evictions at once: they must not run concurrently
// (each would scan and remove the same files), and the last request must not be lost.
#[test]
fn evict_coalesces_concurrent_requests() {
    let (dir, cache) = new_cache();
    cache.0.limit_bytes.store(100, Ordering::Relaxed);
    for i in 0..6u64 {
        let path = dir.path().join(format!("{i:064}"));
        std::fs::write(&path, [0u8; 60]).unwrap();
        set_mtime(&path, Duration::from_secs((10 - i) * 60));
    }

    std::thread::scope(|s| {
        for _ in 0..30 {
            s.spawn(|| cache.0.evict());
        }
    });

    let total: u64 = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(
        total <= 100,
        "cache is {total} bytes after eviction, limit 100"
    );
    assert!(
        !cache.0.evicting.load(Ordering::Relaxed),
        "eviction flag left set"
    );
    // Oldest first: only the newest file survives.
    assert!(dir.path().join(format!("{:064}", 5)).exists());
}
