//! File-system safety for exports: output-folder confinement, and moves that never replace a
//! file unless the overwrite policy says so. Everything here is blocking; async callers run it
//! in `spawn_blocking`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::path;

/// What happens when the output file already exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverwritePolicy {
    /// Keep the existing file and report it as the output.
    #[default]
    Skip,
    /// Replace the existing file.
    Overwrite,
    /// Write to the first free `name (N).ext`.
    Rename,
}

impl OverwritePolicy {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Overwrite => "overwrite",
            Self::Rename => "rename",
        }
    }
}

impl<'de> Deserialize<'de> for OverwritePolicy {
    /// `"overwrite"` and `"rename"`; anything else (including `""`) is skip, as in Go.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "overwrite" => Self::Overwrite,
            "rename" => Self::Rename,
            _ => Self::Skip,
        })
    }
}

/// Why an output folder was refused.
#[derive(Debug, thiserror::Error)]
pub enum OutputDirError {
    #[error("outputDir: invalid path")]
    Invalid,
    #[error("outputDir: {0}")]
    Io(#[from] io::Error),
    #[error("outputDir must be inside one of the allowed export directories")]
    NotAllowed,
}

/// Makes an export folder absolute and clean. A relative value would resolve against the
/// engine's working directory, and one starting with `-` would reach FFmpeg as an option.
pub(crate) fn normalize_output_dir(dir: &str) -> Result<String, OutputDirError> {
    if dir.contains('\0') {
        return Err(OutputDirError::Invalid);
    }
    absolute(dir).map_err(OutputDirError::Io)
}

/// Go's `filepath.Abs`: joined to the working directory if relative, then cleaned.
fn absolute(p: &str) -> io::Result<String> {
    if path::is_abs(p) {
        return Ok(path::clean(p));
    }
    let cwd = std::env::current_dir()?;
    Ok(path::join(&cwd.to_string_lossy(), p))
}

/// Makes `p` absolute and resolves symlinks in its longest existing prefix, keeping the
/// not-yet-created rest, so a path is judged by where it will really land.
pub(crate) fn resolve_existing(p: &str) -> io::Result<PathBuf> {
    let abs = PathBuf::from(absolute(p)?);
    let mut rest = Vec::new();
    let mut cur = abs.as_path();
    loop {
        if let Ok(real) = fs::canonicalize(cur) {
            return Ok(rest
                .iter()
                .rev()
                .fold(real, |acc: PathBuf, name| acc.join(name)));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                cur = parent;
            }
            _ => return Ok(abs),
        }
    }
}

/// Output-folder confinement: when roots are configured, an export folder must lie inside one
/// of them, compared after resolving symlinks.
pub(crate) struct Confinement {
    /// `None`: unrestricted. `Some` (even empty, when no root resolved): confined.
    roots: Option<Vec<PathBuf>>,
    /// Roots added after start-up (folders picked in the native dialog). Must read from a source
    /// the web page cannot write.
    more: Option<crate::MoreRoots>,
}

impl Confinement {
    pub(crate) fn new(roots: &[PathBuf], more: Option<crate::MoreRoots>) -> Self {
        let resolved = (!roots.is_empty()).then(|| {
            roots
                .iter()
                .filter(|r| !r.as_os_str().is_empty())
                .filter_map(|r| resolve_existing(&r.to_string_lossy()).ok())
                .collect()
        });
        Self {
            roots: resolved,
            more,
        }
    }

    pub(crate) fn check(&self, dir: &str) -> Result<(), OutputDirError> {
        let Some(roots) = &self.roots else {
            return Ok(());
        };
        let real = resolve_existing(dir)?;
        let more = self.more.as_ref().map(|f| f()).unwrap_or_default();
        let more = more
            .iter()
            .filter_map(|r| resolve_existing(&r.to_string_lossy()).ok());
        if roots
            .iter()
            .cloned()
            .chain(more)
            .any(|root| real.starts_with(&root))
        {
            Ok(())
        } else {
            Err(OutputDirError::NotAllowed)
        }
    }
}

