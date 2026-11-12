//! dsync#129 slice 2 — the controller's fallback glue: the step-down on the first PTP message
//! through the real loop, the source's line, a follower locked through the real datagram path,
//! and the hand-back to PTP with no wall step.

use super::*;
use crate::clock::MockSystemClock;
use crate::fallback::config::FallbackConfig;
use crate::traits::{MockNtpSource, MockPtpNetwork};
use std::collections::VecDeque;
use std::sync::Mutex;

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

const ME: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 202);
const SOURCE_IP: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 201);
const GM_IP: Ipv4Addr = Ipv4Addr::new(10, 77, 7, 109);
const SYNC_NS: i64 = 125_000_000;

/// xorshift64* + Box-Muller, a fixed seed (never an unseeded source).
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

/// A transport with no socket: what was sent, and an inbox the test fills.
#[derive(Clone, Default)]
struct FakeTransport {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    inbox: Arc<Mutex<VecDeque<Received>>>,
}

impl FallbackTransport for FakeTransport {
    fn send(&mut self, datagram: &[u8]) -> Result<()> {
        self.sent.lock().expect("sent").push(datagram.to_vec());
        Ok(())
    }
    fn recv(&mut self) -> Result<Option<Received>> {
        Ok(self.inbox.lock().expect("inbox").pop_front())
    }
}

impl FakeTransport {
    fn sent(&self) -> Vec<FallbackSync> {
        self.sent
            .lock()
            .expect("sent")
            .iter()
            .map(|b| FallbackSync::decode(b).expect("a fallback Sync"))
            .collect()
    }
}

fn config(priority: u8) -> SystemConfig {
    let mut config = SystemConfig::default();
    config.filters.calibration_samples = 0;
    config.filters.warmup_secs = 0.0;
    config.fallback = FallbackConfig {
        enabled: true,
        priority,
    };
    config
}

fn controller(
    net: MockPtpNetwork,
    clock: MockSystemClock,
    config: SystemConfig,
) -> (Ctl, FakeTransport) {
    let mut c: Ctl = PtpController::new(
        clock,
        net,
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    );
    let transport = FakeTransport::default();
    c.enable_fallback(Box::new(transport.clone()), ME);
    // No PTP for 30 s: the election and the liveness both see a network without a leader.
    let quiet_since = Instant::now() - Duration::from_secs(30);
    c.last_ptp_packet = quiet_since;
    let priority = c.config.fallback.priority;
    c.fallback.election = Some(Election::new(
        true,
        priority,
        wire::node_id(ME),
        quiet_since,
    ));
    (c, transport)
}

/// A PTPv1 Sync (124 bytes) and its Follow_Up (52 bytes) carrying `t1_ns`.
fn ptp_pair(uuid: [u8; 6], seq: u16, t1_ns: i64) -> (Vec<u8>, Vec<u8>) {
    let mut sync = vec![0u8; 124];
    sync[1] = 1; // versionPTP
    sync[22..28].copy_from_slice(&uuid);
    sync[30..32].copy_from_slice(&seq.to_be_bytes());
    sync[32] = 0; // Sync
    sync[54..60].copy_from_slice(&uuid); // grandmasterClockUuid
    let mut fup = vec![0u8; 52];
    fup[1] = 1;
    fup[22..28].copy_from_slice(&uuid);
    fup[30..32].copy_from_slice(&seq.to_be_bytes());
    fup[32] = 2; // Follow_Up
    fup[42..44].copy_from_slice(&seq.to_be_bytes());
    let secs = (t1_ns / 1_000_000_000) as u32;
    let nanos = (t1_ns % 1_000_000_000) as u32;
    fup[44..48].copy_from_slice(&secs.to_be_bytes());
    fup[48..52].copy_from_slice(&nanos.to_be_bytes());
    (sync, fup)
}

