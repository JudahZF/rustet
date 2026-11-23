//! The page area: draws the page of the current route from `rustet_app`'s navigation state.
//!
//! The shell puts [`page_area`] in the page region for every route whose page is not
//! [`PageState::Local`](rustet_app::nav::PageState::Local). The view follows
//! `app.nav().state()` by itself: placeholders shaped like the page that is coming, the page,
//! or its error state with the recovery action.
//!
//! - [`area`]: the [`PageArea`] view: navigation, the push motion, views kept per history entry
//!   (so Back returns to the same scroll position and focus), and what pages ask for, turned
//!   into `App` calls.
//! - [`view`]: [`PageView`], one per page model: header, shelves, track lists, grids, "load
//!   more", the playing row.
//! - [`menu`]: the context menus of rows, tiles and pages, as [`menu::Entry`] lists.
//! - [`modal`]: the playlist name sheet, the confirm dialog and the playlist picker.
//! - [`history`]: the history mirror that keeps a view per back and forward entry.
//!
//! Views never call `App` themselves: they report a [`PageEvent`] through a [`Sink`], and the
//! area does what it says. Menus, dialogs, toasts and links go to the window through
//! [`rustet_ui::host::Host`].
#![forbid(unsafe_code)]

pub mod area;
pub mod context;
pub mod event;
pub mod history;
pub mod menu;
pub mod modal;
mod placeholder;
pub mod view;

pub use area::{PageArea, page_area};
pub use context::{PageContext, Playing, PlaylistChoice};
pub use event::{DownloadRequest, PageEvent, PlayRequest, Sink};
pub use view::PageView;
