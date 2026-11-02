//! The follower's thread over REAL sockets (Linux): two followers on one host share one port pair,
//! each receives every multicast Sync/Follow_Up of a fake grandmaster on loopback, with kernel
//! receive stamps, and publishes through the seqlock. 319/320 need root, so the sockets bind an
//! unprivileged pair (the factory maps 319 → base, 320 → base + 1); everything else is the
//! production path (`net::create_multicast_socket`: `SO_REUSEADDR`, the group join,
//! `SO_TIMESTAMPNS`).

use super::*;

#[cfg(unix)]
mod sockets {
    use super::*;
    use crate::net::{self, PinnedLookup};
    use crate::net_linux::{PtpSocketFactory, UdpPtpNetwork};
    use crate::ptp_tx::{self, Sender};
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

    const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

    /// The production sockets on `base` / `base + 1` instead of 319 / 320.
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
            let mapped = if port == crate::ptp::PTP_EVENT_PORT {
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

    /// A fake grandmaster: Sync + Follow_Up every 125 ms to the group on loopback, its time base
    /// the raw counter at +20 ppm plus an hour of uptime.
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
                source_uuid: [0, 0x1d, 0xc1, 1, 1, 1],
                grandmaster_uuid: [0, 0x1d, 0xc1, 1, 1, 1],
            };
            let start = mono::mono_now_ns();
            let mut seq = 0u16;
            while !stop.load(Ordering::Relaxed) {
                seq = seq.wrapping_add(1);
                let el = mono::mono_now_ns() - start;
                let t1 = 3_600_000_000_000 + el + el / 50_000;
                let sync = ptp_tx::sync(&sender, seq, t1);
                let _ = sock.send_to(&sync, (group, base));
                let fu = ptp_tx::follow_up(&sender, seq, t1);
                let _ = sock.send_to(&fu, (group, base + 1));
                std::thread::sleep(Duration::from_millis(125));
            }
        })
    }

    fn follower(base: u16) -> Follower {
        let net = UdpPtpNetwork::join(PortPair { base }, "lo".into(), LOOPBACK).expect("join");
        Follower::with_network(net, &FollowerConfig::on_interface("lo")).expect("start")
    }

    #[test]
    fn two_followers_share_the_port_pair_and_each_anchors_on_every_message() {
        // An unprivileged port pair of this test's own (the process id spreads parallel runs).
        // Below Linux's ephemeral range (32768-60999), so no outgoing socket can hold it.
        let base = 20_000 + (std::process::id() % 5_000) as u16 * 2;
        let a = follower(base);
        let b = follower(base);
        let stop = Arc::new(AtomicBool::new(false));
        let gm = grandmaster(base, Arc::clone(&stop));
        let (ca, cb) = (a.clock(), b.clock());
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(ca.now_ptp_ns().is_some() && cb.now_ptp_ns().is_some() && cb.state().windows >= 6) {
            assert!(
                Instant::now() < deadline,
                "no anchor: {:?} / {:?}",
                ca.state(),
                cb.state()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        gm.join().unwrap();
        let (sa, sb) = (ca.state(), cb.state());
        // Every Sync reached both (the kernel's multicast fan-out to every SO_REUSEADDR member).
        assert!(sa.windows >= 5 && sb.windows >= 5, "{:?} / {:?}", sa, sb);
        assert!(sa.windows.abs_diff(sb.windows) <= 1, "{:?} / {:?}", sa, sb);
        assert_eq!(sa.gm_uuid, Some([0, 0x1d, 0xc1, 1, 1, 1]));
        assert!(sa.date_offset_ns.is_some());
        // Both anchored on the same messages: their PTP times agree to well under a ms already
        // during the acquisition (the lock itself takes ~a minute; tests/library_acceptance.rs).
        let m = mono::mono_now_ns();
        let (pa, pb) = (ca.ptp_at_mono_ns(m).unwrap(), cb.ptp_at_mono_ns(m).unwrap());
        assert!((pa - pb).abs() < 1_000_000, "{}ns apart", pa - pb);
        // ... and the grandmaster's time base (an hour of uptime), not the raw counter's.
        assert!((pa - 3_600_000_000_000).abs() < 600_000_000_000);
        drop(a);
        drop(b); // the threads stop and join
        assert!(ca.state().windows >= 5, "a handle outlives the follower");
    }
}

/// A follower is pinned to the interface it is given (dsync#131): one that does not exist is
/// an error naming it, never another NIC.
#[test]
fn starting_on_a_missing_interface_names_it_and_joins_nothing() {
    let err = Follower::start(FollowerConfig::on_interface("dsync-no-such-nic0"))
        .err()
        .expect("no such interface");
    assert!(
        format!("{err:#}").contains("dsync-no-such-nic0"),
        "the error names the pin: {err:#}"
    );
}

#[test]
fn a_reader_handle_answers_nothing_before_the_anchor() {
    struct Silent;
    impl PtpNetwork for Silent {
        #[allow(clippy::type_complexity)]
        fn recv_packet(
            &mut self,
        ) -> Result<
            Option<(
                Vec<u8>,
                usize,
                std::time::SystemTime,
                Option<std::net::Ipv4Addr>,
            )>,
        > {
            Ok(None)
        }
        fn rejoin(&mut self) -> Result<crate::network::RejoinOutcome> {
            Err(anyhow::anyhow!("no network"))
        }
    }
    let f = Follower::with_network(Silent, &FollowerConfig::default()).unwrap();
    let c = f.clock();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(c.now_ptp_ns(), None);
    assert_eq!(c.ptp_at_mono_ns(0), None);
    let s = c.state();
    assert!(!s.locked && !s.holdover);
    assert_eq!(s.windows, 0);
}

/// A scripted receive path: queued datagrams with their wall stamps.
struct Script(std::collections::VecDeque<(Vec<u8>, i64)>);

impl PtpNetwork for Script {
    #[allow(clippy::type_complexity)]
    fn recv_packet(
        &mut self,
    ) -> Result<
        Option<(
            Vec<u8>,
            usize,
            std::time::SystemTime,
            Option<std::net::Ipv4Addr>,
        )>,
    > {
        Ok(self.0.pop_front().map(|(data, wall_ns)| {
            let len = data.len();
            let ts = std::time::UNIX_EPOCH + Duration::from_nanos(wall_ns as u64);
            (data, len, ts, None)
        }))
    }
    fn rejoin(&mut self) -> Result<crate::network::RejoinOutcome> {
        Err(anyhow::anyhow!("scripted"))
    }
}

/// Readings of a world whose wall runs `wall_minus_mono` ahead of the raw counter, 1 ms apart.
fn readings(
    start_mono: i64,
    wall_minus_mono: impl Fn(usize) -> i64,
) -> impl FnMut() -> mono::TimeBaseReading {
    let mut n = 0usize;
    move || {
        n += 1;
        mono::TimeBaseReading {
            mono_ns: start_mono + n as i64 * 1_000_000,
            wall_minus_mono_ns: wall_minus_mono(n),
        }
    }
}

const WALL_MINUS_MONO: i64 = 1_789_000_000_000_000_000;

/// Two Syncs queued while the thread stalled, the wall stepped +300 ms between their receive
/// and the next pass: both carry pre-step stamps, both would be converted 300 ms off with the
/// post-step reading. The whole pass is dropped; the next pass is converted normally.
#[test]
fn a_receive_pass_that_sees_a_wall_step_drops_every_packet_queued_across_it() {
    use crate::ptp_tx::{self, Sender};
    let sender = Sender {
        source_uuid: [0, 0x1d, 0xc1, 7, 7, 7],
        grandmaster_uuid: [0, 0x1d, 0xc1, 7, 7, 7],
    };
    let mut core = core::FollowerCore::new(GmAllowlist::default(), 0);
    let mut rx = Receiver {
        stamps: StampConverter::new(),
        warned_at: None,
    };
    rx.stamps.observe(mono::TimeBaseReading {
        mono_ns: 0,
        wall_minus_mono_ns: WALL_MINUS_MONO,
    });
    let stamp = WALL_MINUS_MONO + 500_000;
    let mut net = Script(
        vec![
            (ptp_tx::sync(&sender, 1, 1_000), stamp),
            (ptp_tx::sync(&sender, 2, 2_000), stamp + 125_000_000),
        ]
        .into(),
    );
    // Every reading of this pass is after the step.
    let mut after_step = readings(1_000_000, |_| WALL_MINUS_MONO + 300_000_000);
    assert!(
        !rx.pass(&mut net, &mut core, &mut after_step),
        "nothing was taken"
    );
    assert_eq!(core.straddled(), 2, "both queued Syncs dropped");
    assert_eq!(core.model().gm_uuid, None, "neither reached the pipeline");

    // The next pass (no step since the last reading) is taken.
    let mut net = Script(vec![(ptp_tx::sync(&sender, 3, 3_000), stamp + 300_000_000)].into());
    let mut steady = readings(10_000_000, |_| WALL_MINUS_MONO + 300_000_000);
    assert!(rx.pass(&mut net, &mut core, &mut steady));
    assert_eq!(core.straddled(), 2);
    assert_eq!(core.model().gm_uuid, Some(sender.grandmaster_uuid));
}

/// A receive thread that ended is never reported locked, even with a locked model published.
#[test]
fn a_clock_whose_receive_thread_is_gone_reports_holdover_never_lock() {
    let model = model::ClockModel {
        anchored: true,
        locked: true,
        ..Default::default()
    };
    let (lock, _writer) = SeqLock::new(model.to_words());
    let clock = PtpClock {
        model: lock,
        heartbeat: Arc::new(AtomicI64::new(mono::mono_now_ns())),
    };
    assert!(clock.state().locked && clock.alive());
    // Stuck: no beat for longer than the stale bound.
    clock.heartbeat.store(
        mono::mono_now_ns() - STALE_AFTER_NS - 1_000_000,
        Ordering::Release,
    );
    let s = clock.state();
    assert!(!s.locked && s.holdover);
    // Dead.
    clock.heartbeat.store(DEAD, Ordering::Release);
    assert!(!clock.alive());
    assert!(!clock.state().locked);
    assert!(
        clock.now_ptp_ns().is_some(),
        "the time itself runs on in holdover"
    );
}

#[test]
fn dropping_the_follower_ends_its_thread_and_the_clock_says_so() {
    let f = Follower::with_network(Script(Default::default()), &FollowerConfig::default()).unwrap();
    let c = f.clock();
    assert!(c.alive());
    drop(f);
    assert!(!c.alive());
}
