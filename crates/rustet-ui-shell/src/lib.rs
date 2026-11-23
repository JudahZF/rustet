//! The app shell: start-up, the main window and everything in it that is not a page.
//!
//! [`run`] starts Rustet: the runtime, one instance per user, the config directory, the prefs and
//! the encrypted store, [`rustet_app::App`], the kit, then the main window. The window is
//! [`shell::Shell`]: the sidebar, the strip with back and forward (and the search field on
//! Search pages), the player pill, the queue and lyrics panels, Now Playing, the sign-in sheet,
//! toasts, and the overlays pages ask for through [`rustet_ui::host::Host`]. The page region is drawn by the views the binary hands in
//! ([`Views`]). Closing the window hides Rustet; only Quit ends it.
#![forbid(unsafe_code)]

mod deck;
mod keys;
mod mini;
mod now_playing;
mod queue;
mod rail;
#[cfg(feature = "scenario")]
pub mod scenario;
mod shell;
mod signin;
mod startup;
#[cfg(all(test, feature = "test-support"))]
mod view_tests;

use gpui::{AnyView, Window};
use rustet_app::nav::{NavState, PageState};
use rustet_app::route::Route;

pub use deck::{DeckDrawing, Primary, TrackLine, deck_drawing};
pub use keys::{dom_key, may_fire};
pub use queue::queue_index;
pub use rail::{destinations, is_active};
pub use signin::{Page, SheetDrawing, sheet_drawing};
pub use startup::run;

/// Makes the view of a region for a window.
pub type ViewFn = fn(&rustet_app::App, &mut Window, &mut gpui::App) -> AnyView;

/// The views the binary fills in: the page area, Settings and Downloads. Each is made once per
/// main window.
#[derive(Clone, Copy)]
pub struct Views {
    pub page: ViewFn,
    pub settings: ViewFn,
    pub downloads: ViewFn,
}

/// The command line of the UI role.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    /// Every argument after the program name: handed to a running Rustet, and used again by a
    /// restart.
    pub raw: Vec<String>,
    /// The first argument that is a link Rustet opens (`rustet rustet://album/…`).
    pub link: Option<String>,
    /// `--hidden`: started at login, stay in the tray until asked.
    pub hidden: bool,
    /// `--no-desktop`: no MPRIS, tray or media keys (tests, a headless run).
    pub no_desktop: bool,
    /// `--scenario NAME`: drive the window for screenshots (`scenario`, feature `scenario`).
    #[cfg(feature = "scenario")]
    pub scenario: Option<String>,
}

impl Args {
    pub fn parse(raw: Vec<String>) -> Self {
        Self {
            link: find_link(&raw).map(str::to_owned),
            hidden: raw.iter().any(|a| a == "--hidden"),
            no_desktop: raw.iter().any(|a| a == "--no-desktop"),
            #[cfg(feature = "scenario")]
            scenario: raw
                .iter()
                .position(|a| a == "--scenario")
                .and_then(|i| raw.get(i + 1))
                .cloned(),
            raw,
        }
    }
}

/// The first argument that is a link Rustet opens: a page of its own (`rustet://search?term=x`,
/// which `rustet_desktop::links::find_link_arg` does not know) or an Apple Music link.
pub fn find_link<S: AsRef<str>>(args: &[S]) -> Option<&str> {
    args.iter()
        .map(AsRef::as_ref)
        .filter(|a| !a.starts_with('-'))
        .find(|a| Route::from_link(a).is_some() || rustet_desktop::links::parse_link(a).is_some())
}

/// What draws the page region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// The page area: every loaded route, with its loading and error states.
    Page,
    Settings,
    Downloads,
}

/// The region for the navigation state: Settings and Downloads are local pages, drawn from
/// their own models; everything else is the page area's.
pub fn region(nav: &NavState) -> Region {
    match (&nav.page, &nav.route) {
        (PageState::Local, Route::Settings { .. }) => Region::Settings,
        (PageState::Local, Route::Downloads) => Region::Downloads,
        _ => Region::Page,
    }
}

#[cfg(test)]
mod tests {
    use rustet_app::nav::MoreState;

    use super::*;

    fn nav(route: Route, page: PageState) -> NavState {
        NavState {
            route,
            page,
            more: MoreState::Idle,
            can_back: false,
            can_forward: false,
        }
    }

    #[test]
    fn local_routes_go_to_their_own_views() {
        assert_eq!(
            region(&nav(Route::Settings { section: None }, PageState::Local)),
            Region::Settings
        );
        assert_eq!(
            region(&nav(Route::Downloads, PageState::Local)),
            Region::Downloads
        );
        assert_eq!(region(&nav(Route::New, PageState::Loading)), Region::Page);
        assert_eq!(
            region(&nav(Route::Album { id: "1".into() }, PageState::Loading)),
            Region::Page
        );
    }

    #[test]
    fn arguments_of_the_ui_role() {
        let a = Args::parse(vec![
            "rustet://album/1440833098".into(),
            "--hidden".into(),
            "--no-desktop".into(),
        ]);
        assert_eq!(a.link.as_deref(), Some("rustet://album/1440833098"));
        assert!(a.hidden && a.no_desktop);
        #[cfg(feature = "scenario")]
        assert_eq!(
            Args::parse(vec!["--scenario".into(), "tour".into()])
                .scenario
                .as_deref(),
            Some("tour")
        );
        assert_eq!(
            find_link(&["--hidden", "rustet://search?term=boygenius"]),
            Some("rustet://search?term=boygenius")
        );
        assert!(find_link(&["https://music.apple.com/us/album/x/1440833098"]).is_some());
        assert_eq!(find_link(&["--no-desktop", "tour"]), None);
    }
}
