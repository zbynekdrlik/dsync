//! dantesync#142 — the controller's side of the drift file (`crate::drift_file`): the seed at
//! start, the seed judge over the first measured windows, the save every 10 minutes while
//! LOCKED and at shutdown, and the `/status` fields.
//!
//! The seed changes only where the servo starts: `drift_baseline_ppm` (the rate servo's
//! integrator) and the frequency word before the warmup. LOCK is untouched (`LOCK_STABLE_COUNT`
//! measured windows under 5 µs/s), and so are the ACQ gains. A seed the first
//! [`SEED_JUDGE_WINDOWS`] measured windows disagree with is dropped: the integrator returns to 0
//! and the servo learns as a cold start does. The file is rewritten only while LOCKED, so a
//! dropped seed is replaced only by a freshly locked value.

use super::*;
use crate::drift_file::{self, DriftRecord, SeedJudge, SeedVerdict, SEED_JUDGE_WINDOWS};
use std::path::{Path, PathBuf};

/// While LOCKED the learned frequency is saved this often (and at shutdown).
pub(super) const DRIFT_SAVE_INTERVAL: Duration = Duration::from_secs(600);

// A seed must be judged before the servo can lock on it.
const _: () = assert!(SEED_JUDGE_WINDOWS < LOCK_STABLE_COUNT);

/// The drift file's state in the controller (the one `freq_seed` field).
#[derive(Debug, Default)]
pub(super) struct FreqSeed {
    /// `drift.json`; `None` until `main` opens it (unit tests, a node without the file).
    path: Option<PathBuf>,
    /// The seed read at start (also after it was dropped); `None` = no usable file.
    pub(super) seed_ppm: Option<f64>,
    /// `/status.freq_seed_source`: `"file"`, `"none: <why>"` or `"rejected: <why>"`; empty until
    /// the file was opened.
    pub(super) source: String,
    /// Judging the seed (`Some` from the seed until the verdict).
    pub(super) judge: Option<SeedJudge>,
    /// The next save while LOCKED; `None` while not locked (set at the lock).
    pub(super) next_save: Option<Instant>,
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// dantesync#142: open the drift file at `path` and, when it holds a usable learned frequency,
    /// seed the servo with it and write it to the clock now (before the warmup). Anything else (no
    /// file, unreadable, malformed, older than 30 days, out of range) starts from 0 and logs why.
    pub fn open_drift_file(&mut self, path: &Path) {
        self.freq_seed.path = Some(path.to_path_buf());
        let now_unix_s = wall_now_ns().div_euclid(1_000_000_000);
        match drift_file::load_seed(path, now_unix_s, DRIFT_MAX_PPM) {
            Ok(record) => self.seed_frequency(record, now_unix_s),
            Err(why) => {
                info!(
                    "[FREQ-SEED] none: {} ({}) — the servo learns the frequency from 0",
                    why,
                    path.display()
                );
                self.freq_seed.source = format!("none: {why}");
            }
        }
        self.update_shared_status();
    }

    fn seed_frequency(&mut self, record: DriftRecord, now_unix_s: i64) {
        let ppm = record.freq_ppm;
        self.drift_baseline_ppm = ppm;
        self.applied_freq_ppm = ppm;
        self.last_adj_ppm = ppm;
        self.freq_seed.seed_ppm = Some(ppm);
        self.freq_seed.source = "file".to_string();
        self.freq_seed.judge = Some(SeedJudge::default());
        match self.clock.adjust_frequency(1.0 + ppm / 1_000_000.0) {
            Ok(()) => info!(
                "[FREQ-SEED] seeded {:+.3}ppm from the drift file (saved {} s ago); judged on the \
                 first {} measured windows",
                ppm,
                now_unix_s - record.written_unix_s,
                SEED_JUDGE_WINDOWS
            ),
            // The servo's next window writes the seeded word anyway.
            Err(e) => warn!(
                "[FREQ-SEED] seeded {:+.3}ppm, but writing it to the clock failed: {} (the first \
                 window writes it)",
                ppm, e
            ),
        }
    }

