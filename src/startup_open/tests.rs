//! dsync#206 — the unpinned startup open.

use super::*;
use anyhow::anyhow;

#[derive(Debug, PartialEq)]
enum Net {
    Open(&'static str),
    Unopened,
}

#[test]
fn an_open_that_works_is_used_and_nothing_is_reported_206() {
    let got = open_unpinned(|| Ok(Net::Open("eth0")), || Net::Unopened).expect("a start");
    assert_eq!(got.network, Net::Open("eth0"));
    assert_eq!(got.not_open, None);
}

#[test]
fn a_device_missing_at_boot_starts_unopened_and_names_it_never_ends_the_loop_206() {
    // mbc-snv's boot (19.11.2026): Npcap listed only the loopback device.
    let got = open_unpinned(
        || -> Result<Net> {
            Err(anyhow!(
                "Interface '{{CC545DEA-6E37-4FF6-A98E-0297BB71B85A}}' not found. Available: \
                 [\"\\\\Device\\\\NPF_Loopback\"]"
            ))
        },
        || Net::Unopened,
    )
    .expect("a missing device must never end the startup");
    assert_eq!(got.network, Net::Unopened);
    let why = got.not_open.expect("the reason is kept for the alarm");
    assert!(
        why.contains("CC545DEA-6E37-4FF6-A98E-0297BB71B85A"),
        "{why}"
    );
}
