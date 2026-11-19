//! One PI window of the phase lock: the late-window hold, the pulled or plain PI word, then the
//! excursion detector (dsync#204) and the #166 boost's clock. Split out of `ptp_phase_lock.rs`
//! (dsync#210) with no behaviour change.

use super::excursion::{self, ExcursionArm};
use super::{
    PhaseLockCore, ERROR_CLAMP_US, FREQ_CLAMP_PPM, FSTEP_PULL_TAU_S, GM_BOOST_HOLD_S,
    GM_BOOST_RAMP_S,
};

impl PhaseLockCore {
    /// One window's word: the late-window hold, the pulled PI or the plain PI (with the gains in
    /// force), then the excursion detector (dsync#204) and the #166 boost's clock. Returns the
    /// word and a boost the excursion started in this window.
    pub(super) fn pi_window(
        &mut self,
        e_raw_us: f64,
        dt: f64,
        late: bool,
        armable: bool,
    ) -> (f64, Option<ExcursionArm>) {
        let tracked_us = (e_raw_us - self.pull_us).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        let feed_forward = -self.pull_us / FSTEP_PULL_TAU_S;
        let f = if late {
            self.late_held += 1;
            self.late_windows = self.late_windows.saturating_add(1);
            self.last_freq_ppm
        } else if self.pull_us != 0.0 {
            self.late_held = 0;
            self.last_taken_e_us = Some(e_raw_us);
            self.pulled_word(e_raw_us, dt)
        } else {
            self.late_held = 0;
            self.last_taken_e_us = Some(e_raw_us);
            let e_us = e_raw_us.clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
            let (k_p, k_i) = self.gains();
            // offset = local − master: a fast local clock GROWS e, so the correction is negative.
            self.i_ppm = (self.i_ppm - k_i * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
            (self.i_ppm - k_p * e_us).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM)
        };
        // dsync#204: a window the PI took feeds the excursion mean (after its word, so the gains
        // move from the next window on).
        let excursion = if late {
            // The schedule still runs on a late window (as #166's clock), with no new target.
            self.excursion.advance(dt);
            None
        } else {
            self.excursion.set_loop_word(excursion::LoopWord {
                corr_ppm: f - feed_forward,
                i_ppm: self.i_ppm,
                k_i: self.gains().1,
            });
            self.excursion.observe(tracked_us, dt, armable)
        };
        // dsync#166: the boost's clock is the engaged windows' grandmaster time (late ones
        // included), so it is the same on every box that took the same windows.
        if let Some(t) = self.boost_s {
            let t = t + dt;
            self.boost_s = (t < GM_BOOST_HOLD_S + GM_BOOST_RAMP_S).then_some(t);
        }
        (f, excursion)
    }
}
