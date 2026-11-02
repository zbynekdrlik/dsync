//! The Windows Npcap NTP transport (dsync#53). The PTP capture and the shared pcap helpers
//! (device lookup, the run-time Npcap load, HostHighPrec captures) moved to the `dsync-ptp` crate
//! (dsync#136) and are re-exported here, so `crate::net_pcap::…` paths are unchanged.

pub use dsync_ptp::net_pcap::*;

use anyhow::{anyhow, Result};
use dsync_ptp::wpcap::{Capture, Device};
use log::{debug, info};
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, SystemTime};

/// Select the Npcap device whose IPv4 subnet can actually reach
/// `server_ip` — NOT the PTP capture interface (dsync#53 continuation).
///
/// Confirmed live on the stream box: `PcapNtpTransport` used to inherit the
/// PTP capture device (`find_device(interface_name)`), which fails outright
/// on a dual-homed host where PTP (the grandmaster) and NTP (LAN) live on different
/// subnets — the NTP server is genuinely unreachable from that device
/// (WSAENETUNREACH / os error 10051). This instead enumerates EVERY Npcap
/// device's IPv4 addresses and picks the one whose subnet contains the
/// (already-resolved) `server_ip`, via the pure, unit-tested
/// `ntp_packet::select_ntp_pcap_device` — the same decision a manual
/// `Find-NetRoute` makes, made automatically.
pub fn find_device_for_ntp_server(server_ip: Ipv4Addr) -> Result<Device> {
    let devices = list_devices()?;

    let mut candidates = Vec::new();
    let mut candidate_device_idx = Vec::new();
    for (idx, d) in devices.iter().enumerate() {
        for a in &d.addresses {
            if let (std::net::IpAddr::V4(ip), Some(std::net::IpAddr::V4(netmask))) =
                (a.addr, a.netmask)
            {
                if !ip.is_loopback() {
                    candidates.push(crate::ntp_packet::CandidateInterface {
                        name: d.name.clone(),
                        ip,
                        netmask: Some(netmask),
                    });
                    candidate_device_idx.push(idx);
                }
            }
        }
    }

    match crate::ntp_packet::select_ntp_pcap_device(server_ip, &candidates) {
        Some(ci) => Ok(devices[candidate_device_idx[ci]].clone()),
        None => {
            let considered: Vec<String> = candidates
                .iter()
                .map(|c| format!("{} ({})", c.name, c.ip))
                .collect();
            Err(anyhow!(
                "No Npcap-capturable interface can reach NTP server {} -- considered: [{}] \
                 (dsync#53: this host may be dual-homed with PTP and NTP on separate \
                 networks)",
                server_ip,
                considered.join(", ")
            ))
        }
    }
}

// ============================================================================
// #53 — kernel-timestamped NTP transport (Windows)
// ============================================================================
// The Windows NTP client's offset scattered by tens of milliseconds even
// while this same box's PTP servo (above) reported locked, because
// `NtpClient::measure_once()` (src/ntp.rs) took t1/t4 as userspace
// `SystemTime::now()` calls around a blocking socket — exactly the
// scheduling-jitter problem `NpcapPtpNetwork` above was built to avoid for
// PTP. `PcapNtpTransport` gives NTP the same treatment: a SEPARATE Npcap
// capture (own device lookup, own HostHighPrec timestamps, own BPF filter —
// unicast NTP traffic to one server, nothing to do with the PTP multicast
// group) sees both our own outgoing request leaving the NIC (t1) and the
// server's reply arriving (t4), sidestepping userspace scheduling delay on
// both ends. t2/t3 come from the reply packet's own fields.
//
// This glue is intentionally thin: everything it depends on (packet
// build/parse, the offset/RTT formula, frame parsing) lives in the
// zero-I/O, fully-unit-tested `ntp_packet` module. This file only opens the
// capture, sends the request, and correlates captured packets by direction.
const NTP_PORT: u16 = 123;

/// Upper bound on one pcap-based NTP round trip. NTP checks run every
/// 10-30s in production (`controller.rs`'s adaptive interval) — this bounds
/// the cost of a single check so a lost request or reply can never hang the
/// sync loop (dsync#53's "bound the cost" requirement).
const NTP_PCAP_TIMEOUT: Duration = Duration::from_millis(500);

/// Kernel-timestamped NTP transport for Windows: captures t1 (our own
/// request leaving the NIC) and t4 (the server's reply arriving) via Npcap
/// `HostHighPrec` timestamps instead of userspace `SystemTime::now()`.
pub struct PcapNtpTransport {
    capture: Capture,
    socket: UdpSocket,
    server_ip: Ipv4Addr,
    local_ip: Ipv4Addr,
}

impl PcapNtpTransport {
    /// `server_ip` is the already-resolved NTP server address (the BPF
    /// filter needs a concrete IP, not a hostname). The capture device is
    /// selected by which interface can actually REACH `server_ip`
    /// (`find_device_for_ntp_server`, dsync#53 continuation) — NOT by
    /// inheriting the PTP capture interface, which fails outright on a
    /// dual-homed host where PTP and NTP live on different subnets.
    pub fn new(server_ip: Ipv4Addr, dscp: &crate::dscp::DscpConfig) -> Result<Self> {
        let device = find_device_for_ntp_server(server_ip)?;
        let local_ip = device_ipv4(&device)?;

        let filter = format!("udp and host {} and port {}", server_ip, NTP_PORT);
        let capture = open_hiprec_capture(&device, &filter)?;

        // A plain socket only to SEND the request -- Npcap sees the packet
        // leave the NIC and gives us the real t1, so this socket's own
        // send() timing is irrelevant (that userspace timing is exactly the
        // defect this transport exists to route around).
        let socket = UdpSocket::bind((local_ip, 0))?;
        socket.connect((server_ip, NTP_PORT))?;

        // dsync#52: mark the NTP client request egress with DSCP. On Windows
        // the OS filters a socket-set IP_TOS (needs a QoS policy at provisioning),
        // so `crate::dscp::apply` is a logged no-op here — wired for completeness
        // and to surface the QoS guidance in the log. See `crate::dscp`.
        crate::dscp::apply(&socket, dscp, "ntp-client request (pcap)");

        info!(
            "[NTP][Npcap] kernel-timestamped NTP transport ready: {} ({}) -> {}",
            device.name, local_ip, server_ip
        );

        Ok(Self {
            capture,
            socket,
            server_ip,
            local_ip,
        })
    }

