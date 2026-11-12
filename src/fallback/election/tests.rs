//! The election on a simulated network of several nodes (virtual `Instant`s, 5 ms steps): every
//! source multicasts a Sync each `SYNC_INTERVAL` to the live nodes of its partition, and a PTP
//! grandmaster (when present) reaches every live node at 8 Hz.

use super::*;
use crate::fallback::wire::node_id;

const DT: Duration = Duration::from_millis(5);

struct Node {
    el: Election,
    ip: Ipv4Addr,
    alive: bool,
    partition: u8,
    next_send: Option<Instant>,
    seq: u16,
    epoch: u32,
    /// Every Sync this node sent: (time since start, seq).
    sent: Vec<Duration>,
}

struct Net {
    t0: Instant,
    now: Instant,
    nodes: Vec<Node>,
    /// A PTP grandmaster sends from this instant on (8 Hz), until `ptp_until`.
    ptp_from: Option<Instant>,
    ptp_until: Option<Instant>,
    next_ptp: Option<Instant>,
    events: Vec<(Duration, usize, Event)>,
}

impl Net {
    fn new(priorities: &[u8]) -> Self {
        let t0 = Instant::now();
        let nodes = priorities
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                let ip = Ipv4Addr::new(10, 77, 9, 10 + i as u8);
                Node {
                    el: Election::new(true, p, node_id(ip), t0),
                    ip,
                    alive: true,
                    partition: 0,
                    next_send: None,
                    seq: 0,
                    epoch: 0,
                    sent: Vec::new(),
                }
            })
            .collect();
        Net {
            t0,
            now: t0,
            nodes,
            ptp_from: None,
            ptp_until: None,
            next_ptp: None,
            events: Vec::new(),
        }
    }

    fn at(&self) -> Duration {
        self.now - self.t0
    }

    /// A node that (re)starts now, with a fresh election.
    fn boot(&mut self, i: usize) {
        let n = &mut self.nodes[i];
        n.el = Election::new(true, n.el.priority(), node_id(n.ip), self.now);
        n.alive = true;
        n.next_send = None;
    }

    fn record(&mut self, i: usize, events: Vec<Event>) {
        let at = self.at();
        for e in events {
            if let Event::Role {
                to: Role::Source, ..
            } = e
            {
                self.nodes[i].epoch += 1;
                self.nodes[i].next_send = Some(self.now);
            }
            self.events.push((at, i, e));
        }
    }

    fn step(&mut self) {
        self.now += DT;
        // The PTP grandmaster.
        let ptp_live = self.ptp_from.is_some_and(|f| self.now >= f)
            && self.ptp_until.map_or(true, |u| self.now < u);
        if ptp_live {
            let due = self.next_ptp.map_or(true, |n| self.now >= n);
            if due {
                self.next_ptp = Some(self.now + Duration::from_millis(125));
                for i in 0..self.nodes.len() {
                    if self.nodes[i].alive {
                        let ev = self.nodes[i]
                            .el
                            .note_ptp(self.now, Some(Ipv4Addr::new(10, 77, 9, 230)));
                        self.record(i, ev);
                    }
                }
            }
        }
        for i in 0..self.nodes.len() {
            if !self.nodes[i].alive {
                continue;
            }
            let ev = self.nodes[i].el.tick(self.now);
            self.record(i, ev);
        }
        // Sources send.
        for i in 0..self.nodes.len() {
            let n = &self.nodes[i];
            if !n.alive || n.el.role() != Role::Source {
                continue;
            }
            if n.next_send.is_some_and(|t| self.now < t) {
                continue;
            }
            let sync = FallbackSync {
                priority: n.el.priority(),
                node: n.el.me(),
                seq: n.seq,
                epoch: n.epoch,
                t1_ns: 0,
            };
            let (from, part) = (n.ip, n.partition);
            let at = self.at();
            let n = &mut self.nodes[i];
            n.seq = n.seq.wrapping_add(1);
            n.next_send = Some(self.now + SYNC_INTERVAL);
            n.sent.push(at);
            for j in 0..self.nodes.len() {
                if j != i && self.nodes[j].alive && self.nodes[j].partition == part {
                    let ev = self.nodes[j].el.note_announce(&sync, Some(from), self.now);
                    self.record(j, ev);
                }
            }
        }
    }

    fn run_until(&mut self, at: Duration) {
        while self.at() < at {
            self.step();
        }
    }

    fn sources(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&i| self.nodes[i].alive && self.nodes[i].el.role() == Role::Source)
            .collect()
    }

    fn role(&self, i: usize) -> Role {
        self.nodes[i].el.role()
    }

    fn id(&self, i: usize) -> NodeId {
        self.nodes[i].el.me()
    }

    /// When node `i` last became `to`.
    fn became(&self, i: usize, to: Role) -> Option<Duration> {
        self.events
            .iter()
            .rev()
            .find(|(_, n, e)| *n == i && matches!(e, Event::Role { to: t, .. } if *t == to))
            .map(|(at, ..)| *at)
    }

    /// Instants at which more than one live node was a source, sampled every step after `from`.
    fn assert_one_source_from(&mut self, from: Duration, until: Duration) {
        self.run_until(from);
        while self.at() < until {
            self.step();
            assert!(
                self.sources().len() <= 1,
                "two sources at {:?}: {:?}",
                self.at(),
                self.sources()
            );
        }
    }
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

