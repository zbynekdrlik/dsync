//! The PTP RATE servo — the acquisition loop of the dsync service, shared with the in-process
//! follower (dsync#136: EXTRACTED from `PtpController`, behaviour-preserving).
//!
//! # The algorithm (unchanged from the service)
//!
//! PTPv1 timestamps of the audio network's grandmaster are device uptime, not UTC: the absolute offset between the local clock
//! and the grandmaster is meaningless for the RATE. What matters is the offset's rate of change:
//! a stable offset means the frequencies match, a growing one means the local clock runs fast.
//! Every sample window gives one offset (the window median, µs); this servo:
//!
//! 1. measures the rate `d(offset)/dt` against the previous window ([`RateServo::measure_offset`];
//!    dsync#142: the first window after a reset has no previous offset and is NOT a
//!    measurement);
//! 2. filters it through the MAD spike filter and an adaptive EMA ([`RateServo::measure`]);
//! 3. moves the gain mode ACQ → PROD → NANO on the smoothed rate's stability
//!    ([`RateServo::update_modes`]);
//! 4. turns it into the frequency word: a P term on the rate plus the integrator
//!    `drift_baseline_ppm` (the learned oscillator error), and counts the lock: 5 stable windows
//!    (|rate| < 5 µs/s) lock, unstable windows unlock gradually ([`RateServo::word`]).
//!
//! Under the PTP phase lock (the default discipline, [`crate::ptp_phase_lock`]) this servo only
//! ACQUIRES: once it locks, the phase lock takes the frequency word. The service's own hooks run
//! between the steps above — the legacy `phase_slew` decoupling, the dsync#142 seed judge, the
//! #146 decoupling ([`WordDeviation`]) — so the steps are separate methods, called in this order.
//!
//! # Time
//!
//! The servo is generic over its time stamp ([`ServoTime`]). The window's span divides an offset
//! delta measured in PTP time, so it is measured from the window's own packets, never from when
//! the window happens to be processed: the service passes the grandmaster time of the window's
//! last Sync ([`GmNs`], since 1.25: before, `Instant::now()` at processing, which made the
//! simulations' rate depend on wall time — every simulated span was under 0.1 s, and a pause put a
//! simulated offset over a real second), the in-process follower passes the raw monotonic receive
//! stamp of the window ([`MonoNs`]). Either way a simulation drives it deterministically.

use crate::spike_filter::{FilterMode, JitterEstimator, SpikeFilter};
use log::{debug, info};
use std::time::Instant;

/// Acquisition phase (FAST convergence): aggressive P-term for quick lock.
pub const P_GAIN_ACQ: f64 = 0.8;
/// Limit to prevent wild swings.
pub const P_MAX_ACQ_PPM: f64 = 200.0;

/// Production phase (gentle stability).
pub const P_GAIN_PROD: f64 = 0.1;
/// Allow enough for high drift rates.
pub const P_MAX_PROD_PPM: f64 = 100.0;

// NANO phase (ultra-precise for sub-µs capable systems)
// Entry: drift < 0.5 µs/s sustained for 15 samples
// Exit: drift > 1.0 µs/s for 5 samples (hysteresis)
/// 10x smaller than PROD - minimize hunting.
pub const P_GAIN_NANO: f64 = 0.01;
/// Tiny corrections only.
pub const P_MAX_NANO_PPM: f64 = 10.0;
/// 10x smaller I-term.
pub const I_GAIN_NANO: f64 = 0.005;
/// Enter NANO if drift < 0.5 µs/s.
pub const NANO_ENTER_RATE_US: f64 = 0.5;
/// Exit NANO if drift > 1.0 µs/s.
pub const NANO_EXIT_RATE_US: f64 = 1.0;
/// 15 samples to enter NANO.
pub const NANO_SUSTAIN_COUNT: usize = 15;
/// 5 consecutive samples above threshold to exit (hysteresis).
pub const NANO_EXIT_COUNT: usize = 5;
/// Ignore drift < 0.1 µs/s (noise floor).
pub const NANO_DEADBAND_US: f64 = 0.1;

/// Max drift baseline / frequency word (ppm).
pub const DRIFT_MAX_PPM: f64 = 500.0;

/// Lock detection: stable windows to lock.
pub const LOCK_STABLE_COUNT: usize = 5;

/// A time stamp the servo can measure a window's span with.
pub trait ServoTime: Copy {
    /// Seconds from `earlier` to `self` (0 when `earlier` is later).
    fn secs_since(self, earlier: Self) -> f64;
}

