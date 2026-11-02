//! dsync#146 — the frequency the FIRST engagement starts from, measured on the acquisition.
//!
//! The first engagement keeps the early anchor and retires the acquisition drift along the pull
//! reference (#134, τ = [`super::FSTEP_PULL_TAU_S`]). The pull only follows `e` when the integrator
//! starts on the right frequency; it used to start on the rate servo's word, which locks at
//! |rate| < 5 µs/s and so may be several ppm off. Each ppm off is a µs per second the phase walks
//! away from the reference, which the ~100 s PI then retires: the closed-loop sim of a cold gen3
//! boot stalled at e ≈ −117 µs for minutes (`controller/date_sync/lock_sim_tests.rs`).
//!
//! The acquisition itself measures the frequency: between the anchor and the lock the core sees
//! every window's `e` and the word the rate servo applied, so the OPEN-LOOP phase
//! `p = e − ∫ word dt` (the camera-box issue 1372 detector's quantity) has the oscillator's rate
//! against the grandmaster as its slope, whatever the servo commanded. The engagement starts the
//! integrator on `−slope(p)` over the last [`BOOT_FIT_WINDOW_S`] when that fit is better than
//! [`BOOT_FIT_MAX_SIGMA_PPM`]; otherwise (a seeded boot locks seconds after the anchor, too short
//! a ring) on the rate servo's word, as before.
//!
//! The word in force between two window medians is half the previous word and half the last one
//! (a median sits mid-window, the #142 judge's rule): during the acquisition the word moves a lot
//! from window to window, so the ring integrates their average.

use super::freq_step::{fit_line, LineFit};
use super::{DT_MAX_S, DT_MIN_S};
use std::collections::VecDeque;

/// Seconds of acquisition windows the frequency is fitted on.
pub const BOOT_FIT_WINDOW_S: f64 = 20.0;

/// The fit seeds the integrator only when its slope's standard error is at most this (ppm).
pub const BOOT_FIT_MAX_SIGMA_PPM: f64 = 1.0;

/// A first engagement that started on a SHORT ring (a seeded boot locks seconds after the anchor)
/// keeps fitting through the engagement, and once the ring spans [`BOOT_FIT_WINDOW_S`] re-seeds
/// the integrator ONCE when the fit differs from it by at least this (ppm) and 3 σ. Without it a
/// hand-over a few ppm off walks `e` out by ~37 µs per ppm through the ~100 s PI (peak at ~100 s):
/// past the 150 µs unlock bound from ~4 ppm — a [41] flap seconds after the first lock.
pub const BOOT_RESEED_MIN_PPM: f64 = 0.5;

/// The frequency measured on the acquisition, as the engagement used it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BootFit {
    /// The word that holds `e` still: minus the open-loop phase's slope (ppm).
    pub freq_ppm: f64,
    pub sigma_ppm: f64,
    pub points: usize,
    pub span_s: f64,
}

/// The open-loop phase of the acquisition windows.
#[derive(Clone, Debug, Default)]
pub struct BootFreqFit {
    ring: VecDeque<(f64, f64)>,
    clock_s: f64,
    applied_us: f64,
    /// The last two words the rate servo applied (ppm), oldest first.
    words: Option<(f64, f64)>,
}

