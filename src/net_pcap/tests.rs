use super::*;

#[test]
fn test_pcap_ntp_transport_new_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_pcap_ntp_transport_new_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = PcapNtpTransport::new(
        Ipv4Addr::new(127, 0, 0, 1),
        &crate::dscp::DscpConfig::default(),
    );
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok -- \
         this used to crash the whole process (#58)"
    );
}

#[test]
fn test_find_device_for_ntp_server_gracefully_errors_without_npcap_runtime() {
    if wpcap_runtime_available() {
        eprintln!(
            "skipping test_find_device_for_ntp_server_gracefully_errors_without_npcap_runtime: \
             Npcap runtime IS installed on this machine"
        );
        return;
    }
    let result = find_device_for_ntp_server(Ipv4Addr::new(10, 77, 9, 202));
    assert!(
        result.is_err(),
        "expected a graceful Err when the Npcap runtime is missing, got Ok -- \
         same #58 guard as find_device()"
    );
}
