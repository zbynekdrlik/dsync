//! dsync#147 — where the daemon keeps its state: `config.json` (and `config.json.last-good`),
//! `date-offset.json`, `drift.json` and, on Windows, the service log.
//!
//! The project was renamed from its old name to dsync (1.25.0). The state directory follows the
//! new name, but a box installed before the rename keeps its old directory. The daemon resolves
//! the directory once at start (ROZHODNUTÉ #147, 2026-11-02: resolve, never copy):
//!
//! 1. the new directory (`/etc/dsync`, `C:\ProgramData\dsync`) when it exists;
//! 2. else the legacy directory (`/etc/dantesync`, `C:\ProgramData\DanteSync`) when it exists,
//!    used IN PLACE: nothing is copied, so a box keeps exactly one `drift.json` (the stability
//!    gate's `--cold` moves aside only the legacy one);
//! 3. else it creates the new directory.
//!
//! The choice is logged and reported in `/status` (`state_dir`). A deployer that renames the
//! legacy directory gets the new one at the next start, with no code change.
//!
//! [`existing`] is the read-only lookup for the tray, which never creates a system directory.

use std::path::{Path, PathBuf};

/// The new state directory of this platform.
#[cfg(windows)]
pub const NEW_DIR: &str = r"C:\ProgramData\dsync";
/// The new state directory of this platform.
#[cfg(not(windows))]
pub const NEW_DIR: &str = "/etc/dsync";

/// The pre-1.25 state directory of this platform, used in place when only it exists.
#[cfg(windows)]
pub const LEGACY_DIR: &str = r"C:\ProgramData\DanteSync";
/// The pre-1.25 state directory of this platform, used in place when only it exists.
#[cfg(not(windows))]
pub const LEGACY_DIR: &str = "/etc/dantesync";

/// Which rule picked the state directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// The new directory existed.
    New,
    /// Only the legacy directory existed; it is used in place.
    Legacy,
    /// Neither existed; the new directory was created.
    Created,
    /// Neither existed and creating the new one failed. The daemon still uses the new path: every
    /// state write then fails and logs, as a missing directory did before.
    CreateFailed,
}

impl Choice {
    /// The word the start log and `/status` use for this choice.
    pub fn as_str(self) -> &'static str {
        match self {
            Choice::New => "new",
            Choice::Legacy => "legacy",
            Choice::Created => "created",
            Choice::CreateFailed => "create-failed",
        }
    }
}

/// The resolved state directory and the rule that picked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    pub path: PathBuf,
    pub choice: Choice,
    /// The error text when [`Choice::CreateFailed`].
    pub error: Option<String>,
}

impl StateDir {
    /// `config.json`.
    pub fn config_path(&self) -> PathBuf {
        self.path.join("config.json")
    }

    /// `date-offset.json` (the NTP master's fleet date offset, dsync#126).
    pub fn date_state_path(&self) -> PathBuf {
        self.path.join("date-offset.json")
    }

    /// `drift.json` (the learned frequency, dsync#142).
    pub fn drift_path(&self) -> PathBuf {
        self.path.join("drift.json")
    }

    /// The Windows service log.
    pub fn log_path(&self) -> PathBuf {
        self.path.join("dsync.log")
    }

    /// The one log line that says which directory is used and why.
    pub fn describe(&self) -> String {
        let path = self.path.display();
        match self.choice {
            Choice::New => format!("[State] state dir {} (exists)", path),
            Choice::Legacy => format!(
                "[State] state dir {} (legacy dir used in place: {} does not exist)",
                path, NEW_DIR
            ),
            Choice::Created => {
                format!("[State] state dir {} (created: no state dir existed)", path)
            }
            Choice::CreateFailed => format!(
                "[State] state dir {} could not be created: {} — state files will not be saved",
                path,
                self.error.as_deref().unwrap_or("unknown error")
            ),
        }
    }
}

/// The existing state directory, new before legacy, or `None` when neither exists. Never creates
/// anything (the tray).
pub fn existing(new: &Path, legacy: &Path) -> Option<PathBuf> {
    if new.is_dir() {
        Some(new.to_path_buf())
    } else if legacy.is_dir() {
        Some(legacy.to_path_buf())
    } else {
        None
    }
}

/// Resolve the state directory from `new` and `legacy` (the rules in the module doc), creating
/// `new` when neither exists.
pub fn resolve(new: &Path, legacy: &Path) -> StateDir {
    if new.is_dir() {
        return StateDir {
            path: new.to_path_buf(),
            choice: Choice::New,
            error: None,
        };
    }
    if legacy.is_dir() {
        return StateDir {
            path: legacy.to_path_buf(),
            choice: Choice::Legacy,
            error: None,
        };
    }
    match std::fs::create_dir_all(new) {
        Ok(()) => StateDir {
            path: new.to_path_buf(),
            choice: Choice::Created,
            error: None,
        },
        Err(e) => StateDir {
            path: new.to_path_buf(),
            choice: Choice::CreateFailed,
            error: Some(e.to_string()),
        },
    }
}

/// [`resolve`] with this platform's directories.
pub fn resolve_platform() -> StateDir {
    resolve(Path::new(NEW_DIR), Path::new(LEGACY_DIR))
}

/// [`existing`] with this platform's directories.
pub fn existing_platform() -> Option<PathBuf> {
    existing(Path::new(NEW_DIR), Path::new(LEGACY_DIR))
}

#[cfg(test)]
mod tests;
