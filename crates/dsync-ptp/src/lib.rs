//! # dsync-ptp — the Dante PTPv1 code of dsync, shared by the service and in-process followers
//!
//! dantesync#136 (owner ruling 2026-11-02, "build it"): dsync is the single maintainer of the Dante
//! PTP code. The `dantesync` service (which disciplines the whole PC's clock) and every application
//! that locks to Dante PTP in its own process use THIS crate, so a fix lands once for both.
//!
//! - The PTP code the service runs: the PTPv1 wire format ([`ptp`]), the receive paths ([`net`],
//!   `net_linux` on unix, `net_pcap` on Windows, behind [`network::PtpNetwork`]), the rate servo
//!   ([`rate_servo`]), the phase lock ([`ptp_phase_lock`]), the DSYX [41] lock gate
//!   ([`lock_gate`]), the spike filter, the grandmaster allowlist and the re-join schedule.
//! - The in-process follower ([`follower`]): the same pipeline on a VIRTUAL clock over the PC's raw
//!   monotonic counter. It never touches the system clock and never sends; several followers and
//!   the service share UDP 319/320 on one PC. [`follower::PtpClock::now_ptp_ns`] is a lock-free
//!   read for a real-time thread.
//!
//! Consumers pin it by the service's release tag:
//! `dsync-ptp = { git = "https://github.com/zbynekdrlik/dsync", tag = "v1.24.0" }`.

pub mod follower;
pub mod frame;
pub mod gm_filter;
pub mod lock_gate;
pub mod mono;
pub mod net;
pub mod network;
pub mod ptp;
pub mod ptp_phase_lock;
pub mod ptp_rejoin;
pub mod ptp_tx;
pub mod rate_servo;
pub mod seqlock;
pub mod spike_filter;

#[cfg(unix)]
pub mod net_linux;

#[cfg(windows)]
pub mod net_pcap;

/// This crate's version: always the dantesync service version it was released with.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
