//! Streaming response bodies: pipeline output, cache files and growing cache entries.
//!
//! Every streaming body holds a [`DropGuard`] of the request's [`CancellationToken`], so the work
//! behind it stops when the client goes away (Go's request context).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt};
use rustet_core::pipeline::Sink;
use rustet_diskcache::StreamingReader;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWrite, DuplexStream};
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard, PollSender};
use tracing::{error, warn};

use crate::http::json;

/// Buffer of the pipes between a pipeline and the code that consumes its output.
pub(crate) const PIPE_BUFFER: usize = 64 * 1024;
/// Read size for copies, the size of Go's `io.Copy` buffer.
pub(crate) const CHUNK: usize = 32 * 1024;

/// A body over `stream` that cancels `guard`'s token when it is dropped.
pub(crate) fn guarded_body<S>(stream: S, guard: DropGuard) -> Body
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    Body::from_stream(stream.map(move |item| {
        let _keep = &guard;
        item
    }))
}

/// The rest of a growing cache entry as a body. A read error ends the body cleanly, as Go's
/// `io.Copy` into the response did: the client sees a short body, not a reset. Dropping the body
/// drops the reader; the download it follows runs on.
pub(crate) fn reader_body(reader: StreamingReader) -> Body {
    let stream = reader.into_stream().scan((), |(), item| {
        futures::future::ready(
            item.inspect_err(|e| warn!("cache stream ended early: {e}"))
                .ok(),
        )
    });
    Body::from_stream(stream.map(Ok::<_, std::convert::Infallible>))
}

/// A [`Sink`] that forwards writes over a channel and records the headers set before the first
/// byte. The pipeline side of [`stream_media`].
pub(crate) struct ChannelSink {
    tx: PollSender<Bytes>,
    headers: Arc<Mutex<Vec<(String, String)>>>,
    started: bool,
}

impl AsyncWrite for ChannelSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let broken = || io::Error::new(io::ErrorKind::BrokenPipe, "client went away");
        match self.tx.poll_reserve(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(_)) => return Poll::Ready(Err(broken())),
            Poll::Ready(Ok(())) => {}
        }
        self.started = true;
        match self.tx.send_item(Bytes::copy_from_slice(buf)) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(broken())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Sink for ChannelSink {
    fn set_header(&mut self, name: &str, value: &str) {
        if !self.started {
            self.headers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((name.to_owned(), value.to_owned()));
        }
    }
}

/// Streams what `run` writes into its sink as the response (Go's `streamMedia` with its
/// `firstByteWriter`). Headers wait for the first byte:
/// - an error before any byte is a 502 JSON `{"error": …}` instead of an empty 200;
/// - after the first byte the response is a 200 with `Content-Type: content_type`,
///   `Accept-Ranges: none` and the headers the source set; a later error just ends the body.
///
/// `run` executes in its own task; `cancel` stops it and is cancelled when the response or its
/// body is dropped (the client went away).
pub(crate) async fn stream_media<F, E>(
    cancel: CancellationToken,
    content_type: &'static str,
    run: impl FnOnce(ChannelSink) -> F,
) -> Response
where
    F: Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let guard = cancel.clone().drop_guard();
    let (tx, mut rx) = mpsc::channel::<Bytes>(16);
    let headers = Arc::new(Mutex::new(Vec::new()));
    let sink = ChannelSink {
        tx: PollSender::new(tx),
        headers: headers.clone(),
        started: false,
    };
    let fut = run(sink);
    let mut task = tokio::spawn(fut);

    let first = tokio::select! {
        // The channel first: a task that sent its bytes and finished must not read as empty.
        biased;
        chunk = rx.recv() => chunk,
        // Finished (or failed) without sending anything.
        res = &mut task => return empty_outcome(outcome(res), &cancel),
    };
    let Some(first) = first else {
        // The sink was dropped without a byte; the task's result decides.
        return empty_outcome(outcome(task.await), &cancel);
    };

    let mut map = HeaderMap::new();
    for (name, value) in headers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain(..)
    {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            map.insert(n, v);
        }
    }
    map.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    map.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));

    // Log a failure after the first byte; the body just ends there.
    let watch_cancel = cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = outcome(task.await)
            && !watch_cancel.is_cancelled()
        {
            error!("stream error (partial): {e}");
        }
    });
    let rest = futures::stream::unfold(
        rx,
        |mut rx| async move { rx.recv().await.map(|b| (Ok(b), rx)) },
    );
    let body = futures::stream::once(async { Ok(first) }).chain(rest);
    (StatusCode::OK, map, guarded_body(body, guard)).into_response()
}

