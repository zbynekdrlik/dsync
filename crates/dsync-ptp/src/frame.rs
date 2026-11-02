//! Parsing an Ethernet + IPv4 + UDP frame as Npcap captures it: shared by the PTP capture
//! (`net_pcap`) and the service's pcap NTP transport (moved from the service's `ntp_packet`,
//! dantesync#136; its tests stay there, through the service's re-export).

use std::net::Ipv4Addr;

/// Pure — takes raw captured bytes, no `pcap` crate types — so it is
/// testable without a capture device. Shared by the PTP (`net_pcap.rs`) and
/// NTP (dantesync#53) Windows Npcap capture paths, which otherwise
/// duplicated this exact parsing.
///
/// #53 GREEN: validates length, EtherType (IPv4 = 0x0800), and IP protocol
/// (UDP = 17) before extracting the real source IP, destination port, and
/// payload.
pub fn parse_udp_frame(data: &[u8]) -> Option<(Ipv4Addr, u16, &[u8])> {
    const ETH_IP_UDP_HEADER: usize = 42;
    if data.len() < ETH_IP_UDP_HEADER {
        return None;
    }
    if data[12] != 0x08 || data[13] != 0x00 {
        return None; // not IPv4
    }
    if data[23] != 17 {
        return None; // not UDP
    }
    let src_ip = Ipv4Addr::new(data[26], data[27], data[28], data[29]);
    let dst_port = ((data[36] as u16) << 8) | data[37] as u16;
    Some((src_ip, dst_port, &data[ETH_IP_UDP_HEADER..]))
}
