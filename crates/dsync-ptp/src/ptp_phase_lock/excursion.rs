//! dsync#204 — a sustained phase EXCURSION arms the #166 gain boost.
//!
//! The plain PI (~100 s, critically damped) leaves a frequency RAMP `ρ` (ppm/s) a standing phase
//! error of `ρ / K_I`. A load that warms the local crystal moves it by a few ppm over minutes:
//! resolume-pp (19.11.2026) went −3.8 → −9.6 ppm and read +95 µs for minutes while still in LOCK.
//! The grandmaster-change boost (`K_P × 4`, `K_I × 16`, still ζ = 1, τ 25 s) cuts that error 16×,
//! but it was armed only by a time-base change. Here the TRACKED error itself arms it: its mean
//! over [`EXCURSION_TAU_S`] beyond [`EXCURSION_ARM_US`] is not window noise (the noise of that
//! mean is a few µs), it is a frequency the loop is behind on.
//!
//! The boost here starts from the gains in force (never a word step under a standing error): it
//! ramps in geometrically over [`EXCURSION_RAMP_IN_S`], holds [`GM_BOOST_HOLD_S`] and returns
//! over [`GM_BOOST_RAMP_S`] as the #166 schedule does. An excursion that is still there re-arms
//! it from wherever it is, continuously (a ramp that goes on keeps the boost). A pure function of
//! the windows' grandmaster `dt` and their `e`, so two followers fed the same windows agree.
//!
//! Rejected: a faster base loop (every µs of window noise passes as `K_P` ppm of word jitter, the
//! camera-box 1372 doctrine), and a third integrator / ramp feed-forward (a ramp estimate from a
//! noisy ring is ppm-scale noise on the word, and it changes the steady-state law of every box).

use std::collections::VecDeque;

use super::{fit_line, FSTEP_MIN_PPM, GM_BOOST_GAIN, GM_BOOST_HOLD_S, GM_BOOST_RAMP_S};

/// Grandmaster seconds of the tracked error's running mean (an exponential average). Its noise is
/// the window noise × √(dt / 2τ): ~3 µs for the 14 µs window `e` sd of a USB Dante NIC at 1 s
/// windows, ~1 µs on resolume-pp (sd 3.5-5.3 µs).
pub const EXCURSION_TAU_S: f64 = 10.0;

/// The running mean of the tracked error (µs) beyond which the boost arms. Over 8 σ of the mean's
/// noise on the noisiest NIC measured, and clear of the plain PI's normal regime: a 0.1 ppm/min
/// wander stands at 17 µs, a 0.4 ppm hand-over peaks at ~15 µs (the 1372 pins stay the plain PI).
/// The fast-ramp cost: a 6 ppm / 2 min ramp peaks at ~44 µs (20 µs: ~42; 30 µs: ~50).
pub const EXCURSION_ARM_US: f64 = 25.0;

/// Grandmaster seconds of the tracked errors whose least-squares slope is the rate the error moves
/// at (µs/s). Long enough for the line fit's minimum of 12 points on ~1 s Dante windows; its
/// noise is ~1 µs/s at a 11 µs window `e` sd.
pub const EXCURSION_RATE_WINDOW_S: f64 = 15.0;

/// An error moving faster than this (µs/s = ppm) is a frequency STEP, the camera-box 1372
/// detector's (it confirms in ~20-25 s and re-seeds the integrator): a boost armed under it would
/// learn part of the step first and the detector would follow only the rest. Half the detector's
/// minimum. A local ramp the plain PI lags reads ~1-2 µs/s when its error reaches the bound.
pub const EXCURSION_MAX_RATE_PPM: f64 = FSTEP_MIN_PPM / 2.0;

/// An error already shrinking faster than this (µs/s) is being retired by the loop (a
/// re-engagement off the line, a pull's tail): boosting a retirement only swings the word
/// (`K_P × 4` on 500 µs is 40 ppm, the #166 review). About 2 σ of the rate's noise.
pub const EXCURSION_RETIRING_PPM: f64 = 1.0;

