//! Tokens and the developer-token cache.
//!
//! Neither token type prints its value: `Debug` shows a placeholder, and there is no
//! `Display`. Code that must send a token calls `secret()`.

use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{Client, Error};

/// Re-scrape when this share of the token's life has passed (risk R1 of the spec).
const REFRESH_AT_LIFE: f64 = 0.8;
/// A token whose expiry cannot be read is re-scraped after this long.
const UNKNOWN_LIFE: Duration = Duration::from_secs(24 * 3600);
/// A scraped token is used at least this long, so a token Apple serves late in its life
/// does not cause a scrape per request.
const MIN_REUSE: Duration = Duration::from_secs(10 * 60);
/// A 401 re-scrapes only a token older than this, so one expired user token does not
/// make every call scrape the web player.
const MIN_AGE_FOR_RESCRAPE: Duration = Duration::from_secs(5 * 60);

/// The web player's developer token (a JWT, about 70 days of life).
#[derive(Clone, PartialEq, Eq)]
pub struct DeveloperToken(String);

impl DeveloperToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token, for an `Authorization` header. Never log it.
    pub fn secret(&self) -> &str {
        &self.0
    }

    /// The `exp` claim, when the token is a readable JWT.
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.claims().map(|(_, exp)| exp)
    }

    /// `(iat, exp)` from the JWT payload. The signature is not checked: Apple does that.
    fn claims(&self) -> Option<(SystemTime, SystemTime)> {
        #[derive(Deserialize)]
        struct Claims {
            iat: u64,
            exp: u64,
        }
        let payload = self.0.split('.').nth(1)?;
        let json = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
        let claims: Claims = serde_json::from_slice(&json).ok()?;
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        (claims.exp > claims.iat).then(|| (at(claims.iat), at(claims.exp)))
    }

    /// When to replace the token, seen from `now`.
    fn refresh_at(&self, now: SystemTime) -> SystemTime {
        let Some((iat, exp)) = self.claims() else {
            return now + UNKNOWN_LIFE;
        };
        let life = exp.duration_since(iat).unwrap_or_default();
        (iat + life.mul_f64(REFRESH_AT_LIFE)).max(now + MIN_REUSE)
    }
}

impl fmt::Debug for DeveloperToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeveloperToken(<redacted>)")
    }
}

/// The listener's music user token (`media-user-token` from the web sign-in).
#[derive(Clone, PartialEq, Eq)]
pub struct UserToken(String);

impl UserToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token, for a `Media-User-Token` header. Never log it.
    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UserToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserToken(<redacted>)")
    }
}

struct Cached {
    token: DeveloperToken,
    obtained: Instant,
    refresh_at: SystemTime,
}

impl Cached {
    fn new(token: DeveloperToken) -> Self {
        Self {
            refresh_at: token.refresh_at(SystemTime::now()),
            token,
            obtained: Instant::now(),
        }
    }
}

/// The developer token, scraped on first use and again when it ages or is refused.
#[derive(Default)]
pub(crate) struct DeveloperTokenCache {
    // An async mutex: concurrent callers wait for one scrape instead of starting their own.
    slot: Mutex<Option<Cached>>,
}

impl DeveloperTokenCache {
    pub(crate) async fn get(
        &self,
        client: &Client,
        cancel: &CancellationToken,
    ) -> Result<DeveloperToken, Error> {
        let mut slot = self.slot.lock().await;
        if let Some(cached) = slot.as_ref()
            && SystemTime::now() < cached.refresh_at
        {
            return Ok(cached.token.clone());
        }
        let token = cancel
            .run_until_cancelled(client.token())
            .await
            .ok_or(Error::Cancelled)?;
        let token = match token {
            Ok(token) => DeveloperToken::new(token),
            // An aged token still works until Apple refuses it.
            Err(err) => match slot.as_mut() {
                Some(cached) => {
                    tracing::warn!("developer token refresh failed, keeping the old one: {err}");
                    cached.refresh_at = SystemTime::now() + MIN_REUSE;
                    return Ok(cached.token.clone());
                }
                None => return Err(err),
            },
        };
        tracing::debug!(expires = ?token.expires_at(), "developer token scraped");
        *slot = Some(Cached::new(token.clone()));
        Ok(token)
    }

    pub(crate) async fn set(&self, token: DeveloperToken) {
        *self.slot.lock().await = Some(Cached::new(token));
    }

    /// Makes the cached token look obtained `by` earlier.
    #[cfg(test)]
    pub(crate) async fn age(&self, by: Duration) {
        if let Some(cached) = self.slot.lock().await.as_mut() {
            cached.obtained -= by;
        }
    }

    /// Apple refused `rejected` with 401. Drops it so the next [`get`](Self::get)
    /// scrapes, unless it was obtained moments ago. Returns whether a retry may help.
    pub(crate) async fn refused(&self, rejected: &DeveloperToken) -> bool {
        let mut slot = self.slot.lock().await;
        match slot.as_ref() {
            Some(cached) if cached.token != *rejected => true,
            Some(cached) if cached.obtained.elapsed() < MIN_AGE_FOR_RESCRAPE => false,
            _ => {
                *slot = None;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(iat: u64, exp: u64) -> DeveloperToken {
        let payload =
            URL_SAFE_NO_PAD.encode(format!(r#"{{"iss":"AMPWebPlay","iat":{iat},"exp":{exp}}}"#));
        DeveloperToken::new(format!("eyJhbGciOiJFUzI1NiJ9.{payload}.sig"))
    }

    #[test]
    fn refresh_at_80_percent_of_life_but_not_sooner_than_min_reuse() {
        let at = |s| UNIX_EPOCH + Duration::from_secs(s);
        let token = jwt(1_000_000, 1_000_000 + 100 * 3600);
        assert_eq!(token.expires_at(), Some(at(1_000_000 + 100 * 3600)));
        assert_eq!(token.refresh_at(at(1_000_000)), at(1_000_000 + 80 * 3600));
        // Scraped at 95 % of its life: kept for MIN_REUSE, not re-scraped at once.
        let late = at(1_000_000 + 95 * 3600);
        assert_eq!(token.refresh_at(late), late + MIN_REUSE);
        // Unreadable: a day.
        let opaque = DeveloperToken::new("opaque");
        assert_eq!(opaque.expires_at(), None);
        assert_eq!(opaque.refresh_at(at(5)), at(5) + UNKNOWN_LIFE);
    }

    #[test]
    fn tokens_never_print() {
        let dev = format!("{:?}", DeveloperToken::new("eyJsecret"));
        let user = format!("{:?}", UserToken::new("mut-secret"));
        assert!(!dev.contains("secret") && !user.contains("secret"));
    }

    #[tokio::test]
    async fn a_refused_token_is_dropped_only_when_not_fresh() {
        let cache = DeveloperTokenCache::default();
        let token = DeveloperToken::new("a");
        cache.set(token.clone()).await;
        assert!(!cache.refused(&token).await, "just obtained: no re-scrape");
        assert!(cache.slot.lock().await.is_some());
        cache.age(MIN_AGE_FOR_RESCRAPE).await;
        assert!(cache.refused(&token).await);
        assert!(cache.slot.lock().await.is_none());
        cache.set(DeveloperToken::new("b")).await;
        assert!(
            cache.refused(&token).await,
            "already replaced: retry with the new one"
        );
    }
}