impl ServoTime for Instant {
    fn secs_since(self, earlier: Self) -> f64 {
        self.duration_since(earlier).as_secs_f64()
    }
}

/// The grandmaster's time of a window's last Sync in ns (t1, the service's time base): the time
/// the window's offsets are in. A grandmaster whose time goes backwards (a restart, another GM)
/// gives a span of 0, which carries no measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GmNs(pub i64);

impl ServoTime for GmNs {
    fn secs_since(self, earlier: Self) -> f64 {
        (self.0.saturating_sub(earlier.0)).max(0) as f64 / 1e9
    }
}

/// A raw monotonic stamp in ns (the in-process follower's time base, [`crate::mono`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonoNs(pub i64);

impl ServoTime for MonoNs {
    fn secs_since(self, earlier: Self) -> f64 {
        (self.0.saturating_sub(earlier.0)).max(0) as f64 / 1e9
    }
}

/// The rate measurement of one window ([`RateServo::measure_offset`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OffsetMeasurement {
    /// The window had a previous offset to differentiate (dsync#142).
    pub measured: bool,
    /// Seconds since the previous window (1.0 when there was none).
    pub dt_secs: f64,
    /// `d(offset)/dt` (µs/s = ppm); the smoothed rate for a span ≤ 0.1 s; 0 when not measured.
    pub raw_rate_ppm: f64,
}

/// The frequency word of one window ([`RateServo::word`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ServoWord {
    /// Drift baseline + P term (ppm), clamped to ±[`DRIFT_MAX_PPM`].
    pub word_ppm: f64,
    /// The gain mode's name: "NANO", "PROD" or "ACQ".
    pub phase_name: &'static str,
}

/// The rate servo's state (one per clock it disciplines).
#[derive(Debug)]
pub struct RateServo<T> {
    /// Learned drift baseline (auto-tuned from average correction when stable).
    pub drift_baseline_ppm: f64,
    /// Lock state - true when synchronized and stable.
    pub is_locked: bool,
    pub lock_stable_count: usize,
    /// Production mode state (with hysteresis).
    pub in_production_mode: bool,
    /// NANO mode state (ultra-precise for sub-µs capable systems).
    pub in_nano_mode: bool,
    /// Track consecutive sub-threshold samples for entry.
    pub nano_sustain_count: usize,
    /// Track consecutive above-threshold samples for exit (hysteresis).
    pub nano_exit_count: usize,
    /// Rate-of-change tracking: the previous window's offset and when it was taken.
    pub last_offset_us: Option<f64>,
    pub last_offset_time: Option<T>,
    /// Exponential moving average of rate.
    pub smoothed_rate_ppm: f64,
    /// Accumulated phase error tracking (estimated drift between NTP steps; monitoring only).
    pub accumulated_phase_error_us: f64,
    pub last_phase_accumulation_time: Option<T>,
    /// Spike filter for rejecting timestamp noise spikes (MAD-based, adapts to the box's noise).
    pub spike_filter: SpikeFilter,
    /// Jitter estimator for the adaptive EMA alpha (0.3 on a quiet box, 0.1 on a noisy one).
    pub jitter_estimator: JitterEstimator,
}

impl<T: ServoTime> Default for RateServo<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ServoTime> RateServo<T> {
    pub fn new() -> Self {
        RateServo {
            drift_baseline_ppm: 0.0,
            is_locked: false,
            lock_stable_count: 0,
            in_production_mode: false,
            in_nano_mode: false,
            nano_sustain_count: 0,
            nano_exit_count: 0,
            last_offset_us: None,
            last_offset_time: None,
            smoothed_rate_ppm: 0.0,
            accumulated_phase_error_us: 0.0,
            last_phase_accumulation_time: None,
            spike_filter: SpikeFilter::new(),
            jitter_estimator: JitterEstimator::new(),
        }
    }

