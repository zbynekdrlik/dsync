//! dsync#129 slice 2 — who is the fallback source, pure and `Instant`-driven.
//!
//! The rules (dstack ROZHODNUTÉ 2.11.2026 over the spec):
//!
//! - **Only with no leader at all.** A candidate becomes the source only when it has seen no PTP
//!   time message from ANY source (the controller reports every one, before its allowlist) and no
//!   announce of any other fallback source for [`SILENCE`] (the controller's `PTP_TIMEOUT_SECS`,
//!   the one staleness) plus its rank delay (`(255 − priority) × RANK_STEP`), so the best
//!   candidate starts first. Before the first packet the silence counts from the start.
//! - **Step down on the first PTP message** ([`Election::note_ptp`]): a source and a follower are
//!   `candidate` at once, no hold.
//! - **Never announce while another leader is visible**: a live source (heard within `SILENCE`)
//!   is never pre-empted by a better candidate that starts later. Only two sources that run at
//!   the same time (a healed partition, a tied start) resolve: the lower-ranked one yields on the
//!   first better announce. Rank = (priority, then the LOWER node id).
//! - Followers follow the best live announcer.
//! - Two nodes on one priority are a configuration fault: reported once per peer
//!   ([`Event::PriorityClash`]) and broken by the node id, so there is never a second source.

use super::wire::{format_node, FallbackSync, NodeId};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// The fallback source's Sync interval: 8 Hz, the Dante grandmaster's cadence that every window,
/// floor and fit constant of the sample pipeline is tuned on.
pub const SYNC_INTERVAL: Duration = Duration::from_millis(125);

/// No leader for this long = none (the controller's `PTP_TIMEOUT_SECS`, pinned to it by a const
/// assert there through `ptp_rejoin::REJOIN_AFTER`).
pub const SILENCE: Duration = crate::ptp_rejoin::REJOIN_AFTER;

/// Each priority below 255 waits this much longer before it starts (≤ 5.1 s at priority 1).
pub const RANK_STEP: Duration = Duration::from_millis(20);

/// A peer silent this long is forgotten (its entry only; the liveness rule is `SILENCE`).
const FORGET_AFTER: Duration = Duration::from_secs(300);

/// At most this many peers are kept (a flood of forged ids never grows memory): a new one
/// replaces the one heard longest ago. A real fleet has a handful of candidate sources.
pub const MAX_PEERS: usize = 64;

/// The role of this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The feature is off.
    Off,
    /// Taking part, neither following nor announcing.
    Candidate,
    /// Following the fallback source with this id.
    Follower(NodeId),
    /// Announcing.
    Source,
}

impl Role {
    /// The `/status.fallback_role` name.
    pub fn name(&self) -> &'static str {
        match self {
            Role::Off => "off",
            Role::Candidate => "candidate",
            Role::Follower(_) => "follower",
            Role::Source => "source",
        }
    }
}

/// What the election decided; the controller logs every one as a `[FALLBACK]` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Role {
        from: Role,
        to: Role,
        reason: String,
    },
    PriorityClash {
        peer: NodeId,
        ip: Option<Ipv4Addr>,
        priority: u8,
    },
}

/// Another node's announces.
#[derive(Debug, Clone)]
pub struct Peer {
    pub priority: u8,
    pub ip: Option<Ipv4Addr>,
    pub epoch: u32,
    pub last_heard: Instant,
    clash_reported: bool,
}

pub struct Election {
    me: NodeId,
    priority: u8,
    role: Role,
    /// The last PTP time message, or the start.
    last_ptp: Instant,
    /// The last announce of any other node.
    last_peer: Option<Instant>,
    peers: BTreeMap<NodeId, Peer>,
}

/// Higher is better: the priority, then the lower node id.
fn show_ip(ip: Option<Ipv4Addr>) -> String {
    ip.map_or_else(|| "?".to_string(), |ip| ip.to_string())
}

fn rank(priority: u8, id: &NodeId) -> (u8, std::cmp::Reverse<NodeId>) {
    (priority, std::cmp::Reverse(*id))
}

