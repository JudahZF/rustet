//! An in-memory cache of catalog responses, so going back to a page does not hit the
//! network.
//!
//! It follows the response headers: `Cache-Control: max-age` (less `Age`) sets how long an
//! answer is fresh; `no-store` is not kept. A stale answer with an `ETag` is revalidated
//! with `If-None-Match`. The total body size is capped; the least recently used answers go
//! first. Keys are full URLs, so storefront and language are part of the key.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::header::{AGE, CACHE_CONTROL, ETAG, HeaderMap, HeaderValue};

pub(crate) struct ResponseCache {
    capacity: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    bytes: usize,
    /// Increases on every use; the smallest `used` is the least recently used entry.
    clock: u64,
}

struct Entry {
    body: Bytes,
    etag: Option<HeaderValue>,
    fresh_until: Instant,
    used: u64,
}

pub(crate) enum Lookup {
    Fresh(Bytes),
    /// Stale, but Apple can confirm it is unchanged.
    Stale(HeaderValue),
    Miss,
}

impl ResponseCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // The state stays consistent even if a holder panicked: every update is one step.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn lookup(&self, key: &str) -> Lookup {
        let mut state = self.state();
        state.clock += 1;
        let clock = state.clock;
        let Some(entry) = state.entries.get_mut(key) else {
            return Lookup::Miss;
        };
        entry.used = clock;
        if Instant::now() < entry.fresh_until {
            return Lookup::Fresh(entry.body.clone());
        }
        if let Some(etag) = &entry.etag {
            return Lookup::Stale(etag.clone());
        }
        state.remove(key);
        Lookup::Miss
    }

    /// Keeps a 200 answer as its headers allow.
    pub(crate) fn store(&self, key: String, headers: &HeaderMap, body: Bytes) {
        let mut state = self.state();
        state.remove(&key);
        let Some(fresh_for) = freshness(headers) else {
            return;
        };
        let etag = headers.get(ETAG).cloned();
        if (fresh_for.is_zero() && etag.is_none()) || body.len() > self.capacity {
            return;
        }
        state.clock += 1;
        state.bytes += body.len();
        let entry = Entry {
            fresh_until: Instant::now() + fresh_for,
            used: state.clock,
            etag,
            body,
        };
        state.entries.insert(key, entry);
        while state.bytes > self.capacity {
            let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            state.remove(&oldest);
        }
    }

    /// Apple answered 304 to a revalidation: the kept body is current again.
    pub(crate) fn revalidated(&self, key: &str, headers: &HeaderMap) -> Option<Bytes> {
        let mut state = self.state();
        let entry = state.entries.get_mut(key)?;
        entry.fresh_until = Instant::now() + freshness(headers).unwrap_or_default();
        Some(entry.body.clone())
    }

    pub(crate) fn clear(&self) {
        *self.state() = State::default();
    }
}

impl State {
    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes -= entry.body.len();
        }
    }
}

/// How long an answer stays fresh, or `None` when it must not be kept.
fn freshness(headers: &HeaderMap) -> Option<Duration> {
    let cache_control = headers.get(CACHE_CONTROL)?.to_str().ok()?;
    let mut max_age: Option<u64> = None;
    for directive in cache_control.split(',').map(str::trim) {
        let (name, value) = directive.split_once('=').unwrap_or((directive, ""));
        match name.to_ascii_lowercase().as_str() {
            "no-store" => return None,
            "no-cache" => max_age = Some(0),
            "max-age" if max_age.is_none() => max_age = value.trim_matches('"').parse().ok(),
            _ => {}
        }
    }
    let age: u64 = headers
        .get(AGE)
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .unwrap_or(0);
    Some(Duration::from_secs(max_age?.saturating_sub(age)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| (k.parse().unwrap(), HeaderValue::from_static(v)))
            .collect()
    }

    #[test]
    fn freshness_follows_cache_control_and_age() {
        let f = |pairs| freshness(&headers(pairs));
        assert_eq!(
            f(&[("cache-control", "max-age=60, public, no-transform")]),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            f(&[("cache-control", "max-age=60"), ("age", "52")]),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            f(&[("cache-control", "max-age=60, s-maxage=0, private")]),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            f(&[(
                "cache-control",
                "private, no-cache, no-store, no-transform, must-revalidate, max-age=0"
            )]),
            None
        );
        assert_eq!(f(&[]), None);
    }

    #[test]
    fn fresh_stale_and_eviction() {
        let cache = ResponseCache::new(10);
        let fresh = headers(&[("cache-control", "max-age=60"), ("etag", "\"a\"")]);
        let stale = headers(&[("cache-control", "max-age=0"), ("etag", "\"b\"")]);
        cache.store("a".into(), &fresh, Bytes::from_static(b"aaaa"));
        cache.store("b".into(), &stale, Bytes::from_static(b"bbbb"));
        assert!(matches!(cache.lookup("a"), Lookup::Fresh(b) if &b[..] == b"aaaa"));
        assert!(matches!(cache.lookup("b"), Lookup::Stale(e) if e == "\"b\""));
        assert_eq!(
            cache.revalidated("b", &fresh).as_deref(),
            Some(&b"bbbb"[..])
        );
        assert!(matches!(cache.lookup("b"), Lookup::Fresh(_)));

        // "a" was used before "b": it goes when "c" needs the room.
        cache.store("c".into(), &fresh, Bytes::from_static(b"cccc"));
        assert!(matches!(cache.lookup("a"), Lookup::Miss));
        assert!(matches!(cache.lookup("b"), Lookup::Fresh(_)));
        assert!(matches!(cache.lookup("c"), Lookup::Fresh(_)));

        // Too large for the cache, and no-store: not kept.
        cache.store("d".into(), &fresh, Bytes::from_static(b"01234567890"));
        cache.store(
            "e".into(),
            &headers(&[("cache-control", "no-store")]),
            Bytes::new(),
        );
        assert!(matches!(cache.lookup("d"), Lookup::Miss));
        assert!(matches!(cache.lookup("e"), Lookup::Miss));
        assert_eq!(cache.state().bytes, 8);
    }
}