/// True if `p` exists and is not a directory.
fn file_exists(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| !m.is_dir())
}

/// `base (N)ext` for a path whose Go extension is `ext`.
fn numbered(base: &str, n: usize, ext: &str) -> String {
    format!("{base} ({n}){ext}")
}

/// The early overwrite decision, before downloading: `None` means skip (the file exists and the
/// policy keeps it), otherwise the path to download for. The final move re-checks atomically.
pub(crate) fn overwrite_path(p: &str, policy: OverwritePolicy) -> Option<String> {
    if !file_exists(Path::new(p)) {
        return Some(p.to_owned());
    }
    match policy {
        OverwritePolicy::Overwrite => Some(p.to_owned()),
        OverwritePolicy::Rename => {
            let ext = path::ext(p);
            let base = &p[..p.len() - ext.len()];
            (1..)
                .map(|i| numbered(base, i, ext))
                .find(|c| !file_exists(Path::new(c)))
        }
        OverwritePolicy::Skip => None,
    }
}

/// How many `name (N).ext` candidates the rename policy tries.
pub(crate) const MAX_RENAME_ATTEMPTS: usize = 10_000;

/// Moves `tmp` to `dest` honouring `policy` without a check-then-act gap: overwrite replaces;
/// rename takes the first free `name (N).ext`; skip keeps the existing file. The claim on a name
/// is a hard link, which fails atomically when the name is taken; without hard links,
/// [`move_no_replace`] is just as atomic. Returns where the content ended up (the existing file
/// when skipped). `tmp` is gone afterwards, also on failure.
pub(crate) fn move_no_clobber(
    tmp: &Path,
    dest: &str,
    policy: OverwritePolicy,
    max_attempts: usize,
) -> io::Result<String> {
    if policy == OverwritePolicy::Overwrite {
        return move_file(tmp, Path::new(dest)).map(|()| dest.to_owned());
    }
    let ext = path::ext(dest);
    let base = &dest[..dest.len() - ext.len()];
    for i in 0..max_attempts {
        let candidate = if i == 0 {
            dest.to_owned()
        } else {
            numbered(base, i, ext)
        };
        match fs::hard_link(tmp, &candidate) {
            Ok(()) => {
                let _ = fs::remove_file(tmp);
                return Ok(candidate);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            // No hard links here (or another device): a move that atomically refuses an
            // existing name, so a file created meanwhile is never replaced.
            Err(_) => match move_no_replace(tmp, Path::new(&candidate)) {
                Ok(()) => return Ok(candidate),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    let _ = fs::remove_file(tmp);
                    return Err(e);
                }
            },
        }
        // The name is taken.
        if policy == OverwritePolicy::Skip {
            let _ = fs::remove_file(tmp);
            return Ok(dest.to_owned());
        }
    }
    let _ = fs::remove_file(tmp);
    Err(io::Error::other(format!(
        "no free name for {} after {max_attempts} attempts",
        path::base(dest)
    )))
}

/// Renames `src` over `dst`, falling back to an atomic copy across devices.
fn move_file(src: &Path, dst: &Path) -> io::Result<()> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_file(src, dst)?;
    let _ = fs::remove_file(src);
    Ok(())
}

/// Copies `src` to `dst` through a temp file in `dst`'s folder renamed into place, so a failure
/// never leaves a partial `dst` or damages an existing one.
pub(crate) fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src)?;
    let dir = dst.parent().unwrap_or(Path::new("."));
    let mut out = tempfile::Builder::new()
        .prefix(".am-copy-")
        .tempfile_in(dir)?;
    io::copy(&mut input, &mut out)?;
    out.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(out.path(), fs::Permissions::from_mode(0o644))?;
    }
    out.persist(dst).map_err(|e| e.error)?;
    Ok(())
}

