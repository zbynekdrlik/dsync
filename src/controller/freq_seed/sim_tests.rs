//! dantesync#142 — closed-loop simulation of a cold start on iemmixer's gen3 oscillator (the
//! servo learned +33.3 ppm at 47.8 s and +35.7 ppm at 78.3 s on v1.20.0, LOCK ~86 s after boot).
//!
//! The world: the window offset moves by (natural + word) µs per second, the word being whatever
//! the controller last wrote to the clock (closed loop, `.claude/rules/clock-discipline-and-
//! testing.md`), plus a seeded per-window jitter. A window is 4 Syncs at 8 Hz = 0.5 s; its time
//! base is set by hand (`last_offset_time`), so the run takes no wall time. Window 1 is the first
//! window after "Sync established" (the warmup is before it).
//!
//! Measured on this model (printed by `cargo test --lib freq_seed::sim_tests -- --nocapture`;
//! quoted in the PR): a good seed locks within a few windows, a seed 50 ppm off is rejected at the
//! 3rd measured window and then learns like the cold start, and the cold start is unchanged
//! apart from the first window no longer counting.

use super::super::*;
use crate::clock::MockSystemClock;
use crate::drift_file::{self, DriftRecord, SEED_JUDGE_WINDOWS};
use crate::traits::{MockNtpSource, MockPtpNetwork};

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

/// gen3: the oscillator runs 35 ppm slow against the grandmaster, so the servo learns +35 ppm.
const NATURAL_PPM: f64 = -35.0;
const LEARNED_PPM: f64 = -NATURAL_PPM;
const WINDOW_S: f64 = 0.5;
/// Per-window jitter of the median offset (µs, 1 σ): a Linux kernel-timestamped box.
const JITTER_US: f64 = 1.0;
const MAX_WINDOWS: usize = 600;

struct Run {
    /// Window (1-based) at which LOCK was reached.
    lock_window: Option<usize>,
    /// The windows at which the gain mode switched to PROD.
    prod_entries: Vec<usize>,
    /// The windows at which it fell back from PROD to ACQ.
    acq_returns: Vec<usize>,
    /// The integrator right after the seed was judged (window `SEED_JUDGE_WINDOWS + 1`).
    baseline_after_judge: f64,
    source: Option<String>,
    final_baseline: f64,
}

impl Run {
    fn lock_s(&self) -> f64 {
        self.lock_window.expect("locked") as f64 * WINDOW_S
    }
}

