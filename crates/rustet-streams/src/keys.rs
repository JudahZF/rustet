//! Content keys from Apple's Widevine licence endpoint, cached for the process lifetime.

use std::sync::Arc;
use std::time::Duration;

use rustet_widevine::Identity;
use rustet_widevine::wvkey::{self, ContentKey, LicenceRequest};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{Error, Streams, blocking};

/// The licence server's host, contacted once at startup so the first key request skips the TLS handshake.
const LICENCE_HOST: &str = "https://play.itunes.apple.com/";
const WARM_TIMEOUT: Duration = Duration::from_secs(8);

/// The key-cache key of one track's content key.
fn cache_key(kid_base64: &str, uri_prefix: &str) -> String {
    format!("{kid_base64}\0{uri_prefix}")
}

impl Streams {
    /// The content key for one track, from the cache or from the licence server.
    ///
    /// With `force_refresh` the cache is bypassed and a fresh licence is acquired: use it after a decryption failure
    /// to recover from a stale key. A successful answer replaces the cached key. Key bytes must never reach a field
    /// that is serialised or returned to API clients.
    pub(crate) async fn acquire_key(
        &self,
        req: &LicenceRequest<'_>,
        force_refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<ContentKey, Error> {
        let ck = cache_key(req.kid_base64, req.uri_prefix);
        if !force_refresh && let Some(key) = self.lock_keys().get(&ck) {
            return Ok(key.clone());
        }
        let identity = self
            .identity
            .get_or_init(|| blocking(|| Arc::new(Identity::load())))
            .await;
        let key = wvkey::acquire_key(
            &self.licence_client,
            &self.licence_url,
            identity,
            req,
            cancel,
        )
        .await
        .map_err(Error::Licence)?;
        if !key.key.is_empty() {
            self.lock_keys().insert(ck, key.clone());
        }
        Ok(key)
    }

    /// Evicts a cached content key, so the next request for it goes to the licence server. Call it when a segment
    /// fails to decrypt: the cached key may be stale.
    pub fn invalidate_key(&self, kid_base64: &str, uri_prefix: &str) {
        self.lock_keys().remove(&cache_key(kid_base64, uri_prefix));
    }

    /// Opens a connection to the licence server so the first licence request skips the handshake. Call once at
    /// engine startup, from a background task.
    pub async fn warm_licence_pool(&self, cancel: &CancellationToken) {
        let warm = tokio::time::timeout(WARM_TIMEOUT, self.licence_client.get(LICENCE_HOST).send());
        tokio::select! {
            () = cancel.cancelled() => {}
            res = warm => if let Ok(Ok(resp)) = res {
                debug!(status = resp.status().as_u16(), "licence pool warmed");
            },
        }
    }

    fn lock_keys(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, ContentKey>> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn has_cached_key(&self, kid_base64: &str, uri_prefix: &str) -> bool {
        self.lock_keys()
            .contains_key(&cache_key(kid_base64, uri_prefix))
    }

    #[cfg(test)]
    pub(crate) fn seed_key(&self, kid_base64: &str, uri_prefix: &str, key: ContentKey) {
        self.lock_keys()
            .insert(cache_key(kid_base64, uri_prefix), key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_separate_kid_and_uri() {
        assert_eq!(cache_key("kid1", "uri1"), cache_key("kid1", "uri1"));
        assert_ne!(cache_key("kid1", "uri1"), cache_key("kid2", "uri1"));
        assert_ne!(cache_key("kid1", "uri1"), cache_key("kid1", "uri2"));
        // The separator keeps the boundary: "ab"+"c" and "a"+"bc" differ.
        assert_ne!(cache_key("ab", "c"), cache_key("a", "bc"));
    }

    #[tokio::test]
    async fn a_cached_key_skips_the_licence_server() {
        let tmp = tempfile::tempdir().unwrap();
        // Nothing listens there: a request would fail.
        let s = Streams::for_tests(tmp.path(), "http://127.0.0.1:9/licence");
        let key = ContentKey {
            key: vec![7; 16],
            iv: [0; 16],
        };
        s.seed_key("kid", "skd://x", key.clone());
        let req = LicenceRequest {
            adam_id: "1",
            kid_base64: "kid",
            uri_prefix: "skd://x",
            token: "t",
            media_user_token: "m",
        };
        let got = s
            .acquire_key(&req, false, &CancellationToken::new())
            .await
            .unwrap();
        assert!(got == key);

        // force_refresh bypasses the cache (and fails here, with no server).
        assert!(matches!(
            s.acquire_key(&req, true, &CancellationToken::new()).await,
            Err(Error::Licence(_))
        ));

        s.invalidate_key("kid", "skd://x");
        assert!(!s.has_cached_key("kid", "skd://x"));
        s.invalidate_key("kid", "skd://x"); // idempotent
        s.invalidate_key("never-stored", "uri");
    }
}
