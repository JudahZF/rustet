//! Where the OSCrypt `v11` password comes from on Linux: the Secret Service, KWallet, or nowhere
//! (Chromium's `basic_text`), chosen from the desktop as Chromium does
//! (`key_storage_util_linux.cc`).

use std::future::Future;

use crate::platform::Env;

/// Why the keyring password could not be had.
#[derive(Debug, thiserror::Error)]
pub enum KeyringError {
    #[cfg(target_os = "linux")]
    #[error("D-Bus: {0}")]
    DBus(#[from] zbus::Error),
    #[error("the keyring refused: {0}")]
    Refused(&'static str),
    #[error("random number generator: {0}")]
    Random(getrandom::Error),
}

/// A source of the OSCrypt `v11` password. Tests use a fake; nothing in a test may talk to the
/// user's real keyring.
pub trait PasswordSource: Send + Sync {
    /// `Ok(None)` when this desktop keeps no keyring (the store is then plain JSON, as Electron
    /// wrote it). When the keyring has no password yet, one is created, as Chromium does.
    fn password(&self) -> impl Future<Output = Result<Option<String>, KeyringError>> + Send;
}

/// A KWallet daemon generation (D-Bus name `org.kde.kwalletd`, `…5`, `…6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KWalletVersion {
    Four,
    Five,
    Six,
}

/// The password store Chromium picks for a desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxBackend {
    /// GNOME Keyring, KeePassXC or any other `org.freedesktop.secrets` service.
    SecretService,
    KWallet(KWalletVersion),
    /// No keyring: Electron reports encryption unavailable and writes plain JSON.
    Basic,
}

impl LinuxBackend {
    /// Chromium's choice from `XDG_CURRENT_DESKTOP` (then `DESKTOP_SESSION`), `KDE_SESSION_VERSION`.
    /// Desktops it does not know (Hyprland, Sway, i3, …) get no keyring.
    pub fn from_desktop(env: &Env) -> Self {
        let kde = || match env.get("KDE_SESSION_VERSION") {
            Some("5") => Self::KWallet(KWalletVersion::Five),
            Some("6") => Self::KWallet(KWalletVersion::Six),
            _ => Self::KWallet(KWalletVersion::Four),
        };
        if let Some(current) = env.get("XDG_CURRENT_DESKTOP") {
            for token in current.split(':').map(str::trim) {
                match token {
                    "Unity" | "GNOME" | "X-Cinnamon" | "Deepin" | "Pantheon" | "XFCE" | "UKUI"
                    | "LXQt" | "COSMIC" => {
                        return Self::SecretService;
                    }
                    "KDE" => return kde(),
                    _ => {}
                }
            }
        }
        match env.get("DESKTOP_SESSION") {
            Some("gnome" | "mate" | "deepin" | "xfce" | "ukui" | "cinnamon" | "pantheon") => {
                Self::SecretService
            }
            Some("kde4" | "kde-plasma") => Self::KWallet(KWalletVersion::Four),
            Some("kde" | "plasma" | "plasmawayland") => kde(),
            _ => Self::Basic,
        }
    }
}

/// The names Electron may have stored Rustet's password under: its app name is the packaged
/// `productName` (`Rustet`) or the package name (`rustet`) for a run from source. Lookup tries each
/// in turn; a new password is stored under the first.
pub const APP_NAMES: [&str; 2] = ["Rustet", "rustet"];

/// The real OS keyring for a [`LinuxBackend`].
#[derive(Debug, Clone)]
pub struct OsKeyring {
    pub backend: LinuxBackend,
}

impl PasswordSource for OsKeyring {
    async fn password(&self) -> Result<Option<String>, KeyringError> {
        #[cfg(target_os = "linux")]
        {
            match self.backend {
                LinuxBackend::Basic => Ok(None),
                LinuxBackend::SecretService => {
                    super::secret_service::password(&APP_NAMES).await.map(Some)
                }
                LinuxBackend::KWallet(version) => super::kwallet::password(version, APP_NAMES[0])
                    .await
                    .map(Some),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self.backend;
            Ok(None)
        }
    }
}

/// A fresh keyring password, as Chromium makes it: base64 of 16 random bytes.
pub(crate) fn new_password() -> Result<String, KeyringError> {
    use base64::Engine;
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(KeyringError::Random)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(pairs: &[(&str, &str)]) -> LinuxBackend {
        LinuxBackend::from_desktop(&pairs.iter().copied().collect())
    }

    #[test]
    fn desktop_selects_the_backend() {
        assert_eq!(
            backend(&[("XDG_CURRENT_DESKTOP", "GNOME")]),
            LinuxBackend::SecretService
        );
        assert_eq!(
            backend(&[("XDG_CURRENT_DESKTOP", "ubuntu:GNOME")]),
            LinuxBackend::SecretService
        );
        assert_eq!(
            backend(&[("XDG_CURRENT_DESKTOP", "KDE"), ("KDE_SESSION_VERSION", "6")]),
            LinuxBackend::KWallet(KWalletVersion::Six)
        );
        assert_eq!(
            backend(&[("XDG_CURRENT_DESKTOP", "KDE"), ("KDE_SESSION_VERSION", "5")]),
            LinuxBackend::KWallet(KWalletVersion::Five)
        );
        assert_eq!(
            backend(&[("XDG_CURRENT_DESKTOP", "KDE")]),
            LinuxBackend::KWallet(KWalletVersion::Four)
        );
        assert_eq!(
            backend(&[("XDG_CURRENT_DESKTOP", "Hyprland")]),
            LinuxBackend::Basic
        );
        assert_eq!(
            backend(&[("XDG_CURRENT_DESKTOP", "sway")]),
            LinuxBackend::Basic
        );
        assert_eq!(
            backend(&[("DESKTOP_SESSION", "gnome")]),
            LinuxBackend::SecretService
        );
        assert_eq!(backend(&[]), LinuxBackend::Basic);
    }

    #[test]
    fn new_passwords_look_like_chromiums() {
        let p = new_password().unwrap();
        assert_eq!(p.len(), 24);
        assert_ne!(p, new_password().unwrap());
    }
}
