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

#[test]
fn the_spec_is_canonical_and_a_blank_one_is_unset_212() {
    use crate::net::{canonical_interface_spec, is_interface_list};
    assert_eq!(
        canonical_interface_spec(" enp1s0 , enp3s0 ,").as_deref(),
        Some("enp1s0,enp3s0")
    );
    assert_eq!(
        canonical_interface_spec("Ethernet 2").as_deref(),
        Some("Ethernet 2")
    );
    assert_eq!(canonical_interface_spec(" , "), None);
    assert_eq!(canonical_interface_spec(""), None);
    assert!(is_interface_list("enp1s0,enp3s0"));
    assert!(!is_interface_list("enp3s0"));
    assert!(!is_interface_list("enp3s0,"));
}

#[test]
fn a_stale_rejoin_moves_on_to_the_next_usable_listed_nic_212() {
    use crate::net::{choose_pinned, PinnedChoice, PinnedOrder};
    let other = Ipv4Addr::new(10, 77, 9, 134);
    let both = |name: &str| match name {
        "enp1s0" => PinnedLookup::Ready(other),
        "enp3s0" => PinnedLookup::Ready(CAM),
        _ => PinnedLookup::Missing,
    };
    let at = |order| choose_pinned("enp1s0,enp3s0", order, both);
    let ready = |name: &str, ip| PinnedChoice {
        name: name.to_string(),
        lookup: PinnedLookup::Ready(ip),
    };
    assert_eq!(
        at(PinnedOrder::Startup),
        ready("enp1s0", other),
        "the list order"
    );
    assert_eq!(at(PinnedOrder::MoveOn("enp1s0")), ready("enp3s0", CAM));
    assert_eq!(
        at(PinnedOrder::MoveOn("enp3s0")),
        ready("enp1s0", other),
        "wraps round"
    );
    assert_eq!(
        at(PinnedOrder::MoveOn("wg0")),
        ready("enp1s0", other),
        "an unlisted NIC is no position"
    );
    // A single name: always that one, today's #131 pin.
    for order in [
        PinnedOrder::Startup,
        PinnedOrder::Stay("enp3s0"),
        PinnedOrder::MoveOn("enp3s0"),
    ] {
        assert_eq!(choose_pinned("enp3s0", order, both), ready("enp3s0", CAM));
    }
    // Only listed names are ever looked at.
    let mut looked = Vec::new();
    choose_pinned("enp1s0,enp3s0", PinnedOrder::Startup, |n| {
        looked.push(n.to_string());
        PinnedLookup::NoIpv4
    });
    assert_eq!(looked, vec!["enp1s0", "enp3s0"]);
}

/// Review round 1: the cable is in a port on a segment without DHCP (link-local) and the other
/// port keeps a static address without carrier. "Routable first" would hold the dead port for
/// ever; a stale re-join takes the NEXT usable listed NIC, link-local or not.
#[test]
fn a_stale_rejoin_reaches_a_link_local_port_past_a_routable_dead_one_212() {
    use crate::net::{choose_pinned, PinnedOrder};
    let ll = Ipv4Addr::new(169, 254, 3, 7);
    let look = |name: &str| match name {
        "enp1s0" => PinnedLookup::Ready(CAM),
        "enp3s0" => PinnedLookup::LinkLocalOnly(ll),
        _ => PinnedLookup::Missing,
    };
    let silent_on_1 = choose_pinned("enp1s0,enp3s0", PinnedOrder::MoveOn("enp1s0"), look);
    assert_eq!(
        (silent_on_1.name.as_str(), silent_on_1.lookup),
        ("enp3s0", PinnedLookup::LinkLocalOnly(ll))
    );
    // Nothing usable after a move: the list is named.
    let none = choose_pinned("enp1s0,enp3s0", PinnedOrder::MoveOn("enp1s0"), |_| {
        PinnedLookup::NoIpv4
    });
    assert_eq!(
        (none.name.as_str(), none.lookup),
        ("enp1s0,enp3s0", PinnedLookup::NoIpv4)
    );
}

/// Review round 1: PTP still flows on the joined NIC (the #191 link-local leave): the re-join
/// stays on it while it is usable, never hops to another listed port.
#[test]
fn a_rejoin_while_ptp_flows_stays_on_the_joined_listed_nic_212() {
    use crate::net::{choose_pinned, rejoin_order, PinnedOrder};
    let look = |name: &str| match name {
        "enp1s0" => PinnedLookup::Ready(Ipv4Addr::new(10, 77, 9, 134)),
        "enp3s0" => PinnedLookup::Ready(CAM),
        _ => PinnedLookup::Missing,
    };
    let stay = choose_pinned("enp1s0,enp3s0", PinnedOrder::Stay("enp3s0"), look);
    assert_eq!(
        (stay.name.as_str(), stay.lookup),
        ("enp3s0", PinnedLookup::Ready(CAM))
    );
    // The joined NIC lost its address: the startup order.
    let gone = choose_pinned("enp1s0,enp3s0", PinnedOrder::Stay("enp3s0"), |n| {
        if n == "enp1s0" {
            PinnedLookup::Ready(CAM)
        } else {
            PinnedLookup::NoIpv4
        }
    });
    assert_eq!(gone.name, "enp1s0");

    // dsync#215: the caller's judgement of the TRUSTED grandmaster decides, never a count of
    // every time message (a foreign grandmaster on the joined port).
    use crate::network::TrustedPtp;
    assert_eq!(
        rejoin_order("enp3s0", TrustedPtp::Flowing),
        PinnedOrder::Stay("enp3s0")
    );
    assert_eq!(
        rejoin_order("enp3s0", TrustedPtp::Silent),
        PinnedOrder::MoveOn("enp3s0")
    );
}

#[test]
fn an_unusable_choice_is_the_typed_error_naming_the_list_212() {
    use crate::net::{choose_pinned, PinnedChoice, PinnedOrder};
    let none = choose_pinned("enp1s0,enp3s0", PinnedOrder::Startup, |_| {
        PinnedLookup::NoIpv4
    });
    assert_eq!(
        none.clone().usable(),
        Err(PinnedInterfaceUnavailable {
            name: "enp1s0,enp3s0".to_string(),
            missing: false
        })
    );
    let gone = choose_pinned("enp1s0,enp3s0", PinnedOrder::Startup, |_| {
        PinnedLookup::Missing
    });
    assert_eq!(gone.usable().map_err(|e| e.missing), Err(true));
    let e: anyhow::Error = none.usable().expect_err("unusable").into();
    assert_eq!(
        e.to_string(),
        "pinned list: none of the PTP interfaces enp1s0,enp3s0 has an IPv4 address -- \
         waiting for one of them, never another NIC"
    );
    let ll = Ipv4Addr::new(169, 254, 1, 1);
    assert_eq!(
        PinnedChoice {
            name: "enp1s0".to_string(),
            lookup: PinnedLookup::LinkLocalOnly(ll),
        }
        .usable(),
        Ok(("enp1s0".to_string(), ll)),
        "a re-join takes the NIC's link-local address"
    );
}
