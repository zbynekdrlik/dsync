//! The follower's pipeline in a closed-loop world (`.claude/rules/clock-discipline-and-testing.md`):
//! a grandmaster and the PC's raw counter each run at their own rate against true time, every
//! Sync reaches the follower after a seeded, jittered latency, and the follower's virtual clock is
//! judged against the grandmaster line it should trail by that latency.

use super::*;
use crate::ptp_tx::{self, Sender};

const SYNC_PERIOD_NS: i64 = 125_000_000;
const LATENCY_NS: i64 = 40_000;
const LOCK_BOUND_NS: i64 = 50_000;
/// The world's mean extra latency (the |gauss| jitter's mean, ≤ 0.8 × 10 µs here): the line the
/// follower trails is the MEAN latency, so a judge against the nominal latency allows it.
const JITTER_MEAN_ALLOWANCE_NS: i64 = 10_000;
/// Settled: well past the boot pull (τ 20 s) that runs on after the lock.
const SETTLED_BOUND_NS: i64 = 20_000;

/// xorshift64* + Box-Muller, a fixed seed per run.
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

const GM_A: Sender = Sender {
    source_uuid: [0, 0x1d, 0xc1, 0x0a, 0x0a, 0x0a],
    grandmaster_uuid: [0, 0x1d, 0xc1, 0x0a, 0x0a, 0x0a],
};
const GM_B: Sender = Sender {
    source_uuid: [0, 0x1d, 0xc1, 0x0b, 0x0b, 0x0b],
    grandmaster_uuid: [0, 0x1d, 0xc1, 0x0b, 0x0b, 0x0b],
};

struct World {
    /// True time (ns since the world began).
    t: i64,
    gm: Sender,
    gm_base: i64,
    gm_ppm: f64,
    pc_ppm: f64,
    mono_base: i64,
    wall_minus_mono: i64,
    jitter_ns: f64,
    noise: Noise,
    seq: u16,
    source_ip: Ipv4Addr,
    sync_period_ns: i64,
    /// Extra receive delay of the Syncs whose sequence id is in the set (late windows).
    late: std::collections::HashSet<u16>,
    late_ns: i64,
}

impl World {
    fn new(seed: u64, jitter_ns: f64) -> Self {
        World {
            t: 0,
            gm: GM_A,
            gm_base: 3_600_000_000_000, // the grandmaster has been up an hour
            gm_ppm: 3.0,
            pc_ppm: -21.0,
            mono_base: 777_000_000_000,
            wall_minus_mono: 1_789_000_000_000_000_000,
            jitter_ns,
            noise: Noise(seed),
            seq: 0,
            source_ip: Ipv4Addr::new(10, 77, 9, 184),
            sync_period_ns: SYNC_PERIOD_NS,
            late: std::collections::HashSet::new(),
            late_ns: 0,
        }
    }

    fn gm_time(&self, t: i64) -> i64 {
        self.gm_base + t + (t as f64 * self.gm_ppm * 1e-6) as i64
    }

    fn mono(&self, t: i64) -> i64 {
        self.mono_base + t + (t as f64 * self.pc_ppm * 1e-6) as i64
    }

    /// One Sync period: the Sync and its Follow_Up reach `core` (the same packets, each core with
    /// its own latency jitter, when there are several).
    fn tick(&mut self, cores: &mut [&mut FollowerCore]) {
        self.t += self.sync_period_ns;
        self.seq = self.seq.wrapping_add(1);
        let t1 = self.gm_time(self.t);
        let sync = ptp_tx::sync(&self.gm, self.seq, t1);
        let follow_up = ptp_tx::follow_up(&self.gm, self.seq, t1);
        for core in cores.iter_mut() {
            let late = if self.late.contains(&self.seq) {
                self.late_ns
            } else {
                0
            };
            let rx =
                self.t + LATENCY_NS + late + (self.jitter_ns * self.noise.gauss()).abs() as i64;
            let mono_rx = self.mono(rx);
            let wall_rx = mono_rx + self.wall_minus_mono;
            let packet = |data| RxPacket {
                data,
                mono_rx_ns: mono_rx,
                wall_rx_ns: wall_rx,
                source_ip: Some(self.source_ip),
            };
            assert_eq!(core.on_packet(packet(&sync)), PacketVerdict::TimeMessage);
            assert_eq!(
                core.on_packet(packet(&follow_up)),
                PacketVerdict::TimeMessage
            );
            core.on_tick(mono_rx + 1_000_000);
        }
    }

