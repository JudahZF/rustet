//! The export queue and the job pipeline: resolve → download → tag → move.
//!
//! The manager acquires, decrypts, tags and writes media. It knows nothing about HTTP: the
//! engine routes requests to it and forwards its events to SSE clients.

use std::collections::HashMap;
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::FutureExt;
use rustet_ampapi::lyrics::{LyricsFormat, LyricsRequest, ttml_to_lrc, ttml_to_srt, ttml_to_vtt};
use rustet_core::media::OpenRequest;
use rustet_core::pipeline::{Sink, StreamContext, StreamKind};
use rustet_core::pq::Queue;
use rustet_playback::{OpenError, StreamClass, StreamError};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

use crate::cache::{AudioCache, audio_qualifier};
use crate::ffmpeg::{self, with_suffix};
use crate::fs::{self, Confinement, MAX_RENAME_ATTEMPTS, OutputDirError};
use crate::job::{
    ExportCapabilities, ExportEvent, ExportJob, ExportRequest, Phase, Source, lyrics_ext, now,
};
use crate::tagger::{self, DEFAULT_ARTWORK_SIZE, TrackMeta};
use crate::template::{self, TemplateVars};
use crate::throttle::BwController;
use crate::tool::{self, ToolError, ToolTrustError};
use crate::{EventSink, MoreRoots, path};

/// How long [`Manager::stop`] waits for running exports. Cancelled jobs stop their tools
/// (SIGTERM, then a kill after the tool grace period), so this stays under the shell's kill
/// delay.
const STOP_WAIT: Duration = Duration::from_millis(2500);

/// Configures a [`Manager`]. The default is valid.
#[derive(Default)]
pub struct Options {
    /// Receives every job event.
    pub events: Option<EventSink>,
    /// Lets audio exports reuse bytes playback fetched.
    pub cache: Option<Arc<dyn AudioCache>>,
    /// The minimum export rate (bytes/s) while playback streams; 0 selects 128 KiB/s.
    pub floor_bps: u64,
    /// When not empty, a request's output folder must lie inside one of these (compared after
    /// resolving symlinks). Empty means unrestricted, which is only right when every caller is
    /// trusted; the HTTP server sets it, because request bodies come from a web page.
    pub output_roots: Vec<PathBuf>,
    /// Roots added after start-up (the folder picked in a native dialog), consulted on every
    /// check. Must read from a source the web page cannot write.
    pub more_roots: Option<MoreRoots>,
}

