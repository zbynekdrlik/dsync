//! dsync#204 — a sustained phase excursion of an UNLEARNED frequency boosts the PI gains.
//!
//! The plain PI (~100 s, critically damped) leaves a frequency RAMP `ρ` (ppm/s) a standing phase
//! error of `ρ / K_I`. A load that warms the local crystal moves it by a few ppm over minutes:
//! resolume-pp (19.11.2026) went −3.8 → −9.6 ppm and read +95 µs for minutes while still in LOCK.
//! The grandmaster-change boost (`K_P × 4`, `K_I × 16`, still ζ = 1, τ 25 s) cuts that error 16×,
//! but only a time-base change armed it. Here the same boost is driven by what the PLAIN PI would
//! do on this clock.
//!
//! **The decision never reads the gains in force (v1.35.14).** A [`PlainMirror`] runs the plain
//! PI on the real loop's OPEN-LOOP phase: each window it takes the real loop's correction back out
//! of the tracked error (`e` minus a running pull) and puts its own in, and it takes every re-seed
//! of the real integrator (a followed step, a boot or grandmaster fit). Its error is the error the
//! plain PI would hold now, `ρ / K_I` in a steady ramp, whatever boost runs. v1.35.12/13 judged
//! `g²·|mean|` of the boosted error and `u` with the `K_P` in force instead. Both are right in a
//! steady ramp and wrong while the gain moves: the gain changes at once, the error only over the
//! loop's τ, so each gain step moved its own input (a positive feedback). On the thermal ramp's
//! release that turned two followers' ~40 ns of residue into a 2-7 µs split on 1 s Dante windows.
//! The mirror's error moves with the gain only through the one window the real word lags the
//! command, ~K_P·e·dt.
//!
//! A boost is NEEDED while the mirror's running mean (τ [`EXCURSION_TAU_S`]) is beyond
//! [`EXCURSION_ARM_US`]. It STARTS only when the mirror's error is also
//! - carried by an UNLEARNED FREQUENCY `u` of its own sign ([`EXCURSION_UNLEARNED_PPM`]). The
//!   mirror is `de/dt = u − K_P·e`, so `u = de/dt + K_P·e`. A ramp the PI lags has `u ≈ K_P·e > 0`;
//!   a path-delay level shift or an engagement's offset the PI retires has `u ≈ 0` (then of the
//!   opposite sign, as the integrator learns a little of it): boosting those would only be a 4×
//!   word swing,
//! - with `|u|` no larger than [`EXCURSION_MAX_RATE_PPM`]: a larger one is a frequency STEP, the
//!   camera-box 1372 detector's (it confirms in ~20-25 s and re-seeds the integrator),
//! - SUSTAINED: `u` is judged by a low quantile over the last [`EXCURSION_SUSTAIN_S`], twice the
//!   rate's ring, so the seconds a jump spends inside that ring (its slope reads as `u`) never
//!   start a boost.
//!
//! **Every input enters continuously** (two followers on one PC read the same windows ± ~40 ns of
//! conversion residue, `.claude/rules/ptp-library.md` #136/#170: an all-or-nothing arm let them
//! arm a window apart and split ~1.2 µs). Each condition is a linear taper to a factor in [0, 1];
//! the TARGET is `needed × max(start, min(memory, charge))`. A charge `C = max(C − dt/T, target)`
//! holds it (`T` = [`GM_BOOST_HOLD_S`] + [`GM_BOOST_RAMP_S`]), its exponent `min(1, C·T/ramp)` is
//! the #166 schedule (full for the hold, then back geometrically over the ramp), and the exponent
//! in force rises toward it by at most `dt / `[`EXCURSION_RAMP_IN_S`] (never a word step under a
//! standing error). The gain is `GM_BOOST_GAIN^level`. A running boost holds itself only up to
//! its own charge: `needed × charge ≤ charge`, so it holds while the ramp lasts and can never grow
//! itself (v1.35.13 held on `min(level, memory)`, and `level` = 1.4 × the charge in the return,
//! so `needed > 0.71` re-raised a returning boost: a fork two followers took apart). `max`, `min`
//! and the tapers are continuous, so the gain trajectory is continuous in the windows' `e`
//! (`the_boost_is_continuous_in_its_input_204`).
//!
//! Rejected: a faster base loop (every µs of window noise passes as `K_P` ppm of word jitter, the
//! camera-box 1372 doctrine), and a third integrator / ramp feed-forward (a ramp estimate from a
//! noisy ring is ppm-scale noise on the word, and it changes the steady-state law of every box).

