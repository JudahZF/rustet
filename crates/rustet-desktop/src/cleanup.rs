//! Keeps the config directory from growing without bound and removes what earlier versions left
//! behind (`config-cleanup.mjs`). Deletion is rule-based and conservative:
//!   * only paths named below are ever removed (never a glob over the whole directory),
//!   * anything live (credentials, prefs, DRM session, identity) is on a protected list,
//!   * stale profile data must have been untouched for [`STALE_DAYS`],
//!   * nothing outside the config directory is touched, and symlinks are removed, not followed.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

const DAY: Duration = Duration::from_secs(24 * 3600);
const MB: u64 = 1024 * 1024;
pub const STALE_DAYS: u32 = 14;

/// Live state. A rule that would touch one of these (or anything inside) is dropped.
pub const PROTECTED: [&str; 11] = [
    "rustet-store.enc",
    "electron-prefs.json",
    "eq.json",
    "mv-dec.key",
    "hrtf.wav",
    "rustetplay",
    "drm/files",
    // Apple's FairPlay libraries (`drm/rootfs/system/lib64`). The Electron app listed this
    // directory as obsolete and so deleted the libraries it had downloaded.
    "drm/rootfs",
    "engine-data",
    "electron-session",
    "cef-session",
];

/// Written by the hybris/Android-wrapper era or by earlier app names; nothing reads them now.
pub const OBSOLETE: [&str; 6] = [
    "wrapper",
    "wrapper-data",
    "webview-data",
    "drm/hybris-linker",
    "drm/drm-rootless",
    "drm/drm-native",
];

/// Chromium profile entries at the config root from before the profile moved to
/// `electron-session/`. Removed only once untouched for [`STALE_DAYS`].
pub const STALE_ROOT_PROFILE: [&str; 27] = [
    "Cache",
    "Code Cache",
    "GPUCache",
    "GPUPersistentCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "Crashpad",
    "Dictionaries",
    "DIPS",
    "DIPS-wal",
    "DIPS-shm",
    "Local Storage",
    "Session Storage",
    "Network Persistent State",
    "Partitions",
    "Preferences",
    "Local State",
    "Shared Dictionary",
    "SharedStorage",
    "SharedStorage-wal",
    "Trust Tokens",
    "Trust Tokens-journal",
    "blob_storage",
    "DevToolsActivePort",
    "SingletonLock",
    "SingletonCookie",
    "SingletonSocket",
];

/// Regenerable caches inside the live profile, capped. Over the cap they are deleted at startup,
/// before Chromium opens them, and rebuilt on demand.
pub const CACHE_CAPS: [(&str, u64); 6] = [
    ("electron-session/Partitions/apple-music/Cache", 400 * MB),
    (
        "electron-session/Partitions/apple-music/Code Cache",
        100 * MB,
    ),
    ("electron-session/Cache", 100 * MB),
    ("electron-session/Code Cache", 50 * MB),
    // The CEF shell's profile, the same budget as Electron's Apple Music partition. On Linux
    // Chromium keeps these caches in the cache directory instead ([`CACHE_ROOT_CAPS`]).
    ("cef-session/Default/Cache", 400 * MB),
    ("cef-session/Default/Code Cache", 100 * MB),
];

/// The CEF profile's caches where Chromium puts them on Linux: under the per-user cache
/// directory, at the profile's path relative to the config directory.
pub const CACHE_ROOT_CAPS: [(&str, u64); 2] = [
    ("cef-session/Default/Cache", 400 * MB),
    ("cef-session/Default/Code Cache", 100 * MB),
];

/// Files in the per-user cache directory that no current code reads: the plaintext library
/// cache the encrypted one replaced, WebKit-era caches, research databases written there by
/// mistake.
pub const OBSOLETE_CACHE: [&str; 5] = [
    "library.json",
    "WebKitCache",
    "CacheStorage",
    "apk-graph.db",
    "win-graph.db",
];

/// Crashed atomic writes leave these behind; they are never read again.
const CHROMIUM_TEMP_PREFIX: &str = ".org.chromium.Chromium.";
const TEMP_MAX_AGE: Duration = DAY;
const CRASH_DUMP_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const STAMP_NAME: &str = ".cleanup-stamp";

/// Total size and newest modification time of a file or tree (symlinks are not followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measure {
    pub bytes: u64,
    pub newest: SystemTime,
}

