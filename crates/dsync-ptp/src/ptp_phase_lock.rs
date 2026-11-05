//! dsync#117 — the PTP PHASE LOCK: rate AND phase from the grandmaster, nothing else.
//!
//! # The owner contract (issue #117, ROZHODNUTÉ issuecomment-5836853146)
//!
//! RATE = the PTPv1 tick only; NTP = date stepping only. Every PC running dsync must tick
//! exactly with the grandmaster, and all PCs must show the same wall time. Before #117 the PTP
//! servo was RATE-ONLY: it drove `d(offset)/dt` to zero and never looked at the offset itself
//! (`initial_epoch_offset_ns` was written once and never read, and NANO mode ignored rates below a
//! 0.1 µs/s deadband). Rate-only is not phase: two boxes at "the same rate" random-walk apart
//! (0.1 ppm uncorrected = 360 µs/h), so cross-box agreement was held by NTP — through #97's
//! `phase_slew`, which steered the RATE by up to ±5-19 ppm away from the grandmaster tick.
//!
//! # What this module does
//!
//! It holds the system clock on `wall = PTP_time + D`, where `D` is the fleet date offset
//! (`crate::date_offset`). The error it drives to zero is
//!
//! ```text
//! e = (t2 − t1) − D          t1 = grandmaster send time (PTP), t2 = local receive time (wall)
//! ```
//!
//! with a PI controller whose integrator IS the learned oscillator frequency error:
//!
//! ```text
//! f = −(K_P·e + I),   I ← I + K_I·e·dt        (f in ppm = µs/s, e in µs)
//! ```
//!
//! It is ONE loop on ONE measurement (the PTP offset) giving both rate and phase — not a second
//! loop beside the rate servo — so the #97 decoupling proof reduces to a fact visible in the
//! signature of [`PhaseLockCore::on_window`]: no argument carries NTP. The only NTP-derived input
//! anywhere is `D`, and `D` changes only by a STEP of the wall of exactly the same size at the same
//! instant (`note_step`), which leaves `e` untouched. The bench (`tests/two_clock_bench.rs`)
//! proves it by running two UTC scenarios and asserting the frequency command sequences are
//! bit-identical.
//!
//! # Lifecycle
//!
//! - Acquisition stays on the existing PTP RATE servo in `controller.rs` (also pure PTP). This
//!   core engages once the controller reports PTP lock, taking over the frequency word
//!   bumplessly (its integrator starts at the rate servo's word), and hands the learned frequency
//!   back if lock is lost.
//! - `D` is anchored at the FIRST window, before the lock (dsync#134: `median(t2 − t1)` is an
//!   absolute offset of two time bases and needs no rate lock, so the date layer places the fleet
//!   date seconds after the first Sync instead of ~40 s later). The rate servo leaves the phase
//!   free while it acquires, so the wall drifts off that anchor by its rate error (~0.4 ms on a
//!   −21 ppm box, past 1 ms on a 50 ppm one). The FIRST engagement keeps the anchor within
//!   [`BOOT_HOLD_NS`] and retires the drift along the pull reference — no second step.
//! - `D` is anchored from the node's own wall (the boot step or the fleet date placed on it), replaced by
//!   the master's published `D` when the node follows the authority (`set_anchor`), shifted by
//!   every applied step (`note_step`), and RE-ANCHORED from the continuous wall when the
//!   grandmaster changes (`request_rebase`) or its time base jumps by more than
//!   [`DISCONTINUITY_NS`] (a grandmaster reboot keeps its UUID but restarts its uptime). A
//!   re-anchor never steps the wall: the new grandmaster's time base is simply adopted.
//! - The NANO 0.1 µs/s deadband has no equivalent here: the phase term corrects every residual,
//!   so nothing drifts freely.
//!
//! # A grandmaster FREQUENCY step (camera-box issue 1372, dsync slice)
//!
//! A PTP leader re-election moves the grandmaster's frequency by up to ~25 ppm at once, and the
//! slow PI above would take ~8-12 minutes to follow it (with a phase error near 1 ms on the way).
//! [`freq_step::FreqStepDetector`] measures the frequency the loop has not learned, from the
//! open-loop phase over the last 20 s of windows, and confirms a clean step. The core then:
//!
//! - re-seeds the integrator by the measured error (bounded to [`FSTEP_MAX_PPM`] per event), so
//!   the word follows the step at once;
//! - retires the phase the step left along a decaying reference ([`FSTEP_PULL_TAU_S`]): the PI
//!   tracks `e − r` and the word carries the reference's own rate `−r/τ`, so the recovery neither
//!   disturbs the integrator nor overshoots;
//! - never steps the wall and never touches `D`.
//!
//! Without a confirmed step nothing changes: the words are bit-identical to the plain PI.
//!
//! Pure (explicit inputs, no I/O, no logging) so the controller and the bench run the same code.

pub mod boot_fit;
pub mod freq_step;

pub use boot_fit::{
    BootFit, BootFreqFit, BOOT_FIT_MAX_SIGMA_PPM, BOOT_FIT_WINDOW_S, BOOT_REFINE_MIN_PPM,
    BOOT_REFINE_WINDOW_S, BOOT_RESEED_MIN_PPM,
};

pub use freq_step::{
    fit_line, fit_ring, split_test, FreqStepDetector, FreqStepEstimate, LineFit, RingFit,
    SplitTest, FSTEP_CONFIRM, FSTEP_HOLDOFF_S, FSTEP_LINEARITY_F_MAX, FSTEP_MAX_PPM, FSTEP_MIN_PPM,
    FSTEP_SIGMAS, FSTEP_WINDOW_S,
};

/// Proportional gain, ppm per µs of phase error (i.e. 1/s).
///
/// Chosen with the integrator for a critically-damped second-order loop
/// (`s² + K_P·s + K_I`, ζ = K_P / (2·√K_I) = 1) with a ~100 s time constant. That is slow on
/// purpose: the error is a median of software-timestamped PTP samples (tens of µs of noise), and
/// every µs of noise the proportional term passes becomes 0.02 ppm of frequency jitter. The loop
/// only has to follow oscillator wander (fractions of a ppm per minute); a frequency ramp `r`
/// (ppm/s) leaves a steady phase error of `r / K_I` — 0.1 ppm/min ⇒ 17 µs.
pub const K_P_PER_S: f64 = 0.02;

/// Integral gain, ppm per µs·s (1/s²). `K_P² / 4` = critical damping.
pub const K_I_PER_S2: f64 = K_P_PER_S * K_P_PER_S / 4.0;

/// The phase error fed to the controller is clamped to ±this (µs), so a burst of bad samples, or
/// a large error on re-engagement, can command at most `K_P·clamp` = 40 ppm of proportional slew
/// and cannot wind the integrator up.
pub const ERROR_CLAMP_US: f64 = 2_000.0;

/// Output clamp (ppm) — the same ±500 ppm envelope the rate servo uses.
pub const FREQ_CLAMP_PPM: f64 = 500.0;

/// A measured `t2 − t1` further than this from `D` is not a phase error but a jump of the
/// grandmaster's time base (a GM reboot restarts its uptime under the same UUID). Re-anchor.
pub const DISCONTINUITY_NS: i64 = 1_000_000_000;

/// On (re-)ENGAGEMENT, a phase error larger than this (the clock free-ran while the rate servo
/// held it, e.g. through a lock loss) is not slewed back — at the proportional clamp that would
/// keep the rate off the PTP tick for minutes. `D` is re-anchored on the current offset instead
/// (the wall stays where it is), and the date layer re-aligns it with ONE step: a follower's
/// next authority poll sees its `D` differ and joins; the master rebases the fleet offset.
pub const REANCHOR_ON_ENGAGE_NS: i64 = 1_000_000;

/// dsync#146 — while ENGAGED, a JUMP of the error the PI tracks (`e` minus a running pull) by
/// more than [`REANCHOR_ON_ENGAGE_NS`] from one window to the next, that then stays beyond that
/// bound for this many consecutive windows (3 s), is a phase jump (another writer set the clock,
/// the grandmaster's time base jumped by less than [`DISCONTINUITY_NS`]), not something to slew:
/// the clamped PI would run ~40-140 ppm off the tick for minutes. `D` is re-anchored on the
/// current offset (`Realigned`, exactly what a re-engagement that far off does) and the
/// integrator goes back to its value before the jump. Only a JUMP arms it: an error that GROWS
/// past the bound is an unlearned frequency (a hand-over far off, a step), which the boot re-seed,
/// the frequency-step follow or the PI retire — re-aligning it would only repeat. Before #146 the
/// rate servo's unlock produced that re-engagement; an engaged phase lock now holds the rate lock.
pub const REALIGN_ENGAGED_WINDOWS: usize = 6;

/// dsync#134 — the FIRST engagement after the early anchor (the box anchored `D` at its first
/// window and placed the fleet date on it while the rate servo was still acquiring) keeps that
/// anchor up to this far off it (ns), and retires the acquisition drift along the pull reference
/// ([`FSTEP_PULL_TAU_S`]) instead of re-anchoring: a re-anchor here would be a SECOND date step,
/// seconds after the first, under every application that started on the placed date. Beyond it
/// (more than the error clamp) the [`REANCHOR_ON_ENGAGE_NS`] rule applies as before.
pub const BOOT_HOLD_NS: i64 = ERROR_CLAMP_US as i64 * 1_000;

