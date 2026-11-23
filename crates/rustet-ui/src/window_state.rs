//! Remembers the main window's size, position and maximised state across runs.
//!
//! Wayland never tells a client where its window is, and does not let it choose: there only
//! the size and maximised state come back. On X11 and Windows the position does too.

use std::io;
use std::path::{Path, PathBuf};

use gpui::{Bounds, Pixels, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowState {
    /// `None` where the platform does not report positions (Wayland).
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

impl WindowState {
    /// The state for gpui's `window_bounds()`. `inset` is the client-side decoration band
    /// included in those bounds; it is removed so a restored window has the same content size.
    pub fn from_bounds(bounds: WindowBounds, inset: Pixels, position_known: bool) -> Self {
        let (b, maximized) = match bounds {
            WindowBounds::Windowed(b) => (b, false),
            WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => (b, true),
        };
        let b = Bounds::new(
            b.origin + point(inset, inset),
            b.size - size(inset * 2., inset * 2.),
        );
        Self {
            x: position_known.then_some(f32::from(b.origin.x)),
            y: position_known.then_some(f32::from(b.origin.y)),
            width: f32::from(b.size.width),
            height: f32::from(b.size.height),
            maximized,
        }
    }

    /// The bounds to open with. Sizes are clamped to `min`; a missing position centres on the
    /// display (`centered` is the caller's centred bounds at this size).
    pub fn to_bounds(
        self,
        min: gpui::Size<Pixels>,
        centered: impl FnOnce(gpui::Size<Pixels>) -> Bounds<Pixels>,
    ) -> WindowBounds {
        let sz = size(
            px(self.width).max(min.width),
            px(self.height).max(min.height),
        );
        let b = match (self.x, self.y) {
            (Some(x), Some(y)) => Bounds::new(point(px(x), px(y)), sz),
            _ => centered(sz),
        };
        if self.maximized {
            WindowBounds::Maximized(b)
        } else {
            WindowBounds::Windowed(b)
        }
    }

    pub fn load(path: &Path) -> Option<Self> {
        serde_json::from_slice::<Self>(&std::fs::read(path).ok()?)
            .ok()
            .filter(|s| {
                s.width.is_finite() && s.height.is_finite() && s.width > 0. && s.height > 0.
            })
    }

    /// Writes atomically (temporary file, then rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp: PathBuf = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window.json");
        let b = Bounds::new(point(px(40.), px(60.)), size(px(1200.), px(800.)));
        let state = WindowState::from_bounds(WindowBounds::Maximized(b), px(0.), true);
        state.save(&path).unwrap();
        let back = WindowState::load(&path).unwrap();
        assert_eq!(back, state);
        let min = size(px(1300.), px(400.));
        assert_eq!(
            back.to_bounds(min, |_| unreachable!()),
            WindowBounds::Maximized(Bounds::new(
                point(px(40.), px(60.)),
                size(px(1300.), px(800.))
            ))
        );
        // Wayland: no position, so the window is centred.
        let wl = WindowState::from_bounds(WindowBounds::Windowed(b), px(0.), false);
        // The client-side decoration band is not part of the saved size.
        let csd = WindowState::from_bounds(WindowBounds::Windowed(b), px(10.), true);
        assert_eq!((csd.x, csd.width, csd.height), (Some(50.), 1180., 780.));
        let centred = Bounds::new(point(px(1.), px(2.)), size(px(1200.), px(800.)));
        assert_eq!(
            wl.to_bounds(min.map(|_| px(0.)), |_| centred),
            WindowBounds::Windowed(centred)
        );
        // Garbage is ignored.
        std::fs::write(&path, b"{\"width\": -1, \"height\": 5}").unwrap();
        assert_eq!(WindowState::load(&path), None);
    }
}
