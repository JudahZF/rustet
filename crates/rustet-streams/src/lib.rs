//! AAC and music-video (Widevine) streams: segment download, key acquisition, sample decryption, fragment indexing
//! and the licence provider.
//!
//! [`Streams`] is the one owned context of this path. The engine creates it once and shares it with `Arc`; it holds
//! the HTTP clients, the Widevine identity, the content-key cache, the two segment disk caches and the seek-priority
//! gate. Everything else hangs off it:
//!
//! - [`license`]: the licence provider ([`Streams::open_licence`]), the decryptors and the pipeline sources.
//! - [`Streams::download_segments_parallel`] / [`Streams::download_mv_segments_streaming`]: ordered concurrent
//!   segment downloads.
//! - [`decrypt_mp4_streaming`] / [`passthrough_streaming`]: the fMP4 rewriting stages.
//! - [`Streams::plan_direct_seek`]: fragment-level seeks into a music-video segment.
//! - [`MvLiveIndex`]: the fragment index of a growing music-video file.
#![forbid(unsafe_code)]

mod cache;
mod direct;
mod download;
mod error;
mod keys;
pub mod license;
mod mvindex;
mod priority;
mod stream;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustet_widevine::Identity;
use rustet_widevine::wvkey::ContentKey;
use tokio::sync::OnceCell;
use tracing::debug;

use cache::{MvDecCache, SegmentCache};
use priority::BackgroundGate;

pub use direct::DirectPlan;
pub use download::stable_cache_key;
pub use error::Error;
pub use mvindex::{MvFragEntry, MvLiveIndex, VsegFragTiming};
pub use priority::{BackgroundHold, with_seek_priority};
pub use rustet_widevine::wvkey::LicenceRequest;
pub use stream::{decrypt_mp4_streaming, passthrough_streaming};

/// Default capacity of the audio segment cache (512 MiB).
pub const CACHE_MAX_BYTES: i64 = 512 * 1024 * 1024;
/// Default capacity of the music-video segment cache (2 GiB).
pub const DEFAULT_MV_CACHE_MAX_BYTES: i64 = 2 * 1024 * 1024 * 1024;

/// Cache directories, relative to the Rustet cache directory (and, for migration, to the OS cache root).
const SEGMENTS_DIR: &str = "engine/segments";
const MV_SEGMENTS_DIR: &str = "engine/mv-segments";
const MV_DEC_DIR: &str = "engine/mv-dec";

/// The AAC / music-video stream context. Create one per engine and share it with `Arc`.
pub struct Streams {
    /// The shared engine client: licence requests and the licence pool warm-up.
    licence_client: reqwest::Client,
    /// The CDN client for segments (see [`cdn_client`]).
    cdn: reqwest::Client,
    licence_url: String,
    /// Loaded on the first licence request, then kept.
    identity: OnceCell<Arc<Identity>>,
    /// Content keys by `kid \0 uri_prefix`. Process-scoped and never written to disk: a cached key skips the CDM
    /// round trip when the same track opens again (pause → resume).
    keys: Mutex<std::collections::HashMap<String, ContentKey>>,
    segments: Arc<SegmentCache>,
    mv_segments: Arc<SegmentCache>,
    mv_dec: Arc<MvDecCache>,
    gate: BackgroundGate,
    /// Stable keys of music-video segments being completed in the background after a cancelled download.
    mv_background: Mutex<std::collections::HashSet<String>>,
}

impl Streams {
    /// A context with the caches under the Rustet cache directory (`~/.cache/rustet/engine/...`). The cache directories
    /// are created on first use. `client` is the shared engine client, used
    /// for licence requests; segments use a dedicated IPv4-only CDN client without transparent decompression.
    pub fn new(client: reqwest::Client) -> Self {
        Self::with_cache_root(client, rustet_core::appdir::cache_dir())
    }

