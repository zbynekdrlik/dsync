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

/// dantesync#146: an engaged phase lock whose [41] has been 0 this long is a lost clock for the
/// #114 alarm. Longer than a cold boot's pull (30-55 s) and a 25 ppm grandmaster frequency step's
/// drop (~60 s), so neither alarms; a reference the phase lock cannot hold (a grandmaster sending
/// garbage, a sub-minimum frequency step that keeps `e` past 150 µs for minutes) does.
pub(in crate::controller) const PHASE_UNSAFE_ALARM_AFTER: Duration = Duration::from_secs(120);

impl DateSync {
    /// The phase lock wrote `word`: remember its deviation from the learned frequency (0 when the
    /// rate servo's word applies).
    pub(super) fn note_word_deviation(&mut self, engaged_word: Option<f64>) {
        let integrator = self.core.integrator_ppm();
        self.word_deviation.note(engaged_word, integrator);
    }

    /// The deviation in force over the last measured span: the average of the last two words'.
    pub(super) fn span_deviation_ppm(&self) -> f64 {
        self.word_deviation.span_ppm()
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

    /// True while the phase lock owns the frequency word: the rate servo's unstable window then
    /// does not count toward its UNLOCK, so it never disengages the phase lock. Even decoupled,
    /// the rate it measures is the frequency the phase lock has not learned YET, which the phase
    /// lock is retiring itself (its PI, a boot re-seed, a followed frequency step). Read as a lock
    /// loss it drained a fresh lock count in a few windows: a seed 7-15 ppm off (kept by the #142
    /// judge) disengaged the phase lock 7 s after [41] was set (review of #146). Engaged, only the
    /// PTP-offline edge disengages it (`on_ptp_offline_edge`); what [41] reports is the phase.
    pub(in crate::controller) fn phase_lock_holds_rate_lock(&self) -> bool {
        self.date_sync.enabled && self.date_sync.core.engaged()
    }

    /// The lock this node REPORTS: under the phase lock the [`crate::lock_gate`] verdict; under
    /// the legacy discipline the rate servo's lock, as before. PTP staleness is applied by the
    /// publisher.
    pub(in crate::controller) fn reported_lock(&self) -> bool {
        if self.date_sync.enabled {
            self.date_sync.lock_gate.locked()
        } else {
            self.rate.is_locked
        }
    }

    /// One phase-lock window's verdict on `e`, outside the window path (the tests' helpers; the
    /// window path is `phase_window::close_window`).
    #[cfg(test)]
    pub(super) fn judge_reported_lock(&mut self, error_ns: Option<i64>) {
        let event = crate::phase_window::judge(
            &mut self.date_sync.lock_gate,
            &self.date_sync.core,
            error_ns,
        );
        self.after_reported_lock_judged(event);
    }

    /// The gate judged a window (`crate::phase_window`): log its change, and track since when an
    /// engaged phase lock has not held [41] (the clock alarm).
    pub(super) fn after_reported_lock_judged(&mut self, event: Option<GateEvent>) {
        let engaged = self.date_sync.core.engaged();
        log_gate_event(event);
        self.date_sync.unsafe_since = match (engaged, self.date_sync.lock_gate.locked()) {
            (true, false) => self.date_sync.unsafe_since.or(Some(Instant::now())),
            _ => None,
        };
    }

    /// dantesync#146: what the #114 clock alarm reads as "locked". Under the phase lock the rate
    /// lock is held while engaged, so it alone would never say a broken reference is lost: an
    /// engaged phase lock that has not held [41] for [`PHASE_UNSAFE_ALARM_AFTER`] is not locked.
    pub(in crate::controller) fn alarm_sees_lock(&self) -> bool {
        self.rate.is_locked
            && self
                .date_sync
                .unsafe_since
                .map_or(true, |t| t.elapsed() < PHASE_UNSAFE_ALARM_AFTER)
    }

    /// Tests about something else (staleness, the re-join) that need a node REPORTING its lock:
    /// the phase lock engaged (anchored at 0 if it was not yet) and within the bound for the lock
    /// count.
    #[cfg(test)]
    pub(in crate::controller) fn lock_reported_for_test(&mut self) {
        // Only for a fresh core, one already engaged, or one this helper anchored itself (at 0):
        // the window below bypasses the controller's anchor-event handling, so on a core
        // anchored elsewhere it could re-anchor unseen.
        assert!(
            self.date_sync.core.engaged()
                || matches!(self.date_sync.core.anchor_ns(), None | Some(0)),
            "lock_reported_for_test on an anchored, disengaged core"
        );
        if !self.date_sync.core.engaged() {
            self.date_sync
                .core
                .on_window(0, true, self.rate.drift_baseline_ppm, 0.5);
        }
        for _ in 0..crate::lock_gate::LOCK_WINDOWS {
            self.judge_reported_lock(Some(0));
        }
    }

    /// [41] cleared outside a window (the tests' helper; the PTP-offline edge runs
    /// `phase_window::on_outage`).
    #[cfg(test)]
    pub(in crate::controller) fn clear_reported_lock(&mut self) {
        self.date_sync.word_deviation.clear();
        self.date_sync.unsafe_since = None;
        let event = self.date_sync.lock_gate.reset();
        log_gate_event(event);
    }
}

pub(super) fn log_gate_event(event: Option<GateEvent>) {
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
