//! The shell's side of the Apple Account: saved sign-in details in the store, sign-in again on
//! launch (`drm-autoauth.mjs`), and sign-out (`drm-signout.mjs`).
//!
//! Sign-out contract with the engine (`rustet-drm`, `logout_files.rs`): the shell never touches the
//! engine's session files. When the engine's sign-out route failed, the shell writes a new,
//! uniquely named request file `logout-pending.backstop-<32 hex>` containing `account` or
//! `device` into the session directory; the engine completes it on its next start.

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::engine::{EngineApi, EngineResponse};
use crate::store::Store;

const EMAIL: &str = "drm_email";
const PASSWORD: &str = "drm_password";
const VERIFIED: &str = "drm_verified";

/// Prefix of the engine's errors about this installation (missing libraries).
const SETUP_PREFIX: &str = "FairPlay setup: ";
const STATUS_PATH: &str = "/api/v1/drm/status";
const AUTH_PATH: &str = "/api/v1/drm/authenticate";

// ── Saved details ────────────────────────────────────────────────────────────

/// `drm:save-credentials`, called once Apple accepted a sign-in. The email (at most 256
/// characters) is kept to pre-fill the form; the password only with `remember: true`. Returns
/// whether it was saved.
pub async fn save_credentials(store: &Store, c: &Value) -> bool {
    let Some(email) = c
        .get("email")
        .and_then(Value::as_str)
        .filter(|e| e.encode_utf16().count() <= 256)
    else {
        return false;
    };
    let password = c.get("password").and_then(Value::as_str).filter(|p| {
        c.get("remember") == Some(&Value::Bool(true))
            && !p.is_empty()
            && p.encode_utf16().count() <= 256
    });
    store.update(|m| {
        m.insert(EMAIL.into(), Value::from(email.trim()));
        m.insert(VERIFIED.into(), Value::Bool(true));
        match password {
            Some(p) => m.insert(PASSWORD.into(), Value::from(p)),
            None => m.remove(PASSWORD),
        };
    });
    true
}

/// `drm:saved-email`: the account signed in on this computer, `""` if none.
pub async fn saved_email(store: &Store) -> String {
    if store
        .get(VERIFIED)
        .as_ref()
        .is_some_and(crate::prefs::js_truthy)
        && let Some(Value::String(email)) = store.get(EMAIL)
    {
        return email;
    }
    String::new()
}

/// Removes the saved details (and schedules the store write).
pub async fn forget_credentials(store: &Store) {
    store.update(|m| {
        for k in [EMAIL, PASSWORD, VERIFIED] {
            m.remove(k);
        }
    });
}

// ── Sign-in on launch ────────────────────────────────────────────────────────

/// How an automatic sign-in ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoAuthOutcome {
    /// The engine did not answer. Details kept.
    NoEngine,
    /// The engine has (or is loading) a session; nothing sent.
    HasSession,
    /// Apple turned the details down; they were removed so a changed password is tried once,
    /// not on every launch (repeated bad attempts can lock the account).
    Rejected,
    /// The failure is about this installation (missing libraries, no DRM layer); details kept.
    Kept,
    SignedIn,
    /// Apple wants a verification code: the details were removed, the sign-in sheet takes over.
    NeedsCode,
    /// No answer within the time limit. Details kept.
    Timeout,
}

/// Wall clock and sleep, so tests can run the poll loop on fake time.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn sleep(&self, d: Duration) -> impl Future<Output = ()> + Send;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        time::OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000) as i64
    }
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// Poll interval and overall limit of [`auto_authenticate`].
#[derive(Debug, Clone, Copy)]
pub struct AutoAuthTiming {
    pub poll: Duration,
    pub timeout: Duration,
}

impl Default for AutoAuthTiming {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(2),
            timeout: Duration::from_secs(8 * 60),
        }
    }
}

/// A sign-in or saved details the outcome depends on.
pub struct Credentials<'a> {
    pub email: &'a str,
    pub password: &'a str,
}

fn timestamp_ms(snap: &Value) -> Option<i64> {
    let s = snap.get("timestamp")?.as_str()?;
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    Some(t.unix_timestamp_nanos().div_euclid(1_000_000) as i64)
}

