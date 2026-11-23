//! The FairPlay session: the wrapper's start-up sequence, sign-in and its dialog/credential
//! handlers, account tokens, key contexts and sample decryption (`native/main.c`,
//! `native/drm_lib.c`, the FairPlay half of `drm_hybris.c`).
//!
//! Apple's libraries hold process-wide state (one request context, one FootHill session, one
//! lease manager), so the session is a process-wide singleton too. Locks:
//! - `INIT` serialises starts and sign-ins (`g_init_mutex`).
//! - `Wrapper::fp_call` serialises every key open, sample decrypt and lease refresh
//!   (`g_fp_call_mutex`): no decrypt can run on a context a refresh is freeing.
//! - `Wrapper::preshare` guards the cached prefetch context (`g_ctx_mutex`).
//! - `SIGN_IN` holds the sign-in state the handlers read.
//!
//! No lock is held while Apple's code runs on a guarded thread, except by the thread waiting
//! for it (see `guard`). The C never destroyed the objects it built for Apple's calls (control
//! blocks, requests, returned strings); neither does this port.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use super::api::{Api, FootHillConfig, MakeShared, Variants, Vtables};
use super::cxx::{
    SharedPtr, StdString, StdVector, emplace_block, guest_alloc, make_shared_manual, string_lossy,
};
use super::loader::{Hybris, Resolver};
use super::wire::{music_token_request, subscription_offline_available};
use super::{
    Credentials, DecryptError, Error, FairPlayConfig, KeyContextError, SignInPrompt,
    VerificationReply, callbacks, guard, recovery,
};

/// Stack for threads that run Apple's code: glibc's default thread stack, which the C's
/// threads (and the engine's cgo calls) had.
pub(crate) const GUEST_STACK_SIZE: usize = 8 << 20;

/// The device string when the caller gives none (cmdline.c's default).
const DEFAULT_DEVICE_INFO: &str =
    "Music/4.9/Android/10/Samsung S9/7663313/en-US/en-US/dc28071e981c439e";

/// The key every ALAC init segment names; its context, opened under adam ID "0", is shared by
/// all tracks and cached.
const PREFETCH_KEY_URI: &CStr = c"skd://itunes.apple.com/P000000000/s1/e1";

const KEY_SERVER: &CStr = c"https://play.itunes.apple.com/WebObjects/MZPlay.woa/music/fps";

/// Apple's FPS certificate chain (base64), from `native/import.h`.
const FPS_CERT_NUL: &str = concat!(include_str!("fps_cert.b64"), "\0");
const FPS_CERT: &CStr = match CStr::from_bytes_with_nul(FPS_CERT_NUL.as_bytes()) {
    Ok(c) => c,
    Err(_) => panic!("fps_cert.b64 must not contain NUL"),
};

/// `AuthenticateResponse::responseType()` of a successful sign-in.
const AUTH_SUCCESS: c_int = 6;

/// `NfcRKVnxuKZy04KWbdFu71Ou`'s selector for CBCS sample decryption.
const NFC_SELECTOR_SAMPLE: u32 = 5;

/// `CredentialsResponse::ResponseType` (AuthenticateFlow::_buttonActionForResponseType).
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum CredentialsAnswer {
    /// The first button: ends the flow as cancelled.
    Cancel = 0,
    /// Resend the notification to the trusted devices.
    Resend = 1,
    /// OK: submit the password (with the code appended).
    Submit = 2,
    /// "Can't get to your devices": Apple raises the phone-OTP sub-menu as a dialog.
    CantReachDevices = 3,
}

/// The wrapper's lifecycle states that reach the log and the `drm-state` file.
#[derive(Clone, Copy, Debug)]
enum WrapperState {
    WaitingDialog,
    Waiting2fa,
    Running,
    Failed,
}

impl WrapperState {
    fn as_str(self) -> &'static str {
        match self {
            Self::WaitingDialog => "WAITING_DIALOG",
            Self::Waiting2fa => "WAITING_2FA",
            Self::Running => "RUNNING",
            Self::Failed => "FAILED",
        }
    }
}

/// The address of the slot holding a key's live decrypt context (what `getKdContext`
/// returned). Owned by `libandroidappmusic.so`; never freed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KdSlot(*mut *mut c_void);

// SAFETY: the slot is owned by Apple's library and only dereferenced under `fp_call`.
unsafe impl Send for KdSlot {}
// SAFETY: as above.
unsafe impl Sync for KdSlot {}

/// An open key context for one (adam ID, key URI).
#[derive(Clone, Copy, Debug)]
pub struct KeyContext {
    slot: KdSlot,
    /// The lease generation it belongs to; a refresh frees older contexts.
    epoch: u32,
}

/// State the start sequence leaves for later sign-ins.
struct Session {
    base_dir: PathBuf,
    req_ctx: SharedPtr,
    guid: SharedPtr,
    /// Strings handed to Apple's code during the start, kept alive until the next start (the C
    /// kept them in globals).
    _strings: Vec<CString>,
}

/// The wrapper's globals (`main.c`), created on the first start.
pub(crate) struct Wrapper {
    api: Api,
    vtables: Vtables,
    variants: Variants,
    resolver: Resolver,
    /// `apInf.obj`: the presentation interface the handlers answer through.
    presentation: AtomicPtr<c_void>,
    /// `FHinstance`: the SVFootHillSessionCtrl singleton.
    foothill: AtomicPtr<c_void>,
    /// `leaseMgr`: the SVPlaybackLeaseManager.
    lease_manager: AtomicPtr<c_void>,
    preshare: Mutex<Option<KdSlot>>,
    fp_call: Mutex<()>,
    /// Bumped on every refresh, so callers can drop cached key contexts (`g_decrypt_epoch`).
    epoch: AtomicU32,
    /// A start succeeded (`fairplay_inited`).
    ready: AtomicBool,
    session: Mutex<Option<Session>>,
}

/// The sign-in state the handlers read (`amUsername`, `amPassword`, `g_last_auth_error`, ...).
struct SignIn {
    username: Option<String>,
    /// The password while a sign-in runs; the verification code is appended to it.
    password: Option<String>,
    /// Length of the bare password: Apple asks for the code once per attempt and the code is
    /// appended, so the password is cut back to this first; otherwise a second prompt would
    /// send password+code1+code2.
    password_len: usize,
    prompt: Option<Arc<dyn SignInPrompt>>,
    base_dir: Option<PathBuf>,
    /// Why the last sign-in failed (Apple's own wording when it gave one), for the host.
    last_error: String,
    /// The host gave no verification code (the user cancelled or the wait ran out).
    cancelled: bool,
    /// Title and text of the last dialog Apple raised during the sign-in.
    last_dialog: String,
}