/// Moves `src` to `dst` only if `dst` does not exist, never replacing a file that appears there
/// meanwhile; fails with `AlreadyExists` when `dst` is taken. Linux tries an atomic
/// `renameat2(RENAME_NOREPLACE)` first; otherwise the name is claimed with `O_EXCL` and `src` is
/// copied into it, so a plain rename (which would replace a racing file) is never used.
pub(crate) fn move_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        use rustix::io::Errno;
        match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(()),
            // Filesystems or kernels without it, and moves across devices: copy instead.
            Err(Errno::INVAL | Errno::NOSYS | Errno::XDEV | Errno::PERM | Errno::OPNOTSUPP) => {}
            Err(e) => return Err(e.into()),
        }
    }
    copy_into_new(src, dst)
}

/// Claims `dst` exclusively, copies `src` into it and removes `src`. A failed copy removes the
/// claimed (partial) `dst`, which is ours: nobody else can have written it.
pub(crate) fn copy_into_new(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src)?;
    let mut out = OpenOptions::new().write(true).create_new(true).open(dst)?;
    let res = io::copy(&mut input, &mut out)
        .and_then(|_| out.flush())
        .and_then(|()| out.sync_all());
    drop(out);
    if let Err(e) = res {
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    let _ = fs::remove_file(src);
    Ok(())
}