/// THE step-down (dstack ROZHODNUTÉ 2.11.2026, rules 3 + 5), through the real loop: a node that
/// IS the fallback source receives one PTP time message — from a grandmaster the allowlist DROPS
/// (any leader on the network counts) — and leaves the source role in that very iteration; it
/// sends no Sync after it, and announces nothing while PTP stays.
#[test]
fn the_source_steps_down_on_the_first_ptp_message_before_its_next_sync_129() {
    let (ptp, _) = ptp_pair([0x00, 0x1d, 0xc1, 0x08, 0x02, 0x14], 1, 5_000_000_000);
    let mut net = MockPtpNetwork::new();
    net.expect_recv_packet()
        .times(1)
        .returning(move || Ok(Some((ptp.clone(), 124, SystemTime::now(), Some(GM_IP)))));
    net.expect_recv_packet().returning(|| Ok(None));
    net.expect_rejoin()
        .returning(|| Err(anyhow::anyhow!("no NIC in a unit test")));
    let mut cfg = config(200);
    cfg.gm_allowlist = vec!["10.77.9.0/24".to_string()];
    let (mut c, transport) = controller(net, MockSystemClock::new(), cfg);

    let t = Instant::now();
    c.service_fallback(t);
    assert_eq!(
        c.fallback_role(),
        Role::Source,
        "no leader for 30 s: the source"
    );
    assert_eq!(transport.sent().len(), 1, "its first Sync at once");
    assert_eq!(
        c.get_status_shared().read().expect("status").mode,
        FALLBACK_MODE
    );

    // One loop iteration: the PTP Sync arrives (and is dropped by the allowlist).
    c.process_loop_iteration().expect("an iteration");
    assert_eq!(
        c.fallback_role(),
        Role::Candidate,
        "the source stepped down in the iteration that saw the PTP message"
    );
    assert_eq!(
        c.current_gm_uuid, None,
        "the dropped grandmaster was not adopted"
    );

    // Its next Sync was due 125 ms later: never sent, nor any later one.
    for ms in [125_u64, 250, 1_000, 9_000] {
        c.service_fallback(t + Duration::from_millis(ms));
    }
    assert_eq!(transport.sent().len(), 1, "a Sync after the step-down");
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.fallback_role.as_deref(), Some("candidate"));
    assert_ne!(st.mode, FALLBACK_MODE);
}

#[test]
fn the_source_line_continues_the_ptp_line_and_absorbs_every_wall_step_129() {
    let mut clock = MockSystemClock::new();
    clock.expect_step_clock().returning(|_, _| Ok(()));
    let (mut c, transport) = controller(MockPtpNetwork::new(), clock, config(255));
    let t = Instant::now();
    c.service_fallback(t);
    assert!(c.fallback_is_source());
    assert_eq!(c.fallback.base_ns, 0, "never anchored: the wall itself");
    let first = transport.sent()[0];
    assert_eq!(first.priority, 255);
    assert_eq!(first.node, wire::node_id(ME));
    assert_eq!(first.seq, 0);
    let wall = wall_now_ns();
    assert!((wall - first.t1_ns).abs() < 1_000_000_000, "t1 = the wall");

    // A wall step of +10 ms, then −3 ms: the line must not move with them.
    c.step_wall(Duration::from_millis(10), 1).expect("step");
    assert_eq!(c.fallback.base_ns, 10_000_000);
    c.step_wall(Duration::from_millis(3), -1).expect("step");
    assert_eq!(c.fallback.base_ns, 7_000_000);
    let before = wall_now_ns();
    c.service_fallback(t + Duration::from_millis(125));
    let after = wall_now_ns();
    let second = transport.sent()[1];
    assert_eq!(second.seq, 1);
    assert_eq!(second.epoch, first.epoch);
    assert!(
        second.t1_ns >= before - 7_000_000 && second.t1_ns <= after - 7_000_000,
        "t1 = wall − base, read at the send"
    );
    // 8 Hz: nothing before the next interval, one per interval after it.
    c.service_fallback(t + Duration::from_millis(200));
    assert_eq!(transport.sent().len(), 2);
    c.service_fallback(t + Duration::from_millis(250));
    assert_eq!(transport.sent().len(), 3);

    // A local NTP step is held on the source only.
    assert!(c.fallback_holds_ntp_step(1_500));
    c.fallback_note_ptp(t + Duration::from_millis(300), Some(GM_IP));
    assert!(!c.fallback_holds_ntp_step(1_500), "no longer the source");
    // A step while not the source leaves the (unused) base alone.
    c.step_wall(Duration::from_millis(1), 1).expect("step");
    assert_eq!(c.fallback.base_ns, 7_000_000);
}

