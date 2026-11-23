//! Artwork: Apple CDN images fetched on tokio, decoded and down-scaled off the UI thread,
//! cached on disk and in a byte-capped memory LRU, and handed to gpui as `RenderImage`s.
//!
//! Views ask every frame: [`ArtworkStore::image`] returns the image when it is in memory and
//! otherwise queues a load and returns `None` (draw a placeholder); when it arrives the store
//! re-renders exactly the views that asked (batched to one redraw per frame).
//! [`crate::content::ArtworkView`] does all of this for a view. A queued load that no view asked for again within
//! [`STALE_AFTER`] is dropped before it starts, so a fast scroll through 5,000 albums only
//! fetches what stopped on screen.

pub mod disk;

use std::collections::{HashMap, HashSet};

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{App, AppContext, Context, Entity, EntityId, Global, RenderImage, Task, Window};
use image::{Frame, RgbaImage, imageops::FilterType};
use tokio::sync::Semaphore;

use crate::runtime;
pub use disk::DiskCache;

/// A queued load that has not been asked for in this long is abandoned.
pub const STALE_AFTER: Duration = Duration::from_millis(400);

/// Requested sizes snap to these edges (device pixels), so nearby sizes share cache entries.
const SIZE_STEPS: [u32; 7] = [64, 128, 200, 300, 400, 600, 1000];

/// The cached edge for a wanted edge: the next size step up.
fn edge_for(edge_px: f32) -> u32 {
    SIZE_STEPS
        .iter()
        .copied()
        .find(|&s| s as f32 >= edge_px)
        .unwrap_or(SIZE_STEPS[SIZE_STEPS.len() - 1])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreConfig {
    /// Decoded images kept in memory (BGRA bytes).
    pub memory_cap_bytes: usize,
    /// Downloads in flight at once.
    pub max_fetches: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            memory_cap_bytes: 128 << 20,
            max_fetches: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    /// Apple's template URL, with `{w}` and `{h}`.
    template: Arc<str>,
    edge: u32,
}

impl Key {
    /// The square image at this edge: Apple's default crop (`bb`), JPEG.
    fn url(&self) -> String {
        let e = self.edge.to_string();
        self.template
            .replace("{w}", &e)
            .replace("{h}", &e)
            .replace("{c}", "bb")
            .replace("{f}", "jpg")
    }
}

/// Counters for the gallery's frame-time report.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub memory_bytes: usize,
    pub in_memory: usize,
    pub loading: usize,
    pub from_disk: u64,
    pub from_network: u64,
    pub abandoned: u64,
    pub failed: u64,
}

#[derive(Debug, thiserror::Error)]
enum LoadError {
    #[error("abandoned")]
    Abandoned,
    #[error("fetch: {0}")]
    Http(#[from] reqwest::Error),
    #[error("decode: {0}")]
    Decode(#[from] image::ImageError),
}

enum Source {
    Disk,
    Network,
}

/// Shared with the tokio side.
struct Shared {
    http: reqwest::Client,
    disk: Option<Arc<DiskCache>>,
    fetches: Semaphore,
    /// Last time a view asked for each queued key.
    wanted: Mutex<HashMap<Key, Instant>>,
}

pub struct ArtworkStore {
    shared: Arc<Shared>,
    config: StoreConfig,
    /// Each image with the tick it was last asked for.
    images: HashMap<Key, (Arc<RenderImage>, u64)>,
    tick: u64,
    loading: HashMap<Key, Task<()>>,
    /// Evicted images whose GPU atlas entries are freed at the next [`Self::release`].
    evicted: Vec<Arc<RenderImage>>,
    /// A pending notification: arrivals within one frame share one redraw.
    flush: Option<Task<()>>,
    /// The views that asked for each loading image, and those to tell at the next flush.
    waiting: HashMap<Key, Vec<EntityId>>,
    to_notify: HashSet<EntityId>,
    /// When each image was first drawn (for its fade-in).
    shown: HashMap<Key, Instant>,
    stats: Stats,
}

/// The app's artwork store, as a gpui global.
struct GlobalStore(Entity<ArtworkStore>);

impl Global for GlobalStore {}

/// How long arrivals are gathered before observers are told (about one frame).
const NOTIFY_BATCH: Duration = Duration::from_millis(16);

impl ArtworkStore {
    pub fn new(
        http: reqwest::Client,
        disk: Option<Arc<DiskCache>>,
        config: StoreConfig,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            shared: Arc::new(Shared {
                http,
                disk,
                fetches: Semaphore::new(config.max_fetches),
                wanted: Mutex::default(),
            }),
            config,
            images: HashMap::new(),
            tick: 0,
            loading: HashMap::new(),
            evicted: Vec::new(),
            flush: None,
            waiting: HashMap::new(),
            to_notify: HashSet::new(),
            shown: HashMap::new(),
            stats: Stats::default(),
        })
    }