fn outcome<E: std::fmt::Display>(
    res: Result<Result<(), E>, tokio::task::JoinError>,
) -> Result<(), String> {
    match res {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// The response of a stream that ended before its first byte: a 502 for a failure, an empty
/// 200 for a clean end or a client that went away.
fn empty_outcome(res: Result<(), String>, cancel: &CancellationToken) -> Response {
    match res {
        Err(e) if !cancel.is_cancelled() => json(StatusCode::BAD_GATEWAY, &json!({"error": e})),
        _ => StatusCode::OK.into_response(),
    }
}

/// A pipe between a pipeline (the first half) and the code that consumes its output.
pub(crate) fn pipe() -> (DuplexStream, DuplexStream) {
    tokio::io::duplex(PIPE_BUFFER)
}

// ── Byte ranges of a complete file (Go's http.ServeContent) ──────────────────

/// The single byte range a `Range` header asks for, resolved against `size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeRequest {
    /// No `Range` header, or several ranges (served whole, like a client that cannot use them).
    Full,
    /// `start..=end`.
    Partial { start: u64, end: u64 },
    /// Malformed.
    Invalid,
    /// Well-formed but outside the file.
    NoOverlap,
}

/// Parses a `Range` header the way Go's `http.ServeContent` does for one range.
pub(crate) fn parse_range(header: Option<&str>, size: u64) -> RangeRequest {
    let Some(h) = header.filter(|h| !h.is_empty()) else {
        return RangeRequest::Full;
    };
    let Some(spec) = h.strip_prefix("bytes=") else {
        return RangeRequest::Invalid;
    };
    let parts: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let [part] = parts[..] else {
        return if parts.is_empty() {
            RangeRequest::Invalid
        } else {
            RangeRequest::Full
        };
    };
    let Some((a, b)) = part.split_once('-') else {
        return RangeRequest::Invalid;
    };
    let (a, b) = (a.trim(), b.trim());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    if a.is_empty() {
        // bytes=-N: the last N bytes.
        let Some(n) = digits(b).then(|| b.parse::<u64>().ok()).flatten() else {
            return RangeRequest::Invalid;
        };
        if n == 0 {
            return RangeRequest::NoOverlap;
        }
        return RangeRequest::Partial {
            start: size.saturating_sub(n),
            end: size.saturating_sub(1),
        };
    }
    let Some(start) = digits(a).then(|| a.parse::<u64>().ok()).flatten() else {
        return RangeRequest::Invalid;
    };
    if start >= size {
        return RangeRequest::NoOverlap;
    }
    let end = if b.is_empty() {
        size - 1
    } else {
        let Some(end) = digits(b).then(|| b.parse::<u64>().ok()).flatten() else {
            return RangeRequest::Invalid;
        };
        if end < start {
            return RangeRequest::Invalid;
        }
        end.min(size - 1)
    };
    RangeRequest::Partial { start, end }
}

/// Serves a complete file with byte-range support: 200 with `Content-Length`, 206 with
/// `Content-Range`, 416 for a range outside the file. `Accept-Ranges: bytes` always.
pub(crate) async fn serve_file(
    file: tokio::fs::File,
    content_type: &'static str,
    range: Option<&str>,
) -> io::Result<Response> {
    use tokio::io::AsyncSeekExt;
    let size = file.metadata().await?.len();
    let (status, start, len, content_range) = match parse_range(range, size) {
        RangeRequest::Full => (StatusCode::OK, 0, size, None),
        RangeRequest::Partial { start, end } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{size}")),
        ),
        bad @ (RangeRequest::Invalid | RangeRequest::NoOverlap) => {
            let msg = if bad == RangeRequest::Invalid {
                "invalid range"
            } else {
                "invalid range: failed to overlap"
            };
            let mut resp = crate::http::text_error(StatusCode::RANGE_NOT_SATISFIABLE, msg);
            if let Ok(v) = HeaderValue::try_from(format!("bytes */{size}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return Ok(resp);
        }
    };
    let mut file = file;
    file.seek(io::SeekFrom::Start(start)).await?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
        file.take(len),
        CHUNK,
    ));
    let mut resp = (status, body).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Some(cr) = content_range.and_then(|v| HeaderValue::try_from(v).ok()) {
        h.insert(header::CONTENT_RANGE, cr);
    }
    Ok(resp)
}

/// The start offset of a `Range` header (`bytes=X-` or `bytes=X-Y`); 0 for anything else.
pub(crate) fn range_start(range: &str) -> u64 {
    range
        .strip_prefix("bytes=")
        .map(|s| s.split_once('-').map_or(s, |(a, _)| a))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_resolve_like_serve_content() {
        use RangeRequest::*;
        for (h, want) in [
            (None, Full),
            (Some(""), Full),
            (Some("bytes=0-"), Partial { start: 0, end: 9 }),
            (Some("bytes=2-4"), Partial { start: 2, end: 4 }),
            (Some("bytes=8-100"), Partial { start: 8, end: 9 }),
            (Some("bytes=-3"), Partial { start: 7, end: 9 }),
            (Some("bytes=-30"), Partial { start: 0, end: 9 }),
            (Some("bytes=10-"), NoOverlap),
            (Some("bytes=-0"), NoOverlap),
            (Some("bytes=4-2"), Invalid),
            (Some("bytes=a-"), Invalid),
            (Some("items=0-1"), Invalid),
            (Some("bytes=0-1,3-4"), Full),
        ] {
            assert_eq!(parse_range(h, 10), want, "{h:?}");
        }
    }

    #[test]
    fn range_start_takes_the_first_offset() {
        for (h, want) in [
            ("bytes=100-", 100),
            ("bytes=5-9", 5),
            ("bytes=-5", 0),
            ("x", 0),
            ("bytes=z-", 0),
        ] {
            assert_eq!(range_start(h), want, "{h}");
        }
    }
}
