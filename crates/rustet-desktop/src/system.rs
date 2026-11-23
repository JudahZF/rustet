//! Small services of the shell: external commands, the power profile, debug-log pruning, the
//! navigation allow-list, EQ and download-folder file helpers.

use std::fs;
use std::future::Future;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::prefs::js_truthy;
use crate::theme::{safe_name, valid_str};

/// Runs an external command and returns its standard output (`execFile`). `None` when it could
/// not start, failed, or timed out. Tests use a fake.
pub trait CommandRunner: Send + Sync {
    fn run(&self, cmd: &str, args: &[&str]) -> impl Future<Output = Option<String>> + Send;
}

/// The real [`CommandRunner`]: a child process with a time limit.
#[derive(Debug, Clone, Copy)]
pub struct SystemRunner {
    pub timeout: Duration,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
        }
    }
}

impl CommandRunner for SystemRunner {
    async fn run(&self, cmd: &str, args: &[&str]) -> Option<String> {
        let child = tokio::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(self.timeout, child).await.ok()?.ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// `system:powerProfile`: the ACPI platform profile (`performance`, `balanced`, `quiet`, …) from
/// `<sysfs>/firmware/acpi/platform_profile`, or `None`. `sysfs` is `/sys` outside tests.
pub async fn power_profile(sysfs: &Path) -> Option<String> {
    let raw = tokio::fs::read_to_string(sysfs.join("firmware/acpi/platform_profile"))
        .await
        .ok()?;
    Some(raw.trim().to_owned()).filter(|s| !s.is_empty())
}

/// Debug logs kept in the logs directory.
pub const DEBUG_LOGS_KEPT: usize = 10;

/// Deletes all but the newest [`DEBUG_LOGS_KEPT`] `debug-*.log` files (names carry an ISO
/// timestamp, so name order is age order).
pub fn prune_debug_logs(logs_dir: &Path) {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("debug-") && n.ends_with(".log"))
        .collect();
    names.sort_unstable_by(|a, b| b.cmp(a));
    for old in names.iter().skip(DEBUG_LOGS_KEPT) {
        let _ = fs::remove_file(logs_dir.join(old));
    }
}

/// The host of an `https:` URL. Origin decisions compare parsed hosts: substring tests also
/// accept `music.apple.com.attacker.example`.
pub fn https_host(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    (u.scheme() == "https")
        .then(|| u.host_str().map(str::to_owned))
        .flatten()
}

/// May the privileged window navigate to this URL itself? Apple's own hosts only.
pub fn navigation_allowed(url: &str) -> bool {
    https_host(url).is_some_and(|h| h == "apple.com" || h.ends_with(".apple.com"))
}

/// `app:open-external`: only `https://` links go to the system browser.
pub fn external_url_allowed(url: &Value) -> bool {
    url.as_str().is_some_and(|u| u.starts_with("https://"))
}

/// The engine confines exports to the home folder; a folder the user picked in the dialog is
/// their own choice, so it is recorded (the last 8) in the file the engine reads
/// (`RUSTET_EXPORT_PICKED`).
pub fn record_export_folder(file: &Path, folder: &Path) -> io::Result<()> {
    let folder = folder.to_string_lossy();
    let current = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<&str> = current.split('\n').filter(|l| !l.is_empty()).collect();
    if lines.contains(&folder.as_ref()) {
        return Ok(());
    }
    lines.push(&folder);
    let keep = &lines[lines.len().saturating_sub(8)..];
    fs::write(file, keep.join("\n") + "\n")
}

// ── EQ files ─────────────────────────────────────────────────────────────────

/// Largest EQ file the page may save.
pub const EQ_FILE_MAX: usize = 5 * 1024 * 1024;

/// A validated `eq:save-file` request: what to suggest in the save dialog and what to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqSave {
    pub default_file_name: String,
    pub content: String,
}

impl EqSave {
    /// `{ filename, content }` from the page. The content is a string of at most 5 MiB; the
    /// name is reduced to a safe base name (default `eq.txt`).
    pub fn parse(arg: &Value) -> Option<Self> {
        let content = arg
            .get("content")?
            .as_str()
            .filter(|c| c.encode_utf16().count() <= EQ_FILE_MAX)?;
        let name = match arg.get("filename") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(v) if js_truthy(v) => v.to_string(),
            _ => "eq.txt".to_owned(),
        };
        let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
        Some(Self {
            default_file_name: safe_name(base),
            content: content.to_owned(),
        })
    }
}

/// A file-dialog filter.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FileFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

impl FileFilter {
    fn new(name: &str, extensions: &[&str]) -> Self {
        Self {
            name: name.into(),
            extensions: extensions.iter().map(|e| (*e).into()).collect(),
        }
    }
}

/// The filters the save dialog offers for `eq:save-file`.
pub fn eq_save_filters() -> Vec<FileFilter> {
    vec![
        FileFilter::new("EqualizerAPO config", &["txt"]),
        FileFilter::new("JSON", &["json"]),
        FileFilter::new("All files", &["*"]),
    ]
}

