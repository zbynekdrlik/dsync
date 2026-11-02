//! dsync#142 — the learned frequency survives a restart (a drift file, as chrony's and
//! ntpd's `driftfile`).
//!
//! A cold start learned the oscillator's offset against the grandmaster from 0 at the ACQ
//! integrator's 0.05 × rate per window: ~+35 ppm on iemmixer's gen3 box took until ~78 s, and
//! LOCK (`LOCK_STABLE_COUNT` windows under 5 µs/s) came ~86 s after boot. Nothing carried the
//! learned frequency across a reboot: the kernel / Windows adjustment resets at boot.
//!
//! Now a LOCKED node saves its learned frequency (`drift_baseline_ppm`, the rate servo's
//! integrator, which the phase lock keeps on its own integrator) every 10 minutes and at
//! shutdown, and a start seeds the servo with it before the warmup. LOCK stays exactly as strict:
//! the seed only changes where the servo starts.
//!
//! This module is the record, its validation and the seed judge ([`SeedJudge`]): a seed the first
//! measured windows disagree with (another box's oscillator, e.g. the disk moved) is dropped and
//! the servo learns from 0. The controller glue is `controller/freq_seed.rs`.
//!
//! The file is `drift.json` beside `date-offset.json` (`/etc/dsync/`,
//! `C:\ProgramData\dsync\`), written by [`crate::atomic_file`] (temp file, fsync, rename,
//! directory fsync): a power cut leaves the old record or the new one, never a torn one.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The version of the record. A record of another version is not used (cold learn).
pub const DRIFT_FILE_VERSION: u32 = 1;

/// A record older than this is not used: a box that stood that long may have new hardware, and
/// its oscillator has aged. 30 days.
pub const MAX_SEED_AGE_S: i64 = 30 * 86_400;

/// A record written more than this AFTER the wall clock at start is not used: the wall (an RTC
/// at boot) or the record is wrong, and its age cannot be judged. One day covers an RTC that is
/// hours off; a dead CMOS battery (a year 2000 wall) falls back to the cold learn.
pub const MAX_FUTURE_SKEW_S: i64 = 86_400;

/// How many MEASURED windows judge a seed. Fewer than `LOCK_STABLE_COUNT` (5), so a seed is
/// always accepted or dropped before the servo can lock on it.
pub const SEED_JUDGE_WINDOWS: usize = 4;

/// A seed is dropped when the oscillator's mean rate against it over the first
/// [`SEED_JUDGE_WINDOWS`] measured windows is beyond this (µs/s = ppm off the seed): the rate at
/// which the servo leaves PROD for ACQ. A disagreement this large means the seed belongs to
/// another oscillator (a moved disk, new hardware), not to a warmer or colder one; the servo then
/// learns from 0 as a cold start does. A smaller error is kept and corrected by the servo from the
/// seed.
pub const SEED_REJECT_RATE_PPM: f64 = 20.0;

/// The saved record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriftRecord {
    /// The learned frequency (ppm) the node was locked on.
    pub freq_ppm: f64,
    /// The wall clock when it was written (s since the Unix epoch).
    pub written_unix_s: i64,
}

#[derive(Serialize, Deserialize)]
struct DriftFile {
    version: u32,
    freq_ppm: f64,
    written_unix_s: i64,
}

/// The record as the file's JSON text.
pub fn encode(record: &DriftRecord) -> String {
    let file = DriftFile {
        version: DRIFT_FILE_VERSION,
        freq_ppm: record.freq_ppm,
        written_unix_s: record.written_unix_s,
    };
    // A finite f64 and two integers always serialize; `save` refuses a non-finite value first.
    serde_json::to_string_pretty(&file).unwrap_or_default()
}

/// The record from the file's JSON text, or why it is not usable.
pub fn decode(text: &str) -> Result<DriftRecord, String> {
    let f: DriftFile =
        serde_json::from_str(text).map_err(|e| format!("not a drift record: {e}"))?;
    if f.version != DRIFT_FILE_VERSION {
        return Err(format!(
            "version {} (this build reads {})",
            f.version, DRIFT_FILE_VERSION
        ));
    }
    Ok(DriftRecord {
        freq_ppm: f.freq_ppm,
        written_unix_s: f.written_unix_s,
    })
}

