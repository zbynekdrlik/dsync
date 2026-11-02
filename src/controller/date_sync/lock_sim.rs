//! dantesync#146 — a closed-loop simulation of the WHOLE controller under the phase lock: the rate
//! servo's acquisition, the phase lock's engagement, the reported lock (`/status.is_locked`, DSYX
//! [41]) and its phase error `e`, through a cold or seeded boot, a coordinated date step and a
//! grandmaster frequency step.
//!
//! The world (`.claude/rules/clock-discipline-and-testing.md`, closed loop): the offset `t2 − t1`
//! moves by (natural + grandmaster + word) µs per second, the word being whatever the controller
//! last wrote to the clock. A window is 4 Syncs at 8 Hz = 0.5 s with seeded per-sample jitter; the
//! rate servo gets the window's median offset (µs) and the phase lock the same median as a raw
//! `t2 − t1` (ns), exactly the pair `process_settled_sync` hands over. A wall step moves the
//! offset by the step and runs the controller's real step path (`apply_date_step`); its 2 s grace
//! is modelled by not feeding the next 4 windows. The window's time base is set by hand
//! (`last_offset_time`, `pending_t1_ns`), so a run takes no wall time.

use super::super::*;
use crate::clock::MockSystemClock;
use crate::date_offset::StepKind;
use crate::drift_file::{self, DriftRecord};
use crate::traits::{MockNtpSource, MockPtpNetwork};
use std::sync::Mutex;

pub(super) type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

pub(super) const WINDOW_S: f64 = 0.5;
const SAMPLES: usize = 4;
/// Windows skipped after a step: the controller's 2 s post-step grace.
const GRACE_WINDOWS: usize = 4;
/// The raw `t2 − t1` of the world's zero (any absolute value: `D` is anchored on it).
const BASE_NS: i64 = 1_789_000_000_000_000_000;

/// xorshift64* + Box-Muller, a fixed seed per run (never an unseeded source).
pub(super) struct Noise(pub(super) u64);
impl Noise {
    fn uniform(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64).max(1e-300)
    }
    pub(super) fn gauss(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// One window as the outside world sees it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Obs {
    /// Window number (1-based, counted from "Sync established").
    pub(super) n: usize,
    /// `/status.is_locked` — DSYX [41].
    pub(super) reported: bool,
    /// `/status.ptp_phase_error_us`.
    pub(super) e_us: Option<f64>,
    /// The phase lock owns the word.
    pub(super) engaged: bool,
}

pub(super) struct Sim {
    pub(super) c: Ctl,
    word: Arc<Mutex<f64>>,
    stepped_us: Arc<Mutex<f64>>,
    /// The oscillator against the grandmaster (ppm).
    pub(super) natural_ppm: f64,
    true_offset_us: f64,
    t1_ns: i64,
    noise: Noise,
    jitter_us: f64,
    pub(super) n: usize,
    pub(super) log: Vec<Obs>,
    _dir: tempfile::TempDir,
}

impl Sim {
    /// A fresh controller under the phase lock; `seed` = the drift file's frequency (or none).
    pub(super) fn new(
        natural_ppm: f64,
        seed: Option<f64>,
        noise_seed: u64,
        jitter_us: f64,
    ) -> Self {
        let word = Arc::new(Mutex::new(0.0_f64));
        let stepped_us = Arc::new(Mutex::new(0.0_f64));
        let w = word.clone();
        let s = stepped_us.clone();
        let mut clock = MockSystemClock::new();
        clock.expect_adjust_frequency().returning(move |factor| {
            *w.lock().expect("word") = (factor - 1.0) * 1_000_000.0;
            Ok(())
        });
        clock.expect_step_clock().returning(move |d, sign| {
            *s.lock().expect("stepped") += d.as_nanos() as f64 / 1_000.0 * sign as f64;
            Ok(())
        });
        let mut c: Ctl = PtpController::new(
            clock,
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            Arc::new(RwLock::new(SyncStatus::default())),
            super::tests::phase_lock_config(),
        );
        c.current_gm_uuid = Some(super::tests::PL_GM);
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
        Sim {
            c,
            word,
            stepped_us,
            natural_ppm,
            true_offset_us: 0.0,
            t1_ns: 10_000_000_000,
            noise: Noise(noise_seed),
            jitter_us,
            n: 0,
            log: Vec::new(),
            _dir: dir,
        }
    }

