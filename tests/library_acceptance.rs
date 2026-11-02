//! dsync#136 — the acceptance test of the library form (dstack's wish): the dsync service and
//! two in-process library followers on ONE PC agree on PTP time within the lock bound (50 µs).
//!
//! Real sockets, real kernel receive stamps, the real code on both sides:
//!
//! - a fake grandmaster thread multicasts real PTPv1 Sync / Follow_Up (the encoding the fixtures
//!   prove, `dsync_ptp::ptp_tx`) over loopback, 8 per second, its time base this host's raw counter
//!   at +20 ppm plus an hour of uptime;
//! - three receivers share ONE port pair with `SO_REUSEADDR` (the production
//!   `net::create_multicast_socket`), so the kernel hands each of them every message;
//! - two are library [`Follower`]s; the third is the service's own [`PtpController`], driven
//!   through `process_loop_iteration` (the real packet path, servo, phase lock and [41] gate).
//!
//! What a test cannot do is steer this host's clock, and it must not: the service's
//! `SystemClock` here is a virtual WALL over the raw counter (it obeys `adjust_frequency` and
//! `step_clock` exactly as the platform clock would), and its receive path converts each kernel
//! stamp onto that wall. 319/320 need root, so the pair is unprivileged (319 → base, 320 → base+1).
//!
//! The service's PTP time is `wall − D` (its `/status.date_offset_ns`), the followers' is
//! `PtpClock::ptp_at_mono_ns`; all three are read at the same raw-counter instants.

#![cfg(target_os = "linux")]

use anyhow::{anyhow, Result};
use dsync::clock::SystemClock;
use dsync::config::SystemConfig;
use dsync::controller::PtpController;
use dsync::ntp::NtpMeasurement;
use dsync::status::SyncStatus;
use dsync::traits::{NtpSource, PtpNetwork, RejoinOutcome};
use dsync_ptp::follower::model::VirtualClock;
use dsync_ptp::follower::{Follower, FollowerConfig};
use dsync_ptp::mono;
use dsync_ptp::net::{self, PinnedLookup};
use dsync_ptp::net_linux::{PtpSocketFactory, UdpPtpNetwork};
use dsync_ptp::ptp_tx::{self, Sender};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;
const LOCK_BOUND_NS: i64 = 50_000;
const UNLOCK_BOUND_NS: i64 = 150_000;
/// A cold service locks in ~60-80 s ([41] after the boot pull, #146); the follower is the same law.
const LOCK_DEADLINE: Duration = Duration::from_secs(240);
/// How long the three are compared once all report a lock.
const COMPARE_FOR: Duration = Duration::from_secs(30);
/// The grandmaster's time base: an hour of uptime, +20 ppm against this host's raw counter.
const GM_UPTIME_NS: i64 = 3_600_000_000_000;
const GM_PPM_DIV: i64 = 50_000; // 1 / 20 ppm

/// The production sockets on an unprivileged pair.
struct PortPair {
    base: u16,
}

impl PtpSocketFactory for PortPair {
    fn resolve_interface(&mut self) -> Result<(String, Ipv4Addr)> {
        Ok(("lo".into(), LOOPBACK))
    }
    fn interface_carrying(&mut self, _ip: Ipv4Addr) -> Option<String> {
        Some("lo".into())
    }
    fn open(&mut self, port: u16, ip: Ipv4Addr) -> Result<UdpSocket> {
        let mapped = if port == dsync_ptp::ptp::PTP_EVENT_PORT {
            self.base
        } else {
            self.base + 1
        };
        net::create_multicast_socket(mapped, ip)
    }
    fn lookup_pinned(&mut self, _name: &str) -> PinnedLookup {
        PinnedLookup::Ready(LOOPBACK)
    }
}

fn join(base: u16) -> UdpPtpNetwork<PortPair> {
    UdpPtpNetwork::join(PortPair { base }, "lo".into(), LOOPBACK).expect("join the group")
}

/// The service's wall: a virtual clock over the raw counter plus every step it took.
struct Wall {
    clock: VirtualClock,
    stepped_ns: i64,
    origin_ns: i64,
}