    /// One kernel-timestamped NTP round trip: send a request, capture our
    /// own outgoing packet (t1) and the server's reply (t4), parse t2/t3
    /// from the reply payload, and compute offset/RTT.
    ///
    /// Adversarial-review fix (#53 continuation): before sending, DRAIN any
    /// packets already queued in the capture (a leftover reply from a prior
    /// `measure_once` call, or the userspace rsntp fallback's own request/
    /// reply exchange to the same server:123, which the open BPF filter also
    /// matches). And a captured reply is only accepted as t4 when its echoed
    /// Origin Timestamp actually matches THIS request's transmit timestamp
    /// (`reply_origin_matches_request`) -- without both of these, a stale or
    /// foreign reply could be silently paired with the current request,
    /// producing a self-consistent but wrong measurement (the exact fat-tail
    /// bug this fixes: healthy median, occasional multi-ms-wrong sample).
    pub fn measure_once(&mut self) -> Result<crate::ntp::RawSample> {
        use crate::ntp_packet::{
            build_client_request, compute_offset_rtt_us, is_unsynchronized_server_reply,
            parse_reply, parse_udp_frame, reply_origin_matches_request, systemtime_to_unix_micros,
        };

        // Drain anything already queued before this request exists -- a
        // stale reply/request sitting in the capture buffer must never be
        // considered for THIS round trip's t1/t4.
        const MAX_DRAIN_PACKETS: u32 = 64;
        for _ in 0..MAX_DRAIN_PACKETS {
            match self.capture.next_packet() {
                Ok(Some(_)) => {
                    debug!("[NTP][Npcap] drained a stale queued packet before sending");
                }
                Ok(None) => break,
                Err(_) => break, // any other capture error: nothing more to drain
            }
        }

        let request_transmit_ts_us = systemtime_to_unix_micros(SystemTime::now());
        let request = build_client_request(request_transmit_ts_us);
        self.socket.send(&request)?;

        let mut t1_us: Option<i64> = None;
        let mut t4_reply: Option<(i64, crate::ntp_packet::ParsedReply)> = None;
        let deadline = std::time::Instant::now() + NTP_PCAP_TIMEOUT;

        while t4_reply.is_none() && std::time::Instant::now() < deadline {
            match self.capture.next_packet() {
                Ok(Some(packet)) => {
                    let Some((src_ip, _dst_port, payload)) = parse_udp_frame(packet.data) else {
                        continue;
                    };
                    let ts =
                        pcap_ts_to_systemtime(packet.ts.tv_sec as i64, packet.ts.tv_usec as i64);
                    let ts_us = systemtime_to_unix_micros(ts);

                    if src_ip == self.local_ip && t1_us.is_none() {
                        t1_us = Some(ts_us);
                        debug!("[NTP][Npcap] t1 (our request) captured at {}us", ts_us);
                    } else if src_ip == self.server_ip {
                        match parse_reply(payload) {
                            Ok(reply) => {
                                if reply_origin_matches_request(
                                    reply.origin_ts_us,
                                    request_transmit_ts_us,
                                ) {
                                    t4_reply = Some((ts_us, reply));
                                    debug!(
                                        "[NTP][Npcap] t4 (server reply) captured at {}us",
                                        ts_us
                                    );
                                } else {
                                    debug!(
                                        "[NTP][Npcap] ignoring reply with mismatched origin \
                                         timestamp {}us (our request: {}us) -- stale or foreign \
                                         reply, not paired",
                                        reply.origin_ts_us, request_transmit_ts_us
                                    );
                                }
                            }
                            // dsync#134 re-review: the server says its clock is unsynchronized
                            // (LI = 3) — no reply will be usable; stop now instead of waiting out
                            // the timeout (5 samples would block the loop ~2.5 s).
                            Err(e) if is_unsynchronized_server_reply(payload) => {
                                return Err(anyhow!("NTP/Npcap: {}", e));
                            }
                            Err(e) => {
                                debug!(
                                    "[NTP][Npcap] ignoring unparseable server-port packet: {}",
                                    e
                                )
                            }
                        }
                    }
                }
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }

        let t1_us = t1_us.ok_or_else(|| {
            anyhow!(
                "NTP/Npcap: never observed our own outgoing request within {:?}",
                NTP_PCAP_TIMEOUT
            )
        })?;
        let (t4_us, reply) = t4_reply
            .ok_or_else(|| anyhow!("NTP/Npcap: no server reply within {:?}", NTP_PCAP_TIMEOUT))?;

        let (offset_us, rtt_us) =
            compute_offset_rtt_us(t1_us, reply.receive_ts_us, reply.transmit_ts_us, t4_us);

        Ok(crate::ntp::RawSample {
            offset_us,
            rtt_us: rtt_us.max(0) as u64,
        })
    }
}

#[cfg(test)]
mod tests;
