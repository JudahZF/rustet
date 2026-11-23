//! The most recently selected music-video quality label, for example `"1920x1080"`.
//!
//! A process-wide cell: the Apple provider writes it when it picks a video variant and
//! the engine reads it for its cache status, without either depending on the other.

use std::sync::RwLock;

static LABEL: RwLock<String> = RwLock::new(String::new());

/// Records the resolution of the most recently selected music-video variant.
pub fn set(resolution: &str) {
    let mut label = LABEL.write().unwrap_or_else(|e| e.into_inner());
    resolution.clone_into(&mut label);
}

/// The last recorded label; empty if none was set.
pub fn get() -> String {
    LABEL.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_concurrent() {
        set("");
        assert_eq!(get(), "", "unset label must be empty");
        std::thread::scope(|s| {
            for l in ["1920x1080", "1280x720", "3840x2160"] {
                s.spawn(move || set(l));
                s.spawn(get);
            }
        });
        set("1920x1080");
        assert_eq!(get(), "1920x1080");
    }
}
