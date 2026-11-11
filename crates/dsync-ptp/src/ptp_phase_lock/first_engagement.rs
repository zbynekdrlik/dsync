//! dsync#170 — when the FIRST phase-lock engagement may start, decided in grandmaster time.
//!
//! Two followers on one PC read the same kernel stamps up to ~40 ns of conversion residue. The
//! first engagement used to be the rate servo's lock, a COUNT of |rate| < 5 µs/s windows: at that
//! edge the residue moved it by a window between the two, and one window of engagement split
//! leaves ≈ 50 µs × dt/τ ≈ 1.25 µs between them when [41] sets. The acquisition ring's age is a
//! sum of grandmaster `dt`s and its fit's σ reads 0.03-0.06 ppm against a 1 ppm gate, so both
//! followers decide the same window. The policy lives in `phase_window::engage_on`; the core only
//! answers this query ([`super::PhaseLockCore::first_engagement_readiness`]).

use super::boot_fit::BootFreqFit;

/// A seeded boot (an accepted frequency seed: the service's `drift.json`, a follower's
/// `with_frequency_seed`) engages once its acquisition ring has run this long in grandmaster
/// time. Its 20 s fit is not needed (the seed is the frequency, the short ring keeps fitting and
/// re-seeds once at 20 s).
///
/// The phase lock starts on the rate servo's word, and that word carries its ACQ P term on a
/// per-window rate: at 30 µs per-sample jitter, a few seconds after the anchor it is still ppm
/// off, and the boot pull then walked `e` past the [41] unlock bound (the whole-controller
/// `lock_sim`, 20 noise seeds × 7 seeds × 3 jitters: flaps in 1-15 of 20 at 2 / 4 / 6 s, none at
/// 8 s, none again over 40 fresh noise seeds; the rate-lock law before: none). A seeded quiet
/// box sets [41] at ~11 s instead of ~4 s; a seed 15 ppm off at ~19 s instead of ~33 s. Pinned by
/// `lock_sim_tests`' `a_seeded_boot_waits_for_the_seeded_ring_170`.
pub const BOOT_SEEDED_ENGAGE_S: f64 = 8.0;

/// The escape valve (review of #170): the acquisition ring restarts on every anchor event, a
/// window gap beyond `DT_MAX_S` and every PTP outage, so a grandmaster that flaps or a link that
/// drops during the acquisition could keep the first engagement `NotYet` for ever, where the
/// rate lock would have engaged. After this much grandmaster time anchored and never engaged
/// (counted outside the ring, so its restarts do not clear it) the rate lock decides again.
pub const FIRST_ENGAGEMENT_OVERDUE_S: f64 = 60.0;

/// Whether the FIRST engagement may start now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstEngagement {
    /// The acquisition ring ran the whole fit window and its fit holds (σ ≤
    /// `BOOT_FIT_MAX_SIGMA_PPM`): engage on it.
    Fit,
    /// The ring ran the whole fit window and the fit is refused (a noisy box): the rate servo's
    /// lock decides, as before #170.
    Refused,
    /// An accepted seed and [`BOOT_SEEDED_ENGAGE_S`] of ring: engage.
    Seeded,
    /// [`FIRST_ENGAGEMENT_OVERDUE_S`] anchored without a ready ring: the rate lock decides.
    Overdue,
    /// None of these yet (or a re-anchor is pending, whose window restarts the ring): wait.
    NotYet,
}

/// The readiness of an acquisition `ring` that has `acquire_s` of anchored grandmaster time
/// behind it; `rebase_pending`: the next window re-anchors (and restarts the ring), so nothing
/// engages before it.
pub(super) fn readiness(
    ring: &BootFreqFit,
    seed_accepted: bool,
    acquire_s: f64,
    rebase_pending: bool,
) -> FirstEngagement {
    if rebase_pending {
        FirstEngagement::NotYet
    } else if ring.full() {
        if ring.estimate().is_some() {
            FirstEngagement::Fit
        } else {
            FirstEngagement::Refused
        }
    } else if seed_accepted && ring.points() > 0 && ring.clock_s() >= BOOT_SEEDED_ENGAGE_S - 1e-6 {
        FirstEngagement::Seeded
    } else if acquire_s >= FIRST_ENGAGEMENT_OVERDUE_S - 1e-6 {
        FirstEngagement::Overdue
    } else {
        FirstEngagement::NotYet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring after `windows` 0.5 s windows of a constant `e` (+ `noise_us`, alternating).
    fn ring(windows: usize, noise_us: f64) -> BootFreqFit {
        let mut f = BootFreqFit::new();
        for n in 0..windows {
            let e = if n % 2 == 0 { noise_us } else { -noise_us };
            f.observe(e, 0.5, 0.0);
        }
        f
    }

    #[test]
    fn the_readiness_of_the_ring_in_grandmaster_time_170() {
        let short = ring(10, 1.0);
        assert_eq!(
            readiness(&short, false, 4.5, false),
            FirstEngagement::NotYet
        );
        let seeded = ring(17, 1.0);
        assert_eq!(seeded.clock_s(), 8.0);
        assert_eq!(
            readiness(&seeded, true, 8.0, false),
            FirstEngagement::Seeded
        );
        assert_eq!(
            readiness(&seeded, false, 8.0, false),
            FirstEngagement::NotYet
        );
        assert_eq!(
            readiness(&ring(16, 1.0), true, 7.5, false),
            FirstEngagement::NotYet
        );
        let full = ring(41, 1.0);
        assert_eq!(readiness(&full, false, 20.0, false), FirstEngagement::Fit);
        assert_eq!(
            readiness(&ring(41, 70.0), false, 20.0, false),
            FirstEngagement::Refused
        );
        // A pending re-anchor restarts the ring: never engage before it.
        assert_eq!(readiness(&full, true, 20.0, true), FirstEngagement::NotYet);
    }

    #[test]
    fn a_ring_that_keeps_restarting_hands_the_first_engagement_back_to_the_rate_lock_170() {
        let short = ring(5, 1.0);
        let just = FIRST_ENGAGEMENT_OVERDUE_S - 0.5;
        assert_eq!(
            readiness(&short, false, just, false),
            FirstEngagement::NotYet
        );
        let overdue = FIRST_ENGAGEMENT_OVERDUE_S;
        assert_eq!(
            readiness(&short, false, overdue, false),
            FirstEngagement::Overdue
        );
        assert_eq!(
            readiness(&short, false, overdue, true),
            FirstEngagement::NotYet
        );
        // A ready ring answers first.
        assert_eq!(
            readiness(&ring(41, 1.0), false, overdue, false),
            FirstEngagement::Fit
        );
    }
}