    /// The world's offset over one window under the word in force; returns the window's median.
    fn advance(&mut self) -> f64 {
        let applied = *self.word.lock().expect("word");
        let dt = WINDOW_S / SAMPLES as f64;
        let mut samples = [0.0_f64; SAMPLES];
        for sample in samples.iter_mut() {
            self.true_offset_us += (self.natural_ppm + applied) * dt;
            *sample = self.true_offset_us + self.jitter_us * self.noise.gauss();
        }
        // A step lands on the wall (t2): it moves the offset by the step.
        let stepped = std::mem::take(&mut *self.stepped_us.lock().expect("stepped"));
        self.true_offset_us += stepped;
        self.t1_ns += (WINDOW_S * 1e9) as i64;
        samples.sort_by(|a, b| a.total_cmp(b));
        samples[SAMPLES / 2]
    }

    /// One PTP window through the controller.
    pub(super) fn window(&mut self) -> Obs {
        self.n += 1;
        let median_us = self.advance();
        if self.c.rate.last_offset_us.is_some() {
            self.c.rate.last_offset_time = Some(Instant::now() - Duration::from_secs_f64(WINDOW_S));
        }
        self.c.date_sync.pending_median_ns = Some(BASE_NS + (median_us * 1_000.0).round() as i64);
        self.c.date_sync.pending_t1_ns = self.t1_ns;
        self.c.apply_self_tuning_servo(median_us);
        let obs = self.observe();
        self.log.push(obs);
        obs
    }

    pub(super) fn run(&mut self, windows: usize) {
        for _ in 0..windows {
            self.window();
        }
    }

    /// Run to the first reported lock (at most `max` windows), then `after` more: each run stops
    /// where its assertion is decided (the #142 coverage-timing lesson: under tarpaulin every
    /// instrumented window here slows the timing-sensitive tests running beside it).
    pub(super) fn run_past_lock(&mut self, max: usize, after: usize) {
        while self.first_lock().is_none() && self.n < max {
            self.window();
        }
        self.run(after);
    }

    fn observe(&self) -> Obs {
        let st = self.c.get_status_shared();
        let st = st.read().expect("status");
        Obs {
            n: self.n,
            reported: st.is_locked,
            e_us: st.ptp_phase_error_us,
            engaged: self.c.date_sync.core.engaged(),
        }
    }

    /// The world's offset jumps by `us` with no step the controller knows of (another writer set
    /// the clock, or the grandmaster's time base jumped by less than 1 s).
    pub(super) fn jump_us(&mut self, us: f64) {
        self.true_offset_us += us;
    }

    /// A coordinated date step of `delta_ns` through the controller's real step path, then its 2 s
    /// grace (the world runs on, nothing is fed).
    pub(super) fn date_step(&mut self, delta_ns: i64) {
        self.c.apply_date_step(delta_ns, StepKind::Coordinated, 7);
        for _ in 0..GRACE_WINDOWS {
            self.n += 1;
            self.advance();
            let obs = self.observe();
            self.log.push(obs);
        }
        self.c.last_ntp_step = Some(Instant::now() - Duration::from_secs(3));
    }

    /// The window (1-based) of the first reported lock.
    pub(super) fn first_lock(&self) -> Option<usize> {
        self.log.iter().find(|o| o.reported).map(|o| o.n)
    }

    /// Every change of the reported lock after the first lock: `(window, now locked)`.
    pub(super) fn transitions_after_first_lock(&self) -> Vec<(usize, bool)> {
        let Some(first) = self.first_lock() else {
            return Vec::new();
        };
        self.log
            .windows(2)
            .filter(|p| p[1].n > first && p[0].reported != p[1].reported)
            .map(|p| (p[1].n, p[1].reported))
            .collect()
    }

    /// The worst |e| (µs) of any window reported locked.
    pub(super) fn worst_locked_e_us(&self) -> f64 {
        self.log
            .iter()
            .filter(|o| o.reported)
            .map(|o| o.e_us.map_or(f64::INFINITY, f64::abs))
            .fold(0.0, f64::max)
    }

    /// |e| (µs) at the first reported lock.
    pub(super) fn e_at_first_lock_us(&self) -> Option<f64> {
        let first = self.first_lock()?;
        self.log[first - 1].e_us.map(f64::abs)
    }
}