    /// dantesync#142: a MEASURED window's raw rate while the seed is judged. The judge reads the
    /// rate the oscillator shows against the SEED itself: the raw rate minus what the word applied
    /// over this window (the seed plus the servo's P term) differed from the seed, so the ACQ P
    /// term cannot hide a wrong seed. A rejected seed returns the integrator to 0 (the cold learn)
    /// before this window's integrator step.
    pub(super) fn judge_freq_seed(&mut self, raw_rate_ppm: f64) {
        let seed = self.freq_seed.seed_ppm.unwrap_or(0.0);
        let off_seed_ppm = raw_rate_ppm - (self.applied_freq_ppm - seed);
        let Some(judge) = self.freq_seed.judge.as_mut() else {
            return;
        };
        let Some(verdict) = judge.on_measured(off_seed_ppm) else {
            return;
        };
        self.freq_seed.judge = None;
        match verdict {
            SeedVerdict::Accepted { median_rate_ppm } => info!(
                "[FREQ-SEED] accepted {:+.3}ppm: the first {} measured windows are {:+.1}us/s \
                 off it",
                seed, SEED_JUDGE_WINDOWS, median_rate_ppm
            ),
            SeedVerdict::Rejected { median_rate_ppm } => {
                warn!(
                    "[FREQ-SEED] REJECTED {:+.3}ppm: the first {} measured windows are \
                     {:+.1}us/s off it (beyond {}us/s; another oscillator?) — learning the \
                     frequency from 0",
                    seed,
                    SEED_JUDGE_WINDOWS,
                    median_rate_ppm,
                    drift_file::SEED_REJECT_RATE_PPM
                );
                self.freq_seed.source = format!(
                    "rejected: the first {} measured windows were {:+.1} us/s off the seed",
                    SEED_JUDGE_WINDOWS, median_rate_ppm
                );
                self.drift_baseline_ppm = 0.0;
                self.lock_stable_count = 0;
            }
        }
    }

    /// dantesync#142: after each window — while LOCKED, save the learned frequency every
    /// [`DRIFT_SAVE_INTERVAL`] (the first one an interval after the lock).
    pub(super) fn tick_drift_file(&mut self) {
        if !self.is_locked || self.ptp_offline || self.freq_seed.path.is_none() {
            self.freq_seed.next_save = None;
            return;
        }
        let now = Instant::now();
        match self.freq_seed.next_save {
            None => self.freq_seed.next_save = Some(now + DRIFT_SAVE_INTERVAL),
            Some(due) if now >= due => {
                self.save_drift_file("every 10 min while locked");
                self.freq_seed.next_save = Some(now + DRIFT_SAVE_INTERVAL);
            }
            Some(_) => {}
        }
    }

    /// dantesync#142: at shutdown, save the learned frequency when LOCKED (else the file keeps
    /// the last locked value). `main` calls it after the sync loop ends.
    pub fn save_drift_file_at_shutdown(&mut self) {
        if self.is_locked && !self.ptp_offline {
            self.save_drift_file("at shutdown");
        } else {
            info!("[FREQ-SEED] not locked at shutdown: the drift file keeps its last locked value");
        }
    }

    fn save_drift_file(&mut self, why: &str) {
        let Some(path) = self.freq_seed.path.clone() else {
            return;
        };
        let record = DriftRecord {
            freq_ppm: self.drift_baseline_ppm,
            written_unix_s: wall_now_ns().div_euclid(1_000_000_000),
        };
        match drift_file::save(&path, &record) {
            Ok(None) => info!(
                "[FREQ-SEED] saved {:+.3}ppm ({}) to {}",
                record.freq_ppm,
                why,
                path.display()
            ),
            Ok(Some(e)) => warn!(
                "[FREQ-SEED] saved {:+.3}ppm ({}), but syncing {}'s directory failed: {} (a power \
                 cut may bring the previous record back)",
                record.freq_ppm,
                why,
                path.display(),
                e
            ),
            Err(e) => warn!(
                "[FREQ-SEED] could not save {:+.3}ppm ({}) to {}: {}",
                record.freq_ppm,
                why,
                path.display(),
                e
            ),
        }
    }

    /// dantesync#142: the `/status` fields.
    pub(super) fn publish_freq_seed(&self, status: &mut SyncStatus) {
        status.freq_seed_ppm = self.freq_seed.seed_ppm;
        status.freq_seed_source = if self.freq_seed.source.is_empty() {
            None
        } else {
            Some(self.freq_seed.source.clone())
        };
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod sim_tests;