    /// Time passes with no packet.
    fn silence(&mut self, ns: i64, cores: &mut [&mut FollowerCore]) {
        let end = self.t + ns;
        while self.t < end {
            self.t += self.sync_period_ns;
            for core in cores.iter_mut() {
                core.on_tick(self.mono(self.t));
            }
        }
    }

    /// The follower's published PTP time minus the grandmaster line it should trail by the
    /// latency, now.
    fn error_ns(&self, core: &FollowerCore) -> Option<i64> {
        let m = core.model();
        m.ptp_at(self.mono(self.t))
            .map(|ptp| ptp - self.gm_time(self.t - LATENCY_NS))
    }
}

fn new_core(world: &World) -> FollowerCore {
    FollowerCore::new(GmAllowlist::default(), world.mono(0))
}

/// Run until the core reports a lock (at most `max_s`), returning the lock's time (s).
fn run_to_lock(world: &mut World, core: &mut FollowerCore, max_s: i64) -> f64 {
    while !core.model().locked {
        assert!(
            world.t < max_s * 1_000_000_000,
            "no lock within {}s (rate lock {}, engaged {}, e {:?})",
            max_s,
            core.rate_locked(),
            core.engaged(),
            core.model().offset_ns
        );
        world.tick(&mut [core]);
    }
    world.t as f64 / 1e9
}

/// A statistic of seeded noise: run several seeds and judge the worst
/// (`.claude/rules/clock-discipline-and-testing.md`).
const SEEDS: [u64; 4] = [0x5eed_0001, 0x5eed_0011, 0x5eed_0021, 0x5eed_0031];

#[test]
fn a_cold_follower_locks_and_holds_the_grandmaster_line_within_the_lock_bound() {
    for seed in SEEDS {
        cold_lock_run(seed);
    }
}

fn cold_lock_run(seed: u64) {
    let mut world = World::new(seed, 5_000.0);
    let mut core = new_core(&world);
    assert_eq!(
        core.model().ptp_at(world.mono(0)),
        None,
        "no time before the anchor"
    );
    let locked_at = run_to_lock(&mut world, &mut core, 300);
    assert!(
        locked_at > 5.0,
        "a lock needs the acquisition: {}",
        locked_at
    );
    // [41]'s definition: set at |e| <= 50 us; the boot pull keeps retiring the rest after it.
    let (mut worst, mut worst_settled) = (0, 0);
    for i in 0..(180 * 8) {
        world.tick(&mut [&mut core]);
        let m = core.model();
        assert!(m.locked, "the lock flapped at {}s", world.t as f64 / 1e9);
        let e = world.error_ns(&core).expect("anchored").abs();
        worst = worst.max(e);
        if i >= 60 * 8 {
            worst_settled = worst_settled.max(e);
        }
    }
    assert!(
        worst <= LOCK_BOUND_NS + JITTER_MEAN_ALLOWANCE_NS,
        "worst error while locked {}ns",
        worst
    );
    assert!(
        worst_settled <= SETTLED_BOUND_NS,
        "settled error {}ns",
        worst_settled
    );
    let m = core.model();
    assert!(
        (m.rate_ppm - (3.0 + 21.0)).abs() < 0.5,
        "the virtual clock runs the grandmaster's rate against the raw counter: {}",
        m.rate_ppm
    );
    assert_eq!(m.gm_uuid, Some(GM_A.grandmaster_uuid));
    assert_eq!((m.epoch, m.reanchors, m.holdovers), (0, 0, 0));
    assert!(m.windows > 100);
    // wall = PTP + D on this PC's wall clock.
    let d = m.date_offset_ns.expect("date offset");
    let wall_now = world.mono(world.t) + world.wall_minus_mono;
    let ptp_now = m.ptp_at(world.mono(world.t)).unwrap();
    assert!(
        (wall_now - ptp_now - d).abs() < 100_000,
        "D off by {}",
        wall_now - ptp_now - d
    );
}

