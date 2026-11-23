//! The bridge between gpui's executors and tokio.
//!
//! gpui runs views on its own foreground executor; the workspace crates (reqwest, the engine
//! client) need a tokio runtime. The UI owns one small multi-thread runtime, stored as a gpui
//! global. Network and disk work runs there; results come back to a view through a gpui task:
//!
//! ```ignore
//! let job = rustet_ui::runtime::spawn(cx, async move { client.get(url).send().await });
//! self.loading = cx.spawn(async move |this, cx| {
//!     let result = job.await;
//!     this.update(cx, |view, cx| { view.result = Some(result); cx.notify() }).ok();
//! });
//! ```
//!
//! Cancellation follows ownership: the view stores the gpui `Task`; dropping the view drops the
//! task, which drops the [`TokioTask`], which aborts the tokio task. Nothing blocks the UI
//! thread: awaiting a tokio `JoinHandle` from gpui's executor only parks that gpui task.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use gpui::{App, Global};
use tokio::runtime::{Handle, Runtime};
use tokio::task::{JoinError, JoinHandle};

/// The UI's tokio runtime, as a gpui global.
pub struct Tokio {
    // Kept alive for the life of the app; `handle` spawns onto it.
    _runtime: Option<Runtime>,
    handle: Handle,
}

impl Global for Tokio {}

impl Tokio {
    /// Starts a two-worker runtime and installs it. Two workers are plenty: the UI's work is
    /// I/O bound, and image decoding goes to the blocking pool (at most four threads).
    pub fn install(cx: &mut App) -> std::io::Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            // Decoding is CPU work: a few threads keep it from crowding the UI thread's core.
            .max_blocking_threads(4)
            .thread_name("rustet-ui-io")
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        cx.set_global(Self {
            _runtime: Some(runtime),
            handle,
        });
        Ok(())
    }

    /// Installs an existing runtime's handle (tests, or a binary that already has a runtime).
    pub fn install_handle(cx: &mut App, handle: Handle) {
        cx.set_global(Self {
            _runtime: None,
            handle,
        });
    }

    pub fn handle(cx: &App) -> &Handle {
        &cx.global::<Self>().handle
    }
}

/// Runs `future` on the UI's tokio runtime. The returned future resolves to its output, and
/// aborts the tokio task when dropped.
pub fn spawn<F>(cx: &App, future: F) -> TokioTask<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    TokioTask(Tokio::handle(cx).spawn(future))
}

/// Runs blocking work (decoding, file I/O) on tokio's blocking pool.
pub fn spawn_blocking<F, T>(cx: &App, f: F) -> TokioTask<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    TokioTask(Tokio::handle(cx).spawn_blocking(f))
}

/// A tokio task that is aborted when this handle is dropped.
#[must_use = "dropping a TokioTask aborts it"]
pub struct TokioTask<T>(JoinHandle<T>);

impl<T> Future for TokioTask<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for TokioTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