    /// Step 1: the window's raw rate against the previous window's offset, and the offset kept
    /// for the next one.
    pub fn measure_offset(&mut self, offset_us: f64, now: T) -> OffsetMeasurement {
        // Track offset for rate calculation
        let dt_secs = self
            .last_offset_time
            .map(|t| now.secs_since(t))
            .unwrap_or(1.0);

        // dsync#142: a window without a previous offset (the first after warmup, the first
        // after any measurement reset: a clock step, a PTP outage) has no rate to measure. It only
        // records the offset: no spike-filter or smoothing input, no gain-mode change, no
        // integrator step and no lock count. Before 1.22 it went on as a measured 0 µs/s, which
        // switched a cold box to PROD for one window and counted toward LOCK (gen3, dstack).
        let measured = self.last_offset_us.is_some();

        // Calculate instantaneous rate of change (drift rate in ppm)
        // delta_offset / delta_time gives us the frequency error
        let raw_rate_ppm = if let Some(prev_offset) = self.last_offset_us {
            if dt_secs > 0.1 {
                // Need meaningful time delta
                let delta_offset = offset_us - prev_offset;
                // Convert: us/s = ppm.
                // #97 (review 🔵): the ±500 spike clamp is applied by the caller AFTER its
                // decoupling, so it bounds the RESIDUAL the servo actually consumes.
                delta_offset / dt_secs
            } else {
                self.smoothed_rate_ppm // Keep previous
            }
        } else {
            0.0
        };

        // Store for next iteration
        self.last_offset_us = Some(offset_us);
        self.last_offset_time = Some(now);
        OffsetMeasurement {
            measured,
            dt_secs,
            raw_rate_ppm,
        }
    }

    /// Step 2: the rate of a MEASURED window (dsync#142: never called for a window without a
    /// previous offset): the spike filter, the smoothed rate and the accumulated phase error.
    ///
    /// `now` is read where the controller read its clock before the extraction: after the
    /// filter, at the accumulation step.
    pub fn measure(&mut self, raw_rate_ppm: f64, now: impl FnOnce() -> T) -> f64 {
        // =======================================================================
        // ADAPTIVE SPIKE DETECTION
        // =======================================================================
        // Filter raw rate through MAD-based outlier detector.
        // Uses current mode for threshold selection (stricter in LOCK/NANO).
        // Spikes from timestamp jitter are replaced with median of window.
        // =======================================================================
        let filter_mode = if self.in_nano_mode {
            FilterMode::Nano
        } else if self.is_locked {
            FilterMode::Lock
        } else if self.in_production_mode {
            FilterMode::Prod
        } else {
            FilterMode::Acq
        };

        let filter_result = self.spike_filter.filter(raw_rate_ppm, filter_mode);
        let filtered_rate_ppm = filter_result.value;

        // Log when spike is detected and rejected
        if filter_result.is_spike {
            info!(
                "[Spike] REJECTED {:+.1}us/s (dev={:.1}, thresh={:.1}, median={:.1})",
                raw_rate_ppm,
                filter_result.deviation,
                filter_result.threshold,
                filter_result.median
            );
        }

        // Log spike statistics periodically (every 100 samples)
        let (total, rejected, ratio) = self.spike_filter.stats();
        if total > 0 && total % 100 == 0 {
            debug!(
                "[Spike] Stats: {}/{} rejected ({:.1}%), MAD={:.2}",
                rejected, total, ratio, filter_result.mad
            );
        }

        // Smooth rate with exponential moving average (on FILTERED rate)
        // Use adaptive alpha from jitter estimator:
        // - Low-jitter systems (strih.lan): α=0.3 for responsive tracking
        // - High-jitter systems (stream.lan): α=0.1 for heavy smoothing
        let adaptive_alpha = self.jitter_estimator.add_sample(filtered_rate_ppm);
        self.smoothed_rate_ppm =
            self.smoothed_rate_ppm * (1.0 - adaptive_alpha) + filtered_rate_ppm * adaptive_alpha;
        let rate_ppm = self.smoothed_rate_ppm;

        // Log jitter statistics periodically (every 50 samples when adjusted)
        if self.jitter_estimator.sample_count() > 0
            && self.jitter_estimator.sample_count() % 50 == 0
            && (adaptive_alpha - 0.3).abs() > 0.01
        {
            info!(
                "[Jitter] stddev={:.2}µs/s α={:.2} (samples={})",
                self.jitter_estimator.last_jitter(),
                adaptive_alpha,
                self.jitter_estimator.sample_count()
            );
        }

        // =======================================================================
        // ACCUMULATED PHASE ERROR TRACKING
        // =======================================================================
        // Track estimated phase drift between NTP steps for monitoring.
        // Uses smoothed rate (µs/s) integrated over time.
        // =======================================================================
        let now = now();
        if let Some(last_time) = self.last_phase_accumulation_time {
            let dt = now.secs_since(last_time);
            // rate_ppm is in µs/s, so rate_ppm * dt gives µs of accumulated error
            self.accumulated_phase_error_us += rate_ppm * dt;
        }
        self.last_phase_accumulation_time = Some(now);
        rate_ppm
    }