/// dsync#136, the dstack probe on mbc-snv (3.11.2026): the Windows follower engaged on the rate
/// servo's word ~2.4 ppm off, and its boot fit, measuring the right frequency at σ 0.3-0.5 ppm,
/// never re-seeded: the Dante grandmaster's 4 Syncs a second made a window every ~1.016 s of
/// grandmaster time, which a 20 s ring never spans exactly. The PI then walked `e` to +190 µs
/// (lost [41]) or never reached it. Here: 254 ms Syncs, a window median scattering by ~15 µs.
#[test]
fn a_follower_whose_windows_do_not_divide_the_fit_window_locks_and_holds_136() {
    for seed in SEEDS {
        let mut world = World::new(seed, 35_000.0);
        world.sync_period_ns = 254_000_000;
        // mbc-snv: the counter 5.4 ppm fast of the grandmaster, so the rate servo locks within a
        // few windows of the anchor and the first engagement continues its fit.
        world.gm_ppm = 0.0;
        world.pc_ppm = 5.4;
        let mut core = new_core(&world);
        let locked_at = run_to_lock(&mut world, &mut core, 120);
        let mut worst = 0;
        for i in 0..(240 * 4) {
            world.tick(&mut [&mut core]);
            assert!(
                core.model().locked,
                "seed {seed:#x}: locked at {locked_at}s, lost at {}s (e {:?})",
                world.t as f64 / 1e9,
                core.model().offset_ns
            );
            if i >= 120 * 4 {
                worst = worst.max(world.error_ns(&core).expect("anchored").abs());
            }
        }
        assert!(
            core.core.boot_reseed().is_some() || core.core.engaged_on_fit().is_some(),
            "seed {seed:#x}: the first engagement's frequency came from no fit"
        );
        // The line the follower trails is the MEAN latency: |gauss| × 35 µs adds √(2/π) × 35 µs
        // ≈ 27.9 µs to the nominal one this judge measures against.
        let mean_jitter_ns = (35_000.0 * (2.0 / std::f64::consts::PI).sqrt()) as i64;
        assert!(
            worst <= LOCK_BOUND_NS + mean_jitter_ns,
            "seed {seed:#x}: settled error {worst}ns"
        );
    }
}

/// dsync#136, the mbc-snv probe: the Dante NIC there is a USB adapter, and now and then 3 of a
/// window's 4 Syncs arrive ~0.5 ms late. That window's median `e` reads +455…+504 µs, and the PI's
/// proportional term turned each into an 8-10 µs move of the follower's line (the probe's
/// diff stepped −19 → −28 → −35 → −45 µs). A lone late window says nothing about the clock: the
/// line must not move for it.
#[test]
fn a_lone_late_window_does_not_move_the_followers_line_136() {
    for seed in SEEDS {
        let run = |late: bool| {
            let mut world = World::new(seed, 10_000.0);
            world.sync_period_ns = 250_000_000;
            world.gm_ppm = 0.0;
            world.pc_ppm = 5.4;
            world.late_ns = 500_000;
            let mut core = new_core(&world);
            run_to_lock(&mut world, &mut core, 120);
            for _ in 0..(120 * 4) {
                world.tick(&mut [&mut core]);
            }
            if late {
                // Three late windows a few seconds apart, as at 69 / 72 / 79 s on mbc: 3 of the
                // 4 Syncs of each.
                for start in [0u16, 12, 40] {
                    for k in 1..=3 {
                        world.late.insert(world.seq.wrapping_add(start + k));
                    }
                }
            }
            let mut errors = Vec::new();
            for _ in 0..(60 * 4) {
                world.tick(&mut [&mut core]);
                errors.push(world.error_ns(&core).expect("anchored"));
                assert!(core.model().locked, "seed {seed:#x}: the lock dropped");
            }
            errors
        };
        let (clean, late) = (run(false), run(true));
        let worst = clean
            .iter()
            .zip(&late)
            .map(|(a, b)| (a - b).abs())
            .max()
            .unwrap();
        assert!(
            worst <= 3_000,
            "seed {seed:#x}: the late windows moved the line by {worst}ns"
        );
    }
}

/// mbc-snv's world (dsync#136): 254 ms Syncs, the counter 5.4 ppm fast of the grandmaster,
/// Windows-level receive jitter (25 µs per sample: a window `e` sd of ~11 µs, mbc read ~14).
fn mbc_world(seed: u64) -> World {
    let mut world = World::new(seed, 25_000.0);
    world.sync_period_ns = 254_000_000;
    world.gm_ppm = 0.0;
    world.pc_ppm = 5.4;
    world
}