impl Wall {
    fn at(&self, mono_ns: i64) -> i64 {
        self.origin_ns + self.clock.at(mono_ns) + self.stepped_ns
    }
}

#[derive(Clone)]
struct WallClock(Arc<Mutex<Wall>>);

impl SystemClock for WallClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let mut w = self.0.lock().unwrap();
        w.clock
            .set_rate(mono::mono_now_ns(), (factor - 1.0) * 1_000_000.0);
        Ok(())
    }
    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        self.0.lock().unwrap().stepped_ns += offset.as_nanos() as i64 * sign as i64;
        Ok(())
    }
}

/// The service's receive path: the real sockets, each kernel stamp moved onto the service's wall
/// (kernel realtime → raw counter → the virtual wall).
struct ServiceNet {
    inner: UdpPtpNetwork<PortPair>,
    wall: WallClock,
}

impl PtpNetwork for ServiceNet {
    #[allow(clippy::type_complexity)]
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime, Option<Ipv4Addr>)>> {
        let Some((data, size, ts, ip)) = self.inner.recv_packet()? else {
            return Ok(None);
        };
        let reading = mono::read_time_base();
        let real_ns = ts.duration_since(UNIX_EPOCH)?.as_nanos() as i64;
        let wall_ns = self.wall.0.lock().unwrap().at(reading.to_mono(real_ns));
        Ok(Some((
            data,
            size,
            UNIX_EPOCH + Duration::from_nanos(wall_ns as u64),
            ip,
        )))
    }
    fn rejoin(&mut self) -> Result<RejoinOutcome> {
        self.inner.rejoin()
    }
}

/// No NTP in this world: the service's date path finds no reading and never steps.
struct NoNtp;
impl NtpSource for NoNtp {
    fn get_offset(&self) -> Result<NtpMeasurement> {
        Err(anyhow!("no NTP in the acceptance test"))
    }
}

fn grandmaster(base: u16, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let sock = UdpSocket::bind(SocketAddrV4::new(LOOPBACK, 0)).expect("sender");
        sock.set_multicast_loop_v4(true).expect("loop");
        sock.set_multicast_ttl_v4(0)
            .expect("ttl 0: never leaves the host");
        socket2::SockRef::from(&sock)
            .set_multicast_if_v4(&LOOPBACK)
            .expect("multicast if");
        let group = Ipv4Addr::new(224, 0, 1, 129);
        let sender = Sender {
            source_uuid: [0, 0x1d, 0xc1, 0x13, 0x60, 0x01],
            grandmaster_uuid: [0, 0x1d, 0xc1, 0x13, 0x60, 0x01],
        };
        let mut seq = 0u16;
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            seq = seq.wrapping_add(1);
            let m = mono::mono_now_ns();
            let t1 = GM_UPTIME_NS + m + m / GM_PPM_DIV;
            sock.send_to(&ptp_tx::sync(&sender, seq, t1), (group, base))
                .expect("send Sync");
            sock.send_to(&ptp_tx::follow_up(&sender, seq, t1), (group, base + 1))
                .expect("send Follow_Up");
            next += Duration::from_millis(125);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    })
}

