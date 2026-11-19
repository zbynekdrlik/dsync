//! The PI word while a followed frequency step's phase is retired (camera-box issue 1372). Split
//! out of `ptp_phase_lock.rs` (dsync#210) with no behaviour change.

use super::{PhaseLockCore, ERROR_CLAMP_US, FREQ_CLAMP_PPM, FSTEP_PULL_DONE_US, FSTEP_PULL_TAU_S};

impl PhaseLockCore {
    /// The word while a followed step's phase is retired: the PI tracks `e − r` (so it sees no
    /// error when the phase follows the reference) and the word carries the reference's own rate
    /// `−r/τ` as feed-forward. The reference then decays by one window.
    pub(super) fn pulled_word(&mut self, e_raw_us: f64, dt: f64) -> f64 {
        let e_us = (e_raw_us - self.pull_us).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        let pull_rate = -self.pull_us / FSTEP_PULL_TAU_S;
        let (k_p, k_i) = self.gains();
        self.i_ppm = (self.i_ppm - k_i * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        let f = (self.i_ppm - k_p * e_us + pull_rate).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        self.pull_us *= (-dt / FSTEP_PULL_TAU_S).exp();
        if self.pull_us.abs() < FSTEP_PULL_DONE_US {
            self.pull_us = 0.0;
        }
        f
    }
}
