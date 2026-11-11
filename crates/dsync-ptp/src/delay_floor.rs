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

/// A window minimum is CLIPPED to this many robust standard deviations (1.4826 MAD of the ring
/// around its line) above the floor for the phase lock's fits: a USB NIC still delays ~20 % of
/// window minima, by tens of µs to ms. Never below [`FIT_LATE_MIN_NS`]. A clip, never a hold:
/// two followers on one PC read the same stamp up to ns of residue, and a yes/no decision at the
/// gate's edge gave them different fits (the v1.30.0 release run, 3.3 µs apart).
pub const FIT_LATE_SIGMAS: f64 = 3.0;
/// The late gate's floor (a noiseless ring has a MAD of 0).
pub const FIT_LATE_MIN_NS: i64 = 5_000;

/// The newest window's own minimum, for the phase lock's open-loop fits (dsync#160): unlike the
/// floor (a line through ~10 windows, correlated from window to window) it is white, which the
/// fits' σ assumes. `ns` is clipped at the late gate above the floor (continuous in the input);
/// `late` says it was (for logs and tests only, never for a decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FitSample {
    pub ns: i64,
    pub late: bool,
}

/// Sorts a few values (≤ 45, every window of every box) in place; `gt(a, b)` = `a` after `b`.
/// The std sort's debug-build checks made it ~70 % of the two-clock bench's time (callgrind), and
/// the bench runs in debug in CI.
fn insertion_sort_by<T: Copy>(v: &mut [T], gt: impl Fn(&T, &T) -> bool) {
    for i in 1..v.len() {
        let x = v[i];
        let mut j = i;
        while j > 0 && gt(&v[j - 1], &x) {
            v[j] = v[j - 1];
            j -= 1;
        }
        v[j] = x;
    }
}

/// The `k`-th smallest of `v` (finite values), with every smaller one moved before it
/// (Hoare's quickselect, middle pivot): ~3 n comparisons where a sort of the 45 slopes took ~500.
fn select_nth(v: &mut [f64], k: usize) -> f64 {
    let (mut lo, mut hi) = (0, v.len() - 1);
    while lo < hi {
        let pivot = v[lo + (hi - lo) / 2];
        let (mut i, mut j) = (lo, hi);
        loop {
            while v[i] < pivot {
                i += 1;
            }
            while v[j] > pivot {
                j -= 1;
            }
            if i >= j {
                break;
            }
            v.swap(i, j);
            i += 1;
            j -= 1;
        }
        if k <= j {
            hi = j;
        } else {
            lo = j + 1;
        }
    }
    v[k]
}

/// The last [`FLOOR_RING_WINDOWS`] window minima of the raw `t2 − t1` (ns) with their `t1`.
#[derive(Debug, Clone, Default)]
pub struct DelayFloor {
    ring: VecDeque<(i64, i64)>,
    /// The ring's Theil-Sen slope, recomputed once per ring change (it is read several times a
    /// window: the reset check, the floor, the fit sample).
    slope: f64,
}

impl DelayFloor {
    pub fn new() -> Self {
        DelayFloor {
            ring: VecDeque::with_capacity(FLOOR_RING_WINDOWS),
            slope: 0.0,
        }
    }

    /// dsync#170: a window closed with these samples `(t1, raw t2 − t1)`: its least-delayed
    /// sample, every sample first carried along the floor's slope to `read_t1_ns`, enters the
    /// ring AT `read_t1_ns`. Returns the floor there ([`Self::push_window`]), `None` for no
    /// sample.
    ///
    /// The point is continuous in every stamp. Taking the least raw sample at ITS own `t1` was
    /// not: two samples a few ns apart are 125 ms apart in `t1`, so which one is "least" moved
    /// the point by the slope × 125 ms (~0.5 µs while the rate servo acquires), and two
    /// followers on one PC (the same stamps ± ns of conversion residue) took different points.
    /// Carried along the slope, near-equal samples give near-equal points at one fixed time.
    pub fn push_samples(
        &mut self,
        samples: impl IntoIterator<Item = (i64, i64)>,
        read_t1_ns: i64,
    ) -> Option<i64> {
        let slope = self.slope;
        let least = samples
            .into_iter()
            .map(|(t1, raw)| {
                raw.wrapping_add((slope * read_t1_ns.wrapping_sub(t1) as f64).round() as i64)
            })
            .min()?;
        Some(self.push_window(read_t1_ns, least, read_t1_ns))
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
        self.slope = self.theil_sen_slope();
        self.floor_at(read_t1_ns)
            .expect("the ring holds the window just pushed")
    }