static WRAPPER: OnceLock<Wrapper> = OnceLock::new();
static INIT: Mutex<()> = Mutex::new(());
static SIGN_IN: Mutex<SignIn> = Mutex::new(SignIn {
    username: None,
    password: None,
    password_len: 0,
    prompt: None,
    base_dir: None,
    last_error: String::new(),
    cancelled: false,
    last_dialog: String::new(),
});

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_last_error(msg: &str) {
    lock(&SIGN_IN).last_error = msg.to_owned();
}

/// The error for a failed start or sign-in, from the last sign-in error (`hybris_last_error`).
fn last_error() -> Error {
    match lock(&SIGN_IN).last_error.as_str() {
        "" => Error::Failed(None),
        "Sign-in cancelled" => Error::SignInCancelled,
        why => Error::Failed(Some(why.to_owned())),
    }
}

/// A sign-in is over: forget the account so a later request for credentials cannot resubmit
/// it.
fn forget_credentials() {
    let mut s = lock(&SIGN_IN);
    s.username = None;
    s.password = None;
    s.password_len = 0;
}

/// The wrapper state callback of the hybris path: the log only.
fn report_state(state: WrapperState) {
    info!("[hybris] wrapper state: {}", state.as_str());
}

/// `write_drm_state`: the state callback, then `<base_dir>/drm-state`.
fn write_drm_state(state: WrapperState) {
    report_state(state);
    let base = lock(&SIGN_IN).base_dir.clone();
    if let Some(base) = base {
        let _ = std::fs::write(base.join("drm-state"), format!("{}\n", state.as_str()));
    }
}

/// `drm_posix_path`: the statically linked SQLite in libmediaplatform treats a path not
/// starting with '/' as relative (it mangles Windows paths), so the path gets a leading '/'
/// and forward slashes. A POSIX-absolute path is unchanged.
fn posix_path(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() + 1);
    if path.first() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend(path.iter().map(|&b| if b == b'\\' { b'/' } else { b }));
    out
}

fn invalid(what: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        what.to_owned(),
    ))
}

/// Splits the device string as `strtok(.., "/")` did (empty fields are skipped); it must give
/// 9 fields: client id, version, platform, product version, model, build, locale, language,
/// Android id.
fn device_fields(device_info: &str) -> Result<Vec<CString>, Error> {
    let info = if device_info.is_empty() {
        DEFAULT_DEVICE_INFO
    } else {
        device_info
    };
    let fields = info
        .split('/')
        .filter(|f| !f.is_empty())
        .take(9)
        .map(|f| CString::new(f).map_err(|_| Error::DeviceInfo))
        .collect::<Result<Vec<_>, _>>()?;
    if fields.len() != 9 {
        return Err(Error::DeviceInfo);
    }
    Ok(fields)
}

/// Starts the session or, once it runs, signs in with new credentials (`hybris_fairplay_init`).
pub(crate) fn start(hybris: &'static Hybris, cfg: FairPlayConfig) -> Result<FairPlay, Error> {
    let _init = lock(&INIT);
    if let Some(w) = WRAPPER.get().filter(|w| w.ready.load(Ordering::SeqCst)) {
        let Some(credentials) = cfg.credentials else {
            return Ok(FairPlay { w });
        };
        lock(&SIGN_IN).prompt = Some(cfg.prompt);
        let out = guard::run("login", move || w.login_after_start(credentials));
        forget_credentials();
        return match out? {
            Ok(true) => Ok(FairPlay { w }),
            Ok(false) => Err(last_error()),
            Err(guard::Aborted) => {
                warn!("[guard] drm_lib_login: exception escaped");
                Err(last_error())
            }
        };
    }

    let out = guard::run("init", move || init(hybris, cfg));
    forget_credentials();
    match out? {
        Ok(result) => result.map(|w| FairPlay { w }),
        Err(guard::Aborted) => {
            warn!("[guard] drm_lib_init: SVError escaped");
            report_state(WrapperState::Failed); // drm_lib_abort_init
            Err(last_error())
        }
    }
}

