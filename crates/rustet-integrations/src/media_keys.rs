//! Global media keys on X11: the port of the Electron shell's `globalShortcut` registrations
//! (MediaPlayPause, MediaNextTrack, MediaPreviousTrack, MediaStop), which Chromium implements
//! as passive key grabs on the X root window.
//!
//! Most desktops route media keys to the active MPRIS player themselves (see [`crate::mpris`]);
//! there the desktop already holds the grab and ours fails with "already taken", as it did in
//! Electron. The grab matters on bare X11 window managers and for Bluetooth AVRCP buttons that
//! arrive as plain key events. Wayland has no global grabs: without an X server (or with
//! XWayland, which only sees keys while an X window has focus) this does little or nothing.
//! On Windows the media keys go through SMTC instead.

use std::thread;

use tokio::sync::mpsc;
use x11rb::{
    connection::Connection,
    errors::{ConnectionError, ReplyError},
    protocol::{
        Event,
        xproto::{
            ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, GrabMode, ModMask,
            WindowClass,
        },
    },
    rust_connection::RustConnection,
};

use crate::command::Transport;

// XF86 keysyms (X11/XF86keysym.h) and the command each one sends.
const KEYS: [(u32, Transport, &str); 4] = [
    (0x1008_FF14, Transport::PlayPause, "MediaPlayPause"), // XF86AudioPlay
    (0x1008_FF17, Transport::Next, "MediaNextTrack"),      // XF86AudioNext
    (0x1008_FF16, Transport::Previous, "MediaPreviousTrack"), // XF86AudioPrev
    (0x1008_FF15, Transport::Pause, "MediaStop"),          // XF86AudioStop
];

#[derive(Debug, thiserror::Error)]
pub enum MediaKeysError {
    #[error("no X display: {0}")]
    Connect(#[from] x11rb::errors::ConnectError),
    #[error("X11: {0}")]
    Connection(#[from] ConnectionError),
    #[error("X11: {0}")]
    Reply(#[from] ReplyError),
    #[error("X11: {0}")]
    Id(#[from] x11rb::errors::ReplyOrIdError),
}

/// The grabs. Dropping the handle releases them and stops the listener thread.
pub struct MediaKeys {
    conn: std::sync::Arc<RustConnection>,
    wakeup: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl MediaKeys {
    /// Grabs the media keys on `display` (`None`: `$DISPLAY`). Keys another client holds are
    /// skipped with a warning. Presses arrive on the returned receiver.
    pub fn spawn(
        display: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Transport>), MediaKeysError> {
        let (conn, screen) = x11rb::connect(display)?;
        let root = conn.setup().roots[screen].root;
        let keycodes = keycodes_for(&conn)?;

        // NumLock and CapsLock must not stop a key from matching, so each key is grabbed with
        // every combination of the two.
        let locks = [
            ModMask::from(0u16),
            ModMask::LOCK,
            ModMask::M2,
            ModMask::LOCK | ModMask::M2,
        ];
        let mut grabbed = Vec::new();
        for &(sym, cmd, name) in &KEYS {
            let codes: Vec<u8> = keycodes
                .iter()
                .filter(|(_, s)| *s == sym)
                .map(|(c, _)| *c)
                .collect();
            let mut ok = !codes.is_empty();
            for &code in &codes {
                for &m in &locks {
                    ok &= conn
                        .grab_key(false, root, m, code, GrabMode::ASYNC, GrabMode::ASYNC)?
                        .check()
                        .is_ok();
                }
            }
            if ok {
                grabbed.extend(codes.into_iter().map(|c| (c, cmd)));
            } else {
                tracing::warn!("media key shortcut already taken by another app: {name}");
            }
        }

        // An unmapped window the handle can message to wake the listener when it is dropped.
        let wakeup = conn.generate_id()?;
        conn.create_window(
            0,
            wakeup,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )?
        .check()?;
        conn.flush()?;

        let conn = std::sync::Arc::new(conn);
        let (tx, rx) = mpsc::unbounded_channel();
        let listener = conn.clone();
        let thread = thread::Builder::new()
            .name("rustet-media-keys".into())
            .spawn(move || listen(&listener, wakeup, &grabbed, &tx))
            .expect("spawn media key thread");
        Ok((
            Self {
                conn,
                wakeup,
                thread: Some(thread),
            },
            rx,
        ))
    }
}

/// `(keycode, keysym)` for every keysym of the keyboard mapping.
fn keycodes_for(conn: &RustConnection) -> Result<Vec<(u8, u32)>, MediaKeysError> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    let per = usize::from(map.keysyms_per_keycode.max(1));
    Ok(map
        .keysyms
        .chunks(per)
        .zip(min..=max)
        .flat_map(|(syms, code)| syms.iter().map(move |&s| (code, s)))
        .collect())
}

fn listen(
    conn: &RustConnection,
    wakeup: u32,
    grabbed: &[(u8, Transport)],
    tx: &mpsc::UnboundedSender<Transport>,
) {
    loop {
        match conn.wait_for_event() {
            Ok(Event::KeyPress(e)) => {
                if let Some(&(_, cmd)) = grabbed.iter().find(|(c, _)| *c == e.detail)
                    && tx.send(cmd).is_err()
                {
                    return;
                }
            }
            Ok(Event::ClientMessage(e)) if e.window == wakeup => return,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("media keys: X connection lost: {e}");
                return;
            }
        }
    }
}

impl Drop for MediaKeys {
    fn drop(&mut self) {
        let wake = ClientMessageEvent::new(32, self.wakeup, x11rb::NONE, [0u32; 5]);
        let sent = self
            .conn
            .send_event(false, self.wakeup, EventMask::NO_EVENT, wake)
            .map_err(drop)
            .and_then(|_| self.conn.flush().map_err(drop));
        if sent.is_ok()
            && let Some(t) = self.thread.take()
        {
            let _ = t.join();
        }
        // Closing the connection releases the grabs.
    }
}
