//! External tools (FFmpeg, VLC): request-path validation, trust checks on the binary, PATH
//! lookup, and running a tool as a cancellable child process.
//!
//! The engine finds the bundled FFmpeg on `PATH`: the shell puts its `bin` folder first.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::path;

/// How long a cancelled tool gets to exit after the stop signal before it is killed, and how
/// long its output pipes are drained after it exits.
pub(crate) const TOOL_STOP_GRACE: Duration = Duration::from_secs(2);

/// Keeps this much of a tool's stderr for the error message.
const STDERR_TAIL: usize = 2048;

/// The name rule for a tool path from a request: empty (look up on PATH), a bare tool name, or a
/// clean absolute path whose file name is the tool or `<tool>-<suffix>` (versioned installs).
/// Tool paths arrive in request bodies from a web page, so they must not be able to point the
/// exporter at an arbitrary executable. A pure name check; [`check_tool_executable`] checks the
/// file.
pub(crate) fn valid_tool_path(p: &str, names: &[&str]) -> bool {
    if p.is_empty() {
        return true;
    }
    if p.contains('\0') {
        return false;
    }
    if path::is_abs(p) {
        let slashed: String = if cfg!(windows) {
            p.replace('/', "\\")
        } else {
            p.to_owned()
        };
        if path::clean(p) != slashed {
            return false;
        }
    } else if p.chars().any(path::is_sep) || p.contains('/') {
        return false; // "bin/ffmpeg": neither a bare name nor absolute
    }
    tool_name_ok(path::base(p), names)
}

fn tool_name_ok(base: &str, names: &[&str]) -> bool {
    let lowered;
    let base = if cfg!(windows) {
        lowered = base.to_lowercase();
        lowered.strip_suffix(".exe").unwrap_or(&lowered)
    } else {
        base
    };
    names.iter().any(|n| {
        base == *n
            || base
                .strip_prefix(n)
                .is_some_and(|rest| rest.starts_with('-'))
    })
}

/// Why a tool binary is not trusted.
#[derive(Debug, thiserror::Error)]
pub enum ToolTrustError {
    /// The path (or its symlink target) does not exist or cannot be read.
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("{path} resolves to {resolved}, which is not the expected tool")]
    WrongTarget { path: String, resolved: String },
    #[error("{0} is not an executable file")]
    NotExecutable(String),
    #[error("{0} is writable by other users")]
    Writable(String),
    #[error("{0} is owned by another user")]
    ForeignOwner(String),
    #[error("{0} is in a directory writable by other users")]
    WritableDir(String),
    #[error("{0} is in a directory owned by another user")]
    ForeignDir(String),
}

impl ToolTrustError {
    /// True when the file does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }
}

/// Checks that an absolute tool path is what it claims to be: a regular, executable file (after
/// resolving symlinks, whose target must still carry the tool's name) owned by root or the
/// current user and not writable by anyone else, in a directory nobody else can write to. Bare
/// names are left to the PATH lookup.
pub(crate) fn check_tool_executable(p: &str, names: &[&str]) -> Result<(), ToolTrustError> {
    if p.is_empty() || !path::is_abs(p) {
        return Ok(());
    }
    let io_err = |source| ToolTrustError::Io {
        path: p.to_owned(),
        source,
    };
    let resolved = std::fs::canonicalize(p).map_err(io_err)?;
    let shown = resolved.to_string_lossy().into_owned();
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    if !tool_name_ok(&name, names) {
        return Err(ToolTrustError::WrongTarget {
            path: p.to_owned(),
            resolved: shown,
        });
    }
    check_tool_trust(&resolved)
}

/// Unix: mode bits and owners of the file and its directory. Group-writable counts too: another
/// member of the group could replace the checked file between this check and the exec.
#[cfg(unix)]
pub(crate) fn check_tool_trust(resolved: &Path) -> Result<(), ToolTrustError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let shown = || resolved.to_string_lossy().into_owned();
    let io_err = |source| ToolTrustError::Io {
        path: shown(),
        source,
    };
    let me = rustix::process::geteuid().as_raw();
    let foreign = |uid: u32| uid != 0 && uid != me;

    let info = std::fs::metadata(resolved).map_err(io_err)?;
    let mode = info.permissions().mode();
    if !info.is_file() || mode & 0o111 == 0 {
        return Err(ToolTrustError::NotExecutable(shown()));
    }
    if mode & 0o022 != 0 {
        return Err(ToolTrustError::Writable(shown()));
    }
    if foreign(info.uid()) {
        return Err(ToolTrustError::ForeignOwner(shown()));
    }
    let dir = std::fs::metadata(resolved.parent().unwrap_or(Path::new("/"))).map_err(io_err)?;
    if dir.permissions().mode() & 0o022 != 0 {
        return Err(ToolTrustError::WritableDir(shown()));
    }
    if foreign(dir.uid()) {
        return Err(ToolTrustError::ForeignDir(shown()));
    }
    Ok(())
}

