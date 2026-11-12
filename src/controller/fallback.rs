//! dsync#129 slice 2 — the controller's glue for the fallback time source (`crate::fallback`).
//!
//! - Every loop iteration [`PtpController::service_fallback`] drains the fallback socket, runs
//!   the election and, on the source, sends a Sync every `SYNC_INTERVAL`.
//! - Every PTP time message the loop receives (PTPv1 or PTPv2, allowed by `gm_allowlist` or not)
//!   is reported first ([`PtpController::fallback_note_ptp`]): a source or a follower steps down
//!   in that iteration, before its next Sync could be sent (dstack, 2.11.2026).
//! - A follower's samples go into the ONE pipeline (`process_sync_pair`): the source is a new
//!   sync source, so the soft reset and the #158 re-anchor (`GmContinued` within 1 ms of `D`,
//!   else `Rebased`) take it, neither with a wall step. There is no second servo.
//! - The source sends its time line: `wall − D` in effect while its phase lock holds an anchor
//!   (after a Dante loss the line continues the PTP line on holdover; `D` moves with every step
//!   and slew the wall takes), else `wall − base` with every wall step added to `base`
//!   (`step_wall` is the one step choke point). Either way no step of the source's wall moves the
//!   line, so the source keeps its normal NTP / date path (review of #129: holding its NTP steps
//!   protected nothing and let its wall drift from UTC and from its followers).
//! - Liveness: `ptp_stale_at` stays "no allowed PTP for 10 s" (the re-join, the clock alarm, the
//!   receive counters); `time_source_stale_at` adds "and no live fallback source followed" for
//!   the offline edge and `/status`. With the feature off both are the same.

use super::*;
use crate::fallback::election::SYNC_INTERVAL;
use crate::fallback::wire::{self, format_node, FallbackSync, NodeId};
use crate::fallback::{Election, Event, FallbackTransport, Received, Role};
use std::net::Ipv4Addr;

/// At most this many datagrams are read per loop iteration (a flood never starves the loop).
const MAX_DATAGRAMS_PER_ITERATION: usize = 64;

/// A repeating fallback warning (a failing socket, a held NTP step) is logged at most this often.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// `/status.gm_class` values.
pub(super) const GM_CLASS_PTP: &str = "ptp";
pub(super) const GM_CLASS_FALLBACK: &str = "dsync-fallback";
pub(super) const GM_CLASS_NONE: &str = "none";

/// `/status.mode` on the fallback source and on a follower whose phase lock is within the bound.
pub(super) const FALLBACK_MODE: &str = "FALLBACK";

/// Opens the fallback transport on an interface address (`main`: the UDP sockets).
pub type FallbackOpener = Box<dyn FnMut(Ipv4Addr) -> Result<Box<dyn FallbackTransport>> + Send>;

/// All fallback state of one controller (the feature is off while `election` is `None`).
#[derive(Default)]
pub(super) struct FallbackGlue {
    pub(super) election: Option<Election>,
    pub(super) transport: Option<Box<dyn FallbackTransport>>,
    /// Re-opens the transport after a PTP re-join (`None` in the unit tests).
    reopener: Option<FallbackOpener>,
    /// The PTP interface's IPv4 (the node id is derived from it).
    pub(super) ip: Option<Ipv4Addr>,
    // ---- the source ----
    /// `t1 = wall − base_ns`.
    pub(super) base_ns: i64,
    pub(super) epoch: u32,
    seq: u16,
    next_send: Option<Instant>,
    sent: u64,
    // ---- a follower ----
    /// The source (and its epoch) whose samples the pipeline takes now.
    pub(super) followed: Option<(NodeId, u32)>,
    /// The last sample taken from it.
    pub(super) last_rx: Option<Instant>,
    // ---- throttles ----
    send_warned_at: Option<Instant>,
    recv_warned_at: Option<Instant>,
    spoof_warned_at: Option<Instant>,
}

