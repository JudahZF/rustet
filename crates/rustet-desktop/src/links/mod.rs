//! Links: parsing what Rustet is asked to open, claiming URL schemes, and handing links from a
//! second `rustet` process to the running one.

pub mod deeplink;
pub mod schemes;
pub mod single_instance;

pub use deeplink::{Intent, ItemType, Link, Unsupported, find_link_arg, parse_link};
