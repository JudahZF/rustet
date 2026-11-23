//! This install's device identity for the FairPlay session.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use crate::logout_files::is_hex;
use crate::session::{create_private_dir, write_private};

/// The native library's default 9-field device string, without its last field. Only that
/// field, the Android ID, identifies the device to Apple; it used to be the same for every
/// install.
const DEVICE_INFO_PREFIX: &str = "Music/4.9/Android/10/Samsung S9/7663313/en-US/en-US/";
pub(crate) const LEGACY_ANDROID_ID: &str = "dc28071e981c439e";
pub(crate) const DEVICE_ID_FILE: &str = "device-id";

/// This install's device string for the FairPlay session.
///
/// The Android ID is generated once (64 random bits, like a real Android ID) and kept in
/// `base_dir/device-id`. Apple ties a saved login to the ID it was created with, so an install
/// that already has a session keeps the old shared ID until "Forget This Device" removes the
/// file, instead of being signed out by the upgrade.
pub fn device_info(base_dir: &Path) -> io::Result<String> {
    Ok(format!("{DEVICE_INFO_PREFIX}{}", android_id(base_dir)?))
}

fn android_id(base_dir: &Path) -> io::Result<String> {
    let path = base_dir.join(DEVICE_ID_FILE);
    if let Ok(s) = fs::read_to_string(&path)
        && is_hex(s.trim(), 16, false)
    {
        return Ok(s.trim().to_owned());
    }
    let id = if fs::metadata(base_dir.join("session-ready")).is_ok() {
        LEGACY_ANDROID_ID.to_owned()
    } else {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).map_err(io::Error::other)?;
        b.iter().fold(String::with_capacity(16), |mut s, byte| {
            let _ = write!(s, "{byte:02x}");
            s
        })
    };
    create_private_dir(base_dir)?;
    let tmp = base_dir.join(format!("{DEVICE_ID_FILE}.tmp"));
    write_private(&tmp, format!("{id}\n").as_bytes())?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn android_id_of(dir: &Path) -> String {
        let info = device_info(dir).unwrap();
        assert!(
            info.starts_with(DEVICE_INFO_PREFIX) && info.matches('/').count() == 8,
            "{info:?} is not 9 fields"
        );
        info[DEVICE_INFO_PREFIX.len()..].to_owned()
    }

    #[test]
    fn a_new_install_gets_its_own_stable_id() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (id_a, id_b) = (android_id_of(a.path()), android_id_of(b.path()));
        assert!(
            is_hex(&id_a, 16, false) && id_a != LEGACY_ANDROID_ID,
            "{id_a:?}"
        );
        assert_ne!(id_a, id_b, "two installs got the same device ID");
        assert_eq!(
            android_id_of(a.path()),
            id_a,
            "the device ID changed between starts"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(a.path().join(DEVICE_ID_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    // Apple ties a saved login to its device ID.
    #[test]
    fn an_existing_session_keeps_the_shared_id() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("session-ready"), "1").unwrap();
        assert_eq!(android_id_of(dir.path()), LEGACY_ANDROID_ID);
    }

    #[test]
    fn a_corrupt_id_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(DEVICE_ID_FILE), "not-an-id\n").unwrap();
        assert!(is_hex(&android_id_of(dir.path()), 16, false));
    }
}
