//! Rustet's per-user config and cache directories.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The directory name under the OS config and cache roots.
pub const NAME: &str = "rustet";

/// The per-user config directory, for example `~/.config/rustet`.
pub fn config_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| root(dirs::config_dir(), ".config").join(NAME))
}

/// The per-user cache directory, for example `~/.cache/rustet`.
pub fn cache_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| root(dirs::cache_dir(), ".cache").join(NAME))
}

/// The OS directory, else `~/<fallback>`, else the temporary directory.
fn root(os_dir: Option<PathBuf>, fallback: &str) -> PathBuf {
    os_dir
        .or_else(|| dirs::home_dir().map(|home| home.join(fallback)))
        .unwrap_or_else(std::env::temp_dir)
}