/// The line the follower trails is the MEAN latency: |gauss| × 25 µs adds √(2/π) × 25 µs.
fn mbc_mean_jitter_ns() -> i64 {
    (25_000.0 * (2.0 / std::f64::consts::PI).sqrt()) as i64
}

/// The worst |error| (ns) over `secs` of `world` once `core` is locked, and when it locked (s).
fn worst_after_lock(world: &mut World, core: &mut FollowerCore, secs: i64) -> (i64, f64) {
    let locked_at = run_to_lock(world, core, 120);
    let mut worst = 0;
    let end = world.t + secs * 1_000_000_000;
    while world.t < end {
        world.tick(&mut [&mut *core]);
        worst = worst.max(world.error_ns(core).expect("anchored").abs());
    }
    (worst, locked_at)
}

/// dsync#136 (dstack, 3.11.2026): the boot must not be worse than the service's, which starts
/// from `drift.json`. Without a seed the 20 s fit hands over ~0.5 ppm off and the follower sat
/// +20…40 µs off the service for ~5 minutes on mbc. The app hands in the service's word (DSYX
/// [36-39]); here 0.3 ppm off the truth, about its P term.
#[test]
fn a_seeded_follower_holds_the_line_from_its_lock_on_136() {
    let truth_ppm = -5.4;
    for seed in SEEDS {
        let mut world = mbc_world(seed);
        let mut core = new_core(&world).with_seed(truth_ppm + 0.3);
        // From the first engagement on, the learned frequency is within 0.5 ppm of the truth:
        // the hand-over the boot fit could only get to ~0.5 ppm on mbc after 20 s.
        while !core.engaged() {
            world.tick(&mut [&mut core]);
        }
        let mut first_lock = None;
        for _ in 0..(60 * 4) {
            if core.model().locked && first_lock.is_none() {
                first_lock = Some(world.t as f64 / 1e9);
            }
            let learned = core.core.integrator_ppm();
            assert!(
                (learned - truth_ppm).abs() <= 0.5,
                "seed {seed:#x}: learned {learned} at {}s",
                world.t as f64 / 1e9
            );
            world.tick(&mut [&mut core]);
        }
        let (worst, _) = worst_after_lock(&mut world, &mut core, 300);
        let locked_at = first_lock.expect("locked within the first minute of engagement");
        assert!(
            locked_at < 30.0,
            "seed {seed:#x}: locked only at {locked_at}s"
        );
        assert!(
            worst <= SETTLED_P90_NS + mbc_mean_jitter_ns(),
            "seed {seed:#x}: worst {worst}ns after the lock"
        );
        assert!(core.model().settled, "seed {seed:#x}: not settled");
    }
}

/// A seed from another oscillator (40 ppm off) is dropped by the judge, and the follower learns
/// as a cold start does.
#[test]
fn a_wrong_seed_is_dropped_and_the_follower_learns_as_a_cold_start_136() {
    for seed in SEEDS {
        let mut world = mbc_world(seed);
        let mut core = new_core(&world).with_seed(-5.4 + 40.0);
        let (worst, _) = worst_after_lock(&mut world, &mut core, 300);
        assert!(core.seed_judge.is_none(), "judged");
        assert_eq!(core.rate.lock_stable_count > 0, true);
        assert!(
            worst <= LOCK_BOUND_NS + mbc_mean_jitter_ns(),
            "seed {seed:#x}: worst {worst}ns after the lock"
        );
        assert!(
            (core.model().rate_ppm + 5.4).abs() < 0.5,
            "seed {seed:#x}: learned {}",
            core.model().rate_ppm
        );
    }
}

/// dsync#136 (dstack): without a seed the boot must not rely on one 20 s fit (σ ~0.5 ppm under
/// Windows noise) and the ~100 s PI for the rest: the longer refinement brings the learned
/// frequency within 0.2 ppm of the truth ~90 s after the engagement, and keeps it there.
#[test]
fn without_a_seed_the_refinement_brings_the_frequency_within_a_fifth_ppm_136() {
    let truth_ppm = -5.4;
    let mut worst_ppm: f64 = 0.0;
    for seed in 0..8u64 {
        let mut world = mbc_world(0x0136_0000 + seed);
        let mut core = new_core(&world);
        while !core.engaged() {
            world.tick(&mut [&mut core]);
        }
        let engaged_at = world.t;
        while world.t < engaged_at + 90_000_000_000 {
            world.tick(&mut [&mut core]);
        }
        for _ in 0..(120 * 4) {
            world.tick(&mut [&mut core]);
            worst_ppm = worst_ppm.max((core.core.integrator_ppm() - truth_ppm).abs());
        }
        assert!(!core.core.refining(), "seed {seed}: the refinement closed");
    }
    assert!(
        worst_ppm <= 0.2,
        "the learned frequency strayed {worst_ppm:.3} ppm from the truth 90-210 s after engaging"
    );
}