/// Windows has no POSIX mode bits or uid to inspect: require a regular `.exe` file. Directory
/// ACLs are the system's concern.
#[cfg(windows)]
pub(crate) fn check_tool_trust(resolved: &Path) -> Result<(), ToolTrustError> {
    let shown = resolved.to_string_lossy().into_owned();
    let info = std::fs::metadata(resolved).map_err(|source| ToolTrustError::Io {
        path: shown.clone(),
        source,
    })?;
    if !info.is_file() || !shown.to_lowercase().ends_with(".exe") {
        return Err(ToolTrustError::NotExecutable(shown));
    }
    Ok(())
}

/// Finds an executable on PATH (or checks an explicit path), like Go's `exec.LookPath`.
pub(crate) fn look_path(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

/// Why a tool run failed.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("ffmpeg not found; install ffmpeg or set FFmpegPath")]
    FfmpegNotFound,
    #[error("start: {0}")]
    Spawn(#[source] io::Error),
    #[error("{status}{}", fmt_tail(.stderr))]
    Exit { status: ExitStatus, stderr: String },
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn fmt_tail(stderr: &str) -> String {
    match stderr.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(line) => format!(": {}", line.trim()),
        None => String::new(),
    }
}

/// Whether the caller wants the tool's stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stdout {
    Discard,
    Capture,
}

/// Runs a tool to completion and returns its stdout (empty unless captured). The child is tied
/// to `cancel`: cancelling asks it to stop (SIGTERM, so FFmpeg and VLC close their output), and
/// kills it if it has not exited after [`TOOL_STOP_GRACE`]. Dropping the future kills it too.
/// Stderr is always drained (and logged at debug level), so a chatty tool never blocks on a full
/// pipe.
pub(crate) async fn run_tool(
    cancel: &CancellationToken,
    program: impl AsRef<OsStr>,
    args: &[&OsStr],
    stdout: Stdout,
) -> Result<Vec<u8>, ToolError> {
    let program = program.as_ref();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(match stdout {
            Stdout::Discard => Stdio::null(),
            Stdout::Capture => Stdio::piped(),
        })
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(ToolError::Spawn)?;
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let mut drain =
        std::pin::pin!(async move { tokio::join!(read_all(out_pipe), read_all(err_pipe)) });

    let mut drained = None;
    let status = loop {
        tokio::select! {
            r = &mut drain, if drained.is_none() => drained = Some(r),
            s = child.wait() => break s?,
            () = cancel.cancelled() => {
                stop(&mut child).await;
                return Err(ToolError::Cancelled);
            }
        }
    };
    // The tool has exited. A grandchild that inherited the pipes may keep them open: bound the
    // wait for end of output.
    let (out, err) = match drained {
        Some(r) => r,
        None => tokio::time::timeout(TOOL_STOP_GRACE, drain)
            .await
            .unwrap_or_default(),
    };
    let stderr = String::from_utf8_lossy(&err);
    if !stderr.is_empty() {
        debug!(tool = %program.to_string_lossy(), "{}", stderr.trim_end());
    }
    if !status.success() {
        let mut start = stderr.len().saturating_sub(STDERR_TAIL);
        while !stderr.is_char_boundary(start) {
            start += 1;
        }
        return Err(ToolError::Exit {
            status,
            stderr: stderr[start..].to_owned(),
        });
    }
    Ok(out)
}

async fn read_all(pipe: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        // A read error just ends the capture; the exit status reports failure.
        let _ = pipe.read_to_end(&mut buf).await;
    }
    buf
}

