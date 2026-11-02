//! dantesync#146 — the controller glue of the reported lock (DSYX [41], `/status.is_locked`) under
//! the PTP phase lock, and the decoupling that keeps the phase lock's own corrections from reading
//! as a lock loss to the rate servo.
//!
//! **The decoupling (the rules' standing requirement for a second loop on this clock, the #97
//! shape).** Once the phase lock owns the frequency word, the rate servo still measures
//! `d(offset)/dt` — and that is now the phase lock's DELIBERATE deviation from the learned
//! frequency: its P term and, on the first engagement, the boot pull's `−r/τ` (35 ppm for 0.7 ms
//! on gen3). Read as a rate, it decays the rate lock, the phase lock disengages and DROPS the boot
//! pull; the plain ~100 s PI then retires the rest over minutes (the gen3 boots, gate run
//! 41372498483). So every measured rate is first decoupled: `rate − (word − integrator)`, averaged
//! over the two words in force across the window (a median sits mid-window, the #142 judge's
//! rule). What is left is the frequency the phase lock has NOT learned, the honest rate evidence.
//! Without an engagement both deviations are 0 and the rate servo is unchanged, bit for bit.

use super::*;
use crate::lock_gate::{GateEvent, LOCK_BOUND_NS, UNLOCK_BOUND_NS, UNLOCK_WINDOWS};

impl DateSync {
    /// The phase lock wrote `word`: remember its deviation from the learned frequency (0 when the
    /// rate servo's word applies).
    pub(super) fn note_word_deviation(&mut self, engaged_word: Option<f64>) {
        let deviation = engaged_word.map_or(0.0, |w| w - self.core.integrator_ppm());
        self.word_deviation_ppm = [self.word_deviation_ppm[1], deviation];
    }

    /// The deviation in force over the last measured span: the average of the last two words'.
    pub(super) fn span_deviation_ppm(&self) -> f64 {
        (self.word_deviation_ppm[0] + self.word_deviation_ppm[1]) / 2.0
    }
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// The rate servo's measured rate with the phase lock's own deviation removed (a no-op under
    /// the legacy discipline and while the rate servo owns the word).
    pub(in crate::controller) fn decouple_phase_lock_rate(&self, raw_rate_ppm: f64) -> f64 {
        if self.date_sync.enabled {
            raw_rate_ppm - self.date_sync.span_deviation_ppm()
        } else {
            raw_rate_ppm
        }
    }

    /// The lock this node REPORTS: under the phase lock the [`crate::lock_gate`] verdict; under
    /// the legacy discipline the rate servo's lock, as before. PTP staleness is applied by the
    /// publisher.
    pub(in crate::controller) fn reported_lock(&self) -> bool {
        if self.date_sync.enabled {
            self.date_sync.lock_gate.locked()
        } else {
            self.is_locked
        }
    }

    /// One phase-lock window: the gate's verdict on `e`, logged on a change.
    pub(super) fn judge_reported_lock(&mut self, error_ns: Option<i64>) {
        let engaged = self.date_sync.core.engaged();
        let event = self.date_sync.lock_gate.on_window(engaged, error_ns);
        log_gate_event(event);
    }

    /// Tests about something else (staleness, the re-join) that need a node REPORTING its lock:
    /// the phase lock engaged (anchored at 0 if it was not yet) and within the bound for the lock
    /// count.
    #[cfg(test)]
    pub(in crate::controller) fn lock_reported_for_test(&mut self) {
        if !self.date_sync.core.engaged() {
            self.date_sync
                .core
                .on_window(0, true, self.drift_baseline_ppm, 0.5);
        }
        for _ in 0..crate::lock_gate::LOCK_WINDOWS {
            self.judge_reported_lock(Some(0));
        }
    }

    /// The phase lock disengaged outside a window (the PTP-offline edge): [41] clears at once.
    pub(super) fn clear_reported_lock(&mut self) {
        self.date_sync.word_deviation_ppm = [0.0; 2];
        let event = self.date_sync.lock_gate.reset();
        log_gate_event(event);
    }
}

fn log_gate_event(event: Option<GateEvent>) {
    match event {
        None => {}
        Some(GateEvent::Locked { error_ns }) => info!(
            "[LOCK] === LOCKED === DSYX [41] = 1: phase error {:+.1}us, within {}us for the \
             last windows",
            error_ns as f64 / 1_000.0,
            LOCK_BOUND_NS / 1_000
        ),
        Some(GateEvent::PhaseLost { error_ns }) => warn!(
            "[LOCK] === UNLOCKED === DSYX [41] = 0: phase error {:+.1}us, beyond {}us for {} \
             windows",
            error_ns as f64 / 1_000.0,
            UNLOCK_BOUND_NS / 1_000,
            UNLOCK_WINDOWS
        ),
        Some(GateEvent::Disengaged) => warn!(
            "[LOCK] === UNLOCKED === DSYX [41] = 0: the phase lock no longer owns the frequency"
        ),
    }
}

#[cfg(test)]
mod tests;