/// dsync#134 (review of PR 135) — an UNLOCKED first anchor needs this many consecutive windows
/// whose medians agree within [`ANCHOR_AGREE_NS`] (one 4-sample window is not trusted alone: an
/// interim grandmaster at a cold power-up, a bad window). About 1.5 s. A LOCKED window anchors at
/// once, as before.
pub const ANCHOR_CONFIRM_WINDOWS: usize = 3;

/// How close (ns) the confirming windows' medians must be to the first of them.
pub const ANCHOR_AGREE_NS: i64 = 1_000_000;

/// `dt` is clamped to this range (s): a first update or a long gap must not scale the integrator
/// step by an unbounded interval.
pub const DT_MIN_S: f64 = 0.01;
pub const DT_MAX_S: f64 = 5.0;

/// Time constant (s) of the reference along which the phase a frequency step left is retired.
/// Its rate `−r/τ` is at most `ERROR_CLAMP_US / τ` = 100 ppm, inside the output clamp; 400 µs
/// (a 25 ppm step detected after ~20 s) is under 50 µs ~40 s later.
pub const FSTEP_PULL_TAU_S: f64 = 20.0;

/// Below this (µs) the pull reference is dropped (its rate is then < 0.03 ppm).
pub const FSTEP_PULL_DONE_US: f64 = 0.5;

/// dsync#136 — a window whose `e` jumps more than this (µs) from the last window the PI took is a
/// LATE window, held: the PI keeps its word and learns nothing from it. The [41] unlock bound,
/// ~8 σ of the steady window noise at 30 µs per-sample jitter. mbc-snv's USB Dante NIC delivers
/// 3 of a window's 4 Syncs ~0.5 ms late now and then; its +455…+504 µs windows each moved the
/// line 8-10 µs through the P term.
pub const LATE_WINDOW_JUMP_US: f64 = 150.0;

/// The escape valve: a jump that persists this many windows is the clock (a path change, a
/// re-anchor's aftermath), and the next window is taken whatever it reads.
pub const LATE_WINDOW_MAX_HOLD: usize = 3;

/// What happened to the anchor `D` in one window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnchorEvent {
    None,
    /// First anchor (the first PTP lock).
    Anchored {
        anchor_ns: i64,
    },
    /// Re-anchored from the continuous wall because the TIME BASE changed: a grandmaster change
    /// (`request_rebase`), or a > 1 s jump of the grandmaster's time (a reboot under the same
    /// UUID). The fleet date offset follows it into the new base (the authority shifts it by the
    /// same base shift), with no wall step anywhere.
    Rebased {
        old_ns: i64,
        new_ns: i64,
    },
    /// Re-anchored on (re-)engagement more than [`REANCHOR_ON_ENGAGE_NS`] off `D` in the SAME time
    /// base: this box's wall wandered (it free-ran through a lock loss, or took local NTP steps).
    /// The fleet date offset must NOT follow — this box re-aligns its own wall to it with one step.
    Realigned {
        old_ns: i64,
        new_ns: i64,
    },
}

/// A grandmaster frequency step the loop followed (camera-box issue 1372).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreqStep {
    /// The step (ppm): the grandmaster's frequency relative to this box moved by this, so the word
    /// follows it (positive = the grandmaster sped up, the word goes up). Minus the measured error,
    /// bounded to ±[`FSTEP_MAX_PPM`].
    pub step_ppm: f64,
    /// The frequency the loop had not learned, as measured (ppm): the unbounded `−step_ppm`.
    pub error_ppm: f64,
    /// The slope's standard error (ppm) and the linearity statistic of the confirming fit.
    pub sigma_ppm: f64,
    pub linearity_f: f64,
    /// Seconds of windows the confirming fit spanned.
    pub span_s: f64,
    pub integrator_before_ppm: f64,
    pub integrator_after_ppm: f64,
    /// The phase error the step left (µs), retired along the pull reference.
    pub pull_us: f64,
}

/// The result of one PTP sample window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowOutcome {
    /// The frequency word to apply (ppm) while engaged; `None` = not engaged, the rate servo's
    /// word applies.
    pub freq_ppm: Option<f64>,
    /// `e = (t2 − t1) − D` (ns) once anchored.
    pub error_ns: Option<i64>,
    pub event: AnchorEvent,
    /// A frequency step confirmed and followed in THIS window (the controller logs it).
    pub freq_step: Option<FreqStep>,
}

/// The phase-lock state of one box.
#[derive(Clone, Debug, Default)]
pub struct PhaseLockCore {
    anchor_ns: Option<i64>,
    rebase_pending: bool,
    engaged: bool,
    /// Integrator = the learned frequency word (ppm), i.e. minus the oscillator's error vs the GM.
    i_ppm: f64,
    last_error_ns: Option<i64>,
    last_freq_ppm: f64,
    /// camera-box issue 1372: the frequency-step detector (reads only `e` and the words).
    fstep: FreqStepDetector,
    /// The phase (µs) still being retired after a followed frequency step; 0 = none.
    pull_us: f64,
    freq_steps: u32,
    last_freq_step: Option<FreqStep>,
    /// dsync#134: the core has engaged at least once since start (the boot hold applies to
    /// the first engagement only).
    engaged_once: bool,
    /// dsync#134: the unlocked first anchor being confirmed — the first window's median and
    /// how many agreeing windows so far.
    anchor_candidate: Option<(i64, usize)>,
    /// dsync#146: the open-loop phase of the acquisition (anchored, never engaged), and the
    /// frequency the first engagement started from when it was measured there.
    boot_fit: BootFreqFit,
    engaged_on_fit: Option<BootFit>,
    /// dsync#146: the first engagement started on a short ring and is still fitting.
    boot_fit_open: bool,
    /// dsync#146: the once-only re-seed of that engagement, when it happened.
    boot_reseed: Option<BootFit>,
    /// dsync#146: consecutive engaged windows beyond the re-align bound, and the integrator
    /// before the first of them.
    far_windows: usize,
    i_before_far_ppm: f64,
    /// dsync#146: the tracked error (ns) of the last engaged window, to tell a jump.
    last_tracked_ns: Option<f64>,
    /// dsync#136: the `e` (µs) of the last window the PI took, the consecutive late windows held
    /// since, and every late window held since start.
    last_taken_e_us: Option<f64>,
    late_held: usize,
    late_windows: u64,
    /// dsync#136: the second, longer fit of the first engagement (opened when the first one
    /// closes), and the re-seed it made.
    refine: Option<BootFreqFit>,
    boot_refine: Option<BootFit>,
    /// dsync#136: the acquisition's last `e` taken into the boot fit, and its late windows in a row.
    boot_last_e_us: Option<f64>,
    boot_late_held: usize,
}

