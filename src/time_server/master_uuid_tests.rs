//! dsync#150 — the PTP master UUID in the fixed 120-byte DSYX reply (Dante Controller's clock
//! stats need the follower's master and grandmaster; dstack#1).

use super::tests::master_status;
use super::*;

const GM: [u8; 6] = [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c];
/// A boundary clock relaying the grandmaster: the master differs from the grandmaster.
const MASTER: [u8; 6] = [0x00, 0x1d, 0xc1, 0x99, 0x98, 0x97];

#[test]
fn the_dsyx_reply_carries_the_master_uuid_after_the_date_extension_150() {
    let mut status = master_status();
    status.ptp_source_uuid = Some(MASTER);
    let reply = build_response_ext(5, &status);
    assert_eq!(reply.len(), DSYX_REPLY_SIZE);
    assert_eq!(DSYX_REPLY_SIZE, 120);
    assert_eq!(MASTER_UUID_RANGE, 112..118);
    assert_eq!(
        &reply[112..118],
        &MASTER,
        "master = the followed Sync's sourceUuid"
    );
    assert_eq!(
        &reply[42..48],
        &GM,
        "the grandmaster stays at the base [42-47]"
    );
    assert_eq!(&reply[118..120], &[0, 0], "reserved");
    let parsed = parse_reply(&reply, 5, Instant::now(), 0).expect("valid");
    assert_eq!(
        parsed
            .ext
            .expect("date extension still decodes")
            .announce
            .seq,
        3
    );
}

#[test]
fn a_node_without_date_state_still_reports_its_master_with_a_zero_extension_slot_150() {
    // Cold boot, legacy discipline, a pending re-anchor: no date state, but Dante Controller
    // still needs the master. The slot is 48 zero bytes, version 0 = no extension for every
    // reader since dsync#88, so nobody reads D = 0 from it.
    let mut status = SyncStatus {
        gm_uuid: Some(GM),
        ptp_source_uuid: Some(MASTER),
        ..SyncStatus::default()
    };
    status.is_locked = false;
    let reply = build_response_ext(9, &status);
    assert_eq!(reply.len(), DSYX_REPLY_SIZE);
    assert!(
        reply[64..112].iter().all(|&b| b == 0),
        "empty extension slot"
    );
    assert_eq!(&reply[112..118], &MASTER);
    let parsed = parse_reply(&reply, 9, Instant::now(), 0).expect("valid");
    assert_eq!(parsed.ext, None, "version 0 = no extension");
    assert_eq!(parsed.gm_uuid, Some(GM));
}

#[test]
fn no_sync_followed_yet_reads_as_a_zero_master_150() {
    let reply = build_response_ext(1, &SyncStatus::default());
    assert_eq!(reply.len(), DSYX_REPLY_SIZE);
    assert!(reply[112..120].iter().all(|&b| b == 0));
}

#[test]
fn a_dsyn_reply_stays_the_same_64_bytes_150() {
    let mut status = master_status();
    status.ptp_source_uuid = Some(MASTER);
    let base = build_response(2, &status);
    assert_eq!(base.len(), RESPONSE_SIZE);
    assert!(
        !base.windows(6).any(|w| w == MASTER),
        "the master UUID is DSYX-only"
    );
}
