//! dantesync#116 — `config.json`: the file format, its migration, and loading it so that a file
//! that does not parse NEVER costs the node its real configuration.
//!
//! Before 1.21 every failure (unreadable, a UTF-8 BOM, a value of the wrong shape) took the
//! first-boot path and wrote the default template OVER the operator's file: on 13.9.2026 the
//! rig's NTP master lost its master role and the stream box its `gm_allowlist` that way. Now:
//!
//! - a good load writes `config.json.last-good` (atomically, only when it changed);
//! - a failed load writes NOTHING and runs the last-good copy, else the defaults, and says so:
//!   [`ConfigState`] goes to `/status.config_state` and drives the clock alarm;
//! - a UTF-8 BOM (and a UTF-16 file with its BOM) is decoded, and `gm_allowlist` may be a single
//!   string (`crate::config`).
//!
//! The paths are injected, so the loading itself is unit-tested (`load_config` in `main` read a
//! hardcoded path, which is how #68's panicking migration and #116 reached the fleet).

use super::{HttpStatusConfig, NtpServerConfig, SystemConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Simplified configuration - only NTP server needs to be managed
/// All other parameters auto-adjust based on platform defaults
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// camera-box issue 1073: this gained `#[serde(default)]` so a file that
    /// omits `ntp_server` (e.g. an operator who writes only
    /// `{"system": {"gm_allowlist": [...]}}` during the fix rollout) parses
    /// instead of failing the WHOLE-file parse — which (before dantesync#116)
    /// sent `load_config` down its overwrite-with-defaults path and SILENTLY
    /// DELETED the allowlist they just added. Missing
    /// → the fleet default (same value the fresh-config template already writes).
    #[serde(default = "default_ntp_server")]
    pub ntp_server: String,

    /// NTP server mode configuration (optional - disabled by default)
    /// When enabled, this machine becomes an NTP server for the network
    #[serde(default)]
    pub ntp_server_mode: NtpServerConfig,

    /// HTTP status endpoint configuration (optional - enabled by default, port 8898).
    /// Lets LAN automation (e.g. camera-box CI) read PTP/NTP status without the
    /// Windows-only named pipe (#47).
    #[serde(default)]
    pub http_status: HttpStatusConfig,

    /// Advanced system tuning (optional - uses auto-optimized defaults if omitted)
    #[serde(default)]
    pub system: SystemConfig,
}

pub fn default_ntp_server() -> String {
    "10.77.8.2".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ntp_server: default_ntp_server(),
            ntp_server_mode: NtpServerConfig::default(),
            http_status: HttpStatusConfig::default(),
            system: SystemConfig::default(),
        }
    }
}

/// Fill in config keys added by later versions, returning `true` when anything
/// changed (i.e. the file should be written back).
///
/// Extracted from `load_config()` in #68 so it is testable at all (a panicking
/// mutable index once reached the fleet's clock master). dantesync#116 moved the
/// whole loader here, with injected paths, so the loading is tested too.
///
/// **Never index a nested value mutably without proving it is an object first.**
/// `serde_json`'s `IndexMut<&str>` panics on any value that is neither an object
/// nor null, so a hand-edited `"ntp_server_mode": true` — a very plausible
/// attempt to enable it — would abort the daemon at startup, which under a
/// service manager is a restart loop rather than a degraded start. A malformed
/// value is instead left alone and fails the typed parse, which [`load`] answers
/// with the last-good copy (dantesync#116), never with a panic.
pub fn migrate_config_json(json: &mut serde_json::Value) -> bool {
    let mut needs_migration = false;

    // Migrate: add _ntp_server_examples if missing
    if json.get("_ntp_server_examples").is_none() {
        json["_ntp_server_examples"] = serde_json::Value::String(
            "sk.pool.ntp.org, europe.pool.ntp.org, time.google.com, time.cloudflare.com"
                .to_string(),
        );
        needs_migration = true;
    }

    // Migrate: add ntp_server_mode if missing
    if json.get("ntp_server_mode").is_none() {
        json["ntp_server_mode"] = serde_json::json!({
            "enabled": false,
            "port": 123,
            "stratum": 3,
            "max_step_us": 100_000
        });
        needs_migration = true;
    } else if let Some(server_mode) = json["ntp_server_mode"].as_object_mut() {
        // #68: an EXISTING master's config predates the correction bound.
        // `#[serde(default)]` already supplies it, but writing it out makes the
        // knob visible/tunable in the file the operator actually reads.
        if !server_mode.contains_key("max_step_us") {
            server_mode.insert("max_step_us".to_string(), serde_json::json!(100_000));
            needs_migration = true;
        }
    }

    // Migrate: add http_status if missing (#47 — existing installs get the
    // endpoint enabled by default on next start, same as a fresh config)
    if json.get("http_status").is_none() {
        json["http_status"] = serde_json::json!({
            "enabled": true,
            "port": 8898
        });
        needs_migration = true;
    }

    needs_migration
}

