//! Apple's FairPlay libraries for lossless playback, which cannot ship with Rustet
//! (`ensureFairPlayLibs`, `fairplay-fetch.mjs`). They come from the sources pinned in
//! `fairplay-libs.lock`, each checked against its SHA-256.
//!
//! Linux runs `scripts/fetch-fairplay-libs.sh` (installed next to the app, with the lock file and
//! the stubs beside it). Windows has no shell: [`fetch_fairplay_libs`] does the same download in
//! process. The engine only loads the libraries at startup, so the shell reloads it once they
//! arrive (right away when nothing plays, otherwise at the next pause).

use std::future::Future;
use std::io;
use std::path::Path;
use std::process::Stdio;

use sha2::{Digest, Sha256};
use tokio::io::AsyncBufReadExt;
use tracing::{error, info, warn};

/// Our stand-ins for Android system libraries, installed with Apple's.
const STUBS: [&str; 3] = ["libandroid.so", "libOpenSLES.so", "libdl.so"];
/// The largest library is about 14 MiB.
pub const MAX_LIB_BYTES: usize = 64 * 1024 * 1024;

/// The outcome of [`ensure_fairplay_libs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// Already installed: nothing to do.
    Present,
    /// No fetch script next to the app (a run from source): nothing to do.
    NoScript,
    /// Downloaded and verified: reload the engine.
    Installed,
    /// The download failed; lossless stays off.
    Failed,
}

/// Linux: runs `sh <script> --dest <lib64> --quiet` unless every library in the lock beside
/// the script (`fairplay-libs.lock`) and every stub is in `lib64`. The script's error output
/// goes to the log.
pub async fn ensure_fairplay_libs(lib64: &Path, script: &Path) -> FetchOutcome {
    if !script.exists() {
        return FetchOutcome::NoScript;
    }
    if libs_installed(lib64, &script.with_file_name("fairplay-libs.lock")) {
        return FetchOutcome::Present;
    }
    info!("downloading the FairPlay libraries for lossless playback");
    let child = tokio::process::Command::new("sh")
        .arg(script)
        .arg("--dest")
        .arg(lib64)
        .arg("--quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            error!("FairPlay download could not start: {e}");
            return FetchOutcome::Failed;
        }
    };
    if let Some(stderr) = child.stderr.take() {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            warn!("fairplay: {}", line.trim());
        }
    }
    match child.wait().await {
        Ok(s) if s.success() => {
            info!("FairPlay libraries ready; enabling lossless");
            FetchOutcome::Installed
        }
        _ => {
            error!("FairPlay libraries unavailable; lossless stays off");
            FetchOutcome::Failed
        }
    }
}

/// Whether every file the lock pins, and every stub, is in `lib64`. One file is not enough: a
/// download that stops partway leaves the first libraries without their dependencies. The
/// script moves a file in only after its checksum matched, so a present file is complete.
fn libs_installed(lib64: &Path, lock_path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(lock_path) else {
        return false;
    };
    let lock = parse_lock(&text);
    !lock.entries.is_empty()
        && lock
            .entries
            .iter()
            .map(|(_, name)| name.as_str())
            .chain(STUBS)
            .all(|name| lib64.join(name).exists())
}

/// `fairplay-libs.lock`: download sources (`source https://…`) and the pinned files
/// (`<sha256>  <name>.so`). Other lines are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    pub sources: Vec<String>,
    pub entries: Vec<(String, String)>,
}

pub fn parse_lock(text: &str) -> Lock {
    let mut lock = Lock::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("source")
            && rest.starts_with(char::is_whitespace)
        {
            let url = rest.trim();
            if url.starts_with("https://") && !url.contains(char::is_whitespace) {
                lock.sources.push(url.trim_end_matches('/').to_owned());
            }
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let hash_ok = hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let name_ok = name.ends_with(".so")
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b));
        if hash_ok && name_ok && line.starts_with(hash) {
            lock.entries.push((hash.to_owned(), name.to_owned()));
        }
    }
    lock
}

/// Downloads one URL (at most [`MAX_LIB_BYTES`]). Tests use a fake.
pub trait Download: Send + Sync {
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
}

