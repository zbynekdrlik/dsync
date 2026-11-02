//! dsync#142 — the controller's side of the drift file (`crate::drift_file`): the seed at
//! start, the seed judge over the first measured windows, the save every 10 minutes while
//! LOCKED and at shutdown, and the `/status` fields.
//!
//! The seed changes only where the servo starts: `drift_baseline_ppm` (the rate servo's
//! integrator) and the frequency word before the warmup. LOCK is untouched (`LOCK_STABLE_COUNT`
//! measured windows under 5 µs/s), and so are the ACQ gains. A seed the first
//! [`SEED_JUDGE_WINDOWS`] measured windows disagree with is dropped: the integrator returns to 0
//! and the servo learns as a cold start does. The file is rewritten only while LOCKED, so a
//! dropped seed is replaced only by a freshly locked value.
//!
//! The save (a small file, fsync'd twice) runs on the sync loop once per 10 minutes, as the
//! master's `date-offset.json` heartbeat does (#126): a few ms on a 1 ms loop, between windows.

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
    pub(super) path: Option<PathBuf>,
    /// The seed read at start (also after it was dropped); `None` = no usable file.
    pub(super) seed_ppm: Option<f64>,
    /// `/status.freq_seed_source`: `"file"`, `"none: <why>"` or `"rejected: <why>"`; empty until
    /// the file was opened.
    pub(super) source: String,
    /// Judging the seed (`Some` from the seed until the verdict).
    pub(super) judge: Option<SeedJudge>,
    /// The next save while LOCKED; `None` while not locked (set at the lock).
    pub(super) next_save: Option<Instant>,
    /// The servo's word (`applied_freq_ppm`) written at the end of the last window, and the one
    /// before it. A window's median sits mid-window, so a window's rate spans half of the
    /// previous window (under `word_before`) and half of this one (under `word_last`).
    pub(super) word_last: f64,
    pub(super) word_before: f64,
    /// A failed save is warned about once per process (a read-only root answers EROFS every 10
    /// minutes); later failures are logged at debug, a recovery at info.
    pub(super) save_failing: bool,
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// dsync#142: open the drift file at `path` and, when it holds a usable learned frequency,
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
        self.rate.drift_baseline_ppm = ppm;
        self.applied_freq_ppm = ppm;
        self.last_adj_ppm = ppm;
        self.freq_seed.seed_ppm = Some(ppm);
        self.freq_seed.source = "file".to_string();
        self.freq_seed.judge = Some(SeedJudge::default());
        self.freq_seed.word_last = ppm;
        self.freq_seed.word_before = ppm;
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

    /// dsync#142: a MEASURED window's raw rate (over `dt_secs`) while the seed is judged. The
    /// judge reads the rate the oscillator shows against the SEED itself: the raw rate minus what
    /// the word in force over this window's span (half the previous word, half the last one, see
    /// `FreqSeed::word_last`) differed from the seed, so the ACQ P term (gain 0.8) neither hides
    /// nor inflates a seed's error.
    ///
    /// Returns the amount to take off this window's raw rate: 0, except at a rejection, where the
    /// servo is put where a cold start would be — the integrator at 0, the lock count at 0, and
    /// this window's rate, the smoothed rate and the spike filter rebased to a word of 0 (they
    /// were measured against the seeded word; left as they are, the integrator's first steps went
    /// the wrong way).
    pub(super) fn judge_freq_seed(&mut self, raw_rate_ppm: f64, dt_secs: f64) -> f64 {
        let seed = self.freq_seed.seed_ppm.unwrap_or(0.0);
        let word_ppm = (self.freq_seed.word_before + self.freq_seed.word_last) / 2.0;
        let off_seed_ppm = raw_rate_ppm - (word_ppm - seed);
        let Some(judge) = self.freq_seed.judge.as_mut() else {
            return 0.0;
        };
        let Some(verdict) = judge.on_measured(off_seed_ppm, dt_secs) else {
            return 0.0;
        };
        self.freq_seed.judge = None;
        match verdict {
            SeedVerdict::Accepted { mean_rate_ppm } => {
                info!(
                    "[FREQ-SEED] accepted {:+.3}ppm: over the first {} measured windows the \
                     oscillator runs {:+.1}us/s off it",
                    seed, SEED_JUDGE_WINDOWS, mean_rate_ppm
                );
                0.0
            }
            SeedVerdict::Rejected { mean_rate_ppm } => {
                warn!(
                    "[FREQ-SEED] REJECTED {:+.3}ppm: over the first {} measured windows the \
                     oscillator runs {:+.1}us/s off it (beyond {}us/s; another oscillator?) — \
                     learning the frequency from 0",
                    seed,
                    SEED_JUDGE_WINDOWS,
                    mean_rate_ppm,
                    drift_file::SEED_REJECT_RATE_PPM
                );
                self.freq_seed.source = format!(
                    "rejected: over the first {} measured windows the oscillator ran {:+.1} us/s \
                     off the seed",
                    SEED_JUDGE_WINDOWS, mean_rate_ppm
                );
                self.rate.drift_baseline_ppm = 0.0;
                self.rate.lock_stable_count = 0;
                self.rate.smoothed_rate_ppm -= word_ppm;
                self.rate.spike_filter.clear();
                // Its history mixes rates against the seed with rates against 0: a false jitter.
                self.rate.jitter_estimator.clear();
                word_ppm
            }
        }
    }

    /// dsync#142: after each window — note the word it wrote (for the seed judge), and while
    /// LOCKED save the learned frequency every [`DRIFT_SAVE_INTERVAL`] (the first one an interval
    /// after the lock). dsync#146: "locked" is the REPORTED lock ([41]): under the phase lock
    /// the rate lock is held while engaged, so it no longer says the frequency has settled.
    pub(super) fn tick_drift_file(&mut self) {
        self.freq_seed.word_before = self.freq_seed.word_last;
        self.freq_seed.word_last = self.applied_freq_ppm;
        if !self.reported_lock() || self.ptp_offline || self.freq_seed.path.is_none() {
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

    /// dsync#142: at shutdown, save the learned frequency when LOCKED (else the file keeps
    /// the last locked value). `main` calls it after the sync loop ends.
    pub fn save_drift_file_at_shutdown(&mut self) {
        if self.reported_lock() && !self.ptp_offline {
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
            freq_ppm: self.rate.drift_baseline_ppm,
            written_unix_s: wall_now_ns().div_euclid(1_000_000_000),
        };
        match drift_file::save(&path, &record) {
            Ok(None) => {
                let recovered = std::mem::take(&mut self.freq_seed.save_failing);
                info!(
                    "[FREQ-SEED] saved {:+.3}ppm ({}) to {}{}",
                    record.freq_ppm,
                    why,
                    path.display(),
                    if recovered {
                        " (saving works again)"
                    } else {
                        ""
                    }
                );
            }
            Ok(Some(e)) => warn!(
                "[FREQ-SEED] saved {:+.3}ppm ({}), but syncing {}'s directory failed: {} (a power \
                 cut may bring the previous record back)",
                record.freq_ppm,
                why,
                path.display(),
                e
            ),
            Err(e) if self.freq_seed.save_failing => debug!(
                "[FREQ-SEED] could not save {:+.3}ppm ({}) to {}: {}",
                record.freq_ppm,
                why,
                path.display(),
                e
            ),
            Err(e) => {
                self.freq_seed.save_failing = true;
                warn!(
                    "[FREQ-SEED] could not save {:+.3}ppm ({}) to {}: {} — the next start learns \
                     the frequency from 0 (or from an older record); further failures are logged \
                     at debug",
                    record.freq_ppm,
                    why,
                    path.display(),
                    e
                );
            }
        }
    }

    /// dsync#142: the `/status` fields.
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