/// The start sequence (`drm_lib_init_impl`).
fn init(hybris: &'static Hybris, cfg: FairPlayConfig) -> Result<&'static Wrapper, Error> {
    {
        let mut s = lock(&SIGN_IN);
        s.prompt = Some(Arc::clone(&cfg.prompt));
        s.base_dir = Some(cfg.base_dir.clone());
    }

    // The wrapper keeps its own handles for symbol lookup; opening the libraries again returns
    // the ones Hybris::load loaded. (The C also set HYBRIS_ANDROID_LIB64 here, which nothing
    // reads.)
    let resolver = hybris.open_for_lookup(&cfg.lib64_dir)?;
    let w = match WRAPPER.get() {
        Some(w) => w,
        None => {
            let wrapper = Wrapper {
                api: Api::resolve(&resolver)?,
                vtables: Vtables::resolve(&resolver)?,
                variants: Variants::resolve(&resolver)?,
                resolver,
                presentation: AtomicPtr::new(ptr::null_mut()),
                foothill: AtomicPtr::new(ptr::null_mut()),
                lease_manager: AtomicPtr::new(ptr::null_mut()),
                preshare: Mutex::new(None),
                fp_call: Mutex::new(()),
                epoch: AtomicU32::new(0),
                ready: AtomicBool::new(false),
                session: Mutex::new(None),
            };
            WRAPPER.get_or_init(|| wrapper)
        }
    };

    let fields = device_fields(&cfg.device_info)?;
    let signing_in = cfg.credentials.is_some();
    if let Some(c) = cfg.credentials {
        let mut s = lock(&SIGN_IN);
        s.password_len = c.password.len();
        s.username = Some(c.username);
        s.password = Some(c.password);
    }

    let guid = w.init_internal(&fields[8]);
    let callbacks = callbacks::lease_callbacks();
    let (req_ctx, mut strings) = w.init_ctx(&cfg.base_dir, &fields)?;
    strings.extend(fields);

    if signing_in && !w.login(req_ctx, &cfg.base_dir) {
        warn!("[drm_lib] login failed");
        return Err(last_error());
    }

    let api = &w.api;
    let lease_manager = guest_alloc(16).cast::<c_void>();
    w.lease_manager.store(lease_manager, Ordering::SeqCst);
    let automatically: u8 = 1;
    // SAFETY: `lease_manager` is 16 zeroed bytes for SVPlaybackLeaseManager (the C's global
    // `uint8_t leaseMgr[16]`); the callbacks are std::function objects of the expected layout.
    unsafe {
        debug!("[drm_lib] lease ctor");
        (api.lease_manager_ctor)(
            lease_manager,
            callbacks.end_lease.cast(),
            callbacks.playback_error.cast(),
        );
        debug!("[drm_lib] refreshLeaseAutomatically");
        (api.refresh_lease_automatically)(lease_manager, &automatically);
        debug!("[drm_lib] requestLease");
        (api.request_lease)(lease_manager, &automatically);
        debug!("[drm_lib] requestLease returned");
        debug!("[drm_lib] FHinstance");
        w.foothill
            .store((api.foothill_instance)(), Ordering::SeqCst);
    }

    // After the lease manager and the FootHill session exist, so the worker's refresh is safe.
    debug!("[drm_lib] start_recovery_thread");
    recovery::start_worker()?;

    debug!("[drm_lib] offline_available");
    let offline = w.offline_available(req_ctx);
    debug!(
        "[drm_lib] offline={}, get_storefront_id",
        i32::from(offline)
    );

    let mut tokens = w.fetch_account_tokens(req_ctx, guid, &cfg.base_dir);
    if tokens.is_none() && !signing_in {
        // No password to offer and the stored session gave no tokens: let the library renew
        // the existing account on its own (AuthenticateFlow::runWithTimeout goes straight to
        // _authenticateUsingExistingAccount when the account is known). Anything that would
        // need the user is cancelled by the credentials handler, which finds no credentials.
        info!("[drm_lib] stored session gave no tokens; trying a silent sign-in");
        if w.login(req_ctx, &cfg.base_dir) {
            tokens = w.fetch_account_tokens(req_ctx, guid, &cfg.base_dir);
        } else {
            set_last_error("Your Apple session has expired. Sign in again.");
        }
    }
    let Some(tokens) = tokens else {
        return Err(last_error());
    };
    debug!("[drm_lib] music_token ok");

    tokens.write(&cfg.base_dir);
    report_state(WrapperState::Running);
    *lock(&w.session) = Some(Session {
        base_dir: cfg.base_dir,
        req_ctx,
        guid,
        _strings: strings,
    });
    w.ready.store(true, Ordering::SeqCst);
    Ok(w)
}

/// The account tokens a start or sign-in fetches.
struct Tokens {
    storefront_id: String,
    music_token: String,
}

impl Tokens {
    /// `write_storefront_id` and `write_music_token`.
    fn write(&self, base_dir: &Path) {
        debug!("[+] StoreFront ID: {}", self.storefront_id);
        if let Err(e) = std::fs::write(base_dir.join("STOREFRONT_ID"), &self.storefront_id) {
            warn!("[!] write_storefront_id: {e}");
        }
        if !self.music_token.is_empty() {
            debug!(
                "[+] Music-Token: {}...",
                self.music_token.get(..14).unwrap_or(&self.music_token)
            );
            if let Err(e) = std::fs::write(base_dir.join("MUSIC_TOKEN"), &self.music_token) {
                warn!("[!] write_music_token: {e}");
            }
        }
    }
}

impl Wrapper {
    /// `drm_init_internal`: FootHill configuration and the device GUID. Returns the GUID object.
    fn init_internal(&self, android_id: &CStr) -> SharedPtr {
        let api = &self.api;
        debug!("[+] starting...");
        // The C set ANDROID_DNS_MODE=local here; the guest's getenv hook answers it (`hooks`).
        // The C also called _resolv_set_nameservers_for_net(0, {"1.1.1.1", "8.8.8.8"}, 2, ".");
        // glibc has no such function, so that bound to the C's own no-op stub. Not called.

        let conf1 = StdString::borrow(android_id);
        let conf2 = StdString::borrow(c"");
        static API_LEVEL: u32 = 29;
        static FLAG: u8 = 1;
        debug!("[trace] before FootHillConfig");
        // SAFETY: the strings outlive the calls. A by-value string is the callee's to keep or
        // move from, so it is a well-formed guest string, destroyed afterwards as the caller's
        // copy of a by-value argument is.
        unsafe {
            match self.variants.foothill_config {
                FootHillConfig::ByRef(config) => config(&conf1),
                FootHillConfig::ByValue(config) => {
                    let mut own = StdString::guest_owned(android_id.to_bytes());
                    config(&mut own);
                    own.destroy_guest();
                }
            }
        }
        debug!("[trace] after FootHillConfig");

        debug!("[trace] before DeviceGUID::instance");
        // SAFETY: DeviceGUID::instance() takes no arguments.
        let guid = unsafe { (api.device_guid_instance)() }.ptr;
        debug!("[trace] after DeviceGUID::instance obj={:p}", guid.obj);
        debug!("[trace] before DeviceGUID::configure");
        // SAFETY: `guid.obj` is the DeviceGUID; the arguments are live for the call.
        let _ = unsafe { (api.device_guid_configure)(guid.obj, &conf1, &conf2, &API_LEVEL, &FLAG) };
        debug!("[trace] after DeviceGUID::configure");
        guid
    }

    /// `drm_init_ctx`: the request context, its configuration, and the presentation interface
    /// with our dialog and credential handlers. Returns the request context and the strings
    /// that must outlive it.
    fn init_ctx(
        &self,
        base_dir: &Path,
        fields: &[CString],
    ) -> Result<(SharedPtr, Vec<CString>), Error> {
        let api = &self.api;
        debug!("[+] initializing ctx...");
        let base = base_dir.as_os_str().as_bytes();
        let mpl_db = CString::new(posix_path(&[base, b"/mpl_db"].concat()))
            .map_err(|_| invalid("base dir"))?;
        let fair_play_dir = CString::new(posix_path(base)).map_err(|_| invalid("base dir"))?;
        let mut mpl_db_str = StdString::borrow(&mpl_db);

        debug!("[cp1] make_shared RequestContext");
        // SAFETY: the export / constructor take the path string, which outlives the call.
        let req_ctx = unsafe {
            match self.variants.make_request_context {
                MakeShared::Export(make) => make(&mut mpl_db_str).ptr,
                MakeShared::Manual { vtable, ctor } => {
                    make_shared_manual(vtable, 0x5e0, Some(0), |obj| ctor(obj, &mpl_db_str))
                }
            }
        };
        if req_ctx.obj.is_null() {
            warn!("[drm_lib] drm_init_ctx failed");
            return Err(last_error());
        }

        let config = emplace_block(self.vtables.request_context_config, 480, 32);
        debug!(
            "[cp3] reqCtxCfg ctrl_blk={:p} obj={:p}",
            config.ctrl, config.obj
        );
        let field = |i: usize| StdString::borrow(&fields[i]);
        // SAFETY: `config.obj` is fresh storage for a RequestContextConfig; every string
        // outlives its call (the setters copy them).
        unsafe {
            debug!("[cp4] RequestContextConfigC2");
            (api.config_ctor)(config.obj);
            debug!("[cp5] setBaseDirectoryPath");
            (api.set_base_directory_path)(config.obj, &mpl_db_str);
            debug!("[cp6] setClientIdentifier");
            (api.set_client_identifier)(config.obj, &field(0));
            debug!("[cp7] setVersionIdentifier");
            (api.set_version_identifier)(config.obj, &field(1));
            debug!("[cp8] setPlatformIdentifier");
            (api.set_platform_identifier)(config.obj, &field(2));
            debug!("[cp9] setProductVersion");
            (api.set_product_version)(config.obj, &field(3));
            debug!("[cp10] setDeviceModel");
            (api.set_device_model)(config.obj, &field(4));
            debug!("[cp11] setBuildVersion");
            (api.set_build_version)(config.obj, &field(5));
            debug!("[cp12] setLocaleIdentifier");
            (api.set_locale_identifier)(config.obj, &field(6));
            debug!("[cp13] setLanguageIdentifier");
            (api.set_language_identifier)(config.obj, &field(7));
        }

        // Builds that expect host callbacks (4.9.x/5.x) get them here.
        callbacks::install_request_context_callbacks(&self.resolver, config.obj);

        // SAFETY: `req_ctx` and `config` are the live objects built above.
        unsafe {
            debug!("[cp14] RequestContextManager::configure");
            (api.request_context_manager_configure)(&req_ctx);
            debug!("[cp15] RequestContext::init");
            let _ = (api.request_context_init)(req_ctx.obj, &config);
            debug!("[cp16] setFairPlayDirectoryPath");
            (api.set_fair_play_directory_path)(req_ctx.obj, &StdString::borrow(&fair_play_dir));
        }

        debug!("[cp17] make_shared AndroidPresentationInterface");
        // SAFETY: the export / constructor take no arguments besides the object.
        let presentation = unsafe {
            match self.variants.make_presentation {
                MakeShared::Export(make) => make().ptr,
                MakeShared::Manual { vtable, ctor } => {
                    make_shared_manual(vtable, 0x120, Some(0x18), |obj| ctor(obj))
                }
            }
        };
        self.presentation.store(presentation.obj, Ordering::SeqCst);
        // SAFETY: the handlers have the C signatures the setters expect and live forever.
        unsafe {
            debug!("[cp18] setDialogHandler");
            (api.set_dialog_handler)(presentation.obj, dialog_handler);
            debug!("[cp19] setCredentialsHandler");
            (api.set_credentials_handler)(presentation.obj, credentials_handler);
            debug!("[cp20] setPresentationInterface");
            (api.set_presentation_interface)(req_ctx.obj, &presentation);
        }
        debug!("[cp21] init_ctx done");
        Ok((req_ctx, vec![mpl_db, fair_play_dir]))
    }

    /// Runs Apple's AuthenticateFlow (`login`). Records why it failed for [`last_error`].
    /// Only a successful sign-in replaces the old tokens: a wrong password must not sign the
    /// existing session out.
    fn login(&self, req_ctx: SharedPtr, base_dir: &Path) -> bool {
        let api = &self.api;
        debug!("[+] logging in...");
        {
            let mut s = lock(&SIGN_IN);
            s.last_error.clear();
            s.cancelled = false;
            s.last_dialog.clear();
        }
        // SAFETY: `req_ctx` is the live request context; the flow's results are read through
        // the accessors Apple exports, each checked for null.
        let response_type = unsafe {
            let flow = match self.variants.make_authenticate_flow {
                MakeShared::Export(make) => make(&req_ctx).ptr,
                MakeShared::Manual { vtable, ctor } => {
                    make_shared_manual(vtable, 0x100, Some(8), |obj| ctor(obj, &req_ctx))
                }
            };
            (api.authenticate_flow_run)(flow.obj);
            let resp = (api.authenticate_flow_response)(flow.obj);
            if resp.is_null() || (*resp).obj.is_null() {
                set_last_error("Apple did not answer the sign-in request");
                return false;
            }
            let resp = (*resp).obj;
            let response_type = (api.authenticate_response_type)(resp);
            if response_type != AUTH_SUCCESS {
                let customer_message = string_lossy((api.authenticate_customer_message)(resp));
                let mut s = lock(&SIGN_IN);
                if !customer_message.is_empty() {
                    warn!("[!] server message: {customer_message}");
                    s.last_error = customer_message;
                }
                if !s.last_dialog.is_empty() {
                    s.last_error = s.last_dialog.clone();
                }
                drop(s);
                let err = (api.authenticate_error)(resp);
                if !err.is_null() && !(*err).obj.is_null() {
                    let (code, what) = self.store_error(err);
                    warn!("[!] auth error: code={code}, message={what}");
                } else {
                    warn!("[!] auth failed: response type {response_type}");
                }
                let mut s = lock(&SIGN_IN);
                if s.last_error.is_empty() {
                    s.last_error = "Apple rejected the sign-in".into();
                }
            }
            response_type
        };
        let mut s = lock(&SIGN_IN);
        if s.cancelled && response_type != AUTH_SUCCESS {
            s.last_error = "Sign-in cancelled".into();
        }
        drop(s);
        if response_type == AUTH_SUCCESS {
            let _ = std::fs::remove_file(base_dir.join("STOREFRONT_ID"));
            let _ = std::fs::remove_file(base_dir.join("MUSIC_TOKEN"));
        }
        response_type == AUTH_SUCCESS
    }

    /// A sign-in into a running session, another account or one that expired
    /// (`drm_lib_login_impl`): the cached tokens belong to the old account, so they are
    /// fetched again.
    fn login_after_start(&self, credentials: Credentials) -> bool {
        {
            let mut s = lock(&SIGN_IN);
            s.password_len = credentials.password.len();
            s.username = Some(credentials.username);
            s.password = Some(credentials.password);
        }
        let Some((req_ctx, guid, base_dir)) = lock(&self.session)
            .as_ref()
            .map(|s| (s.req_ctx, s.guid, s.base_dir.clone()))
        else {
            return false;
        };
        if !self.login(req_ctx, &base_dir) {
            warn!("[drm_lib] login failed");
            return false;
        }
        debug!("[drm_lib] login ok");
        let storefront_id = self.storefront_id(req_ctx);
        let dev_token = self.dev_token(req_ctx);
        let Some(music_token) =
            dev_token.and_then(|dev| self.music_user_token(guid, &dev, req_ctx))
        else {
            set_last_error("Signed in, but Apple returned no music token");
            return false;
        };
        Tokens {
            storefront_id,
            music_token,
        }
        .write(&base_dir);
        true
    }

    /// The storefront, developer and music tokens for the signed-in account
    /// (`fetch_account_tokens`). When `createMusicToken` fails (the session may have expired)
    /// the `MUSIC_TOKEN` stored by the web-session path is used.
    fn fetch_account_tokens(
        &self,
        req_ctx: SharedPtr,
        guid: SharedPtr,
        base_dir: &Path,
    ) -> Option<Tokens> {
        let storefront_id = self.storefront_id(req_ctx);
        debug!("[drm_lib] storefront_id={storefront_id}, get_dev_token");
        let Some(dev_token) = self.dev_token(req_ctx) else {
            warn!("[drm_lib] failed to get dev token");
            set_last_error("Signed in, but Apple returned no dev token");
            return None;
        };
        debug!("[drm_lib] dev_token ok, get_music_token");
        let music_token = self
            .music_user_token(guid, &dev_token, req_ctx)
            .or_else(|| {
                let stored = std::fs::read(base_dir.join("MUSIC_TOKEN")).ok()?;
                let mut stored = stored.get(..8191).unwrap_or(&stored).to_vec();
                while stored
                    .last()
                    .is_some_and(|b| matches!(b, b'\n' | b'\r' | b' '))
                {
                    stored.pop();
                }
                (!stored.is_empty()).then(|| {
                    debug!(
                        "[drm_lib] music_token: using stored token from disk ({} bytes)",
                        stored.len()
                    );
                    String::from_utf8_lossy(&stored).into_owned()
                })
            });
        let Some(music_token) = music_token else {
            warn!("[drm_lib] failed to get music token");
            set_last_error("Signed in, but Apple returned no music token");
            return None;
        };
        Some(Tokens {
            storefront_id,
            music_token,
        })
    }

    /// `get_account_storefront_id`.
    fn storefront_id(&self, req_ctx: SharedPtr) -> String {
        let no_bag = SharedPtr::NULL;
        // SAFETY: `req_ctx.obj` is the live request context; a null URL bag means the default.
        let region = unsafe { (self.api.store_front_identifier)(req_ctx.obj, &no_bag) };
        // SAFETY: a string Apple just returned by value.
        String::from_utf8_lossy(unsafe { region.as_bytes() }).into_owned()
    }

    /// The code and text of a `StoreErrorCondition`.
    ///
    /// # Safety
    /// `err` must point at a live shared_ptr to a StoreErrorCondition.
    unsafe fn store_error(&self, err: *const SharedPtr) -> (i32, String) {
        // SAFETY: per the caller; the accessors return a reference and a C string or null.
        unsafe {
            let obj = (*err).obj;
            let code = (self.api.store_error_code)(obj);
            let code = if code.is_null() { -1 } else { (*code).value };
            let what = (self.api.store_error_what)(obj);
            let what = if what.is_null() {
                "none".into()
            } else {
                CStr::from_ptr(what).to_string_lossy().into_owned()
            };
            (code, what)
        }
    }

    /// An HTTP message for Apple's URLRequest (`mediaplatform::HTTPMessage(url, method)`).
    fn http_message(&self, url: &'static CStr, method: &'static CStr) -> SharedPtr {
        let message = emplace_block(self.vtables.http_message, 2048, 32);
        let mut url = StdString::guest_owned(url.to_bytes());
        let mut method = StdString::guest_owned(method.to_bytes());
        // SAFETY: fresh storage for the message. Both strings are by value: well-formed guest
        // strings the constructor may move from, then destroyed as the caller's copies.
        unsafe {
            (self.api.http_message_ctor)(message.obj, &mut url, &mut method);
            url.destroy_guest();
            method.destroy_guest();
        }
        message
    }

    /// Runs `message` through Apple's URLRequest with the request context's session. `None`
    /// after logging a request error; otherwise the response body, empty when there is none.
    fn run_request(
        &self,
        message: SharedPtr,
        req_ctx: SharedPtr,
        params: &[(&CStr, &CStr)],
        what: &str,
    ) -> Option<Vec<u8>> {
        let api = &self.api;
        let request = guest_alloc(2048).cast::<c_void>();
        // SAFETY: fresh storage for a URLRequest, live message and context, parameter strings
        // that outlive their calls; every returned reference is checked for null.
        unsafe {
            (api.url_request_ctor)(request, &message, &req_ctx);
            for (name, value) in params {
                (api.url_request_set_parameter)(
                    request,
                    &StdString::borrow(name),
                    &StdString::borrow(value),
                );
            }
            (api.url_request_run)(request);
            let err = (api.url_request_error)(request);
            if !err.is_null() && !(*err).obj.is_null() {
                let (code, message) = self.store_error(err);
                warn!("[!] {what} error: code={code}, message={message}");
                return None;
            }
            let response = (api.url_request_response)(request);
            if response.is_null() || (*response).obj.is_null() {
                return Some(Vec::new());
            }
            let http = (api.url_response_underlying)((*response).obj);
            if http.is_null() || (*http).obj.is_null() {
                return Some(Vec::new());
            }
            // The HTTPMessage keeps its body Data 48 bytes in.
            let data = *(*http).obj.cast::<u8>().add(48).cast::<*mut c_void>();
            if data.is_null() {
                return Some(Vec::new());
            }
            let bytes = (api.data_bytes)(data);
            let len = (api.data_length)(data);
            if bytes.is_null() {
                return Some(Vec::new());
            }
            Some(std::slice::from_raw_parts(bytes.cast::<u8>(), len).to_vec())
        }
    }

    /// The developer token (`get_dev_token`): `GET apiToken?clientId=musicAndroid&version=1`.
    fn dev_token(&self, req_ctx: SharedPtr) -> Option<String> {
        let message = self.http_message(
            c"https://sf-api-token-service.itunes.apple.com/apiToken",
            c"GET",
        );
        let body = self.run_request(
            message,
            req_ctx,
            &[(c"clientId", c"musicAndroid"), (c"version", c"1")],
            "devToken",
        )?;
        let token = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|json| {
                json.get("token")
                    .and_then(|t| t.as_str())
                    .map(str::to_owned)
            });
        if token.is_none() {
            warn!("[!] devToken error: token field missing in response");
        }
        token
    }

    /// The device GUID as text (`get_guid`; `Data::bytes()` is not NUL-terminated).
    fn guid(&self, guid: SharedPtr) -> Option<String> {
        // SAFETY: `guid.obj` is the DeviceGUID; the returned Data is read within its length.
        unsafe {
            let data = (self.api.device_guid_guid)(guid.obj).ptr.obj;
            if data.is_null() {
                return None;
            }
            let bytes = (self.api.data_bytes)(data);
            let len = (self.api.data_length)(data);
            if bytes.is_null() {
                return None;
            }
            Some(
                String::from_utf8_lossy(std::slice::from_raw_parts(bytes.cast::<u8>(), len))
                    .into_owned(),
            )
        }
    }

    /// The music user token (`get_music_user_token`): `POST createMusicToken`.
    fn music_user_token(
        &self,
        guid: SharedPtr,
        dev_token: &str,
        req_ctx: SharedPtr,
    ) -> Option<String> {
        let Some(guid) = self.guid(guid) else {
            warn!("[!] createMusicToken: no device GUID");
            return None;
        };
        let message = self.http_message(
            c"https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/createMusicToken",
            c"POST",
        );
        let headers: [(&CStr, &CStr); 4] = [
            (c"Content-Type", c"application/json; charset=UTF-8"),
            (c"Expect", c""),
            (c"X-Apple-Requesting-Bundle-Id", c"com.apple.android.music"),
            (
                c"X-Apple-Requesting-Bundle-Version",
                c"Music/4.9 Android/10 model/Samsung S9 build/7663313 (dt:66)",
            ),
        ];
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let body = music_token_request(&guid, dev_token, now_ms);
        // SAFETY: the message is live; the header strings outlive their calls. setBodyData
        // copies the body (it builds `Data(bytes, length, copy = true)`), so `body` only needs
        // to outlive the call.
        unsafe {
            for (name, value) in headers {
                (self.api.http_message_set_header)(
                    message.obj,
                    &StdString::borrow(name),
                    &StdString::borrow(value),
                );
            }
            (self.api.http_message_set_body_data)(
                message.obj,
                body.as_ptr().cast::<c_char>().cast_mut(),
                body.len(),
            );
        }
        let response = self.run_request(message, req_ctx, &[], "createMusicToken")?;
        let json = serde_json::from_slice::<serde_json::Value>(&response).unwrap_or_default();
        let field = |key: &str| json.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        let token = field("music_token");
        if token.is_none() {
            warn!(
                "[!] createMusicToken failed: {} ({})",
                field("error_description")
                    .as_deref()
                    .unwrap_or("unknown error"),
                field("error").as_deref().unwrap_or("?")
            );
        }
        token
    }

    /// Whether the account's FairPlay subscription allows offline content (`offline_available`).
    fn offline_available(&self, req_ctx: SharedPtr) -> bool {
        // SAFETY: `req_ctx.obj` is the live request context; the status vector is only read
        // within its bounds.
        unsafe {
            let fair_play = (self.api.request_context_fair_play)(req_ctx.obj).ptr;
            if fair_play.obj.is_null() {
                // The account may lack a FairPlay subscription object.
                info!("[drm] offline_available: FairPlay obj is null");
                return false;
            }
            let status: StdVector = (self.api.get_subscription_status)(fair_play.obj);
            subscription_offline_available(status.begin.cast(), status.end.cast())
        }
    }

    /// Derives the key context for (rustet, uri) (`getKdContext`): `getPersistentKey()` fetches
    /// the persistent key from Apple, `decryptContext()` turns it into an SVFootHillPContext,
    /// whose `kdContext()` is the slot. rustet "0" is the shared prefetch key and is cached; the
    /// long FairPlay network calls run without the cache lock, so a refresh never blocks on
    /// them.
    fn kd_context(&self, rustet: &CStr, uri: &CStr) -> Option<KdSlot> {
        let preshare = rustet.to_bytes() == b"0";
        if preshare && let Some(cached) = *lock(&self.preshare) {
            return Some(cached);
        }
        debug!(
            "[.] adamId: {}, uri: {}",
            rustet.to_string_lossy(),
            uri.to_string_lossy()
        );

        let default_id = StdString::borrow(rustet);
        let key_uri = StdString::borrow(uri);
        let key_format = StdString::borrow(c"com.apple.streamingkeydelivery");
        let key_format_version = StdString::borrow(c"1");
        let server_uri = StdString::borrow(KEY_SERVER);
        let protocol_type = StdString::borrow(c"simplified");
        let fps_cert = StdString::borrow(FPS_CERT);
        static DECRYPTOR_TYPE: c_int = 0;
        static FLAG: u8 = 0;
        let foothill = self.foothill.load(Ordering::SeqCst);
        // SAFETY: `foothill` is the session singleton; every string outlives the calls; the
        // persistent key's first field is its ckc string, which decryptContext takes.
        let slot = unsafe {
            let key = (self.api.get_persistent_key)(
                foothill,
                &default_id,
                &default_id,
                &key_uri,
                &key_format,
                &key_format_version,
                &server_uri,
                &protocol_type,
                &fps_cert,
            )
            .ptr;
            if key.obj.is_null() {
                return None;
            }
            let context = (self.api.decrypt_context)(
                foothill,
                key.obj.cast::<StdString>(),
                &DECRYPTOR_TYPE,
                &FLAG,
            )
            .ptr;
            if context.obj.is_null() {
                return None;
            }
            *(self.api.kd_context)(context.obj)
        };
        if slot.is_null() {
            return None;
        }
        let slot = KdSlot(slot);
        if preshare {
            *lock(&self.preshare) = Some(slot);
        }
        Some(slot)
    }
}