/// Signs in with saved details when the engine has no session. `forget` removes the saved
/// details; it runs when Apple rejects them or asks for a code.
pub async fn auto_authenticate<F: Future<Output = ()> + Send>(
    engine: &impl EngineApi,
    creds: Credentials<'_>,
    forget: impl FnOnce() -> F,
    clock: &impl Clock,
    timing: AutoAuthTiming,
) -> AutoAuthOutcome {
    let Some(EngineResponse {
        status: 200,
        json: Some(status),
    }) = engine.request(Method::GET, STATUS_PATH, None).await
    else {
        return AutoAuthOutcome::NoEngine;
    };
    let state = |v: &Value, k: &str| {
        v.get("state")
            .and_then(|s| s.get(k))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    // A session still loading must not be replaced by a fresh sign-in, which would ask Apple for
    // another code.
    if state(&status, "fairplay").as_deref() == Some("ready")
        || state(&status, "session").as_deref() == Some("valid")
    {
        return AutoAuthOutcome::HasSession;
    }
    info!("auto-authenticating with saved DRM credentials");
    let sent_at = clock.now_ms();
    let body = json!({ "email": creds.email, "password": creds.password });
    let Some(res) = engine.request(Method::POST, AUTH_PATH, Some(body)).await else {
        return AutoAuthOutcome::NoEngine;
    };
    match res.status {
        200 | 202 => {}
        400 => {
            forget().await;
            return AutoAuthOutcome::Rejected;
        }
        // 503: no DRM layer in this build; nothing to retry with.
        _ => return AutoAuthOutcome::Kept,
    }
    let deadline = sent_at + timing.timeout.as_millis() as i64;
    while clock.now_ms() < deadline {
        if let Some(snap) = engine
            .request(Method::GET, STATUS_PATH, None)
            .await
            .and_then(|r| r.json)
        {
            // Ignore what the engine reported before it took this attempt.
            let stale = timestamp_ms(&snap).is_some_and(|at| at < sent_at - 1000);
            if !stale {
                let auth = state(&snap, "authentication");
                if auth.as_deref() == Some("logged_in")
                    || state(&snap, "fairplay").as_deref() == Some("ready")
                {
                    return AutoAuthOutcome::SignedIn;
                }
                match auth.as_deref() {
                    Some("challenging") => {
                        forget().await;
                        return AutoAuthOutcome::NeedsCode;
                    }
                    Some("failed") => {
                        let message = snap
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if message.contains(SETUP_PREFIX) {
                            return AutoAuthOutcome::Kept;
                        }
                        forget().await;
                        return AutoAuthOutcome::Rejected;
                    }
                    _ => {}
                }
            }
        }
        clock.sleep(timing.poll).await;
    }
    AutoAuthOutcome::Timeout
}

/// What the shell does once the page is ready: drops details that never completed a sign-in,
/// and with a saved password, gives the engine 3.5 s to report its DRM state and then signs in.
/// `None` when there was nothing to do.
pub async fn sign_in_on_launch(
    store: &Store,
    engine: &impl EngineApi,
    clock: &impl Clock,
) -> Option<AutoAuthOutcome> {
    let email = store.get(EMAIL);
    let password = store.get(PASSWORD);
    let truthy = |v: &Option<Value>| v.as_ref().is_some_and(crate::prefs::js_truthy);
    if !truthy(&email) && !truthy(&password) {
        return None;
    }
    // Details are kept only from a sign-in Apple accepted (save_credentials marks them). Anything
    // else is an abandoned attempt that would raise Apple's code prompt unasked on every launch.
    if !truthy(&store.get(VERIFIED)) || !truthy(&email) {
        forget_credentials(store).await;
        info!("removed saved sign-in details that never completed a sign-in");
        return None;
    }
    // Only the email was kept: it pre-fills the form, there is nothing to sign in with.
    let (Some(Value::String(email)), Some(Value::String(password))) = (email, password) else {
        return None;
    };
    if password.is_empty() {
        return None;
    }
    clock.sleep(Duration::from_millis(3500)).await;
    let outcome = auto_authenticate(
        engine,
        Credentials {
            email: &email,
            password: &password,
        },
        || forget_credentials(store),
        clock,
        AutoAuthTiming::default(),
    )
    .await;
    match outcome {
        AutoAuthOutcome::Rejected => warn!("saved DRM credentials rejected; cleared"),
        AutoAuthOutcome::NoEngine | AutoAuthOutcome::HasSession => {}
        other => info!("DRM auto sign-in: {other:?}"),
    }
    Some(outcome)
}

// ── Sign-out ─────────────────────────────────────────────────────────────────

/// The two sign-out scopes the engine knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backstop {
    Account,
    Device,
}