use std::collections::VecDeque;

mod plain_mirror;

use plain_mirror::PlainMirror;

use super::{fit_line, FSTEP_MIN_PPM, GM_BOOST_GAIN, GM_BOOST_HOLD_S, GM_BOOST_RAMP_S, K_P_PER_S};

/// Grandmaster seconds of the mirror's running mean (an exponential average). Its noise is the
/// window noise × √(dt / 2τ): ~3 µs for the 14 µs window `e` sd of a USB Dante NIC at 1 s
/// windows, ~1 µs on resolume-pp (sd 3.5-5.3 µs). 7 s (a lag ~3 µs shorter on a 6 ppm / 2 min
/// ramp) let the two-clock bench's 0.1 ppm/min wander start a boost; 8 s passed it by little.
pub const EXCURSION_TAU_S: f64 = 10.0;

/// The running mean of the mirror's error (µs) at which the boost's target is half: the taper
/// runs over ± [`EXCURSION_ARM_TAPER_US`] around it, so it starts at 25 µs. Clear of the plain
/// PI's normal regime: a 0.1 ppm/min wander stands at 17 µs, and with a 0.4 ppm hand-over on top
/// the mean stays under 25 µs (the 1372 pins stay the plain PI bit for bit).
pub const EXCURSION_ARM_US: f64 = 30.0;
pub const EXCURSION_ARM_TAPER_US: f64 = 5.0;

/// Grandmaster seconds of the mirror's errors whose least-squares slope is the rate the error
/// moves at (µs/s). Long enough for the line fit's minimum of 12 points on ~1 s Dante windows;
/// its noise is ~1 µs/s at a 11 µs window `e` sd.
pub const EXCURSION_RATE_WINDOW_S: f64 = 15.0;

/// An unlearned frequency larger than this (ppm) is a frequency STEP, the camera-box 1372
/// detector's: a boost under it would learn part of the step first and the detector would follow
/// only the rest. The detector's own minimum; the taper runs ± 1 ppm around it. Half of it (the
/// v1.35.13 bound) cut a 6 ppm / 2 min ramp's start: the plain PI it mirrors lags that ramp by
/// 3-4 ppm about a minute in, so the start waited for the ramp's u to fall back and the
/// error reached 78 µs at ±20 µs window noise on 1 s windows. A step reads its whole size at once:
/// beyond 7 ppm it never starts a boost, and the detector's confirmation restarts the mean.
pub const EXCURSION_MAX_RATE_PPM: f64 = FSTEP_MIN_PPM;

/// The unlearned frequency (ppm, in the error's own direction) at which the target starts, and
/// where it is full. A ramp the plain PI lags at 25 µs reads ≥ `K_P·25` = 0.5 ppm; a level shift
/// being retired reads ~0, then negative.
pub const EXCURSION_UNLEARNED_PPM: f64 = 0.25;
pub const EXCURSION_UNLEARNED_FULL_PPM: f64 = 0.5;

/// Grandmaster seconds over which the unlearned frequency must hold, judged by its
/// [`EXCURSION_SUSTAIN_QUANTILE`] (a low quantile, so window noise dipping under the bound now and
/// then does not stop a real ramp). Twice [`EXCURSION_RATE_WINDOW_S`]: the seconds a jump spends
/// inside the rate's ring (where its slope reads as `u`) are at most half the window, and the
/// windows before the jump or after it read `u ≈ 0`.
pub const EXCURSION_SUSTAIN_S: f64 = 2.0 * EXCURSION_RATE_WINDOW_S;