#[test]
fn with_no_ptp_the_best_priority_is_the_only_source_and_everyone_follows_it_129() {
    let mut net = Net::new(&[100, 200, 0, 50]);
    net.run_until(secs(30.0));
    assert_eq!(net.sources(), vec![1], "priority 200 is the source");
    let started = net.became(1, Role::Source).expect("became the source");
    let expected = SILENCE + Election::rank_delay(200);
    assert!(
        started >= expected && started <= expected + DT,
        "started at {started:?}, expected {expected:?}"
    );
    for i in [0, 2, 3] {
        assert_eq!(net.role(i), Role::Follower(net.id(1)), "node {i}");
        let followed = net.became(i, Role::Follower(net.id(1))).expect("follows");
        assert!(
            followed <= started + SYNC_INTERVAL,
            "node {i} followed at {followed:?}, the source started at {started:?}"
        );
    }
    // Nobody else ever announced.
    for i in [0, 2, 3] {
        assert!(net.nodes[i].sent.is_empty(), "node {i} sent");
    }
}

#[test]
fn the_source_lost_hands_over_to_the_next_priority_129() {
    let mut net = Net::new(&[100, 200, 0, 50]);
    net.run_until(secs(30.0));
    assert_eq!(net.sources(), vec![1]);
    net.nodes[1].alive = false;
    let lost = net.at();
    net.assert_one_source_from(lost, secs(60.0));
    assert_eq!(net.sources(), vec![0], "priority 100 took over");
    let started = net.became(0, Role::Source).expect("took over");
    let wait = SILENCE + Election::rank_delay(100);
    assert!(
        started >= lost + wait - SYNC_INTERVAL && started <= lost + wait + DT,
        "took over at {started:?}, the source was lost at {lost:?}"
    );
    for i in [2, 3] {
        assert_eq!(net.role(i), Role::Follower(net.id(0)), "node {i}");
    }
}

/// THE step-down (dstack ROZHODNUTÉ 2.11.2026, rule 3 + 5): the first PTP time message moves the
/// source and every follower to `candidate` at that instant; no Sync is sent after it, and while
/// PTP stays no node announces or follows again.
#[test]
fn the_first_ptp_message_steps_every_source_and_follower_down_at_once_129() {
    let mut net = Net::new(&[100, 200, 0, 50]);
    net.run_until(secs(30.0));
    assert_eq!(net.sources(), vec![1]);
    let ptp_at = net.now + secs(0.0123);
    net.ptp_from = Some(ptp_at);
    net.run_until(secs(30.2));
    let first_ptp = ptp_at - net.t0;
    for i in 0..4 {
        assert_eq!(net.role(i), Role::Candidate, "node {i}");
    }
    let down = net.became(1, Role::Candidate).expect("stepped down");
    assert!(
        down >= first_ptp && down <= first_ptp + DT,
        "stepped down at {down:?}, the first PTP message came at {first_ptp:?}"
    );
    assert!(
        net.nodes[1].sent.iter().all(|&t| t < first_ptp + DT),
        "a Sync after the PTP message: {:?}",
        net.nodes[1].sent.last()
    );
    let sent_before = net.nodes[1].sent.len();
    net.run_until(secs(120.0));
    assert_eq!(
        net.nodes[1].sent.len(),
        sent_before,
        "announced while PTP is visible"
    );
    for i in 0..4 {
        assert_eq!(net.role(i), Role::Candidate, "node {i}");
    }
}

