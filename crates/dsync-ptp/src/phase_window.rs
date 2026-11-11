//! The glue around one phase-lock window, shared by the service's controller and the in-process
//! follower (dsync#136): how a window's `dt` is taken, when the phase lock may engage, the
//! [41] gate's verdict, the #146 word deviation, the rate servo's hand-back, and the PTP-outage
//! edge. Each consumer runs its own side effects between these steps (the service: the date
//! layer's anchor events, the step probe, the logs), but the arithmetic and the order live here
//! once.

use crate::lock_gate::{GateEvent, PhaseLockGate};
use crate::ptp_phase_lock::{FirstEngagement, PhaseLockCore, WindowOutcome};
use crate::rate_servo::WordDeviation;

/// The window's `dt` in GRANDMASTER time (`t1`), so loop scheduling and the clock's own steps
/// cannot distort it; `fallback_s` (the rate servo's span) for the first window. A grandmaster
/// change jumps it; `PhaseLockCore::on_window` clamps that one window.
pub fn window_dt(last_t1_ns: &mut Option<i64>, t1_ns: i64, fallback_s: f64) -> f64 {
    let dt = last_t1_ns
        .map(|prev| t1_ns.wrapping_sub(prev) as f64 / 1e9)
        .unwrap_or(fallback_s);
    *last_t1_ns = Some(t1_ns);
    dt
}

/// When the phase lock may own the word this window (`on_window`'s `ptp_locked`), never while
/// PTP is stale.
///
/// dsync#170: the FIRST engagement is a grandmaster-time decision on the acquisition fit
/// ([`PhaseLockCore::first_engagement_readiness`]), not the rate servo's lock: two followers on
/// one PC read the rate a few ns of stamp residue apart, its lock count split them by a window,
/// and one window of engagement split leaves ~1.25 µs between them at the gate's lock. A fit the
/// ring refuses (a noisy box), or a ring that keeps restarting for
/// `FIRST_ENGAGEMENT_OVERDUE_S`, falls back to the rate lock. Before the anchor (a locked window
/// anchors at once, #134) and for every re-engagement after it (an outage, a lock loss) the rate
/// lock decides, as before.
pub fn engage_on(
    core: &PhaseLockCore,
    rate_locked: bool,
    seed_accepted: bool,
    ptp_offline: bool,
) -> bool {
    if ptp_offline {
        return false;
    }
    if core.engaged_once() || core.anchor_ns().is_none() {
        return rate_locked;
    }
    match core.first_engagement_readiness(seed_accepted) {
        FirstEngagement::Fit | FirstEngagement::Seeded => true,
        FirstEngagement::Refused | FirstEngagement::Overdue => rate_locked,
        FirstEngagement::NotYet => false,
    }
}

/// One window's [41] verdict: the gate on `e`, engaged = the phase lock owns the word after it.
pub fn judge(
    gate: &mut PhaseLockGate,
    core: &PhaseLockCore,
    error_ns: Option<i64>,
) -> Option<GateEvent> {
    gate.on_window(core.engaged(), error_ns)
}

/// What the window leaves for the clock and the rate servo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowClose {
    /// The [41] gate's change, if any (the consumer logs it).
    pub gate_event: Option<GateEvent>,
    /// The frequency word to apply: the phase lock's while engaged, else the rate servo's.
    pub word_ppm: f64,
    /// The rate servo's new drift baseline: the learned frequency while engaged and on the
    /// window that disengaged (a bumpless hand-back); `None` = unchanged.
    pub baseline_ppm: Option<f64>,
    /// dsync#170: this window engaged the phase lock, so the rate servo ADOPTS the lock
    /// (`RateServo::adopt_lock`). An engagement on the fit can come before the rate servo's own
    /// lock; without the adoption the next window's `engage_on` (the rate lock, for a core that
    /// engaged once) would disengage it at once.
    pub adopt_rate_lock: bool,
}