/// The quantile of the sustained unlearned frequency. A jump fills at most half of the window, so
/// any quantile below 0.5 stays on the windows before or after it; at 0.4 a 40 µs path shift
/// already started a boost on 0.5 s windows. 0.35 rather than 0.25: on 1 s windows the ring
/// carries ~15 strongly correlated slopes, and the 25 % one waited out a noise dip of a few
/// seconds on a 6 ppm / 2 min ramp.
pub const EXCURSION_SUSTAIN_QUANTILE: f64 = 0.35;

/// Grandmaster seconds after an engagement or a re-anchor (any restart of the mean) before the
/// target may rise, tapered over the next [`EXCURSION_HOLDOFF_TAPER_S`]: the running mean and the
/// rate's ring fill first (a rate from a few windows is not one).
pub const EXCURSION_ENGAGE_HOLDOFF_S: f64 = 30.0;
pub const EXCURSION_HOLDOFF_TAPER_S: f64 = 10.0;

/// Grandmaster seconds over which the boost's exponent may rise from 0 to 1 (`GAIN^(t/ramp)`):
/// `K_P` grows by ≤ ~26 % per 0.5 s window, so under a 30 µs error the gain change itself moves
/// the word by ≤ ~0.2 ppm × the gain per window, never a step. 5 s left a 6 ppm / 2 min ramp at
/// ±20 µs window noise at 50.9 µs p99 (51.7 max, 200 runs); 3 s holds it at 49.8 max.
pub const EXCURSION_RAMP_IN_S: f64 = 3.0;

/// Grandmaster seconds a start is remembered (linearly forgotten): a running boost holds only
/// while it remembers one. A genuine ramp keeps starting; after a finished one the boost lets go
/// within this plus the hold and the return even when window noise keeps `needed` up (the third
/// review: on ±35-50 µs windows the mean alone read past the bound and held the boost for ever).
pub const EXCURSION_START_MEMORY_S: f64 = 600.0;

/// The boost's exponent at which a start is counted and logged.
pub const EXCURSION_REPORT_LEVEL: f64 = 0.25;

/// The charge's whole decay (s): the hold, then the return.
const EXCURSION_CHARGE_S: f64 = GM_BOOST_HOLD_S + GM_BOOST_RAMP_S;

/// A boost starting from the normal gains in one window (the caller logs it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExcursionArm {
    /// The running mean (µs) of the plain PI's error then.
    pub mean_us: f64,
    /// The unlearned frequency (ppm) then.
    pub unlearned_ppm: f64,
    /// The target it rose toward (0-1).
    pub target: f64,
}

/// The real loop's word after a window, as the core hands it to
/// [`ExcursionBoost::set_loop_word`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopWord {
    /// The correction acting on the tracked error (ppm): the word minus the pull's feed-forward.
    pub corr_ppm: f64,
    /// The integrator after the window (ppm).
    pub i_ppm: f64,
    /// The `K_I` it learned with this window (1/s²).
    pub k_i: f64,
}

/// The excursion detector and the boost it drives.
#[derive(Clone, Debug, Default)]
pub struct ExcursionBoost {
    /// The plain PI run on the same clock; `None` until the first window of an engagement.
    mirror: Option<PlainMirror>,
    /// The real loop's word for the interval after this window, set by the core before
    /// [`Self::observe`] (`None`: the real loop is taken to be the plain PI).
    loop_word: Option<LoopWord>,
    /// Running mean (µs) of the mirror's error; `None` until the first window of an engagement.
    mean_us: Option<f64>,
    /// The memory of a start (0-1): `max(memory − dt/`[`EXCURSION_START_MEMORY_S`]`, start)`. A
    /// running boost holds only while it remembers a start: the escape valve when window noise
    /// keeps `needed` up after the ramp is gone.
    start_memory: f64,
    /// The mirror's errors of the last [`EXCURSION_RATE_WINDOW_S`] (`(s, µs)`), and the
    /// grandmaster seconds since the mean restarted.
    ring: VecDeque<(f64, f64)>,
    clock_s: f64,
    /// The last [`EXCURSION_SUSTAIN_S`] of unlearned frequencies (`(s, ppm)`).
    raw: VecDeque<(f64, f64)>,
    /// The held target (0-1), decaying over [`EXCURSION_CHARGE_S`].
    charge: f64,
    /// The boost's exponent in force (0 = the normal gains, 1 = × [`GM_BOOST_GAIN`]).
    level: f64,
    /// Boosts that reached [`EXCURSION_REPORT_LEVEL`] since start.
    arms: u32,
}