#[test]
fn ptp_gone_again_brings_the_fallback_back_after_the_silence_129() {
    let mut net = Net::new(&[100, 200, 0, 50]);
    net.ptp_from = Some(net.t0);
    net.ptp_until = Some(net.t0 + secs(40.0));
    net.run_until(secs(40.0));
    assert!(net.sources().is_empty(), "never with PTP present");
    net.run_until(secs(70.0));
    assert_eq!(net.sources(), vec![1]);
    let started = net.became(1, Role::Source).expect("source");
    assert!(started >= secs(40.0) + SILENCE, "started at {started:?}");
}

#[test]
fn a_healed_partition_keeps_one_source_within_one_interval_129() {
    let mut net = Net::new(&[200, 100, 0, 0]);
    net.nodes[1].partition = 1;
    net.nodes[3].partition = 1;
    net.run_until(secs(30.0));
    assert_eq!(net.sources(), vec![0, 1], "one source per partition");
    assert_eq!(net.role(3), Role::Follower(net.id(1)));
    for n in net.nodes.iter_mut() {
        n.partition = 0;
    }
    let healed = net.at();
    net.run_until(healed + SYNC_INTERVAL + DT);
    assert_eq!(net.sources(), vec![0], "the lower priority yielded");
    assert_eq!(net.role(1), Role::Follower(net.id(0)));
    assert_eq!(net.role(3), Role::Follower(net.id(0)));
    net.assert_one_source_from(net.at(), secs(60.0));
}

#[test]
fn a_live_source_is_never_pre_empted_by_a_better_node_that_starts_later_129() {
    let mut net = Net::new(&[200, 100, 0]);
    net.nodes[0].alive = false;
    net.run_until(secs(30.0));
    assert_eq!(net.sources(), vec![1]);
    net.boot(0);
    net.assert_one_source_from(net.at(), secs(90.0));
    assert_eq!(net.sources(), vec![1], "the incumbent stays");
    assert_eq!(net.role(0), Role::Follower(net.id(1)));
    assert!(
        net.nodes[0].sent.is_empty(),
        "the better node never announced"
    );
}

#[test]
fn a_ptp_message_keeps_a_candidate_from_ever_starting_129() {
    // Every ~9 s one PTP time message (e.g. a foreign grandmaster the allowlist drops): never a
    // 10 s silence, never a source.
    let t0 = Instant::now();
    let mut el = Election::new(true, 255, node_id(Ipv4Addr::new(10, 0, 0, 1)), t0);
    let mut t = t0;
    for k in 0..2_000 {
        t += Duration::from_millis(50);
        if k % 180 == 0 {
            el.note_ptp(t, None);
        }
        assert!(el.tick(t).is_empty());
        assert_eq!(el.role(), Role::Candidate);
    }
}

#[test]
fn equal_priorities_are_reported_and_still_give_one_source_129() {
    let mut net = Net::new(&[100, 100, 0]);
    net.assert_one_source_from(secs(0.0), secs(40.0));
    let lower = if net.id(0) < net.id(1) { 0 } else { 1 };
    let other = 1 - lower;
    // Both may start in the same step (the same rank delay); the higher id yields at once.
    assert_eq!(net.sources(), vec![lower]);
    assert_eq!(net.role(other), Role::Follower(net.id(lower)));
    let clashes: Vec<_> = net
        .events
        .iter()
        .filter(|(_, _, e)| matches!(e, Event::PriorityClash { priority: 100, .. }))
        .map(|(_, n, _)| *n)
        .collect();
    assert_eq!(
        clashes,
        vec![other],
        "reported once, by the node that heard the clash"
    );
}

