//! The Windows shell integration seam: SMTC (system media transport controls), taskbar
//! thumbnail buttons and progress, jump-list tasks, launch at login.
//!
//! Only the platform-neutral part is implemented: [`parse_cmd_arg`] and the [`WinIntegration`]
//! API the shell calls on every platform. Its methods do nothing yet; a Windows implementation
//! fills them in behind `cfg(windows)` (the Electron reference is `electron/win-integration.mjs`):
//!
//! - `init`: set the AppUserModelID [`APP_USER_MODEL_ID`]; set three jump-list tasks that
//!   start the app with `--rustet-cmd=playpause|next|previous` ("Play / Pause" — "Toggle playback",
//!   "Next track" — "Skip to the next track", "Previous track" — "Go back to the previous
//!   track"); add the thumbnail toolbar: Previous, Play or Pause (by status), Next.
//! - `update` (every `mpris:update`): keep status, duration (`mpris:length` µs → ms, position
//!   reset on new metadata) and position; swap the Play/Pause thumbnail button when the status
//!   changes; set the taskbar progress (none when Stopped or without a duration, "paused" state
//!   when Paused); push to SMTC right away on a status or metadata change or `seeked`, otherwise
//!   at most once a second. SMTC metadata: title, artists joined with ", ", album, artwork
//!   (`mpris:artUrl`), timeline (duration, position clamped to it). SMTC buttons map to
//!   play/pause/next/previous and `seekto` to `setPosition`; send them as [`PageCommand`]s.
//!   Hardware media keys arrive through SMTC, so no key grab is used on Windows.
//! - `refresh`: re-apply the thumbnail buttons and progress after the window is shown.
//! - `open_at_login` / `set_open_at_login`: the login item, started with `--hidden`; never
//!   registered from an unpackaged development run.
//!
//! Elsewhere (and on Windows until that is written) every method is a no-op and the login item
//! is reported as unsupported.

use tokio::sync::mpsc;

use crate::{
    command::{PageCommand, Transport},
    now_playing::MprisUpdate,
};

/// The AppUserModelID Rustet registers on Windows.
pub const APP_USER_MODEL_ID: &str = "io.github.judahzf.Rustet";

/// The transport command of a `--rustet-cmd=<cmd>` argument (jump-list task, second launch).
/// Only the plain transport commands are accepted.
pub fn parse_cmd_arg<S: AsRef<str>>(argv: &[S]) -> Option<Transport> {
    argv.iter().find_map(|a| {
        let cmd = a.as_ref().strip_prefix("--rustet-cmd=")?;
        serde_json::from_value(serde_json::Value::String(cmd.to_owned())).ok()
    })
}

/// A native window handle (`HWND` on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeWindow(pub isize);

/// Windows media controls, taskbar and login item. See the module documentation.
pub struct WinIntegration {
    commands: mpsc::UnboundedSender<PageCommand>,
}

impl WinIntegration {
    /// Commands from SMTC, the thumbnail buttons and jump-list tasks go to `commands`
    /// (for the page's `mpris:cmd`).
    pub fn new(commands: mpsc::UnboundedSender<PageCommand>) -> Self {
        Self { commands }
    }

    /// Once, when the main window exists.
    pub fn init(&mut self, _window: NativeWindow) {}

    /// Every `mpris:update`.
    pub fn update(&mut self, _update: &MprisUpdate) {}

    /// After the main window is shown again.
    pub fn refresh(&mut self) {}

    /// A second launch's arguments: true when they were a transport command, which is sent.
    /// Only Windows uses this path (jump-list tasks), as in the Electron shell.
    pub fn handle_argv<S: AsRef<str>>(&self, argv: &[S]) -> bool {
        if !cfg!(windows) {
            return false;
        }
        let Some(cmd) = parse_cmd_arg(argv) else {
            return false;
        };
        let _ = self.commands.send(cmd.into());
        true
    }

    /// Whether Rustet starts at login; `None` where this is not supported.
    pub fn open_at_login(&self) -> Option<bool> {
        None
    }

    /// Turns starting at login on or off and returns the new state; `None` where unsupported.
    pub fn set_open_at_login(&mut self, _on: bool) -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_arg_accepts_only_transport_commands() {
        assert_eq!(
            parse_cmd_arg(&["app.exe", "--rustet-cmd=playpause"]),
            Some(Transport::PlayPause)
        );
        assert_eq!(
            parse_cmd_arg(&["--hidden", "--rustet-cmd=next"]),
            Some(Transport::Next)
        );
        assert_eq!(parse_cmd_arg(&["--rustet-cmd=rm -rf"]), None);
        assert_eq!(parse_cmd_arg(&["--rustet-cmd=evil"]), None);
        assert_eq!(parse_cmd_arg(&["--rustet-cmd=Next"]), None);
        assert_eq!(parse_cmd_arg::<&str>(&[]), None);
    }
}