/// `(v − lo) / (hi − lo)` clamped to [0, 1]: a continuous step.
fn taper(v: f64, lo: f64, hi: f64) -> f64 {
    ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
}

impl ExcursionBoost {
    /// The boost factor in force (`None` = the normal gains).
    pub fn gain(&self) -> Option<f64> {
        (self.level > 0.0).then(|| GM_BOOST_GAIN.powf(self.level))
    }

    /// The boost's exponent in force (0-1).
    pub fn level(&self) -> f64 {
        self.level
    }

    /// The running mean (µs) of the plain PI's error.
    pub fn mean_us(&self) -> Option<f64> {
        self.mean_us
    }

    /// The mirror's error (µs): what the plain PI would hold now.
    #[cfg(test)]
    pub(crate) fn plain_error_us(&self) -> Option<f64> {
        self.mirror.as_ref().map(|m| m.e_us())
    }

    /// Boosts started from the normal gains since start.
    pub fn arms(&self) -> u32 {
        self.arms
    }

    /// The mean restarts (an engagement, a re-anchor, a followed step: `e` is a new quantity).
    pub fn restart_mean(&mut self) {
        self.mirror = None;
        self.mean_us = None;
        self.ring.clear();
        self.raw.clear();
        self.clock_s = 0.0;
    }

    /// The real loop's word for the interval after this window. Without it the real loop is
    /// taken to be the plain PI (the mirror's error is then the tracked error itself).
    pub fn set_loop_word(&mut self, word: LoopWord) {
        self.loop_word = Some(word);
    }

    /// A lock loss ends the boost with the engagement that ran it (as #166's).
    pub fn end(&mut self) {
        self.charge = 0.0;
        self.level = 0.0;
        self.start_memory = 0.0;
        self.restart_mean();
    }

    /// The boost's clock runs `dt` with no new target (a late window: it says nothing about the
    /// clock, but every box that took the same windows runs the same schedule). The real word is
    /// held through it, and so is the mirror's.
    pub fn advance(&mut self, dt: f64) {
        if let Some(m) = self.mirror.as_mut() {
            m.skip(dt);
        }
        if !self.ring.is_empty() {
            self.clock_s += dt;
        }
        self.charge = (self.charge - dt / EXCURSION_CHARGE_S).max(0.0);
        self.level = self.level.min(self.scheduled());
    }

    /// The exponent the charge holds: full for the hold, then back over the ramp.
    fn scheduled(&self) -> f64 {
        (self.charge * EXCURSION_CHARGE_S / GM_BOOST_RAMP_S).min(1.0)
    }

    /// The unlearned frequency the window held in the error's direction `sign`: the
    /// [`EXCURSION_SUSTAIN_QUANTILE`] of `u·sign` over the last [`EXCURSION_SUSTAIN_S`]. An order
    /// statistic is continuous in its samples; a jump through the rate's ring fills at most half
    /// of the window, so the quantile stays on the windows before or after it.
    fn sustained_unlearned(&self, sign: f64) -> Option<f64> {
        if self.raw.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = self.raw.iter().map(|&(_, u)| u * sign).collect();
        v.sort_by(f64::total_cmp);
        let i = ((v.len() - 1) as f64 * EXCURSION_SUSTAIN_QUANTILE).floor() as usize;
        Some(v[i])
    }

    /// The mirror takes this window: its error (µs).
    fn mirror_window(&mut self, tracked_us: f64, dt: f64) -> f64 {
        let word = self.loop_word.take();
        match self.mirror.as_mut() {
            Some(m) => m.take(tracked_us, dt, word),
            None => self
                .mirror
                .insert(PlainMirror::new(tracked_us, word))
                .e_us(),
        }
    }