/// Why [`Manager::enqueue`] refused a request. The text is the HTTP 400 body.
#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("assetId is required")]
    MissingAssetId,
    #[error("options.ffmpegPath must name an ffmpeg binary")]
    FfmpegName,
    #[error("options.vlcPath must name a vlc or cvlc binary")]
    VlcName,
    #[error("options.ffmpegPath: {0}")]
    FfmpegUntrusted(#[source] ToolTrustError),
    #[error("options.vlcPath: {0}")]
    VlcUntrusted(#[source] ToolTrustError),
    #[error(transparent)]
    OutputDir(#[from] OutputDirError),
    #[error("export queue is closed")]
    Closed,
}

/// Why [`Manager::prioritize`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PrioritizeError {
    /// HTTP 404.
    #[error("job not found")]
    NotFound,
    /// The job is running or finished (HTTP 409).
    #[error("job is not queued")]
    NotQueued,
}

/// Why a job failed. The text is the job's `error`.
#[derive(Debug, thiserror::Error)]
enum JobError {
    #[error("cancelled")]
    Cancelled,
    #[error("music video {id} not found in {storefront}{cause}")]
    MusicVideoNotFound {
        id: String,
        storefront: String,
        cause: Cause,
    },
    #[error("song {id} not found in {storefront}{cause}")]
    SongNotFound {
        id: String,
        storefront: String,
        cause: Cause,
    },
    #[error("playlist {id}{cause}")]
    Playlist { id: String, cause: Cause },
    #[error("library playlist {id}{cause}")]
    LibraryPlaylist { id: String, cause: Cause },
    #[error("filename template escapes output directory")]
    Escapes,
    #[error("mkdir {dir}: {source}")]
    Mkdir { dir: String, source: io::Error },
    #[error("create temp: {0}")]
    CreateTemp(#[source] io::Error),
    #[error("open session: {0}")]
    Open(#[source] OpenError),
    #[error("stream: {0}")]
    Stream(#[source] StreamError),
    #[error("stream video: {0}")]
    StreamVideo(#[source] StreamError),
    #[error("stream audio: {0}")]
    StreamAudio(#[source] StreamError),
    #[error("mux video+audio: {0} (ffmpeg required)")]
    Mux(#[source] ToolError),
    #[error("move to {dest}: {source}")]
    Move { dest: String, source: io::Error },
    #[error("{0}")]
    Panic(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// An optional underlying error, shown as `: <error>`.
#[derive(Debug, Default)]
struct Cause(Option<String>);

impl Cause {
    fn of(e: impl std::fmt::Display) -> Self {
        Self(Some(e.to_string()))
    }
}

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(e) => write!(f, ": {e}"),
            None => Ok(()),
        }
    }
}

/// Enqueues and runs export jobs one at a time: highest priority first, FIFO among equals.
///
/// One worker is deliberate: the DRM backend is one process, and one active download keeps the
/// bandwidth limiter's meaning simple (the maximum background egress rate). Post-processing
/// (tagging, conversion, the move) runs concurrently with the next download.
///
/// Dropping the manager cancels every job; [`Manager::stop`] also waits for them.
pub struct Manager {
    inner: Arc<Inner>,
}

struct Inner {
    jobs: Mutex<HashMap<String, Entry>>,
    queue: Queue<Arc<WorkItem>>,
    events: Option<EventSink>,
    playback: Arc<rustet_playback::Manager>,
    catalog: rustet_ampapi::Client,
    http: reqwest::Client,
    cache: Option<Arc<dyn AudioCache>>,
    bw: Arc<BwController>,
    confinement: Confinement,
    /// Enqueue counter for `queue_pos`.
    seq: AtomicI64,
    /// Parent of every job's token.
    shutdown: CancellationToken,
    /// The worker and every post-process task.
    tasks: TaskTracker,
    #[cfg(test)]
    hook: Mutex<Option<tests::Hook>>,
}

struct Entry {
    job: ExportJob,
    /// The original request without credentials, for retry. Finished jobs can stay in the list
    /// indefinitely, and a retry gets fresh tokens anyway.
    request: ExportRequest,
    cancel: CancellationToken,
}

/// A queued job with its credentials.
struct WorkItem {
    id: String,
    req: ExportRequest,
    cancel: CancellationToken,
}

impl Manager {
    /// A manager that acquires media through `playback`, catalog data and artwork through
    /// `http`. Starts the worker, so it must be called inside a tokio runtime.
    pub fn new(
        playback: Arc<rustet_playback::Manager>,
        http: reqwest::Client,
        opts: Options,
    ) -> Self {
        let stats = {
            let pm = Arc::downgrade(&playback);
            Arc::new(move || {
                pm.upgrade()
                    .map(|pm| pm.foreground_stats())
                    .unwrap_or_default()
            })
        };
        let inner = Arc::new(Inner {
            jobs: Mutex::default(),
            queue: Queue::new(),
            events: opts.events,
            playback,
            catalog: rustet_ampapi::Client::new(http.clone()),
            http,
            cache: opts.cache,
            bw: Arc::new(BwController::new(Some(stats), opts.floor_bps as f64)),
            confinement: Confinement::new(&opts.output_roots, opts.more_roots),
            seq: AtomicI64::new(0),
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
            #[cfg(test)]
            hook: Mutex::default(),
        });
        inner.tasks.spawn(Arc::clone(&inner).worker());
        Self { inner }
    }

    /// Validates and queues a request and returns the new job.
    pub fn enqueue(&self, req: ExportRequest) -> Result<ExportJob, EnqueueError> {
        self.inner.enqueue(req)
    }

    /// A snapshot of one job.
    pub fn get(&self, id: &str) -> Option<ExportJob> {
        let jobs = self.inner.lock();
        let entry = jobs.get(id)?;
        Some(
            self.inner
                .snapshot(&entry.job, &self.inner.queue_positions()),
        )
    }

    /// Snapshots of every known job, in enqueue order.
    pub fn list(&self) -> Vec<ExportJob> {
        let jobs = self.inner.lock();
        let pos = self.inner.queue_positions();
        let mut out: Vec<ExportJob> = jobs
            .values()
            .map(|e| self.inner.snapshot(&e.job, &pos))
            .collect();
        out.sort_by_key(|j| j.queue_pos);
        out
    }

    /// Cancels or removes a job. Returns false if it is unknown.
    /// - queued: leaves the queue at once and ends cancelled;
    /// - running: its token is cancelled and it ends cancelled shortly (a job the worker has just
    ///   taken from the queue counts as running);
    /// - finished: removed from the list ("Clear done").
    pub fn cancel(&self, id: &str) -> bool {
        let inner = &self.inner;
        let (cancel, event) = {
            let mut jobs = inner.lock();
            let Some(entry) = jobs.get_mut(id) else {
                return false;
            };
            if entry.job.phase.is_terminal() {
                jobs.remove(id);
                return true;
            }
            let event = inner.queue.remove(id).map(|_| {
                entry.job.phase = Phase::Cancelled;
                entry.job.updated_at = now();
                entry.job.event(0, "")
            });
            (entry.cancel.clone(), event)
        };
        cancel.cancel();
        if let Some(ev) = event {
            inner.emit(ev);
        }
        true
    }

    /// Re-enqueues a failed or cancelled job's request with the given current credentials.
    /// `None` if the job is unknown or in another state.
    pub fn retry(&self, id: &str, token: &str, mut_token: &str) -> Option<ExportJob> {
        let mut req = {
            let jobs = self.inner.lock();
            let entry = jobs.get(id)?;
            if !matches!(entry.job.phase, Phase::Failed | Phase::Cancelled) {
                return None;
            }
            entry.request.clone()
        };
        token.clone_into(&mut req.token);
        mut_token.clone_into(&mut req.mut_token);
        self.inner.enqueue(req).ok()
    }

    /// Changes a queued job's priority and returns its new snapshot. A running or finished job
    /// is never reordered.
    pub fn prioritize(&self, id: &str, priority: i32) -> Result<ExportJob, PrioritizeError> {
        let inner = &self.inner;
        let mut jobs = inner.lock();
        let entry = jobs.get_mut(id).ok_or(PrioritizeError::NotFound)?;
        if !inner.queue.update(id, priority) {
            return Err(PrioritizeError::NotQueued);
        }
        entry.job.priority = priority;
        entry.job.updated_at = now();
        Ok(inner.snapshot(&entry.job, &inner.queue_positions()))
    }

    /// Closes the queue, cancels every job (which stops their FFmpeg and VLC children) and waits,
    /// at most 2.5 s, for the worker and every post-process task, so no child keeps writing after
    /// the engine exits.
    pub async fn stop(&self) {
        let inner = &self.inner;
        inner.queue.close();
        inner.shutdown.cancel();
        inner.tasks.close();
        if tokio::time::timeout(STOP_WAIT, inner.tasks.wait())
            .await
            .is_err()
        {
            warn!("export: stop timed out after {STOP_WAIT:?} with work still running");
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.inner.queue.close();
        self.inner.shutdown.cancel();
    }
}

/// The catalog metadata of one track.
#[derive(Debug, Default)]
struct Resolved {
    title: String,
    artist: String,
    album: String,
    artwork_url: String,
    genre: String,
    release_date: String,
    content_rating: String,
    composer: String,
    isrc: String,
    audio_traits: Vec<String>,
    duration_ms: u64,
    track_number: u32,
    disc_number: u32,
    track_total: u32,
    copyright: String,
    record_label: String,
    upc: String,
    has_lyrics: bool,
    mastered: bool,
}

/// The codec label and file extension for the export.
fn codec_ext(caps: ExportCapabilities, convert_to_flac: bool) -> (&'static str, &'static str) {
    let (codec, ext) = if caps.video {
        ("mv", "mp4")
    } else if caps.atmos {
        ("atmos", "m4a")
    } else if caps.lossless {
        ("alac", "m4a")
    } else {
        ("aac", "m4a")
    };
    if convert_to_flac && caps.lossless && !caps.video {
        (codec, "flac")
    } else {
        (codec, ext)
    }
}

/// The quality label for templates.
fn quality(caps: ExportCapabilities, traits: &[String]) -> &'static str {
    if caps.video {
        "MV"
    } else if caps.atmos {
        "Atmos"
    } else if caps.lossless {
        if traits.iter().any(|t| t == "hi-res-lossless") {
            "Hi-Res Lossless"
        } else {
            "Lossless"
        }
    } else {
        "AAC"
    }
}

/// A conservative bytes-per-millisecond estimate, for the progress total.
fn bytes_per_ms(caps: ExportCapabilities) -> u64 {
    if caps.video {
        8_000_000 / 8_000
    } else if caps.atmos {
        768_000 / 8_000
    } else if caps.lossless {
        1_500_000 / 8_000
    } else {
        256_000 / 8_000
    }
}

fn or<'a>(v: &'a str, default: &'a str) -> &'a str {
    if v.is_empty() { default } else { v }
}

fn new_id() -> String {
    let mut b = [0u8; 6];
    // Without the OS random source the IDs would collide; nothing useful can continue.
    getrandom::fill(&mut b).expect("OS random source unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

/// Runs blocking file-system work off the runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        // No critical section can leave the map inconsistent.
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, ev: ExportEvent) {
        if let Some(sink) = &self.events {
            sink(ev);
        }
    }

    /// Queued job IDs → 1-based run order, from one sorted snapshot of the queue.
    fn queue_positions(&self) -> HashMap<String, usize> {
        self.queue
            .snapshot()
            .into_iter()
            .enumerate()
            .map(|(i, e)| (e.id, i + 1))
            .collect()
    }

    /// A copy for API clients. `limit_bps` is the limit in force now: only a job streaming from
    /// the network is throttled.
    fn snapshot(&self, job: &ExportJob, pos: &HashMap<String, usize>) -> ExportJob {
        let mut s = job.clone();
        s.queue_index = pos.get(&job.id).copied().unwrap_or(0);
        s.limit_bps = (s.phase == Phase::Downloading && s.source == Some(Source::Network))
            .then(|| self.bw.current_limit())
            .flatten();
        s.throttled = s.limit_bps.is_some();
        s
    }

    fn enqueue(&self, mut req: ExportRequest) -> Result<ExportJob, EnqueueError> {
        if req.asset_id.is_empty() {
            return Err(EnqueueError::MissingAssetId);
        }
        let opts = &req.options;
        if !tool::valid_tool_path(&opts.ffmpeg_path, &["ffmpeg"]) {
            return Err(EnqueueError::FfmpegName);
        }
        if !tool::valid_tool_path(&opts.vlc_path, &["vlc", "cvlc"]) {
            return Err(EnqueueError::VlcName);
        }
        tool::check_tool_executable(&opts.ffmpeg_path, &["ffmpeg"])
            .map_err(EnqueueError::FfmpegUntrusted)?;
        tool::check_tool_executable(&opts.vlc_path, &["vlc", "cvlc"])
            .map_err(EnqueueError::VlcUntrusted)?;
        if req.output_dir.is_empty() {
            req.output_dir = fs::default_output_dir();
        }
        req.output_dir = fs::normalize_output_dir(&req.output_dir)?;
        self.confinement.check(&req.output_dir)?;
        if req.options.artwork_size <= 0 {
            req.options.artwork_size = i64::from(DEFAULT_ARTWORK_SIZE);
        }
        if req.options.lrc_type.is_empty() {
            req.options.lrc_type = "lyrics".to_owned();
        }

        let id = new_id();
        let created = now();
        let job = ExportJob {
            id: id.clone(),
            asset_id: req.asset_id.clone(),
            phase: Phase::Queued,
            percent: 0,
            queue_pos: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            queue_index: 0,
            priority: req.priority,
            title: req.hint_title.clone(),
            artist_name: req.hint_artist.clone(),
            artwork_url: req.hint_artwork.clone(),
            bytes_done: 0,
            bytes_total: 0,
            output: String::new(),
            error: String::new(),
            source: None,
            limit_bps: None,
            throttled: false,
            created_at: created,
            updated_at: created,
        };
        let cancel = self.shutdown.child_token();
        let mut stored = req.clone();
        stored.token.clear();
        stored.mut_token.clear();
        let priority = req.priority;
        let item = Arc::new(WorkItem {
            id: id.clone(),
            req,
            cancel: cancel.clone(),
        });

        let (snapshot, event) = {
            let mut jobs = self.lock();
            if !self.queue.push(id.clone(), item, priority) {
                return Err(EnqueueError::Closed);
            }
            let event = job.event(0, "");
            let snapshot = self.snapshot(&job, &self.queue_positions());
            jobs.insert(
                id,
                Entry {
                    job,
                    request: stored,
                    cancel,
                },
            );
            (snapshot, event)
        };
        self.emit(event);
        Ok(snapshot)
    }

    /// Runs `f` on a live job; `None` when it is gone.
    fn update<R>(&self, id: &str, f: impl FnOnce(&mut ExportJob) -> R) -> Option<R> {
        self.lock().get_mut(id).map(|e| f(&mut e.job))
    }

    fn advance(&self, id: &str, phase: Phase, percent: u32) {
        let event = self.update(id, |j| {
            j.advance(phase, percent).then(|| j.event(percent, ""))
        });
        if let Some(Some(ev)) = event {
            self.emit(ev);
        }
    }

    /// Ends a job. A job whose token fired (Cancel, Stop) ends cancelled, not failed.
    fn fail(&self, id: &str, err: &JobError) {
        let msg = err.to_string();
        let event = {
            let mut jobs = self.lock();
            let Some(entry) = jobs.get_mut(id) else {
                return;
            };
            let cancelled = entry.cancel.is_cancelled();
            if !entry.job.fail(msg.clone(), cancelled) {
                return;
            }
            entry.job.event(0, &msg)
        };
        self.emit(event);
    }

    fn set_output(&self, id: &str, output: &str) {
        self.update(id, |j| {
            output.clone_into(&mut j.output);
            j.updated_at = now();
        });
    }

    fn set_source(&self, id: &str, source: Source) {
        self.update(id, |j| j.source = Some(source));
    }

    fn reset_progress(&self, id: &str) {
        self.update(id, |j| {
            j.bytes_done = 0;
            j.percent = 0;
        });
    }

    /// Removes a job that turned into other jobs (an album or playlist). No event: the expanded
    /// tracks announce themselves.
    fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    // ── Worker ──────────────────────────────────────────────────────────────

    async fn worker(self: Arc<Self>) {
        while let Some(item) = self.queue.pop().await {
            #[cfg(test)]
            {
                let hook = self.hook.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook(Arc::clone(&self), item).await;
                    continue;
                }
            }
            self.execute(item).await;
        }
    }

    /// Runs one job through download; post-processing continues in its own task.
    async fn execute(self: &Arc<Self>, item: Arc<WorkItem>) {
        let res = AssertUnwindSafe(self.run(&item)).catch_unwind().await;
        let err = match res {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e,
            Err(p) => JobError::Panic(format!("panic: {}", panic_message(&*p))),
        };
        self.fail(&item.id, &err);
    }

    async fn run(self: &Arc<Self>, item: &Arc<WorkItem>) -> Result<(), JobError> {
        let (id, req, cancel) = (&item.id, &item.req, &item.cancel);
        // Cancelled between the pop and here: end without doing any work.
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }

        self.advance(id, Phase::Resolving, 0);
        let sf = or(&req.storefront, "us");
        let lang = or(&req.language, "en-US");

        if req.capabilities.playlist {
            return self.expand_playlist(item, sf, lang).await;
        }
        let ra = if req.capabilities.video {
            self.resolve_video(item, sf, lang).await?
        } else {
            match self.resolve_song(item, sf, lang).await? {
                Some(ra) => ra,
                None => return Ok(()), // an album, expanded into its tracks
            }
        };

        let (codec, ext) = codec_ext(req.capabilities, req.options.convert_to_flac);
        self.update(id, |j| {
            j.title.clone_from(&ra.title);
            j.artist_name.clone_from(&ra.artist);
            j.artwork_url.clone_from(&ra.artwork_url);
        });
        let meta = TrackMeta {
            title: ra.title.clone(),
            artist_name: ra.artist.clone(),
            album_artist: ra.artist.clone(),
            album_name: ra.album.clone(),
            track_number: ra.track_number,
            track_total: ra.track_total,
            disc_number: ra.disc_number,
            disc_total: 0,
            release_date: ra.release_date.clone(),
            genre: ra.genre.clone(),
            composer: ra.composer.clone(),
            copyright: ra.copyright.clone(),
            record_label: ra.record_label.clone(),
            isrc: ra.isrc.clone(),
            upc: ra.upc.clone(),
            content_rating: ra.content_rating.clone(),
            artwork_url: ra.artwork_url.clone(),
            has_lyrics: ra.has_lyrics,
        };

        // {tag}: the content-rating marker, then the Apple Digital Master marker.
        let opts = &req.options;
        let mut tag = match ra.content_rating.as_str() {
            "explicit" => opts.explicit_choice.clone(),
            "clean" => opts.clean_choice.clone(),
            _ => String::new(),
        };
        if ra.mastered {
            tag.push_str(&opts.master_choice);
        }
        let url_artist = template::slugify(&ra.artist);
        let vars = TemplateVars {
            title: &ra.title,
            artist: &ra.artist,
            album_artist: &ra.artist,
            album: &ra.album,
            track_number: ra.track_number,
            disc_number: ra.disc_number,
            year: template::year_from_date(&ra.release_date),
            genre: &ra.genre,
            codec,
            ext,
            quality: quality(req.capabilities, &ra.audio_traits),
            tag: &tag,
            release_date: &ra.release_date,
            isrc: &ra.isrc,
            song_id: &req.asset_id,
            url_artist: &url_artist,
        };
        let rel = template::render(&req.filename_template, &vars);
        let out = template::contain(&req.output_dir, &rel).ok_or(JobError::Escapes)?;

        // Early overwrite decision; the final move enforces the policy atomically.
        let policy = opts.overwrite_policy;
        let checked = out.clone();
        let Some(final_path) = blocking(move || Ok(fs::overwrite_path(&checked, policy))).await?
        else {
            self.set_output(id, &out);
            self.advance(id, Phase::Done, 100);
            return Ok(());
        };

        self.advance(id, Phase::Downloading, 0);
        let tmp = self
            .download(item, sf, lang, &final_path, ra.duration_ms)
            .await?;

        // The job's own token (not a request's) bounds post-processing, so it survives client
        // disconnects while Cancel and Stop still end it.
        let this = Arc::clone(self);
        let item = Arc::clone(item);
        self.tasks.spawn(async move {
            let res = AssertUnwindSafe(this.post_process(&item, meta, tmp, final_path))
                .catch_unwind()
                .await;
            let err = match res {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(p) => JobError::Panic(format!("post-process panic: {}", panic_message(&*p))),
            };
            this.fail(&item.id, &err);
        });
        Ok(())
    }

    // ── Resolve ─────────────────────────────────────────────────────────────

    async fn resolve_video(
        &self,
        item: &WorkItem,
        sf: &str,
        lang: &str,
    ) -> Result<Resolved, JobError> {
        let req = &item.req;
        let not_found = |cause| JobError::MusicVideoNotFound {
            id: req.asset_id.clone(),
            storefront: sf.to_owned(),
            cause,
        };
        let resp = self
            .catalog
            .music_video(&item.cancel, sf, &req.asset_id, lang, &req.token)
            .await
            .map_err(|e| not_found(Cause::of(e)))?;
        let mv = resp
            .data
            .first()
            .ok_or_else(|| not_found(Cause::default()))?;
        let a = &mv.attributes;
        let mut ra = Resolved {
            title: a.name.clone(),
            artist: a.artist_name.clone(),
            album: a.album_name.clone(),
            artwork_url: a.artwork.url.clone(),
            genre: a.genre_names.first().cloned().unwrap_or_default(),
            release_date: a.release_date.clone(),
            content_rating: a.content_rating.clone(),
            isrc: a.isrc.clone(),
            duration_ms: a.duration_in_millis,
            track_number: a.track_number,
            disc_number: a.disc_number,
            has_lyrics: req.options.embed_lyrics || req.options.save_lrc_sidecar,
            ..Resolved::default()
        };
        if let Some(album) = mv.relationships.albums.data.first() {
            let al = &album.attributes;
            ra.track_total = al.track_count;
            ra.copyright.clone_from(&al.copyright);
            ra.record_label.clone_from(&al.record_label);
            ra.upc.clone_from(&al.upc);
        }
        Ok(ra)
    }

    /// Resolves a song; an ID that is an album instead becomes one job per track (`None`).
    async fn resolve_song(
        &self,
        item: &WorkItem,
        sf: &str,
        lang: &str,
    ) -> Result<Option<Resolved>, JobError> {
        let req = &item.req;
        let resp = self
            .catalog
            .song(&item.cancel, sf, &req.asset_id, lang, &req.token)
            .await;
        let song = match &resp {
            Ok(r) => r.data.first(),
            Err(_) => None,
        };
        let Some(song) = song else {
            if item.cancel.is_cancelled() {
                return Err(JobError::Cancelled);
            }
            if self.expand_album(item, sf, lang).await {
                return Ok(None);
            }
            return Err(JobError::SongNotFound {
                id: req.asset_id.clone(),
                storefront: sf.to_owned(),
                cause: resp.err().map(Cause::of).unwrap_or_default(),
            });
        };
        let a = &song.attributes;
        let mut ra = Resolved {
            title: a.name.clone(),
            artist: a.artist_name.clone(),
            album: a.album_name.clone(),
            artwork_url: a.artwork.url.clone(),
            genre: a.genre_names.first().cloned().unwrap_or_default(),
            release_date: a.release_date.clone(),
            content_rating: a.content_rating.clone(),
            composer: a.composer_name.clone(),
            isrc: a.isrc.clone(),
            audio_traits: a.audio_traits.to_vec(),
            duration_ms: a.duration_in_millis,
            track_number: a.track_number,
            disc_number: a.disc_number,
            has_lyrics: a.has_lyrics,
            mastered: a.is_mastered_for_itunes || a.is_apple_digital_master,
            ..Resolved::default()
        };
        if let Some(album) = song.relationships.albums.data.first() {
            let al = &album.attributes;
            ra.track_total = al.track_count;
            ra.copyright.clone_from(&al.copyright);
            ra.record_label.clone_from(&al.record_label);
            ra.upc.clone_from(&al.upc);
        }
        Ok(Some(ra))
    }

    /// Queues one job per track of the album `item` names. False if it is not an album with
    /// tracks.
    async fn expand_album(&self, item: &WorkItem, sf: &str, lang: &str) -> bool {
        let req = &item.req;
        let Ok(album) = self
            .catalog
            .album(&item.cancel, sf, &req.asset_id, lang, &req.token)
            .await
        else {
            return false;
        };
        let Some(album) = album
            .data
            .first()
            .filter(|a| !a.relationships.tracks.data.is_empty())
        else {
            return false;
        };
        for track in album.relationships.tracks.data.iter() {
            let video = match track.r#type.as_str() {
                "songs" => false,
                "music-videos" => true,
                _ => continue,
            };
            self.enqueue_track(req, &track.id, video);
        }
        self.remove(&item.id);
        true
    }

    /// Queues one job per track of a playlist (catalog or library) and removes the playlist job.
    async fn expand_playlist(&self, item: &WorkItem, sf: &str, lang: &str) -> Result<(), JobError> {
        let (req, cancel) = (&item.req, &item.cancel);
        let id = || req.asset_id.clone();
        if req.capabilities.library_playlist {
            let tracks = self
                .catalog
                .library_playlist_tracks(cancel, &req.asset_id, lang, &req.token, &req.mut_token)
                .await
                .map_err(|e| JobError::LibraryPlaylist {
                    id: id(),
                    cause: Cause::of(e),
                })?;
            for track in &tracks {
                if let Some(catalog_id) = track.attributes.play_params.stream_catalog_id() {
                    self.enqueue_track(req, catalog_id, track.r#type == "library-music-videos");
                }
            }
        } else {
            let resp = self
                .catalog
                .playlist(cancel, sf, &req.asset_id, lang, &req.token, &req.mut_token)
                .await
                .map_err(|e| JobError::Playlist {
                    id: id(),
                    cause: Cause::of(e),
                })?;
            let tracks = resp
                .data
                .first()
                .map(|p| &p.relationships.tracks.data[..])
                .filter(|t| !t.is_empty())
                .ok_or_else(|| JobError::Playlist {
                    id: id(),
                    cause: Cause::default(),
                })?;
            for track in tracks {
                let video = match track.r#type.as_str() {
                    "songs" => false,
                    "music-videos" => true,
                    _ => continue,
                };
                self.enqueue_track(req, &track.id, video);
            }
        }
        self.remove(&item.id);
        Ok(())
    }

    /// Queues one track of an expanded collection with the collection's request (credentials
    /// and priority included).
    fn enqueue_track(&self, collection: &ExportRequest, asset_id: &str, video: bool) {
        let mut req = collection.clone();
        asset_id.clone_into(&mut req.asset_id);
        req.capabilities.playlist = false;
        req.capabilities.library_playlist = false;
        req.capabilities.video |= video;
        if let Err(e) = self.enqueue(req) {
            warn!(asset_id, "export: could not queue a collection track: {e}");
        }
    }

    // ── Download ────────────────────────────────────────────────────────────

    /// Fills a temp file next to `final_path` with the decrypted media and returns its path.
    async fn download(
        &self,
        item: &WorkItem,
        sf: &str,
        lang: &str,
        final_path: &str,
        duration_ms: u64,
    ) -> Result<PathBuf, JobError> {
        let (id, req, cancel) = (&item.id, &item.req, &item.cancel);
        if duration_ms > 0 {
            let total = duration_ms * bytes_per_ms(req.capabilities);
            self.update(id, |j| j.bytes_total = total);
        }
        let final_path = Path::new(final_path);
        let dir = final_path.parent().unwrap_or(Path::new("."));
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| JobError::Mkdir {
                dir: dir.to_string_lossy().into_owned(),
                source,
            })?;
        let tmp = dir.join(format!(".{id}.am-export.tmp"));

        // Prefer bytes playback already fetched: the committed cache, then the tail of an
        // in-progress playback download, and only then the network.
        if !req.capabilities.video && self.reuse_cached_audio(item, &tmp).await {
            return Ok(tmp);
        }
        if cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        self.set_source(id, Source::Network);
        // Only the network path competes with playback, so only it is throttled.
        let _throttle = self.bw.start();

        let open = OpenRequest {
            asset_id: req.asset_id.clone(),
            storefront: sf.to_owned(),
            token: req.token.clone(),
            mut_token: req.mut_token.clone(),
            language: lang.to_owned(),
            lossless: req.capabilities.lossless,
            atmos: req.capabilities.atmos,
            video: req.capabilities.video,
            mv_max_height: u32::try_from(req.mv_max_height).unwrap_or(0),
            ..OpenRequest::default()
        };
        // A private session: releasing it can never delete a session playback is serving.
        let sess = self
            .playback
            .open_private(cancel, open)
            .await
            .map_err(JobError::Open)?;
        let ctx = StreamContext::new(cancel.clone());

        if !req.capabilities.video {
            // Straight to the temp file: buffering in memory held a whole track (~250 MB for
            // hi-res ALAC) per export.
            let res = self.stream_audio(&ctx, id, &sess.id, &tmp).await;
            self.playback.release(&sess.id);
            return res.map(|()| tmp);
        }

        let streamed = self.stream_video(&ctx, id, &sess.id, &tmp).await;
        self.playback.release(&sess.id);
        let (video, audio) = streamed?;
        let muxed =
            ffmpeg::mux_video_audio(cancel, &req.options.ffmpeg_path, &video, &audio, &tmp).await;
        let _ = tokio::fs::remove_file(&video).await;
        let _ = tokio::fs::remove_file(&audio).await;
        if let Err(e) = muxed {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(if cancel.is_cancelled() {
                JobError::Cancelled
            } else {
                JobError::Mux(e)
            });
        }
        Ok(tmp)
    }

    async fn stream_audio(
        &self,
        ctx: &StreamContext,
        id: &str,
        sess: &str,
        tmp: &Path,
    ) -> Result<(), JobError> {
        let file = File::create(tmp).await.map_err(JobError::CreateTemp)?;
        let mut sink = self.bw.writer(Progress::new(file, self, id));
        let mut res = self
            .playback
            .stream(
                ctx,
                StreamClass::Background,
                sess,
                StreamKind::Audio,
                &mut sink,
            )
            .await
            .map_err(JobError::Stream);
        if res.is_ok() {
            res = sink.flush().await.map_err(JobError::Io);
        }
        close(sink.into_inner().inner).await;
        if res.is_err() {
            let _ = tokio::fs::remove_file(tmp).await;
        }
        res
    }

    /// Streams the video track (counted as progress) and the audio track into two temp files.
    async fn stream_video(
        &self,
        ctx: &StreamContext,
        id: &str,
        sess: &str,
        tmp: &Path,
    ) -> Result<(PathBuf, PathBuf), JobError> {
        let video = with_suffix(tmp, ".video");
        let file = File::create(&video).await.map_err(JobError::CreateTemp)?;
        let mut sink = self.bw.writer(Progress::new(file, self, id));
        let mut res = self
            .playback
            .stream(
                ctx,
                StreamClass::Background,
                sess,
                StreamKind::Video,
                &mut sink,
            )
            .await
            .map_err(JobError::StreamVideo);
        if res.is_ok() {
            res = sink.flush().await.map_err(JobError::Io);
        }
        close(sink.into_inner().inner).await;
        if let Err(e) = res {
            let _ = tokio::fs::remove_file(&video).await;
            return Err(e);
        }

        let audio = with_suffix(tmp, ".audio");
        let res = async {
            let file = File::create(&audio).await.map_err(JobError::CreateTemp)?;
            let mut sink = self.bw.writer(file);
            let mut res = self
                .playback
                .stream(
                    ctx,
                    StreamClass::Background,
                    sess,
                    StreamKind::Audio,
                    &mut sink,
                )
                .await
                .map_err(JobError::StreamAudio);
            if res.is_ok() {
                res = sink.flush().await.map_err(JobError::Io);
            }
            close(sink.into_inner()).await;
            res
        }
        .await;
        if let Err(e) = res {
            let _ = tokio::fs::remove_file(&video).await;
            let _ = tokio::fs::remove_file(&audio).await;
            return Err(e);
        }
        Ok((video, audio))
    }

    /// Fills `tmp` from bytes playback already fetched: the committed cache file, then the tail
    /// of an in-progress playback download. True only when `tmp` holds the complete track; any
    /// failure removes the partial file and resets progress, so the network stream that follows
    /// never appends to a partial copy.
    async fn reuse_cached_audio(&self, item: &WorkItem, tmp: &Path) -> bool {
        let Some(cache) = &self.cache else {
            return false;
        };
        let (id, asset) = (&item.id, &item.req.asset_id);
        let q = audio_qualifier(item.req.capabilities);

        if let Some(path) = cache.path(asset, q).await {
            let copied = match File::open(&path).await {
                Ok(f) => self.copy_to_temp(item, tmp, f).await,
                Err(e) => Err(e.into()),
            };
            match copied {
                Ok(()) => {
                    self.set_source(id, Source::Cache);
                    return true;
                }
                Err(e) => warn!(job = %id, "export: cache copy failed, falling back: {e}"),
            }
        }
        if let Some(tail) = cache.tail_reader(asset, q) {
            self.set_source(id, Source::Playback);
            match self.copy_to_temp(item, tmp, tail).await {
                Ok(()) => return true,
                Err(e) => {
                    warn!(job = %id, "export: playback tail failed, falling back to network: {e}")
                }
            }
        }
        false
    }

    /// Copies `r` into a fresh `tmp`, counting progress. A playback tail read can wait on its
    /// producer, so the copy stops at once when the job is cancelled. Any failure leaves no temp
    /// file behind.
    async fn copy_to_temp(
        &self,
        item: &WorkItem,
        tmp: &Path,
        mut r: impl AsyncRead + Unpin,
    ) -> Result<(), JobError> {
        let file = File::create(tmp).await?;
        self.reset_progress(&item.id);
        let mut sink = Progress::new(file, self, &item.id);
        let copy = async {
            tokio::io::copy(&mut r, &mut sink).await?;
            sink.flush().await
        };
        let res = match item.cancel.run_until_cancelled(copy).await {
            Some(r) => r.map_err(JobError::Io),
            None => Err(JobError::Cancelled),
        };
        drop(r);
        close(sink.inner).await;
        if res.is_err() {
            let _ = tokio::fs::remove_file(tmp).await;
            self.reset_progress(&item.id);
        }
        res
    }

    // ── Post-process ────────────────────────────────────────────────────────

    async fn post_process(
        &self,
        item: &WorkItem,
        meta: TrackMeta,
        tmp: PathBuf,
        final_path: String,
    ) -> Result<(), JobError> {
        let (id, req, cancel) = (&item.id, &item.req, &item.cancel);
        let opts = &req.options;
        let sf = or(&req.storefront, "us");
        let lang = or(&req.language, "en-US");
        let artwork_size = u32::try_from(opts.artwork_size).unwrap_or(DEFAULT_ARTWORK_SIZE);
        self.advance(id, Phase::Tagging, 80);

        let mut lyrics = String::new();
        if (opts.embed_lyrics || opts.save_lrc_sidecar) && meta.has_lyrics {
            lyrics = if req.capabilities.video {
                self.music_video_lyrics(item, sf, lang, &tmp).await
            } else {
                let lr = LyricsRequest {
                    storefront: sf,
                    song_id: &req.asset_id,
                    kind: &opts.lrc_type,
                    language: lang,
                    format: opts.lrc_format,
                    token: &req.token,
                    media_user_token: &req.mut_token,
                };
                self.catalog.lyrics(cancel, &lr).await.unwrap_or_default()
            };
        }

        if !req.capabilities.video {
            let artwork = if opts.embed_artwork && !meta.artwork_url.is_empty() {
                // Non-fatal: tag without artwork if the download fails.
                tagger::download_artwork(&self.http, cancel, &meta.artwork_url, artwork_size)
                    .await
                    .ok()
            } else {
                None
            };
            let (path, meta, lyrics) = (tmp.clone(), meta.clone(), lyrics.clone());
            let tagged = tokio::task::spawn_blocking(move || {
                tagger::tag_file(&path, &meta, artwork, &lyrics)
            })
            .await;
            match tagged {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!(asset = %req.asset_id, "export: tag warning: {e}"),
                Err(e) => warn!(asset = %req.asset_id, "export: tagger failed: {e}"),
            }
        }

        if opts.save_lrc_sidecar && !lyrics.is_empty() {
            let base = &final_path[..final_path.len() - path::ext(&final_path).len()];
            let sidecar = format!("{base}.{}", lyrics_ext(opts.lrc_format));
            if let Err(e) = tokio::fs::write(&sidecar, &lyrics).await {
                warn!(asset = %req.asset_id, "export: lyrics sidecar: {e}");
            }
        }

        let mut moves = vec![(tmp, final_path)];
        if opts.convert_to_flac && req.capabilities.lossless && !req.capabilities.video {
            let (tmp, final_path) = moves.pop().expect("one move");
            moves = self
                .convert_flac(item, &meta, tmp, final_path, artwork_size)
                .await;
        }

        if cancel.is_cancelled() {
            // Cancelled during post-processing: leave nothing behind.
            for (tmp, _) in &moves {
                let _ = tokio::fs::remove_file(tmp).await;
            }
            return Err(JobError::Cancelled);
        }
        self.advance(id, Phase::Moving, 96);
        // The early existence check ran long ago, and another job may have produced the same
        // file since, so the policy is enforced atomically here.
        let policy = opts.overwrite_policy;
        let mut output = String::new();
        for (i, (tmp, dest)) in moves.into_iter().enumerate() {
            let target = dest.clone();
            let moved =
                blocking(move || fs::move_no_clobber(&tmp, &target, policy, MAX_RENAME_ATTEMPTS))
                    .await
                    .map_err(|source| JobError::Move { dest, source })?;
            if i == 0 {
                output = moved;
            }
        }
        self.set_output(id, &output);
        self.advance(id, Phase::Done, 100);
        Ok(())
    }

    /// Fetches a music video's TTML lyrics, embeds them as a subtitle track when possible, and
    /// returns them in the requested format (empty when there are none).
    async fn music_video_lyrics(
        &self,
        item: &WorkItem,
        sf: &str,
        lang: &str,
        tmp: &Path,
    ) -> String {
        let (req, cancel) = (&item.req, &item.cancel);
        let opts = &req.options;
        let lr = LyricsRequest {
            storefront: sf,
            song_id: &req.asset_id,
            kind: &opts.lrc_type,
            language: lang,
            format: LyricsFormat::Ttml,
            token: &req.token,
            media_user_token: &req.mut_token,
        };
        let ttml = match self.catalog.lyrics(cancel, &lr).await {
            Ok(t) if !t.is_empty() => t,
            _ => return String::new(),
        };
        let srt = ttml_to_srt(&ttml).unwrap_or_default();
        if !srt.is_empty() {
            let srt_path = with_suffix(tmp, ".srt");
            if tokio::fs::write(&srt_path, &srt).await.is_ok() {
                if let Err(e) =
                    ffmpeg::add_subtitle_track(cancel, &opts.ffmpeg_path, tmp, &srt_path).await
                {
                    warn!(asset = %req.asset_id, "export: subtitle embed warning: {e}");
                }
                let _ = tokio::fs::remove_file(&srt_path).await;
            }
        }
        match opts.lrc_format {
            LyricsFormat::Ttml => ttml,
            LyricsFormat::Srt => srt,
            LyricsFormat::Vtt => ttml_to_vtt(&ttml).unwrap_or_default(),
            LyricsFormat::Lrc => ttml_to_lrc(&ttml).unwrap_or_default(),
        }
    }

    /// Converts the tagged ALAC temp file to FLAC and returns the (temp, final) pairs to move:
    /// the FLAC, plus the `.m4a` when the original is kept. VLC decodes and FFmpeg encodes and
    /// tags; without VLC, or if it fails, FFmpeg does both. If that fails too the `.m4a` is
    /// exported instead.
    async fn convert_flac(
        &self,
        item: &WorkItem,
        meta: &TrackMeta,
        tmp: PathBuf,
        final_path: String,
        artwork_size: u32,
    ) -> Vec<(PathBuf, String)> {
        let (req, cancel) = (&item.req, &item.cancel);
        let opts = &req.options;
        let asset = &req.asset_id;
        let flac_tmp = with_suffix(&tmp, ".flac");
        let m4a_final = format!(
            "{}.m4a",
            final_path.strip_suffix(".flac").unwrap_or(&final_path)
        );
        let converted = 'convert: {
            if let Some(vlc) = ffmpeg::find_vlc(&opts.vlc_path) {
                let raw = with_suffix(&flac_tmp, ".raw.flac");
                match ffmpeg::vlc_to_flac(cancel, &vlc, &tmp, &raw).await {
                    Ok(()) => {
                        let art = if opts.embed_artwork {
                            tagger::artwork_to_temp(&self.http, cancel, meta, artwork_size, &raw)
                                .await
                        } else {
                            None
                        };
                        self.tag_raw_flac(item, meta, &raw, art.as_deref(), &flac_tmp)
                            .await;
                        if let Some(art) = art {
                            let _ = tokio::fs::remove_file(art).await;
                        }
                        break 'convert true;
                    }
                    Err(e) => {
                        info!(asset = %asset, "export: vlc transcode failed: {e}; trying ffmpeg");
                        let _ = tokio::fs::remove_file(&raw).await;
                    }
                }
            }
            match ffmpeg::convert_to_flac(cancel, &opts.ffmpeg_path, &tmp, None, &flac_tmp, meta)
                .await
            {
                Ok(()) => true,
                Err(e) => {
                    warn!(asset = %asset, "export: flac conversion failed: {e}; keeping .m4a");
                    let _ = tokio::fs::remove_file(&flac_tmp).await;
                    false
                }
            }
        };
        if !converted {
            return vec![(tmp, m4a_final)];
        }
        if opts.keep_original {
            vec![(flac_tmp, final_path), (tmp, m4a_final)]
        } else {
            let _ = tokio::fs::remove_file(&tmp).await;
            vec![(flac_tmp, final_path)]
        }
    }

    /// Re-encodes VLC's raw FLAC into `dst` with tags (and artwork, retried without it on
    /// failure); as a last resort the raw file itself becomes `dst`.
    async fn tag_raw_flac(
        &self,
        item: &WorkItem,
        meta: &TrackMeta,
        raw: &Path,
        art: Option<&Path>,
        dst: &Path,
    ) {
        let (cancel, ff) = (&item.cancel, &item.req.options.ffmpeg_path);
        let asset = &item.req.asset_id;
        let mut res = ffmpeg::tag_flac(cancel, ff, raw, art, dst, meta).await;
        if let Err(e) = &res {
            warn!(asset = %asset, "export: flac tag failed: {e}; retrying without art");
            let _ = tokio::fs::remove_file(dst).await;
            res = ffmpeg::tag_flac(cancel, ff, raw, None, dst, meta).await;
        }
        match res {
            Ok(()) => {
                let _ = tokio::fs::remove_file(raw).await;
            }
            Err(e) => {
                warn!(asset = %asset, "export: flac tag failed (no art): {e}; keeping raw");
                if tokio::fs::rename(raw, dst).await.is_err() {
                    let _ = tokio::fs::remove_file(raw).await;
                }
            }
        }
    }
}

/// Closes a file after any write still in flight has finished, so it can be removed at once
/// (Windows cannot delete an open file).
async fn close(file: File) {
    drop(file.into_std().await);
}

/// A sink that counts written bytes into the job's progress: `bytes_done`, and up to 79 % of the
/// estimated total (tagging and the move take the rest).
struct Progress<'a, W> {
    inner: W,
    owner: &'a Inner,
    id: &'a str,
}

impl<'a, W> Progress<'a, W> {
    fn new(inner: W, owner: &'a Inner, id: &'a str) -> Self {
        Self { inner, owner, id }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Progress<'_, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let res = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res
            && n > 0
        {
            self.owner.update(self.id, |j| {
                j.bytes_done += n as u64;
                if let Some(pct) = (j.bytes_done * 79).checked_div(j.bytes_total) {
                    j.percent = u32::try_from(pct).unwrap_or(79).min(79);
                }
            });
        }
        res
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<W: AsyncWrite + Send + Unpin> Sink for Progress<'_, W> {}

#[cfg(test)]
mod tests;
