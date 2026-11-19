//! dsync#204 — the LOCAL oscillator's frequency ramps by a few ppm over minutes (resolume-pp,
//! 19.11.2026: a GPU/CPU load warmed the crystal, the applied word went −3.8 → −8.5 → −9.6 ppm,
//! and `ptp_phase_error_us` ramped −2 → +95 µs over ~5 min with the box still in LOCK).
//!
//! The plain critically damped PI (~100 s) leaves a ramp `ρ` (ppm/s) a standing phase error of
//! `ρ / K_I`: 6 ppm over 5 min is 0.02 ppm/s, i.e. 200 µs. These closed-loop runs settle a core,
//! then move the oscillator along a ramp under seeded window noise and judge the TRUE phase error
//! (the plant's, not the noisy measurement) against the <50 µs target.

use super::*;

const DT: f64 = 0.5;
const D: i64 = 1_789_000_000_000_000_000;

/// The plant: `e` integrates the oscillator error plus the applied word (µs/s = ppm), one window
/// of transport delay between the command and its effect.
struct Plant {
    e_ns: f64,
    osc_ppm: f64,
    applied: std::collections::VecDeque<f64>,
}

impl Plant {
    fn new(osc_ppm: f64, word: f64) -> Self {
        Plant {
            e_ns: 0.0,
            osc_ppm,
            applied: std::iter::repeat(word).take(2).collect(),
        }
    }
    fn advance(&mut self, cmd: f64, dt: f64) {
        self.applied.push_back(cmd);
        let f = self.applied.pop_front().expect("delay line");
        self.e_ns += (self.osc_ppm + f) * dt * 1_000.0;
    }
}

/// Seeded uniform window noise, ±`half_us`.
struct Jitter {
    state: u64,
    half_ns: f64,
}

impl Jitter {
    fn new(seed: u64, half_us: f64) -> Self {
        Jitter {
            state: seed | 1,
            half_ns: half_us * 1_000.0,
        }
    }
    fn next(&mut self) -> f64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        ((self.state >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0 * self.half_ns
    }
}

/// The oscillator's extra error (ppm) `t` seconds after the load starts.
type Profile = fn(f64) -> f64;

/// 6 ppm along a straight line over 5 minutes, then flat (the ticket's wording).
fn linear_6_ppm_over_5_min(t: f64) -> f64 {
    6.0 * (t / 300.0).clamp(0.0, 1.0)
}

/// 6 ppm along a straight line over 2 minutes, then flat.
fn linear_6_ppm_over_2_min(t: f64) -> f64 {
    6.0 * (t / 120.0).clamp(0.0, 1.0)
}

/// A crystal warming: 6 ppm with a 300 s time constant. The closest shape to the box: on the plain
/// PI it peaks at ~100 µs (resolume-pp read +95 µs).
fn thermal_6_ppm_tau_300_s(t: f64) -> f64 {
    6.0 * (1.0 - (-t.max(0.0) / 300.0).exp())
}

pub(super) const PROFILES: [(&str, Profile); 3] = [
    ("6 ppm / 5 min linear", linear_6_ppm_over_5_min),
    ("6 ppm / 2 min linear", linear_6_ppm_over_2_min),
    ("6 ppm thermal tau 300 s", thermal_6_ppm_tau_300_s),
];

/// The worst true |e| (µs, the plant's phase against the anchor in force) of a settled core while the oscillator moves by `sign × profile`,
/// over `secs` from the start of the load, and the core after it.
pub(super) fn ramp_run(
    profile: Profile,
    sign: f64,
    jitter_us: f64,
    seed: u64,
    secs: f64,
) -> (f64, PhaseLockCore) {
    let mut jitter = Jitter::new(seed, jitter_us);
    let mut c = PhaseLockCore::new();
    let osc0 = -3.8;
    let mut p = Plant::new(osc0, -osc0);
    // Settle: the boot pull and the refinement are long done after 1000 s.
    for _ in 0..2_000 {
        let out = c.on_window(D + (p.e_ns + jitter.next()).round() as i64, true, -osc0, DT);
        p.advance(out.freq_ppm.expect("engaged"), DT);
    }
    // `e` against the anchor in force: `D` was placed on the first windows' noisy reading.
    let e_us = |c: &PhaseLockCore, p: &Plant| {
        (p.e_ns - (c.anchor_ns().expect("anchored") - D) as f64) / 1_000.0
    };
    assert!(
        e_us(&c, &p).abs() < 10.0,
        "settled before the load: {}",
        e_us(&c, &p)
    );
    let mut worst_us = 0.0f64;
    let windows = (secs / DT) as usize;
    for n in 0..windows {
        p.osc_ppm = osc0 + sign * profile(n as f64 * DT);
        let out = c.on_window(D + (p.e_ns + jitter.next()).round() as i64, true, -osc0, DT);
        p.advance(out.freq_ppm.expect("engaged"), DT);
        worst_us = worst_us.max(e_us(&c, &p).abs());
    }
    (worst_us, c)
}

/// dsync#204: under a local-oscillator ramp of 6 ppm (both directions, three shapes, Windows-level
/// window noise of ±10 and ±20 µs, several seeds) the phase lock keeps |e| < 50 µs.
#[test]
fn a_local_oscillator_ramp_of_6_ppm_keeps_the_phase_within_50_us_204() {
    let mut report = Vec::new();
    let mut fails = Vec::new();
    for (name, profile) in PROFILES {
        for sign in [1.0, -1.0] {
            for jitter_us in [10.0, 20.0] {
                let mut worst = 0.0f64;
                for seed in [0x204_0001_u64, 0x204_0011, 0x204_0021] {
                    let (w, _) = ramp_run(profile, sign, jitter_us, seed, 900.0);
                    worst = worst.max(w);
                }
                report.push(format!("{name} {sign:+} ±{jitter_us} µs: {worst:.1} µs"));
                if worst >= 50.0 {
                    fails.push(format!("{name} {sign:+} ±{jitter_us} µs: {worst:.1} µs"));
                }
            }
        }
    }
    eprintln!(
        "dsync#204 local ramp, worst |e|:\n  {}",
        report.join("\n  ")
    );
    assert!(fails.is_empty(), "|e| ≥ 50 µs: {fails:?}");
}