/// The file written on a genuine first install (no `config.json`, no last-good copy): every
/// section visible, so the operator sees what can be set.
pub const TEMPLATE: &str = r#"{
  "_ntp_server_examples": "sk.pool.ntp.org, europe.pool.ntp.org, time.google.com, time.cloudflare.com",
  "ntp_server": "10.77.8.2",
  "ntp_server_mode": {
    "enabled": false,
    "port": 123,
    "stratum": 3,
    "max_step_us": 100000
  },
  "http_status": {
    "enabled": true,
    "port": 8898
  },
  "system": {
    "_interface": "dantesync#131 — set \"interface\" to the PTP NIC (e.g. \"enp2s0\", on Windows \"Ethernet\") to pin it; unset = first IPv4 NIC",
    "_phase_slew": "dantesync#97 — set enabled:true (per-box, canary rollout) to SLEW sub-50ms UTC errors instead of stepping",
    "phase_slew": {
      "enabled": false
    }
  }
}"#;

/// `/status.config_state.loaded_from`: the operator's `config.json` was used.
pub const LOADED_FROM_FILE: &str = "file";
/// `config.json` did not load; the copy of the last good load runs.
pub const LOADED_FROM_LAST_GOOD: &str = "last-good";
/// Neither loaded; the built-in defaults run.
pub const LOADED_FROM_DEFAULTS: &str = "defaults";

/// dantesync#116 — where the running configuration came from, published as
/// `/status.config_state` (`null` in a pre-1.21 blob).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigState {
    /// [`LOADED_FROM_FILE`], [`LOADED_FROM_LAST_GOOD`] or [`LOADED_FROM_DEFAULTS`].
    pub loaded_from: String,
    /// Why `config.json` was not used (the IO / JSON / serde message); `null` when it was.
    #[serde(default)]
    pub error: Option<String>,
}

impl ConfigState {
    fn file() -> Self {
        ConfigState {
            loaded_from: LOADED_FROM_FILE.to_string(),
            error: None,
        }
    }

    /// The clock-alarm reason while the node does NOT run its own `config.json`; `None` when it
    /// does. A fallback means the master role, the allowlist or the pinned interface of the file
    /// may be missing: a production fault that must stay loud until the file is fixed.
    pub fn alarm_reason(&self) -> Option<String> {
        if self.loaded_from == LOADED_FROM_FILE {
            return None;
        }
        let running = if self.loaded_from == LOADED_FROM_LAST_GOOD {
            "running the last-good config"
        } else {
            "running defaults"
        };
        Some(format!(
            "config.json not loaded ({}), {running}",
            self.error.as_deref().unwrap_or("unknown error")
        ))
    }
}

/// A loaded configuration and where it came from.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub state: ConfigState,
}

/// The last-good copy beside `config`: `config.json.last-good`.
pub fn last_good_path(config: &Path) -> PathBuf {
    let mut p = config.as_os_str().to_owned();
    p.push(".last-good");
    PathBuf::from(p)
}

/// The text of a config file: a UTF-8 BOM is dropped, a UTF-16 file (LE or BE, with its BOM —
/// PowerShell 5's `Out-File` default) is decoded. Anything else must be UTF-8.
pub fn decode_text(bytes: &[u8]) -> Result<String, String> {
    fn utf16(bytes: &[u8], le: bool) -> Result<String, String> {
        if bytes.len() % 2 != 0 {
            return Err("UTF-16 text with an odd byte count".to_string());
        }
        let units = bytes.chunks_exact(2).map(|c| {
            if le {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        });
        char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .map_err(|e| format!("invalid UTF-16: {e}"))
    }
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => {
            String::from_utf8(rest.to_vec()).map_err(|e| format!("invalid UTF-8: {e}"))
        }
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        _ => String::from_utf8(bytes.to_vec()).map_err(|e| format!("invalid UTF-8: {e}")),
    }
}

/// A config text that parsed: the typed config, its (migrated) JSON, and whether the migration
/// added anything.
#[derive(Debug)]
pub struct Parsed {
    pub config: Config,
    pub json: serde_json::Value,
    pub migrated: bool,
}

/// Decode, parse, migrate and type a config file's bytes. Pure: nothing is read or written.
pub fn parse(bytes: &[u8]) -> Result<Parsed, String> {
    let text = decode_text(bytes)?;
    let mut json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))?;
    // The migration indexes the root mutably: anything but an object would panic there.
    if !json.is_object() {
        return Err("not a JSON object".to_string());
    }
    let migrated = migrate_config_json(&mut json);
    let config = serde_json::from_value::<Config>(json.clone())
        .map_err(|e| format!("not a valid config: {e}"))?;
    Ok(Parsed {
        config,
        json,
        migrated,
    })
}