    /// Step 3: the servo's gain mode after a MEASURED window (dsync#142: never moved by a
    /// window without a previous offset): NANO entry/exit while locked, then ACQ/PROD.
    pub fn update_modes(&mut self, rate_ppm: f64) {
        // THREE-PHASE CONTROL: ACQ → PROD → NANO based on rate stability
        let abs_rate = rate_ppm.abs();

        // NANO mode transitions (from LOCK state only)
        if self.is_locked {
            if abs_rate < NANO_ENTER_RATE_US {
                self.nano_sustain_count += 1;
                self.nano_exit_count = 0; // Reset exit counter when drift is good
                                          // Log progress towards NANO every 10 samples
                #[allow(clippy::manual_is_multiple_of)]
                if self.nano_sustain_count % 10 == 0 && !self.in_nano_mode {
                    debug!(
                        "[NANO] Sustain count: {}/{}",
                        self.nano_sustain_count, NANO_SUSTAIN_COUNT
                    );
                }
                if self.nano_sustain_count >= NANO_SUSTAIN_COUNT && !self.in_nano_mode {
                    self.in_nano_mode = true;
                    info!(
                        "[PTP] === NANO MODE === Ultra-precise servo engaged (after {} samples)",
                        NANO_SUSTAIN_COUNT
                    );
                }
            } else if abs_rate > NANO_EXIT_RATE_US {
                // Above exit threshold - count towards exit (hysteresis)
                self.nano_exit_count += 1;
                if self.in_nano_mode {
                    if self.nano_exit_count >= NANO_EXIT_COUNT {
                        self.in_nano_mode = false;
                        self.nano_sustain_count = 0;
                        self.nano_exit_count = 0;
                        info!(
                            "[PTP] === LOCK MODE === Exiting NANO (drift {:+.2}us/s for {} samples)",
                            rate_ppm, NANO_EXIT_COUNT
                        );
                    } else {
                        debug!(
                            "[NANO] Exit warning {}/{}: drift {:+.2}us/s",
                            self.nano_exit_count, NANO_EXIT_COUNT, rate_ppm
                        );
                    }
                }
                // Reset sustain count when we exceed exit threshold (even if not in NANO yet)
                // This ensures we need CONSECUTIVE samples below threshold to enter
                if self.nano_sustain_count > 0 {
                    debug!(
                        "[NANO] Reset entry counter: drift {:+.2}us/s > exit threshold",
                        abs_rate
                    );
                    self.nano_sustain_count = 0;
                }
            } else {
                // Between thresholds (0.5-1.0): reset exit counter but don't change entry counter
                self.nano_exit_count = 0;
            }
        } else {
            // Not locked - can't be in NANO
            self.in_nano_mode = false;
            self.nano_sustain_count = 0;
            self.nano_exit_count = 0;
        }

        // ACQ/PROD transitions
        if abs_rate < 5.0 {
            // Rate stable within 5µs/s
            self.in_production_mode = true;
        } else if abs_rate > 20.0 {
            // Rate unstable above 20µs/s
            self.in_production_mode = false;
        }
    }