/// Apple's dialog handler (`dialogHandler`). "Sign In" offers to use the existing Apple ID;
/// during a sign-in any dialog with buttons goes to the user (the phone-OTP sub-menu after
/// "Can't get to your devices?" may have one option only); anything else is dismissed with
/// its first button ("Not Now" for concurrent-stream dialogs): without a selected button
/// handleProtocolDialogResponse dereferences null.
unsafe extern "C" fn dialog_handler(
    j: i64,
    dialog: *mut SharedPtr,
    _response_handler: *mut SharedPtr,
) {
    let Some(w) = WRAPPER.get() else { return };
    let api = &w.api;
    // SAFETY: Apple passes a live ProtocolDialog; the response block is built as the library
    // expects and the buttons are elements of the dialog's own vector.
    unsafe {
        let d = (*dialog).obj;
        let title = string_lossy((api.dialog_title)(d));
        let text = string_lossy((api.dialog_message)(d));
        debug!("[.] dialogHandler: {{title: {title}, message: {text}}}");
        if !title.is_empty() && title != "Sign In" {
            // Keep what Apple said: the host shows it, and tells a locked account from a wrong
            // password by it.
            lock(&SIGN_IN).last_dialog = if text.is_empty() {
                title.clone()
            } else {
                format!("{title}: {text}")
            };
        }

        // The library keeps this response (and its control block) after the handler returns
        // and processes it on another thread, so it lives on the heap, never freed.
        let response = emplace_block(w.vtables.dialog_response, 72, 24);
        (api.dialog_response_ctor)(response.obj);

        let buttons = (api.dialog_buttons)(d);
        let buttons: &[SharedPtr] = if buttons.is_null() {
            &[]
        } else {
            (*buttons).as_slice()
        };
        info!(
            "[.] dialogHandler: title='{title}' buttons={}",
            buttons.len()
        );
        let button_title = |b: &SharedPtr| string_lossy((api.button_title)(b.obj));

        let (signing_in, prompt) = {
            let s = lock(&SIGN_IN);
            (s.username.is_some(), s.prompt.clone())
        };
        let selected = if title == "Sign In" {
            buttons
                .iter()
                .find(|b| button_title(b) == "Use Existing Apple ID")
        } else if let (true, Some(prompt), false) = (signing_in, prompt, buttons.is_empty()) {
            let labels: Vec<String> = buttons.iter().map(button_title).collect();
            write_drm_state(WrapperState::WaitingDialog);
            // A panic must not unwind into Apple's code: it cancels the sign-in instead, with
            // the first button, as for an out-of-range choice.
            let chosen = catch_unwind(AssertUnwindSafe(|| prompt.dialog_choice(&title, &labels)))
                .unwrap_or_else(|_| {
                    warn!("[.] dialogHandler: the prompt panicked; cancelling the sign-in");
                    lock(&SIGN_IN).cancelled = true;
                    0
                });
            let chosen = if chosen < buttons.len() { chosen } else { 0 };
            debug!(
                "[.] dialogHandler: user picked button {chosen} '{}'",
                labels[chosen]
            );
            buttons.get(chosen)
        } else {
            let first = buttons.first();
            if let Some(b) = first {
                debug!("[.] dialogHandler: auto-dismissing '{}'", button_title(b));
            }
            first
        };
        if let Some(button) = selected {
            (api.dialog_response_set_selected_button)(response.obj, button);
        }
        (api.handle_protocol_dialog_response)(w.presentation.load(Ordering::SeqCst), &j, &response);
    }
}

