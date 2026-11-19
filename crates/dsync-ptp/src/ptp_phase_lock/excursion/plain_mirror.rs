//! dsync#204 (v1.35.14) — the plain PI run on the real loop's open-loop phase.
//!
//! The excursion boost must be decided on a quantity its own gain does not move (two followers on
//! one PC otherwise split by µs: a gain step moved its own input before the loop could answer).
//! The real loop's tracked error obeys `d(tracked)/dt = osc + corr` (the plant, and the real
//! correction: the word minus the pull's feed-forward). The mirror's error obeys the same plant
//! with its own correction: each window it adds the tracked error's move, takes the real
//! correction of the interval back out and puts its own in.
//!
//! Its integrator is the real one plus the difference of the two learning laws, so a re-seed of
//! the real integrator (a followed frequency step, a boot or grandmaster fit) is the mirror's too:
//! otherwise a followed 25 ppm step read as 25 ppm the mirror still had to learn, and the boost
//! started under the next step (the bench's `back_to_back_steps_are_both_followed_1372`).
//!
//! Without the real word (a caller that gives none) the real loop is taken to be the plain PI:
//! the mirror's error is then the tracked error itself.
//!
//! The mirror leaves the true plain PI only where the real loop is clamped (the tracked error at
//! `ERROR_CLAMP_US`, the real integrator at `FREQ_CLAMP_PPM`). It stays bounded there: it is a
//! closed plain PI disturbed by a bounded correction, and those states end in a re-alignment
//! (a restart of the mirror) anyway.

use super::super::{K_I_PER_S2, K_P_PER_S};
use super::LoopWord;

#[derive(Clone, Copy, Debug)]
pub(super) struct PlainMirror {
    /// The mirror's error (µs).
    e_us: f64,
    /// Its integrator (ppm), and the word for the interval after the last window.
    i_ppm: f64,
    word_ppm: f64,
    /// The mirror's integrator minus the real one (ppm).
    i_offset_ppm: f64,
    /// The last tracked error (µs) and the real correction for the interval after it (ppm).
    last_tracked_us: f64,
    last_corr_ppm: Option<f64>,
    /// The two corrections' difference (µs) over the late windows since the last one taken.
    pending_us: f64,
}

impl PlainMirror {
    /// The mirror starts on the tracked error and the real integrator.
    pub(super) fn new(tracked_us: f64, real: Option<LoopWord>) -> Self {
        let i_ppm = real.map_or(0.0, |w| w.i_ppm);
        PlainMirror {
            e_us: tracked_us,
            i_ppm,
            word_ppm: i_ppm - K_P_PER_S * tracked_us,
            i_offset_ppm: 0.0,
            last_tracked_us: tracked_us,
            last_corr_ppm: real.map(|w| w.corr_ppm),
            pending_us: 0.0,
        }
    }

    /// The mirror's error (µs).
    pub(super) fn e_us(&self) -> f64 {
        self.e_us
    }

    /// The interval since the last window: the mirror's word acted instead of the real one.
    fn swap_us(&self, dt: f64) -> f64 {
        self.last_corr_ppm
            .map_or(0.0, |corr| (self.word_ppm - corr) * dt)
    }

    /// One window taken: the tracked error, its grandmaster seconds, and the real word after it.
    /// Returns the mirror's error (µs).
    pub(super) fn take(&mut self, tracked_us: f64, dt: f64, real: Option<LoopWord>) -> f64 {
        self.e_us += tracked_us - self.last_tracked_us + self.pending_us + self.swap_us(dt);
        self.pending_us = 0.0;
        self.last_tracked_us = tracked_us;
        match real {
            Some(w) => {
                self.i_offset_ppm += (w.k_i * tracked_us - K_I_PER_S2 * self.e_us) * dt;
                self.i_ppm = w.i_ppm + self.i_offset_ppm;
            }
            None => self.i_ppm -= K_I_PER_S2 * self.e_us * dt,
        }
        self.word_ppm = self.i_ppm - K_P_PER_S * self.e_us;
        self.last_corr_ppm = real.map(|w| w.corr_ppm);
        self.e_us
    }

    /// A late window: no measurement; the real word and the mirror's are both held through it.
    pub(super) fn skip(&mut self, dt: f64) {
        self.pending_us += self.swap_us(dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dsync#204 (v1.35.14): under a real loop at ×4 (and a re-seed of its integrator), the
    /// mirror's error is the error a plain PI holds on the same clock, to float rounding: the
    /// boost's decision input is no function of the gain in force.
    #[test]
    fn the_mirror_is_the_plain_pi_whatever_gain_the_real_loop_runs_204() {
        let (dt, g) = (1.016, 4.0);
        let (mut e_real, mut i_real) = (0.0f64, 0.0f64);
        let (mut e_plain, mut i_plain) = (0.0f64, 0.0f64);
        let mut mirror: Option<PlainMirror> = None;
        let mut worst = 0.0f64;
        for n in 0..1_500 {
            let t = n as f64 * dt;
            let osc = -3.8 + 6.0 * (1.0 - (-t / 300.0).exp());
            // A followed frequency step re-seeds the real integrator; the plain PI takes it too.
            if n == 700 {
                i_real += 5.0;
                i_plain += 5.0;
            }
            let k_i = K_I_PER_S2 * g * g;
            i_real -= k_i * e_real * dt;
            let w_real = i_real - K_P_PER_S * g * e_real;
            i_plain -= K_I_PER_S2 * e_plain * dt;
            let w_plain = i_plain - K_P_PER_S * e_plain;
            let word = Some(LoopWord {
                corr_ppm: w_real,
                i_ppm: i_real,
                k_i,
            });
            let e_mirror = match mirror.as_mut() {
                Some(m) => m.take(e_real, dt, word),
                None => mirror.insert(PlainMirror::new(e_real, word)).e_us(),
            };
            worst = worst.max((e_mirror - e_plain).abs());
            e_real += (osc + w_real) * dt;
            e_plain += (osc + w_plain) * dt;
        }
        assert!(e_plain.abs() > 1.0, "the plain PI held no error: {e_plain}");
        assert!(worst < 1e-6, "the mirror left the plain PI by {worst} µs");
    }

    /// A late window holds both words: the mirror counts the difference over it.
    #[test]
    fn a_late_window_holds_both_words_204() {
        let word = |corr_ppm| {
            Some(LoopWord {
                corr_ppm,
                i_ppm: 0.0,
                k_i: K_I_PER_S2,
            })
        };
        let mut m = PlainMirror::new(0.0, word(2.0));
        m.skip(1.0);
        // Over the late window and the next one the real word (+2 ppm) alone moved the tracked
        // error by 4 µs (no oscillator error): the mirror's word (0) would have moved nothing.
        let e = m.take(4.0, 1.0, word(2.0));
        assert!(e.abs() < 1e-12, "{e}");
        // Without the late window's share the mirror would read 2 µs.
        let mut m = PlainMirror::new(0.0, word(2.0));
        let e = m.take(4.0, 1.0, word(2.0));
        assert!((e - 2.0).abs() < 1e-12, "{e}");
    }
}