#[test]
fn a_source_with_a_ptp_anchor_continues_the_ptp_line_129() {
    let (mut c, transport) = controller(MockPtpNetwork::new(), MockSystemClock::new(), config(9));
    // The phase lock anchored D = wall − PTP time (3.5 s here) while PTP was there.
    let d = 3_500_000_000;
    c.date_sync.core.set_anchor(d);
    let t = Instant::now();
    c.service_fallback(t);
    let anchor = c.date_sync.d_in_effect(wall_now_ns()).expect("anchored");
    assert_eq!(c.fallback.base_ns, anchor);
    let sync = transport.sent()[0];
    let ptp_now = wall_now_ns() - anchor;
    assert!(
        (ptp_now - sync.t1_ns).abs() < 1_000_000_000,
        "the line is this node's view of the PTP time"
    );
}

#[test]
fn the_feature_off_changes_nothing_129() {
    let mut c: Ctl = PtpController::new(
        MockSystemClock::new(),
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        SystemConfig::default(),
    );
    let now = Instant::now();
    c.service_fallback(now);
    c.fallback_note_ptp(now, None);
    assert_eq!(c.fallback_role(), Role::Off);
    assert!(!c.fallback_holds_ntp_step(5_000));
    c.last_ptp_packet = now - Duration::from_secs(PTP_TIMEOUT_SECS + 1);
    assert_eq!(c.time_source_stale_at(now), c.ptp_stale_at(now));
    c.update_shared_status();
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.fallback_role.as_deref(), Some("off"));
    assert_eq!(st.gm_class.as_deref(), Some(GM_CLASS_NONE));
    assert_eq!(st.fallback_source_ip, None);
    assert_eq!(st.mode, "NTP-only");
}

#[test]
fn foreign_and_own_datagrams_are_ignored_129() {
    let (mut c, transport) = controller(MockPtpNetwork::new(), MockSystemClock::new(), config(0));
    let now = Instant::now();
    let own = FallbackSync {
        priority: 0,
        node: wire::node_id(ME),
        seq: 0,
        epoch: 1,
        t1_ns: 1,
    };
    let mut inbox = transport.inbox.lock().expect("inbox");
    inbox.push_back(Received {
        bytes: b"not a fallback datagram".to_vec(),
        rx: SystemTime::now(),
        from: Some(SOURCE_IP),
    });
    inbox.push_back(Received {
        bytes: own.encode().to_vec(),
        rx: SystemTime::now(),
        from: Some(ME),
    });
    drop(inbox);
    c.service_fallback(now);
    assert_eq!(c.fallback_role(), Role::Candidate);
    assert!(c
        .fallback
        .election
        .as_ref()
        .expect("on")
        .peer(&own.node)
        .is_none());
    assert!(transport.sent().is_empty(), "priority 0 never sends");
}