/// Asks the tool to stop, then kills it after the grace period.
async fn stop(child: &mut Child) {
    interrupt(child);
    if tokio::time::timeout(TOOL_STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

/// SIGTERM lets FFmpeg and VLC close their output.
#[cfg(unix)]
fn interrupt(child: &mut Child) {
    use rustix::process::{Pid, Signal, kill_process};
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    if let Some(pid) = pid {
        let _ = kill_process(pid, Signal::TERM);
    }
}

/// Windows has no signal to send: the tool is ended at once.
#[cfg(windows)]
fn interrupt(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[cfg(unix)]
    #[test]
    fn valid_tool_paths() {
        for (p, want) in [
            ("", true),
            ("ffmpeg", true),
            ("/usr/bin/ffmpeg", true),
            ("/opt/ffmpeg-7/bin/ffmpeg", true),
            ("/usr/local/bin/ffmpeg-7.1", true),
            ("/bin/sh", false),
            ("/usr/bin/python3", false),
            ("/tmp/ffmpegx", false),
            ("/usr/bin/ffmpeg/../sh", false),
            ("/tmp/ffmpeg.m4a", false),
            ("bin/ffmpeg", false),
            ("../ffmpeg", false),
            ("ffmpeg\0", false),
            ("/usr//bin/ffmpeg", false),
        ] {
            assert_eq!(valid_tool_path(p, &["ffmpeg"]), want, "{p:?}");
        }
        assert!(valid_tool_path("/usr/bin/cvlc", &["vlc", "cvlc"]));
        assert!(!valid_tool_path("/usr/bin/bash", &["vlc", "cvlc"]));
    }

    // On Windows a path with '/' is as much a path as one with '\', and a drive letter makes it
    // absolute.
    #[cfg(windows)]
    #[test]
    fn valid_tool_paths_windows() {
        for (p, want) in [
            (r"C:\ffmpeg\bin\ffmpeg.exe", true),
            ("C:/ffmpeg/bin/ffmpeg.exe", true),
            ("ffmpeg.exe", true),
            ("ffmpeg", true),
            ("../ffmpeg", false),
            ("bin/ffmpeg", false),
            (r"bin\ffmpeg", false),
            (r"..\ffmpeg.exe", false),
            (r"C:\Windows\System32\cmd.exe", false),
            (r"C:\ffmpeg\..\cmd.exe", false),
        ] {
            assert_eq!(valid_tool_path(p, &["ffmpeg"]), want, "{p:?}");
        }
    }

    #[cfg(unix)]
    fn write_tool(dir: &Path, name: &str, mode: u32) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn tool_executable_checks() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let names = &["ffmpeg"];

        assert!(check_tool_executable(&write_tool(dir, "ffmpeg", 0o755), names).is_ok());
        assert!(
            check_tool_executable("ffmpeg", names).is_ok(),
            "a bare name is left to PATH"
        );
        assert!(matches!(
            check_tool_executable(&write_tool(dir, "ffmpeg-plain", 0o644), names),
            Err(ToolTrustError::NotExecutable(_))
        ));
        assert!(matches!(
            check_tool_executable(&write_tool(dir, "ffmpeg-open", 0o757), names),
            Err(ToolTrustError::Writable(_))
        ));
        let missing = check_tool_executable(&dir.join("ffmpeg-missing").to_string_lossy(), names);
        assert!(missing.is_err_and(|e| e.is_not_found()));

        let link = dir.join("ffmpeg-link");
        std::os::unix::fs::symlink("/bin/sh", &link).unwrap();
        assert!(
            check_tool_executable(&link.to_string_lossy(), names).is_err(),
            "a symlink named ffmpeg that resolves to /bin/sh was accepted"
        );

        let open = dir.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            check_tool_executable(&write_tool(&open, "ffmpeg", 0o755), names),
            Err(ToolTrustError::WritableDir(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn group_writable_dir_is_not_trusted() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tool = PathBuf::from(write_tool(dir, "ffmpeg", 0o755));
        assert!(
            check_tool_trust(&tool).is_ok(),
            "a private directory must be trusted"
        );
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(check_tool_trust(&tool).is_err());
    }

    // A cancelled job ends its tool at once (SIGTERM).
    #[cfg(unix)]
    #[tokio::test]
    async fn tool_stops_on_cancel() {
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stopper.cancel();
        });
        let start = Instant::now();
        let res = run_tool(&cancel, "sleep", &["30".as_ref()], Stdout::Discard).await;
        assert!(matches!(res, Err(ToolError::Cancelled)));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "tool kept running"
        );
    }

    // A tool that ignores SIGTERM is killed after the grace period.
    #[cfg(unix)]
    #[tokio::test]
    async fn tool_ignoring_term_is_killed() {
        let pid_file = tempfile::NamedTempFile::new().unwrap();
        let script = format!(
            "echo $$ > {}; trap '' TERM; while :; do sleep 0.05; done",
            pid_file.path().display()
        );
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await; // let the trap take effect
            stopper.cancel();
        });
        let start = Instant::now();
        let res = run_tool(
            &cancel,
            "sh",
            &["-c".as_ref(), script.as_ref()],
            Stdout::Discard,
        )
        .await;
        assert!(matches!(res, Err(ToolError::Cancelled)));
        assert!(start.elapsed() < TOOL_STOP_GRACE + Duration::from_secs(1));
        let pid: i32 = std::fs::read_to_string(pid_file.path())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        assert!(
            rustix::process::test_kill_process(pid).is_err(),
            "tool process still alive"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_status_and_stderr_are_reported() {
        let cancel = CancellationToken::new();
        let err = run_tool(
            &cancel,
            "sh",
            &["-c".as_ref(), "echo boom >&2; exit 3".as_ref()],
            Stdout::Discard,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().ends_with(": boom"), "{err}");
        let out = run_tool(
            &cancel,
            "sh",
            &["-c".as_ref(), "echo hi".as_ref()],
            Stdout::Capture,
        )
        .await
        .unwrap();
        assert_eq!(out, b"hi\n");
    }
}
