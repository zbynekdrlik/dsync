//! Parsing an Ethernet + IPv4 + UDP frame as Npcap captures it: shared by the PTP capture
//! (`net_pcap`) and the service's pcap NTP transport (moved from the service's `ntp_packet`,
//! dsync#136; its tests stay there, through the service's re-export).

use std::net::Ipv4Addr;

/// Pure — takes raw captured bytes, no `pcap` crate types — so it is
/// testable without a capture device. Shared by the PTP (`net_pcap.rs`) and
/// NTP (dsync#53) Windows Npcap capture paths, which otherwise
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

/// dsync#200: a BPF filter that also matches its frames when they carry one 802.1Q tag.
pub fn vlan_aware_filter(filter: &str) -> String {
    filter.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ethernet(14) + IPv4(20) + UDP(8) + payload, with one 802.1Q tag (VLAN 70) after the MACs.
    fn tagged_udp_frame(src_ip: [u8; 4], dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; 46 + payload.len()];
        frame[12] = 0x81; // TPID 0x8100
        frame[13] = 0x00;
        frame[14] = 0x00; // PCP/DEI 0, VLAN id 70
        frame[15] = 70;
        frame[16] = 0x08; // inner EtherType IPv4
        frame[17] = 0x00;
        frame[27] = 17; // IP protocol = UDP
        frame[30..34].copy_from_slice(&src_ip);
        frame[40] = (dst_port >> 8) as u8;
        frame[41] = (dst_port & 0xFF) as u8;
        frame[46..].copy_from_slice(payload);
        frame
    }

    /// dsync#200 RED: Npcap on a Windows VLAN virtual adapter (dsg's `Dante70`) delivers
    /// every frame with its 802.1Q tag; the PTP Sync inside must still parse.
    #[test]
    fn parse_udp_frame_reads_a_frame_with_one_vlan_tag() {
        let frame = tagged_udp_frame([10, 77, 7, 198], 319, b"ptp-sync");
        let (src_ip, dst_port, payload) =
            parse_udp_frame(&frame).expect("a VLAN-tagged IPv4/UDP frame must parse");
        assert_eq!(src_ip, Ipv4Addr::new(10, 77, 7, 198));
        assert_eq!(dst_port, 319);
        assert_eq!(payload, b"ptp-sync");
    }

    /// dsync#200 RED: a tagged frame whose inner EtherType is not IPv4 is still rejected.
    #[test]
    fn parse_udp_frame_rejects_a_tagged_non_ipv4_frame() {
        let mut frame = tagged_udp_frame([10, 77, 7, 198], 319, b"x");
        frame[16] = 0x86;
        frame[17] = 0xDD;
        assert!(parse_udp_frame(&frame).is_none());
    }

    /// dsync#200 RED: a tagged frame too short for its headers is rejected, not read past.
    #[test]
    fn parse_udp_frame_rejects_a_short_tagged_frame() {
        let frame = tagged_udp_frame([10, 77, 7, 198], 319, b"");
        assert!(parse_udp_frame(&frame[..45]).is_none());
    }

    /// dsync#200 RED: the BPF `udp` primitive reads the EtherType at offset 12, which is 0x8100
    /// on a tagged frame; the filter must also try the `vlan`-shifted form.
    #[test]
    fn vlan_aware_filter_adds_the_vlan_branch() {
        assert_eq!(
            vlan_aware_filter("udp and dst host 224.0.1.129"),
            "(udp and dst host 224.0.1.129) or (vlan and udp and dst host 224.0.1.129)"
        );
    }
}