/// The filters for `eq:open-file`: the page's own when valid (at most 8, names of 1–64
/// characters, at most 16 extensions of `[A-Za-z0-9*]{1,10}` each), else the defaults.
pub fn eq_open_filters(arg: &Value) -> Vec<FileFilter> {
    let valid = |f: &Value| {
        valid_str(f.get("name").unwrap_or(&Value::Null), 64).is_some()
            && f.get("extensions")
                .and_then(Value::as_array)
                .is_some_and(|exts| {
                    exts.len() <= 16
                        && exts.iter().all(|e| {
                            e.as_str().is_some_and(|e| {
                                (1..=10).contains(&e.len())
                                    && e.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'*')
                            })
                        })
                })
    };
    arg.get("filters")
        .and_then(Value::as_array)
        .filter(|fs| fs.len() <= 8 && fs.iter().all(valid))
        .and_then(|fs| {
            fs.iter()
                .map(|f| serde_json::from_value(f.clone()).ok())
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                FileFilter::new("EqualizerAPO / text / JSON", &["txt", "json", "csv"]),
                FileFilter::new("All files", &["*"]),
            ]
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn navigation_only_to_apple_hosts() {
        for ok in [
            "https://music.apple.com/us/browse",
            "https://authorize.music.apple.com/x",
            "https://appleid.apple.com/",
            "https://apple.com/",
        ] {
            assert!(navigation_allowed(ok), "{ok}");
        }
        for bad in [
            "https://evil.example/",
            "https://music.apple.com.evil.example/",
            "https://notapple.com/",
            "http://music.apple.com/",
            "https://evilapple.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(!navigation_allowed(bad), "{bad}");
        }
        assert_eq!(
            https_host("https://Music.Apple.com/x").as_deref(),
            Some("music.apple.com")
        );
        assert!(external_url_allowed(&json!("https://x.example")));
        assert!(!external_url_allowed(&json!("file:///x")));
    }

    #[test]
    fn prune_keeps_the_newest_ten() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..13 {
            fs::write(d.path().join(format!("debug-2026-01-{i:02}.log")), "").unwrap();
        }
        fs::write(d.path().join("other.log"), "").unwrap();
        prune_debug_logs(d.path());
        let mut left: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left.len(), 11);
        assert!(!left.contains(&"debug-2026-01-02.log".to_owned()));
        assert!(left.contains(&"debug-2026-01-03.log".to_owned()));
        assert!(left.contains(&"other.log".to_owned()));
    }

    #[tokio::test]
    async fn power_profile_reads_sysfs() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(power_profile(d.path()).await, None);
        fs::create_dir_all(d.path().join("firmware/acpi")).unwrap();
        fs::write(
            d.path().join("firmware/acpi/platform_profile"),
            "balanced\n",
        )
        .unwrap();
        assert_eq!(power_profile(d.path()).await.as_deref(), Some("balanced"));
    }

    #[test]
    fn export_folders_keep_the_last_eight_without_repeats() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("export-folders.txt");
        for i in 0..10 {
            record_export_folder(&f, Path::new(&format!("/m/{i}"))).unwrap();
        }
        record_export_folder(&f, Path::new("/m/9")).unwrap();
        let text = fs::read_to_string(&f).unwrap();
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            (2..10).map(|i| format!("/m/{i}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn eq_save_validation() {
        let s =
            EqSave::parse(&json!({ "filename": "../../x/My EQ.txt", "content": "Preamp: -3 dB" }))
                .unwrap();
        assert_eq!(s.default_file_name, "My EQ.txt");
        assert_eq!(
            EqSave::parse(&json!({ "content": "x" }))
                .unwrap()
                .default_file_name,
            "eq.txt"
        );
        assert_eq!(
            EqSave::parse(&json!({ "filename": "a", "content": 5 })),
            None
        );
        assert_eq!(
            EqSave::parse(&json!({ "content": "x".repeat(EQ_FILE_MAX + 1) })),
            None
        );
        assert_eq!(EqSave::parse(&Value::Null), None);
    }

    #[test]
    fn eq_open_filters_validation() {
        let mine = json!({ "filters": [{ "name": "Text", "extensions": ["txt", "*"] }] });
        assert_eq!(
            eq_open_filters(&mine),
            vec![FileFilter::new("Text", &["txt", "*"])]
        );
        for bad in [
            json!({ "filters": [{ "name": "", "extensions": ["txt"] }] }),
            json!({ "filters": [{ "name": "x", "extensions": ["t.xt"] }] }),
            json!({ "filters": "x" }),
            Value::Null,
        ] {
            assert_eq!(
                eq_open_filters(&bad)[0].name,
                "EqualizerAPO / text / JSON",
                "{bad}"
            );
        }
    }
}
