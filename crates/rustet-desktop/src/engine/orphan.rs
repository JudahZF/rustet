//! Recovering an engine left running by a crashed shell (`recoverOrphanEngine`), and the port
//! check (`isPortFree`).
//!
//! A pid in the session lock file (or a process listening on the port) is not proof of
//! ownership. Only an orphan of this exact executable, data directory and port is stopped, and
//! the lock file is never deleted: a replacement inode would let two engines own the session.

use std::fs;
use std::path::Path;
use std::time::Duration;

use tracing::info;

/// Whether the loopback port accepts a new listener.
pub fn is_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Linux: is `pid` an orphan (parent 1) of this user, running `binary` in `data_dir` with
/// `--api <port>`? Reads `<proc>/<pid>/…`; `proc` is `/proc` outside tests.
#[cfg(unix)]
pub fn is_owned_orphan(proc: &Path, pid: u32, binary: &Path, data_dir: &Path, port: u16) -> bool {
    if pid <= 1 || pid == std::process::id() {
        return false;
    }
    let dir = proc.join(pid.to_string());
    let Ok(status) = fs::read_to_string(dir.join("status")) else {
        return false;
    };
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .map(str::trim)
    };
    if field("PPid:") != Some("1") {
        return false;
    }
    let uid = field("Uid:")
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse::<u32>().ok());
    if uid != Some(rustix::process::getuid().as_raw()) {
        return false;
    }
    if !same_file(&dir.join("exe"), binary) || !same_file(&dir.join("cwd"), data_dir) {
        return false;
    }
    let Ok(cmdline) = fs::read(dir.join("cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let port = port.to_string();
    args.iter()
        .position(|a| *a == b"--api")
        .is_some_and(|i| i > 0 && args.get(i + 1) == Some(&port.as_bytes()))
}

/// What Windows reports about a process (`Win32_Process`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsProcess {
    pub exe: String,
    pub cmdline: String,
}

/// Windows: is the process at `pid` our engine: image path `binary` (case-insensitive) and
/// `--api <port>` (optionally quoted) on its command line? Querying the process is the Windows
/// port's job; this is the decision.
pub fn is_owned_orphan_windows(
    pid: u32,
    own_pid: u32,
    info: Option<&WindowsProcess>,
    binary: &str,
    port: u16,
) -> bool {
    if pid <= 4 || pid == own_pid {
        return false;
    }
    let Some(info) = info.filter(|i| !i.exe.is_empty() && !i.cmdline.is_empty()) else {
        return false;
    };
    if info.exe.to_lowercase() != binary.to_lowercase() {
        return false;
    }
    let port = port.to_string();
    let mut rest = info.cmdline.as_str();
    while let Some(i) = rest.find("--api") {
        rest = &rest[i + 5..];
        let after = rest.trim_start();
        if after.len() == rest.len() {
            continue; // `--api` must be followed by whitespace
        }
        let after = after.strip_prefix('"').unwrap_or(after);
        if let Some(tail) = after.strip_prefix(port.as_str()) {
            let tail = tail.strip_prefix('"').unwrap_or(tail);
            if tail.is_empty() || tail.starts_with(char::is_whitespace) {
                return true;
            }
        }
    }
    false
}