/// Close a window after the consumer handled its anchor event: the gate's verdict on `e`, the
/// word deviation the rate servo's next measurement is decoupled from (#146), the word and the
/// hand-back.
pub fn close_window(
    core: &PhaseLockCore,
    gate: &mut PhaseLockGate,
    deviation: &mut WordDeviation,
    out: &WindowOutcome,
    was_engaged: bool,
    servo_word_ppm: f64,
) -> WindowClose {
    let gate_event = judge(gate, core, out.error_ns);
    deviation.note(out.freq_ppm, core.integrator_ppm());
    let (word_ppm, baseline_ppm) = match out.freq_ppm {
        Some(word) => (word, Some(core.integrator_ppm())),
        None if was_engaged => (servo_word_ppm, Some(core.integrator_ppm())),
        None => (servo_word_ppm, None),
    };
    WindowClose {
        gate_event,
        word_ppm,
        baseline_ppm,
        adopt_rate_lock: !was_engaged && core.engaged(),
    }
}

/// The PTP-offline edge's phase-lock part.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outage {
    /// The gate cleared (`Disengaged` when it was locked).
    pub gate_event: Option<GateEvent>,
    /// The learned frequency to hold through the outage, when the phase lock was engaged (not the
    /// last word, which carries its last P term).
    pub held_ppm: Option<f64>,
}