#[test]
fn the_service_and_two_library_followers_on_one_pc_agree_within_the_lock_bound_136() {
    // Below Linux's ephemeral range (32768-60999), and apart from the crate test's 20000-29999.
    let base = 30_000 + (std::process::id() % 1_000) as u16 * 2;
    let stop = Arc::new(AtomicBool::new(false));

    // The two library followers.
    let f1 = Follower::with_network(join(base), &FollowerConfig::on_interface("lo")).unwrap();
    let f2 = Follower::with_network(join(base), &FollowerConfig::on_interface("lo")).unwrap();
    let (c1, c2) = (f1.clock(), f2.clock());

    // The service, on its own thread like `sync_loop::run` (1 ms loop).
    let wall = WallClock(Arc::new(Mutex::new(Wall {
        clock: VirtualClock::new(),
        stepped_ns: 0,
        origin_ns: mono::wall_now_ns() - mono::mono_now_ns(),
    })));
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    let service = {
        let (wall, status, stop) = (wall.clone(), Arc::clone(&status), Arc::clone(&stop));
        std::thread::spawn(move || {
            let net = ServiceNet {
                inner: join(base),
                wall: wall.clone(),
            };
            let mut ctl = PtpController::new(wall, net, NoNtp, status, SystemConfig::default());
            while !stop.load(Ordering::Relaxed) {
                ctl.process_loop_iteration().expect("a loop iteration");
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };

    let gm = grandmaster(base, Arc::clone(&stop));
    let svc = |m: i64| -> Option<(bool, i64)> {
        let st = status.read().unwrap();
        let d = st.date_offset_ns?;
        Some((st.is_locked, wall.0.lock().unwrap().at(m) - d))
    };

    let started = Instant::now();
    let all_locked = loop {
        let m = mono::mono_now_ns();
        let locked = svc(m).is_some_and(|(l, _)| l) && c1.state().locked && c2.state().locked;
        if locked {
            break started.elapsed();
        }
        if started.elapsed() > LOCK_DEADLINE {
            stop.store(true, Ordering::Relaxed);
            panic!(
                "not all locked within {:?}: service {:?}, follower 1 {:?}, follower 2 {:?}",
                LOCK_DEADLINE,
                svc(m),
                c1.state(),
                c2.state()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let (mut n, mut worst_sf, mut worst_ff, mut sum_sf) = (0i64, 0i64, 0i64, 0i64);
    let (mut worst_line, mut locked_all_along) = (0i64, true);
    let compare_until = Instant::now() + COMPARE_FOR;
    while Instant::now() < compare_until {
        let m = mono::mono_now_ns();
        let (svc_locked, s) = svc(m).expect("the service is anchored");
        let p1 = c1.ptp_at_mono_ns(m).expect("follower 1 anchored");
        let p2 = c2.ptp_at_mono_ns(m).expect("follower 2 anchored");
        locked_all_along &= svc_locked && c1.state().locked && c2.state().locked;
        let line = GM_UPTIME_NS + m + m / GM_PPM_DIV;
        worst_sf = worst_sf.max((s - p1).abs()).max((s - p2).abs());
        worst_ff = worst_ff.max((p1 - p2).abs());
        worst_line = worst_line.max((s - line).abs()).max((p1 - line).abs());
        sum_sf += (s - p1).abs();
        n += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Relaxed);
    gm.join().unwrap();
    service.join().unwrap();
    let (s1, s2) = (c1.state(), c2.state());
    drop((f1, f2));

    // The numbers the issue reports (`cargo test -- --nocapture`).
    eprintln!(
        "[#136 acceptance] all locked after {:.1}s; over {}s ({} samples): service vs follower \
         worst {:.1}us (mean {:.1}us), follower vs follower worst {:.1}us, worst vs the \
         grandmaster line {:.1}us; follower 1 {:?}; follower 2 {:?}",
        all_locked.as_secs_f64(),
        COMPARE_FOR.as_secs(),
        n,
        worst_sf as f64 / 1e3,
        sum_sf as f64 / n as f64 / 1e3,
        worst_ff as f64 / 1e3,
        worst_line as f64 / 1e3,
        s1,
        s2
    );
    assert!(n >= 100);
    assert!(locked_all_along, "a lock dropped while comparing");
    assert!(
        worst_sf <= LOCK_BOUND_NS,
        "service vs follower {}ns",
        worst_sf
    );
    // One kernel stamp per datagram reaches every receiver: two followers run the same law on the
    // same stamps and differ only by float rounding of their different virtual-clock origins.
    assert!(worst_ff <= 1_000, "follower vs follower {}ns", worst_ff);
    // Not only agreeing with each other: every clock sits on the grandmaster's line, behind it
    // by the loopback receive latency plus at most the phase error the [41] UNLOCK bound allows.
    assert!(
        worst_line <= UNLOCK_BOUND_NS,
        "{}ns off the grandmaster line",
        worst_line
    );
    assert_eq!(s1.gm_uuid, Some([0, 0x1d, 0xc1, 0x13, 0x60, 0x01]));
    assert_eq!((s1.epoch, s1.holdovers), (0, 0));
}
