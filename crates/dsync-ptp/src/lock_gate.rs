//! dsync#146 — the REPORTED lock under the PTP phase lock: DSYX byte [41], `/status.is_locked`,
//! the tray and the IPC (all of them read `/status.is_locked`, one definition).
//!
//! # The contract (ROZHODNUTÉ on #146, dstack confirmed 2026-11-02)
//!
//! [41] = 1 means the clock is safe for a 250 µs Dante flow. The stack gates its media-clock ready
//! on [41] alone; while it runs, [41] = 0 is a holdover (not a mute), and before its first ready
//! the stack holds its TX back. So:
//!
//! - **Lock:** the phase lock owns the frequency word and `|e| ≤` [`LOCK_BOUND_NS`] (50 µs, the
//!   project precision target) for [`LOCK_WINDOWS`] consecutive windows (1.5 s: one lucky window
//!   is not a lock). `e = (t2 − t1) − D` is `/status.ptp_phase_error_us`.
//! - **Unlock:** `|e| >` [`UNLOCK_BOUND_NS`] (150 µs) for [`UNLOCK_WINDOWS`] consecutive windows
//!   (2 s), or the phase lock disengaged (a rate-lock loss, the PTP-offline edge). PTP staleness
//!   clears the published value on its own (`is_locked && !stale`, #112).
//!
//! # Why these bounds (measured, `controller/date_sync/lock_sim.rs`)
//!
//! The steady `e` of a 0.5 s window has a standard deviation of 6 µs (max 25) at 10 µs per-sample
//! jitter and 18 µs (max 74) at a pessimistic 30 µs: 150 µs is 8 σ there, so noise and one bad
//! window never clear [41], and the hysteresis between 50 and 150 µs means it never chatters. A
//! grandmaster FREQUENCY step (followed, camera-box issue 1372) moves `e` to about −450 µs
//! (25 ppm) or +180 µs (10 ppm): past 150 µs for 4 windows [41] drops ONCE and returns once `e` is
//! back — a real loss (the 250 µs flow is late), not a flap. A peak just past 150 µs may not drop
//! it (the 10 ppm step at 30 µs jitter did not: never 4 windows in a row). A date step leaves `e` unchanged (`D` moves with the wall), and its
//! grace has no windows, so it never moves [41].
//!
//! Pure (explicit inputs, no I/O, no logging): the controller logs the transitions it returns.

/// Lock: `|e|` at or under this (ns) …
pub const LOCK_BOUND_NS: i64 = 50_000;
/// … for this many consecutive engaged windows.
pub const LOCK_WINDOWS: usize = 3;
/// Unlock: `|e|` beyond this (ns) …
pub const UNLOCK_BOUND_NS: i64 = 150_000;
/// … for this many consecutive windows.
pub const UNLOCK_WINDOWS: usize = 4;

const _: () = assert!(UNLOCK_BOUND_NS > LOCK_BOUND_NS, "hysteresis");

/// Why the reported lock changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateEvent {
    /// Set: the last [`LOCK_WINDOWS`] windows were engaged and within [`LOCK_BOUND_NS`].
    Locked { error_ns: i64 },
    /// Cleared: [`UNLOCK_WINDOWS`] windows beyond [`UNLOCK_BOUND_NS`].
    PhaseLost { error_ns: i64 },
    /// Cleared: the phase lock no longer owns the word.
    Disengaged,
}

/// The reported-lock state of one box.
#[derive(Clone, Debug, Default)]
pub struct PhaseLockGate {
    locked: bool,
    /// Consecutive engaged windows within the lock bound (while unlocked).
    inside: usize,
    /// Consecutive windows beyond the unlock bound (while locked).
    outside: usize,
}

impl PhaseLockGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The reported lock ([41]).
    pub fn locked(&self) -> bool {
        self.locked
    }

    /// One phase-lock window: `engaged` = the phase lock owns the word after it, `error_ns` = its
    /// `e` (`None` before the anchor).
    pub fn on_window(&mut self, engaged: bool, error_ns: Option<i64>) -> Option<GateEvent> {
        let Some(e) = error_ns.filter(|_| engaged) else {
            return self.reset();
        };
        let off = e.unsigned_abs();
        if self.locked {
            if off > UNLOCK_BOUND_NS as u64 {
                self.outside += 1;
                if self.outside >= UNLOCK_WINDOWS {
                    self.locked = false;
                    self.outside = 0;
                    self.inside = 0;
                    return Some(GateEvent::PhaseLost { error_ns: e });
                }
            } else {
                self.outside = 0;
            }
        } else if off <= LOCK_BOUND_NS as u64 {
            self.inside += 1;
            if self.inside >= LOCK_WINDOWS {
                self.locked = true;
                self.inside = 0;
                self.outside = 0;
                return Some(GateEvent::Locked { error_ns: e });
            }
        } else {
            self.inside = 0;
        }
        None
    }

    /// The phase lock disengaged (or PTP went offline): unlocked, every count from scratch.
    pub fn reset(&mut self) -> Option<GateEvent> {
        let was = self.locked;
        *self = Self::default();
        was.then_some(GateEvent::Disengaged)
    }
}

#[cfg(test)]
mod tests;