impl Backstop {
    /// `account` or `device`; anything else is `None`.
    pub fn parse(v: &Value) -> Option<Self> {
        match v.as_str()? {
            "account" => Some(Self::Account),
            "device" => Some(Self::Device),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Device => "device",
        }
    }
}

/// The result of [`write_backstop`]. The rename is the commit point: before it nothing is
/// visible; after it the engine will see the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackstopWrite {
    pub committed: bool,
    /// The directory entry was fsynced, so the request survives a crash.
    pub durable: bool,
    pub error: Option<String>,
}

fn random_hex() -> io::Result<String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    Ok(nonce.iter().map(|b| format!("{b:02x}")).collect())
}

/// Durably writes a sign-out request into the session directory `dir`.
pub fn write_backstop(dir: &Path, scope: Backstop) -> BackstopWrite {
    let commit = || -> io::Result<()> {
        let name = format!("logout-pending.backstop-{}", random_hex()?);
        let (target, tmp) = (dir.join(&name), dir.join(format!("{name}.tmp")));
        crate::platform::create_private_dir(dir)?;
        let written = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut f = options.open(&tmp)?;
            f.write_all(scope.as_str().as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &target)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    };
    if let Err(e) = commit() {
        return BackstopWrite {
            committed: false,
            durable: false,
            error: Some(e.to_string()),
        };
    }
    // Windows cannot open a directory to fsync it: there the entry is not guaranteed durable
    // (Rustet has no DRM sign-in on Windows).
    if cfg!(windows) {
        return BackstopWrite {
            committed: true,
            durable: false,
            error: None,
        };
    }
    match fs::File::open(dir).and_then(|d| d.sync_all()) {
        Ok(()) => BackstopWrite {
            committed: true,
            durable: true,
            error: None,
        },
        Err(e) => BackstopWrite {
            committed: true,
            durable: false,
            error: Some(e.to_string()),
        },
    }
}

/// `drm:clear-credentials` arguments: `{ reload?: bool, backstop?: 'account' | 'device' }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearOptions {
    /// A sign-out (the default); `false` is an abandoned sign-in.
    pub reload: bool,
    /// Passed only when the engine's sign-out route failed.
    pub backstop: Option<Backstop>,
}

impl ClearOptions {
    pub fn parse(opts: &Value) -> Self {
        let backstop = opts.get("backstop").filter(|v| !v.is_null());
        let parsed = backstop.and_then(Backstop::parse);
        if let (Some(raw), None) = (backstop, parsed) {
            warn!("ignoring invalid sign-out scope: {raw}");
        }
        Self {
            reload: opts.get("reload") != Some(&Value::Bool(false)),
            backstop: parsed,
        }
    }
}

/// What `drm:clear-credentials` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClearResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<&'static str>,
}

/// The fixed order of a sign-out's steps:
/// 1. a backstop scope (the engine route failed) is recorded first; if it could not be
///    committed, stop: no credentials removed, no reload (`marker`).
/// 2. the saved credentials are removed; failure stops before the reload (`credentials`).
/// 3. the engine is reloaded (sign-out only): Apple's libraries keep the signed-out account in
///    memory otherwise, and a recorded backstop is completed on start.
///
/// A backstop committed but not crash-durable still proceeds, with a `durability` warning.
pub async fn clear_credentials_flow(
    opts: ClearOptions,
    record: impl FnOnce(Backstop) -> BackstopWrite,
    delete_credentials: impl Future<Output = bool>,
    reload_engine: impl FnOnce(),
) -> ClearResult {
    let mut warning = None;
    if let Some(scope) = opts.backstop.filter(|_| opts.reload) {
        let r = record(scope);
        if !r.committed {
            return ClearResult {
                ok: false,
                error: Some("marker"),
                warning: None,
            };
        }
        if !r.durable {
            warning = Some("durability");
        }
    }
    if !delete_credentials.await {
        return ClearResult {
            ok: false,
            error: Some("credentials"),
            warning: None,
        };
    }
    if opts.reload {
        reload_engine();
    }
    ClearResult {
        ok: true,
        error: None,
        warning,
    }
}

