//! One libvlc instance, shared by the decks that play on it.

use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::mpsc::UnboundedSender;

use crate::deck::Deck;
use crate::events::{DeckEvent, DeckId};
use crate::{Error, ffi};

/// A libvlc instance: the loaded plugins, the audio output module and the audio identity the
/// desktop mixer shows ("Rustet"). Cheap to clone; released when the last clone and the last
/// deck on it are dropped.
#[derive(Clone)]
pub struct Instance(Arc<Shared>);

pub(crate) struct Shared {
    pub(crate) api: &'static ffi::Api,
    pub(crate) raw: NonNull<ffi::Instance>,
    next_deck: AtomicU32,
}

// SAFETY: a libvlc instance may be used from any thread; libvlc locks internally.
unsafe impl Send for Shared {}
// SAFETY: as above, every libvlc call on an instance is thread-safe.
unsafe impl Sync for Shared {}

impl Instance {
    /// Loads libvlc and creates an instance with libvlc's default options.
    ///
    /// Fails with [`Error::Unavailable`] when libvlc is not installed or bundled: the engine then
    /// runs without libvlc playback.
    pub fn new() -> Result<Self, Error> {
        Self::with_args(&[])
    }

    /// Like [`Instance::new`] with extra libvlc command-line options. Tests pass
    /// `--aout=adummy` so nothing is heard.
    pub fn with_args(args: &[&CStr]) -> Result<Self, Error> {
        let api = ffi::api().map_err(|e| Error::Unavailable(e.to_owned()))?;
        let argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        let argc = c_int::try_from(argv.len()).expect("few libvlc arguments");
        // SAFETY: argv holds `argc` NUL-terminated strings that outlive the call.
        let raw = NonNull::new(unsafe { (api.libvlc_new)(argc, argv.as_ptr()) })
            .ok_or(Error::InstanceFailed)?;
        // A stable audio identity lets the desktop mixer retain its per-app settings
        // (from PR #1: https://github.com/silentone12725/apple-music-linux/pull/1).
        let version = env!("CARGO_PKG_VERSION");
        let agent = CString::new(format!("Rustet/{version}")).expect("no NUL");
        let version = CString::new(version).expect("no NUL");
        // SAFETY: `raw` is live; libvlc copies the strings.
        unsafe {
            (api.libvlc_set_user_agent)(raw.as_ptr(), c"Rustet".as_ptr(), agent.as_ptr());
            (api.libvlc_set_app_id)(
                raw.as_ptr(),
                c"io.github.judahzf.Rustet".as_ptr(),
                version.as_ptr(),
                c"rustet".as_ptr(),
            );
        }
        Ok(Self(Arc::new(Shared {
            api,
            raw,
            next_deck: AtomicU32::new(0),
        })))
    }

    /// Creates a deck on this instance whose events go to `events`. Decks of one instance can
    /// share one channel: each event names its deck. The deck keeps its audio output between
    /// tracks, so a volume set on it while idle or cued applies to its next track.
    pub fn deck(&self, events: UnboundedSender<DeckEvent>) -> Result<Deck, Error> {
        Deck::create(self.clone(), Some(events), true)
    }

    pub(crate) fn next_deck_id(&self) -> DeckId {
        DeckId(self.0.next_deck.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) fn shared(&self) -> &Shared {
        &self.0
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the last reference is gone: every deck (which holds a clone) has released its
        // media player, so the instance is released exactly once.
        unsafe { (self.api.libvlc_release)(self.raw.as_ptr()) };
    }
}