/// Apple's credentials handler (`credentialHandler`): answers with the account being signed
/// in, appending the verification code when Apple asks for one. With nobody signing in (the
/// credentials are dropped once a sign-in ends), Apple wants the account again mid-session,
/// e.g. for an expired token: answering would resubmit an old password or ask for a code
/// nobody expects, so the request is cancelled and the host shows its own sign-in.
unsafe extern "C" fn credentials_handler(
    request: *mut SharedPtr,
    _response_handler: *mut SharedPtr,
) {
    let Some(w) = WRAPPER.get() else { return };
    let api = &w.api;
    // SAFETY: Apple passes a live CredentialsRequest.
    let (need_code, title, message) = unsafe {
        let r = (*request).obj;
        (
            (api.credentials_requires_code)(r) != 0,
            string_lossy((api.credentials_title)(r)),
            string_lossy((api.credentials_message)(r)),
        )
    };
    info!("[.] credentialHandler: {{title: {title}, message: {message}, 2FA: {need_code}}}");

    let (username, mut password, prompt) = {
        let mut s = lock(&SIGN_IN);
        let len = s.password_len;
        if let Some(p) = s.password.as_mut().filter(|p| len > 0 && p.len() > len) {
            p.truncate(len);
        }
        (s.username.clone(), s.password.clone(), s.prompt.clone())
    };
    let mut answer = match (&username, &password) {
        (Some(_), Some(_)) => CredentialsAnswer::Submit,
        _ => {
            info!("[.] credentialHandler: no sign-in in progress, cancelling the request");
            CredentialsAnswer::Cancel
        }
    };

    if need_code && !matches!(answer, CredentialsAnswer::Cancel) {
        write_drm_state(WrapperState::Waiting2fa);
        // A panic must not unwind into Apple's code: it is a cancel.
        let reply = prompt.map_or(VerificationReply::Cancel, |p| {
            catch_unwind(AssertUnwindSafe(|| p.verification_code())).unwrap_or_else(|_| {
                warn!("[.] credentialHandler: the prompt panicked; cancelling the sign-in");
                VerificationReply::Cancel
            })
        });
        match reply {
            // No code: answering with the bare password would only count as another wrong
            // attempt at Apple, and repeated ones lock the account. Cancel instead.
            VerificationReply::Cancel => {
                answer = CredentialsAnswer::Cancel;
                lock(&SIGN_IN).cancelled = true;
            }
            // Resend without submitting a code; the password stays as it is.
            VerificationReply::Resend => answer = CredentialsAnswer::Resend,
            // Apple then shows the phone-OTP sub-menu through the dialog handler.
            VerificationReply::CantReachDevices => answer = CredentialsAnswer::CantReachDevices,
            VerificationReply::Code(code) => {
                if let Some(p) = password.as_mut() {
                    p.extend(code.chars().take(6));
                    lock(&SIGN_IN).password = Some(p.clone());
                }
            }
        }
    }

    let cancelled = matches!(answer, CredentialsAnswer::Cancel);
    let user = CString::new(if cancelled {
        ""
    } else {
        username.as_deref().unwrap_or("")
    })
    .unwrap_or_default();
    let pass = CString::new(if cancelled {
        ""
    } else {
        password.as_deref().unwrap_or("")
    })
    .unwrap_or_default();
    debug!(
        "[.] credential payload lengths: username={} password={}",
        user.as_bytes().len(),
        pass.as_bytes().len()
    );

    // SAFETY: fresh storage for the response, built as the library expects (kept and used on
    // another thread, so never freed); the strings outlive the setters, which copy them.
    unsafe {
        let response = emplace_block(w.vtables.credentials_response, 80, 24);
        (api.credentials_response_ctor)(response.obj);
        (api.credentials_set_user_name)(response.obj, &StdString::borrow(&user));
        (api.credentials_set_password)(response.obj, &StdString::borrow(&pass));
        (api.credentials_set_response_type)(response.obj, answer as c_int);
        (api.handle_credentials_response)(w.presentation.load(Ordering::SeqCst), &response);
    }
}

