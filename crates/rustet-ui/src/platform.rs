//! Window features gpui does not offer, done through the platform directly.
//!
//! - Always on top (the mini player): X11 `_NET_WM_STATE_ABOVE`, asked of the window manager
//!   over a second X connection of our own (X11 lets any client do this for any window).
//!   Wayland has no client protocol for it; compositors offer it through their own rules
//!   (Hyprland `pin`, KWin window rules).
//!
//! Hiding to the tray needs nothing here: with `QuitMode::Explicit` the app closes the main
//! window and keeps running, then opens a new one.

use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("not supported on this display server")]
    Unsupported,
    #[error("x11: {0}")]
    X11(String),
}

/// The X11 window id behind a gpui window, or `None` on other display servers.
pub fn x11_window(window: &Window) -> Option<u32> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok(),
        _ => None,
    }
}

/// Asks the window manager to keep `window` above others (or to stop).
pub fn set_always_on_top(window: &Window, on: bool) -> Result<(), PlatformError> {
    let xid = x11_window(window).ok_or(PlatformError::Unsupported)?;
    x11::set_above(xid, on).map_err(|e| PlatformError::X11(e.to_string()))
}

/// Whether the window manager reports `window` as kept above others.
pub fn is_always_on_top(window: &Window) -> Result<bool, PlatformError> {
    let xid = x11_window(window).ok_or(PlatformError::Unsupported)?;
    x11::is_above(xid).map_err(|e| PlatformError::X11(e.to_string()))
}

mod x11 {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask};
    use x11rb::rust_connection::RustConnection;

    pub type Error = Box<dyn std::error::Error>;

    fn connect() -> Result<(RustConnection, u32), Error> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        Ok((conn, root))
    }

    fn atom(conn: &RustConnection, name: &str) -> Result<u32, Error> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }

    /// EWMH: a `_NET_WM_STATE` client message to the root window adds or removes
    /// `_NET_WM_STATE_ABOVE` (data: action, property, second property, source = application).
    pub fn set_above(xid: u32, on: bool) -> Result<(), Error> {
        let (conn, root) = connect()?;
        let state = atom(&conn, "_NET_WM_STATE")?;
        let above = atom(&conn, "_NET_WM_STATE_ABOVE")?;
        let event = ClientMessageEvent::new(32, xid, state, [u32::from(on), above, 0, 1, 0]);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        conn.flush()?;
        Ok(())
    }

    pub fn is_above(xid: u32) -> Result<bool, Error> {
        let (conn, _) = connect()?;
        let state = atom(&conn, "_NET_WM_STATE")?;
        let above = atom(&conn, "_NET_WM_STATE_ABOVE")?;
        let reply = conn
            .get_property(false, xid, state, AtomEnum::ATOM, 0, 64)?
            .reply()?;
        Ok(reply
            .value32()
            .is_some_and(|mut atoms| atoms.any(|a| a == above)))
    }
}