/// Stops an orphaned engine named by the session lock file: SIGTERM, up to 2 s for it to go,
/// then SIGKILL if it is still ours. Anything else in the lock file is left alone.
#[cfg(unix)]
pub async fn recover_orphan_engine(
    proc: &Path,
    lock_path: &Path,
    binary: &Path,
    data_dir: &Path,
    port: u16,
) {
    use rustix::process::{Pid, Signal, kill_process};
    let Ok(owner) = fs::read_to_string(lock_path) else {
        return;
    };
    let owner = owner.trim();
    if owner.is_empty() || !owner.bytes().all(|b| b.is_ascii_digit()) {
        return;
    }
    let Some(pid) = owner.parse::<i32>().ok().and_then(Pid::from_raw) else {
        return;
    };
    let raw = pid.as_raw_nonzero().get() as u32;
    let owned = || is_owned_orphan(proc, raw, binary, data_dir, port);
    if !owned() {
        return;
    }
    info!("stopping orphan engine (pid {raw})");
    if kill_process(pid, Signal::TERM).is_err() {
        return;
    }
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !owned() {
            return;
        }
    }
    // Recheck ownership right before escalating.
    if owned() {
        let _ = kill_process(pid, Signal::KILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_occupied_port_is_detected() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(!is_port_free(port));
        drop(l);
        assert!(is_port_free(port));
    }

    #[cfg(unix)]
    struct Fixture {
        proc: tempfile::TempDir,
        binary: std::path::PathBuf,
        data: std::path::PathBuf,
        _root: tempfile::TempDir,
    }

    #[cfg(unix)]
    fn fixture(
        parent: u32,
        uid: u32,
        args: &str,
        exe_is_binary: bool,
        cwd_is_data: bool,
    ) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("rustet");
        let other = root.path().join("other");
        let data = root.path().join("engine-data");
        fs::write(&binary, b"").unwrap();
        fs::write(&other, b"").unwrap();
        fs::create_dir(&data).unwrap();
        let proc = tempfile::tempdir().unwrap();
        let p = proc.path().join("123456");
        fs::create_dir(&p).unwrap();
        fs::write(
            p.join("status"),
            format!("Name:\trustet\nPPid:\t{parent}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        fs::write(p.join("cmdline"), args).unwrap();
        std::os::unix::fs::symlink(if exe_is_binary { &binary } else { &other }, p.join("exe"))
            .unwrap();
        std::os::unix::fs::symlink(if cwd_is_data { &data } else { root.path() }, p.join("cwd"))
            .unwrap();
        Fixture {
            proc,
            binary,
            data,
            _root: root,
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_accepts_only_our_exact_orphan() {
        let uid = rustix::process::getuid().as_raw();
        let args = "rustet\0engine\0--api\x0020025\0";
        let check = |f: &Fixture| is_owned_orphan(f.proc.path(), 123456, &f.binary, &f.data, 20025);
        assert!(check(&fixture(1, uid, args, true, true)));
        assert!(
            !check(&fixture(std::process::id(), uid, args, true, true)),
            "an active parent"
        );
        assert!(
            !check(&fixture(1, uid + 1, args, true, true)),
            "another user"
        );
        assert!(
            !check(&fixture(1, uid, args, false, true)),
            "another executable"
        );
        assert!(
            !check(&fixture(1, uid, args, true, false)),
            "another data directory"
        );
        assert!(
            !check(&fixture(
                1,
                uid,
                "rustet\0engine\0--api\x0010000\0",
                true,
                true
            )),
            "another port"
        );
        assert!(
            !check(&fixture(1, uid, "--api\x0020025\0", true, true)),
            "--api first"
        );
        let f = fixture(1, uid, args, true, true);
        assert!(!is_owned_orphan(
            f.proc.path(),
            std::process::id(),
            &f.binary,
            &f.data,
            20025
        ));
        assert!(!is_owned_orphan(
            f.proc.path(),
            0,
            &f.binary,
            &f.data,
            20025
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_leaves_an_active_owner_and_the_lock_file_untouched() {
        use std::os::unix::fs::MetadataExt;
        let d = tempfile::tempdir().unwrap();
        let lock = d.path().join("engine-session.lock");
        fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
        let inode = fs::metadata(&lock).unwrap().ino();
        recover_orphan_engine(
            Path::new("/proc"),
            &lock,
            &std::env::current_exe().unwrap(),
            &std::env::current_dir().unwrap(),
            20025,
        )
        .await;
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            format!("{}\n", std::process::id())
        );
        assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
    }

    #[test]
    fn windows_recovery_accepts_only_our_engine_image_and_port() {
        let bin = r"C:\Program Files\Rustet\rustet.exe";
        let ok = WindowsProcess {
            exe: bin.to_lowercase(),
            cmdline: format!(r#""{bin}" engine --api 20025"#),
        };
        let check =
            |info: Option<&WindowsProcess>, pid| is_owned_orphan_windows(pid, 99, info, bin, 20025);
        assert!(check(Some(&ok), 4242));
        assert!(check(
            Some(&WindowsProcess {
                cmdline: format!(r#""{bin}" engine --api "20025""#),
                ..ok.clone()
            }),
            4242
        ));
        assert!(!check(
            Some(&WindowsProcess {
                exe: r"C:\Windows\notepad.exe".into(),
                ..ok.clone()
            }),
            4242
        ));
        assert!(!check(
            Some(&WindowsProcess {
                cmdline: format!(r#""{bin}" --api 20026"#),
                ..ok.clone()
            }),
            4242
        ));
        assert!(!check(
            Some(&WindowsProcess {
                cmdline: format!(r#""{bin}" --api 200250"#),
                ..ok.clone()
            }),
            4242
        ));
        assert!(!check(None, 4242));
        assert!(!check(Some(&ok), 99), "our own pid");
        assert!(!check(Some(&ok), 4), "system");
    }
}