    /// Step 4: the frequency word for this window — the gains of the current mode, the integrator
    /// and the lock count (both only for a MEASURED window, dsync#142).
    ///
    /// `hold_lock` (dsync#146): the phase lock owns the word, so an unstable window does not
    /// count toward the UNLOCK — the rate it measures is the frequency the phase lock has not
    /// learned yet, which the phase lock is retiring itself.
    pub fn word(&mut self, rate_ppm: f64, measured: bool, hold_lock: bool) -> ServoWord {
        let abs_rate = rate_ppm.abs();
        // Select gains based on mode
        let (p_gain, p_max, i_gain, phase_name) = if self.in_nano_mode {
            (P_GAIN_NANO, P_MAX_NANO_PPM, I_GAIN_NANO, "NANO")
        } else if self.in_production_mode {
            (P_GAIN_PROD, P_MAX_PROD_PPM, 0.05, "PROD")
        } else {
            (P_GAIN_ACQ, P_MAX_ACQ_PPM, 0.05, "ACQ")
        };

        // P-term: responds to rate of change (not absolute offset!)
        // NANO mode: apply deadband - don't correct tiny rates (noise)
        let effective_rate = if self.in_nano_mode && abs_rate < NANO_DEADBAND_US {
            0.0 // Within deadband, no correction needed
        } else {
            rate_ppm
        };

        // Negative rate = clock too slow, need positive adjustment
        let p_term = (-effective_rate * p_gain).clamp(-p_max, p_max);

        // I-term: Integrate rate error to learn true drift
        // Uses mode-appropriate gain
        let i_term = -effective_rate * i_gain;
        if measured {
            self.drift_baseline_ppm =
                (self.drift_baseline_ppm + i_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);
        }

        // Total correction = drift baseline + P-term
        let total_correction =
            (self.drift_baseline_ppm + p_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);

        // Lock state: based on rate stability, not absolute offset
        let rate_stable = abs_rate < 5.0; // Within 5ppm
        if !measured {
            // dsync#142: no rate, no lock evidence either way.
        } else if rate_stable {
            self.lock_stable_count += 1;
            if self.lock_stable_count >= LOCK_STABLE_COUNT && !self.is_locked {
                self.is_locked = true;
                info!(
                    "[PTP] === LOCKED === Adj:{:+.1}ppm",
                    self.drift_baseline_ppm
                );
            }
        } else if hold_lock {
            // dsync#146: the phase lock owns the word.
        } else {
            if self.lock_stable_count > 0 {
                self.lock_stable_count -= 1; // Gradual unlock
            }
            if self.lock_stable_count == 0 && self.is_locked {
                self.is_locked = false;
                info!("[PTP] === UNLOCKED === Drift:{:+.1}us/s", rate_ppm);
            }
        }
        ServoWord {
            word_ppm: total_correction,
            phase_name,
        }
    }

    /// dsync#142 / dsync#136: a seed the judge rejected. Put the servo where a cold start would
    /// be — the integrator and the lock count at 0, the smoothed rate rebased from the seeded
    /// word (`word_ppm`, the word in force over the judged window) to a word of 0, the spike
    /// filter and the jitter estimate cleared (their history mixes rates against both words).
    /// The caller takes `word_ppm` off this window's raw rate too.
    pub fn drop_seed(&mut self, word_ppm: f64) {
        self.drift_baseline_ppm = 0.0;
        self.lock_stable_count = 0;
        self.smoothed_rate_ppm -= word_ppm;
        self.spike_filter.clear();
        self.jitter_estimator.clear();
    }

    /// Forget the previous window's offset: the next window is no measurement (dsync#142).
    /// For every measurement reset (a clock step, a PTP outage, a new time base).
    pub fn reset_measurement(&mut self) {
        self.last_offset_us = None;
        self.last_offset_time = None;
    }

    /// The mode name a window reports: NANO, LOCK, or the gain mode's name.
    pub fn status_name(&self, phase_name: &'static str) -> &'static str {
        if self.in_nano_mode {
            "NANO"
        } else if self.is_locked {
            "LOCK"
        } else {
            phase_name
        }
    }
}

/// dsync#146 — the phase lock's own deviation from its learned frequency, removed from the
/// rate servo's measurement once the phase lock owns the word.
///
/// Once engaged, the rate servo still measures `d(offset)/dt` — and that is the phase lock's
/// DELIBERATE deviation from the learned frequency: its P term and, on the first engagement, the
/// boot pull's `−r/τ`. Read as a rate it decays the rate lock. So every measured rate is first
/// decoupled: `rate − (word − integrator)`, averaged over the two words in force across the window
/// (a median sits mid-window). Without an engagement both deviations are 0 and the rate servo is
/// unchanged, bit for bit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WordDeviation {
    /// The phase lock's last two words minus its integrator (ppm), oldest first.
    pub ppm: [f64; 2],
}

impl WordDeviation {
    /// The phase lock wrote `engaged_word` (with `integrator_ppm` its learned frequency); `None`
    /// = the rate servo's word applies (deviation 0).
    pub fn note(&mut self, engaged_word: Option<f64>, integrator_ppm: f64) {
        let deviation = engaged_word.map_or(0.0, |w| w - integrator_ppm);
        self.ppm = [self.ppm[1], deviation];
    }

    /// The deviation in force over the last measured span: the average of the last two words'.
    pub fn span_ppm(&self) -> f64 {
        (self.ppm[0] + self.ppm[1]) / 2.0
    }

    pub fn clear(&mut self) {
        self.ppm = [0.0; 2];
    }
}

#[cfg(test)]
mod tests;
