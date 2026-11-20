//! The single-instance lock of the `dsync` service (moved from `main.rs`, dsync#214): one
//! daemon per box, an old (pre-1.25) and a new one never both (dsync#147).
//! On Windows, we use a named mutex (like the tray app does)
//! On Linux, we use flock on a lock file

#[cfg(unix)]
use anyhow::anyhow;
use anyhow::Result;
#[cfg(unix)]
use nix::fcntl::{flock, FlockArg};
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
#[cfg(windows)]
use windows::Win32::System::Threading::CreateMutexW;

#[cfg(unix)]
pub(crate) struct SingleInstanceGuard {
    _files: Vec<File>,
}

/// dsync#147: the singleton lock files. The pre-1.25 daemon held only the legacy one, so this
/// build takes BOTH: an old and a new daemon on one box (a deployer's unit switch half done) must
/// never both discipline the clock.
#[cfg(unix)]
const SINGLETON_LOCK_PATHS: [&str; 2] = ["/var/run/dsync.lock", "/var/run/dantesync.lock"];

/// Take an exclusive, non-blocking `flock` on every path in order; the first one held by another
/// process is an error naming it.
#[cfg(unix)]
fn lock_files_exclusive(paths: &[&str]) -> Result<Vec<File>> {
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| anyhow!("Failed to create lock file {}: {}", path, e))?;
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(_) => files.push(file),
            Err(nix::errno::Errno::EAGAIN) => {
                return Err(anyhow!(
                    "Another instance of dsync is already running! (Lockfile: {})",
                    path
                ))
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(files)
}

#[cfg(unix)]
impl std::fmt::Debug for SingleInstanceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleInstanceGuard").finish()
    }
}

#[cfg(windows)]
pub(crate) struct SingleInstanceGuard {
    _handles: Vec<HANDLE>,
}

/// dsync#147: the singleton mutexes — this build's and the pre-1.25 daemon's (see the Unix lock
/// files): an old and a new daemon must never both run.
#[cfg(windows)]
const SINGLETON_MUTEX_NAMES: [&str; 2] =
    ["Global\\DsyncDaemonMutex", "Global\\DanteSyncDaemonMutex"];

#[cfg(windows)]
impl std::fmt::Debug for SingleInstanceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleInstanceGuard").finish()
    }
}

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        for handle in &self._handles {
            unsafe {
                let _ = CloseHandle(*handle);
            }
        }
    }
}

pub(crate) fn acquire_singleton_lock() -> Result<SingleInstanceGuard> {
    #[cfg(unix)]
    {
        lock_files_exclusive(&SINGLETON_LOCK_PATHS)
            .map(|files| SingleInstanceGuard { _files: files })
    }
    #[cfg(windows)]
    {
        use anyhow::anyhow;

        let mut guard = SingleInstanceGuard {
            _handles: Vec::with_capacity(SINGLETON_MUTEX_NAMES.len()),
        };
        for name in SINGLETON_MUTEX_NAMES {
            let mutex_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                match CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())) {
                    Ok(handle) => {
                        // Check if mutex already existed (another instance is running)
                        if let Err(e) = GetLastError() {
                            if e.code() == ERROR_ALREADY_EXISTS.to_hresult() {
                                let _ = CloseHandle(handle);
                                // `guard` drops here and closes the mutexes taken so far.
                                return Err(anyhow!(
                                    "Another instance of dsync is already running! ({})",
                                    name
                                ));
                            }
                        }
                        guard._handles.push(handle);
                    }
                    Err(e) => return Err(anyhow!("Failed to create singleton mutex: {:?}", e)),
                }
            }
        }
        Ok(guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dsync#147: the new AND the legacy lock file are both taken; either one held by another
    /// holder (a pre-1.25 daemon holds only the legacy one) refuses the start.
    #[cfg(unix)]
    #[test]
    fn the_singleton_takes_the_new_and_the_legacy_lock_147() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("dsync.lock");
        let legacy = dir.path().join("legacy.lock");
        let paths = [new.to_str().unwrap(), legacy.to_str().unwrap()];

        // A pre-1.25 daemon holding only the legacy lock blocks the start.
        let old = lock_files_exclusive(&paths[1..]).expect("the old daemon's lock");
        let err = lock_files_exclusive(&paths).expect_err("the legacy lock is held");
        assert!(err.to_string().contains("already running"), "{err}");
        assert!(err.to_string().contains("legacy.lock"), "{err}");
        drop(old);

        // Free: both taken; a second instance is refused on the new one.
        let first = lock_files_exclusive(&paths).expect("both locks free");
        assert_eq!(first.len(), 2);
        let err = lock_files_exclusive(&paths).expect_err("held by the first");
        assert!(err.to_string().contains("dsync.lock"), "{err}");
        drop(first);
        assert!(lock_files_exclusive(&paths).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn singleton_lock_prevents_duplicate_instances() {
        // First lock should succeed
        let lock1 = acquire_singleton_lock();
        assert!(lock1.is_ok(), "First lock acquisition should succeed");

        // Second lock should fail (another instance already running)
        let lock2 = acquire_singleton_lock();
        assert!(lock2.is_err(), "Second lock acquisition should fail");

        // Verify the error message mentions "already running"
        let err_msg = lock2.unwrap_err().to_string();
        assert!(
            err_msg.contains("already running"),
            "Error should mention 'already running', got: {}",
            err_msg
        );

        // After dropping lock1, a new lock should succeed
        drop(lock1);
        let lock3 = acquire_singleton_lock();
        assert!(lock3.is_ok(), "Lock after release should succeed");
    }
}
