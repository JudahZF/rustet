//! Rustet's application logic without a UI toolkit.
//!
//! `rustet-ui` (gpui) draws and handles input. Everything it shows and does comes from here, as
//! plain async Rust that tests run without a display:
//!
//! - [`app`]: the one [`App`] a view layer starts and holds. It ties the parts below together;
//!   its module documentation says what must run on the tokio runtime.
//! - [`nav`], [`player`], [`downloads`], [`account::AccountModel`]: the models views draw. What
//!   a view observes is a `tokio::sync::watch` or a broadcast stream; what it does is a method.
//! - [`route`], [`pages`], [`library`]: what a screen shows. A [`route::Route`] has a typed loader
//!   that returns a page model.
//! - [`engine`], [`account`], [`settings`], [`integrations`]: what the app does. The engine
//!   child process and its player API, sign-in state, preferences, and the desktop integrations
//!   driven by player events.
#![forbid(unsafe_code)]

pub mod account;
pub mod app;
pub mod downloads;
pub mod engine;
pub mod integrations;
pub mod library;
pub mod nav;
pub mod pages;
pub mod player;
pub mod route;
pub mod settings;

pub use app::{App, AppConfig};
