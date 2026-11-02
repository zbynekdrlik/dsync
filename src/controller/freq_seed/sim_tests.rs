//! dantesync#142 — closed-loop simulation of a cold start on iemmixer's gen3 oscillator (the
//! servo learned +33.3 ppm at 47.8 s and +35.7 ppm at 78.3 s on v1.20.0, LOCK ~86 s after boot).
//!
//! The world: the offset moves by (natural + word) µs per second, the word being whatever the
//! controller last wrote to the clock (closed loop, `.claude/rules/clock-discipline-and-
//! testing.md`). A window is 4 Syncs at 8 Hz = 0.5 s, each sample with seeded jitter, the word
//! held through the window (it changes at the window's end, as in the controller) and the
//! window's offset the controller's median (`sorted[len / 2]`), which sits mid-window. The
//! window's time base is set by hand (`last_offset_time`), so a run takes no wall time. Window 1
//! is the first window after "Sync established" (the warmup is before it).
//!
//! Measured on this model (printed by `cargo test --lib freq_seed::sim_tests -- --nocapture`;
//! quoted in the PR). The model's cold start locks in ~33 s against ~70 s after "Sync
//! established" on the rig (its jitter and the oscillator are idealized): read the RELATIVE
//! numbers here, the rig's boot log for the absolute one.

use super::super::*;
use crate::clock::MockSystemClock;
use crate::drift_file::{self, DriftRecord, SEED_JUDGE_WINDOWS};
use crate::traits::{MockNtpSource, MockPtpNetwork};

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

/// gen3: the oscillator runs 35 ppm slow against the grandmaster, so the servo learns +35 ppm.
const NATURAL_PPM: f64 = -35.0;
const LEARNED_PPM: f64 = -NATURAL_PPM;
const WINDOW_S: f64 = 0.5;
const SAMPLES: usize = 4;
/// Per-sample timestamp jitter (µs, 1 σ): a Linux kernel-timestamped box.
const LINUX_JITTER_US: f64 = 1.0;
/// … and a Windows Npcap box: a moderate and a pessimistic per-sample figure (neither measured on
/// the rig).
const WINDOWS_JITTER_US: [f64; 2] = [10.0, 30.0];
const MAX_WINDOWS: usize = 120;