/// `$XDG_MUSIC_DIR/Rustet-Downloads`, else `~/Music/Rustet-Downloads` (the temp folder when there is
/// no home).
pub(crate) fn default_output_dir() -> String {
    let base = std::env::var_os("XDG_MUSIC_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Music")
        });
    base.join("Rustet-Downloads").to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    fn read(p: impl AsRef<Path>) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn skip_keeps_existing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = write(dir.path(), "a.m4a", "old");
        let tmp = write(dir.path(), ".tmp", "new");
        let got = move_no_clobber(&tmp, &s(&dest), OverwritePolicy::Skip, MAX_RENAME_ATTEMPTS);
        assert_eq!(got.unwrap(), s(&dest));
        assert_eq!(read(&dest), "old", "skip policy overwrote an existing file");
        assert!(!tmp.exists(), "temp file left behind");
    }

    #[test]
    fn rename_picks_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let dest = write(dir.path(), "a.m4a", "one");
        write(dir.path(), "a (1).m4a", "two");
        let tmp = write(dir.path(), ".tmp", "three");
        let got = move_no_clobber(
            &tmp,
            &s(&dest),
            OverwritePolicy::Rename,
            MAX_RENAME_ATTEMPTS,
        )
        .unwrap();
        assert_eq!(path::base(&got), "a (2).m4a");
        assert_eq!(read(&got), "three");
        assert_eq!(read(&dest), "one", "existing file clobbered");
    }

    #[test]
    fn overwrite_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let dest = write(dir.path(), "a.m4a", "old");
        let tmp = write(dir.path(), ".tmp", "new");
        move_no_clobber(
            &tmp,
            &s(&dest),
            OverwritePolicy::Overwrite,
            MAX_RENAME_ATTEMPTS,
        )
        .unwrap();
        assert_eq!(read(&dest), "new");
    }

    #[test]
    fn new_file() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = write(dir.path(), ".tmp", "x");
        let dest = s(&dir.path().join("fresh.m4a"));
        let got = move_no_clobber(&tmp, &dest, OverwritePolicy::Skip, MAX_RENAME_ATTEMPTS).unwrap();
        assert_eq!(got, dest);
        assert_eq!(read(&dest), "x");
    }

    #[test]
    fn rename_gives_up_after_the_attempt_bound() {
        let dir = tempfile::tempdir().unwrap();
        let dest = write(dir.path(), "a.m4a", "0");
        write(dir.path(), "a (1).m4a", "1");
        write(dir.path(), "a (2).m4a", "2");
        let tmp = write(dir.path(), ".tmp", "new");
        assert!(move_no_clobber(&tmp, &s(&dest), OverwritePolicy::Rename, 3).is_err());
        assert!(
            !tmp.exists(),
            "the temp file must not be left behind on failure"
        );
        for (name, want) in [("a.m4a", "0"), ("a (1).m4a", "1"), ("a (2).m4a", "2")] {
            assert_eq!(read(dir.path().join(name)), want, "{name} was modified");
        }
    }

    #[test]
    fn failed_copy_leaves_destination_intact() {
        let dir = tempfile::tempdir().unwrap();
        let dst = write(dir.path(), "keep.m4a", "precious");
        assert!(copy_file(&dir.path().join("does-not-exist"), &dst).is_err());
        assert_eq!(read(&dst), "precious");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "leftover files"
        );
    }

    #[test]
    fn move_no_replace_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let dst = write(dir.path(), "a.m4a", "existing");
        let src = write(dir.path(), ".tmp", "new");
        let err = move_no_replace(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&dst), "existing");
        let free = dir.path().join("b.m4a");
        move_no_replace(&src, &free).unwrap();
        assert_eq!(read(&free), "new");
        assert!(!src.exists(), "source must be gone after the move");
    }

    // The copy fallback (no atomic rename on this filesystem) claims the name too.
    #[test]
    fn copy_into_new_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let dst = write(dir.path(), "a.m4a", "existing");
        let src = write(dir.path(), ".tmp", "new");
        let err = copy_into_new(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(&dst), "existing");
        assert!(src.exists(), "source must be kept when the name is taken");
    }

    #[test]
    fn overwrite_path_policies() {
        let dir = tempfile::tempdir().unwrap();
        let missing = s(&dir.path().join("missing.m4a"));
        assert_eq!(
            overwrite_path(&missing, OverwritePolicy::Skip),
            Some(missing)
        );
        let existing = s(&write(dir.path(), "a.m4a", ""));
        assert_eq!(overwrite_path(&existing, OverwritePolicy::Skip), None);
        assert_eq!(
            overwrite_path(&existing, OverwritePolicy::Overwrite).as_deref(),
            Some(existing.as_str())
        );
        let renamed = overwrite_path(&existing, OverwritePolicy::Rename).unwrap();
        assert_eq!(path::base(&renamed), "a (1).m4a");
    }

    #[test]
    fn normalized_output_dir_is_always_absolute() {
        for input in [
            "-y",
            "-i/evil",
            "relative/dir",
            "../up",
            ".",
            "/already/abs",
            "/a/../b",
        ] {
            let got = normalize_output_dir(input).unwrap();
            assert!(path::is_abs(&got), "{input:?} -> {got:?}");
        }
        assert!(matches!(
            normalize_output_dir("a\0b"),
            Err(OutputDirError::Invalid)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn roots_confine_exports() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let r = root.path();
        let c = Confinement::new(&[r.to_owned()], None);
        for (dir, want) in [
            (s(r), true),
            (s(&r.join("Music").join("New")), true), // not created yet
            (s(outside.path()), false),
            (format!("{}/../elsewhere", s(r)), false),
            (format!("{}-sibling", s(r)), false), // shares a prefix, not a path element
            (s(&r.join("escape")), false),        // a symlink out of the root
            (s(&r.join("escape").join("deeper")), false),
            ("/etc".to_owned(), false),
        ] {
            assert_eq!(c.check(&dir).is_ok(), want, "{dir}");
        }
        assert!(
            Confinement::new(&[], None).check("/etc").is_ok(),
            "no roots configured must mean unrestricted"
        );
        let unresolvable = Confinement::new(&[PathBuf::from("/definitely/not/a/dir/\0")], None);
        assert!(
            unresolvable.check(&s(r)).is_err(),
            "roots were configured, so a path outside them must be refused even if no root resolved"
        );
    }

    #[test]
    fn more_roots_extend_confinement() {
        use std::sync::{Arc, Mutex};
        let (root, extra, other) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let picked: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let source = Arc::clone(&picked);
        let c = Confinement::new(
            &[root.path().to_owned()],
            Some(Box::new(move || source.lock().unwrap().clone())),
        );
        assert!(
            c.check(&s(extra.path())).is_err(),
            "accepted before it was picked"
        );
        picked.lock().unwrap().push(extra.path().to_owned());
        assert!(c.check(&s(extra.path())).is_ok(), "picked folder rejected");
        assert!(
            c.check(&s(other.path())).is_err(),
            "unrelated folder accepted"
        );
    }
}