/// Whether a record may seed the servo at `now_unix_s` (the wall clock at start): its age within
/// [`MAX_SEED_AGE_S`] (and not more than [`MAX_FUTURE_SKEW_S`] in the future), its frequency
/// finite and within ±`max_ppm` (the servo's own `DRIFT_MAX_PPM`). `Err` = why not.
pub fn validate(record: &DriftRecord, now_unix_s: i64, max_ppm: f64) -> Result<f64, String> {
    let age_s = now_unix_s.saturating_sub(record.written_unix_s);
    if age_s > MAX_SEED_AGE_S {
        return Err(format!(
            "{} days old (more than {})",
            age_s / 86_400,
            MAX_SEED_AGE_S / 86_400
        ));
    }
    if age_s < -MAX_FUTURE_SKEW_S {
        return Err(format!(
            "written {} s in the future of this start's wall clock",
            -age_s
        ));
    }
    if !record.freq_ppm.is_finite() || record.freq_ppm.abs() > max_ppm {
        return Err(format!(
            "{} ppm is outside ±{} ppm",
            record.freq_ppm, max_ppm
        ));
    }
    Ok(record.freq_ppm)
}

/// The seed at start, read from `path`: `Ok` = the frequency to seed, `Err` = why the servo
/// learns from 0 instead (no file, unreadable, malformed, another version, too old, out of range).
pub fn load_seed(path: &Path, now_unix_s: i64, max_ppm: f64) -> Result<DriftRecord, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("no drift file yet".to_string())
        }
        Err(e) => return Err(format!("unreadable: {e}")),
    };
    let record = decode(&text)?;
    validate(&record, now_unix_s, max_ppm)?;
    Ok(record)
}

/// Write the record to `path` power-loss safe ([`crate::atomic_file::write`]). `Err` = nothing
/// replaced; `Ok(Some(e))` = the record is in place, only the directory sync failed.
pub fn save(path: &Path, record: &DriftRecord) -> std::io::Result<Option<std::io::Error>> {
    if !record.freq_ppm.is_finite() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the learned frequency is not a finite number",
        ));
    }
    crate::atomic_file::write(path, encode(record).as_bytes())
}

/// The verdict on a seed after [`SEED_JUDGE_WINDOWS`] measured windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeedVerdict {
    /// The windows agree with the seed: their mean rate against it (ppm).
    Accepted { mean_rate_ppm: f64 },
    /// They do not: their mean rate against it, beyond [`SEED_REJECT_RATE_PPM`].
    Rejected { mean_rate_ppm: f64 },
}

/// Judges a seed on the first measured windows' rates against the seed (µs/s = ppm: what the
/// oscillator shows with the seed alone as its word), weighted by each window's length. Each
/// window's rate is (its offset − the previous window's) / dt, so the weighted mean telescopes to
/// the offset's slope over the whole span (the controller judges only windows with a measured
/// rate, never one shorter than 0.1 s, which carries the smoothed rate instead): only the first and the last window's jitter remain,
/// divided by the span (2 s), and a spike inside the span cancels out. A per-window median of
/// three would keep each window's own jitter (on a Windows box tens of µs/s per window).
#[derive(Clone, Debug, Default)]
pub struct SeedJudge {
    windows: usize,
    weighted_sum: f64,
    span_s: f64,
}

impl SeedJudge {
    /// A measured window's rate against the seed and its length; `Some` once the judge has its
    /// windows (then it is done).
    pub fn on_measured(&mut self, rate_off_seed_ppm: f64, dt_s: f64) -> Option<SeedVerdict> {
        self.windows += 1;
        self.weighted_sum += rate_off_seed_ppm * dt_s;
        self.span_s += dt_s;
        if self.windows < SEED_JUDGE_WINDOWS {
            return None;
        }
        let mean_rate_ppm = if self.span_s > 0.0 {
            self.weighted_sum / self.span_s
        } else {
            0.0
        };
        Some(if mean_rate_ppm.abs() > SEED_REJECT_RATE_PPM {
            SeedVerdict::Rejected { mean_rate_ppm }
        } else {
            SeedVerdict::Accepted { mean_rate_ppm }
        })
    }
}

#[cfg(test)]
mod tests;
