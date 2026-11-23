//! Platform-specific file handling.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

/// Open options for every cache file. On POSIX an open file can already be renamed or deleted.
/// On Windows the share mode must include `FILE_SHARE_DELETE`, or renaming a temp file that a
/// reader (or the writer itself) still has open fails with a sharing violation, and no track
/// streamed to a client could ever be committed.
pub(crate) fn shared_options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut opts = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        opts.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    opts
}

pub(crate) fn create_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Reads at `offset` without using or moving a shared cursor where the platform allows it.
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buf, offset)
    }
}

/// Writes at `offset`. Positional writes keep the writer correct on Windows, where a
/// positional read moves the handle's cursor.
pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_write(file, buf, offset)
    }
}

/// Moves a finished temp file to its final name. Windows will not replace a file that
/// something has open (a replay being served from the same entry, say); there the entry that
/// is already in place is kept and the temp file dropped, since both were built for the same
/// asset and quality and hold the same bytes.
pub(crate) fn commit_rename(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let result = fs::rename(tmp, final_path);
    #[cfg(windows)]
    if result.is_err() && fs::metadata(final_path).is_ok_and(|m| m.is_file()) {
        let _ = fs::remove_file(tmp);
        return Ok(());
    }
    result
}
