//! dsync#136 — the run-time Npcap binding. CI's `windows-latest` has no Npcap runtime, so there
//! the test binary itself starting proves it imports no `wpcap.dll`, and every entry point must
//! return the typed `NpcapMissing` error.

use super::*;

#[test]
fn an_ipv4_sockaddr_reads_its_address_136() {
    // sockaddr_in after the family: port 319 (big-endian), 10.77.9.204, 8 zero bytes
    let data = [0x01, 0x3f, 10, 77, 9, 204, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        ip_from_sockaddr(AF_INET, &data),
        Some(IpAddr::V4(Ipv4Addr::new(10, 77, 9, 204)))
    );
}

#[test]
fn an_ipv6_sockaddr_reads_its_address_136() {
    let mut data = [0u8; 26];
    data[6..22].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
    assert_eq!(
        ip_from_sockaddr(AF_INET6, &data),
        Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
    );
}

#[test]
fn another_family_or_a_short_sockaddr_is_no_address_136() {
    assert_eq!(ip_from_sockaddr(17, &[0u8; 14]), None);
    assert_eq!(ip_from_sockaddr(AF_INET, &[0u8; 5]), None);
    assert_eq!(ip_from_sockaddr(AF_INET6, &[0u8; 21]), None);
}

#[test]
fn the_pcap_header_has_the_windows_layout_136() {
    // struct timeval is two 32-bit longs on Windows; then caplen and len.
    assert_eq!(std::mem::size_of::<Timeval>(), 8);
    assert_eq!(std::mem::size_of::<RawPkthdr>(), 16);
    assert_eq!(std::mem::size_of::<RawSockaddr>(), 16);
}

#[test]
fn without_npcap_every_entry_point_is_a_typed_error_and_with_it_the_devices_list_136() {
    match available() {
        Err(missing) => {
            assert!(!missing.detail.is_empty(), "the error names what failed");
            let err = devices().expect_err("no runtime: no device list");
            assert_eq!(err.downcast_ref::<NpcapMissing>(), Some(&missing));
            let dev = Device {
                name: r"\Device\NPF_{00000000-0000-0000-0000-000000000000}".into(),
                desc: None,
                addresses: Vec::new(),
            };
            let settings = CaptureSettings {
                snaplen: 256,
                promisc: false,
                immediate: true,
                timeout_ms: 1,
                host_hiprec: true,
            };
            let err = Capture::open(&dev, settings)
                .err()
                .expect("no runtime: no capture");
            assert!(err.downcast_ref::<NpcapMissing>().is_some());
        }
        Ok(()) => {
            // A PC with Npcap: the list loads, and every device has a name.
            let list = devices().expect("Npcap loaded: the device list works");
            assert!(list.iter().all(|d| !d.name.is_empty()));
        }
    }
}
