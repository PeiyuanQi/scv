//! Private instance files: SCV's state, records, and credentials are
//! written whole, readable only by the user, and survive a crash.

use std::io::{self, Write as _};
use std::path::Path;

/// Replace `path` with `bytes` atomically, as a file only the user can read.
///
/// The bytes go to a temporary file in the same directory, which is synced
/// and renamed over `path`; the directory is then synced so the rename
/// itself survives a crash. A reader sees the old file or the new one, never
/// a partial write. The directory must exist; callers create it with the
/// privacy their data needs.
pub fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no parent", path.display())))?;
    // Named temporary files are created with mode 0600.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_directory(parent)
}

/// Flush a directory's entries, so files created, renamed, or removed in it
/// persist. A no-op where directories cannot be opened (not Unix).
pub fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

#[cfg(test)]
mod tests;