struct Run {
    /// Window (1-based) at which LOCK was reached.
    lock_window: Option<usize>,
    /// The windows at which the gain mode switched to PROD.
    prod_entries: Vec<usize>,
    /// The windows at which it fell back from PROD to ACQ.
    acq_returns: Vec<usize>,
    /// The integrator after the seed was judged and one more window (window
    /// `SEED_JUDGE_WINDOWS + 2`): the direction the learn went from 0.
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
fn run(seed: Option<f64>, noise_seed: u64, jitter_us: f64) -> Run {
    run_for(seed, noise_seed, jitter_us, MAX_WINDOWS)
}

/// [`run`] for `windows` windows only: the statistics below run 160 closed loops per case, and
/// under CI's coverage instrumentation a full-length run each starved the timing-sensitive tests
/// running beside them (the #136 loop-stall bound).
fn run_for(seed: Option<f64>, noise_seed: u64, jitter_us: f64, windows: usize) -> Run {
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
    let dt_sample = WINDOW_S / SAMPLES as f64;
    for n in 1..=windows {
        let applied = *word.lock().expect("word");
        let mut samples = [0.0_f64; SAMPLES];
        for sample in samples.iter_mut() {
            true_offset_us += (NATURAL_PPM + applied) * dt_sample;
            *sample = true_offset_us + jitter_us * noise.gauss();
        }
        samples.sort_by(|a, b| a.total_cmp(b));
        if c.rate.last_offset_us.is_some() {
            c.rate.last_offset_time = Some(Instant::now() - Duration::from_secs_f64(WINDOW_S));
        }
        c.apply_self_tuning_servo(samples[SAMPLES / 2]);
        if c.rate.in_production_mode && !prod {
            out.prod_entries.push(n);
        }
        if !c.rate.in_production_mode && prod {
            out.acq_returns.push(n);
        }
        prod = c.rate.in_production_mode;
        if n == SEED_JUDGE_WINDOWS + 2 {
            out.baseline_after_judge = c.rate.drift_baseline_ppm;
        }
        if c.rate.is_locked && out.lock_window.is_none() {
            out.lock_window = Some(n);
        }
    }
    out.source = c.freq_seed.source.clone().into();
    out.final_baseline = c.rate.drift_baseline_ppm;
    out
}

const SEEDS: [u64; 4] = [
    0x5EED_0142_0001_2026,
    0x5EED_0142_0002_2026,
    0x5EED_0142_0003_2026,
    0x5EED_0142_0004_2026,
];

fn print(label: &str, r: &Run) {
    println!(
        "[#142 sim] {label}: LOCK at window {} = {:.1} s after Sync established; PROD at {:?}, \
         back to ACQ at {:?}; integrator after the judge {:+.2}ppm; source {:?}",
        r.lock_window.unwrap_or(0),
        r.lock_window.unwrap_or(0) as f64 * WINDOW_S,
        r.prod_entries,
        r.acq_returns,
        r.baseline_after_judge,
        r.source
    );
}

fn a_good_seed_locks_the_gen3_oscillator_within_ten_seconds_of_sync_established_142() {
    // A seed within 2 ppm of the oscillator (e.g. a warmer or colder box than when it was saved).
    for good in [LEARNED_PPM, LEARNED_PPM - 2.0, LEARNED_PPM + 2.0] {
        for &s in &SEEDS {
            let r = run(Some(good), s, LINUX_JITTER_US);
            print(&format!("seed {good:+.1}ppm (needs {LEARNED_PPM:+.1})"), &r);
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

/// [`STAT_RUNS`] fixed noise seeds for the rates below (a statistic needs more than one noise sample).
const STAT_RUNS: usize = 20;

fn many_seeds() -> impl Iterator<Item = u64> {
    (1..=STAT_RUNS as u64).map(|k| 0x5EED_0142_0000_2026 ^ k.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn a_good_seed_is_kept_under_windows_level_jitter_142() {
    // At 30 µs per sample the judge's 2 s slope has ~13 µs/s of noise, so a seed 2 ppm off is
    // occasionally dropped: the cost is the cold learn (the behaviour before 1.22), never a wrong
    // seed. At 10 µs it is always kept. A seed from another oscillator is dropped every time.
    // The seed alternates 2 ppm below / above the oscillator across the runs.
    let good = |k: usize| LEARNED_PPM + if k % 2 == 0 { -2.0 } else { 2.0 };
    for jitter in WINDOWS_JITTER_US {
        let runs: Vec<Run> = many_seeds()
            .enumerate()
            .map(|(k, s)| run_for(Some(good(k)), s, jitter, 60))
            .collect();
        let kept: Vec<&Run> = runs
            .iter()
            .filter(|r| r.source.as_deref() == Some("file"))
            .collect();
        let mut lock_s: Vec<f64> = kept.iter().map(|r| r.lock_s()).collect();
        lock_s.sort_by(|a, b| a.total_cmp(b));
        println!(
            "[#142 sim] seed ±2 ppm off, {jitter} us sample jitter: kept in {} of {} runs; LOCK \
             (kept) median {:.1} s, max {:.1} s",
            kept.len(),
            runs.len(),
            lock_s[lock_s.len() / 2],
            lock_s[lock_s.len() - 1]
        );
        if jitter <= 10.0 {
            assert_eq!(kept.len(), runs.len(), "always kept at {jitter} us");
        } else {
            assert!(kept.len() * 10 >= runs.len() * 8, "kept in {}", kept.len());
        }
        let dropped = many_seeds()
            .enumerate()
            .filter(|&(k, s)| {
                run_for(Some(good(k) + 50.0), s, jitter, SEED_JUDGE_WINDOWS + 2)
                    .source
                    .as_deref()
                    .is_some_and(|src| src.starts_with("rejected: "))
            })
            .count();
        assert_eq!(
            dropped, STAT_RUNS,
            "a seed 50 ppm off is dropped in every run"
        );
    }
}

fn the_judge_reads_the_error_against_the_seed_not_against_the_p_term_142() {
    // The ACQ P term (0.8) pulls the word toward the oscillator after the first measured window,
    // so the raw rate understates a wrong seed: 25 ppm off reads ~25, ~15, ~11, ... A judge on the
    // raw rate accepts it; on the rate against the seed it does not. A 15 ppm error (a seed closer
    // than the cold start's 0) is kept. (The window-average word itself is pinned by
    // `freq_seed::tests::the_judge_charges_each_window_half_to_the_previous_word_and_half_to_the_last_142`.)
    for &s in &SEEDS {
        for off in [25.0, -25.0] {
            let r = run(Some(LEARNED_PPM + off), s, LINUX_JITTER_US);
            print(&format!("seed {off:+.0}ppm off"), &r);
            assert!(
                r.source
                    .as_deref()
                    .is_some_and(|s| s.starts_with("rejected: ")),
                "{off:+} ppm off: {:?}",
                r.source
            );
        }
        for off in [15.0, -15.0] {
            let r = run(Some(LEARNED_PPM + off), s, LINUX_JITTER_US);
            print(&format!("seed {off:+.0}ppm off"), &r);
            assert_eq!(r.source.as_deref(), Some("file"), "{off:+} ppm off: kept");
            assert!((r.final_baseline - LEARNED_PPM).abs() < 1.0);
        }
    }
}

fn a_seed_fifty_ppm_off_is_dropped_and_the_cold_learn_runs_142() {
    for wrong in [LEARNED_PPM + 50.0, LEARNED_PPM - 50.0] {
        for &s in &SEEDS {
            let seeded = run(Some(wrong), s, LINUX_JITTER_US);
            let cold = run(None, s, LINUX_JITTER_US);
            print(&format!("seed {wrong:+.1}ppm (50 off)"), &seeded);
            println!(
                "[#142 sim]   ... the cold start with the same noise: LOCK at {:.1} s",
                cold.lock_s()
            );
            let source = seeded.source.clone().expect("source");
            assert!(source.starts_with("rejected: "), "{source}");
            assert!(
                seeded.baseline_after_judge > 0.0 && seeded.baseline_after_judge < 10.0,
                "learned from 0 toward the oscillator's +35 (never the seed's way): {:+.2}",
                seeded.baseline_after_judge
            );
            assert!((seeded.final_baseline - LEARNED_PPM).abs() < 1.0);
            assert!(
                (seeded.lock_s() - cold.lock_s()).abs() <= 10.0,
                "rejected-seed LOCK {:.1} s vs cold {:.1} s",
                seeded.lock_s(),
                cold.lock_s()
            );
        }
    }
}

fn the_unseeded_cold_start_still_learns_and_locks_142() {
    for &s in &SEEDS {
        let r = run(None, s, LINUX_JITTER_US);
        print("no seed", &r);
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

/// The scenarios run one after another in ONE test: run in parallel they kept several cores busy
/// under CI's coverage instrumentation, beside the timing-sensitive tests of the same binary (the
/// NTP-burst loop-stall bound failed on PR 143's first runs).
#[test]
fn closed_loop_simulation_of_the_gen3_cold_boot_142() {
    let _heavy = crate::controller::heavy_test_guard();
    the_unseeded_cold_start_still_learns_and_locks_142();
    a_good_seed_locks_the_gen3_oscillator_within_ten_seconds_of_sync_established_142();
    the_judge_reads_the_error_against_the_seed_not_against_the_p_term_142();
    a_seed_fifty_ppm_off_is_dropped_and_the_cold_learn_runs_142();
    a_good_seed_is_kept_under_windows_level_jitter_142();
}