/// Grandmaster seconds after an engagement or a re-anchor (any restart of the mean) before the
/// boost may arm: the tracked error then holds the engagement's own offset (an anchor placed on a
/// noisy window, a re-engagement off the line), which the plain PI retires in ~3 τ (a 125 µs
/// offset is under the bound after 300 s). Boosting that retirement is the #166 review's word swing,
/// and its rate (`−K_P e`, 0.5 µs/s at 25 µs) is inside the rate's noise, so the retiring gate
/// cannot tell it. A cold boot's own fits block it for ~80 s anyway.
pub const EXCURSION_ENGAGE_HOLDOFF_S: f64 = 300.0;

/// Grandmaster seconds over which the boost ramps in from the normal gains (`GAIN^(t/ramp)`): a
/// `K_P` change of ~15 % per 0.5 s window, so under a 25 µs error the word moves ≤ ~0.08 ppm × the
/// gain per window from the gain change itself, never a step.
pub const EXCURSION_RAMP_IN_S: f64 = 5.0;

/// The boost's whole schedule ends here (s since it armed, ramp-in included).
const EXCURSION_END_S: f64 = EXCURSION_RAMP_IN_S + GM_BOOST_HOLD_S + GM_BOOST_RAMP_S;

/// What armed the boost in one window (the caller logs it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExcursionArm {
    /// The tracked error's running mean (µs) that armed it.
    pub mean_us: f64,
    /// The rate (µs/s) the error moved at then.
    pub rate_ppm: f64,
    /// The boost factor in force when it armed (1 = from the normal gains).
    pub gain_before: f64,
}

/// The excursion detector and the boost it runs.
#[derive(Clone, Debug, Default)]
pub struct ExcursionBoost {
    /// Running mean (µs) of the tracked error; `None` until the first window of an engagement.
    mean_us: Option<f64>,
    /// The tracked errors of the last [`EXCURSION_RATE_WINDOW_S`] (`(s, µs)`) and their clock.
    ring: VecDeque<(f64, f64)>,
    clock_s: f64,
    /// Grandmaster seconds since the boost armed (`None` = not running).
    boost_s: Option<f64>,
    /// Boosts armed since start (a re-arm of a running one included).
    arms: u32,
}

/// The boost factor `t` s after arming.
pub fn excursion_gain(t: f64) -> f64 {
    if t < EXCURSION_RAMP_IN_S {
        GM_BOOST_GAIN.powf((t / EXCURSION_RAMP_IN_S).max(0.0))
    } else if t < EXCURSION_RAMP_IN_S + GM_BOOST_HOLD_S {
        GM_BOOST_GAIN
    } else {
        let out = (t - EXCURSION_RAMP_IN_S - GM_BOOST_HOLD_S) / GM_BOOST_RAMP_S;
        GM_BOOST_GAIN.powf((1.0 - out).clamp(0.0, 1.0))
    }
}

impl ExcursionBoost {
    /// The boost factor in force (`None` = none running).
    pub fn gain(&self) -> Option<f64> {
        self.boost_s.map(excursion_gain)
    }

    /// Seconds since the running boost armed.
    pub fn boost_s(&self) -> Option<f64> {
        self.boost_s
    }

    /// The tracked error's running mean (µs).
    pub fn mean_us(&self) -> Option<f64> {
        self.mean_us
    }

    /// Boosts armed since start.
    pub fn arms(&self) -> u32 {
        self.arms
    }

    /// The mean restarts (an engagement, a re-anchor, a followed step: `e` is a new quantity).
    pub fn restart_mean(&mut self) {
        self.mean_us = None;
        self.ring.clear();
        self.clock_s = 0.0;
    }

    /// A lock loss ends the boost with the engagement that ran it (as #166's).
    pub fn end(&mut self) {
        self.boost_s = None;
        self.restart_mean();
    }

