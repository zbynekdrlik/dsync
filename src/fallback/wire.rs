//! dsync#129 slice 2 — the fallback source's datagram.
//!
//! One-step: the source reads its time line immediately before `send_to` and carries it in the
//! datagram itself. The stamp's error is one-sided (the frame always leaves after the stamp), the
//! kind of delay the follower's delay floor (#160) removes. 32 bytes, big-endian:
//!
//! | Offset | Field |
//! |---|---|
//! | 0..4 | magic `DSFB` |
//! | 4 | version (1) |
//! | 5 | kind (1 = Sync) |
//! | 6 | the source's priority |
//! | 7 | flags (0) |
//! | 8..14 | the source's node id |
//! | 14..16 | sequence |
//! | 16..20 | epoch (a new one each time a node becomes the source) |
//! | 20..28 | `t1`, the source's time line, ns (i64) |
//! | 28..32 | reserved (0) |
//!
//! It goes ONLY to [`GROUP`]:[`PORT`], a dsync-private group: never the PTP group 224.0.1.129 and
//! never ports 319/320, so no Dante device can ever take it for PTP (and it is shorter than a
//! PTPv1 header, so `ptp::is_time_message` never accepts it either).

use std::net::{Ipv4Addr, SocketAddrV4};

/// The dsync-private multicast group (administratively scoped, RFC 2365).
pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 1);
/// Its UDP port.
pub const PORT: u16 = 31910;

pub const MAGIC: [u8; 4] = *b"DSFB";
pub const VERSION: u8 = 1;
pub const KIND_SYNC: u8 = 1;
pub const SIZE: usize = 32;

/// A node's identity on the fallback wire: `D5:FB:` + its PTP interface's IPv4. It is also the
/// "grandmaster UUID" a follower publishes while it follows the source.
pub type NodeId = [u8; 6];

/// The node id of a node whose PTP interface has `ip`.
pub fn node_id(ip: Ipv4Addr) -> NodeId {
    let o = ip.octets();
    [0xD5, 0xFB, o[0], o[1], o[2], o[3]]
}

/// The one destination any fallback datagram is ever sent to.
pub fn destination() -> SocketAddrV4 {
    SocketAddrV4::new(GROUP, PORT)
}

/// One fallback Sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackSync {
    pub priority: u8,
    pub node: NodeId,
    pub seq: u16,
    pub epoch: u32,
    /// The source's time line at the moment it sent the datagram (ns).
    pub t1_ns: i64,
}

impl FallbackSync {
    pub fn encode(&self) -> [u8; SIZE] {
        let mut b = [0u8; SIZE];
        b[0..4].copy_from_slice(&MAGIC);
        b[4] = VERSION;
        b[5] = KIND_SYNC;
        b[6] = self.priority;
        b[8..14].copy_from_slice(&self.node);
        b[14..16].copy_from_slice(&self.seq.to_be_bytes());
        b[16..20].copy_from_slice(&self.epoch.to_be_bytes());
        b[20..28].copy_from_slice(&self.t1_ns.to_be_bytes());
        b
    }

    /// `None` for anything that is not a version-1 fallback Sync (a later version may append
    /// fields: a longer datagram of version 1 is read).
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < SIZE || b[0..4] != MAGIC || b[4] != VERSION || b[5] != KIND_SYNC {
            return None;
        }
        let mut node = [0u8; 6];
        node.copy_from_slice(&b[8..14]);
        Some(FallbackSync {
            priority: b[6],
            node,
            seq: u16::from_be_bytes([b[14], b[15]]),
            epoch: u32::from_be_bytes([b[16], b[17], b[18], b[19]]),
            t1_ns: i64::from_be_bytes(b[20..28].try_into().ok()?),
        })
    }
}

/// `aa:bb:cc:dd:ee:ff`.
pub fn format_node(node: &NodeId) -> String {
    node.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FallbackSync {
        FallbackSync {
            priority: 200,
            node: node_id(Ipv4Addr::new(10, 77, 9, 202)),
            seq: 0xBEEF,
            epoch: 1_794_441_616,
            t1_ns: -1_234_567_890_123,
        }
    }

    #[test]
    fn a_sync_round_trips_129() {
        let s = sample();
        let b = s.encode();
        assert_eq!(b.len(), SIZE);
        assert_eq!(&b[0..4], b"DSFB");
        assert_eq!(FallbackSync::decode(&b), Some(s));
        let mut longer = b.to_vec();
        longer.extend_from_slice(&[9, 9, 9]);
        assert_eq!(
            FallbackSync::decode(&longer),
            Some(s),
            "a later version may append"
        );
    }

    #[test]
    fn anything_else_is_refused_129() {
        let b = sample().encode();
        assert_eq!(FallbackSync::decode(&b[..SIZE - 1]), None, "short");
        for (i, v) in [(0, b'X'), (4, 2), (5, 0)] {
            let mut bad = b;
            bad[i] = v;
            assert_eq!(FallbackSync::decode(&bad), None, "byte {i}");
        }
        assert_eq!(FallbackSync::decode(&[]), None);
    }

    #[test]
    fn the_node_id_names_the_interface_129() {
        let id = node_id(Ipv4Addr::new(10, 77, 9, 202));
        assert_eq!(id, [0xD5, 0xFB, 10, 77, 9, 202]);
        assert_eq!(format_node(&id), "d5:fb:0a:4d:09:ca");
    }

    /// The never-send pin, part 1: the only destination is the private group, never the PTP group
    /// and never a PTP port (part 2: `transport.rs` refuses to open on them). A fallback datagram
    /// is not a PTP time message either.
    #[test]
    fn the_fallback_never_targets_the_ptp_group_or_ports_129() {
        let d = destination();
        assert_eq!(*d.ip(), Ipv4Addr::new(239, 255, 77, 1));
        assert_eq!(d.port(), 31910);
        assert_ne!(*d.ip(), Ipv4Addr::new(224, 0, 1, 129));
        assert!(![crate::ptp::PTP_EVENT_PORT, crate::ptp::PTP_GENERAL_PORT].contains(&d.port()));
        assert!(d.ip().is_multicast());
        assert!(!crate::ptp::is_time_message(&sample().encode()));
    }
}