/// xorshift64* + Box-Muller, a fixed seed per run (never an unseeded source).
struct Noise(u64);
impl Noise {
    fn uniform(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64).max(1e-300)
    }
    fn gauss(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Run the closed loop from a fresh controller; `seed` = the drift file's frequency (or none).
fn run(seed: Option<f64>, noise_seed: u64) -> Run {
    let word = Arc::new(std::sync::Mutex::new(0.0_f64));
    let w = word.clone();
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(move |factor| {
        *w.lock().expect("word") = (factor - 1.0) * 1_000_000.0;
        Ok(())
    });
    let mut config = SystemConfig::default();
    config.filters.calibration_samples = 0;
    config.filters.warmup_secs = 0.0;
    let mut c: Ctl = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    if let Some(ppm) = seed {
        let record = DriftRecord {
            freq_ppm: ppm,
            written_unix_s: wall_now_ns() / 1_000_000_000 - 600,
        };
        drift_file::save(&path, &record).expect("saved");
    }
    c.open_drift_file(&path);

    let mut noise = Noise(noise_seed);
    let mut true_offset_us = 0.0_f64;
    let mut out = Run {
        lock_window: None,
        prod_entries: Vec::new(),
        acq_returns: Vec::new(),
        baseline_after_judge: f64::NAN,
        source: None,
        final_baseline: 0.0,
    };
    let mut prod = false;
    for n in 1..=MAX_WINDOWS {
        let applied = *word.lock().expect("word");
        true_offset_us += (NATURAL_PPM + applied) * WINDOW_S;
        if c.last_offset_us.is_some() {
            c.last_offset_time = Some(Instant::now() - Duration::from_secs_f64(WINDOW_S));
        }
        c.apply_self_tuning_servo(true_offset_us + JITTER_US * noise.gauss());
        if c.in_production_mode && !prod {
            out.prod_entries.push(n);
        }
        if !c.in_production_mode && prod {
            out.acq_returns.push(n);
        }
        prod = c.in_production_mode;
        if n == SEED_JUDGE_WINDOWS + 1 {
            out.baseline_after_judge = c.drift_baseline_ppm;
        }
        if c.is_locked && out.lock_window.is_none() {
            out.lock_window = Some(n);
        }
    }
    out.source = c.freq_seed.source.clone().into();
    out.final_baseline = c.drift_baseline_ppm;
    out
}

const SEEDS: [u64; 4] = [
    0x5EED_0142_0001_2026,
    0x5EED_0142_0002_2026,
    0x5EED_0142_0003_2026,
    0x5EED_0142_0004_2026,
];

#[test]
fn a_good_seed_locks_the_gen3_oscillator_within_ten_seconds_of_sync_established_142() {
    // A seed within 2 ppm of the oscillator (e.g. a warmer or colder box than when it was saved).
    for good in [LEARNED_PPM, LEARNED_PPM - 2.0, LEARNED_PPM + 2.0] {
        for &s in &SEEDS {
            let r = run(Some(good), s);
            println!(
                "[#142 sim] seed {:+.1}ppm (oscillator needs {:+.1}): LOCK at window {} = {:.1} s \
                 after Sync established; PROD at {:?}, back to ACQ at {:?}; source {:?}",
                good,
                LEARNED_PPM,
                r.lock_window.unwrap_or(0),
                r.lock_window.unwrap_or(0) as f64 * WINDOW_S,
                r.prod_entries,
                r.acq_returns,
                r.source
            );
            assert_eq!(r.source.as_deref(), Some("file"), "the seed is kept");
            assert!(r.lock_s() <= 10.0, "LOCK at {:.1} s", r.lock_s());
            assert!(
                r.lock_window.expect("locked") >= LOCK_STABLE_COUNT + 1,
                "LOCK still needs {} measured windows after the first (unmeasured) one",
                LOCK_STABLE_COUNT
            );
            assert!(
                r.acq_returns.is_empty(),
                "a good seed never falls back from PROD to ACQ"
            );
            assert!((r.final_baseline - LEARNED_PPM).abs() < 1.0);
        }
    }
}

#[test]
fn a_seed_fifty_ppm_off_is_dropped_and_the_cold_learn_runs_142() {
    for wrong in [LEARNED_PPM + 50.0, LEARNED_PPM - 50.0] {
        for &s in &SEEDS {
            let seeded = run(Some(wrong), s);
            let cold = run(None, s);
            println!(
                "[#142 sim] seed {:+.1}ppm (50 off): {:?}; integrator after the judge {:+.2}ppm; \
                 LOCK at {:.1} s (the cold start: {:.1} s)",
                wrong,
                seeded.source,
                seeded.baseline_after_judge,
                seeded.lock_s(),
                cold.lock_s()
            );
            let source = seeded.source.clone().expect("source");
            assert!(source.starts_with("rejected: "), "{source}");
            assert!(
                seeded.baseline_after_judge.abs() < 5.0,
                "the integrator went back to ~0 at the verdict (one window's step since), got \
                 {:+.2}",
                seeded.baseline_after_judge
            );
            assert!((seeded.final_baseline - LEARNED_PPM).abs() < 1.0);
            // Learned from 0 like the cold start: never faster by keeping part of the wrong seed,
            // and not slower than the cold start plus the judge's windows (and a margin).
            assert!(
                (seeded.lock_s() - cold.lock_s()).abs() <= 10.0,
                "rejected-seed LOCK {:.1} s vs cold {:.1} s",
                seeded.lock_s(),
                cold.lock_s()
            );
        }
    }
}

#[test]
fn the_unseeded_cold_start_still_learns_and_locks_142() {
    for &s in &SEEDS {
        let r = run(None, s);
        println!(
            "[#142 sim] no seed: LOCK at {:.1} s after Sync established; PROD at {:?}, back to \
             ACQ at {:?}; source {:?}",
            r.lock_s(),
            r.prod_entries,
            r.acq_returns,
            r.source
        );
        assert!(r
            .source
            .as_deref()
            .is_some_and(|s| s.starts_with("none: no drift file")));
        assert!(
            r.prod_entries.first().map_or(true, |&w| w > 1),
            "no false PROD at the first window"
        );
        assert!((r.final_baseline - LEARNED_PPM).abs() < 1.0);
    }
}
