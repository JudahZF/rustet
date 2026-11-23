//! A music video's frames from the engine's frame ring (spec §4.9, `rustet_framering`).
//!
//! A background task owns the ring's [`Reader`]: it polls the ring (one atomic load when nothing
//! is new) and copies each new frame into a fresh buffer, the only copy of the pixels on this
//! side. The buffer becomes the frame's gpui image as is: the ring's BGRA rows are what gpui
//! uploads.
//!
//! One image per frame: on gpui's OpenGL path each image upload costs a fixed 25–30 ms of the
//! main thread whatever its size (measured with the `video` scenario), so cutting a frame into
//! atlas-sized tiles made it slower, and a smaller frame barely helps.
//!
//! The view is redrawn once per new frame and never when the ring has nothing new, so a paused
//! video costs nothing and keeps its last frame. A new ring of the same video (a seek, a new
//! frame size) keeps the last frame until its own first frame; another video starts clean. A
//! ring that ends keeps its last frame until the next ring or until the source goes away; a
//! writer that died clears it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};
use gpui::{
    AnyWindowHandle, Context, ObjectFit, RenderImage, Role, Task, Window, div, img, prelude::*,
};
use rustet_framering::{End, Read, Reader};
use rustet_player::model::VideoSurface;

use crate::theme::ActiveTheme;

/// How often the reader looks for a new frame: well under a frame at 60 fps.
const POLL: Duration = Duration::from_millis(4);
/// How long the reader waits for a ring file that is not there yet.
const OPEN_TRIES: u32 = 25;
const OPEN_RETRY: Duration = Duration::from_millis(40);

/// What the view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSource {
    pub surface: VideoSurface,
    /// Which video the ring belongs to (the queue item). A new ring of the same video keeps the
    /// last frame until its first one; another video starts clean.
    pub video: u64,
}

/// Where the view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No source.
    Idle,
    /// The ring is open; its first frame has not come yet.
    Waiting,
    /// Frames are coming (or the video is paused on one).
    Playing,
    /// The ring ended (the video stopped, or a ring of another size follows): the last frame stays.
    Held,
    /// The ring could not be read or its writer died: nothing to show.
    Lost,
}

/// Counters for checks and measurements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoStats {
    /// Frames turned into images.
    pub presented: u64,
    /// The ring's number of the last frame shown: gaps are frames the view skipped.
    pub last_number: u64,
}

enum Msg {
    Frame {
        width: u32,
        height: u32,
        number: u64,
        pixels: Vec<u8>,
    },
    Ended(End),
    Failed,
}

/// The frame on screen.
struct Shown {
    width: u32,
    height: u32,
    image: Arc<RenderImage>,
}

pub struct VideoView {
    window: AnyWindowHandle,
    source: Option<VideoSource>,
    phase: Phase,
    frame: Option<Shown>,
    /// Images to free from gpui's atlas at the next draw (where the window is at hand).
    retired: Vec<Arc<RenderImage>>,
    stats: VideoStats,
    _pump: Option<(Task<()>, Task<()>)>,
}

impl VideoView {
    pub fn new(window: &mut Window, _: &mut Context<Self>) -> Self {
        Self {
            window: window.window_handle(),
            source: None,
            phase: Phase::Idle,
            frame: None,
            retired: Vec::new(),
            stats: VideoStats::default(),
            _pump: None,
        }
    }

    /// Shows `source`, or nothing. The same ring again changes nothing.
    pub fn set_source(&mut self, source: Option<VideoSource>, cx: &mut Context<Self>) {
        if source == self.source {
            return;
        }
        let other_video = source.as_ref().map(|s| s.video) != self.source.as_ref().map(|s| s.video);
        self._pump = None;
        self.source = source.clone();
        let Some(source) = source else {
            // The last frame is kept for a return to the same video.
            self.phase = Phase::Idle;
            cx.notify();
            return;
        };
        if other_video {
            self.clear();
        }
        self.phase = if self.frame.is_some() {
            Phase::Playing
        } else {
            Phase::Waiting
        };
        self._pump = Some(self.pump(PathBuf::from(&source.surface.ring), cx));
        cx.notify();
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The size of the frame shown.
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        self.frame.as_ref().map(|f| (f.width, f.height))
    }