    /// Creates the app's store, which [`crate::content::ArtworkView`] draws from.
    pub fn install(
        http: reqwest::Client,
        disk: Option<Arc<DiskCache>>,
        config: StoreConfig,
        cx: &mut App,
    ) -> Entity<Self> {
        let store = Self::new(http, disk, config, cx);
        cx.set_global(GlobalStore(store.clone()));
        store
    }

    /// The app's store; panics when [`Self::install`] was not called.
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalStore>().0.clone()
    }

    /// The image of `template` (Apple's URL template) at `edge_px` device pixels, for the view
    /// `view` (re-rendered when it arrives), or `None` while it loads, with the time since the
    /// image was first drawn (for its fade-in).
    pub fn image(
        &mut self,
        template: &str,
        edge_px: f32,
        view: EntityId,
        cx: &mut Context<Self>,
    ) -> Option<(Arc<RenderImage>, Duration)> {
        let template: Arc<str> = template.into();
        let key = Key {
            template: template.clone(),
            edge: edge_for(edge_px),
        };
        match self.get(&template, edge_px, cx) {
            Some(image) => {
                if self.shown.len() > 50_000 {
                    self.shown.clear();
                }
                let since = self.shown.entry(key).or_insert_with(Instant::now).elapsed();
                Some((image, since))
            }
            None => {
                let views = self.waiting.entry(key).or_default();
                if !views.contains(&view) {
                    views.push(view);
                }
                None
            }
        }
    }

    /// The image for `template` at `edge_px` device pixels, or `None` while it loads.
    pub fn get(
        &mut self,
        template: &Arc<str>,
        edge_px: f32,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        let key = Key {
            template: template.clone(),
            edge: edge_for(edge_px),
        };
        self.tick += 1;
        if let Some((image, used)) = self.images.get_mut(&key) {
            *used = self.tick;
            return Some(image.clone());
        }
        self.shared
            .wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), Instant::now());
        if !self.loading.contains_key(&key) {
            let job = runtime::spawn(cx, load(self.shared.clone(), key.clone()));
            let task_key = key.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = job.await;
                this.update(cx, |store, cx| {
                    store.loading.remove(&task_key);
                    store
                        .shared
                        .wanted
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&task_key);
                    match result {
                        Ok(Ok((image, source))) => {
                            match source {
                                Source::Disk => store.stats.from_disk += 1,
                                Source::Network => store.stats.from_network += 1,
                            }
                            if let Some(views) = store.waiting.remove(&task_key) {
                                store.to_notify.extend(views);
                            }
                            store.insert(task_key, image);
                            store.notify_soon(cx);
                        }
                        Ok(Err(LoadError::Abandoned)) => {
                            store.waiting.remove(&task_key);
                            store.stats.abandoned += 1;
                        }
                        Ok(Err(e)) => {
                            tracing::debug!("artwork {}: {e}", task_key.url());
                            store.stats.failed += 1;
                        }
                        Err(_) => store.stats.abandoned += 1,
                    }
                })
                .ok();
            });
            self.loading.insert(key, task);
        }
        None
    }

    /// Tells observers once per batch instead of once per image: during a fast scroll dozens of
    /// images land per frame, and each notification would cost a full redraw.
    fn notify_soon(&mut self, cx: &mut Context<Self>) {
        if self.flush.is_some() {
            return;
        }
        self.flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTIFY_BATCH).await;
            this.update(cx, |this, cx| {
                this.flush = None;
                for view in std::mem::take(&mut this.to_notify) {
                    App::notify(cx, view);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn insert(&mut self, key: Key, image: Arc<RenderImage>) {
        self.shown.remove(&key);
        self.stats.memory_bytes += image_bytes(&image);
        self.tick += 1;
        self.images.insert(key, (image, self.tick));
        if self.stats.memory_bytes <= self.config.memory_cap_bytes {
            return;
        }
        // Over the cap: drop the least recently used down to 90 % of it, in one pass, so
        // eviction is rare and `get` stays O(1).
        let mut by_age: Vec<(u64, Key)> = self
            .images
            .iter()
            .map(|(k, (_, used))| (*used, k.clone()))
            .collect();
        by_age.sort_unstable_by_key(|(used, _)| *used);
        let target = self.config.memory_cap_bytes / 10 * 9;
        for (_, old) in by_age {
            if self.stats.memory_bytes <= target {
                break;
            }
            if let Some((image, _)) = self.images.remove(&old) {
                self.stats.memory_bytes -= image_bytes(&image);
                self.evicted.push(image);
            }
        }
    }

    /// Frees the GPU textures of evicted images. Call once per frame from a window that
    /// shows artwork (the root view's render does it).
    pub fn release(&mut self, window: &mut Window) {
        for image in self.evicted.drain(..) {
            window.drop_image(image).ok();
        }
    }

    pub fn stats(&self) -> Stats {
        Stats {
            in_memory: self.images.len(),
            loading: self.loading.len(),
            ..self.stats
        }
    }
}

fn image_bytes(image: &RenderImage) -> usize {
    let size = image.size(0);
    size.width.0 as usize * size.height.0 as usize * 4
}

/// Disk cache, else network; then decode and down-scale on the blocking pool.
async fn load(shared: Arc<Shared>, key: Key) -> Result<(Arc<RenderImage>, Source), LoadError> {
    let still_wanted = || {
        let last = shared
            .wanted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied();
        last.is_some_and(|t| t.elapsed() <= STALE_AFTER)
    };
    let url = key.url();
    let disk_hit = {
        let (shared, url) = (shared.clone(), url.clone());
        tokio::task::spawn_blocking(move || shared.disk.as_ref().and_then(|d| d.get(&url)))
            .await
            .ok()
            .flatten()
    };
    let (bytes, source) = match disk_hit {
        Some(bytes) => (bytes, Source::Disk),
        None => {
            let _permit = shared
                .fetches
                .acquire()
                .await
                .map_err(|_| LoadError::Abandoned)?;
            // Still wanted? A fast scroll leaves many queued keys nobody asks for anymore.
            if !still_wanted() {
                return Err(LoadError::Abandoned);
            }
            let bytes = shared
                .http
                .get(&url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?
                .to_vec();
            if shared.disk.is_some() {
                let (shared, url, bytes) = (shared.clone(), url.clone(), bytes.clone());
                // Fire and forget: the image is shown whether or not the write succeeds.
                tokio::task::spawn_blocking(move || {
                    if let Some(Err(e)) = shared.disk.as_ref().map(|d| d.put(&url, &bytes)) {
                        tracing::debug!("artwork cache write: {e}");
                    }
                });
            }
            (bytes, Source::Network)
        }
    };
    // Decoding is the costly part: skip it too for images scrolled past while waiting.
    if !still_wanted() {
        return Err(LoadError::Abandoned);
    }
    let edge = key.edge;
    let image = tokio::task::spawn_blocking(move || decode(&bytes, edge))
        .await
        .map_err(|_| LoadError::Abandoned)??;
    Ok((image, source))
}

/// Decodes and fits the image into `edge`×`edge`, as BGRA (what gpui uploads).
pub fn decode(bytes: &[u8], edge: u32) -> Result<Arc<RenderImage>, image::ImageError> {
    let decoded = image::load_from_memory(bytes)?;
    let scaled = if decoded.width() > edge || decoded.height() > edge {
        decoded.resize(edge, edge, FilterType::Triangle)
    } else {
        decoded
    };
    Ok(Arc::new(to_render_image(scaled.into_rgba8())))
}

/// Wraps RGBA pixels as a gpui image (converted to BGRA in place).
pub fn to_render_image(mut rgba: RgbaImage) -> RenderImage {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    RenderImage::new([Frame::new(rgba)])
}