/// Renews the lease and every key context (`refresh_decrypt_ctx`), for the recovery worker:
/// requests a new lease, tears down the cached contexts so fresh keys are derived, and
/// rebuilds the prefetch context. Returns whether the prefetch context was rebuilt
/// (`is_preshare_ctx_ready`): an abort inside Apple's code (an SVError such as "Invalid CKC",
/// likely exactly when a lease is being renewed) is a failed refresh, which the worker retries.
pub(crate) fn refresh_decrypt_ctx() -> bool {
    let Some(w) = WRAPPER.get() else { return false };
    let out = {
        let _call = lock(&w.fp_call);
        w.epoch.fetch_add(1, Ordering::SeqCst);
        // Cleared first, so a refresh that aborts leaves no stale context behind. Every
        // reader of the cache runs under `fp_call`, so none sees it empty mid-refresh.
        *lock(&w.preshare) = None;
        guard::run("refresh", move || {
            let api = &w.api;
            let automatically: u8 = 1;
            // SAFETY: the lease manager and FootHill session were set up before the worker
            // started.
            unsafe {
                (api.request_lease)(w.lease_manager.load(Ordering::SeqCst), &automatically);
                (api.reset_all_contexts)(w.foothill.load(Ordering::SeqCst));
            }
            w.kd_context(c"0", PREFETCH_KEY_URI).is_some()
        })
    };
    match out {
        Ok(Ok(ready)) => {
            info!("[!] refreshed context");
            ready
        }
        Ok(Err(guard::Aborted)) => {
            warn!("[guard] lease refresh: SVError escaped");
            false
        }
        Err(e) => {
            warn!("[!] lease refresh: cannot start its thread: {e}");
            false
        }
    }
}