/// Pretty JSON + a trailing newline: the form written to `config.json` (a migration) and to the
/// last-good copy.
fn render(json: &serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_string_pretty(json)
        .unwrap_or_else(|_| "{}".to_string())
        .into_bytes();
    bytes.push(b'\n');
    bytes
}

/// Write `bytes` to `path` atomically, logging the outcome. Returns whether the file is in place.
fn write_logged(path: &Path, bytes: &[u8], what: &str) -> bool {
    match crate::atomic_file::write(path, bytes) {
        Ok(None) => {
            log::info!("[CONFIG] {what} written: {}", path.display());
            true
        }
        Ok(Some(e)) => {
            log::warn!(
                "[CONFIG] {what} written to {}, but its directory could not be synced: {e}",
                path.display()
            );
            true
        }
        Err(e) => {
            log::warn!(
                "[CONFIG] could not write the {what} {}: {e}",
                path.display()
            );
            false
        }
    }
}

/// Keep `json` as the last-good copy at `last_good`, unless it already holds exactly that.
fn keep_last_good(last_good: &Path, json: &serde_json::Value) {
    let bytes = render(json);
    if std::fs::read(last_good).ok().as_deref() == Some(&bytes[..]) {
        log::debug!("[CONFIG] last-good copy {} is current", last_good.display());
        return;
    }
    write_logged(last_good, &bytes, "last-good copy");
}

/// Read and parse a file; `Ok(None)` when it does not exist.
fn read_parsed(path: &Path) -> Result<Option<Parsed>, String> {
    match std::fs::read(path) {
        Ok(bytes) => parse(&bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("unreadable: {e}")),
    }
}

/// dantesync#116 — load `path`, keeping `last_good` as the fallback.
///
/// - `path` parses: a migration is written back (atomically, only now that the file is known to
///   be good), the last-good copy is refreshed, `loaded_from = "file"`.
/// - `path` exists but does not parse, or cannot be read: `path` is NEVER written. The last-good
///   copy runs (`"last-good"`), else the defaults (`"defaults"`), with the error.
/// - `path` does not exist: with a last-good copy that copy runs (someone removed the file — the
///   template would drop its settings); with none it is a first install, and the template is
///   written and used (`"file"`).
pub fn load(path: &Path, last_good: &Path) -> Loaded {
    let error = match read_parsed(path) {
        Ok(Some(parsed)) => {
            if parsed.migrated {
                write_logged(path, &render(&parsed.json), "migrated config.json");
                log::info!(
                    "Config migrated: added ntp_server_mode / http_status / NTP examples (as needed)"
                );
            }
            keep_last_good(last_good, &parsed.json);
            log::info!("[CONFIG] loaded {}", path.display());
            return Loaded {
                config: parsed.config,
                state: ConfigState::file(),
            };
        }
        Ok(None) if !last_good.exists() => {
            log::info!(
                "[CONFIG] no {} and no last-good copy: first install, writing the template",
                path.display()
            );
            let parsed = parse(TEMPLATE.as_bytes()).expect("the built-in template parses");
            if !write_logged(path, TEMPLATE.as_bytes(), "config.json template") {
                // The template's values ARE the defaults; say there is no file behind them.
                return Loaded {
                    config: parsed.config,
                    state: ConfigState {
                        loaded_from: LOADED_FROM_DEFAULTS.to_string(),
                        error: Some(format!(
                            "{} is missing and cannot be written",
                            path.display()
                        )),
                    },
                };
            }
            keep_last_good(last_good, &parsed.json);
            return Loaded {
                config: parsed.config,
                state: ConfigState::file(),
            };
        }
        Ok(None) => format!("{} is missing", path.display()),
        Err(e) => e,
    };
    log::error!(
        "[CONFIG] {} not loaded: {} -- the file is left untouched; falling back",
        path.display(),
        error
    );
    match read_parsed(last_good) {
        Ok(Some(parsed)) => {
            log::error!(
                "[CONFIG] running the last-good copy {} until {} parses again",
                last_good.display(),
                path.display()
            );
            Loaded {
                config: parsed.config,
                state: ConfigState {
                    loaded_from: LOADED_FROM_LAST_GOOD.to_string(),
                    error: Some(error),
                },
            }
        }
        other => {
            let error = match other {
                Err(e) => format!("{error}; last-good copy not loaded: {e}"),
                _ => error,
            };
            log::error!("[CONFIG] no usable last-good copy: running DEFAULTS ({error})");
            Loaded {
                config: Config::default(),
                state: ConfigState {
                    loaded_from: LOADED_FROM_DEFAULTS.to_string(),
                    error: Some(error),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests;
