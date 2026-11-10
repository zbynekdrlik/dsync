//! dsync#160 — the phase measurement is the delay FLOOR of the last ~10 s, not a window median.
//!
//! A software-stamped receive delay is one-sided: queuing, interrupt moderation and a USB NIC's
//! bulk transfers only ever ADD to it. mbc-snv's ASIX USB Dante NIC (10.11.2026, 2365 samples)
//! showed the per-10 s minimum of the raw `t2 − t1` on one straight line (sd 7.6 µs) while the
//! per-10 s median scattered (sd 70 µs) and single samples ran up to +4.6 ms late. A 4-sample
//! window median follows such a delay; the floor of a ~10 s ring does not.
//!
//! The ring holds each window's MINIMUM with its grandmaster time `t1`; a window of 4 almost
//! always holds one sample near the floor. The floor is the Theil-Sen line through those minima
//! read at the newest window: the slope is the median of the pairwise slopes, the level the
//! median of the minima carried to the newest `t1` along it. Both medians ignore the ~20 % of
//! window minima a USB NIC still delays.
//!
//! Why a line and not the least carried minimum: the raw offset drifts while the rate is being
//! learned (and after a grandmaster frequency step), so a plain sliding minimum lags a ramp by up
//! to the ring's length; and the least of N noisy values sinks as N grows, so a filling ring
//! would read as a rate (−1 ppm over the first 10 s at 25 µs of jitter). A median does neither.
//! The follower (`follower::core`) and the service (`controller::date_sync`) both feed it at
//! their window close.

use std::collections::VecDeque;

/// Window minima in the ring: ~10 s at one window a second.
pub const FLOOR_RING_WINDOWS: usize = 10;

/// The ring's slope and median need this many windows; fewer read the least window minimum,
/// uncarried (a two-point slope or median has no robustness, and over ≤ 2 s the drift is a few
/// µs at most).
pub const FLOOR_SLOPE_MIN_WINDOWS: usize = 3;

/// A window this far off the ring's floor, or with a `t1` not after the newest one, starts a
/// new ring: another time base (a grandmaster reboot restarts its uptime under the same UUID).
/// The phase lock's own discontinuity bound; a medians-of-10 ring would otherwise keep the old
/// base's level for ~5 windows and hide the jump the core re-anchors on.
pub const FLOOR_RESET_NS: i64 = crate::ptp_phase_lock::DISCONTINUITY_NS;

/// A window minimum this many robust standard deviations (1.4826 MAD of the ring around its
/// line) above the floor is a LATE one for the phase lock's fits: a USB NIC still delays ~20 % of
/// window minima, by tens of µs to ms. Never below [`FIT_LATE_MIN_NS`].
pub const FIT_LATE_SIGMAS: f64 = 3.0;
/// The late gate's floor (a noiseless ring has a MAD of 0).
pub const FIT_LATE_MIN_NS: i64 = 5_000;

/// The newest window's own minimum, for the phase lock's open-loop fits (dsync#160): unlike the
/// floor (a line through ~10 windows, correlated from window to window) it is white, which the
/// fits' σ assumes. `late` when it sits beyond the late gate above the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FitSample {
    pub ns: i64,
    pub late: bool,
}

/// The last [`FLOOR_RING_WINDOWS`] window minima of the raw `t2 − t1` (ns) with their `t1`.
#[derive(Debug, Clone, Default)]
pub struct DelayFloor {
    ring: VecDeque<(i64, i64)>,
}

impl DelayFloor {
    pub fn new() -> Self {
        DelayFloor {
            ring: VecDeque::with_capacity(FLOOR_RING_WINDOWS),
        }
    }

    /// A window closed: its least-delayed sample `min_ns`, taken at grandmaster time `min_t1_ns`.
    /// Returns the floor at `read_t1_ns` (this window included) — the window's middle, where the
    /// median it replaces sat (the boot fit and the #142 judge integrate the word to there).
    pub fn push_window(&mut self, min_t1_ns: i64, min_ns: i64, read_t1_ns: i64) -> i64 {
        let new_base = self.ring.back().is_some_and(|&(t, _)| min_t1_ns <= t)
            || self
                .floor_at(min_t1_ns)
                .is_some_and(|f| min_ns.wrapping_sub(f).saturating_abs() > FLOOR_RESET_NS);
        if new_base {
            self.ring.clear();
        }
        if self.ring.len() == FLOOR_RING_WINDOWS {
            self.ring.pop_front();
        }
        self.ring.push_back((min_t1_ns, min_ns));
        self.floor_at(read_t1_ns)
            .expect("the ring holds the window just pushed")
    }

    /// The robust slope of the ring (ns of offset per ns of grandmaster time), 0 below
    /// [`FLOOR_SLOPE_MIN_WINDOWS`].
    pub fn slope(&self) -> f64 {
        if self.ring.len() < FLOOR_SLOPE_MIN_WINDOWS {
            return 0.0;
        }
        let mut slopes = Vec::with_capacity(self.ring.len() * (self.ring.len() - 1) / 2);
        for (i, &(ti, yi)) in self.ring.iter().enumerate() {
            for &(tj, yj) in self.ring.iter().skip(i + 1) {
                let dt = tj.wrapping_sub(ti);
                if dt > 0 {
                    slopes.push(yj.wrapping_sub(yi) as f64 / dt as f64);
                }
            }
        }
        if slopes.is_empty() {
            return 0.0;
        }
        slopes.sort_unstable_by(f64::total_cmp);
        let n = slopes.len();
        if n % 2 == 1 {
            slopes[n / 2]
        } else {
            (slopes[n / 2 - 1] + slopes[n / 2]) / 2.0
        }
    }