/// `drm:clear-credentials` with the real pieces: the backstop goes to `session_dir`, the store
/// is written at once.
pub async fn clear_credentials(
    store: &Store,
    session_dir: &Path,
    opts: &Value,
    reload_engine: impl FnOnce(),
) -> ClearResult {
    let res = clear_credentials_flow(
        ClearOptions::parse(opts),
        |scope| {
            let r = write_backstop(session_dir, scope);
            if !r.committed || !r.durable {
                warn!("sign-out backstop: {r:?}");
            }
            r
        },
        async {
            forget_credentials(store).await;
            store.flush_now().await
        },
        reload_engine,
    )
    .await;
    if !res.ok {
        warn!("sign-out did not complete: {:?}", res.error);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use crate::store::StoreCipher;

    /// A fake engine on a fake clock: `statuses` answer GET status in turn, the last repeats.
    /// `None` is an unreachable engine.
    struct FakeEngine {
        statuses: Vec<Option<Value>>,
        post: u16,
        i: AtomicUsize,
        clock: AtomicI64,
        calls: Mutex<Vec<String>>,
        stale_second: bool,
    }

    impl FakeEngine {
        fn new(statuses: Vec<Option<Value>>) -> Self {
            Self {
                statuses,
                post: 202,
                i: AtomicUsize::new(0),
                clock: AtomicI64::new(1_000_000),
                calls: Mutex::default(),
                stale_second: false,
            }
        }
        fn iso(ms: i64) -> String {
            let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
                .unwrap();
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        }
    }

    impl EngineApi for FakeEngine {
        async fn request(
            &self,
            method: Method,
            path: &str,
            body: Option<Value>,
        ) -> Option<EngineResponse> {
            self.calls.lock().unwrap().push(format!("{method} {path}"));
            if method == Method::POST {
                assert_eq!(body, Some(json!({ "email": "a@b.c", "password": "pw" })));
                return Some(EngineResponse {
                    status: self.post,
                    json: Some(json!({})),
                });
            }
            let n = self.i.fetch_add(1, Ordering::SeqCst);
            let mut s = self.statuses[n.min(self.statuses.len() - 1)].clone()?;
            let now = self.clock.load(Ordering::SeqCst);
            let ts = if self.stale_second && n == 1 {
                now - 60_000
            } else {
                now + 500
            };
            s["timestamp"] = Value::from(Self::iso(ts));
            Some(EngineResponse {
                status: 200,
                json: Some(s),
            })
        }
    }

    impl Clock for FakeEngine {
        fn now_ms(&self) -> i64 {
            self.clock.load(Ordering::SeqCst)
        }
        async fn sleep(&self, d: Duration) {
            self.clock.fetch_add(d.as_millis() as i64, Ordering::SeqCst);
        }
    }

    async fn run(e: &FakeEngine) -> (AutoAuthOutcome, usize) {
        let forgotten = AtomicUsize::new(0);
        let timing = AutoAuthTiming {
            poll: Duration::from_secs(2),
            timeout: Duration::from_secs(60),
        };
        let outcome = auto_authenticate(
            e,
            Credentials {
                email: "a@b.c",
                password: "pw",
            },
            || async {
                forgotten.fetch_add(1, Ordering::SeqCst);
            },
            e,
            timing,
        )
        .await;
        (outcome, forgotten.load(Ordering::SeqCst))
    }

    fn st(v: Value) -> Option<Value> {
        Some(v)
    }

    #[tokio::test]
    async fn engine_unreachable_sends_nothing() {
        let e = FakeEngine::new(vec![None]);
        assert_eq!(run(&e).await, (AutoAuthOutcome::NoEngine, 0));
        assert_eq!(*e.calls.lock().unwrap(), ["GET /api/v1/drm/status"]);
    }

    #[tokio::test]
    async fn an_existing_or_loading_session_is_never_replaced() {
        for state in [
            json!({ "fairplay": "ready" }),
            json!({ "session": "valid" }),
        ] {
            let e = FakeEngine::new(vec![st(json!({ "state": state }))]);
            assert_eq!(run(&e).await.0, AutoAuthOutcome::HasSession);
            assert!(
                !e.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| c.starts_with("POST"))
            );
        }
    }

    #[tokio::test]
    async fn outcomes() {
        let s = |auth: &str| st(json!({ "state": { "authentication": auth } }));
        let e = FakeEngine::new(vec![
            st(json!({ "state": {} })),
            s("logging_in"),
            s("logged_in"),
        ]);
        assert_eq!(run(&e).await, (AutoAuthOutcome::SignedIn, 0));
        let e = FakeEngine::new(vec![
            st(json!({ "state": {} })),
            st(
                json!({ "state": { "authentication": "failed" }, "message": "Your Apple Account or password was incorrect." }),
            ),
        ]);
        assert_eq!(run(&e).await, (AutoAuthOutcome::Rejected, 1));
        let e = FakeEngine::new(vec![
            st(json!({ "state": {} })),
            st(
                json!({ "state": { "authentication": "failed" }, "message": "FairPlay setup: the FairPlay libraries are not installed" }),
            ),
        ]);
        assert_eq!(run(&e).await, (AutoAuthOutcome::Kept, 0));
        let e = FakeEngine::new(vec![st(json!({ "state": {} })), s("challenging")]);
        assert_eq!(run(&e).await, (AutoAuthOutcome::NeedsCode, 1));
        let e = FakeEngine::new(vec![st(json!({ "state": {} })), s("logging_in")]);
        assert_eq!(run(&e).await, (AutoAuthOutcome::Timeout, 0));
    }

    #[tokio::test]
    async fn a_failure_reported_before_this_attempt_is_ignored() {
        let failed = st(json!({ "state": { "authentication": "failed" }, "message": "old" }));
        let mut e = FakeEngine::new(vec![
            st(json!({ "state": {} })),
            failed,
            st(json!({ "state": { "authentication": "logged_in" } })),
        ]);
        e.stale_second = true;
        assert_eq!(run(&e).await, (AutoAuthOutcome::SignedIn, 0));
    }

    #[tokio::test]
    async fn a_rejected_body_clears_and_an_engine_without_drm_keeps() {
        let mut e = FakeEngine::new(vec![st(json!({ "state": {} }))]);
        e.post = 400;
        assert_eq!(run(&e).await, (AutoAuthOutcome::Rejected, 1));
        e.post = 503;
        assert_eq!(run(&e).await, (AutoAuthOutcome::Kept, 0));
    }

    async fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path(), StoreCipher::Plain).await;
        (d, s)
    }

    #[tokio::test]
    async fn saved_details() {
        let (_d, s) = store().await;
        assert!(
            save_credentials(
                &s,
                &json!({ "email": " a@b.c ", "password": "pw", "remember": true })
            )
            .await
        );
        assert_eq!(saved_email(&s).await, "a@b.c");
        assert_eq!(s.get(PASSWORD), Some(json!("pw")));
        assert!(save_credentials(&s, &json!({ "email": "a@b.c", "password": "pw" })).await);
        assert_eq!(
            s.get(PASSWORD),
            None,
            "the password only when asked to remember it"
        );
        assert!(!save_credentials(&s, &json!({ "email": "x".repeat(257) })).await);
        assert!(!save_credentials(&s, &json!({ "email": 5 })).await);
        forget_credentials(&s).await;
        assert_eq!(saved_email(&s).await, "");
    }

    #[tokio::test]
    async fn unverified_details_are_dropped_on_launch() {
        let (_d, s) = store().await;
        s.update(|m| {
            m.insert(EMAIL.into(), json!("a@b.c"));
            m.insert(PASSWORD.into(), json!("pw"));
        });
        let e = FakeEngine::new(vec![None]);
        assert_eq!(sign_in_on_launch(&s, &e, &e).await, None);
        assert_eq!(s.get(EMAIL), None);
        assert!(e.calls.lock().unwrap().is_empty());
        // Email only: nothing to sign in with.
        save_credentials(&s, &json!({ "email": "a@b.c" })).await;
        assert_eq!(sign_in_on_launch(&s, &e, &e).await, None);
        assert_eq!(saved_email(&s).await, "a@b.c");
        save_credentials(
            &s,
            &json!({ "email": "a@b.c", "password": "pw", "remember": true }),
        )
        .await;
        assert_eq!(
            sign_in_on_launch(&s, &e, &e).await,
            Some(AutoAuthOutcome::NoEngine)
        );
    }

    fn backstops(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("logout-pending.backstop-"))
            .collect()
    }

    #[test]
    fn backstop_scopes() {
        assert_eq!(Backstop::parse(&json!("account")), Some(Backstop::Account));
        assert_eq!(Backstop::parse(&json!("device")), Some(Backstop::Device));
        for v in [
            Value::Null,
            json!(""),
            json!("1"),
            json!("Device"),
            json!({}),
            json!(7),
        ] {
            assert_eq!(Backstop::parse(&v), None);
        }
    }

    #[test]
    fn backstops_are_uniquely_named_durable_and_never_share_a_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("files");
        assert_eq!(
            write_backstop(&dir, Backstop::Device),
            BackstopWrite {
                committed: true,
                durable: true,
                error: None
            }
        );
        std::thread::scope(|s| {
            s.spawn(|| write_backstop(&dir, Backstop::Account));
            s.spawn(|| write_backstop(&dir, Backstop::Account));
        });
        let names = backstops(&dir);
        assert_eq!(names.len(), 3);
        for n in &names {
            let nonce = n.strip_prefix("logout-pending.backstop-").unwrap();
            assert!(
                nonce.len() == 32
                    && nonce
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "{n}"
            );
        }
        let mut contents: Vec<_> = names
            .iter()
            .map(|n| fs::read_to_string(dir.join(n)).unwrap())
            .collect();
        contents.sort();
        assert_eq!(contents, ["account", "account", "device"]);
    }

    #[test]
    fn a_failed_backstop_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let not_dir = d.path().join("file");
        fs::write(&not_dir, "").unwrap();
        let r = write_backstop(&not_dir, Backstop::Account);
        assert!(!r.committed && r.error.is_some());
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    async fn flow(
        opts: Value,
        committed: bool,
        durable: bool,
        delete_ok: bool,
    ) -> (ClearResult, Vec<String>) {
        let calls = Mutex::new(Vec::new());
        let res = clear_credentials_flow(
            ClearOptions::parse(&opts),
            |s| {
                calls.lock().unwrap().push(format!("record:{}", s.as_str()));
                BackstopWrite {
                    committed,
                    durable,
                    error: None,
                }
            },
            async {
                calls.lock().unwrap().push("delete".into());
                delete_ok
            },
            || calls.lock().unwrap().push("reload".into()),
        )
        .await;
        (res, calls.into_inner().unwrap())
    }

    #[tokio::test]
    async fn sign_out_order() {
        let ok = ClearResult {
            ok: true,
            error: None,
            warning: None,
        };
        assert_eq!(
            flow(json!({}), true, true, true).await,
            (ok.clone(), vec!["delete".into(), "reload".into()])
        );
        assert_eq!(
            flow(json!({ "backstop": "device" }), true, true, true).await,
            (
                ok.clone(),
                vec!["record:device".into(), "delete".into(), "reload".into()]
            )
        );
        let (r, calls) = flow(json!({ "backstop": "account" }), false, false, true).await;
        assert_eq!(
            serde_json::to_value(r).unwrap(),
            json!({ "ok": false, "error": "marker" })
        );
        assert_eq!(calls, ["record:account"]);
        let (r, calls) = flow(json!({ "backstop": "account" }), true, false, true).await;
        assert_eq!(
            serde_json::to_value(r).unwrap(),
            json!({ "ok": true, "warning": "durability" })
        );
        assert_eq!(calls, ["record:account", "delete", "reload"]);
        let (r, calls) = flow(json!({}), true, true, false).await;
        assert_eq!(
            serde_json::to_value(r).unwrap(),
            json!({ "ok": false, "error": "credentials" })
        );
        assert_eq!(calls, ["delete"]);
        assert_eq!(
            flow(
                json!({ "reload": false, "backstop": "device" }),
                true,
                true,
                true
            )
            .await,
            (ok.clone(), vec!["delete".into()])
        );
        assert_eq!(
            flow(json!({ "backstop": "1" }), true, true, true).await,
            (ok, vec!["delete".into(), "reload".into()])
        );
    }

    #[tokio::test]
    async fn clear_credentials_writes_the_store_at_once() {
        let (d, s) = store().await;
        save_credentials(
            &s,
            &json!({ "email": "a@b.c", "password": "pw", "remember": true }),
        )
        .await;
        let reloaded = AtomicUsize::new(0);
        let res = clear_credentials(
            &s,
            &d.path().join("drm/files"),
            &json!({ "backstop": "account" }),
            || {
                reloaded.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert!(res.ok);
        assert_eq!(reloaded.load(Ordering::SeqCst), 1);
        assert_eq!(backstops(&d.path().join("drm/files")).len(), 1);
        assert_eq!(
            fs::read(d.path().join(crate::store::STORE_FILE_NAME)).unwrap(),
            b"{}"
        );
    }
}
