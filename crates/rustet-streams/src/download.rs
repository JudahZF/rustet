//! Segment downloads from Apple's CDN: retries, the segment caches, and ordered concurrent downloads.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt};
use reqwest::StatusCode;
use reqwest::header::RANGE;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::priority::has_seek_priority;
use crate::{Error, Streams, blocking};

/// Attempts per request; a failed attempt waits 500 ms, then 1 s.
const MAX_ATTEMPTS: u32 = 3;
/// Guards against hung connections. Body reads are bounded by the caller's cancellation instead.
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);
/// Bytes read per segment at most; the rest of a larger body is dropped.
const SEGMENT_CAP: usize = 50 << 20;
/// Lifetime of a background download that completes a cancelled music-video segment.
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How many music-video segment connections may be open at once. More connections do not make the download
/// faster: the CDN link is shared and one connection already saturates it (measured). The second one only hides
/// the per-segment request latency: with one slot the next request starts only after the current body drained,
/// leaving the link idle for about a second at every boundary. An undrained body buffers little (its socket window
/// stays small until it is read).
const MV_LOOKAHEAD: usize = 2;

/// The cache key of a segment URL: the URL without its query (Apple's CDN `accessKey` rotates per session), with
/// any `#bytes=` range kept. Replays then hit the cache under a fresh signed URL, as Android's MediaHlsAssetCache
/// keys by file and byte range.
pub fn stable_cache_key(url: &str) -> String {
    let (base, range) = match url.find("#bytes=") {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    let base = base.split_once('?').map_or(base, |(b, _)| b);
    format!("{base}{range}")
}

/// Splits a `<url>#bytes=<first>-<last>` playlist URL into the URL to fetch and its Range header.
fn split_range(url: &str) -> (&str, Option<String>) {
    match url.split_once("#bytes=") {
        Some((fetch, range)) => (fetch, Some(format!("bytes={range}"))),
        None => (url, None),
    }
}

/// A byte-range request must be answered with 206: a 200 carries the whole file, which would otherwise be streamed
/// and cached as if it were the requested segment.
fn check_status(status: StatusCode, range: Option<&str>) -> Result<(), Error> {
    match (range, status) {
        (Some(_), StatusCode::PARTIAL_CONTENT) | (None, StatusCode::OK) => Ok(()),
        (Some(range), StatusCode::OK) => Err(Error::RangeIgnored(range.to_owned())),
        (_, status) => Err(Error::Status(status.as_u16())),
    }
}

/// Runs `fut` until it completes or `cancel` fires.
pub(crate) async fn cancellable<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        r = fut => r,
    }
}