/// The follower side through the REAL datagram path (`on_fallback_datagram` → the election →
/// `process_sync_pair`), closed loop: the word the controller writes moves the offset the next
/// samples see. A fallback lock reads FALLBACK (never LOCK); then the network gets a PTP
/// grandmaster on the same time line (+200 µs, a continued time base): the follower steps down at
/// the first PTP message and re-locks on PTP, and nothing ever steps the wall.
#[test]
fn a_follower_locks_on_the_fallback_source_and_hands_back_to_ptp_without_a_wall_step_129() {
    let _guard = crate::controller::heavy_test_guard();
    let word = Arc::new(Mutex::new(0.0_f64));
    let w = word.clone();
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(move |factor| {
        *w.lock().expect("word") = (factor - 1.0) * 1_000_000.0;
        Ok(())
    });
    clock.expect_step_clock().never();
    let (mut c, _transport) = controller(MockPtpNetwork::new(), clock, config(0));
    let source = wire::node_id(SOURCE_IP);
    let mut noise = Noise(0x5eed_0129);
    // The follower's oscillator against the source's line, and its wall's offset from the line.
    let natural_ppm = -7.5;
    let wall_minus_line_ns: i64 = 3_456_789_000;
    let mut true_offset_us = 0.0_f64;
    let mut line_ns: i64 = 1_700_000_000 * 1_000_000_000;
    let t0 = Instant::now();
    let mut engaged_at = None;
    let mut reported_at = None;
    let sample = |k: i64, noise: &mut Noise, true_offset_us: &mut f64| {
        let applied = *word.lock().expect("word");
        *true_offset_us += (natural_ppm + applied) * SYNC_NS as f64 / 1e9;
        // 300 µs path + a one-sided queueing delay + a few µs of stamp jitter.
        let delay_us = 300.0 - 20.0 * noise.uniform().ln() + (3.0 * noise.gauss()).abs();
        let rx_ns = wall_minus_line_ns + ((*true_offset_us + delay_us) * 1_000.0).round() as i64;
        let now = t0 + Duration::from_nanos((k * SYNC_NS) as u64);
        (now, rx_ns)
    };

    // 4 minutes of fallback Syncs.
    for k in 0..(240 * 8) {
        line_ns += SYNC_NS;
        let (now, rx_off) = sample(k, &mut noise, &mut true_offset_us);
        let sync = FallbackSync {
            priority: 7,
            node: source,
            seq: k as u16,
            epoch: 42,
            t1_ns: line_ns,
        };
        c.on_fallback_datagram(
            Received {
                bytes: sync.encode().to_vec(),
                rx: SystemTime::UNIX_EPOCH + Duration::from_nanos((line_ns + rx_off) as u64),
                from: Some(SOURCE_IP),
            },
            now,
        );
        let s = k as f64 / 8.0;
        if engaged_at.is_none() && c.date_sync.core.engaged() {
            engaged_at = Some(s);
        }
        if reported_at.is_none() && c.reported_lock() {
            reported_at = Some(s);
        }
    }
    assert_eq!(c.fallback_role(), Role::Follower(source));
    let engaged_at = engaged_at.expect("the phase lock engaged on the fallback source");
    let reported_at = reported_at.expect("the lock gate was set on the fallback source");
    assert!(engaged_at <= 90.0, "engaged at {engaged_at} s");
    assert!(reported_at <= 180.0, "[41] at {reported_at} s");
    let e_us = c.date_sync.core.last_error_ns().expect("e") as f64 / 1_000.0;
    assert!(e_us.abs() <= 50.0, "|e| {e_us:.1} µs at 4 min");
    c.update_shared_status();
    {
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert_eq!(st.mode, FALLBACK_MODE, "a fallback lock never reads LOCK");
        assert!(st.is_locked, "is_locked keeps its meaning");
        assert_eq!(st.gm_class.as_deref(), Some(GM_CLASS_FALLBACK));
        assert_eq!(st.fallback_role.as_deref(), Some("follower"));
        assert_eq!(st.fallback_source_ip.as_deref(), Some("10.77.9.201"));
        assert_eq!(st.fallback_epoch, Some(42));
        assert_eq!(st.gm_uuid, Some(source));
    }
    let continued_before = c.date_sync.core.gm_continued();

    // A PTP grandmaster appears on the same time line, 200 µs off (a continued base).
    let gm = [0x00, 0x1d, 0xc1, 0x08, 0x02, 0x14];
    for k in (240 * 8)..(480 * 8) {
        line_ns += SYNC_NS;
        let (now, rx_off) = sample(k, &mut noise, &mut true_offset_us);
        let t1 = line_ns + 200_000;
        let rx = SystemTime::UNIX_EPOCH + Duration::from_nanos((line_ns + rx_off) as u64);
        let (sync_pkt, fup) = ptp_pair(gm, k as u16, t1);
        c.fallback_note_ptp(now, Some(GM_IP));
        c.note_allowed_ptp_packet(Instant::now());
        let header = PtpV1Header::parse(&sync_pkt).expect("header");
        c.handle_sync_message(&header, &sync_pkt, rx);
        let header = PtpV1Header::parse(&fup).expect("header");
        c.handle_followup_message(&header, &fup);
        if k == 240 * 8 {
            assert_eq!(
                c.fallback_role(),
                Role::Candidate,
                "stepped down at the first PTP message"
            );
        }
    }
    assert_eq!(c.current_gm_uuid, Some(gm));
    assert!(
        c.date_sync.core.gm_continued() > continued_before,
        "the PTP line within 1 ms of D is a continued time base (D kept)"
    );
    assert!(c.date_sync.core.engaged(), "re-locked on PTP");
    let e_us = c.date_sync.core.last_error_ns().expect("e") as f64 / 1_000.0;
    assert!(e_us.abs() <= 50.0, "|e| {e_us:.1} µs on PTP");
    c.update_shared_status();
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.gm_class.as_deref(), Some(GM_CLASS_PTP));
    assert_eq!(st.fallback_role.as_deref(), Some("candidate"));
    assert_eq!(st.mode, "LOCK");
    // `step_clock` is `.never()`: no wall step on the way in or out.
}