/// No PTP: [41] clears at once, the acquisition fit starts again after the outage (its `dt`
/// across it is not the outage's length), and an engaged phase lock disengages and hands back its
/// integrator.
pub fn on_outage(
    core: &mut PhaseLockCore,
    gate: &mut PhaseLockGate,
    deviation: &mut WordDeviation,
) -> Outage {
    deviation.clear();
    let gate_event = gate.reset();
    core.on_ptp_outage();
    let held_ppm = core.engaged().then(|| {
        core.disengage();
        core.integrator_ppm()
    });
    Outage {
        gate_event,
        held_ppm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp_phase_lock::AnchorEvent;

    fn outcome(freq_ppm: Option<f64>, error_ns: Option<i64>) -> WindowOutcome {
        WindowOutcome {
            freq_ppm,
            error_ns,
            event: AnchorEvent::None,
            freq_step: None,
        }
    }

    #[test]
    fn the_dt_is_grandmaster_time_after_the_first_window() {
        let mut last = None;
        assert_eq!(window_dt(&mut last, 5_000_000_000, 0.7), 0.7);
        assert_eq!(window_dt(&mut last, 5_500_000_000, 0.7), 0.5);
        assert_eq!(last, Some(5_500_000_000));
    }

    /// dsync#170: an acquiring core after `windows` unlocked 0.5 s windows (anchored on the third,
    /// where its acquisition ring starts), `e` alternating +-`noise_us`, the rate servo's word 0.
    fn acquiring(windows: usize, noise_us: f64) -> PhaseLockCore {
        let mut core = PhaseLockCore::new();
        for n in 0..windows {
            let e_us = if n % 2 == 0 { noise_us } else { -noise_us };
            core.on_window(5_000_000 + (e_us * 1_000.0) as i64, false, 0.0, 0.5);
        }
        assert!(core.anchor_ns().is_some() && !core.engaged_once());
        core
    }

    /// The windows after which the acquisition ring has run `s` seconds.
    fn ring_windows(s: f64) -> usize {
        crate::ptp_phase_lock::ANCHOR_CONFIRM_WINDOWS + (s / 0.5).round() as usize
    }

    #[test]
    fn the_phase_lock_never_engages_while_ptp_is_stale_and_before_the_anchor_on_the_rate_lock() {
        let core = PhaseLockCore::new();
        assert!(engage_on(&core, true, false, false));
        assert!(!engage_on(&core, true, false, true));
        assert!(!engage_on(&core, false, false, false));
        // A seed changes nothing before the anchor (a locked window anchors at once, #134).
        assert!(!engage_on(&core, false, true, false));
        let fit = acquiring(ring_windows(25.0), 1.0);
        assert!(!engage_on(&fit, true, true, true), "stale: never");
    }

    /// dsync#170: the FIRST engagement is the acquisition ring's readiness, in grandmaster time,
    /// whatever the rate servo's lock says, except a fit the ring refuses, where the rate lock
    /// decides as before.
    #[test]
    fn the_first_engagement_is_the_rings_readiness_not_the_rate_lock_170() {
        // A short ring: not yet, even rate-locked.
        let short = acquiring(ring_windows(3.0), 1.0);
        assert_eq!(
            short.first_engagement_readiness(false),
            FirstEngagement::NotYet
        );
        assert!(!engage_on(&short, true, false, false));
        // A seed accepted: engaged at BOOT_SEEDED_ENGAGE_S of ring, not a window before.
        let seeded_s = crate::ptp_phase_lock::BOOT_SEEDED_ENGAGE_S;
        let before = acquiring(ring_windows(seeded_s) - 1, 1.0);
        assert_eq!(
            before.first_engagement_readiness(true),
            FirstEngagement::NotYet
        );
        assert!(!engage_on(&before, true, true, false));
        let seeded = acquiring(ring_windows(seeded_s), 1.0);
        assert_eq!(
            seeded.first_engagement_readiness(true),
            FirstEngagement::Seeded
        );
        assert!(
            engage_on(&seeded, false, true, false),
            "no rate lock needed"
        );
        assert!(
            !engage_on(&seeded, true, false, false),
            "without a seed: the fit"
        );
        // The full 20 s ring with a good fit: engaged, rate-locked or not.
        let full_s = crate::ptp_phase_lock::BOOT_FIT_WINDOW_S;
        let not_full = acquiring(ring_windows(full_s) - 1, 1.0);
        assert_eq!(
            not_full.first_engagement_readiness(false),
            FirstEngagement::NotYet
        );
        let fit = acquiring(ring_windows(full_s), 1.0);
        assert_eq!(fit.first_engagement_readiness(false), FirstEngagement::Fit);
        assert!(engage_on(&fit, false, false, false));
        // A full ring the fit refuses (sigma > 1 ppm): the rate lock decides.
        let noisy = acquiring(ring_windows(full_s), 70.0);
        assert_eq!(
            noisy.first_engagement_readiness(false),
            FirstEngagement::Refused
        );
        assert!(engage_on(&noisy, true, false, false));
        assert!(!engage_on(&noisy, false, false, false));
    }

    /// dsync#170 (review): a ring that keeps restarting (here a window gap beyond `DT_MAX_S`
    /// every 10 s) never becomes ready; after `FIRST_ENGAGEMENT_OVERDUE_S` anchored the rate lock
    /// decides the first engagement again, as before #170.
    #[test]
    fn a_ring_that_keeps_restarting_hands_the_first_engagement_back_to_the_rate_lock_170() {
        let mut core = acquiring(crate::ptp_phase_lock::ANCHOR_CONFIRM_WINDOWS, 1.0);
        let mut acquired_s = 0.0;
        while acquired_s < crate::ptp_phase_lock::FIRST_ENGAGEMENT_OVERDUE_S - 10.0 {
            assert!(!engage_on(&core, true, false, false), "at {acquired_s}s");
            for _ in 0..10 {
                core.on_window(5_000_000, false, 0.0, 0.5);
            }
            core.on_window(5_000_000, false, 0.0, 6.0);
            acquired_s += 10.0;
        }
        assert_eq!(
            core.first_engagement_readiness(false),
            FirstEngagement::NotYet
        );
        for _ in 0..30 {
            core.on_window(5_000_000, false, 0.0, 0.5);
            core.on_window(5_000_000, false, 0.0, 6.0);
        }
        assert_eq!(
            core.first_engagement_readiness(false),
            FirstEngagement::Overdue
        );
        assert!(engage_on(&core, true, false, false));
        assert!(!engage_on(&core, false, false, false));
    }

    /// dsync#170: once engaged, every later engagement (after an outage or a lock loss) is the
    /// rate lock's, as before; the engaging window asks the rate servo to adopt the lock.
    #[test]
    fn a_re_engagement_is_the_rate_locks_and_the_engaging_window_adopts_it_170() {
        let mut core = acquiring(ring_windows(crate::ptp_phase_lock::BOOT_FIT_WINDOW_S), 1.0);
        let (mut gate, mut dev) = (PhaseLockGate::new(), WordDeviation::default());
        assert!(engage_on(&core, false, false, false));
        let out = core.on_window(5_000_000, true, 0.0, 0.5);
        assert!(core.engaged());
        let close = close_window(&core, &mut gate, &mut dev, &out, false, 0.0);
        assert!(
            close.adopt_rate_lock,
            "the engaging window adopts the rate lock"
        );
        let out = core.on_window(5_000_000, true, 0.0, 0.5);
        let close = close_window(&core, &mut gate, &mut dev, &out, true, 0.0);
        assert!(!close.adopt_rate_lock, "only the engaging window");
        core.disengage();
        assert!(core.engaged_once());
        assert!(engage_on(&core, true, false, false));
        assert!(
            !engage_on(&core, false, true, false),
            "the fit no longer decides"
        );
    }

    /// An engaged core: anchored on a locked window at 0.
    fn engaged_core(integrator_seed: f64) -> PhaseLockCore {
        let mut core = PhaseLockCore::new();
        core.on_window(0, true, integrator_seed, 0.5);
        assert!(core.engaged());
        core
    }

    #[test]
    fn an_engaged_window_applies_the_phase_lock_word_and_hands_the_learned_frequency_back() {
        let core = engaged_core(12.0);
        let (mut gate, mut dev) = (PhaseLockGate::new(), WordDeviation::default());
        let close = close_window(
            &core,
            &mut gate,
            &mut dev,
            &outcome(Some(13.0), Some(0)),
            true,
            99.0,
        );
        assert_eq!(close.word_ppm, 13.0);
        assert_eq!(close.baseline_ppm, Some(core.integrator_ppm()));
        assert_eq!(dev.ppm[1], 13.0 - core.integrator_ppm());
    }

    #[test]
    fn the_window_that_disengages_hands_back_once_and_then_the_rate_servo_owns_the_word() {
        let mut core = engaged_core(12.0);
        core.disengage();
        let (mut gate, mut dev) = (PhaseLockGate::new(), WordDeviation::default());
        let close = close_window(
            &core,
            &mut gate,
            &mut dev,
            &outcome(None, Some(0)),
            true,
            99.0,
        );
        assert_eq!((close.word_ppm, close.baseline_ppm), (99.0, Some(12.0)));
        let close = close_window(
            &core,
            &mut gate,
            &mut dev,
            &outcome(None, Some(0)),
            false,
            98.0,
        );
        assert_eq!((close.word_ppm, close.baseline_ppm), (98.0, None));
        assert_eq!(dev.ppm, [0.0, 0.0]);
    }

    #[test]
    fn the_gate_locks_on_engaged_windows_within_the_bound() {
        let core = engaged_core(0.0);
        let (mut gate, mut dev) = (PhaseLockGate::new(), WordDeviation::default());
        let mut events = Vec::new();
        for _ in 0..crate::lock_gate::LOCK_WINDOWS {
            let c = close_window(
                &core,
                &mut gate,
                &mut dev,
                &outcome(Some(0.0), Some(10_000)),
                true,
                0.0,
            );
            events.extend(c.gate_event);
        }
        assert_eq!(events, vec![GateEvent::Locked { error_ns: 10_000 }]);
    }

    #[test]
    fn an_outage_clears_the_gate_and_holds_the_integrator_of_an_engaged_lock() {
        let mut core = engaged_core(7.5);
        let mut gate = PhaseLockGate::new();
        for _ in 0..crate::lock_gate::LOCK_WINDOWS {
            judge(&mut gate, &core, Some(0));
        }
        assert!(gate.locked());
        let mut dev = WordDeviation { ppm: [1.0, 2.0] };
        let out = on_outage(&mut core, &mut gate, &mut dev);
        assert_eq!(out.gate_event, Some(GateEvent::Disengaged));
        assert_eq!(out.held_ppm, Some(core.integrator_ppm()));
        assert!(!core.engaged() && !gate.locked());
        assert_eq!(dev.span_ppm(), 0.0);
        // A second edge with nothing engaged holds nothing.
        assert_eq!(on_outage(&mut core, &mut gate, &mut dev).held_ppm, None);
    }
}
