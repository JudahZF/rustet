//! Response and request helpers shared by the handlers. They keep the Go engine's wire
//! details: JSON bodies end with a newline, errors are `text/plain` with a newline.

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A JSON response, as Go's `writeJSON` wrote it.
pub(crate) fn json(status: StatusCode, value: &impl Serialize) -> Response {
    let mut body = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    body.push(b'\n');
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

/// A plain-text error, as Go's `http.Error` wrote it.
pub(crate) fn text_error(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        format!("{msg}\n"),
    )
        .into_response()
}

/// A plain-text error answer. Handlers return it with `?`.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl std::fmt::Display) -> Self {
        Self {
            status,
            message: message.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        text_error(self.status, self.message)
    }
}

/// What a handler returns.
pub(crate) type ApiResult = Result<Response, ApiError>;

/// Why a JSON request body was refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BodyError {
    #[error("request body too large")]
    TooLarge,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Reads at most `limit` bytes of `body` and decodes the first JSON value in it. Like Go's
/// `json.Decoder` after `http.MaxBytesReader`, data after the first value is ignored.
pub(crate) async fn read_json<T: DeserializeOwned>(
    body: Body,
    limit: usize,
) -> Result<T, BodyError> {
    let bytes: Bytes = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| BodyError::TooLarge)?;
    let mut values = serde_json::Deserializer::from_slice(&bytes).into_iter::<T>();
    match values.next() {
        Some(v) => Ok(v?),
        // An empty body: decoding it gives serde's end-of-input error.
        None => Ok(serde_json::from_slice::<T>(&bytes)?),
    }
}

/// The first value of query parameter `key`, decoded; empty when absent (Go's
/// `URL.Query().Get`).
pub(crate) fn query_param(uri: &Uri, key: &str) -> String {
    uri.query()
        .and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == key))
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// The first language tag of `Accept-Language`, or `en-US`.
pub(crate) fn lang(headers: &HeaderMap) -> String {
    headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|al| {
            let tag = al.split(',').next()?.split(';').next()?.trim();
            (!tag.is_empty()).then(|| tag.to_owned())
        })
        .unwrap_or_else(|| "en-US".to_owned())
}

/// The leading integer of `s`, as Go's `fmt.Sscanf(s, "%d", &n)` reads it: optional spaces, an
/// optional sign, digits; anything after them is ignored. `None` when there are no digits.
pub(crate) fn scan_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    rest[..digits].parse::<i64>().ok().map(|n| sign * n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_int_reads_the_leading_integer() {
        for (s, want) in [
            ("300", Some(300)),
            ("300px", Some(300)),
            (" 12", Some(12)),
            ("-5", Some(-5)),
            ("+7", Some(7)),
            ("", None),
            ("abc", None),
            ("-", None),
            ("99999999999999999999", None),
        ] {
            assert_eq!(scan_int(s), want, "{s:?}");
        }
    }

    #[test]
    fn lang_takes_the_first_tag() {
        let mut h = HeaderMap::new();
        assert_eq!(lang(&h), "en-US");
        h.insert(
            header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("de-DE;q=0.9, en;q=0.8"),
        );
        assert_eq!(lang(&h), "de-DE");
        h.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static(" , en"));
        assert_eq!(lang(&h), "en-US");
    }

    #[tokio::test]
    async fn read_json_limits_and_ignores_trailing_data() {
        #[derive(serde::Deserialize, Debug)]
        struct B {
            a: i64,
        }
        let b: B = read_json(Body::from(r#"{"a":1} trailing"#), 64)
            .await
            .unwrap();
        assert_eq!(b.a, 1);
        assert!(matches!(
            read_json::<B>(Body::from(vec![b' '; 65]), 64).await,
            Err(BodyError::TooLarge)
        ));
        assert!(matches!(
            read_json::<B>(Body::empty(), 64).await,
            Err(BodyError::Json(_))
        ));
        // Exactly at the limit is accepted.
        let exact = format!("{{\"a\":2}}{}", " ".repeat(64 - 7));
        assert_eq!(read_json::<B>(Body::from(exact), 64).await.unwrap().a, 2);
    }
}