/// `settled` is the lock AND the recent |e| p90 within 30 µs AND a pull's rest within it; a holdover clears
/// it with the lock, and the re-join must earn it again over 30 windows.
#[test]
fn settled_needs_thirty_quiet_windows_and_a_holdover_clears_it_136() {
    let mut world = mbc_world(SEEDS[0]);
    let mut core = new_core(&world).with_seed(-5.4);
    run_to_lock(&mut world, &mut core, 120);
    for _ in 0..(120 * 4) {
        world.tick(&mut [&mut core]);
    }
    let m = core.model();
    assert!(m.settled, "{m:?}");
    let p90 = m.recent_abs_e_ns.expect("30 windows");
    assert!(p90 <= SETTLED_P90_NS, "{p90}");
    world.silence(12_000_000_000, &mut [&mut core]);
    let m = core.model();
    assert!(!m.settled && !m.locked && m.holdover, "{m:?}");
    assert_eq!(m.recent_abs_e_ns, None, "the measurement was dropped");
    // Back: 30 windows (~30 s at 1 s windows) before it can be settled again.
    for _ in 0..(20 * 4) {
        world.tick(&mut [&mut core]);
    }
    assert!(!core.model().settled, "settled after only ~20 windows");
    for _ in 0..(60 * 4) {
        world.tick(&mut [&mut core]);
    }
    assert!(core.model().settled, "{:?}", core.model());
}

/// The acceptance shape on one PC, in the model: two followers, each with its own receive
/// jitter, agree with each other within the lock bound (the real-socket version beside the
/// service is the root crate's `tests/library_acceptance.rs`).
#[test]
fn two_followers_on_one_pc_agree_within_the_lock_bound() {
    for seed in SEEDS {
        two_followers_run(seed ^ 0x2);
    }
}

fn two_followers_run(seed: u64) {
    let mut world = World::new(seed, 10_000.0);
    let mut a = new_core(&world);
    let mut b = new_core(&world);
    while !(a.model().locked && b.model().locked) {
        assert!(world.t < 300_000_000_000, "no lock within 300 s");
        world.tick(&mut [&mut a, &mut b]);
    }
    let mut worst = 0;
    for _ in 0..(60 * 8) {
        world.tick(&mut [&mut a, &mut b]);
        let m = world.mono(world.t);
        let (pa, pb) = (a.model().ptp_at(m).unwrap(), b.model().ptp_at(m).unwrap());
        worst = worst.max((pa - pb).abs());
    }
    assert!(
        worst <= LOCK_BOUND_NS,
        "seed {:#x}: followers {}ns apart",
        seed,
        worst
    );
}

/// The service steps the wall (a nightly date step): the receive stamps move with it, but the
/// follower's timeline is on the raw counter. Only `D` follows the wall.
#[test]
fn a_wall_step_moves_only_the_date_offset_never_the_ptp_timeline() {
    let mut world = World::new(0x5eed_0003, 5_000.0);
    let mut core = new_core(&world);
    run_to_lock(&mut world, &mut core, 300);
    for _ in 0..(90 * 8) {
        world.tick(&mut [&mut core]);
    }
    let d_before = core.model().date_offset_ns.unwrap();
    world.wall_minus_mono += 400_000_000; // +400 ms
    core.on_wall_step();
    for _ in 0..(30 * 8) {
        world.tick(&mut [&mut core]);
        assert!(core.model().locked);
        assert!(world.error_ns(&core).unwrap().abs() <= SETTLED_BOUND_NS);
    }
    // D follows the wall: wall = PTP + D still holds on the stepped wall.
    let m = core.model();
    let d_after = m.date_offset_ns.unwrap();
    let wall_now = world.mono(world.t) + world.wall_minus_mono;
    let ptp_now = m.ptp_at(world.mono(world.t)).unwrap();
    assert!((wall_now - ptp_now - d_after).abs() < 100_000);
    assert!(d_after - d_before > 399_000_000, "D moved by the step");
    assert_eq!(core.model().reanchors, 0);
}

