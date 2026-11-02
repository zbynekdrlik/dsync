use crate::ntp::NtpMeasurement;
use anyhow::Result;

#[cfg_attr(test, mockall::automock)]
pub trait NtpSource {
    /// dantesync#53: returns a burst-filtered `NtpMeasurement` (offset/sign
    /// plus quality fields), not a single raw round trip.
    fn get_offset(&self) -> Result<NtpMeasurement>;
}

/// dantesync#136 — a source shared with the `ntp-burst` thread (`Arc<NtpClient>` in `main`).
impl<T: NtpSource + ?Sized> NtpSource for std::sync::Arc<T> {
    fn get_offset(&self) -> Result<NtpMeasurement> {
        (**self).get_offset()
    }
}

// dsync#136: the PTP receive path lives in the shared `dsync-ptp` crate.
#[cfg(test)]
pub use dsync_ptp::network::MockPtpNetwork;
pub use dsync_ptp::network::{PtpNetwork, RejoinOutcome};
