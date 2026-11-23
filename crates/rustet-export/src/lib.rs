//! Track and music-video export: jobs, queue, tagging, filename templates.
//!
//! A job downloads one track (or expands an album or playlist into one job per track),
//! decrypts it through a private playback session, tags it, optionally converts it to FLAC, and
//! moves it to a path built from a filename template, never replacing a file unless the
//! overwrite policy says so. Jobs run one at a time from a priority queue on a [`Manager`]; their
//! progress is reported as [`ExportEvent`]s, which the engine forwards over SSE.
//!
//! The engine's handlers map onto the manager directly:
//!
//! | Route | Call |
//! |---|---|
//! | `POST /api/v1/export` | [`ExportRequest::from_json`], [`Manager::enqueue`] (202 + [`ExportJob`]) |
//! | `GET /api/v1/export` | [`Manager::list`] |
//! | `GET /api/v1/export/{id}` | [`Manager::get`] |
//! | `DELETE /api/v1/export/{id}` | [`Manager::cancel`] |
//! | `POST /api/v1/export/{id}/retry` | [`Manager::retry`] |
//! | `POST /api/v1/export/{id}/priority` | [`Manager::prioritize`] |
//! | `GET /api/v1/tools` | [`probe_ffmpeg`] |
#![forbid(unsafe_code)]

mod cache;
mod ffmpeg;
mod fs;
mod job;
mod manager;
mod path;
mod probe;
mod tagger;
pub mod template;
mod throttle;
mod tool;

use std::path::PathBuf;

pub use cache::{AudioCache, TailReader};
pub use fs::{OutputDirError, OverwritePolicy};
pub use job::{
    ExportCapabilities, ExportEvent, ExportJob, ExportOptions, ExportRequest, Phase, Source,
};
pub use manager::{EnqueueError, Manager, Options, PrioritizeError};
pub use probe::{FfmpegInfo, probe_ffmpeg};
pub use tagger::{ArtworkError, TagError};
pub use tool::{ToolError, ToolTrustError};

/// Receives export events. It is called outside the manager's locks.
pub type EventSink = Box<dyn Fn(ExportEvent) + Send + Sync>;

/// Supplies output roots added after start-up.
pub type MoreRoots = Box<dyn Fn() -> Vec<PathBuf> + Send + Sync>;