impl Election {
    /// `enabled = false` is [`Role::Off`] for ever.
    pub fn new(enabled: bool, priority: u8, me: NodeId, now: Instant) -> Self {
        Election {
            me,
            priority,
            role: if enabled { Role::Candidate } else { Role::Off },
            last_ptp: now,
            last_peer: None,
            peers: BTreeMap::new(),
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn me(&self) -> NodeId {
        self.me
    }

    pub fn priority(&self) -> u8 {
        self.priority
    }

    pub fn peer(&self, id: &NodeId) -> Option<&Peer> {
        self.peers.get(id)
    }

    /// The source followed now, with its last announce.
    pub fn followed(&self) -> Option<(NodeId, &Peer)> {
        match self.role {
            Role::Follower(id) => self.peers.get(&id).map(|p| (id, p)),
            _ => None,
        }
    }

    /// How long a candidate of this priority waits beyond `SILENCE`.
    pub fn rank_delay(priority: u8) -> Duration {
        RANK_STEP * u32::from(255 - priority)
    }

    /// A PTP time message (any source, any version, allowed or not) was seen at `now`: a PTP
    /// leader is visible. A source or a follower steps down at once.
    pub fn note_ptp(&mut self, now: Instant, from: Option<Ipv4Addr>) -> Vec<Event> {
        self.last_ptp = now;
        match self.role {
            Role::Source | Role::Follower(_) => {
                let from =
                    from.map_or_else(|| "an unknown sender".to_string(), |ip| ip.to_string());
                vec![self.become_(
                    Role::Candidate,
                    format!(
                        "a PTP time message from {from}: a PTP leader is visible, stepping down"
                    ),
                )]
            }
            Role::Off | Role::Candidate => Vec::new(),
        }
    }

    /// An announce `sync` from `ip` arrived at `now`.
    pub fn note_announce(
        &mut self,
        sync: &FallbackSync,
        ip: Option<Ipv4Addr>,
        now: Instant,
    ) -> Vec<Event> {
        if self.role == Role::Off || sync.node == self.me {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.peers.contains_key(&sync.node) && self.peers.len() >= MAX_PEERS {
            if let Some(oldest) = self
                .peers
                .iter()
                .min_by_key(|(_, p)| p.last_heard)
                .map(|(id, _)| *id)
            {
                self.peers.remove(&oldest);
            }
        }
        let peer = self.peers.entry(sync.node).or_insert(Peer {
            priority: sync.priority,
            ip,
            epoch: sync.epoch,
            last_heard: now,
            clash_reported: false,
        });
        peer.priority = sync.priority;
        peer.ip = ip.or(peer.ip);
        peer.epoch = sync.epoch;
        peer.last_heard = now;
        if sync.priority == self.priority && self.priority > 0 && !peer.clash_reported {
            peer.clash_reported = true;
            events.push(Event::PriorityClash {
                peer: sync.node,
                ip: peer.ip,
                priority: sync.priority,
            });
        }
        self.last_peer = Some(now);
        events.extend(self.tick(now));
        events
    }

    /// Re-evaluate the role at `now` (every loop iteration).
    pub fn tick(&mut self, now: Instant) -> Vec<Event> {
        if self.role == Role::Off {
            return Vec::new();
        }
        self.peers
            .retain(|_, p| now.saturating_duration_since(p.last_heard) < FORGET_AFTER);
        let mut events = Vec::new();
        let ptp_visible = now.saturating_duration_since(self.last_ptp) <= SILENCE;
        // Owned, so the role can change below.
        let best = self
            .best_live_peer(now)
            .map(|(id, p)| (id, p.priority, p.ip, p.epoch));
        match self.role {
            Role::Off => {}
            Role::Source => {
                if let Some((id, priority, ip, _)) = best {
                    if rank(priority, &id) > rank(self.priority, &self.me) {
                        let reason = format!(
                            "a better fallback source {} ({}, priority {}) is announcing: yielding",
                            show_ip(ip),
                            format_node(&id),
                            priority
                        );
                        events.push(self.become_(Role::Follower(id), reason));
                    }
                }
            }
            Role::Candidate | Role::Follower(_) => {
                let target = if ptp_visible { None } else { best };
                match (self.role, target) {
                    (Role::Follower(cur), Some((id, ..))) if cur == id => {}
                    (_, Some((id, priority, ip, epoch))) => {
                        let reason = format!(
                            "following the fallback source {} ({}, priority {}, epoch {})",
                            show_ip(ip),
                            format_node(&id),
                            priority,
                            epoch
                        );
                        events.push(self.become_(Role::Follower(id), reason));
                    }
                    (Role::Follower(cur), None) => {
                        let reason = if ptp_visible {
                            "a PTP leader is visible".to_string()
                        } else {
                            format!(
                                "the fallback source {} went silent ({} s without an announce)",
                                format_node(&cur),
                                SILENCE.as_secs()
                            )
                        };
                        events.push(self.become_(Role::Candidate, reason));
                    }
                    _ => {}
                }
                if self.role == Role::Candidate && self.priority > 0 && !ptp_visible {
                    let last_leader = match self.last_peer {
                        Some(p) if p > self.last_ptp => p,
                        _ => self.last_ptp,
                    };
                    let quiet = now.saturating_duration_since(last_leader);
                    let wait = SILENCE + Self::rank_delay(self.priority);
                    if best.is_none() && quiet >= wait {
                        let reason = format!(
                            "no PTP leader and no fallback source for {:.2} s (priority {}, wait \
                             {:.2} s): becoming the fallback source",
                            quiet.as_secs_f64(),
                            self.priority,
                            wait.as_secs_f64()
                        );
                        events.push(self.become_(Role::Source, reason));
                    }
                }
            }
        }
        events
    }

    fn best_live_peer(&self, now: Instant) -> Option<(NodeId, &Peer)> {
        self.peers
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.last_heard) <= SILENCE)
            .max_by_key(|(id, p)| rank(p.priority, id))
            .map(|(id, p)| (*id, p))
    }

    fn become_(&mut self, to: Role, reason: String) -> Event {
        let from = self.role;
        self.role = to;
        Event::Role { from, to, reason }
    }
}

#[cfg(test)]
mod tests;