fn due(last: Option<Instant>, now: Instant) -> bool {
    last.map_or(true, |t| now.saturating_duration_since(t) >= WARN_EVERY)
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// `main`: take part in the fallback time source (`system.fallback.enabled`) over
    /// `transport`, the node identified by its PTP interface's IPv4.
    pub fn enable_fallback(&mut self, transport: Box<dyn FallbackTransport>, iface_ip: Ipv4Addr) {
        let priority = self.config.fallback.priority;
        let me = wire::node_id(iface_ip);
        info!(
            "[FALLBACK] enabled: node {} ({}), priority {}{}, group {} -- a source only when no \
             PTP leader is visible",
            format_node(&me),
            iface_ip,
            priority,
            if priority == 0 { " (follow only)" } else { "" },
            wire::destination()
        );
        self.fallback.election = Some(Election::new(true, priority, me, Instant::now()));
        self.fallback.transport = Some(transport);
        self.fallback.ip = Some(iface_ip);
        self.update_shared_status();
    }

    /// `main`: how to open the transport again on another address, after a PTP re-join.
    pub fn set_fallback_reopener(&mut self, reopener: FallbackOpener) {
        self.fallback.reopener = Some(reopener);
    }

    /// A PTP re-join succeeded on `ip` (`maybe_rejoin_ptp`): the fallback socket joins again
    /// there too (a re-plugged NIC lost its membership, the same as the PTP sockets), and a moved
    /// address is a new node id. A failed re-open keeps the old socket.
    pub(super) fn fallback_rejoin(&mut self, ip: Ipv4Addr) {
        if self.fallback.election.is_none() {
            return;
        }
        if let Some(open) = self.fallback.reopener.as_mut() {
            match open(ip) {
                Ok(transport) => {
                    self.fallback.transport = Some(transport);
                    info!("[FALLBACK] socket re-joined on {}", ip);
                }
                Err(e) => {
                    // The id stays with the socket in use: a Sync's id must be its sender's.
                    warn!(
                        "[FALLBACK] re-join on {} failed: {:#} -- the old socket (and node id) stay",
                        ip, e
                    );
                    return;
                }
            }
        }
        if self.fallback.ip != Some(ip) {
            info!(
                "[FALLBACK] the PTP interface moved to {}: node {}",
                ip,
                format_node(&wire::node_id(ip))
            );
            self.fallback.ip = Some(ip);
            let events = self
                .fallback
                .election
                .as_mut()
                .map(|e| e.rebind(wire::node_id(ip)))
                .unwrap_or_default();
            self.apply_fallback_events(events, Instant::now());
        }
    }

    pub(super) fn fallback_role(&self) -> Role {
        self.fallback
            .election
            .as_ref()
            .map_or(Role::Off, Election::role)
    }

    pub(super) fn fallback_is_source(&self) -> bool {
        self.fallback_role() == Role::Source
    }

    /// Following a fallback source whose last sample is within `PTP_TIMEOUT_SECS`.
    pub(super) fn fallback_following_live(&self, now: Instant) -> bool {
        matches!(self.fallback_role(), Role::Follower(_))
            && self.fallback.last_rx.is_some_and(|t| {
                now.saturating_duration_since(t) <= Duration::from_secs(PTP_TIMEOUT_SECS)
            })
    }

    /// No allowed PTP for `PTP_TIMEOUT_SECS` AND no fallback source followed: no time source.
    pub(super) fn time_source_stale_at(&self, now: Instant) -> bool {
        self.ptp_stale_at(now) && !self.fallback_following_live(now)
    }

    /// Every loop iteration: the received datagrams, the election, the source's Sync.
    pub(super) fn service_fallback(&mut self, now: Instant) {
        if self.fallback.election.is_none() {
            return;
        }
        let mut inbox: Vec<Received> = Vec::new();
        if let Some(transport) = self.fallback.transport.as_mut() {
            for _ in 0..MAX_DATAGRAMS_PER_ITERATION {
                match transport.recv() {
                    Ok(Some(r)) => inbox.push(r),
                    Ok(None) => break,
                    Err(e) => {
                        if due(self.fallback.recv_warned_at, now) {
                            warn!("[FALLBACK] receive failed: {e:#}");
                            self.fallback.recv_warned_at = Some(now);
                        }
                        break;
                    }
                }
            }
        }
        for r in inbox {
            self.on_fallback_datagram(r, now);
        }
        let events = self
            .fallback
            .election
            .as_mut()
            .map(|e| e.tick(now))
            .unwrap_or_default();
        self.apply_fallback_events(events, now);
        if self.fallback_is_source() && self.fallback.next_send.map_or(true, |t| now >= t) {
            self.send_fallback_sync(now);
        }
        // A follower receives no PTP packet, so the loop's packet path (which runs the periodic
        // NTP check after every PTP packet) never runs: run it here, as the no-packet branch
        // does for an NTP-only node. It gates itself on its own interval.
        if self.fallback_following_live(now) {
            self.check_ntp_utc_tracking();
        }
    }

    /// A PTP time message (any version, any sender, before the allowlist) arrived at `now`.
    pub(super) fn fallback_note_ptp(&mut self, now: Instant, from: Option<Ipv4Addr>) {
        let Some(election) = self.fallback.election.as_mut() else {
            return;
        };
        let events = election.note_ptp(now, from);
        self.apply_fallback_events(events, now);
    }

    fn on_fallback_datagram(&mut self, r: Received, now: Instant) {
        let Some(sync) = FallbackSync::decode(&r.bytes) else {
            debug!(
                "[FALLBACK] ignored a {}-byte datagram from {:?}: not a fallback Sync",
                r.bytes.len(),
                r.from
            );
            return;
        };
        let Some(election) = self.fallback.election.as_mut() else {
            return;
        };
        if sync.node == election.me() {
            return;
        }
        // A node id is its sender's IPv4: a datagram claiming another node's id (or with no
        // sender) is dropped before the election sees it.
        if r.from.map(wire::node_id) != Some(sync.node) {
            if due(self.fallback.spoof_warned_at, now) {
                warn!(
                    "[FALLBACK] dropped a Sync claiming node {} from {:?}: the id is not its sender's",
                    format_node(&sync.node),
                    r.from
                );
                self.fallback.spoof_warned_at = Some(now);
            }
            return;
        }
        let events = election.note_announce(&sync, r.from, now);
        self.apply_fallback_events(events, now);
        if self.fallback_role() == Role::Follower(sync.node) {
            self.fallback.last_rx = Some(now);
            self.take_fallback_sample(&sync, r.from, r.rx);
        }
    }

    /// One sample of the followed source into the pipeline the PTP samples take.
    fn take_fallback_sample(
        &mut self,
        sync: &FallbackSync,
        from: Option<Ipv4Addr>,
        rx: SystemTime,
    ) {
        let key = (sync.node, sync.epoch);
        if self.fallback.followed != Some(key) {
            info!(
                "[FALLBACK] time from the fallback source {} ({}, priority {}, epoch {}) -- a new \
                 time line: soft reset, D re-anchored without a wall step",
                from.map_or_else(|| "?".to_string(), |ip| ip.to_string()),
                format_node(&sync.node),
                sync.priority,
                sync.epoch
            );
            self.fallback.followed = Some(key);
            self.current_sync_source = Some(sync.node);
            // `gm_source_ip` names a PTP grandmaster; there is none now.
            self.current_sync_source_ip = None;
            self.soft_reset_for_new_sync_source();
            if self.current_gm_uuid != Some(sync.node) {
                self.current_gm_uuid = Some(sync.node);
                self.on_grandmaster_uuid_change();
            }
        }
        self.process_sync_pair(sync.t1_ns, rx);
    }

    fn apply_fallback_events(&mut self, events: Vec<Event>, now: Instant) {
        if events.is_empty() {
            return;
        }
        for event in events {
            match event {
                // Once per peer (the election reports each clashing peer once; a peer id is its
                // sender's address and the table is capped, so this cannot flood).
                Event::PriorityClash { peer, ip, priority } => error!(
                    "[FALLBACK] priority clash: {} ({}) also has priority {} -- the lower node id \
                     wins; give every node its own system.fallback.priority",
                    ip.map_or_else(|| "?".to_string(), |ip| ip.to_string()),
                    format_node(&peer),
                    priority
                ),
                Event::Role { from, to, reason } => {
                    warn!(
                        "[FALLBACK] role {} -> {}: {}",
                        from.name(),
                        to.name(),
                        reason
                    );
                    if from == Role::Source {
                        info!(
                            "[FALLBACK] source stopped after {} Sync(s) (epoch {})",
                            self.fallback.sent, self.fallback.epoch
                        );
                        self.fallback.next_send = None;
                    }
                    if matches!(from, Role::Follower(_)) && from != to {
                        self.fallback.followed = None;
                        self.fallback.last_rx = None;
                    }
                    if to == Role::Source {
                        self.start_fallback_source(now);
                    }
                }
            }
        }
        self.update_shared_status();
    }

    /// The source's time line at `wall`: `wall − D` in effect while the phase lock holds an
    /// anchor (`D` moves with every step, slew and local NTP step the wall takes, so the line
    /// never does), else `wall − base` (`base` takes every step, see `fallback_note_wall_step`).
    pub(super) fn fallback_line_at(&self, wall: i64) -> i64 {
        let d = if self.date_sync.enabled {
            self.date_sync.d_in_effect(wall)
        } else {
            None
        };
        wall.wrapping_sub(d.unwrap_or(self.fallback.base_ns))
    }

    fn start_fallback_source(&mut self, now: Instant) {
        let wall = wall_now_ns();
        let anchored = self.date_sync.enabled && self.date_sync.d_in_effect(wall).is_some();
        let mut epoch = (wall.max(0) / 1_000_000_000) as u32;
        if epoch == self.fallback.epoch {
            epoch = epoch.wrapping_add(1);
        }
        let f = &mut self.fallback;
        f.base_ns = 0;
        f.epoch = epoch;
        f.seq = 0;
        f.sent = 0;
        f.next_send = Some(now);
        f.followed = None;
        f.last_rx = None;
        // A daily master's outage re-align (#119 1.12) would Join its wall alone when PTP returns;
        // its free-run is the fleet's now (every follower holds its line), and the fleet slews
        // the residual through the sync-source change. So the master does not re-align alone.
        self.date_sync.master_outage_realign = false;
        info!(
            "[FALLBACK] source: line = {}, epoch {}, a Sync every {} ms to {}",
            if anchored {
                "wall - D: the PTP line continued on holdover"
            } else {
                "the wall itself (no PTP anchor), every wall step kept out of it"
            },
            epoch,
            SYNC_INTERVAL.as_millis(),
            wire::destination()
        );
    }

    fn send_fallback_sync(&mut self, now: Instant) {
        let Some(election) = self.fallback.election.as_ref() else {
            return;
        };
        let (priority, node) = (election.priority(), election.me());
        // One-step: the line is read immediately before the send (its error is one-sided).
        let t1_ns = self.fallback_line_at(wall_now_ns());
        let f = &mut self.fallback;
        let Some(transport) = f.transport.as_mut() else {
            return;
        };
        let sync = FallbackSync {
            priority,
            node,
            seq: f.seq,
            epoch: f.epoch,
            t1_ns,
        };
        let result = transport.send(&sync.encode());
        f.seq = f.seq.wrapping_add(1);
        f.next_send = Some(match f.next_send {
            Some(t) if now < t + SYNC_INTERVAL => t + SYNC_INTERVAL,
            _ => now + SYNC_INTERVAL,
        });
        match result {
            Ok(()) => f.sent += 1,
            Err(e) => {
                if due(f.send_warned_at, now) {
                    warn!("[FALLBACK] Sync send failed: {e:#}");
                    f.send_warned_at = Some(now);
                }
            }
        }
    }

    /// `step_wall`: the wall moved by `delta_ns`. The source's line must not (its followers would
    /// read it as a time-base jump): unanchored, the step goes into `base` (anchored, `D` moves
    /// with it already).
    pub(super) fn fallback_note_wall_step(&mut self, delta_ns: i64) {
        if self.fallback_is_source() {
            self.fallback.base_ns = self.fallback.base_ns.wrapping_add(delta_ns);
            info!(
                "[FALLBACK] source: wall stepped {:+} us, the line kept continuous (base {} ns)",
                delta_ns / 1_000,
                self.fallback.base_ns
            );
        }
    }

    /// `/status` at `now`: the gm class, the role, the source; `mode` is overridden by the caller.
    pub(super) fn publish_fallback(&self, status: &mut SyncStatus, now: Instant) {
        let role = self.fallback_role();
        let following = self.fallback_following_live(now);
        status.gm_class = Some(
            if !self.ptp_stale_at(now) {
                GM_CLASS_PTP
            } else if role == Role::Source || following {
                GM_CLASS_FALLBACK
            } else {
                GM_CLASS_NONE
            }
            .to_string(),
        );
        status.fallback_role = Some(role.name().to_string());
        let (ip, epoch) = match role {
            Role::Source => (self.fallback.ip, Some(self.fallback.epoch)),
            Role::Follower(id) => {
                let peer = self.fallback.election.as_ref().and_then(|e| e.peer(&id));
                (peer.and_then(|p| p.ip), peer.map(|p| p.epoch))
            }
            Role::Off | Role::Candidate => (None, None),
        };
        status.fallback_source_ip = ip.map(|ip| ip.to_string());
        status.fallback_epoch = epoch;
        if role == Role::Source {
            // The source's time is its own: no PTP grandmaster to name.
            status.gm_uuid = self.fallback.election.as_ref().map(Election::me);
            status.gm_source_ip = None;
        }
    }
}

#[cfg(test)]
mod tests;