    /// A context whose caches live under `root`. Tests pass a temporary `root`, so they never touch the user's
    /// cache.
    pub fn with_cache_root(client: reqwest::Client, root: &Path) -> Self {
        let cache = |dir: &str, max| Arc::new(SegmentCache::new(root.join(dir), max));
        Self {
            licence_client: client,
            cdn: cdn_client(),
            licence_url: rustet_widevine::wvkey::LICENSE_URL.to_owned(),
            identity: OnceCell::new(),
            keys: Mutex::default(),
            segments: cache(SEGMENTS_DIR, CACHE_MAX_BYTES),
            mv_segments: cache(MV_SEGMENTS_DIR, DEFAULT_MV_CACHE_MAX_BYTES),
            mv_dec: Arc::new(MvDecCache::new(root.join(MV_DEC_DIR))),
            gate: BackgroundGate::new(),
            mv_background: Mutex::default(),
        }
    }

    /// A context for tests: caches in `root`, licence requests to `licence_url`, the built-in identity.
    #[cfg(test)]
    pub(crate) fn for_tests(root: &Path, licence_url: &str) -> Self {
        let mut s = Self::with_cache_root(reqwest::Client::new(), root);
        s.licence_url = licence_url.to_owned();
        s.identity = OnceCell::new_with(Some(Arc::new(Identity::embedded())));
        s
    }

    // ── Caches ──────────────────────────────────────────────────────────────

    /// Loads the in-memory LRU indexes of both segment caches from the files on disk, in the background. Call once
    /// at startup.
    pub fn warm_caches(&self) {
        for cache in [&self.segments, &self.mv_segments] {
            let cache = Arc::clone(cache);
            tokio::task::spawn_blocking(move || cache.warm_from_disk());
        }
    }

    /// Sets the music-video cache capacity. 0 (or a negative value) disables it.
    pub fn set_mv_cache_max_bytes(&self, n: i64) {
        self.mv_segments.set_max_bytes(n);
    }

    pub fn mv_cache_enabled(&self) -> bool {
        self.mv_segments.enabled()
    }

    pub fn mv_cache_max_bytes(&self) -> i64 {
        self.mv_segments.max_bytes()
    }

    /// On-disk usage of the music-video segment cache plus the legacy decrypted-MV cache. The first call counts the
    /// legacy directory (a handful of files), so it can block briefly.
    pub fn mv_cache_total_bytes(&self) -> i64 {
        self.mv_segments.total_bytes() + self.mv_dec.total_bytes()
    }

    /// Deletes the audio segment cache. Music-video and lossless caches are untouched.
    pub async fn clear_segment_cache(&self) -> std::io::Result<()> {
        let cache = Arc::clone(&self.segments);
        blocking(move || cache.clear()).await
    }

    /// Deletes the music-video segment cache and the legacy decrypted-MV cache.
    pub async fn clear_mv_cache(&self) -> std::io::Result<()> {
        let (segments, dec) = (Arc::clone(&self.mv_segments), Arc::clone(&self.mv_dec));
        blocking(move || {
            segments.clear()?;
            dec.clear()
        })
        .await
    }

    // ── Seek priority ───────────────────────────────────────────────────────

    /// Parks music-video downloads that do not run under [`with_seek_priority`] until the hold is released (or
    /// dropped), or `max_hold` elapses. Holds nest.
    pub fn hold_background(&self, max_hold: Duration) -> BackgroundHold {
        self.gate.hold(max_hold)
    }
}

/// Runs blocking file I/O off the runtime workers.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// The CDN client. It differs from the shared engine client on purpose:
/// - IPv4 only: Apple's CDN publishes AAAA records that are unreachable on many Linux machines, which otherwise
///   fail with "network is unreachable".
/// - No transparent decompression: segment bytes and byte ranges must arrive exactly as stored.
/// - No overall timeout: a 14 MB segment at 2 Mbit/s takes about a minute. The response-header timeout is applied
///   per request; body reads are bounded by the caller's cancellation.
fn cdn_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(Duration::from_secs(90))
        .no_gzip()
        .dns_resolver(Ipv4Only)
        .build()
        .expect("the CDN HTTP client builds")
}

/// Resolves host names to their IPv4 addresses only.
struct Ipv4Only;

impl reqwest::dns::Resolve for Ipv4Only {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(SocketAddr::is_ipv4)
                .collect();
            if addrs.is_empty() {
                debug!(%host, "no IPv4 address");
                return Err(format!("{host}: no IPv4 address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Where the caches of a context live, for diagnostics.
impl std::fmt::Debug for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streams")
            .field("segments", &self.segments)
            .field("mv_segments", &self.mv_segments)
            .finish_non_exhaustive()
    }
}