pub fn measure(p: &Path) -> Option<Measure> {
    let st = fs::symlink_metadata(p).ok()?;
    let mut m = Measure {
        bytes: if st.is_file() { st.len() } else { 0 },
        newest: st.modified().unwrap_or(UNIX_EPOCH),
    };
    if st.is_dir()
        && let Ok(entries) = fs::read_dir(p)
    {
        for child in entries.flatten().filter_map(|e| measure(&e.path())) {
            m.bytes += child.bytes;
            m.newest = m.newest.max(child.newest);
        }
    }
    Some(m)
}

fn age(now: SystemTime, t: SystemTime) -> Duration {
    now.duration_since(t).unwrap_or_default()
}

/// Whether `p` lies strictly inside `root`.
fn inside(root: &Path, p: &Path) -> bool {
    p.strip_prefix(root).is_ok_and(|rel| {
        rel.components().next().is_some()
            && rel.components().all(|c| matches!(c, Component::Normal(_)))
    })
}

/// One path the cleanup removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub path: PathBuf,
    pub reason: String,
    pub bytes: u64,
}

/// What a cleanup run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed: usize,
    pub bytes: u64,
    pub errors: usize,
    pub plan: Vec<Removal>,
}

/// Decides what to delete. Only reads the filesystem.
pub fn plan_cleanup(root: &Path, now: SystemTime) -> Vec<Removal> {
    let mut plan = Vec::new();
    let protected: Vec<PathBuf> = PROTECTED.iter().map(|r| root.join(r)).collect();
    let mut add = |rel: &str, reason: String, allow_inside_protected: bool| {
        let abs = root.join(rel);
        let Some(m) = (inside(root, &abs)).then(|| measure(&abs)).flatten() else {
            return;
        };
        // Protected entries are never deleted; a cache cap may sit inside electron-session.
        let hits = |pr: &PathBuf| {
            if allow_inside_protected {
                abs == *pr
            } else {
                abs.starts_with(pr) || pr.starts_with(&abs)
            }
        };
        if !protected.iter().any(hits) {
            plan.push(Removal {
                path: abs,
                reason,
                bytes: m.bytes,
            });
        }
    };

    for rel in OBSOLETE {
        add(rel, "obsolete".into(), false);
    }
    for rel in STALE_ROOT_PROFILE {
        if let Some(m) = measure(&root.join(rel)) {
            let untouched = age(now, m.newest);
            if untouched >= DAY * STALE_DAYS {
                add(
                    rel,
                    format!(
                        "stale profile data (untouched {}d)",
                        untouched.as_secs() / DAY.as_secs()
                    ),
                    false,
                );
            }
        }
    }
    for (rel, max) in CACHE_CAPS {
        if let Some(m) = measure(&root.join(rel))
            && m.bytes > max
        {
            add(
                rel,
                format!(
                    "cache over cap ({}MB > {}MB)",
                    (m.bytes as f64 / MB as f64).round(),
                    max / MB
                ),
                true,
            );
        }
    }

    // Both browser profiles: Electron's (kept for a downgrade) and the CEF shell's.
    let profiles = ["electron-session", "cef-session"].map(|p| root.join(p));

    // Leftover temp files from interrupted writes, in the profile root and each partition.
    let mut temp_dirs = profiles.to_vec();
    if let Ok(parts) = fs::read_dir(profiles[0].join("Partitions")) {
        temp_dirs.extend(parts.flatten().map(|e| e.path()));
    }
    for dir in temp_dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            if !e
                .file_name()
                .to_string_lossy()
                .starts_with(CHROMIUM_TEMP_PREFIX)
            {
                continue;
            }
            if let Ok(st) = fs::symlink_metadata(e.path())
                && age(now, st.modified().unwrap_or(UNIX_EPOCH)) >= TEMP_MAX_AGE
            {
                plan.push(Removal {
                    path: e.path(),
                    reason: "leftover temp file".into(),
                    bytes: st.len(),
                });
            }
        }
    }

    // Old crash dumps.
    for entries in profiles
        .iter()
        .filter_map(|p| fs::read_dir(p.join("Crashpad/completed")).ok())
    {
        for e in entries.flatten() {
            if let Ok(st) = fs::symlink_metadata(e.path())
                && age(now, st.modified().unwrap_or(UNIX_EPOCH)) >= CRASH_DUMP_MAX_AGE
            {
                plan.push(Removal {
                    path: e.path(),
                    reason: "old crash dump".into(),
                    bytes: st.len(),
                });
            }
        }
    }
    plan
}