    /// The floor at grandmaster time `t1_ns`: the median of the window minima carried to `t1_ns`
    /// along the ring's slope. `None` when empty.
    pub fn floor_at(&self, t1_ns: i64) -> Option<i64> {
        let slope = self.slope();
        let mut carried: Vec<i64> = self
            .ring
            .iter()
            .map(|&(t, y)| y.wrapping_add((slope * t1_ns.wrapping_sub(t) as f64).round() as i64))
            .collect();
        if carried.is_empty() {
            return None;
        }
        carried.sort_unstable();
        let n = carried.len();
        if n < FLOOR_SLOPE_MIN_WINDOWS {
            // Too few windows for a median to outvote a late one: the least of them.
            return Some(carried[0]);
        }
        Some(if n % 2 == 1 {
            carried[n / 2]
        } else {
            carried[n / 2 - 1] + (carried[n / 2] - carried[n / 2 - 1]) / 2
        })
    }

    /// Every earlier window is invalid (a time-base change, an outage, a dropped measurement).
    pub fn clear(&mut self) {
        self.ring.clear();
    }

    /// The offset's base moved by `delta_ns` (a slew folded into the date offset): every window
    /// moves with it, as the open window's own samples do.
    pub fn shift(&mut self, delta_ns: i64) {
        for (_, y) in self.ring.iter_mut() {
            *y = y.wrapping_add(delta_ns);
        }
    }

    /// The newest window's minimum as pushed (and shifted since), judged against the floor: see
    /// [`FitSample`]. Below [`FLOOR_SLOPE_MIN_WINDOWS`] nothing is late (no robust scale yet).
    pub fn fit_sample(&self) -> Option<FitSample> {
        let &(t, y) = self.ring.back()?;
        if self.ring.len() < FLOOR_SLOPE_MIN_WINDOWS {
            return Some(FitSample { ns: y, late: false });
        }
        let slope = self.slope();
        let mut carried: Vec<i64> = self
            .ring
            .iter()
            .map(|&(ti, yi)| yi.wrapping_add((slope * t.wrapping_sub(ti) as f64).round() as i64))
            .collect();
        let floor = self.floor_at(t).expect("the ring is not empty");
        let mut dev: Vec<i64> = carried
            .iter_mut()
            .map(|c| c.wrapping_sub(floor).saturating_abs())
            .collect();
        dev.sort_unstable();
        let mad = dev[dev.len() / 2] as f64;
        let gate = ((FIT_LATE_SIGMAS * 1.4826 * mad).round() as i64).max(FIT_LATE_MIN_NS);
        Some(FitSample {
            ns: y,
            late: y.wrapping_sub(floor) > gate,
        })
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    #[test]
    fn the_floor_ignores_late_windows_and_ages_out_160() {
        let mut f = DelayFloor::new();
        assert_eq!(f.floor_at(0), None);
        assert_eq!(f.push_window(0, 500, 0), 500);
        assert_eq!(f.push_window(S, 100, S), 100, "two windows: the lesser");
        for k in 2..=(FLOOR_RING_WINDOWS as i64 + 1) {
            // a flat floor of 1000 with late windows in between: the slope stays 0
            let y = if k % 3 == 0 { 400_000 } else { 1_000 };
            f.push_window(k * S, y, k * S);
        }
        assert_eq!(f.len(), FLOOR_RING_WINDOWS);
        let now = (FLOOR_RING_WINDOWS as i64 + 1) * S;
        assert_eq!(f.floor_at(now), Some(1_000));
        f.shift(-50);
        assert_eq!(f.floor_at(now), Some(950));
        f.clear();
        assert!(f.is_empty());
        assert_eq!(f.floor_at(0), None);
    }

    /// A grandmaster reboot under the same UUID: `t1` restarts and the offset jumps by its
    /// uptime. The next window alone is the floor, so the core sees the jump at once.
    #[test]
    fn another_time_base_starts_a_new_ring_160() {
        let mut f = DelayFloor::new();
        for k in 1..=5 {
            f.push_window(1_000 * S + k * S, 7_000, 1_000 * S + k * S);
        }
        assert_eq!(
            f.push_window(2 * S, 900 * S + 5_000, 2 * S),
            900 * S + 5_000
        );
        assert_eq!(f.len(), 1);
        // A jump beyond the bound with t1 still increasing: a new ring too.
        assert_eq!(f.push_window(3 * S, 5_000 - 2 * S, 3 * S), 5_000 - 2 * S);
        assert_eq!(f.len(), 1);
        // A late window (ms, not s) stays in the ring.
        f.push_window(4 * S, 5_000 - 2 * S + 4_000_000, 4 * S);
        assert_eq!(f.len(), 2);
    }

    /// A ramp (the rate not yet learned) is carried, not lagged: the floor at the newest window
    /// is the ramp there, with a fifth of the windows late by up to 1 ms.
    #[test]
    fn a_ramping_floor_is_read_at_the_newest_window_despite_late_windows_160() {
        let mut f = DelayFloor::new();
        let ramp = |k: i64| 25 * k * 1_000; // 25 ppm: 25 µs per 1 s window
        let mut last = 0;
        for k in 0..30 {
            let late = if k % 5 == 2 { 1_000_000 } else { 0 };
            last = f.push_window(k * S, 7_000 + ramp(k) + late, k * S);
            if k >= 3 {
                assert!(
                    (last - (7_000 + ramp(k))).abs() <= 1,
                    "window {k}: floor {last}"
                );
            }
        }
        assert!((f.slope() - 25e-6).abs() < 1e-9, "slope {}", f.slope());
        assert_eq!(last, 7_000 + ramp(29));
    }
}