/// The running FairPlay session. Its existence means a start succeeded.
#[derive(Clone, Copy)]
pub struct FairPlay {
    w: &'static Wrapper,
}

impl FairPlay {
    /// Opens the key context for (adam ID, `skd://` URI) as the wrapper's `handle()` did
    /// (`hybris_open_kd_ctx_from_uri`). Adam ID "0" is the shared prefetch key and is cached.
    /// Blocks while Apple delivers the key.
    pub fn open_key_context(
        &self,
        adam_id: &str,
        uri: &str,
    ) -> Result<KeyContext, KeyContextError> {
        let (Ok(rustet), Ok(key_uri)) = (CString::new(adam_id), CString::new(uri)) else {
            return Err(KeyContextError::Failed);
        };
        if recovery::active() {
            warn!("[hybris] key context refused: lease recovery in progress");
            return Err(KeyContextError::RecoveryInProgress);
        }
        let w = self.w;
        let (epoch, out) = {
            let _call = lock(&w.fp_call);
            let epoch = w.epoch.load(Ordering::SeqCst);
            (
                epoch,
                guard::run("key", move || w.kd_context(&rustet, &key_uri)),
            )
        };
        match out {
            Ok(Ok(Some(slot))) => {
                debug!("[hybris] key ctx rustet={adam_id} uri={uri}: {:p}", slot.0);
                Ok(KeyContext { slot, epoch })
            }
            Ok(Ok(None)) | Ok(Err(guard::Aborted)) | Err(_) => {
                if let Ok(Err(guard::Aborted)) = out {
                    warn!("[guard] key context: SVError escaped (Invalid CKC or FPS failure)");
                }
                warn!("[hybris] key context failed for rustet={adam_id} uri={uri}");
                Err(KeyContextError::Failed)
            }
        }
    }

