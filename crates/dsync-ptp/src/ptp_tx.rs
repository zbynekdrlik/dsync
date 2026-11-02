//! Encoding a PTPv1 Sync and Follow_Up as the audio network's grandmaster sends them (dsync#136): for
//! simulations and the acceptance test's fake grandmaster. The follower and the service NEVER send
//! (one-way model); nothing in the receive path uses this module.
//!
//! The layout is the one [`crate::ptp`] parses and `tests/fixtures/ptpv1/` proves on real grandmaster
//! bytes: a 40-byte header, a 124-byte Sync, a 52-byte Follow_Up.

/// What identifies the sender and the grandmaster in each message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sender {
    pub source_uuid: [u8; 6],
    pub grandmaster_uuid: [u8; 6],
}

const SYNC_LEN: usize = 124;
const FOLLOW_UP_LEN: usize = 52;

fn header(buf: &mut [u8], sender: &Sender, sequence_id: u16, message_type: u8, control: u8) {
    buf[0..2].copy_from_slice(&1u16.to_be_bytes()); // versionPTP
    buf[2..4].copy_from_slice(&1u16.to_be_bytes()); // versionNetwork
    buf[4..9].copy_from_slice(b"_DFLT");
    buf[20] = message_type;
    buf[21] = 1; // sourceCommunicationTechnology: Ethernet
    buf[22..28].copy_from_slice(&sender.source_uuid);
    buf[28..30].copy_from_slice(&1u16.to_be_bytes()); // sourcePortId
    buf[30..32].copy_from_slice(&sequence_id.to_be_bytes());
    buf[32] = control;
}

fn timestamp(buf: &mut [u8], ns: i64) {
    let ns = ns.max(0);
    buf[0..4].copy_from_slice(&((ns / 1_000_000_000) as u32).to_be_bytes());
    buf[4..8].copy_from_slice(&((ns % 1_000_000_000) as u32).to_be_bytes());
}

/// A Sync (control 0). Its origin timestamp is the coarse send time; the precise one follows.
pub fn sync(sender: &Sender, sequence_id: u16, origin_ns: i64) -> Vec<u8> {
    let mut b = vec![0u8; SYNC_LEN];
    header(&mut b, sender, sequence_id, 1, 0);
    timestamp(&mut b[40..48], origin_ns);
    b[53] = 1; // grandmasterCommunicationTechnology
    b[54..60].copy_from_slice(&sender.grandmaster_uuid);
    b[60..62].copy_from_slice(&1u16.to_be_bytes());
    b[62..64].copy_from_slice(&sequence_id.to_be_bytes());
    b[67] = 4; // grandmasterClockStratum
    b[68..72].copy_from_slice(b"DFLT");
    b[83] = (-2i8) as u8; // syncInterval: 250 ms
    b
}

/// The Follow_Up (control 2) carrying `precise_origin_ns`, the Sync's precise send time.
pub fn follow_up(sender: &Sender, sequence_id: u16, precise_origin_ns: i64) -> Vec<u8> {
    let mut b = vec![0u8; FOLLOW_UP_LEN];
    header(&mut b, sender, sequence_id, 2, 2);
    b[42..44].copy_from_slice(&sequence_id.to_be_bytes());
    timestamp(&mut b[44..52], precise_origin_ns);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::{
        is_time_message, PtpV1Control, PtpV1FollowUpBody, PtpV1Header, PtpV1SyncMessageBody,
    };

    const SENDER: Sender = Sender {
        source_uuid: [0, 0x1d, 0xc1, 1, 2, 3],
        grandmaster_uuid: [0, 0x1d, 0xc1, 9, 8, 7],
    };

    #[test]
    fn an_encoded_sync_parses_as_the_service_reads_a_real_one() {
        let b = sync(&SENDER, 513, 12_345_678_901);
        assert_eq!(b.len(), 124, "a real Sync is 124 bytes");
        assert!(is_time_message(&b));
        let h = PtpV1Header::parse(&b).unwrap();
        assert_eq!(h.message_type, PtpV1Control::Sync);
        assert_eq!(h.version_ptp, 1);
        assert_eq!(h.subdomain_name(), "_DFLT");
        assert_eq!(h.source_uuid, SENDER.source_uuid);
        assert_eq!(h.sequence_id, 513);
        let body = PtpV1SyncMessageBody::parse(&b[PtpV1Header::SIZE..]).unwrap();
        assert_eq!(body.grandmaster_clock_uuid, SENDER.grandmaster_uuid);
        assert_eq!(body.grandmaster_identifier_name(), "DFLT");
    }

    #[test]
    fn an_encoded_follow_up_carries_its_sequence_and_precise_time() {
        let b = follow_up(&SENDER, 513, 12_345_678_901);
        assert_eq!(b.len(), 52, "a real Follow_Up is 52 bytes");
        let h = PtpV1Header::parse(&b).unwrap();
        assert_eq!(h.message_type, PtpV1Control::FollowUp);
        let body = PtpV1FollowUpBody::parse(&b[PtpV1Header::SIZE..]).unwrap();
        assert_eq!(body.associated_sequence_id, 513);
        assert_eq!(body.precise_origin_timestamp.to_nanos(), 12_345_678_901);
    }

    /// The real Sync of the fixture has the same layout: our encoder's fields sit where the
    /// captured packet's do.
    #[test]
    fn the_encoder_matches_the_real_sync_fixture_layout() {
        let real: &[u8] = include_bytes!("../tests/fixtures/ptpv1/dante-sync.bin");
        let ours = sync(&SENDER, 1, 0);
        assert_eq!(real.len(), ours.len());
        assert_eq!(&real[0..4], &ours[0..4], "versions");
        assert_eq!(&real[4..9], &ours[4..9], "subdomain");
        assert_eq!(real[32], ours[32], "control");
        assert_eq!(&real[68..72], &ours[68..72], "grandmaster identifier");
        assert_eq!(real[83], ours[83], "syncInterval");
    }
}
