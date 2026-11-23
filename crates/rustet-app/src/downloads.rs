//! Downloads (exports, spec §1.5): the job list the Downloads page draws, kept in step with the
//! engine's `export` events, its actions (cancel, retry, priority, clear finished), and the
//! "Download" intents of tracks, albums and playlists.
//!
//! The engine runs the jobs with the session's own tokens; a request carries only what to
//! export and the user's Downloads settings ([`ExportRequest::new`]). As the web panel did, a
//! failed job is retried once after `retry-timeout` seconds while "Retry on failure" is on; the
//! retry is a new job, so a job that keeps failing is retried again.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustet_ampapi::Api;
use rustet_ampapi::api::{ResourceRef, ResourceType};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info};

use crate::engine::types::{ExportJob, ExportKind, ExportPhase, ExportProgress, ExportRequest};
use crate::engine::{ClientError, EngineClient, EngineEvent};
use crate::pages::model::{Header, TrackKind, TrackRow};
use crate::pages::{AlbumPage, LibraryAlbumPage, LibraryPlaylistPage, PlaylistPage};
use crate::route::Route;
use crate::settings::Settings;

/// What a "Download" button or menu entry exports.
#[derive(Debug, Clone, Copy)]
pub enum DownloadTarget<'a> {
    Track(&'a TrackRow),
    Album(&'a AlbumPage),
    Playlist(&'a PlaylistPage),
    /// Exported as its catalog album.
    LibraryAlbum(&'a LibraryAlbumPage),
    LibraryPlaylist(&'a LibraryPlaylistPage),
    /// A context menu's [`crate::pages::actions::MenuItem::Download`].
    Item(&'a ResourceRef),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    /// Not in the catalog of this storefront, or not a kind Rustet exports.
    #[error("This item cannot be downloaded.")]
    NotDownloadable,
    #[error("Rustet's engine is not running. Wait a moment, then try again.")]
    Offline,
    /// The engine has no export support or no account for it; its message says which.
    #[error("{0}")]
    Refused(String),
}

impl From<ClientError> for DownloadError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Unreachable(_) => Self::Offline,
            ClientError::NotAuthenticated(_) => {
                Self::Refused("Sign in to Apple Music to download.".to_owned())
            }
            ClientError::BadRequest(m)
            | ClientError::Conflict(m)
            | ClientError::NotFound(m)
            | ClientError::Unavailable(m) => Self::Refused(m),
            e => Self::Refused(e.to_string()),
        }
    }
}

fn hints(request: &mut ExportRequest, header: &Header) {
    request.hint_title = header.title.clone();
    request.hint_artist = header
        .subtitle
        .as_ref()
        .map(|s| s.text.clone())
        .unwrap_or_default();
    request.hint_artwork = header
        .artwork
        .as_ref()
        .map(|a| a.template.clone())
        .unwrap_or_default();
}

/// The export request of `target` with the user's Downloads settings. `lossless`: the engine
/// can play lossless now (the lossless account is signed in).
pub fn request(
    target: DownloadTarget<'_>,
    settings: &Settings,
    lossless: bool,
) -> Result<ExportRequest, DownloadError> {
    let allowed = lossless && settings.playback.lossless_enabled;
    let new = |id: &str, kind| ExportRequest::new(id, kind, &settings.downloads, allowed);
    Ok(match target {
        DownloadTarget::Track(row) => {
            let id = row.id.catalog_id().filter(|_| row.is_playable());
            let kind = match row.kind {
                TrackKind::Song => ExportKind::Song,
                TrackKind::MusicVideo => ExportKind::MusicVideo,
            };
            let mut r = new(id.ok_or(DownloadError::NotDownloadable)?, kind);
            r.hint_title = row.title.clone();
            r.hint_artist = row.artist.text.clone();
            r.hint_artwork = row
                .artwork
                .as_ref()
                .map(|a| a.template.clone())
                .unwrap_or_default();
            r
        }
        DownloadTarget::Album(p) => {
            let mut r = new(&p.id, ExportKind::Album);
            hints(&mut r, &p.header);
            r
        }
        DownloadTarget::Playlist(p) => {
            let mut r = new(&p.id, ExportKind::Playlist);
            hints(&mut r, &p.header);
            r
        }
        DownloadTarget::LibraryAlbum(p) => {
            let Some(Route::Album { id }) = &p.catalog else {
                return Err(DownloadError::NotDownloadable);
            };
            let mut r = new(id, ExportKind::Album);
            hints(&mut r, &p.header);
            r
        }
        DownloadTarget::LibraryPlaylist(p) => {
            let mut r = new(&p.id, ExportKind::LibraryPlaylist);
            hints(&mut r, &p.header);
            r
        }
        DownloadTarget::Item(item) => {
            let kind = match item.kind {
                ResourceType::Songs => ExportKind::Song,
                ResourceType::MusicVideos => ExportKind::MusicVideo,
                ResourceType::Albums => ExportKind::Album,
                ResourceType::Playlists => ExportKind::Playlist,
                ResourceType::LibraryPlaylists => ExportKind::LibraryPlaylist,
                _ => return Err(DownloadError::NotDownloadable),
            };
            new(&item.id, kind)
        }
    })
}

/// The jobs in run order, as the Downloads page lists them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadList {
    /// By `queuePos`: finished first in the order they were queued, then running and queued.
    pub jobs: Vec<ExportJob>,
}

impl DownloadList {
    /// Jobs that are queued or running (the "N active" count).
    pub fn active(&self) -> usize {
        self.jobs.iter().filter(|j| !j.phase.is_terminal()).count()
    }

    /// Done, failed or cancelled jobs ("Clear finished" shows while there are any).
    pub fn finished(&self) -> usize {
        self.jobs.len() - self.active()
    }

    fn set(&mut self, mut jobs: Vec<ExportJob>) {
        jobs.sort_by_key(|j| j.queue_pos);
        self.jobs = jobs;
    }

    fn upsert(&mut self, job: ExportJob) {
        let mut jobs = std::mem::take(&mut self.jobs);
        jobs.retain(|j| j.job_id != job.job_id);
        jobs.push(job);
        self.set(jobs);
    }

    /// Applies a progress event; `false` when the job is not listed (fetch the list).
    fn progress(&mut self, p: ExportProgress) -> bool {
        let Some(job) = self.jobs.iter_mut().find(|j| j.job_id == p.job_id) else {
            return false;
        };
        job.phase = p.phase;
        job.percent = p.percent;
        if !p.output.is_empty() {
            job.output = p.output;
        }
        job.error = p.error;
        if p.phase != ExportPhase::Queued {
            job.queue_index = 0;
        }
        true
    }
}

struct Inner {
    client: EngineClient,
    api: Api,
    settings: watch::Receiver<Settings>,
    list: watch::Sender<DownloadList>,
    /// The engine can play lossless (the snapshot's capability).
    lossless: watch::Sender<bool>,
    /// Failed jobs whose automatic retry is scheduled or done.
    retried: Mutex<HashSet<String>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

/// The downloads model. Cheap to clone.
#[derive(Clone)]
pub struct Downloads(Arc<Inner>);

impl Downloads {
    /// Follows `events`. Must run inside the tokio runtime.
    pub fn start(
        client: EngineClient,
        api: Api,
        settings: watch::Receiver<Settings>,
        events: broadcast::Receiver<EngineEvent>,
    ) -> Self {
        let me = Self(Arc::new(Inner {
            client,
            api,
            settings,
            list: watch::channel(DownloadList::default()).0,
            lossless: watch::channel(false).0,
            retried: Mutex::default(),
            task: Mutex::new(None),
        }));
        let weak = Arc::downgrade(&me.0);
        let task = tokio::spawn(follow(weak, events));
        *me.0.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        me
    }

    pub fn list(&self) -> watch::Receiver<DownloadList> {
        self.0.list.subscribe()
    }

    /// Queues `target` with the current settings.
    pub async fn download(&self, target: DownloadTarget<'_>) -> Result<ExportJob, DownloadError> {
        let mut request = request(target, &self.0.settings.borrow(), *self.0.lossless.borrow())?;
        let locale = self.0.api.locale();
        request.storefront = locale.storefront;
        request.language = locale.language;
        let job = self.0.client.export(&request).await?;
        info!("download queued: {}", job.job_id);
        self.0.list.send_modify(|l| l.upsert(job.clone()));
        Ok(job)
    }

    /// Cancels a queued or running job; removes a finished one.
    pub async fn cancel(&self, job_id: &str) -> Result<(), DownloadError> {
        self.0.client.cancel_export(job_id).await?;
        self.refetch().await;
        Ok(())
    }

    /// A failed or cancelled job again, as a new job.
    pub async fn retry(&self, job_id: &str) -> Result<ExportJob, DownloadError> {
        let job = self.0.client.retry_export(job_id).await?;
        self.0.list.send_modify(|l| l.upsert(job.clone()));
        Ok(job)
    }

    /// Higher runs sooner; only queued jobs move.
    pub async fn set_priority(&self, job_id: &str, priority: i32) -> Result<(), DownloadError> {
        self.0.client.prioritize_export(job_id, priority).await?;
        // Every queued job's place may have changed.
        self.refetch().await;
        Ok(())
    }

    /// Removes every finished job from the list ("Clear").
    pub async fn clear_finished(&self) {
        let finished: Vec<String> = self
            .0
            .list
            .borrow()
            .jobs
            .iter()
            .filter(|j| j.phase.is_terminal())
            .map(|j| j.job_id.clone())
            .collect();
        for id in finished {
            if let Err(e) = self.0.client.cancel_export(&id).await {
                debug!("remove download {id}: {e}");
            }
        }
        self.refetch().await;
    }

    async fn refetch(&self) {
        match self.0.client.exports().await {
            Ok(jobs) => self.0.list.send_modify(|l| l.set(jobs)),
            Err(e) => debug!("download list: {e}"),
        }
        self.schedule_retries();
    }

    /// Schedules the automatic retry of each failed job not retried yet.
    fn schedule_retries(&self) {
        let downloads = self.0.settings.borrow().downloads.clone();
        if !downloads.retry_on_fail {
            return;
        }
        let delay = Duration::from_secs(u64::from(downloads.retry_after_secs));
        let failed: Vec<String> = {
            let mut retried = self.0.retried.lock().unwrap_or_else(|e| e.into_inner());
            self.0
                .list
                .borrow()
                .jobs
                .iter()
                .filter(|j| j.phase == ExportPhase::Failed && retried.insert(j.job_id.clone()))
                .map(|j| j.job_id.clone())
                .collect()
        };
        for id in failed {
            let weak = Arc::downgrade(&self.0);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let Some(inner) = weak.upgrade() else { return };
                let me = Downloads(inner);
                let still_failed =
                    me.0.list
                        .borrow()
                        .jobs
                        .iter()
                        .any(|j| j.job_id == id && j.phase == ExportPhase::Failed);
                let on = me.0.settings.borrow().downloads.retry_on_fail;
                if still_failed && on {
                    info!("retrying download {id}");
                    if let Err(e) = me.retry(&id).await {
                        debug!("retry download {id}: {e}");
                    }
                }
            });
        }
    }
}

async fn follow(weak: std::sync::Weak<Inner>, mut events: broadcast::Receiver<EngineEvent>) {
    loop {
        let event = events.recv().await;
        let Some(inner) = weak.upgrade() else { return };
        let me = Downloads(inner);
        match event {
            Ok(EngineEvent::Snapshot(s)) => {
                me.0.lossless.send_replace(s.capabilities.lossless);
                me.refetch().await;
            }
            Ok(EngineEvent::Export(p)) => {
                let known = me.0.list.send_if_modified(|l| l.progress(p));
                if known {
                    me.schedule_retries();
                } else {
                    me.refetch().await;
                }
            }
            Ok(EngineEvent::ReplayTruncated) | Err(broadcast::error::RecvError::Lagged(_)) => {
                me.refetch().await
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}