    pub fn stats(&self) -> VideoStats {
        self.stats
    }

    fn clear(&mut self) {
        if let Some(f) = self.frame.take() {
            self.retired.push(f.image);
        }
    }

    /// The reader on a background thread, and the task that hands its frames to the view.
    fn pump(&self, path: PathBuf, cx: &mut Context<Self>) -> (Task<()>, Task<()>) {
        // One frame in flight: a slow window makes the reader skip to the newest frame.
        let (mut tx, mut rx) = mpsc::channel::<Msg>(0);
        let executor = cx.background_executor().clone();
        let reader = cx.background_spawn(async move {
            let mut reader = None;
            let mut error = String::new();
            for _ in 0..OPEN_TRIES {
                match Reader::open(&path) {
                    Ok(r) => {
                        reader = Some(r);
                        break;
                    }
                    Err(e) => error = e.to_string(),
                }
                executor.timer(OPEN_RETRY).await;
            }
            let Some(mut reader) = reader else {
                tracing::warn!(path = %path.display(), %error, "video: the frame ring cannot be opened");
                tx.send(Msg::Failed).await.ok();
                return;
            };
            let format = reader.format();
            loop {
                // A fresh buffer each frame: it becomes the image, without another copy.
                let mut pixels = Vec::new();
                let msg = loop {
                    match reader.read(&mut pixels) {
                        Read::Frame(f) => {
                            break Msg::Frame {
                                width: format.width,
                                height: format.height,
                                number: f.number,
                                pixels,
                            };
                        }
                        Read::Unchanged => executor.timer(POLL).await,
                        Read::Ended(end) => break Msg::Ended(end),
                    }
                };
                let ended = matches!(msg, Msg::Ended(_));
                if tx.send(msg).await.is_err() || ended {
                    return;
                }
            }
        });
        let window = self.window;
        let ui = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let delivered = cx.update_window(window, |_, window, cx| {
                    this.update(cx, |view, cx| view.receive(msg, window, cx))
                });
                if !matches!(delivered, Ok(Ok(()))) {
                    break;
                }
            }
        });
        (reader, ui)
    }

    fn receive(&mut self, msg: Msg, window: &mut Window, cx: &mut Context<Self>) {
        match msg {
            Msg::Frame {
                width,
                height,
                number,
                pixels,
            } => {
                // `RgbaImage` is only the container: gpui uploads its bytes as BGRA.
                let Some(buffer) = image::RgbaImage::from_raw(width, height, pixels) else {
                    return;
                };
                let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
                if let Some(old) = self.frame.replace(Shown {
                    width,
                    height,
                    image,
                }) {
                    window.drop_image(old.image).ok();
                }
                self.phase = Phase::Playing;
                self.stats.presented += 1;
                self.stats.last_number = number;
            }
            Msg::Ended(End::Closed | End::Superseded) => self.phase = Phase::Held,
            Msg::Ended(End::Stale) | Msg::Failed => {
                self.clear();
                self.phase = Phase::Lost;
            }
        }
        cx.notify();
    }
}

impl Render for VideoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired.drain(..) {
            window.drop_image(image).ok();
        }
        let theme = cx.theme();
        let message = match (self.frame.is_some(), self.phase) {
            (true, _) => None,
            (false, Phase::Lost) => Some("The video stopped."),
            (false, _) => Some("Loading the video…"),
        };
        let frame = self.frame.as_ref().map(|f| f.image.clone());
        div()
            .id("video")
            .role(Role::Image)
            .aria_label("Music video")
            .relative()
            .size_full()
            .overflow_hidden()
            // Letterboxed on the base colour.
            .bg(theme.base())
            .when_some(frame, |d, image| {
                // Out of the layout: the frame's own size must not size the view.
                d.child(
                    img(image)
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
            })
            .when_some(message, |d, text| {
                d.child(
                    div()
                        .id("video-state")
                        .role(Role::Status)
                        .aria_label(text)
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.colors.text_secondary)
                        .child(text),
                )
            })
    }
}