#[test]
fn a_grandmaster_change_is_a_new_epoch_and_the_follower_relocks_on_the_new_line() {
    let mut world = World::new(0x5eed_0004, 5_000.0);
    let mut core = new_core(&world);
    run_to_lock(&mut world, &mut core, 300);
    // Grandmaster B: another device, days of other uptime, another rate.
    world.gm = GM_B;
    world.gm_base = world.gm_base + 4 * 86_400_000_000_000 + 123_456_789;
    world.gm_ppm = -7.0;
    world.tick(&mut [&mut core]);
    let mut relocked = false;
    for _ in 0..(300 * 8) {
        world.tick(&mut [&mut core]);
        if core.model().epoch == 1 && core.model().locked {
            relocked = true;
            break;
        }
    }
    assert!(relocked, "never relocked on B: {:?}", core.model());
    let m = core.model();
    assert_eq!(m.gm_uuid, Some(GM_B.grandmaster_uuid));
    assert_eq!(m.epoch, 1);
    assert!(m.reanchors >= 1);
    // B runs 10 ppm slower than A: the follower follows the frequency step (it may drop [41]
    // once, as the service does for a grandmaster frequency step) and settles on B's line.
    for _ in 0..(240 * 8) {
        world.tick(&mut [&mut core]);
    }
    assert!(core.model().locked);
    assert!(world.error_ns(&core).unwrap().abs() <= SETTLED_BOUND_NS);
}

#[test]
fn a_ptp_outage_is_a_holdover_on_the_learned_frequency_and_the_lock_returns() {
    let mut world = World::new(0x5eed_0005, 5_000.0);
    let mut core = new_core(&world);
    run_to_lock(&mut world, &mut core, 300);
    for _ in 0..(120 * 8) {
        world.tick(&mut [&mut core]);
    }
    world.silence(30_000_000_000, &mut [&mut core]);
    let m = core.model();
    assert!(m.holdover && !m.locked, "{:?}", m);
    assert_eq!(m.holdovers, 1);
    // 30 s on the learned frequency: the timeline stays near the line (no PI term running away).
    let drift = world.error_ns(&core).unwrap().abs();
    assert!(drift < 100_000, "free-ran {}ns in 30 s", drift);
    let mut back = false;
    for _ in 0..(60 * 8) {
        world.tick(&mut [&mut core]);
        if core.model().locked {
            back = true;
            break;
        }
    }
    assert!(back, "the lock did not return: {:?}", core.model());
    assert!(!core.model().holdover);
}

#[test]
fn a_source_the_allowlist_refuses_is_as_if_it_never_arrived() {
    let world = World::new(1, 0.0);
    let mut core = FollowerCore::new(GmAllowlist::parse(&["10.77.7.0/24".into()]), 0);
    let sync = ptp_tx::sync(&GM_A, 1, world.gm_time(0));
    let foreign = RxPacket {
        data: &sync,
        mono_rx_ns: 1,
        wall_rx_ns: 1,
        source_ip: Some(world.source_ip),
    };
    assert_eq!(core.on_packet(foreign), PacketVerdict::NotAllowed);
    let runt = RxPacket {
        data: &sync[..20],
        source_ip: Some(Ipv4Addr::new(10, 77, 7, 9)),
        ..foreign
    };
    assert_eq!(core.on_packet(runt), PacketVerdict::Other);
    assert_eq!(core.model().gm_uuid, None);
}

/// A Sync whose Follow_Up arrives after a wall step is not paired: its stamp may be on either
/// side of the step.
#[test]
fn a_sync_pending_across_a_wall_step_is_dropped() {
    let world = World::new(1, 0.0);
    let mut core = new_core(&world);
    let sync = ptp_tx::sync(&GM_A, 9, world.gm_time(0));
    let follow_up = ptp_tx::follow_up(&GM_A, 9, world.gm_time(0));
    let p = |data| RxPacket {
        data,
        mono_rx_ns: 5,
        wall_rx_ns: 5,
        source_ip: None,
    };
    core.on_packet(p(&sync));
    core.on_wall_step();
    core.on_packet(p(&follow_up));
    assert_eq!(
        core.straddled(),
        1,
        "the pending Sync (the receive pass drops its own packets)"
    );
    assert!(core.window.is_empty(), "nothing paired");
}
