//! What a page or a panel asks of the window around it.
//!
//! Pages and panels are drawn inside the frame and cannot draw over it. A menu, a dialog or a
//! toast belongs to the window, so they hand it over with [`Host::request`]. The shell
//! installs the handler ([`Host::install`]) and draws what it is given as frame overlays.

use std::rc::Rc;

use gpui::{AnyView, App, Global, Pixels, Point, Window};

use crate::components::{MenuItem, Toast};

/// A request to the window.
pub enum HostRequest {
    /// Show a context menu at a window position.
    Menu {
        items: Vec<MenuItem>,
        position: Point<Pixels>,
    },
    /// Show `view` (a `Sheet` or a `Dialog`) over the frame until [`HostRequest::CloseModal`].
    /// The host gives the focus back to what had it when the modal closes.
    Modal(AnyView),
    CloseModal,
    Toast(Toast),
    /// Open a link Rustet has no page for, in the browser.
    OpenExternal(String),
    /// A music video started: show it (Now Playing).
    ShowNowPlaying,
}

type Handler = Rc<dyn Fn(HostRequest, &mut Window, &mut App)>;

/// The window's handler for [`HostRequest`]s.
pub struct Host(Handler);

impl Global for Host {}

impl Host {
    pub fn install(handler: impl Fn(HostRequest, &mut Window, &mut App) + 'static, cx: &mut App) {
        cx.set_global(Self(Rc::new(handler)));
    }

    /// Hands `request` to the window. Without a host (a test) nothing happens.
    pub fn request(request: HostRequest, window: &mut Window, cx: &mut App) {
        let Some(handler) = cx.try_global::<Self>().map(|host| host.0.clone()) else {
            return;
        };
        handler(request, window, cx);
    }
}