    /// The mirror's error into the running mean and the rate's ring: the mean and the unlearned
    /// frequency `u` at this window (`None` until the ring holds a line).
    fn measure(&mut self, plain_us: f64, dt: f64) -> (f64, Option<f64>) {
        let alpha = 1.0 - (-dt / EXCURSION_TAU_S).exp();
        let mean = match self.mean_us {
            Some(m) => m + alpha * (plain_us - m),
            None => plain_us * alpha,
        };
        self.mean_us = Some(mean);
        if !self.ring.is_empty() {
            self.clock_s += dt;
        }
        let now = self.clock_s;
        self.ring.push_back((now, plain_us));
        while self
            .ring
            .front()
            .is_some_and(|&(t, _)| now - t > EXCURSION_RATE_WINDOW_S + 1e-9)
        {
            self.ring.pop_front();
        }
        // The slope and the level at the same instant (the newest window): the running mean lags a
        // moving error by ~τ × its rate, which reads a retired level shift as unlearned. The
        // mirror runs the plain gains, so its `K_P` is the normal one.
        let unlearned =
            fit_line(self.ring.iter()).map(|l| l.slope_ppm + K_P_PER_S * l.value_at(now));
        if let Some(u) = unlearned {
            self.raw.push_back((now, u));
        }
        while self
            .raw
            .front()
            .is_some_and(|&(t, _)| now - t > EXCURSION_SUSTAIN_S + 1e-9)
        {
            self.raw.pop_front();
        }
        (mean, unlearned)
    }

