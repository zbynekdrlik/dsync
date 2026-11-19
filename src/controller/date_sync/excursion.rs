//! dsync#204 — the controller side of the phase lock's excursion boost: one log line when a
//! growing phase excursion (a local-oscillator ramp the plain PI lags, e.g. a load warming the
//! crystal) starts the gain boost from the normal gains. The decision and the schedule live in the
//! pure core (`crate::ptp_phase_lock::excursion`); nothing here touches the clock, `D` or the date
//! layer.

use super::*;
use crate::ptp_phase_lock::{ExcursionArm, GM_BOOST_GAIN};

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// The boost started in this window: log it once with the values that decided it.
    pub(super) fn note_excursion(&self, arm: Option<ExcursionArm>) {
        let Some(x) = arm else {
            return;
        };
        info!(
            "[PHASE-LOCK] phase excursion: mean e {:+.1}us unlearned {:+.2}ppm, I {:+.3}ppm -- the PI \
             gains boosted toward x{} (target {:.2} of it, {} since start), no wall step, D unchanged",
            x.mean_us,
            x.unlearned_ppm,
            self.date_sync.core.integrator_ppm(),
            GM_BOOST_GAIN,
            x.target,
            self.date_sync.core.excursion_arms()
        );
    }
}
