//! Which grandmaster PORT a receiver follows (dsync#136, dstack's capture on strih-snv, 5.11.2026).
//!
//! A Dante grandmaster with a primary and a secondary network port sends every Sync and Follow_Up
//! from BOTH, under one clock id (`sourceUuid`) and one sequence id; only `sourcePortId` (and the
//! source address) tell them apart. The ports' stamps differ by ~470 µs and their Syncs arrive
//! ~450 µs apart, so each port alone is the same time within tens of µs, but a Sync of one port
//! paired with the other port's Follow_Up is ~470 µs off. The service and the in-process follower
//! paired by sequence id alone, and the follower on strih (which heard both ports) ran −0.48 ms off
//! the service.
//!
//! The rule, shared by both receivers:
//! - a Sync source is ONE port of one clock, [`PortId`];
//! - a Follow_Up pairs only with the pending Sync of the same [`PortId`] and sequence id;
//! - the receiver follows the first port it hears and ignores another port of the same clock while
//!   that one is live; it moves to the other port only after [`PORT_SILENCE_NS`] without a Sync
//!   from the followed one (a primary link down). That move is the same clock, so it is no new time
//!   base: the receiver drops what it has pending, and the phase lock takes the port's few-µs
//!   difference like any path change.
//! - a Sync from another CLOCK is a source change, as before.

use crate::ptp::PtpV1Header;

/// After this long without a Sync from the followed port, another port of the same clock is
/// followed. Four Syncs a second: 2 s is 8 missed Syncs, far past any jitter, and well inside the
/// 10 s PTP staleness (`ptp_rejoin::REJOIN_AFTER`).
pub const PORT_SILENCE_NS: i64 = 2_000_000_000;

/// One port of one PTP clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PortId {
    pub uuid: [u8; 6],
    pub port: u16,
}

impl PortId {
    /// The sender of a message.
    pub fn of(header: &PtpV1Header) -> Self {
        PortId {
            uuid: header.source_uuid,
            port: header.source_port_id,
        }
    }
}

/// What a Sync means for the followed port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortVerdict {
    /// The first Sync heard: this port is followed now.
    First,
    /// A Sync of the followed port.
    Followed,
    /// Another clock: a source change (the caller's time-base handling).
    OtherClock { from: PortId },
    /// Another port of the followed clock, after the followed port fell silent: followed now.
    PortMoved { from: PortId },
    /// Another port of the followed clock while the followed one is live: ignore this Sync.
    OtherPort,
}

/// The followed port.
#[derive(Clone, Copy, Debug, Default)]
pub struct PortLock {
    followed: Option<(PortId, i64)>,
}

impl PortLock {
    /// Judge a Sync from `id`, received at `now_ns` (a monotonic clock).
    pub fn on_sync(&mut self, id: PortId, now_ns: i64) -> PortVerdict {
        let verdict = match self.followed {
            None => PortVerdict::First,
            Some((current, _)) if current == id => PortVerdict::Followed,
            Some((current, _)) if current.uuid != id.uuid => {
                PortVerdict::OtherClock { from: current }
            }
            Some((current, heard)) if now_ns.saturating_sub(heard) > PORT_SILENCE_NS => {
                PortVerdict::PortMoved { from: current }
            }
            Some(_) => return PortVerdict::OtherPort,
        };
        self.followed = Some((id, now_ns));
        verdict
    }

    /// The followed port, if any.
    pub fn followed(&self) -> Option<PortId> {
        self.followed.map(|(id, _)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: PortId = PortId {
        uuid: [0, 0x1d, 0xc1, 8, 2, 0x14],
        port: 1,
    };
    const SECONDARY: PortId = PortId { port: 2, ..PRIMARY };
    const OTHER_GM: PortId = PortId {
        uuid: [0, 0x1d, 0xc1, 9, 9, 9],
        port: 1,
    };

    #[test]
    fn the_first_port_heard_is_followed_and_the_other_port_ignored_while_it_is_live() {
        let mut lock = PortLock::default();
        assert_eq!(lock.on_sync(PRIMARY, 0), PortVerdict::First);
        assert_eq!(lock.on_sync(SECONDARY, 450_000), PortVerdict::OtherPort);
        assert_eq!(lock.on_sync(PRIMARY, 250_000_000), PortVerdict::Followed);
        assert_eq!(
            lock.on_sync(SECONDARY, 250_000_000 + PORT_SILENCE_NS),
            PortVerdict::OtherPort,
            "exactly the silence is not yet silent"
        );
        assert_eq!(lock.followed(), Some(PRIMARY));
    }

    #[test]
    fn the_other_port_is_followed_once_the_followed_one_falls_silent() {
        let mut lock = PortLock::default();
        lock.on_sync(PRIMARY, 0);
        assert_eq!(
            lock.on_sync(SECONDARY, PORT_SILENCE_NS + 1),
            PortVerdict::PortMoved { from: PRIMARY }
        );
        assert_eq!(lock.followed(), Some(SECONDARY));
        assert_eq!(
            lock.on_sync(PRIMARY, PORT_SILENCE_NS + 2),
            PortVerdict::OtherPort,
            "no flip back while the new port is live"
        );
    }

    #[test]
    fn another_clock_is_a_source_change_at_once() {
        let mut lock = PortLock::default();
        lock.on_sync(PRIMARY, 0);
        assert_eq!(
            lock.on_sync(OTHER_GM, 1),
            PortVerdict::OtherClock { from: PRIMARY }
        );
        assert_eq!(lock.followed(), Some(OTHER_GM));
    }
}
