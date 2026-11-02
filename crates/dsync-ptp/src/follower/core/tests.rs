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
        self.t += SYNC_PERIOD_NS;
        self.seq = self.seq.wrapping_add(1);
        let t1 = self.gm_time(self.t);
        let sync = ptp_tx::sync(&self.gm, self.seq, t1);
        let follow_up = ptp_tx::follow_up(&self.gm, self.seq, t1);
        for core in cores.iter_mut() {
            let rx = self.t + LATENCY_NS + (self.jitter_ns * self.noise.gauss()).abs() as i64;
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
            self.t += SYNC_PERIOD_NS;
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

#[test]
fn a_cold_follower_locks_and_holds_the_grandmaster_line_within_the_lock_bound() {
    let mut world = World::new(0x5eed_0001, 5_000.0);
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

/// The acceptance shape on one PC, in the model: two followers, each with its own receive
/// jitter, agree with each other within the lock bound (the real-socket version beside the
/// service is the root crate's `tests/library_acceptance.rs`).
#[test]
fn two_followers_on_one_pc_agree_within_the_lock_bound() {
    let mut world = World::new(0x5eed_0002, 10_000.0);
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
    assert!(worst <= LOCK_BOUND_NS, "followers {}ns apart", worst);
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
        2,
        "the packet itself and the pending Sync"
    );
    assert!(core.window.is_empty(), "nothing paired");
}
