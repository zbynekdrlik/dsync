//! The PTP receive path as a trait: what the service's controller and the in-process follower
//! read packets through (moved from the service's `traits.rs`, dsync#136).

use anyhow::Result;
use std::net::Ipv4Addr;

/// dsync#112 — where a [`PtpNetwork::rejoin`] left the PTP receive path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejoinOutcome {
    /// The interface the PTP group is joined on now: the interface name on Linux, the Npcap
    /// device name on Windows.
    pub iface: String,
    /// The IPv4 address the group is joined on.
    pub ip: Ipv4Addr,
    /// The interface or the address differs from the previous successful join.
    pub changed: bool,
    /// dsync#212: the PINNED name joined (one entry of the `system.interface` list, as the
    /// operator wrote it); `None` when the node is not pinned. On Windows `iface` is the Npcap
    /// device, so this is the only place the listed name is.
    pub pinned_name: Option<String>,
}

/// dsync#215 — what the CALLER of [`PtpNetwork::rejoin`] knows about the trusted grandmaster
/// on the current join. Only the caller can say: it owns the `gm_allowlist` and the staleness
/// (a backend sees every grandmaster's time messages, the allowed and the foreign ones alike).
/// "Trusted" = a source the allowlist allows; with an EMPTY allowlist every source is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustedPtp {
    /// The trusted grandmaster's time still arrives on the current join (the #191 link-local
    /// leave): a pinned list stays on the joined listed NIC while it is usable.
    Flowing,
    /// No trusted time message for the staleness bound (`ptp_rejoin::REJOIN_AFTER`): a pinned
    /// list moves on to the next usable listed NIC, whatever a foreign grandmaster still sends
    /// on the joined one.
    Silent,
}

#[cfg_attr(any(test, feature = "mock"), mockall::automock)]
pub trait PtpNetwork {
    /// Receive a packet. Returns Ok(Some((data, len, timestamp, source_ip))) if packet received.
    /// Returns Ok(None) if no packet (timeout/wouldblock).
    /// source_ip is the IP address of the device sending the PTP packet.
    #[allow(clippy::type_complexity)]
    fn recv_packet(
        &mut self,
    ) -> Result<Option<(Vec<u8>, usize, std::time::SystemTime, Option<Ipv4Addr>)>>;

    /// Reset the network state (e.g. clear buffers). Default impl does nothing.
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    /// dsync#112 — open the PTP receive path again: select the interface NOW (first the one
    /// that carries the address the grandmaster's time was last received on, else the startup
    /// selection), drop the old sockets / capture (and with them the multicast membership, which
    /// may belong to a netdev that is gone), then join again.
    ///
    /// The controller calls it when no allowed PTP packet has come for 10 s, then on a backoff
    /// (`crate::ptp_rejoin`). It never touches the clock. An `Err` when the interface cannot be
    /// resolved leaves the old path in place; an `Err` when the new one cannot be opened leaves
    /// none (nothing is received) until the next attempt. Either way the controller retries on
    /// its schedule.
    ///
    /// dsync#215: `trusted` is the caller's judgement of the trusted grandmaster on the current
    /// join; a pinned LIST stays on the joined listed NIC only while it is
    /// [`TrustedPtp::Flowing`] (`net::rejoin_order`). Unpinned and single-pin re-joins ignore it.
    fn rejoin(&mut self, trusted: TrustedPtp) -> Result<RejoinOutcome>;
}
