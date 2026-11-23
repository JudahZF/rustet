//! Desktop state and services of the shell with no web-view dependency: preferences, encrypted
//! store, themes, wallpaper lookup, links, engine supervisor.
//!
//! The shell (`rustet-shell`) calls these from its bridge handlers and does the page side itself:
//! dialogs, script and style injection, windows. Every function that touches disk takes its
//! directory as a parameter, so nothing here assumes the real config directory.
//!
//! Page input is untrusted: the functions a bridge handler calls take the raw JSON arguments
//! (`serde_json::Value`) and apply the Electron shell's validation themselves.
#![forbid(unsafe_code)]

pub mod atomic;
pub mod cleanup;
pub mod drm;
pub mod engine;
pub mod fairplay;
pub mod links;
pub mod load_failure;
pub mod platform;
pub mod prefs;
pub mod store;
pub mod system;
pub mod theme;
pub mod wallpaper;
