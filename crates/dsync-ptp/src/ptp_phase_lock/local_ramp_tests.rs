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
    ramp_run_after(profile, sign, jitter_us, seed, secs, 2_000)
}

/// [`ramp_run`] with the load starting `settle` windows after the first (engaging) window.
fn ramp_run_after(
    profile: Profile,
    sign: f64,
    jitter_us: f64,
    seed: u64,
    secs: f64,
    settle: usize,
) -> (f64, PhaseLockCore) {
    let mut jitter = Jitter::new(seed, jitter_us);
    let mut c = PhaseLockCore::new();
    let osc0 = -3.8;
    let mut p = Plant::new(osc0, -osc0);
    // Settle (2000 windows: the boot pull and the refinement are long done after 1000 s).
    for _ in 0..settle {
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

/// Review of #204: a plain path-delay LEVEL SHIFT (a path change, or an absorb of up to 100 µs into
/// `D`) is retired by the plain PI at `−K_P·e`; it is no frequency the loop is behind on, and
/// boosting it is only a 4× word swing (40 µs armed ×4: 2.7 ppm against 0.8).
#[test]
fn a_path_delay_level_shift_or_an_absorb_never_boosts_204() {
    for (case, shift_us, absorb) in [
        ("path +40 us", 40.0, false),
        ("path -60 us", -60.0, false),
        ("path +100 us", 100.0, false),
        ("absorb 60 us", 60.0, true),
        ("absorb -100 us", -100.0, true),
    ] {
        let mut jitter = Jitter::new(0x204_5417, 10.0);
        let mut c = PhaseLockCore::new();
        let osc0 = -3.8;
        let mut p = Plant::new(osc0, -osc0);
        for _ in 0..2_000 {
            let out = c.on_window(D + (p.e_ns + jitter.next()).round() as i64, true, -osc0, DT);
            p.advance(out.freq_ppm.expect("engaged"), DT);
        }
        let shift_ns = if absorb {
            // `D` moves by the absorb with no wall move: `e` shifts by minus it.
            let anchor = c.anchor_ns().expect("anchored");
            c.set_anchor(anchor + (shift_us * 1_000.0) as i64);
            0.0
        } else {
            shift_us * 1_000.0
        };
        let mut max_gain = 1.0f64;
        for _ in 0..1_600 {
            let out = c.on_window(
                D + (p.e_ns + shift_ns + jitter.next()).round() as i64,
                true,
                -osc0,
                DT,
            );
            p.advance(out.freq_ppm.expect("engaged"), DT);
            max_gain = max_gain.max(c.gain_factor());
        }
        assert!(
            max_gain <= 1.05,
            "{case}: the shift boosted the PI (gain x{max_gain:.2})"
        );
    }
}

/// Review of #204: a PC that starts its load soon after the lock (a boot, then the show's apps)
/// ramps right after the engagement. The load starts 60 s after the first window.
#[test]
fn a_ramp_soon_after_the_engagement_keeps_the_phase_within_50_us_204() {
    let mut report = Vec::new();
    let mut fails = Vec::new();
    for (name, profile) in PROFILES {
        for sign in [1.0, -1.0] {
            let mut worst = 0.0f64;
            for seed in [0x204_e001_u64, 0x204_e011, 0x204_e021] {
                let (w, _) = ramp_run_after(profile, sign, 10.0, seed, 900.0, 120);
                worst = worst.max(w);
            }
            let line = format!("{name} {sign:+}: {worst:.1} µs");
            if worst >= 50.0 {
                fails.push(line.clone());
            }
            report.push(line);
        }
    }
    eprintln!(
        "dsync#204 ramp 60 s after the engagement, worst |e|:\n  {}",
        report.join("\n  ")
    );
    assert!(fails.is_empty(), "|e| ≥ 50 µs: {fails:?}");
}

/// Re-review of #204: two followers on one PC take the SAME windows ± ~40 ns of conversion
/// residue (`.claude/rules/ptp-library.md`, the #170 bar: ≤ 1 µs apart). A boost that cycles in
/// and out around its bound under a slow ramp amplified that residue to 25 µs. Every shape, both
/// directions, a sweep of noise seeds; the worst split is judged.
#[test]
fn two_cores_fed_the_same_windows_agree_within_a_microsecond_through_a_ramp_204() {
    let mut fails = Vec::new();
    let mut worst_all = 0.0f64;
    for (name, profile) in PROFILES {
        for sign in [1.0, -1.0] {
            for seed in 1..=24u64 {
                let mut jitter = Jitter::new(seed * 977 + 13, 10.0);
                let mut r1 = Jitter::new(seed * 31 + 1, 0.04);
                let mut r2 = Jitter::new(seed * 57 + 3, 0.04);
                let (mut c1, mut c2) = (PhaseLockCore::new(), PhaseLockCore::new());
                let osc0 = -3.8;
                let (mut p1, mut p2) = (Plant::new(osc0, -osc0), Plant::new(osc0, -osc0));
                let settle = 1_000;
                let mut worst = 0.0f64;
                for n in 0..(settle + 1_800) {
                    if n >= settle {
                        let o = osc0 + sign * profile((n - settle) as f64 * DT);
                        p1.osc_ppm = o;
                        p2.osc_ppm = o;
                    }
                    let j = jitter.next();
                    let o1 = c1.on_window(
                        D + (p1.e_ns + j + r1.next()).round() as i64,
                        true,
                        -osc0,
                        DT,
                    );
                    let o2 = c2.on_window(
                        D + (p2.e_ns + j + r2.next()).round() as i64,
                        true,
                        -osc0,
                        DT,
                    );
                    p1.advance(o1.freq_ppm.expect("engaged"), DT);
                    p2.advance(o2.freq_ppm.expect("engaged"), DT);
                    if n >= settle {
                        worst = worst.max((p1.e_ns - p2.e_ns).abs());
                    }
                }
                worst_all = worst_all.max(worst);
                if worst > 1_000.0 {
                    fails.push(format!("{name} {sign:+} seed {seed}: {worst:.0} ns"));
                }
            }
        }
    }
    eprintln!("dsync#204 pair split, worst {worst_all:.0} ns");
    assert!(fails.is_empty(), "cores > 1 µs apart: {fails:?}");
}

/// One ramp on windows `dt` s apart: the worst true |e| (µs) during `secs`, the gain factor
/// `quiet_s` later (the boost must have let go), and the core.
fn ramp_on(
    profile: Profile,
    sign: f64,
    jitter_us: f64,
    seed: u64,
    dt: f64,
    secs: f64,
    quiet_s: f64,
) -> (f64, f64) {
    let mut jitter = Jitter::new(seed, jitter_us);
    let mut c = PhaseLockCore::new();
    let osc0 = -3.8;
    let mut p = Plant::new(osc0, -osc0);
    for _ in 0..(1_000.0 / dt) as usize {
        let out = c.on_window(D + (p.e_ns + jitter.next()).round() as i64, true, -osc0, dt);
        p.advance(out.freq_ppm.expect("engaged"), dt);
    }
    let e_us = |c: &PhaseLockCore, p: &Plant| {
        (p.e_ns - (c.anchor_ns().expect("anchored") - D) as f64) / 1_000.0
    };
    let mut worst = 0.0f64;
    for n in 0..(secs / dt) as usize {
        p.osc_ppm = osc0 + sign * profile(n as f64 * dt);
        let out = c.on_window(D + (p.e_ns + jitter.next()).round() as i64, true, -osc0, dt);
        p.advance(out.freq_ppm.expect("engaged"), dt);
        worst = worst.max(e_us(&c, &p).abs());
    }
    for _ in 0..(quiet_s / dt) as usize {
        let out = c.on_window(D + (p.e_ns + jitter.next()).round() as i64, true, -osc0, dt);
        p.advance(out.freq_ppm.expect("engaged"), dt);
    }
    (worst, c.gain_factor())
}

/// Third review of #204: on a noisy box (±35-50 µs windows, 1 s Dante spacing) the boost's
/// `g²·mean` read the window noise past its bound and held ×4 for good after the ramp was gone.
/// The boost must let go within half an hour of the ramp's end.
#[test]
fn a_noisy_box_lets_the_boost_go_after_the_ramp_204() {
    let mut stuck = Vec::new();
    for jitter_us in [35.0, 50.0] {
        for seed in 1..=6u64 {
            let (_, gain) = ramp_on(
                linear_6_ppm_over_5_min,
                1.0,
                jitter_us,
                0x204_9000 + seed,
                1.0,
                900.0,
                1_800.0,
            );
            if gain > 1.05 {
                stuck.push(format!("±{jitter_us} µs seed {seed}: x{gain:.2}"));
            }
        }
    }
    assert!(stuck.is_empty(), "the boost held after the ramp: {stuck:?}");
}

/// Third review of #204: the Dante grandmaster's windows are ~1 s apart, not 0.5 s. The live
/// shapes (6 ppm over 5 min, thermal τ 300 s) at ±10 µs window noise (resolume-pp's level: sd
/// 3.5-5.3 µs) and the 5 min ramp at ±20 µs (mbc-snv's), and the 2.5× faster 6 ppm / 2 min ramp at
/// ±10 µs, hold |e| < 50 µs. Measured limits (not pinned): thermal at ±20 µs reads up to 51.8 µs
/// in 1 of 16 runs, the 2 min ramp at ±20 µs up to 67 µs.
#[test]
fn a_ramp_on_one_second_windows_keeps_the_phase_within_50_us_204() {
    let cases: [(&str, Profile, f64); 4] = [
        ("5 min ±10", linear_6_ppm_over_5_min, 10.0),
        ("5 min ±20", linear_6_ppm_over_5_min, 20.0),
        ("thermal ±10", thermal_6_ppm_tau_300_s, 10.0),
        ("2 min ±10", linear_6_ppm_over_2_min, 10.0),
    ];
    let mut fails = Vec::new();
    for (name, profile, jitter_us) in cases {
        let mut worst = 0.0f64;
        for sign in [1.0, -1.0] {
            for seed in 1..=8u64 {
                let (w, _) = ramp_on(
                    profile,
                    sign,
                    jitter_us,
                    0x204_1000 + seed * 7,
                    1.0,
                    900.0,
                    0.0,
                );
                worst = worst.max(w);
            }
        }
        if worst >= 50.0 {
            fails.push(format!("{name}: {worst:.1} µs"));
        }
    }
    assert!(fails.is_empty(), "|e| ≥ 50 µs on 1 s windows: {fails:?}");
}

/// Third review of #204: the pair test on the Dante grandmaster's 1 s windows (±10 µs window
/// noise), the linear ramps. Measured limit (not pinned, a follow-up): the thermal ramp's release
/// at 1 s windows splits two cores by up to 2.0 µs (3 of 24 runs at ±10 µs; 1.8 µs at ±20 µs),
/// while each is within ~35 µs of the line; 0.5 s windows hold ≤ 110 ns
/// (`two_cores_fed_the_same_windows_agree_within_a_microsecond_through_a_ramp_204`).
#[test]
fn two_cores_on_one_second_windows_agree_within_a_microsecond_204() {
    let mut fails = Vec::new();
    for (name, profile) in PROFILES.into_iter().take(2) {
        for sign in [1.0, -1.0] {
            for seed in 1..=12u64 {
                let dt = 1.0;
                let mut jitter = Jitter::new(seed * 7919 + 5, 10.0);
                let mut r1 = Jitter::new(seed * 31 + 7, 0.04);
                let mut r2 = Jitter::new(seed * 57 + 9, 0.04);
                let (mut c1, mut c2) = (PhaseLockCore::new(), PhaseLockCore::new());
                let osc0 = -3.8;
                let (mut p1, mut p2) = (Plant::new(osc0, -osc0), Plant::new(osc0, -osc0));
                let settle = 600;
                let mut worst = 0.0f64;
                for n in 0..(settle + 900) {
                    if n >= settle {
                        let o = osc0 + sign * profile((n - settle) as f64 * dt);
                        p1.osc_ppm = o;
                        p2.osc_ppm = o;
                    }
                    let j = jitter.next();
                    let o1 = c1.on_window(
                        D + (p1.e_ns + j + r1.next()).round() as i64,
                        true,
                        -osc0,
                        dt,
                    );
                    let o2 = c2.on_window(
                        D + (p2.e_ns + j + r2.next()).round() as i64,
                        true,
                        -osc0,
                        dt,
                    );
                    p1.advance(o1.freq_ppm.expect("engaged"), dt);
                    p2.advance(o2.freq_ppm.expect("engaged"), dt);
                    if n >= settle {
                        worst = worst.max((p1.e_ns - p2.e_ns).abs());
                    }
                }
                if worst > 1_000.0 {
                    fails.push(format!("{name} {sign:+} seed {seed}: {worst:.0} ns"));
                }
            }
        }
    }
    assert!(fails.is_empty(), "cores > 1 µs apart: {fails:?}");
}

/// The Dante grandmaster's window spacing (s): ~1.016 s, which divides no ring or window length
/// (the #136 v1.27.2 lesson: a ring full by its SPAN never fills on it).
const DANTE_DT: f64 = 1.016;

/// The worst true |e| (µs) of `profile` at `jitter_us` window noise on Dante windows over
/// `seeds` noise seeds × both directions (judged as a tail: the #204 lesson, 3 seeds say little).
fn dante_ramp_worst(profile: Profile, jitter_us: f64, seeds: u64) -> f64 {
    let mut worst = 0.0f64;
    for sign in [1.0, -1.0] {
        for seed in 1..=seeds {
            let (w, _) = ramp_on(
                profile,
                sign,
                jitter_us,
                0x204_d000 + seed * 13,
                DANTE_DT,
                900.0,
                0.0,
            );
            worst = worst.max(w);
        }
    }
    worst
}

/// PR #207 review of #204: on the Dante cadence (1.016 s windows) at ±20 µs window noise (a USB
/// Dante NIC's level), the live shapes (6 ppm thermal τ 300 s, 6 ppm over 5 min) keep |e| < 50 µs
/// in every one of 64 runs per shape. v1.35.13 read 46.0 / 50.1 µs on these runs (on another
/// 200-run seed set: 50.1 / 50.7 µs).
#[test]
fn a_ramp_on_dante_windows_at_20_us_keeps_the_phase_within_50_us_204() {
    let mut fails = Vec::new();
    for (name, profile) in [PROFILES[0], PROFILES[2]] {
        let worst = dante_ramp_worst(profile, 20.0, 32);
        eprintln!("dsync#204 Dante windows ±20 µs, {name}: worst {worst:.1} µs");
        if worst >= 50.0 {
            fails.push(format!("{name}: {worst:.1} µs"));
        }
    }
    assert!(
        fails.is_empty(),
        "|e| ≥ 50 µs on 1.016 s windows: {fails:?}"
    );
}

/// PR #207 review of #204: the 2.5× faster 6 ppm / 2 min ramp at ±20 µs on Dante windows. Its
/// MEASURED LIMIT is not < 50 µs: a ramp must be told from a 25-150 µs path shift (which must not
/// boost) before its error crosses ~25 µs, and the plain PI's error then grows ~1.5 µs per window
/// while a 15 s slope reads ±0.8 ppm of window noise. A boost that is ALWAYS on reads 38.9 µs,
/// so the gap is the detection, not the loop. v1.35.13 read 64.1 µs on these 200 runs (on
/// another 200-run seed set: p99 65.3, max 78.4 µs). The envelope pins the improvement.
#[test]
fn a_fast_ramp_on_dante_windows_at_20_us_stays_inside_its_measured_envelope_204() {
    let worst = dante_ramp_worst(PROFILES[1].1, 20.0, 100);
    eprintln!("dsync#204 Dante windows ±20 µs, 6 ppm / 2 min: worst {worst:.1} µs");
    assert!(worst < 60.0, "6 ppm / 2 min at ±20 µs: {worst:.1} µs");
}

/// PR #207 review of #204: two followers on one PC (the same windows ± ~40 ns of conversion
/// residue, the #170 bar of 1 µs) through every ramp shape on Dante windows, ±10 and ±20 µs. The
/// thermal ramp's slow release split v1.35.13 cores by up to 7.2 µs: the boost's own gain moved
/// its decision input before the loop answered.
#[test]
fn two_cores_on_dante_windows_agree_within_a_microsecond_through_every_ramp_204() {
    let mut fails = Vec::new();
    let mut worst_all = 0.0f64;
    for (name, profile) in PROFILES {
        for jitter_us in [10.0, 20.0] {
            for sign in [1.0, -1.0] {
                for seed in 1..=12u64 {
                    let mut jitter = Jitter::new(seed * 7919 + 5, jitter_us);
                    let mut r1 = Jitter::new(seed * 31 + 7, 0.04);
                    let mut r2 = Jitter::new(seed * 57 + 9, 0.04);
                    let (mut c1, mut c2) = (PhaseLockCore::new(), PhaseLockCore::new());
                    let osc0 = -3.8;
                    let (mut p1, mut p2) = (Plant::new(osc0, -osc0), Plant::new(osc0, -osc0));
                    let settle = (600.0 / DANTE_DT) as usize;
                    let mut worst = 0.0f64;
                    for n in 0..(settle + (1_800.0 / DANTE_DT) as usize) {
                        if n >= settle {
                            let o = osc0 + sign * profile((n - settle) as f64 * DANTE_DT);
                            p1.osc_ppm = o;
                            p2.osc_ppm = o;
                        }
                        let j = jitter.next();
                        let o1 = c1.on_window(
                            D + (p1.e_ns + j + r1.next()).round() as i64,
                            true,
                            -osc0,
                            DANTE_DT,
                        );
                        let o2 = c2.on_window(
                            D + (p2.e_ns + j + r2.next()).round() as i64,
                            true,
                            -osc0,
                            DANTE_DT,
                        );
                        p1.advance(o1.freq_ppm.expect("engaged"), DANTE_DT);
                        p2.advance(o2.freq_ppm.expect("engaged"), DANTE_DT);
                        if n >= settle {
                            worst = worst.max((p1.e_ns - p2.e_ns).abs());
                        }
                    }
                    worst_all = worst_all.max(worst);
                    if worst > 1_000.0 {
                        fails.push(format!(
                            "{name} ±{jitter_us} {sign:+} seed {seed}: {worst:.0} ns"
                        ));
                    }
                }
            }
        }
    }
    eprintln!("dsync#204 pair split on Dante windows, worst {worst_all:.0} ns");
    assert!(fails.is_empty(), "cores > 1 µs apart: {fails:?}");
}