impl PhaseLockCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// `D` (ns), once anchored.
    pub fn anchor_ns(&self) -> Option<i64> {
        self.anchor_ns
    }

    /// True while this core owns the frequency word.
    pub fn engaged(&self) -> bool {
        self.engaged
    }

    /// dsync#134: anchored, but never engaged yet — the boot acquisition, the rate servo still
    /// owns the word. The date layer works on the anchor already (it is the fleet date line).
    pub fn acquiring(&self) -> bool {
        self.anchor_ns.is_some() && !self.engaged_once
    }

    /// The integrator — the learned frequency word (ppm).
    pub fn integrator_ppm(&self) -> f64 {
        self.i_ppm
    }

    /// True between a grandmaster change and the window that re-anchors on it: `D` still belongs
    /// to the OLD time base, so it must not be published as the fleet offset.
    pub fn rebase_pending(&self) -> bool {
        self.rebase_pending
    }

    pub fn last_error_ns(&self) -> Option<i64> {
        self.last_error_ns
    }

    pub fn last_freq_ppm(&self) -> f64 {
        self.last_freq_ppm
    }

    /// dsync#136: the refinement's re-seed of the first engagement, when it happened.
    pub fn boot_refine(&self) -> Option<BootFit> {
        self.boot_refine
    }

    /// dsync#136: the refinement is still fitting.
    pub fn refining(&self) -> bool {
        self.refine.is_some()
    }

    /// dsync#136: late windows the PI held since start.
    pub fn late_windows(&self) -> u64 {
        self.late_windows
    }

    /// Grandmaster frequency steps followed since start (camera-box issue 1372).
    pub fn freq_steps(&self) -> u32 {
        self.freq_steps
    }

    /// The last frequency step followed.
    pub fn last_freq_step(&self) -> Option<FreqStep> {
        self.last_freq_step
    }

    /// dsync#146: the acquisition fit the first engagement started its integrator on
    /// (`None`: it started on the rate servo's word).
    pub fn engaged_on_fit(&self) -> Option<BootFit> {
        self.engaged_on_fit
    }

    /// dsync#146: the first engagement's integrator re-seeded once its ring was full.
    pub fn boot_reseed(&self) -> Option<BootFit> {
        self.boot_reseed
    }

    /// The phase (µs) still being retired after the last followed step; 0 when none.
    pub fn pull_us(&self) -> f64 {
        self.pull_us
    }

    /// Adopt `D` from the date-offset authority (a join or an absorb). The wall is moved by the
    /// caller when the adoption is a step; the anchor simply becomes the authority's value.
    pub fn set_anchor(&mut self, anchor_ns: i64) {
        self.anchor_ns = Some(anchor_ns);
    }

    /// The wall was stepped by `delta_ns`: `D` moves by exactly the same amount, so `e` is
    /// unchanged and the loop sees no disturbance. No-op before the first anchor.
    pub fn note_step(&mut self, delta_ns: i64) {
        if let Some(a) = self.anchor_ns.as_mut() {
            *a = a.wrapping_add(delta_ns);
        }
    }

    /// This box lost PTP: stop owning the frequency word (the integrator and the anchor are kept).
    /// The next locked window re-engages bumplessly from the rate servo's word, and re-anchors
    /// (`Realigned`) if the wall free-ran more than [`REANCHOR_ON_ENGAGE_NS`] meanwhile.
    pub fn disengage(&mut self) {
        self.engaged = false;
        self.drop_step_state();
        self.close_boot_fit();
        self.refine = None;
    }

    /// The boot fit's line while it is open (diagnostics: its slope and σ, even when refused).
    pub fn boot_fit_line(&self) -> Option<LineFit> {
        (!self.engaged_once || self.boot_fit_open)
            .then(|| self.boot_fit.line())
            .flatten()
    }

    /// dsync#146: windows in the acquisition / first-engagement fit (0 = none running).
    pub fn boot_fit_points(&self) -> usize {
        self.boot_fit.points()
    }

    /// dsync#146: this box lost PTP. The fit's open-loop phase needs the TRUE elapsed time
    /// between windows, and the controller's `dt` after an outage is a fallback, not the outage's
    /// length: the fit starts again from the first window after it (review of #146).
    pub fn on_ptp_outage(&mut self) {
        self.close_boot_fit();
    }

    /// dsync#146: the first engagement's fit ends (resolved, or its engagement ended).
    fn close_boot_fit(&mut self) {
        self.boot_fit_open = false;
        self.boot_fit.reset();
    }

    /// The frequency-step ring and a running pull belong to one continuous engagement in one time
    /// base: dropped on a lock loss, an engagement and a re-anchor.
    fn drop_step_state(&mut self) {
        self.fstep.reset();
        self.pull_us = 0.0;
        self.last_taken_e_us = None;
        self.late_held = 0;
    }

    /// The grandmaster (or the sync source) changed: re-anchor `D` from the next window, so the
    /// wall stays continuous in the new grandmaster's time base.
    pub fn request_rebase(&mut self) {
        if self.anchor_ns.is_some() {
            self.rebase_pending = true;
        } else {
            // dsync#134: an early anchor being confirmed starts again in the new time base.
            self.anchor_candidate = None;
        }
    }

    /// Feed one PTP sample window.
    ///
    /// - `median_diff_ns` — the median of `t2 − t1` over the window (raw, NOT the mod-1 s display
    ///   phase and NOT calibration-corrected: `D` is an absolute offset between two time bases).
    /// - `ptp_locked` — the controller's PTP lock verdict (the rate servo's rate-stability lock).
    /// - `rate_servo_freq_ppm` — the word the rate servo would apply now; the integrator starts
    ///   from it on engagement so the hand-over is bumpless.
    /// - `dt_s` — seconds since the previous window.
    ///
    /// No argument carries NTP: this is the whole frequency law while engaged.
    pub fn on_window(
        &mut self,
        median_diff_ns: i64,
        ptp_locked: bool,
        rate_servo_freq_ppm: f64,
        dt_s: f64,
    ) -> WindowOutcome {
        let mut event = AnchorEvent::None;
        // dsync#134: the first engagement after the early anchor holds it (within the hold).
        let first_engagement = ptp_locked && !self.engaged && !self.engaged_once;
        let anchor = match self.anchor_ns {
            None => {
                // dsync#134: anchored at the first windows, locked or not. An unlocked anchor
                // needs ANCHOR_CONFIRM_WINDOWS agreeing windows; a locked window anchors at once.
                if !ptp_locked {
                    let count = match self.anchor_candidate {
                        Some((first, n))
                            if median_diff_ns.wrapping_sub(first).abs() <= ANCHOR_AGREE_NS =>
                        {
                            self.anchor_candidate = Some((first, n + 1));
                            n + 1
                        }
                        _ => {
                            self.anchor_candidate = Some((median_diff_ns, 1));
                            1
                        }
                    };
                    if count < ANCHOR_CONFIRM_WINDOWS {
                        return WindowOutcome {
                            freq_ppm: None,
                            error_ns: None,
                            event,
                            freq_step: None,
                        };
                    }
                }
                self.anchor_candidate = None;
                self.anchor_ns = Some(median_diff_ns);
                event = AnchorEvent::Anchored {
                    anchor_ns: median_diff_ns,
                };
                median_diff_ns
            }
            Some(a) => {
                let off = median_diff_ns.wrapping_sub(a).abs();
                if self.rebase_pending || off > DISCONTINUITY_NS {
                    self.rebase_pending = false;
                    self.anchor_ns = Some(median_diff_ns);
                    event = AnchorEvent::Rebased {
                        old_ns: a,
                        new_ns: median_diff_ns,
                    };
                    median_diff_ns
                } else if ptp_locked
                    && !self.engaged
                    && off > REANCHOR_ON_ENGAGE_NS
                    && !(first_engagement && off <= BOOT_HOLD_NS)
                {
                    self.anchor_ns = Some(median_diff_ns);
                    event = AnchorEvent::Realigned {
                        old_ns: a,
                        new_ns: median_diff_ns,
                    };
                    median_diff_ns
                } else {
                    a
                }
            }
        };

        let mut e_ns = median_diff_ns.wrapping_sub(anchor);
        // dsync#146: a phase jump while engaged is re-aligned, not slewed.
        if ptp_locked && self.engaged && event == AnchorEvent::None {
            let tracked_ns = e_ns as f64 - self.pull_us * 1_000.0;
            let bound = REANCHOR_ON_ENGAGE_NS as f64;
            let jumped = self
                .last_tracked_ns
                .is_some_and(|prev| (tracked_ns - prev).abs() > bound);
            self.last_tracked_ns = Some(tracked_ns);
            if tracked_ns.abs() <= bound {
                self.far_windows = 0;
            } else if self.far_windows > 0 || jumped {
                if self.far_windows == 0 {
                    self.i_before_far_ppm = self.i_ppm;
                }
                self.far_windows += 1;
            }
            if self.far_windows >= REALIGN_ENGAGED_WINDOWS {
                self.far_windows = 0;
                self.i_ppm = self.i_before_far_ppm;
                self.anchor_ns = Some(median_diff_ns);
                event = AnchorEvent::Realigned {
                    old_ns: anchor,
                    new_ns: median_diff_ns,
                };
                e_ns = 0;
                self.last_tracked_ns = None;
            }
        } else {
            self.far_windows = 0;
            self.last_tracked_ns = None;
        }
        self.last_error_ns = Some(e_ns);

        // dsync#146: the acquisition's open-loop phase, for the first engagement's frequency.
        if !self.engaged_once && !ptp_locked {
            if event != AnchorEvent::None {
                self.boot_fit.reset();
                self.boot_last_e_us = None;
                self.boot_late_held = 0;
            }
            // dsync#136: a late window enters the open-loop phase at the previous window's `e`
            // (it says nothing about the clock); the ∫ word dt stays exact.
            let e_us = e_ns as f64 / 1_000.0;
            let e_fit = match self.boot_last_e_us {
                Some(prev)
                    if (e_us - prev).abs() > LATE_WINDOW_JUMP_US
                        && self.boot_late_held < LATE_WINDOW_MAX_HOLD =>
                {
                    self.boot_late_held += 1;
                    prev
                }
                _ => {
                    self.boot_late_held = 0;
                    self.boot_last_e_us = Some(e_us);
                    e_us
                }
            };
            self.boot_fit.observe(e_fit, dt_s, rate_servo_freq_ppm);
        }

        if !ptp_locked {
            if self.engaged {
                // Hand the learned frequency back to the rate servo (the controller copies
                // `integrator_ppm` into its drift baseline).
                self.engaged = false;
                self.drop_step_state();
                self.close_boot_fit();
                self.refine = None;
            }
            return WindowOutcome {
                freq_ppm: None,
                error_ns: Some(e_ns),
                event,
                freq_step: None,
            };
        }

        if !self.engaged {
            self.engaged = true;
            // dsync#146: the first engagement after the early anchor starts on the frequency
            // the acquisition measured (when good enough), so the boot pull follows `e`.
            let fit = (first_engagement && event == AnchorEvent::None)
                .then(|| self.boot_fit.estimate())
                .flatten();
            if first_engagement {
                self.engaged_on_fit = fit;
                // A short ring keeps fitting through the engagement (re-seeded once when full).
                self.boot_fit_open = event == AnchorEvent::None && !self.boot_fit.full();
                // dsync#136: a full ring starts the longer refinement at once.
                if event == AnchorEvent::None && !self.boot_fit_open {
                    self.refine = Some(BootFreqFit::with_window(BOOT_REFINE_WINDOW_S));
                }
            }
            if !self.boot_fit_open {
                self.boot_fit.reset();
            }
            self.engaged_once = true;
            let start = fit.map_or(rate_servo_freq_ppm, |f| f.freq_ppm);
            self.i_ppm = start.clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
            self.drop_step_state();
        }
        if event != AnchorEvent::None {
            self.drop_step_state();
        }
        if first_engagement && event == AnchorEvent::None {
            // dsync#134: the boot hold — the acquisition drift off the early anchor is retired
            // along the pull reference (the PI tracks `e − r`, the word carries `−r/τ`), so the
            // integrator starts undisturbed at the rate servo's word and the wall is not stepped.
            let e_us = e_ns as f64 / 1_000.0;
            if e_us.abs() >= FSTEP_PULL_DONE_US {
                self.pull_us = e_us.clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
            }
        }

        let dt = if dt_s.is_finite() {
            dt_s.clamp(DT_MIN_S, DT_MAX_S)
        } else {
            DT_MIN_S
        };
        let e_raw_us = e_ns as f64 / 1_000.0;
        // camera-box issue 1372: a confirmed grandmaster frequency step re-seeds the integrator.
        let freq_step = self
            .fstep
            .observe(e_raw_us, dt_s, self.i_ppm)
            .map(|est| self.follow_freq_step(est, e_raw_us));
        // dsync#136: a lone late window is held, not learned from (the escape valve takes a
        // jump that persists).
        let late = freq_step.is_none()
            && self.late_held < LATE_WINDOW_MAX_HOLD
            && self
                .last_taken_e_us
                .is_some_and(|taken| (e_raw_us - taken).abs() > LATE_WINDOW_JUMP_US);
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
            // offset = local − master: a fast local clock GROWS e, so the correction is negative.
            self.i_ppm =
                (self.i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
            (self.i_ppm - K_P_PER_S * e_us).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM)
        };
        self.fstep.note_word(f);
        self.last_freq_ppm = f;
        let disturbed = event != AnchorEvent::None || freq_step.is_some();
        // A late window enters the boot fits' open-loop phase at the last taken `e` (it says
        // nothing about the clock); the ∫ word dt stays exact.
        let e_fit = if late {
            self.last_taken_e_us.unwrap_or(e_raw_us)
        } else {
            e_raw_us
        };
        if self.refine.is_some() {
            self.continue_refine(e_fit, dt_s, f, disturbed);
        }
        if self.boot_fit_open {
            self.continue_boot_fit(e_fit, dt_s, f, disturbed);
            if !self.boot_fit_open && !disturbed {
                // dsync#136: the 20 s fit closed: the longer refinement takes over.
                self.refine = Some(BootFreqFit::with_window(BOOT_REFINE_WINDOW_S));
            }
        }
        WindowOutcome {
            freq_ppm: Some(f),
            error_ns: Some(e_ns),
            event,
            freq_step,
        }
    }

    /// dsync#146: one window of the first engagement's continued fit (`f` = the word just
    /// written). Once the ring is full: re-seed the integrator once if the fit says it is off, and
    /// retire the phase left along the pull reference (no wall step, `D` untouched).
    fn continue_boot_fit(&mut self, e_us: f64, dt_s: f64, f: f64, disturbed: bool) {
        if disturbed {
            // A re-anchor or a followed frequency step owns the frequency from here.
            self.close_boot_fit();
            return;
        }
        self.boot_fit.observe(e_us, dt_s, f);
        if !self.boot_fit.full() {
            return;
        }
        if let Some(fit) = self.boot_fit.estimate() {
            if self.reseed(fit, BOOT_RESEED_MIN_PPM, e_us) {
                self.boot_reseed = Some(fit);
            }
        }
        self.close_boot_fit();
    }

    /// dsync#136: one window of the refinement. Once it spans [`BOOT_REFINE_WINDOW_S`]: re-seed
    /// the integrator once if the fit says it is off by ≥ [`BOOT_REFINE_MIN_PPM`] and 3 σ
    /// (ramped, [`Self::reseed`]), the phase left retired along the pull reference (no wall step, `D` untouched).
    fn continue_refine(&mut self, e_fit_us: f64, dt_s: f64, f: f64, disturbed: bool) {
        let Some(fit) = self.refine.as_mut() else {
            return;
        };
        if disturbed {
            self.refine = None;
            return;
        }
        fit.observe(e_fit_us, dt_s, f);
        if !fit.full() {
            return;
        }
        if let Some(est) = fit.estimate() {
            if self.reseed(est, BOOT_REFINE_MIN_PPM, e_fit_us) {
                self.boot_refine = Some(est);
            }
        }
        self.refine = None;
    }

    /// A boot fit's re-seed, CONTINUOUS in the fit (dsync#136, PR #156 CI): the integrator
    /// moves by `w × (fit − integrator)` and the pull reference by `w × (e − pull)`, with `w`
    /// ramping 0 → 1 as the difference goes from half the threshold to the threshold
    /// (`min_ppm` or 3 σ, whichever is larger). An all-or-nothing re-seed turned a few-ns stamp
    /// residue at the edge into a 0.2 ppm split between two followers on one PC. Returns
    /// whether it moved anything.
    fn reseed(&mut self, fit: BootFit, min_ppm: f64, e_us: f64) -> bool {
        let threshold = min_ppm.max(3.0 * fit.sigma_ppm);
        let off = fit.freq_ppm - self.i_ppm;
        let w = ((off.abs() - threshold / 2.0) / (threshold / 2.0)).clamp(0.0, 1.0);
        if w == 0.0 {
            return false;
        }
        self.i_ppm = (self.i_ppm + w * off).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        let pull = self.pull_us + w * (e_us.clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US) - self.pull_us);
        self.pull_us = if pull.abs() >= FSTEP_PULL_DONE_US {
            pull
        } else {
            0.0
        };
        true
    }

    /// Follow a confirmed step: the integrator jumps by the measured error (bounded), and the phase
    /// error the step left becomes the pull reference. No wall step, `D` untouched.
    fn follow_freq_step(&mut self, est: FreqStepEstimate, e_us: f64) -> FreqStep {
        let before = self.i_ppm;
        let step_ppm = (-est.error_ppm).clamp(-FSTEP_MAX_PPM, FSTEP_MAX_PPM);
        self.i_ppm = (before + step_ppm).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        self.pull_us = e_us.clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        let followed = FreqStep {
            step_ppm,
            error_ppm: est.error_ppm,
            sigma_ppm: est.sigma_ppm,
            linearity_f: est.linearity_f,
            span_s: est.span_s,
            integrator_before_ppm: before,
            integrator_after_ppm: self.i_ppm,
            pull_us: self.pull_us,
        };
        self.freq_steps = self.freq_steps.saturating_add(1);
        self.last_freq_step = Some(followed);
        followed
    }

    /// The word while a followed step's phase is retired: the PI tracks `e − r` (so it sees no
    /// error when the phase follows the reference) and the word carries the reference's own rate
    /// `−r/τ` as feed-forward. The reference then decays by one window.
    fn pulled_word(&mut self, e_raw_us: f64, dt: f64) -> f64 {
        let e_us = (e_raw_us - self.pull_us).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        let pull_rate = -self.pull_us / FSTEP_PULL_TAU_S;
        self.i_ppm = (self.i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        let f = (self.i_ppm - K_P_PER_S * e_us + pull_rate).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        self.pull_us *= (-dt / FSTEP_PULL_TAU_S).exp();
        if self.pull_us.abs() < FSTEP_PULL_DONE_US {
            self.pull_us = 0.0;
        }
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 0.5;

    /// A closed-loop plant: `e` integrates the oscillator error plus the applied correction
    /// (µs/s = ppm), with `delay` windows of transport delay between command and effect (the
    /// phase-slew rule: a zero-delay sim hides oscillation).
    struct Plant {
        e_ns: f64,
        osc_ppm: f64,
        applied: std::collections::VecDeque<f64>,
    }

    impl Plant {
        fn new(osc_ppm: f64, delay: usize, initial_word: f64) -> Self {
            Plant {
                e_ns: 0.0,
                osc_ppm,
                applied: std::iter::repeat(initial_word).take(delay + 1).collect(),
            }
        }
        fn advance(&mut self, cmd: f64, dt: f64) {
            self.applied.push_back(cmd);
            let f = self.applied.pop_front().unwrap();
            self.e_ns += (self.osc_ppm + f) * dt * 1_000.0;
        }
    }

    const D: i64 = 1_789_000_000_000_000_000;

    fn run(core: &mut PhaseLockCore, plant: &mut Plant, n: usize, word0: f64) -> (f64, f64) {
        let mut max_late = 0.0f64;
        let mut last = word0;
        for i in 0..n {
            let out = core.on_window(D + plant.e_ns.round() as i64, true, word0, DT);
            last = out.freq_ppm.expect("engaged");
            plant.advance(last, DT);
            if i > n / 2 {
                max_late = max_late.max(plant.e_ns.abs());
            }
        }
        (max_late, last)
    }

    // ---- dsync#134: the date at the FIRST window, one step, no second one at the lock -----

    /// The windows of a box acquiring with the rate servo (not PTP-locked): the wall drifts off
    /// the early anchor by the rate error the servo has not removed yet.
    fn acquire(core: &mut PhaseLockCore, e_ns: i64) -> WindowOutcome {
        core.on_window(D + e_ns, false, 0.0, DT)
    }

    /// The early anchor at `D`: [`ANCHOR_CONFIRM_WINDOWS`] agreeing unlocked windows.
    fn anchor_early(core: &mut PhaseLockCore) {
        for n in 0..ANCHOR_CONFIRM_WINDOWS {
            let out = acquire(core, 0);
            if n + 1 < ANCHOR_CONFIRM_WINDOWS {
                assert_eq!(out.event, AnchorEvent::None, "window {n}: not yet");
            } else {
                assert_eq!(out.event, AnchorEvent::Anchored { anchor_ns: D });
            }
        }
    }

    #[test]
    fn it_anchors_d_after_three_agreeing_windows_before_ptp_lock_but_does_not_engage_134() {
        // `D = median(t2 − t1)` is an absolute offset of two time bases: valid from the first
        // windows, the rate servo's lock is not needed for it (the 50 s boot of camera-box 1483).
        // One 4-sample window is not trusted alone (review of PR 135): three agreeing ones are.
        assert_eq!(ANCHOR_CONFIRM_WINDOWS, 3);
        let mut c = PhaseLockCore::new();
        for e in [0, 20_000] {
            let out = acquire(&mut c, e);
            assert_eq!(out.event, AnchorEvent::None);
            assert_eq!(out.error_ns, None);
            assert_eq!(c.anchor_ns(), None);
        }
        let out = acquire(&mut c, 40_000);
        assert_eq!(
            out.event,
            AnchorEvent::Anchored {
                anchor_ns: D + 40_000
            },
            "anchored on the confirming window"
        );
        assert_eq!(out.error_ns, Some(0));
        assert_eq!(out.freq_ppm, None, "the rate servo keeps the word");
        assert!(!c.engaged());
        // The acquisition drift is measured against it, still without engaging.
        let out = acquire(&mut c, 420_000);
        assert_eq!(out.event, AnchorEvent::None);
        assert_eq!(out.error_ns, Some(380_000));
        assert_eq!(out.freq_ppm, None);
    }

    #[test]
    fn a_window_that_disagrees_restarts_the_early_anchor_134() {
        // An interim grandmaster, a time-base jump or a bad window: more than
        // `ANCHOR_AGREE_NS` apart = no anchor, the count restarts from that window.
        let mut c = PhaseLockCore::new();
        acquire(&mut c, 0);
        acquire(&mut c, 0);
        let out = acquire(&mut c, ANCHOR_AGREE_NS + 1);
        assert_eq!(out.event, AnchorEvent::None);
        assert_eq!(c.anchor_ns(), None);
        acquire(&mut c, ANCHOR_AGREE_NS + 1);
        let out = acquire(&mut c, ANCHOR_AGREE_NS + 1);
        assert_eq!(
            out.event,
            AnchorEvent::Anchored {
                anchor_ns: D + ANCHOR_AGREE_NS + 1
            }
        );
        // A requested rebase before the anchor also restarts the count.
        let mut c = PhaseLockCore::new();
        acquire(&mut c, 0);
        acquire(&mut c, 0);
        c.request_rebase();
        assert_eq!(acquire(&mut c, 0).event, AnchorEvent::None);
        assert_eq!(acquire(&mut c, 0).event, AnchorEvent::None);
        assert!(matches!(
            acquire(&mut c, 0).event,
            AnchorEvent::Anchored { .. }
        ));
    }

    #[test]
    fn the_first_engagement_keeps_the_early_anchor_and_pulls_the_drift_no_step_134() {
        // 1.5 ms of acquisition drift: beyond `REANCHOR_ON_ENGAGE_NS`, which would re-anchor
        // (`Realigned` = a second date step). The first engagement keeps `D` and retires the
        // drift along the pull reference instead.
        let mut c = PhaseLockCore::new();
        anchor_early(&mut c);
        acquire(&mut c, 900_000);
        let out = c.on_window(D + 1_500_000, true, -21.0, DT);
        assert_eq!(out.event, AnchorEvent::None, "no re-anchor: no second step");
        assert_eq!(c.anchor_ns(), Some(D));
        assert_eq!(out.error_ns, Some(1_500_000));
        assert!(c.engaged());
        assert!(
            (c.pull_us() - 1_500.0).abs() < 100.0,
            "pull {}",
            c.pull_us()
        );
        // Bumpless on the integrator: it starts at the rate servo's word …
        assert!((c.integrator_ppm() + 21.0).abs() < 1e-9);
        // … and the word carries the pull's own rate (−r/τ), slowing the fast wall back.
        let f = out.freq_ppm.expect("engaged");
        assert!(
            (f - (-21.0 - 1_500.0 / FSTEP_PULL_TAU_S)).abs() < 1e-6,
            "word {f}"
        );
    }

    #[test]
    fn beyond_the_boot_hold_the_first_engagement_still_realigns_134() {
        // Past the ±2 ms error clamp the pull cannot be the answer: one Join step, the old rule.
        let mut c = PhaseLockCore::new();
        anchor_early(&mut c);
        let out = c.on_window(D + 3_000_000, true, 5.0, DT);
        assert_eq!(
            out.event,
            AnchorEvent::Realigned {
                old_ns: D,
                new_ns: D + 3_000_000
            }
        );
        assert_eq!(c.pull_us(), 0.0);
    }

    #[test]
    fn a_later_re_engagement_keeps_the_realign_rule_bit_for_bit_134() {
        // Only the FIRST engagement after the early anchor holds: a lock loss later in life
        // (the clock free-ran on the held word) re-aligns as before.
        let mut c = PhaseLockCore::new();
        anchor_early(&mut c);
        c.on_window(D + 200_000, true, 5.0, DT);
        c.on_window(D, false, 5.0, DT);
        let out = c.on_window(D + 1_500_000, true, 5.0, DT);
        assert_eq!(
            out.event,
            AnchorEvent::Realigned {
                old_ns: D,
                new_ns: D + 1_500_000
            }
        );
        assert_eq!(c.pull_us(), 0.0);
        assert!((out.freq_ppm.unwrap() - 5.0).abs() < 1e-9, "bumpless");
    }

    #[test]
    fn a_boot_acquisition_lands_on_the_early_anchor_without_any_anchor_event_134() {
        // The strih-snv boot of 24.10.2026: a −21 ppm oscillator, the rate servo walking its word
        // 0 → −19.5 ppm over 40 s (still 7 µs/s off at 30 s), then the lock. Here a +70 ppm box
        // with the same servo shape, whose drift at the lock is past 1 ms — the case that needs
        // the hold. After the lock: one continuous engagement on the FIRST window's `D`.
        for (osc, delay) in [(-21.0f64, 1usize), (70.0, 1), (70.0, 2)] {
            let mut c = PhaseLockCore::new();
            let mut p = Plant::new(osc, delay, 0.0);
            let acq_windows = 80; // 40 s
            let mut anchor = None;
            for n in 0..acq_windows {
                // The rate servo stand-in: the word converges on −osc with τ ≈ 20 s, 1.5 ppm
                // short of it at the lock. The core is told the word the servo applies (the
                // controller passes its `total_correction`, dsync#146's acquisition fit reads it).
                let t = (n + 1) as f64 * DT;
                let word = -osc * (1.0 - (-t / 20.0).exp()) * 0.97;
                let out = c.on_window(D + p.e_ns.round() as i64, false, word, DT);
                if let AnchorEvent::Anchored { anchor_ns } = out.event {
                    assert_eq!(n + 1, ANCHOR_CONFIRM_WINDOWS, "osc {osc}");
                    anchor = Some(anchor_ns);
                } else {
                    assert_eq!(out.event, AnchorEvent::None, "osc {osc}: window {n}");
                }
                p.advance(word, DT);
            }
            let anchor = anchor.expect("anchored early");
            // The plant's phase relative to the early anchor (ns).
            let off = |p: &Plant| (D + p.e_ns.round() as i64 - anchor) as f64;
            let drift_at_lock = off(&p);
            if osc > 40.0 {
                assert!(
                    drift_at_lock.abs() > REANCHOR_ON_ENGAGE_NS as f64,
                    "osc {osc}: the hold must be needed here ({drift_at_lock} ns)"
                );
            }
            let handover = -osc * 0.97;
            let mut events = 0;
            for n in 0..1_200 {
                let out = c.on_window(D + p.e_ns.round() as i64, true, handover, DT);
                if out.event != AnchorEvent::None {
                    events += 1;
                }
                p.advance(out.freq_ppm.expect("engaged"), DT);
                // The rate servo hands over 3 % short (2.1 ppm at 70 ppm). Before #146 the PI
                // learned that through its ~100 s integrator, so the phase was judged from 400 s
                // after the lock; now the engagement starts on the acquisition's measured
                // frequency and the pull (τ 20 s) retires the drift: judged from 120 s.
                if n >= 240 {
                    assert!(
                        off(&p).abs() < 50_000.0,
                        "osc {osc} delay {delay}: |e| {} ns at {} s after the lock",
                        off(&p),
                        n as f64 * DT
                    );
                }
            }
            assert_eq!(events, 0, "osc {osc}: no re-anchor, no second step");
            let fit = c.engaged_on_fit().expect("started on the acquisition fit");
            // Within 1.5 ppm: this plant's transport delay (1-2 windows) is not in the fit's model
            // of the word in force (~0.5 ppm per window of lag on this ramp; the controller's own
            // timing is in it, `lock_sim.rs`), still
            // under the rate servo's 3 % hand-over error (2.1 ppm).
            assert!(
                (fit.freq_ppm + osc).abs() < 1.5,
                "osc {osc} delay {delay}: measured {fit:?}"
            );
            assert_eq!(c.anchor_ns(), Some(anchor), "osc {osc}: the early anchor");
            assert_eq!(
                c.freq_steps(),
                0,
                "osc {osc}: the pull is not a frequency step"
            );
            assert!(
                (c.integrator_ppm() + osc).abs() < 0.05,
                "osc {osc}: learned {}",
                c.integrator_ppm()
            );
        }
    }

    /// dsync#146: a first engagement on a SHORT ring (a seeded boot) with a hand-over 5 ppm off.
    fn short_ring_engagement(c: &mut PhaseLockCore, p: &mut Plant) {
        for _ in 0..(ANCHOR_CONFIRM_WINDOWS + 4) {
            let out = c.on_window(D + p.e_ns.round() as i64, false, 30.0, DT);
            assert_eq!(out.freq_ppm, None);
            p.advance(30.0, DT);
        }
        let out = c.on_window(D + p.e_ns.round() as i64, true, 30.0, DT);
        p.advance(out.freq_ppm.expect("engaged"), DT);
        assert_eq!(c.engaged_on_fit(), None, "too short a ring to start on");
        assert!(
            c.boot_fit_points() > 0,
            "it keeps fitting through the engagement"
        );
    }

    #[test]
    fn a_short_ring_engagement_is_re_seeded_once_on_its_full_fit_146() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(-35.0, 1, 30.0);
        short_ring_engagement(&mut c, &mut p);
        for _ in 0..120 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 30.0, DT);
            p.advance(out.freq_ppm.expect("engaged"), DT);
        }
        let fit = c
            .boot_reseed()
            .expect("re-seeded once the ring spanned 20 s");
        assert!((fit.freq_ppm - 35.0).abs() < 1.0, "{fit:?}");
        assert_eq!(c.boot_fit_points(), 0, "closed after the re-seed");
        assert_eq!(c.freq_steps(), 0, "a re-seed is not a frequency step");
        assert!(p.e_ns.abs() < 50_000.0, "e {} ns", p.e_ns);
    }

    #[test]
    fn a_phase_jump_while_engaged_is_realigned_after_six_windows_146() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(12.0, 1, -12.0);
        run(&mut c, &mut p, 1_200, -12.0);
        let learned = c.integrator_ppm();
        // 5 windows 3 ms off: slewed (a transient), no re-anchor …
        let jump: i64 = 3_000_000;
        for n in 0..(REALIGN_ENGAGED_WINDOWS - 1) {
            let out = c.on_window(D + jump + p.e_ns.round() as i64, true, 0.0, DT);
            assert_eq!(out.event, AnchorEvent::None, "window {n}");
        }
        // … the 6th re-anchors on the current offset, the integrator back where it was.
        let out = c.on_window(D + jump + p.e_ns.round() as i64, true, 0.0, DT);
        assert!(
            matches!(out.event, AnchorEvent::Realigned { old_ns, .. } if old_ns == D),
            "{:?}",
            out.event
        );
        assert_eq!(out.error_ns, Some(0));
        assert_eq!(c.integrator_ppm(), learned, "no windup kept from the jump");
        // One window back inside the bound restarts the count.
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(12.0, 1, -12.0);
        run(&mut c, &mut p, 1_200, -12.0);
        for _ in 0..20 {
            for _ in 0..(REALIGN_ENGAGED_WINDOWS - 1) {
                c.on_window(D + jump, true, 0.0, DT);
            }
            assert_eq!(c.on_window(D, true, 0.0, DT).event, AnchorEvent::None);
        }
        assert_eq!(c.anchor_ns(), Some(D));
        // An error that GROWS past the bound (an unlearned frequency, 100 ppm here) is never a
        // jump: no re-align, the PI / the frequency-step follow own it.
        let mut c = PhaseLockCore::new();
        c.on_window(D, true, 0.0, DT);
        for n in 1..120i64 {
            let out = c.on_window(D + n * 50_000, true, 0.0, DT);
            assert_eq!(out.event, AnchorEvent::None, "window {n}");
        }
        assert_eq!(c.anchor_ns(), Some(D));
    }

    #[test]
    fn a_rebase_during_the_open_fit_closes_it_without_a_re_seed_146() {
        // A level shift in the open-loop phase (a re-anchor) must never feed the fit.
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(-35.0, 1, 30.0);
        short_ring_engagement(&mut c, &mut p);
        for _ in 0..10 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 30.0, DT);
            p.advance(out.freq_ppm.expect("engaged"), DT);
        }
        c.request_rebase();
        let jump: i64 = 3_600 * 1_000_000_000;
        let out = c.on_window(D + jump + p.e_ns.round() as i64, true, 30.0, DT);
        assert!(matches!(out.event, AnchorEvent::Rebased { .. }));
        assert_eq!(c.boot_fit_points(), 0, "closed on the re-anchor");
        for _ in 0..120 {
            let out = c.on_window(D + jump + p.e_ns.round() as i64, true, 30.0, DT);
            p.advance(out.freq_ppm.expect("engaged"), DT);
        }
        assert_eq!(
            c.boot_reseed(),
            None,
            "never re-seeded on a ring with a re-anchor in it"
        );
    }

    #[test]
    fn a_ptp_outage_before_the_engagement_restarts_the_acquisition_fit_146() {
        let mut c = PhaseLockCore::new();
        for _ in 0..(ANCHOR_CONFIRM_WINDOWS + 20) {
            c.on_window(D, false, 0.0, DT);
        }
        assert!(c.boot_fit_points() > 12);
        c.on_ptp_outage();
        assert_eq!(c.boot_fit_points(), 0);
        c.on_window(D, false, 0.0, DT);
        assert_eq!(c.boot_fit_points(), 1, "from the first window after it");
    }

    #[test]
    fn the_first_lock_anchors_on_the_current_offset_and_takes_over_bumplessly() {
        let mut c = PhaseLockCore::new();
        let out = c.on_window(D, true, -23.5, DT);
        assert_eq!(out.event, AnchorEvent::Anchored { anchor_ns: D });
        assert_eq!(out.error_ns, Some(0));
        assert_eq!(
            out.freq_ppm,
            Some(-23.5),
            "e = 0 at the anchor: the word is unchanged"
        );
        assert!(c.engaged());
    }

    #[test]
    fn it_holds_phase_against_a_constant_oscillator_error_with_zero_steady_state() {
        // A +26 ppm oscillator, handed over with a word that is 0.4 ppm off (the rate servo's
        // residual) — the phase term must pull e back and the integrator learn −26 ppm exactly.
        for delay in [0usize, 1, 2] {
            let mut c = PhaseLockCore::new();
            let mut p = Plant::new(26.0, delay, -25.6);
            let (max_late, word) = run(&mut c, &mut p, 4_000, -25.6);
            assert!(max_late < 5_000.0, "delay {delay}: late |e| {max_late} ns");
            assert!(
                (word + 26.0).abs() < 0.01,
                "delay {delay}: word {word} ppm, want −26"
            );
            assert!((c.integrator_ppm() + 26.0).abs() < 0.01);
        }
    }

    #[test]
    fn there_is_no_deadband_a_tiny_rate_error_is_corrected_not_left_to_drift() {
        // The NANO deadband ignored |rate| < 0.1 µs/s forever (360 µs/h). The phase lock must
        // hold a 0.05 ppm residual to microseconds over an hour.
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(0.05, 1, 0.0);
        let (max_late, _) = run(&mut c, &mut p, 7_200, 0.0);
        assert!(
            max_late < 1_000.0,
            "late |e| {max_late} ns after an hour at 0.05 ppm"
        );
    }

    #[test]
    fn a_step_of_the_wall_with_the_same_step_of_d_is_invisible_to_the_loop() {
        let mut a = PhaseLockCore::new();
        let mut b = PhaseLockCore::new();
        let mut pa = Plant::new(7.0, 1, 0.0);
        let mut pb = Plant::new(7.0, 1, 0.0);
        let step: i64 = 51_234_567;
        let mut words_a = Vec::new();
        let mut words_b = Vec::new();
        for i in 0..400 {
            if i == 200 {
                b.note_step(step); // D moves with the wall …
            }
            let off_b = if i >= 200 { step } else { 0 }; // … and so does t2
            let wa = a
                .on_window(D + pa.e_ns.round() as i64, true, 0.0, DT)
                .freq_ppm
                .unwrap();
            let wb = b
                .on_window(D + pb.e_ns.round() as i64 + off_b, true, 0.0, DT)
                .freq_ppm
                .unwrap();
            pa.advance(wa, DT);
            pb.advance(wb, DT);
            words_a.push(wa);
            words_b.push(wb);
        }
        assert_eq!(
            words_a, words_b,
            "bit-identical frequency commands across a date step"
        );
    }

    #[test]
    fn a_grandmaster_change_re_anchors_without_disturbing_the_frequency() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(-12.0, 1, 12.0);
        run(&mut c, &mut p, 2_000, 12.0);
        let word_before = c.last_freq_ppm();
        c.request_rebase();
        // The new GM's uptime is 3.7 days behind: t2 − t1 jumps by +3.7 days, the wall does not.
        let jump: i64 = 320_000 * 1_000_000_000;
        let out = c.on_window(D + jump + p.e_ns.round() as i64, true, 0.0, DT);
        match out.event {
            AnchorEvent::Rebased { old_ns, new_ns } => {
                assert_eq!(old_ns, D);
                assert_eq!(new_ns, D + jump + p.e_ns.round() as i64);
            }
            other => panic!("expected a rebase, got {other:?}"),
        }
        assert_eq!(out.error_ns, Some(0));
        assert!(
            (out.freq_ppm.unwrap() - word_before).abs() < 0.01,
            "no frequency kick"
        );
    }

    #[test]
    fn a_time_base_jump_without_a_uuid_change_is_detected_as_a_discontinuity() {
        let mut c = PhaseLockCore::new();
        c.on_window(D, true, 0.0, DT);
        let out = c.on_window(D - 2 * DISCONTINUITY_NS, true, 0.0, DT);
        assert!(matches!(out.event, AnchorEvent::Rebased { .. }));
        assert_eq!(out.error_ns, Some(0));
        // Just under the threshold is an (absurd) phase error, not a rebase — clamped.
        let out = c.on_window(D - 2 * DISCONTINUITY_NS + DISCONTINUITY_NS, true, 0.0, DT);
        assert_eq!(out.event, AnchorEvent::None);
        assert!(out.freq_ppm.unwrap().abs() <= K_P_PER_S * ERROR_CLAMP_US + 1e-9 + 1.0);
    }

    #[test]
    fn losing_lock_disengages_and_keeps_the_learned_frequency_for_the_hand_back() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(30.0, 1, -30.0);
        run(&mut c, &mut p, 2_000, -30.0);
        let out = c.on_window(D + p.e_ns.round() as i64, false, 0.0, DT);
        assert_eq!(out.freq_ppm, None);
        assert!(!c.engaged());
        assert!((c.integrator_ppm() + 30.0).abs() < 0.05);
        // Re-engaging takes the (rate servo's) word, and the anchor survived.
        let out = c.on_window(D + p.e_ns.round() as i64, true, -29.0, DT);
        assert!(out.freq_ppm.is_some());
        assert_eq!(c.anchor_ns(), Some(D));
    }

    #[test]
    fn re_engaging_far_off_the_anchor_re_anchors_instead_of_slewing() {
        let mut c = PhaseLockCore::new();
        c.on_window(D, true, 5.0, DT);
        c.on_window(D, false, 5.0, DT); // lock lost: the rate servo holds the clock …
                                        // … and it free-ran 3 ms away. Re-engaging must not slew 3 ms at the P clamp.
        let out = c.on_window(D + 3_000_000, true, 5.0, DT);
        assert_eq!(
            out.event,
            AnchorEvent::Realigned {
                old_ns: D,
                new_ns: D + 3_000_000
            },
            "same time base: a re-alignment, not a rebase of the fleet offset"
        );
        assert_eq!(out.error_ns, Some(0));
        assert!(
            (out.freq_ppm.unwrap() - 5.0).abs() < 1e-9,
            "bumpless, no slew"
        );
        // A small re-engagement error is simply tracked.
        c.on_window(D + 3_000_000, false, 5.0, DT);
        let out = c.on_window(D + 3_000_000 + 400_000, true, 5.0, DT);
        assert_eq!(out.event, AnchorEvent::None);
        assert_eq!(out.error_ns, Some(400_000));
    }

    #[test]
    fn disengage_keeps_the_anchor_and_the_learned_frequency() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(-18.0, 1, 18.0);
        run(&mut c, &mut p, 2_000, 18.0);
        c.disengage();
        assert!(!c.engaged());
        assert_eq!(c.anchor_ns(), Some(D));
        assert!((c.integrator_ppm() - 18.0).abs() < 0.05);
        // After a 3 ms free-run the next locked window re-aligns instead of slewing.
        let out = c.on_window(D + 3_000_000, true, 18.0, DT);
        assert!(matches!(out.event, AnchorEvent::Realigned { .. }));
        assert!(c.engaged());
    }

    #[test]
    fn rebase_pending_is_visible_until_the_re_anchoring_window() {
        let mut c = PhaseLockCore::new();
        assert!(!c.rebase_pending());
        c.request_rebase();
        assert!(
            !c.rebase_pending(),
            "nothing to rebase before the first anchor"
        );
        c.on_window(D, true, 0.0, DT);
        c.request_rebase();
        assert!(c.rebase_pending());
        c.on_window(D + 7, true, 0.0, DT);
        assert!(!c.rebase_pending());
    }

    #[test]
    fn the_error_clamp_bounds_the_response_to_a_huge_error() {
        let mut c = PhaseLockCore::new();
        c.on_window(D, true, 0.0, DT);
        let out = c.on_window(D + 900_000_000, true, 0.0, DT); // 900 ms (< discontinuity)
        let f = out.freq_ppm.unwrap();
        assert!(f.abs() <= K_P_PER_S * ERROR_CLAMP_US + K_I_PER_S2 * ERROR_CLAMP_US * DT + 1e-9);
    }

    #[test]
    fn gains_are_critically_damped() {
        let zeta = K_P_PER_S / (2.0 * K_I_PER_S2.sqrt());
        assert!((zeta - 1.0).abs() < 1e-12);
    }

    // ---- camera-box issue 1372: a grandmaster FREQUENCY step --------------------------------

    /// The law before the frequency-step follow (the plain PI), for the bit-for-bit comparison.
    fn plain_pi(i_ppm: &mut f64, e_ns: i64, dt: f64) -> f64 {
        let e_us = (e_ns as f64 / 1_000.0).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        *i_ppm = (*i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        (*i_ppm - K_P_PER_S * e_us).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM)
    }

    #[test]
    fn without_a_confirmed_step_the_words_are_the_plain_pi_bit_for_bit_1372() {
        // A 0.4 ppm hand-over error and a 0.1 ppm/min oscillator ramp for 50 minutes.
        for delay in [0usize, 1, 2] {
            let mut c = PhaseLockCore::new();
            let mut p = Plant::new(26.0, delay, -25.6);
            let mut q = Plant::new(26.0, delay, -25.6);
            let mut i_ref = -25.6;
            for n in 0..6_000 {
                let w = c
                    .on_window(D + p.e_ns.round() as i64, true, -25.6, DT)
                    .freq_ppm
                    .unwrap();
                let w_ref = plain_pi(&mut i_ref, q.e_ns.round() as i64, DT);
                assert_eq!(w.to_bits(), w_ref.to_bits(), "delay {delay}, window {n}");
                let osc = 26.0 + 0.1 * (n as f64 * DT) / 60.0;
                p.osc_ppm = osc;
                q.osc_ppm = osc;
                p.advance(w, DT);
                q.advance(w_ref, DT);
            }
            assert_eq!(c.freq_steps(), 0);
        }
    }

    #[test]
    fn a_frequency_step_re_seeds_the_integrator_in_seconds_and_never_moves_d_1372() {
        for delay in [0usize, 1] {
            let mut c = PhaseLockCore::new();
            let mut p = Plant::new(23.0, delay, -23.0);
            run(&mut c, &mut p, 1_200, -23.0); // 10 minutes locked
            let anchor = c.anchor_ns();
            // The grandmaster speeds up by 25 ppm: the oscillator relative to it drops by 25.
            p.osc_ppm -= 25.0;
            let mut followed = Vec::new();
            for n in 0..1_200 {
                let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
                assert_eq!(out.event, AnchorEvent::None, "delay {delay}");
                if let Some(fs) = out.freq_step {
                    followed.push((n, fs));
                }
                p.advance(out.freq_ppm.unwrap(), DT);
            }
            assert_eq!(followed.len(), 1, "delay {delay}: {followed:?}");
            let (n, fs) = followed[0];
            assert!(
                (n + 1) as f64 * DT <= 30.0,
                "delay {delay}: after {n} windows"
            );
            assert!((fs.step_ppm - 25.0).abs() < 1.0, "delay {delay}: {fs:?}");
            assert_eq!(fs.step_ppm, -fs.error_ppm);
            // The integrator lands on the new frequency (−osc = +2 ppm) at once.
            assert!(
                (fs.integrator_after_ppm - 2.0).abs() < 0.5,
                "delay {delay}: {fs:?}"
            );
            assert!(fs.pull_us < -300.0, "the step left ~−400 µs: {fs:?}");
            assert_eq!(c.last_freq_step(), Some(fs));
            assert_eq!(c.freq_steps(), 1);
            // D never moved; the pull finished; the phase and the frequency are back.
            assert_eq!(c.anchor_ns(), anchor, "delay {delay}");
            assert_eq!(c.pull_us(), 0.0);
            assert!(p.e_ns.abs() < 5_000.0, "delay {delay}: e {} ns", p.e_ns);
            assert!((c.integrator_ppm() - 2.0).abs() < 0.05, "delay {delay}");
        }
    }

    #[test]
    fn the_pull_retires_the_step_phase_without_disturbing_the_integrator_1372() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(23.0, 0, -23.0);
        run(&mut c, &mut p, 1_200, -23.0);
        p.osc_ppm -= 25.0;
        let mut after = None;
        let mut worst_i_err = 0.0f64;
        for n in 0..1_200 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            if out.freq_step.is_some() {
                after = Some(n);
            }
            if after.is_some() {
                worst_i_err = worst_i_err.max((c.integrator_ppm() - 2.0).abs());
            }
            p.advance(out.freq_ppm.unwrap(), DT);
            if let Some(a) = after {
                if n == a + 240 {
                    // Two minutes after the re-seed the phase is back under 50 µs …
                    assert!(p.e_ns.abs() < 50_000.0, "e {} ns", p.e_ns);
                }
            }
        }
        assert!(after.is_some());
        // … and the integrator never left the new frequency by more than a few tenths of a ppm
        // (a plain PI trimming ~400 µs would swing it by ~1.5 ppm).
        assert!(worst_i_err < 0.5, "{worst_i_err}");
    }

    #[test]
    fn a_lock_loss_or_a_re_anchor_drops_the_pull_and_the_ring_1372() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(23.0, 0, -23.0);
        run(&mut c, &mut p, 1_200, -23.0);
        p.osc_ppm -= 25.0;
        for _ in 0..200 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            p.advance(out.freq_ppm.unwrap(), DT);
            if out.freq_step.is_some() {
                break;
            }
        }
        assert!(c.pull_us() != 0.0, "a pull is running");
        // Refill the ring for a few windows, so that dropping it is observable.
        for _ in 0..10 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            p.advance(out.freq_ppm.unwrap(), DT);
        }
        assert!(c.fstep.points() >= 9, "{}", c.fstep.points());
        // Lock lost: nothing is pulled any more and the ring is gone.
        c.on_window(D + p.e_ns.round() as i64, false, 0.0, DT);
        assert_eq!(c.pull_us(), 0.0);
        assert_eq!(c.fstep.points(), 0);
        // Re-engaged, the ring refills …
        for _ in 0..10 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            p.advance(out.freq_ppm.unwrap(), DT);
        }
        assert!(c.fstep.points() >= 9);
        // … and a grandmaster change re-anchors: the ring starts again from that window.
        c.request_rebase();
        let out = c.on_window(D + 7 * DISCONTINUITY_NS, true, 2.0, DT);
        assert!(matches!(out.event, AnchorEvent::Rebased { .. }));
        assert_eq!(c.pull_us(), 0.0);
        assert!(c.fstep.points() <= 1, "{}", c.fstep.points());
        assert_eq!(c.freq_steps(), 1, "the count survives");
    }

    // ---- dsync#136: the boot's re-seeds are continuous in their input ----------------------

    /// One boot: the acquisition holds `e` at 0 on the word `w_acq` (so the engagement starts on
    /// it), then the oscillator needs `+1 ppm`. Returns `e` (ns) of every engaged window.
    fn boot_trajectory(w_acq: f64, windows: usize) -> Vec<f64> {
        let mut core = PhaseLockCore::new();
        for _ in 0..(BOOT_FIT_WINDOW_S / DT) as usize / 2 {
            core.on_window(D, false, w_acq, DT);
        }
        let mut plant = Plant::new(-1.0, 1, w_acq);
        (0..windows)
            .map(|_| {
                let out = core.on_window(D + plant.e_ns.round() as i64, true, w_acq, DT);
                plant.advance(out.freq_ppm.expect("engaged"), DT);
                plant.e_ns
            })
            .collect()
    }

    /// PR #156 CI run 41589062657: two followers on one PC read the same kernel stamps up to the
    /// conversion's few-ns residue, yet ended 0.21 ppm and 16.4 µs apart. The boot re-seeds fired
    /// only past a threshold (0.5 ppm for the 20 s fit, 0.2 for the refinement), so a residue
    /// at the edge re-seeded one follower and not the other. A hand-over swept in 0.002 ppm steps
    /// must move the trajectory by a hair, never jump.
    #[test]
    fn the_boot_reseeds_are_continuous_in_the_hand_over_136() {
        let windows = ((BOOT_FIT_WINDOW_S + BOOT_REFINE_WINDOW_S + 40.0) / DT) as usize;
        let mut prev = boot_trajectory(1.0, windows);
        let mut worst = (0.0f64, 0.0f64);
        for k in 1..=400 {
            let h = k as f64 * 0.002;
            let traj = boot_trajectory(1.0 + h, windows);
            let jump = traj
                .iter()
                .zip(&prev)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            if jump > worst.1 {
                worst = (h, jump);
            }
            prev = traj;
        }
        assert!(
            worst.1 <= 1_000.0,
            "a 0.002 ppm change of the hand-over at {:.3} ppm moved e by {:.0} ns",
            worst.0,
            worst.1
        );
    }

    // ---- dsync#158: a grandmaster change in the same time base is continuous ------------------

    /// One grandmaster change on an engaged, settled core: the new grandmaster's time is
    /// `shift_ns` ahead of the old one's (`t2 − t1` drops by it) and it runs `gm_ppm` faster.
    /// Returns, from the change on, every window's (`e` µs, word relative to the new grandmaster's
    /// tick ppm) and the change window's outcome.
    fn gm_change(
        shift_ns: f64,
        gm_ppm: f64,
        jitter_us: f64,
        seed: u64,
        windows: usize,
    ) -> (PhaseLockCore, WindowOutcome, Vec<(f64, f64)>) {
        let mut noise = seed;
        let mut jitter = move || {
            noise ^= noise << 13;
            noise ^= noise >> 7;
            noise ^= noise << 17;
            ((noise >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0 * jitter_us * 1_000.0
        };
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(-12.0, 1, 12.0);
        for _ in 0..2_000 {
            let out = c.on_window(D + (p.e_ns + jitter()).round() as i64, true, 12.0, DT);
            p.advance(out.freq_ppm.expect("engaged"), DT);
        }
        assert!(
            p.e_ns.abs() < 2_000.0 + 3.0 * jitter_us * 1_000.0,
            "settled before the change"
        );
        c.request_rebase();
        p.e_ns -= shift_ns;
        p.osc_ppm -= gm_ppm;
        let first = c.on_window(D + (p.e_ns + jitter()).round() as i64, true, 12.0, DT);
        let mut trace = vec![(
            p.e_ns / 1_000.0,
            first.freq_ppm.expect("engaged") + p.osc_ppm,
        )];
        p.advance(first.freq_ppm.expect("engaged"), DT);
        for _ in 1..windows {
            let out = c.on_window(D + (p.e_ns + jitter()).round() as i64, true, 12.0, DT);
            let f = out.freq_ppm.expect("engaged");
            trace.push((p.e_ns / 1_000.0, f + p.osc_ppm));
            p.advance(f, DT);
        }
        (c, first, trace)
    }

    #[test]
    fn a_grandmaster_change_within_a_millisecond_keeps_d_and_slews_the_residual_158() {
        // dstack#1, the FOH reboot of 5.11.2026: a Dante failover hands the time over within
        // tens of µs; re-anchoring on it stepped the published time by −88 µs.
        for shift in [88_000.0, -88_000.0, 900_000.0] {
            let (c, first, trace) = gm_change(shift, 0.0, 0.0, 1, 600);
            assert!(
                !matches!(first.event, AnchorEvent::Rebased { .. }),
                "shift {shift}: re-anchored ({:?}) = a step of the published time",
                first.event
            );
            assert_eq!(c.anchor_ns(), Some(D), "shift {shift}: D kept");
            assert!(!c.rebase_pending());
            // The residual is retired at a bounded rate: ≤ 50 ppm against the grandmaster's tick.
            let worst_rate = trace.iter().map(|&(_, r)| r.abs()).fold(0.0, f64::max);
            assert!(
                worst_rate <= 50.0,
                "shift {shift}: slewed at {worst_rate:.1} ppm"
            );
            // … and is gone: |e| ≤ 5 µs after 120 s, and after 300 s.
            let (e120, _) = trace[240];
            let (e_end, _) = *trace.last().unwrap();
            assert!(e120.abs() <= 5.0, "shift {shift}: e {e120:.1} µs at 120 s");
            assert!(
                e_end.abs() <= 1.0,
                "shift {shift}: e {e_end:.1} µs at 300 s"
            );
        }
    }

    #[test]
    fn a_new_time_base_beyond_a_millisecond_still_re_anchors_158() {
        let (c, first, _) = gm_change(1_500_000.0, 0.0, 0.0, 1, 4);
        assert!(
            matches!(first.event, AnchorEvent::Rebased { .. }),
            "{:?}",
            first.event
        );
        assert_ne!(c.anchor_ns(), Some(D));
    }

    #[test]
    fn a_grandmaster_change_relearns_the_new_rate_in_seconds_158() {
        // dstack#1: after the change the service sat at e = +177 µs, still drifting 3 µs/s,
        // minutes later (the ~100 s PI pulling from the old word). A fresh open-loop fit re-seeds
        // the integrator: within 1 ppm of the new grandmaster by 30 s, |e| ≤ 50 µs from 40 s on.
        for (gm_ppm, shift) in [
            (8.0, 88_000.0),
            (-8.0, -88_000.0),
            (4.0, 20_000.0),
            (-25.0, 0.0),
        ] {
            for (jitter_us, seed) in [(0.0, 1), (10.0, 0x5eed_0158), (10.0, 0x5eed_0159)] {
                let (c, _, trace) = gm_change(shift, gm_ppm, jitter_us, seed, 400);
                let label = format!("GM {gm_ppm:+} ppm, shift {shift} ns, jitter {jitter_us} µs");
                let learned = c.integrator_ppm() - (12.0 + gm_ppm);
                assert!(
                    learned.abs() <= 1.0,
                    "{label}: integrator {learned:+.2} ppm off at 200 s"
                );
                let worst_after_40s = trace[80..]
                    .iter()
                    .map(|&(e, _)| e.abs())
                    .fold(0.0, f64::max);
                assert!(
                    worst_after_40s <= 50.0,
                    "{label}: |e| {worst_after_40s:.1} µs after 40 s"
                );
                let worst_rate = trace.iter().map(|&(_, r)| r.abs()).fold(0.0, f64::max);
                assert!(worst_rate <= 50.0, "{label}: slewed at {worst_rate:.1} ppm");
            }
        }
    }
}