    /// The robust slope of the ring (ns of offset per ns of grandmaster time), 0 below
    /// [`FLOOR_SLOPE_MIN_WINDOWS`].
    pub fn slope(&self) -> f64 {
        self.slope
    }

    fn theil_sen_slope(&self) -> f64 {
        if self.ring.len() < FLOOR_SLOPE_MIN_WINDOWS {
            return 0.0;
        }
        // Allocation-free, at most 45 pairs: it runs every window of every box.
        let mut slopes = [0.0f64; FLOOR_RING_WINDOWS * (FLOOR_RING_WINDOWS - 1) / 2];
        let mut n = 0;
        for (i, &(ti, yi)) in self.ring.iter().enumerate() {
            for &(tj, yj) in self.ring.iter().skip(i + 1) {
                let dt = tj.wrapping_sub(ti);
                if dt > 0 {
                    slopes[n] = yj.wrapping_sub(yi) as f64 / dt as f64;
                    n += 1;
                }
            }
        }
        if n == 0 {
            return 0.0;
        }
        // The median by selection (the slopes are finite: dt > 0), not a full sort.
        let slopes = &mut slopes[..n];
        let upper = select_nth(slopes, n / 2);
        if n % 2 == 1 {
            upper
        } else {
            // Below index n / 2 lie the lower half: its largest is the other middle value.
            let lower = slopes[..n / 2]
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max);
            (lower + upper) / 2.0
        }
    }

    /// The ring's minima carried to `t1_ns` along the slope, sorted, and how many there are.
    fn carried_sorted(&self, t1_ns: i64) -> ([i64; FLOOR_RING_WINDOWS], usize) {
        let mut carried = [0i64; FLOOR_RING_WINDOWS];
        let n = self.ring.len();
        for (c, &(t, y)) in carried.iter_mut().zip(self.ring.iter()) {
            *c = y.wrapping_add((self.slope * t1_ns.wrapping_sub(t) as f64).round() as i64);
        }
        insertion_sort_by(&mut carried[..n], |a, b| a > b);
        (carried, n)
    }

    /// The floor of `carried[..n]` (sorted): the median, the least below
    /// [`FLOOR_SLOPE_MIN_WINDOWS`].
    fn floor_of(carried: &[i64]) -> i64 {
        let n = carried.len();
        if n < FLOOR_SLOPE_MIN_WINDOWS {
            // Too few windows for a median to outvote a late one: the least of them.
            return carried[0];
        }
        if n % 2 == 1 {
            carried[n / 2]
        } else {
            carried[n / 2 - 1] + (carried[n / 2] - carried[n / 2 - 1]) / 2
        }
    }

    /// The floor at grandmaster time `t1_ns`: the median of the window minima carried to `t1_ns`
    /// along the ring's slope. `None` when empty.
    pub fn floor_at(&self, t1_ns: i64) -> Option<i64> {
        if self.ring.is_empty() {
            return None;
        }
        let (carried, n) = self.carried_sorted(t1_ns);
        Some(Self::floor_of(&carried[..n]))
    }

    /// Every earlier window is invalid (a time-base change, an outage, a dropped measurement).
    pub fn clear(&mut self) {
        self.ring.clear();
        self.slope = 0.0;
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
        let (carried, n) = self.carried_sorted(t);
        let floor = Self::floor_of(&carried[..n]);
        let mut dev = [0i64; FLOOR_RING_WINDOWS];
        for (d, c) in dev.iter_mut().zip(&carried[..n]) {
            *d = c.wrapping_sub(floor).saturating_abs();
        }
        insertion_sort_by(&mut dev[..n], |a, b| a > b);
        let mad = dev[n / 2] as f64;
        let gate = ((FIT_LATE_SIGMAS * 1.4826 * mad).round() as i64).max(FIT_LATE_MIN_NS);
        let cap = floor.saturating_add(gate);
        Some(FitSample {
            ns: y.min(cap),
            late: y > cap,
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

    /// dsync#170: a floor whose ring holds a ramp of `slope` ns/ns (window minima 1 s apart).
    fn ramped(slope: f64) -> DelayFloor {
        let mut f = DelayFloor::default();
        for k in 0..12_i64 {
            let y = 7_000 + (slope * (k * S) as f64).round() as i64;
            f.push_window(k * S, y, k * S);
        }
        assert!((f.slope() - slope).abs() < 1e-9, "slope {}", f.slope());
        f
    }

    /// dsync#170: the window's point is continuous in its samples. Two near-equal raw samples
    /// 125 ms apart (the slope is 20 ppm: 2.5 µs over them): whichever is "least" by a ns, the
    /// point the window adds to the ring, carried along the slope to the window's middle, moves
    /// by about that ns. The argmin at its own `t1` (`push_window`) put the point 125 ms apart
    /// on the time axis: 2.5 µs apart along the slope.
    #[test]
    fn the_window_point_is_continuous_in_the_samples_170() {
        let slope = 20e-6;
        let read = 12 * S + 250_000_000;
        let raw = 7_000 + (slope * (12 * S) as f64).round() as i64;
        let (t_a, t_b) = (12 * S + 125_000_000, 12 * S + 250_000_000);
        let at_read = |f: &DelayFloor| {
            let &(t, y) = f.ring.back().expect("pushed");
            y + (slope * (read - t) as f64).round() as i64
        };
        let (mut a, mut b) = (ramped(slope), ramped(slope));
        a.push_samples([(t_a, raw + 1), (t_b, raw)], read);
        b.push_samples([(t_a, raw), (t_b, raw + 1)], read);
        assert!(
            (at_read(&a) - at_read(&b)).abs() <= 2,
            "points {} vs {}",
            at_read(&a),
            at_read(&b)
        );
        let (mut c, mut d) = (ramped(slope), ramped(slope));
        c.push_window(t_b, raw, read);
        d.push_window(t_a, raw, read);
        assert!(
            (at_read(&c) - at_read(&d)).abs() >= 2_000,
            "the argmin's point moved {} ns",
            at_read(&c) - at_read(&d)
        );
    }

    /// dsync#170: one sample is carried along the slope to the read time; none is no point.
    #[test]
    fn one_sample_is_carried_to_the_read_time_and_none_is_no_point_170() {
        let slope = 20e-6;
        let mut f = ramped(slope);
        let t1 = 12 * S;
        let read = t1 + 500_000_000;
        let raw = 7_000 + (slope * t1 as f64).round() as i64;
        let mut g = f.clone();
        assert_eq!(
            f.push_samples([(t1, raw)], read),
            Some(g.push_window(read, raw + 10_000, read))
        );
        assert_eq!(f.push_samples([], read), None);
    }

    /// The selection gives the sorted order's k-th value with every smaller one before it, for
    /// every k and length (duplicates included): the slope's median relies on both.
    #[test]
    fn select_nth_matches_a_sort_160() {
        let mut x: u64 = 0x5eed_0160;
        for n in 1..=45 {
            for _ in 0..20 {
                let v: Vec<f64> = (0..n)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        (x % 7) as f64 - 3.0 + (x % 1_000) as f64 * 1e-3
                    })
                    .collect();
                let mut sorted = v.clone();
                sorted.sort_by(f64::total_cmp);
                for k in 0..n {
                    let mut w = v.clone();
                    assert_eq!(select_nth(&mut w, k), sorted[k], "n {n} k {k}");
                    assert!(w[..k].iter().all(|&y| y <= sorted[k]), "n {n} k {k}");
                }
            }
        }
        let mut few = [3i64, -1, 2, 2, 0];
        insertion_sort_by(&mut few, |a, b| a > b);
        assert_eq!(few, [-1, 0, 2, 2, 3]);
    }

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