    /// One window the PI took: `tracked_us` = the error it tracked (`e` minus a running pull),
    /// `dt` its grandmaster seconds, `armable` = no boot fit owns the frequency. Advances the
    /// boost's clock, then arms or re-arms it when the mean is beyond the bound.
    pub fn observe(&mut self, tracked_us: f64, dt: f64, armable: bool) -> Option<ExcursionArm> {
        let alpha = 1.0 - (-dt / EXCURSION_TAU_S).exp();
        let mean = match self.mean_us {
            Some(m) => m + alpha * (tracked_us - m),
            None => tracked_us * alpha,
        };
        self.mean_us = Some(mean);
        if !self.ring.is_empty() {
            self.clock_s += dt;
        }
        let now = self.clock_s;
        self.ring.push_back((now, tracked_us));
        while self
            .ring
            .front()
            .is_some_and(|&(t, _)| now - t > EXCURSION_RATE_WINDOW_S + 1e-9)
        {
            self.ring.pop_front();
        }
        // No rate yet (too few windows since a restart) = not judged: never armed blind.
        let rate_ppm = fit_line(self.ring.iter()).map_or(f64::INFINITY, |l| l.slope_ppm);
        if let Some(t) = self.boost_s {
            let t = t + dt;
            self.boost_s = (t < EXCURSION_END_S).then_some(t);
        }
        let retiring = rate_ppm * mean.signum() < -EXCURSION_RETIRING_PPM;
        if !armable
            || self.clock_s < EXCURSION_ENGAGE_HOLDOFF_S
            || mean.abs() < EXCURSION_ARM_US
            || rate_ppm.abs() > EXCURSION_MAX_RATE_PPM
            || retiring
        {
            return None;
        }
        let gain_before = self.gain().unwrap_or(1.0);
        let t = self.boost_s.unwrap_or(0.0);
        let restart = if t < EXCURSION_RAMP_IN_S {
            // Still ramping in (or not running: from the normal gains).
            t
        } else if t <= EXCURSION_RAMP_IN_S + GM_BOOST_HOLD_S {
            // Held: the hold starts again.
            EXCURSION_RAMP_IN_S
        } else {
            // Returning: back onto the ramp-in where it gives the same gain (continuous).
            EXCURSION_RAMP_IN_S * gain_before.ln() / GM_BOOST_GAIN.ln()
        };
        // Logged (and counted) only when it starts from the normal gains or comes back while returning:
        // a held boost re-arms every window the excursion lasts.
        let fresh = self.boost_s.is_none() || t > EXCURSION_RAMP_IN_S + GM_BOOST_HOLD_S;
        self.boost_s = Some(restart);
        if fresh {
            self.arms = self.arms.saturating_add(1);
            Some(ExcursionArm {
                mean_us: mean,
                rate_ppm,
                gain_before,
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
        while b.clock_s < EXCURSION_ENGAGE_HOLDOFF_S {
            assert_eq!(b.observe(0.0, 0.5, true), None);
        }
        b
    }

    #[test]
    fn the_gain_ramps_in_holds_and_returns_continuously_204() {
        assert_eq!(excursion_gain(0.0), 1.0);
        assert!((excursion_gain(EXCURSION_RAMP_IN_S / 2.0) - 2.0).abs() < 1e-12);
        assert_eq!(excursion_gain(EXCURSION_RAMP_IN_S), GM_BOOST_GAIN);
        assert_eq!(
            excursion_gain(EXCURSION_RAMP_IN_S + GM_BOOST_HOLD_S),
            GM_BOOST_GAIN
        );
        assert!((excursion_gain(EXCURSION_END_S) - 1.0).abs() < 1e-12);
        // The steepest move of the schedule is the ramp-in's, per 0.5 s window.
        let max_step = GM_BOOST_GAIN.powf(0.5 / EXCURSION_RAMP_IN_S) - 1.0 + 1e-9;
        let mut prev = excursion_gain(0.0);
        let mut t = 0.0;
        while t < EXCURSION_END_S {
            t += 0.5;
            let g = excursion_gain(t);
            assert!(
                (g / prev - 1.0).abs() <= max_step,
                "a gain jump at {t}s: {prev} -> {g}"
            );
            prev = g;
        }
    }

    #[test]
    fn noise_alone_never_arms_and_a_standing_error_arms_once_204() {
        let mut b = warmed();
        let mut state = 0x204_u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let n = ((state >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0 * 30.0;
            assert_eq!(b.observe(n, 0.5, true), None, "noise armed it");
        }
        assert_eq!(b.gain(), None);
        let mut armed = 0;
        for _ in 0..60 {
            armed += usize::from(b.observe(EXCURSION_ARM_US + 5.0, 0.5, true).is_some());
        }
        assert_eq!(armed, 1, "a held error arms once (ramp-in and hold)");
        assert!(b.gain().expect("running") > 1.0);
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
        // A slow ramp (0.5 µs/s) past the bound arms.
        let mut b = warmed();
        let armed = (0..400).find_map(|n| b.observe(0.5 * n as f64 * 0.5, 0.5, true));
        let arm = armed.expect("a slow excursion arms");
        assert!(
            arm.rate_ppm.abs() < 1.0 && arm.mean_us >= EXCURSION_ARM_US,
            "{arm:?}"
        );
    }

    #[test]
    fn an_error_the_loop_is_retiring_never_arms_204() {
        let mut b = warmed();
        // 500 µs retired at 2 µs/s: shrinking, past the bound all the way.
        for n in 0..400 {
            assert_eq!(
                b.observe(500.0 - 2.0 * n as f64 * 0.5, 0.5, true),
                None,
                "window {n}"
            );
        }
        assert_eq!(b.gain(), None);
    }

    #[test]
    fn a_re_arm_while_returning_keeps_the_gain_continuous_204() {
        let mut b = warmed();
        for _ in 0..40 {
            b.observe(EXCURSION_ARM_US + 5.0, 0.5, true);
        }
        // Quiet until it is well into its return.
        while b.boost_s().expect("running") < EXCURSION_RAMP_IN_S + GM_BOOST_HOLD_S + 100.0 {
            b.observe(0.0, 0.5, true);
        }
        let before = b.gain().expect("running");
        assert!(before < GM_BOOST_GAIN && before > 1.0);
        let mut arm = None;
        for _ in 0..200 {
            let g = b.gain().expect("running");
            if let Some(a) = b.observe(EXCURSION_ARM_US + 15.0, 0.5, true) {
                arm = Some(a);
                let after = b.gain().expect("running");
                // The same gain the schedule reached this window (one window of its return).
                let one_window = GM_BOOST_GAIN.powf(0.5 / GM_BOOST_RAMP_S) - 1.0 + 1e-9;
                assert!(
                    (after / g - 1.0).abs() <= one_window,
                    "re-arm jumped {g} -> {after}"
                );
                break;
            }
        }
        assert!(arm.is_some(), "a standing error re-arms");
        assert!(arm.unwrap().gain_before > 1.0);
    }

    #[test]
    fn nothing_arms_within_the_holdoff_after_an_engagement_204() {
        let mut b = ExcursionBoost::default();
        let mut armed_at = None;
        for n in 0..2_000 {
            if b.observe(EXCURSION_ARM_US + 10.0, 0.5, true).is_some() {
                armed_at = Some(n as f64 * 0.5);
                break;
            }
        }
        let at = armed_at.expect("a standing excursion arms after the holdoff");
        assert!(
            at >= EXCURSION_ENGAGE_HOLDOFF_S,
            "armed {at}s after the engagement"
        );
        // A restart (a re-anchor) starts the holdoff again.
        b.restart_mean();
        for _ in 0..100 {
            b.observe(EXCURSION_ARM_US + 10.0, 0.5, true);
        }
        assert_eq!(b.arms(), 1);
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

    #[test]
    fn a_window_no_boot_fit_allows_arms_nothing_204() {
        let mut b = warmed();
        for _ in 0..200 {
            assert_eq!(b.observe(100.0, 0.5, false), None);
        }
        assert_eq!(b.gain(), None);
        assert_eq!(b.arms(), 0);
        b.end();
        assert_eq!(b.mean_us(), None);
    }
}