impl BootFreqFit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start again (a new time base, an unconfirmed anchor).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn points(&self) -> usize {
        self.ring.len()
    }

    /// Seconds the ring spans.
    pub fn span_s(&self) -> f64 {
        match (self.ring.front(), self.ring.back()) {
            (Some(&(first, _)), Some(&(last, _))) => last - first,
            _ => 0.0,
        }
    }

    /// The ring spans the whole fit window.
    pub fn full(&self) -> bool {
        self.span_s() >= BOOT_FIT_WINDOW_S - 1e-6
    }

    /// One anchored acquisition window: its phase error, the seconds since the previous window
    /// (grandmaster time) and the word the rate servo applies from now on.
    pub fn observe(&mut self, e_us: f64, dt_s: f64, word_ppm: f64) {
        if !e_us.is_finite() || !word_ppm.is_finite() {
            self.reset();
            return;
        }
        match self.words {
            Some((older, last)) if (DT_MIN_S..=DT_MAX_S).contains(&dt_s) => {
                self.clock_s += dt_s;
                self.applied_us += (older + last) / 2.0 * dt_s;
                let now = self.clock_s;
                self.ring.push_back((now, e_us - self.applied_us));
                while self
                    .ring
                    .front()
                    .is_some_and(|&(t, _)| now - t > BOOT_FIT_WINDOW_S + 1e-9)
                {
                    self.ring.pop_front();
                }
                self.words = Some((last, word_ppm));
            }
            Some(_) => {
                // A gap the ring cannot represent: start again from this window.
                self.reset();
                self.ring.push_back((0.0, e_us));
                self.words = Some((word_ppm, word_ppm));
            }
            None => {
                self.ring.push_back((0.0, e_us));
                // The first window's span ran on the same word before and after it.
                self.words = Some((word_ppm, word_ppm));
            }
        }
    }

    /// The frequency to start the integrator on, when the fit is good enough.
    pub fn estimate(&self) -> Option<BootFit> {
        let line: LineFit = fit_line(self.ring.iter())?;
        (line.sigma_ppm <= BOOT_FIT_MAX_SIGMA_PPM && line.slope_ppm.is_finite()).then_some(
            BootFit {
                freq_ppm: -line.slope_ppm,
                sigma_ppm: line.sigma_ppm,
                points: line.points,
                span_s: line.span_s,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 0.5;

    /// A rate servo stand-in converging on `-osc` while `e` integrates `osc + word`.
    fn acquisition(osc: f64, windows: usize, jitter: impl Fn(usize) -> f64) -> BootFreqFit {
        let mut f = BootFreqFit::new();
        let mut e = 0.0f64;
        let mut word_prev = 0.0f64;
        let mut word = 0.0f64;
        for n in 0..windows {
            // The median sits mid-window: half the interval under each word.
            e += (osc + (word_prev + word) / 2.0) * DT;
            let next = -osc * (1.0 - (-((n + 1) as f64) * DT / 10.0).exp()) * 0.9;
            f.observe(e + jitter(n), DT, next);
            word_prev = word;
            word = next;
        }
        f
    }

    #[test]
    fn it_measures_the_oscillator_under_a_moving_word_146() {
        for osc in [-35.0, 21.0, 70.0] {
            let f = acquisition(osc, 120, |_| 0.0);
            let est = f.estimate().expect("estimate");
            assert!((est.freq_ppm + osc).abs() < 0.01, "osc {osc}: {est:?}");
            assert!((est.span_s - BOOT_FIT_WINDOW_S).abs() < 1e-6);
            assert_eq!(est.points, 41);
        }
    }

    #[test]
    fn a_short_or_noisy_ring_gives_no_estimate_146() {
        // Under the fit's minimum points (a seeded boot engages seconds after the anchor).
        assert_eq!(acquisition(-35.0, 8, |_| 0.0).estimate(), None);
        // ±300 µs alternating noise: the slope's error is beyond the bound.
        let f = acquisition(-35.0, 120, |n| if n % 2 == 0 { 300.0 } else { -300.0 });
        assert_eq!(f.estimate(), None);
    }

    #[test]
    fn a_gap_or_a_reset_starts_the_ring_again_146() {
        let mut f = acquisition(-35.0, 60, |_| 0.0);
        assert!(f.points() > 12);
        f.observe(0.0, DT_MAX_S * 2.0, 35.0);
        assert_eq!(f.points(), 1);
        f.reset();
        assert_eq!(f.points(), 0);
        f.observe(f64::NAN, DT, 35.0);
        assert_eq!(f.points(), 0);
    }
}
