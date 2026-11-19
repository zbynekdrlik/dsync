use crate::net::{classify_pinned_with, NicSnapshot, PinnedInterfaceUnavailable, PinnedLookup};
use std::net::Ipv4Addr;

fn nic(names: &[&str], ipv4: &[Ipv4Addr]) -> NicSnapshot {
    NicSnapshot {
        names: names.iter().map(|n| n.to_string()).collect(),
        ipv4: ipv4.to_vec(),
    }
}

const CAM: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 133);
const WG: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 3);

/// cam3 on 19.11.2026: two onboard ports, the cable in whichever one the crew picked.
fn cable_in(port: &str) -> Vec<NicSnapshot> {
    vec![
        nic(&["wg0"], &[WG]),
        nic(&["enp1s0"], if port == "enp1s0" { &[CAM] } else { &[] }),
        nic(&["enp3s0"], if port == "enp3s0" { &[CAM] } else { &[] }),
    ]
}

#[test]
fn a_pinned_list_binds_whichever_listed_nic_has_the_ipv4_and_follows_a_move_212() {
    for spec in ["enp1s0,enp3s0", "enp3s0,enp1s0", " enp1s0 , enp3s0 "] {
        for port in ["enp1s0", "enp3s0"] {
            assert_eq!(
                classify_pinned_with(spec, &cable_in(port), false),
                PinnedLookup::Ready(CAM),
                "{spec:?} with the cable in {port}"
            );
        }
    }
    // A single name keeps today's meaning: the cable in the other port is not this pin's.
    assert_eq!(
        classify_pinned_with("enp3s0", &cable_in("enp1s0"), false),
        PinnedLookup::NoIpv4
    );
}

#[test]
fn a_pinned_list_never_binds_an_unlisted_nic_212() {
    // Both ports without an address: wg0 has one, and it is never taken.
    assert_eq!(
        classify_pinned_with("enp1s0,enp3s0", &cable_in("none"), false),
        PinnedLookup::NoIpv4
    );
    // One listed name does not exist on this box: the other still counts.
    assert_eq!(
        classify_pinned_with("enp9s0,enp3s0", &cable_in("enp3s0"), false),
        PinnedLookup::Ready(CAM)
    );
    assert_eq!(
        classify_pinned_with("enp9s0,eth7", &cable_in("enp3s0"), false),
        PinnedLookup::Missing
    );
}

#[test]
fn a_routable_listed_nic_beats_a_link_local_one_212() {
    let ll = Ipv4Addr::new(169, 254, 3, 7);
    let nics = [nic(&["enp1s0"], &[ll]), nic(&["enp3s0"], &[CAM])];
    assert_eq!(
        classify_pinned_with("enp1s0,enp3s0", &nics, false),
        PinnedLookup::Ready(CAM)
    );
    let only_ll = [nic(&["enp1s0"], &[ll]), nic(&["enp3s0"], &[])];
    assert_eq!(
        classify_pinned_with("enp1s0,enp3s0", &only_ll, false),
        PinnedLookup::LinkLocalOnly(ll)
    );
}

#[test]
fn a_windows_list_takes_friendly_names_and_guids_212() {
    let nics = [
        nic(&["Ethernet", "{AAAA-1111}"], &[]),
        nic(&["Ethernet 2", "{BBBB-2222}"], &[CAM]),
    ];
    for spec in [
        "Ethernet,Ethernet 2",
        "ethernet, {bbbb-2222}",
        "{AAAA-1111},Ethernet 2",
    ] {
        assert_eq!(
            classify_pinned_with(spec, &nics, true),
            PinnedLookup::Ready(CAM),
            "{spec:?}"
        );
    }
}

#[test]
fn the_alarm_names_the_whole_list_when_none_is_usable_212() {
    let none = PinnedInterfaceUnavailable {
        name: "enp1s0,enp3s0".to_string(),
        missing: false,
    };
    assert_eq!(
        none.alarm_reason(),
        "none of the PTP interfaces enp1s0,enp3s0 has an IPv4 address"
    );
    let gone = PinnedInterfaceUnavailable {
        name: "enp1s0,enp3s0".to_string(),
        missing: true,
    };
    assert_eq!(
        gone.alarm_reason(),
        "none of the PTP interfaces enp1s0,enp3s0 exists"
    );
    // A single name: the #131 wording, unchanged.
    let one = PinnedInterfaceUnavailable {
        name: "enp3s0".to_string(),
        missing: false,
    };
    assert_eq!(
        one.alarm_reason(),
        "PTP interface enp3s0 has no IPv4 address"
    );
}