impl Download for reqwest::Client {
    async fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let res = reqwest::Client::get(self, url)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status().as_u16()));
        }
        let body = res.bytes().await.map_err(|e| e.to_string())?;
        if body.len() > MAX_LIB_BYTES {
            return Err("file too large".into());
        }
        Ok(body.to_vec())
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What [`fetch_fairplay_libs`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchReport {
    pub fetched: usize,
    /// Files (or `fairplay-libs.lock`) that are not in place.
    pub failed: Vec<String>,
}

impl FetchReport {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// The in-process download (Windows): every pinned file not already matching its checksum is
/// fetched from the first source that serves the right bytes, then the stubs are copied in.
pub async fn fetch_fairplay_libs(
    lock_path: &Path,
    stubs_dir: &Path,
    dest: &Path,
    http: &impl Download,
) -> io::Result<FetchReport> {
    let lock_failed = || FetchReport {
        fetched: 0,
        failed: vec!["fairplay-libs.lock".into()],
    };
    let Ok(text) = tokio::fs::read_to_string(lock_path).await else {
        return Ok(lock_failed());
    };
    let lock = parse_lock(&text);
    if lock.sources.is_empty() || lock.entries.is_empty() {
        return Ok(lock_failed());
    }
    tokio::fs::create_dir_all(dest).await?;
    let mut report = FetchReport::default();
    for (want, name) in &lock.entries {
        let target = dest.join(name);
        if tokio::fs::read(&target)
            .await
            .is_ok_and(|b| sha256_hex(&b) == *want)
        {
            continue;
        }
        let part = dest.join(format!(".{name}.part"));
        let mut ok = false;
        for src in &lock.sources {
            let result = async {
                let body = http.get(&format!("{src}/{name}")).await?;
                if sha256_hex(&body) != *want {
                    return Err("checksum mismatch".to_owned());
                }
                tokio::fs::write(&part, &body)
                    .await
                    .map_err(|e| e.to_string())?;
                tokio::fs::rename(&part, &target)
                    .await
                    .map_err(|e| e.to_string())
            }
            .await;
            match result {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    warn!("{name} from {src} failed: {e}");
                    let _ = tokio::fs::remove_file(&part).await;
                }
            }
        }
        if ok {
            report.fetched += 1;
        } else {
            report.failed.push(name.clone());
        }
    }
    for stub in STUBS {
        let from = stubs_dir.join(stub);
        if from.exists() {
            tokio::fs::copy(&from, dest.join(stub)).await?;
        } else if !dest.join(stub).exists() {
            report.failed.push(stub.into());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn parse_lock_reads_sources_and_entries() {
        let h = "a".repeat(64);
        let r = parse_lock(&format!(
            "# x\nsource https://a/b/\nsource http://insecure/x\n{h}  libz.so\nbad  line.so\n{h} lib/evil.so\n"
        ));
        assert_eq!(r.sources, ["https://a/b"]);
        assert_eq!(r.entries, [(h, "libz.so".to_owned())]);
    }

    struct Fake {
        calls: Mutex<Vec<String>>,
        serve: fn(&str) -> Result<Vec<u8>, String>,
    }

    impl Download for Fake {
        async fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.calls.lock().unwrap().push(url.to_owned());
            (self.serve)(url)
        }
    }

    fn setup(
        content: &[u8],
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("lock");
        std::fs::write(
            &lock,
            format!(
                "# c\nsource https://a.example/x/\nsource https://b.example/y\n{}  libz.so\n",
                sha256_hex(content)
            ),
        )
        .unwrap();
        let (stubs, dest) = (d.path().join("stubs"), d.path().join("lib64"));
        (d, lock, stubs, dest)
    }

    #[tokio::test]
    async fn downloads_verifies_and_falls_back_to_the_second_source() {
        let (_d, lock, stubs, dest) = setup(b"zzz");
        std::fs::create_dir_all(&stubs).unwrap();
        for n in STUBS {
            std::fs::write(stubs.join(n), "stub").unwrap();
        }
        let http = Fake {
            calls: Mutex::default(),
            serve: |url| {
                Ok(if url.starts_with("https://a.example") {
                    b"tampered".to_vec()
                } else {
                    b"zzz".to_vec()
                })
            },
        };
        let r = fetch_fairplay_libs(&lock, &stubs, &dest, &http)
            .await
            .unwrap();
        assert!(r.ok(), "{r:?}");
        assert_eq!(std::fs::read(dest.join("libz.so")).unwrap(), b"zzz");
        assert!(dest.join("libdl.so").exists());
        assert_eq!(
            *http.calls.lock().unwrap(),
            ["https://a.example/x/libz.so", "https://b.example/y/libz.so"]
        );
        assert!(
            !std::fs::read_dir(&dest).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part"))
        );
        http.calls.lock().unwrap().clear();
        assert_eq!(
            fetch_fairplay_libs(&lock, &stubs, &dest, &http)
                .await
                .unwrap()
                .fetched,
            0,
            "nothing downloaded twice"
        );
        assert!(http.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_matching_source_leaves_no_file() {
        let (_d, lock, stubs, dest) = setup(b"zzz");
        let http = Fake {
            calls: Mutex::default(),
            serve: |_| Ok(b"nope".to_vec()),
        };
        let r = fetch_fairplay_libs(&lock, &stubs, &dest, &http)
            .await
            .unwrap();
        assert!(!r.ok());
        assert!(r.failed.contains(&"libz.so".to_owned()));
        assert!(!dest.join("libz.so").exists());
        let missing = fetch_fairplay_libs(&dest.join("no-lock"), &stubs, &dest, &http)
            .await
            .unwrap();
        assert_eq!(missing.failed, ["fairplay-libs.lock"]);
    }

    #[tokio::test]
    async fn the_script_runs_with_dest_and_quiet() {
        let d = tempfile::tempdir().unwrap();
        let lib64 = d.path().join("lib64");
        let script = d.path().join("fetch.sh");
        assert_eq!(
            ensure_fairplay_libs(&lib64, &script).await,
            FetchOutcome::NoScript
        );
        let h = "a".repeat(64);
        std::fs::write(
            d.path().join("fairplay-libs.lock"),
            format!("{h}  libandroidappmusic.so\n{h}  libCoreFP.so\n"),
        )
        .unwrap();
        // The fake script installs the set where --dest points, if called as the shell does.
        std::fs::write(&script, "[ \"$1\" = --dest ] && [ \"$3\" = --quiet ] || exit 2\nmkdir -p \"$2\" && cd \"$2\" && touch libandroidappmusic.so libCoreFP.so libandroid.so libOpenSLES.so libdl.so\n").unwrap();
        assert_eq!(
            ensure_fairplay_libs(&lib64, &script).await,
            FetchOutcome::Installed
        );
        assert_eq!(
            ensure_fairplay_libs(&lib64, &script).await,
            FetchOutcome::Present
        );
        // A download that stopped partway: the first library is there, a dependency is not.
        std::fs::remove_file(lib64.join("libCoreFP.so")).unwrap();
        assert_eq!(
            ensure_fairplay_libs(&lib64, &script).await,
            FetchOutcome::Installed
        );
        std::fs::write(&script, "echo broken >&2; exit 1\n").unwrap();
        assert_eq!(
            ensure_fairplay_libs(&d.path().join("other"), &script).await,
            FetchOutcome::Failed
        );
    }
}
