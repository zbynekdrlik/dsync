//! Write a small file so a reader never sees a torn one: a temp file beside it, synced, renamed
//! over the target, then the directory synced (Unix) so a power cut cannot bring the old file
//! back.
//!
//! Extracted from the master's fleet-date record (dsync#126, `controller/date_sync/
//! restart_file.rs`) for `config.json` and its last-good copy (dsync#116): one
//! implementation for every file this daemon rewrites.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The directory a file at `path` lives in (the directory to sync after the rename; `.` for a
/// bare file name).
pub fn dir_to_sync(path: &Path) -> Option<PathBuf> {
    match path.parent() {
        Some(d) if d.as_os_str().is_empty() => Some(PathBuf::from(".")),
        other => other.map(Path::to_path_buf),
    }
}

/// Sync a directory (Unix; a no-op elsewhere).
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Write `bytes` to `path` by temp (`<path>.tmp`, synced) + rename, then sync the directory with
/// `sync_dir`. `Err` = nothing replaced (the target is unchanged). `Ok(Some(e))` = the new file is
/// in place, only the directory sync after the rename failed (its durability across a power cut
/// is not proven): the caller decides how loudly to say so.
pub fn write_with(
    path: &Path,
    bytes: &[u8],
    sync_dir: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<Option<std::io::Error>> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Renamed: the new file is in place. A directory that refuses the sync does not undo it.
    Ok(dir_to_sync(path).and_then(|dir| sync_dir(&dir).err()))
}

/// [`write_with`] with the real directory sync.
pub fn write(path: &Path, bytes: &[u8]) -> std::io::Result<Option<std::io::Error>> {
    write_with(path, bytes, sync_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_replaced_whole_and_the_temp_file_renamed_away_116() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"old").expect("seed");
        assert!(write(&path, b"new").expect("written").is_none());
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
        assert!(!dir.path().join("config.json.tmp").exists());
        assert!(write(&dir.path().join("no/such/dir/x"), b"x").is_err());
    }

    #[test]
    fn a_failed_directory_sync_is_reported_but_the_file_is_in_place_116() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.json");
        let mut synced = None;
        let late = write_with(&path, b"{}", |d| {
            synced = Some(d.to_path_buf());
            Err(std::io::Error::from_raw_os_error(22))
        })
        .expect("the rename is done");
        assert!(late.is_some());
        assert_eq!(synced.as_deref(), Some(dir.path()));
        assert_eq!(std::fs::read(&path).expect("read"), b"{}");
    }
}