    /// Drops every key context and has Apple derive fresh keys (a new lease, then
    /// `resetAllContexts`). For a key that decrypts to garbage: the library hands out an
    /// expired cached key as if it were valid. Contexts opened before the call then fail with
    /// [`DecryptError::StaleContext`]. Returns whether the prefetch context was rebuilt.
    /// Blocks while Apple answers.
    pub fn refresh_keys(&self) -> bool {
        refresh_decrypt_ctx()
    }

    /// Decrypts one sample in place with `NfcRKVnxuKZy04KWbdFu71Ou`
    /// (`hybris_backend_decrypt`). Only whole 16-byte blocks are touched; a sample shorter
    /// than one block is left as it is.
    pub fn decrypt(&self, ctx: &KeyContext, sample: &mut [u8]) -> Result<(), DecryptError> {
        let whole = sample.len() & !0xF;
        if whole == 0 {
            return Ok(());
        }
        let w = self.w;
        let _call = lock(&w.fp_call);
        if ctx.epoch != w.epoch.load(Ordering::SeqCst) {
            return Err(DecryptError::StaleContext); // freed by a lease refresh; reopen the key
        }
        // The context lives in the slot: the C's TCP path decrypted with *kdContext, and
        // passing the slot itself yields garbage samples.
        // SAFETY: the slot belongs to the current lease generation (checked above, under the
        // lock that refreshes take), so Apple has not freed it.
        let live = unsafe { *ctx.slot.0 };
        if live.is_null() {
            return Err(DecryptError::Failed);
        }
        let data = sample.as_mut_ptr().cast::<c_void>();
        // SAFETY: `live` is a decrypt context; input and output are the same `whole` bytes.
        unsafe { (w.api.nfc_decrypt)(live, NFC_SELECTOR_SAMPLE, data, data, whole) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_needs_nine_fields() {
        assert_eq!(device_fields("").unwrap().len(), 9);
        assert_eq!(
            device_fields("").unwrap()[8].to_str().unwrap(),
            "dc28071e981c439e"
        );
        // strtok skips empty fields.
        assert_eq!(
            device_fields("a//b/c/d/e/f/g/h/i").unwrap()[1]
                .to_str()
                .unwrap(),
            "b"
        );
        assert!(matches!(device_fields("a/b/c"), Err(Error::DeviceInfo)));
    }
}