#[test]
fn priority_zero_follows_only_and_off_does_nothing_129() {
    let t0 = Instant::now();
    let mut zero = Election::new(true, 0, node_id(Ipv4Addr::new(10, 0, 0, 1)), t0);
    let mut off = Election::new(false, 255, node_id(Ipv4Addr::new(10, 0, 0, 2)), t0);
    let sync = FallbackSync {
        priority: 9,
        node: node_id(Ipv4Addr::new(10, 0, 0, 3)),
        seq: 0,
        epoch: 1,
        t1_ns: 0,
    };
    for k in 1..=1_200 {
        let t = t0 + Duration::from_millis(50) * k;
        assert!(zero.tick(t).is_empty(), "priority 0 never starts");
        assert!(off.tick(t).is_empty());
        assert!(off.note_ptp(t, None).is_empty());
        assert!(off.note_announce(&sync, None, t).is_empty());
        assert_eq!(off.role(), Role::Off);
    }
    assert_eq!(zero.role(), Role::Candidate);
    let t = t0 + secs(61.0);
    zero.note_announce(&sync, Some(Ipv4Addr::new(10, 0, 0, 3)), t);
    assert_eq!(zero.role(), Role::Follower(sync.node));
    assert_eq!(
        zero.followed().map(|(id, p)| (id, p.epoch)),
        Some((sync.node, 1))
    );
    // Its own datagrams (multicast loop) are never a peer.
    let mut me = Election::new(true, 5, sync.node, t0);
    assert!(me.note_announce(&sync, None, t).is_empty());
    assert!(me.peer(&sync.node).is_none());
}

#[test]
fn a_source_going_silent_drops_its_followers_to_candidate_129() {
    let t0 = Instant::now();
    let mut el = Election::new(true, 0, node_id(Ipv4Addr::new(10, 0, 0, 1)), t0);
    let src = node_id(Ipv4Addr::new(10, 0, 0, 3));
    let sync = FallbackSync {
        priority: 9,
        node: src,
        seq: 0,
        epoch: 1,
        t1_ns: 0,
    };
    let heard = t0 + secs(20.0);
    el.note_announce(&sync, None, heard);
    assert_eq!(el.role(), Role::Follower(src));
    assert!(
        el.tick(heard + SILENCE).is_empty(),
        "still live at the bound"
    );
    let ev = el.tick(heard + SILENCE + DT);
    assert_eq!(el.role(), Role::Candidate);
    assert!(matches!(&ev[..], [Event::Role { reason, .. }] if reason.contains("went silent")));
    assert_eq!(Role::Off.name(), "off");
    assert_eq!(Role::Source.name(), "source");
    assert_eq!(Role::Follower(src).name(), "follower");
    assert_eq!(Role::Candidate.name(), "candidate");
}

#[test]
fn a_flood_of_forged_ids_never_grows_the_peer_table_past_its_cap_129() {
    let t0 = Instant::now();
    let mut el = Election::new(true, 0, node_id(Ipv4Addr::new(10, 0, 0, 1)), t0);
    let t = t0 + secs(11.0);
    for i in 0..10_000_u32 {
        let o = i.to_be_bytes();
        let sync = FallbackSync {
            priority: 1,
            node: [0xD5, 0xFB, o[0], o[1], o[2], o[3]],
            seq: 0,
            epoch: 1,
            t1_ns: 0,
        };
        el.note_announce(&sync, None, t + Duration::from_micros(u64::from(i)));
        assert!(el.peers.len() <= MAX_PEERS);
    }
    // The newest are kept.
    let last = 9_999_u32.to_be_bytes();
    assert!(el
        .peer(&[0xD5, 0xFB, last[0], last[1], last[2], last[3]])
        .is_some());
}

#[test]
fn a_moved_interface_gives_a_new_id_and_restarts_the_source_under_it_129() {
    let t0 = Instant::now();
    let old = node_id(Ipv4Addr::new(10, 0, 0, 1));
    let new = node_id(Ipv4Addr::new(10, 0, 0, 9));
    let mut el = Election::new(true, 9, old, t0);
    el.tick(t0 + secs(20.0));
    assert_eq!(el.role(), Role::Source);
    assert!(el.rebind(old).is_empty(), "the same id: nothing");
    let ev = el.rebind(new);
    assert_eq!(el.me(), new);
    assert!(matches!(
        &ev[..],
        [Event::Role {
            to: Role::Candidate,
            ..
        }]
    ));
    let ev = el.tick(t0 + secs(20.1));
    assert!(matches!(
        &ev[..],
        [Event::Role {
            to: Role::Source,
            ..
        }]
    ));
}