/// Removes a file, a symlink (not its target) or a whole tree.
fn remove(p: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(p) {
        Ok(st) if st.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn execute(root: &Path, plan: Vec<Removal>, dry_run: bool, label: &str) -> CleanupReport {
    let mut report = CleanupReport::default();
    for item in &plan {
        if !inside(root, &item.path) {
            continue; // defence in depth
        }
        let rel = item.path.strip_prefix(root).unwrap_or(&item.path).display();
        info!(
            "{} {label}{rel} ({}KB): {}",
            if dry_run { "would remove" } else { "removing" },
            item.bytes / 1024,
            item.reason
        );
        if dry_run {
            report.removed += 1;
            report.bytes += item.bytes;
            continue;
        }
        match remove(&item.path) {
            Ok(()) => {
                report.removed += 1;
                report.bytes += item.bytes;
            }
            Err(e) => {
                report.errors += 1;
                warn!("could not remove {}: {e}", item.path.display());
            }
        }
    }
    report.plan = plan;
    report
}

/// Runs the plan for the config directory `root`.
pub fn run_cleanup(root: &Path, now: SystemTime, dry_run: bool) -> CleanupReport {
    execute(root, plan_cleanup(root, now), dry_run, "")
}

/// Runs at most once per `every`, so startup does not walk large cache trees every launch. The
/// time of the last run is kept in `<root>/.cleanup-stamp` (milliseconds since the epoch).
pub fn run_cleanup_throttled(
    root: &Path,
    now: SystemTime,
    every: Duration,
) -> Option<CleanupReport> {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64;
    let last = fs::read_to_string(root.join(STAMP_NAME))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok());
    if last.is_some_and(|last| now_ms - last < every.as_millis() as f64) {
        return None;
    }
    let report = run_cleanup(root, now, false);
    if fs::create_dir_all(root).is_ok() {
        let _ = fs::write(root.join(STAMP_NAME), format!("{now_ms}"));
    }
    Some(report)
}

/// [`run_cleanup_throttled`] with the daily interval.
pub const CLEANUP_INTERVAL: Duration = DAY;

/// Removes the [`OBSOLETE_CACHE`] entries from the per-user cache directory `cache_root`, and
/// the browser caches there that are over their [`CACHE_ROOT_CAPS`].
pub fn cleanup_cache(cache_root: &Path, dry_run: bool) -> CleanupReport {
    let obsolete = OBSOLETE_CACHE.iter().filter_map(|rel| {
        let abs = cache_root.join(rel);
        let m = measure(&abs)?;
        Some(Removal {
            path: abs,
            reason: "obsolete".into(),
            bytes: m.bytes,
        })
    });
    let over_cap = CACHE_ROOT_CAPS.iter().filter_map(|&(rel, max)| {
        let abs = cache_root.join(rel);
        let m = measure(&abs).filter(|m| m.bytes > max)?;
        Some(Removal {
            path: abs,
            reason: format!("cache over cap ({}MB > {}MB)", m.bytes / MB, max / MB),
            bytes: m.bytes,
        })
    });
    execute(
        cache_root,
        obsolete.chain(over_cap).collect(),
        dry_run,
        "cache/",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_791_504_000) // 2026-10-09
    }

    /// Writes `rel` with `size` zero bytes, `age_days` old, and ages every ancestor below `root`.
    fn file(root: &Path, rel: &str, size: usize, age_days: u64) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![0u8; size]).unwrap();
        let t = now() - DAY * age_days as u32;
        let mut d = p.clone();
        while d != root && d.starts_with(root) {
            fs::File::options()
                .read(true)
                .open(&d)
                .and_then(|f| f.set_modified(t))
                .ok();
            d = d.parent().unwrap().to_path_buf();
        }
        p
    }

    fn exists(root: &Path, rel: &str) -> bool {
        fs::symlink_metadata(root.join(rel)).is_ok()
    }

    const LIVE: [&str; 12] = [
        "rustet-store.enc",
        "electron-prefs.json",
        "eq.json",
        "mv-dec.key",
        "rustetplay/device_private_key",
        "rustetplay/device_client_id_blob",
        "drm/files/MUSIC_TOKEN",
        "drm/files/mpl_db/x",
        "engine-data/config.yaml",
        "electron-session/Partitions/apple-music/Cookies",
        "cef-session/Default/Cookies",
        "cef-session/Default/Local Storage/leveldb/000003.log",
    ];

    #[test]
    fn live_state_is_never_removed() {
        let r = tempfile::tempdir().unwrap();
        for rel in LIVE {
            file(r.path(), rel, 10, 400);
        }
        run_cleanup(r.path(), now(), false);
        for rel in LIVE {
            assert!(exists(r.path(), rel), "{rel}");
        }
    }

    #[test]
    fn obsolete_wrapper_era_directories_are_removed() {
        let r = tempfile::tempdir().unwrap();
        for rel in [
            "wrapper/rootfs/data/x",
            "wrapper-data/rootfs/y",
            "webview-data/z",
            "drm/hybris-linker/b",
            "drm/files/MUSIC_TOKEN",
            "drm/rootfs/system/lib64/libCoreFP.so",
        ] {
            file(r.path(), rel, 10, 0);
        }
        let res = run_cleanup(r.path(), now(), false);
        for rel in [
            "wrapper",
            "wrapper-data",
            "webview-data",
            "drm/hybris-linker",
        ] {
            assert!(!exists(r.path(), rel), "{rel}");
        }
        assert!(exists(r.path(), "drm/files/MUSIC_TOKEN"));
        assert!(
            exists(r.path(), "drm/rootfs/system/lib64/libCoreFP.so"),
            "the FairPlay libraries stay"
        );
        assert_eq!(res.errors, 0);
    }

    #[test]
    fn stale_root_profile_data_goes_only_when_untouched_long_enough() {
        let r = tempfile::tempdir().unwrap();
        file(
            r.path(),
            "Partitions/apple-music/Cache/f",
            10,
            u64::from(STALE_DAYS) + 1,
        );
        file(r.path(), "GPUCache/g", 10, u64::from(STALE_DAYS) - 1);
        file(r.path(), "Local State", 10, u64::from(STALE_DAYS) + 30);
        run_cleanup(r.path(), now(), false);
        assert!(!exists(r.path(), "Partitions"));
        assert!(!exists(r.path(), "Local State"));
        assert!(exists(r.path(), "GPUCache"), "recently used data stays");
    }

    #[test]
    fn a_directory_with_any_recent_file_is_not_stale() {
        let r = tempfile::tempdir().unwrap();
        file(r.path(), "Partitions/old", 10, 100);
        file(r.path(), "Partitions/new", 10, 1);
        run_cleanup(r.path(), now(), false);
        assert!(exists(r.path(), "Partitions/old"));
    }

    #[test]
    fn oversized_caches_are_cleared_in_budget_ones_kept() {
        let r = tempfile::tempdir().unwrap();
        let big = file(
            r.path(),
            "electron-session/Partitions/apple-music/Cache/big",
            0,
            0,
        );
        fs::File::options()
            .write(true)
            .open(&big)
            .unwrap()
            .set_len(401 * MB)
            .unwrap(); // sparse
        file(
            r.path(),
            "electron-session/Partitions/apple-music/Code Cache/small",
            1024,
            0,
        );
        run_cleanup(r.path(), now(), false);
        assert!(!exists(
            r.path(),
            "electron-session/Partitions/apple-music/Cache"
        ));
        assert!(exists(
            r.path(),
            "electron-session/Partitions/apple-music/Code Cache/small"
        ));
        assert!(
            exists(r.path(), "electron-session"),
            "the profile itself is protected"
        );
    }

    /// A sparse file of `len` bytes.
    fn sized(root: &Path, rel: &str, len: u64) {
        let f = file(root, rel, 0, 0);
        fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_len(len)
            .unwrap();
    }

    #[test]
    fn the_cef_profile_cache_is_capped_and_the_profile_kept() {
        // Config directory (the cache's place on Windows).
        let r = tempfile::tempdir().unwrap();
        sized(
            r.path(),
            "cef-session/Default/Cache/Cache_Data/big",
            401 * MB,
        );
        file(r.path(), "cef-session/Default/Code Cache/js/small", 1024, 0);
        file(r.path(), "cef-session/Default/Cookies", 10, 400);
        run_cleanup(r.path(), now(), false);
        assert!(!exists(r.path(), "cef-session/Default/Cache"));
        assert!(exists(r.path(), "cef-session/Default/Code Cache/js/small"));
        assert!(exists(r.path(), "cef-session/Default/Cookies"));

        // Cache directory (Linux: Chromium keeps the profile's caches there).
        let c = tempfile::tempdir().unwrap();
        sized(c.path(), "cef-session/Default/Code Cache/js/big", 101 * MB);
        file(
            c.path(),
            "cef-session/Default/Cache/Cache_Data/small",
            1024,
            0,
        );
        assert_eq!(cleanup_cache(c.path(), false).removed, 1);
        assert!(!exists(c.path(), "cef-session/Default/Code Cache"));
        assert!(exists(
            c.path(),
            "cef-session/Default/Cache/Cache_Data/small"
        ));
    }

    #[test]
    fn temp_files_and_old_crash_dumps() {
        let r = tempfile::tempdir().unwrap();
        let base = "electron-session/Partitions/apple-music/";
        file(
            r.path(),
            &format!("{base}.org.chromium.Chromium.old"),
            10,
            3,
        );
        file(
            r.path(),
            &format!("{base}.org.chromium.Chromium.fresh"),
            10,
            0,
        );
        file(r.path(), &format!("{base}Cookies"), 10, 0);
        file(
            r.path(),
            "electron-session/Crashpad/completed/old.dmp",
            10,
            30,
        );
        file(
            r.path(),
            "electron-session/Crashpad/completed/new.dmp",
            10,
            1,
        );
        run_cleanup(r.path(), now(), false);
        assert!(!exists(
            r.path(),
            &format!("{base}.org.chromium.Chromium.old")
        ));
        assert!(exists(
            r.path(),
            &format!("{base}.org.chromium.Chromium.fresh")
        ));
        assert!(exists(r.path(), &format!("{base}Cookies")));
        assert!(!exists(
            r.path(),
            "electron-session/Crashpad/completed/old.dmp"
        ));
        assert!(exists(
            r.path(),
            "electron-session/Crashpad/completed/new.dmp"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_removed_never_followed() {
        let r = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        file(outside.path(), "keep.txt", 10, 0);
        std::os::unix::fs::symlink(outside.path(), r.path().join("wrapper")).unwrap();
        run_cleanup(r.path(), now(), false);
        assert!(!exists(r.path(), "wrapper"));
        assert!(
            outside.path().join("keep.txt").exists(),
            "the target outside survives"
        );
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let r = tempfile::tempdir().unwrap();
        file(r.path(), "wrapper/x", 2048, 0);
        assert_eq!(run_cleanup(r.path(), now(), true).removed, 1);
        assert!(exists(r.path(), "wrapper/x"));
    }

    #[test]
    fn throttled_runs_at_most_once_per_interval() {
        let r = tempfile::tempdir().unwrap();
        file(r.path(), "wrapper/x", 10, 0);
        assert!(run_cleanup_throttled(r.path(), now(), CLEANUP_INTERVAL).is_some());
        file(r.path(), "wrapper/again", 10, 0);
        assert!(
            run_cleanup_throttled(r.path(), now() + Duration::from_secs(1), CLEANUP_INTERVAL)
                .is_none()
        );
        assert!(exists(r.path(), "wrapper/again"));
        assert!(run_cleanup_throttled(r.path(), now() + DAY * 2, CLEANUP_INTERVAL).is_some());
        assert!(!exists(r.path(), "wrapper"));
    }

    #[test]
    fn no_rule_reaches_a_protected_path() {
        let r = tempfile::tempdir().unwrap();
        for p in PROTECTED {
            file(
                r.path(),
                if p == "electron-session" {
                    "electron-session/keep"
                } else {
                    p
                },
                10,
                999,
            );
        }
        for item in plan_cleanup(r.path(), now()) {
            for p in PROTECTED {
                assert_ne!(item.path, r.path().join(p), "{p} is planned for deletion");
            }
        }
    }

    #[test]
    fn cache_cleanup_removes_only_the_listed_entries() {
        let r = tempfile::tempdir().unwrap();
        for rel in [
            "library.json",
            "WebKitCache/a/b",
            "apk-graph.db",
            "library.enc",
            "library.key",
            "playback/1-alac.m4a",
            "autoeq/index.json",
        ] {
            file(r.path(), rel, 10, 0);
        }
        assert_eq!(cleanup_cache(r.path(), false).removed, 3);
        for keep in [
            "library.enc",
            "library.key",
            "playback/1-alac.m4a",
            "autoeq/index.json",
        ] {
            assert!(exists(r.path(), keep), "{keep}");
        }
        for gone in ["library.json", "WebKitCache", "apk-graph.db"] {
            assert!(!exists(r.path(), gone), "{gone}");
        }
    }
}