    /// One window the PI took: `tracked_us` = the error it tracked (`e` minus a running pull),
    /// `dt` its grandmaster seconds, `armable` = no boot fit owns the frequency and no frequency
    /// step was followed in this window. The core hands the real word first
    /// ([`Self::set_loop_word`]). Returns the window where a boost starts from the normal gains.
    pub fn observe(&mut self, tracked_us: f64, dt: f64, armable: bool) -> Option<ExcursionArm> {
        let plain_us = self.mirror_window(tracked_us, dt);
        let (mean, unlearned) = self.measure(plain_us, dt);
        // Full by the time since the restart, never by the ring's span: a ~1.016 s Dante window
        // spacing never spans exactly the window (the #136 v1.27.2 lesson).
        let sustained = (self.clock_s >= EXCURSION_SUSTAIN_S)
            .then(|| self.sustained_unlearned(mean.signum()))
            .flatten();
        let needed = taper(
            mean.abs(),
            EXCURSION_ARM_US - EXCURSION_ARM_TAPER_US,
            EXCURSION_ARM_US + EXCURSION_ARM_TAPER_US,
        );
        // Only a START needs the unlearned frequency (a ramp, not a level shift or a step).
        let start = match (unlearned, sustained) {
            (Some(u), Some(u_low)) => {
                taper(
                    -u.abs(),
                    -(EXCURSION_MAX_RATE_PPM + 1.0),
                    -(EXCURSION_MAX_RATE_PPM - 1.0),
                ) * taper(u_low, EXCURSION_UNLEARNED_PPM, EXCURSION_UNLEARNED_FULL_PPM)
                    * taper(
                        self.clock_s,
                        EXCURSION_ENGAGE_HOLDOFF_S,
                        EXCURSION_ENGAGE_HOLDOFF_S + EXCURSION_HOLDOFF_TAPER_S,
                    )
            }
            _ => 0.0,
        };
        self.start_memory = (self.start_memory - dt / EXCURSION_START_MEMORY_S).max(start);
        // A running boost holds itself up to its own charge, never above it: `needed ≤ 1`, so
        // only a start raises the charge.
        let target = if armable {
            needed * start.max(self.start_memory.min(self.charge))
        } else {
            0.0
        };
        self.charge = (self.charge - dt / EXCURSION_CHARGE_S).max(target);
        let before = self.level;
        self.level = self.scheduled().min(before + dt / EXCURSION_RAMP_IN_S);
        // Counted and logged when it reaches a quarter (a ×1.4 boost): a target that only grazes
        // the bound is not reported as a boost.
        if before < EXCURSION_REPORT_LEVEL && self.level >= EXCURSION_REPORT_LEVEL {
            self.arms = self.arms.saturating_add(1);
            Some(ExcursionArm {
                mean_us: mean,
                unlearned_ppm: unlearned.unwrap_or(0.0),
                target,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A detector past its engagement holdoff (quiet windows).
    fn warmed() -> ExcursionBoost {
        let mut b = ExcursionBoost::default();
        while b.clock_s < EXCURSION_ENGAGE_HOLDOFF_S + EXCURSION_HOLDOFF_TAPER_S {
            assert_eq!(b.observe(0.0, 0.5, true), None);
        }
        b
    }

    /// A growing excursion: 1 µs/s from `start_us`, `n` windows.
    fn grow(b: &mut ExcursionBoost, start_us: f64, n: usize) -> Vec<Option<ExcursionArm>> {
        (0..n)
            .map(|i| b.observe(start_us + i as f64 * 0.5, 0.5, true))
            .collect()
    }

    #[test]
    fn the_boost_ramps_in_holds_and_returns_without_a_jump_204() {
        let mut b = warmed();
        let arms = grow(&mut b, 30.0, 160);
        assert_eq!(arms.iter().flatten().count(), 1, "one start");
        assert_eq!(b.level(), 1.0, "full after the ramp-in");
        assert_eq!(b.gain(), Some(GM_BOOST_GAIN));
        // Quiet: held for the hold, then back over the ramp, never a jump.
        let max_step = 0.5 / EXCURSION_RAMP_IN_S + 1e-12;
        let mut prev = b.level();
        let mut t = 0.0;
        let mut left_full_at = None;
        while b.gain().is_some() {
            b.observe(0.0, 0.5, true);
            t += 0.5;
            assert!((b.level() - prev).abs() <= max_step, "a jump at {t}s");
            if b.level() < 1.0 && left_full_at.is_none() {
                left_full_at = Some(t);
            }
            prev = b.level();
            assert!(t < 1_000.0, "never returned");
        }
        // The hold counts from the last window the boost was still needed: the running mean of
        // the excursion (here a 110 µs one, far beyond what a boosted loop leaves) decays over a
        // few of its time constants after the input goes quiet.
        let left = left_full_at.expect("returned");
        assert!(
            (GM_BOOST_HOLD_S - 10.0..=GM_BOOST_HOLD_S + 200.0).contains(&left),
            "the hold ended at {left}s"
        );
        assert!(
            (t - left - GM_BOOST_RAMP_S).abs() <= 10.0,
            "the return took {}s",
            t - left
        );
    }

    #[test]
    fn a_late_window_advances_the_schedule_and_never_raises_it_204() {
        let mut b = warmed();
        grow(&mut b, 30.0, 160);
        let mut t = 0.0;
        while b.gain().is_some() {
            b.advance(0.5);
            t += 0.5;
            assert!(t < 1_000.0, "never returned");
        }
        assert!(t >= GM_BOOST_HOLD_S, "{t}");
    }

    #[test]
    fn noise_alone_never_boosts_and_a_growing_error_starts_once_204() {
        let mut b = warmed();
        let mut state = 0x204_u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let n = ((state >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0 * 30.0;
            assert_eq!(b.observe(n, 0.5, true), None, "noise boosted it");
        }
        assert_eq!(b.gain(), None);
        let starts = grow(&mut b, 30.0, 120).into_iter().flatten().count();
        assert_eq!(starts, 1);
        assert_eq!(b.arms(), 1);
    }

    #[test]
    fn a_level_shift_the_pi_retires_is_not_boosted_204() {
        // The critically damped PI's answer to a phase step S: e = S (1 − ωt) e^(−ωt), ω = K_P/2.
        // Its unlearned frequency is −S ω² t e^(−ωt): nothing to learn, then the little the
        // integrator learned from the shift. Up to the late-window bound (150 µs) its undershoot
        // (−0.135 S) stays under the bound; beyond it the undershoot IS a frequency the integrator
        // mis-learned, which the boost may retire.
        let w = K_P_PER_S / 2.0;
        for s in [40.0, -60.0, 100.0, -150.0] {
            let mut b = warmed();
            for n in 0..2_000 {
                let t = n as f64 * 0.5;
                let e = s * (1.0 - w * t) * (-w * t).exp();
                assert_eq!(b.observe(e, 0.5, true), None, "shift {s}, {t}s");
            }
            assert_eq!(b.gain(), None);
        }
    }

    #[test]
    fn an_error_growing_at_a_frequency_steps_rate_is_left_to_the_step_detector_204() {
        let mut b = warmed();
        // 25 ppm: the error grows 25 µs/s for the detector's 20 s ring.
        for n in 0..40 {
            assert_eq!(
                b.observe(25.0 * n as f64 * 0.5, 0.5, true),
                None,
                "window {n}"
            );
        }
        assert_eq!(b.gain(), None);
        // A slow ramp (0.8 µs/s) past the bound starts it.
        let mut b = warmed();
        let arm = (0..400)
            .find_map(|n| b.observe(0.8 * n as f64 * 0.5, 0.5, true))
            .expect("a slow excursion starts the boost");
        assert!(
            arm.unlearned_ppm.abs() < EXCURSION_MAX_RATE_PPM
                && arm.mean_us >= EXCURSION_ARM_US - EXCURSION_ARM_TAPER_US,
            "{arm:?}"
        );
    }

    #[test]
    fn nothing_starts_within_the_holdoff_after_an_engagement_204() {
        let mut b = ExcursionBoost::default();
        let at = (0..2_000)
            .find_map(|n| {
                b.observe(30.0 + n as f64 * 0.5, 0.5, true)
                    .map(|_| n as f64 * 0.5)
            })
            .expect("a growing excursion starts after the holdoff");
        assert!(
            at >= EXCURSION_ENGAGE_HOLDOFF_S,
            "started {at}s after the engagement"
        );
    }

    /// Re-review of #204: the continuity sweep proves nothing unless it crosses a whole boost.
    #[test]
    fn the_continuity_sweep_crosses_a_full_boost_204() {
        let mut b = warmed();
        let max = (0..400)
            .map(|n| {
                b.observe(0.5 * n as f64 * 0.5, 0.5, true);
                b.gain().unwrap_or(1.0)
            })
            .fold(1.0, f64::max);
        assert!(max > 3.9, "the sweep's input never boosted fully: x{max}");
    }

    /// Re-review of #204: a late window's seconds count on the detector's own clock too, or the
    /// next rate is fitted over a ring whose times lost them.
    #[test]
    fn a_late_window_counts_on_the_detectors_clock_204() {
        let mut b = warmed();
        let before = b.clock_s;
        b.advance(0.5);
        b.advance(0.5);
        b.observe(0.0, 0.5, true);
        assert!(
            (b.clock_s - before - 1.5).abs() < 1e-9,
            "{}",
            b.clock_s - before
        );
    }

    #[test]
    fn a_window_no_boot_fit_allows_starts_nothing_204() {
        let mut b = warmed();
        for n in 0..200 {
            assert_eq!(b.observe(30.0 + n as f64 * 0.5, 0.5, false), None);
        }
        assert_eq!(b.gain(), None);
        assert_eq!(b.arms(), 0);
        b.end();
        assert_eq!(b.mean_us(), None);
    }

    /// Review of #204: two followers on one PC read the same windows ± ~40 ns of conversion
    /// residue (`.claude/rules/ptp-library.md`, #136/#170), so the boost must be CONTINUOUS in its
    /// input: an all-or-nothing arm let them arm a window apart and split by ~1.2 µs. Sweep the
    /// input in 1 ns steps across a whole arming and assert the gain trajectory never jumps.
    #[test]
    fn the_boost_is_continuous_in_its_input_204() {
        let mut worst = 0.0f64;
        let mut prev: Option<Vec<f64>> = None;
        for k in 0..=3_000 {
            let offset_us = k as f64 * 0.001;
            let mut b = warmed();
            let traj: Vec<f64> = (0..400)
                .map(|n| {
                    b.observe(0.5 * n as f64 * 0.5 + offset_us, 0.5, true);
                    b.gain().unwrap_or(1.0)
                })
                .collect();
            if let Some(p) = &prev {
                for (a, c) in p.iter().zip(&traj) {
                    worst = worst.max((a - c).abs());
                }
            }
            prev = Some(traj);
        }
        assert!(
            worst <= 0.01,
            "the gain jumped by {worst} for a 1 ns input change"
        );
    }
}