/// Runs `attempt` up to [`MAX_ATTEMPTS`] times with exponential backoff. Wrap it in [`cancellable`]: cancellation
/// also cuts the backoff short.
async fn with_retries<T, Fut: Future<Output = Result<T, Error>>>(
    what: &str,
    mut attempt: impl FnMut() -> Fut,
) -> Result<T, Error> {
    let mut n = 0;
    loop {
        match attempt().await {
            Ok(v) => return Ok(v),
            Err(e) if n + 1 < MAX_ATTEMPTS && e.is_retryable() => {
                debug!(what, attempt = n, error = %e, "segment request failed, retrying");
                tokio::time::sleep(Duration::from_millis(500 << n)).await;
                n += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Reads a body up to [`SEGMENT_CAP`] bytes.
async fn read_capped(mut resp: reqwest::Response) -> Result<Bytes, Error> {
    let mut data = BytesMut::new();
    while let Some(chunk) = resp.chunk().await.map_err(Error::Body)? {
        let take = chunk.len().min(SEGMENT_CAP - data.len());
        data.extend_from_slice(&chunk[..take]);
        if data.len() == SEGMENT_CAP {
            break;
        }
    }
    Ok(data.freeze())
}

/// One music-video segment whose download is under way, as handed from a prefetch task to the writer.
enum Prefetched {
    Cached(Bytes),
    /// Headers have arrived; the body is still flowing. The permit holds one of the [`MV_LOOKAHEAD`] connection
    /// slots until the writer drained the body.
    Open {
        resp: reqwest::Response,
        key: String,
        slot: OwnedSemaphorePermit,
    },
}

impl Streams {
    /// One GET with the header timeout and the status check.
    pub(crate) async fn request(
        &self,
        url: &str,
        range: Option<&str>,
    ) -> Result<reqwest::Response, Error> {
        let mut req = self.cdn.get(url);
        if let Some(range) = range {
            req = req.header(RANGE, range);
        }
        let resp = tokio::time::timeout(HEADER_TIMEOUT, req.send())
            .await
            .map_err(|_| Error::HeaderTimeout)?
            .map_err(Error::Fetch)?;
        check_status(resp.status(), range)?;
        Ok(resp)
    }

    /// Opens `url` with an optional Range header, with retries, up to the response headers.
    pub(crate) async fn open_range(
        &self,
        url: &str,
        range: Option<&str>,
    ) -> Result<reqwest::Response, Error> {
        with_retries(url, || self.request(url, range)).await
    }

    /// Opens a playlist URL (`#bytes=` ranges allowed) with retries, up to the response headers.
    async fn open(&self, url: &str) -> Result<reqwest::Response, Error> {
        let (fetch, range) = split_range(url);
        self.open_range(fetch, range.as_deref()).await
    }

    /// Downloads a whole segment with retries; a failed body read is retried too.
    async fn fetch_bytes(&self, url: &str) -> Result<Bytes, Error> {
        let (fetch, range) = split_range(url);
        with_retries(url, || async {
            read_capped(self.request(fetch, range.as_deref()).await?).await
        })
        .await
    }

    pub(crate) async fn cached_audio(&self, key: &str) -> Option<Bytes> {
        let (cache, key) = (Arc::clone(&self.segments), key.to_owned());
        blocking(move || cache.get(&key)).await.map(Bytes::from)
    }

    pub(crate) async fn cached_mv(&self, key: &str) -> Option<Bytes> {
        let (cache, key) = (Arc::clone(&self.mv_segments), key.to_owned());
        blocking(move || cache.get(&key)).await.map(Bytes::from)
    }

    async fn store_audio(&self, key: &str, data: Bytes) {
        let (cache, key) = (Arc::clone(&self.segments), key.to_owned());
        blocking(move || cache.put(&key, &data)).await;
    }

    async fn store_mv(&self, key: &str, data: Bytes) {
        if self.mv_segments.enabled() {
            let (cache, key) = (Arc::clone(&self.mv_segments), key.to_owned());
            blocking(move || cache.put(&key, &data)).await;
        }
    }

    /// An audio segment, from the cache or the CDN (then cached).
    pub(crate) async fn fetch_segment(
        &self,
        url: &str,
        cancel: &CancellationToken,
    ) -> Result<Bytes, Error> {
        let key = stable_cache_key(url);
        if let Some(cached) = self.cached_audio(&key).await {
            return Ok(cached);
        }
        let data = cancellable(cancel, self.fetch_bytes(url)).await?;
        self.store_audio(&key, data.clone()).await;
        Ok(data)
    }

    /// Fetches up to `concurrency` audio segments at once and writes them to `w` strictly in playlist order. A
    /// segment found in the cache flows to `w` without waiting for slower network segments before it to start.
    /// At most `concurrency` segments are in flight or waiting to be written, so memory stays bounded.
    pub async fn download_segments_parallel(
        &self,
        urls: &[String],
        w: &mut (dyn AsyncWrite + Send + Unpin),
        concurrency: usize,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        self.download_audio_segments(urls, None, w, concurrency, cancel)
            .await
    }

    /// [`Self::download_segments_parallel`]; with `init_duration_ms`, `urls[0]` is the init segment and gets the
    /// track length in its moov ([`crate::stream::patch_init_duration`]).
    pub(crate) async fn download_audio_segments(
        &self,
        urls: &[String],
        init_duration_ms: Option<u64>,
        w: &mut (dyn AsyncWrite + Send + Unpin),
        concurrency: usize,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        debug!(
            segments = urls.len(),
            concurrency,
            first = urls.first().map(String::as_str),
            "download segments"
        );
        // Indexes, not `urls.iter()`: a closure taking a borrowed argument trips rustc's Send check of the caller.
        let fetches = (0..urls.len()).map(|index| self.fetch_logged(index, &urls[index], cancel));
        let mut segments = futures::stream::iter(fetches).buffered(concurrency.max(1));
        let mut init_duration_ms = init_duration_ms;
        while let Some(data) = cancellable(cancel, segments.try_next()).await? {
            let data = match init_duration_ms.take() {
                Some(ms) => crate::stream::patch_init_duration(data, ms),
                None => data,
            };
            w.write_all(&data).await?;
        }
        Ok(())
    }

    async fn fetch_logged(
        &self,
        index: usize,
        url: &str,
        cancel: &CancellationToken,
    ) -> Result<Bytes, Error> {
        let t0 = Instant::now();
        let res = self.fetch_segment(url, cancel).await;
        match &res {
            Ok(d) => debug!(index, size = d.len(), elapsed = ?t0.elapsed(), "segment arrived"),
            Err(e) => debug!(index, elapsed = ?t0.elapsed(), error = %e, "segment failed"),
        }
        res.map_err(|e| Error::Segment {
            index,
            source: Box::new(e),
        })
    }

    /// Copies a response body to `w` as it arrives, pausing while a background hold is active (unless `priority`):
    /// a paused read lets the receive window close, so the connection stops taking bandwidth. With `tee` the bytes
    /// are also collected, for the cache.
    pub(crate) async fn copy_body(
        &self,
        mut resp: reqwest::Response,
        w: &mut (dyn AsyncWrite + Send + Unpin),
        mut tee: Option<&mut BytesMut>,
        priority: bool,
        cancel: &CancellationToken,
    ) -> Result<u64, Error> {
        let mut n = 0;
        loop {
            self.gate.wait(priority, cancel).await?;
            let Some(chunk) =
                cancellable(cancel, async { resp.chunk().await.map_err(Error::Body) }).await?
            else {
                return Ok(n);
            };
            w.write_all(&chunk).await?;
            if let Some(tee) = tee.as_deref_mut() {
                tee.extend_from_slice(&chunk);
            }
            n += chunk.len() as u64;
        }
    }

    /// Streams one music-video segment to `w` as its bytes arrive, through the MV cache. There is no retry once the
    /// body started: the partial bytes are already in `w`, and a retry would send `[partial][full]`, which MSE
    /// cannot decode.
    async fn stream_mv_segment(
        &self,
        url: &str,
        w: &mut (dyn AsyncWrite + Send + Unpin),
        priority: bool,
        cancel: &CancellationToken,
    ) -> Result<u64, Error> {
        let key = stable_cache_key(url);
        if let Some(cached) = self.cached_mv(&key).await {
            debug!(
                key = short_key(&key),
                len = cached.len(),
                "mv segment cache hit"
            );
            w.write_all(&cached).await?;
            return Ok(cached.len() as u64);
        }
        let resp = cancellable(cancel, self.open(url)).await?;
        let mut buf = self.mv_segments.enabled().then(BytesMut::new);
        let n = self
            .copy_body(resp, w, buf.as_mut(), priority, cancel)
            .await?;
        if let Some(buf) = buf {
            self.store_mv(&key, buf.freeze()).await;
        }
        Ok(n)
    }

    /// Streams music-video segments to `w` in order. Segment 0 (the init segment) streams on its own, with no other
    /// download competing for it. The rest use at most `min(prefetch, 2)` open connections: the next segment's
    /// request is issued while the current body still drains, and every body streams to `w` as it arrives, so the
    /// output grows continuously.
    ///
    /// Downloads made outside [`with_seek_priority`](crate::with_seek_priority) pause while a background hold is
    /// active. Cancellation (or dropping the future) stops every download at once.
    pub async fn download_mv_segments_streaming(
        self: &Arc<Self>,
        urls: &[String],
        w: &mut (dyn AsyncWrite + Send + Unpin),
        prefetch: usize,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        debug!(
            segments = urls.len(),
            prefetch,
            first = urls.first().map(String::as_str),
            "download mv segments"
        );
        let Some((first, rest)) = urls.split_first() else {
            return Ok(());
        };
        let priority = has_seek_priority();
        // Ends the prefetch tasks however this function returns.
        let cancel = cancel.child_token();
        let _stop = cancel.clone().drop_guard();

        let t0 = Instant::now();
        self.stream_mv_segment(first, w, priority, &cancel)
            .await
            .map_err(|e| Error::Segment {
                index: 0,
                source: Box::new(e),
            })?;
        let lookahead = prefetch.clamp(1, MV_LOOKAHEAD);
        debug!(elapsed = ?t0.elapsed(), lookahead, "mv segment 0 done");
        if rest.is_empty() {
            return Ok(());
        }

        // The launcher queues one task per segment in playlist order; the queue and the connection slots both hold
        // `lookahead`, so at most that many segments are open or waiting.
        let (tx, mut rx) = mpsc::channel::<JoinHandle<Result<Prefetched, Error>>>(lookahead);
        let slots = Arc::new(Semaphore::new(lookahead));
        // It ends by itself once the writer is gone: the queue closes and the slots free up as the cancelled
        // prefetch tasks end.
        let launcher = {
            let (streams, cancel, urls) = (Arc::clone(self), cancel.clone(), rest.to_vec());
            tokio::spawn(async move {
                for url in urls {
                    let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                        return;
                    };
                    let task = tokio::spawn(Arc::clone(&streams).prefetch_mv(
                        url,
                        slot,
                        priority,
                        cancel.clone(),
                    ));
                    if tx.send(task).await.is_err() {
                        return;
                    }
                }
            })
        };

        let result = async {
            for index in 1..urls.len() {
                let task = cancellable(&cancel, async { Ok(rx.recv().await) })
                    .await?
                    .ok_or(Error::Cancelled)?;
                let prefetched = cancellable(&cancel, async {
                    task.await
                        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
                })
                .await
                .map_err(|e| Error::Segment {
                    index,
                    source: Box::new(e),
                })?;
                let t1 = Instant::now();
                let n = match prefetched {
                    Prefetched::Cached(data) => {
                        self.gate.wait(priority, &cancel).await?;
                        w.write_all(&data).await?;
                        data.len() as u64
                    }
                    Prefetched::Open { resp, key, slot } => {
                        let mut buf = self.mv_segments.enabled().then(BytesMut::new);
                        let n = self
                            .copy_body(resp, w, buf.as_mut(), priority, &cancel)
                            .await
                            .map_err(|e| Error::Segment {
                                index,
                                source: Box::new(e),
                            })?;
                        // Frees the connection slot before the (blocking) cache write.
                        drop(slot);
                        if let Some(buf) = buf.filter(|b| !b.is_empty()) {
                            self.store_mv(&key, buf.freeze()).await;
                        }
                        n
                    }
                };
                debug!(index, size = n, elapsed = ?t1.elapsed(), "mv segment streamed");
            }
            Ok(())
        }
        .await;
        launcher.abort();
        result
    }

    /// Opens one prefetched music-video segment: waits out a background hold, then serves it from the cache or
    /// opens the connection (retrying up to the headers only).
    async fn prefetch_mv(
        self: Arc<Self>,
        url: String,
        slot: OwnedSemaphorePermit,
        priority: bool,
        cancel: CancellationToken,
    ) -> Result<Prefetched, Error> {
        // A seek producer is fetching the segment the player waits on: open no connection against the same CDN
        // bandwidth meanwhile.
        self.gate.wait(priority, &cancel).await?;
        let key = stable_cache_key(&url);
        if let Some(cached) = self.cached_mv(&key).await {
            return Ok(Prefetched::Cached(cached));
        }
        match cancellable(&cancel, self.open(&url)).await {
            Ok(resp) => Ok(Prefetched::Open { resp, key, slot }),
            Err(e) => {
                debug!(url, error = %e, "mv prefetch failed");
                Err(e)
            }
        }
    }

    /// A whole music-video segment (an init segment), through the MV cache. When `cancel` has already fired, the
    /// download is completed in the background so the next attempt finds it in the cache.
    pub(crate) async fn fetch_mv_segment(
        self: &Arc<Self>,
        url: &str,
        cancel: &CancellationToken,
    ) -> Result<Bytes, Error> {
        let key = stable_cache_key(url);
        if let Some(cached) = self.cached_mv(&key).await {
            debug!(
                key = short_key(&key),
                len = cached.len(),
                "mv segment cache hit"
            );
            return Ok(cached);
        }
        if cancel.is_cancelled() {
            self.complete_in_background(key, url.to_owned());
            return Err(Error::Cancelled);
        }
        let data = cancellable(cancel, self.fetch_bytes(url)).await?;
        self.store_mv(&key, data.clone()).await;
        Ok(data)
    }

    /// Downloads a segment whose session was cancelled, with its own 10-minute deadline, unless one is already
    /// under way for the same key.
    fn complete_in_background(self: &Arc<Self>, key: String, url: String) {
        if !self.lock_background().insert(key.clone()) {
            return;
        }
        let streams = Arc::clone(self);
        tokio::spawn(async move {
            let (fetch, range) = split_range(&url);
            let res = tokio::time::timeout(BACKGROUND_TIMEOUT, async {
                read_capped(streams.request(fetch, range.as_deref()).await?).await
            })
            .await;
            match res {
                Ok(Ok(data)) => {
                    debug!(
                        key = short_key(&key),
                        len = data.len(),
                        "mv background download cached"
                    );
                    streams.store_mv(&key, data).await;
                }
                Ok(Err(e)) => {
                    warn!(key = short_key(&key), error = %e, "mv background download failed")
                }
                Err(_) => warn!(key = short_key(&key), "mv background download timed out"),
            }
            streams.lock_background().remove(&key);
        });
    }

    fn lock_background(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<String>> {
        self.mv_background.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The tail of a cache key, for log lines.
fn short_key(k: &str) -> &str {
    match k.char_indices().rev().nth(19) {
        Some((i, _)) if i > 0 => &k[i..],
        _ => k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_cache_key_drops_query_and_keeps_range() {
        for (input, want) in [
            ("https://cdn/f.m4a", "https://cdn/f.m4a"),
            ("https://cdn/f.m4a?accessKey=NEW123", "https://cdn/f.m4a"),
            (
                "https://cdn/f.m4a#bytes=0-1000",
                "https://cdn/f.m4a#bytes=0-1000",
            ),
            (
                "https://cdn/f.m4a?accessKey=X#bytes=100-200",
                "https://cdn/f.m4a#bytes=100-200",
            ),
            (
                "https://cdn/f.m4a?k=Y#bytes=200-300",
                "https://cdn/f.m4a#bytes=200-300",
            ),
        ] {
            assert_eq!(stable_cache_key(input), want, "{input}");
        }
        // The same bytes under two signed URLs share a key; two ranges of one file do not.
        assert_eq!(
            stable_cache_key("https://cdn/v.mp4?accessKey=1#bytes=0-99"),
            stable_cache_key("https://cdn/v.mp4?accessKey=3#bytes=0-99")
        );
        assert_ne!(
            stable_cache_key("https://cdn/v.mp4?accessKey=1#bytes=0-99"),
            stable_cache_key("https://cdn/v.mp4?accessKey=2#bytes=100-199")
        );
    }

    #[test]
    fn range_status_rules() {
        assert!(check_status(StatusCode::PARTIAL_CONTENT, Some("bytes=0-9")).is_ok());
        assert!(check_status(StatusCode::OK, None).is_ok());
        assert!(matches!(
            check_status(StatusCode::OK, Some("bytes=0-9")),
            Err(Error::RangeIgnored(_))
        ));
        assert!(matches!(
            check_status(StatusCode::PARTIAL_CONTENT, None),
            Err(Error::Status(206))
        ));
        assert!(matches!(
            check_status(StatusCode::NOT_FOUND, None),
            Err(Error::Status(404))
        ));
    }

    #[test]
    fn short_key_keeps_the_tail() {
        assert_eq!(short_key("short"), "short");
        assert_eq!(
            short_key("0123456789abcdefghijKLMN"),
            "456789abcdefghijKLMN"
        );
    }
}
