// dsync#136: the PTP code (wire format, receive paths, servo, phase lock, lock gate, filters)
// lives in the shared `dsync-ptp` crate; re-exported so every `crate::ptp::…` path is unchanged.
pub mod atomic_file;
pub mod clock;
pub mod clock_alarm;
pub mod config;
pub mod controller;
pub mod date_offset;
pub mod date_step_trigger;
pub mod drift_file;
pub mod dscp;
pub use dsync_ptp::gm_filter;
pub mod http_status;
pub use dsync_ptp::lock_gate;
pub use dsync_ptp::net;
pub mod ntp;
pub mod ntp_burst;
pub mod ntp_packet;
pub mod ntp_server;
pub mod phase_slew;
pub mod pinned_wait;
pub use dsync_ptp::phase_window;
pub use dsync_ptp::ptp;
pub use dsync_ptp::ptp_phase_lock;
pub use dsync_ptp::ptp_rejoin;
pub use dsync_ptp::rate_servo;
pub use dsync_ptp::spike_filter;
pub mod status;
pub mod sync_loop;
pub mod time_server;
pub mod traits;
pub mod tray_view;

#[cfg(unix)]
pub use dsync_ptp::net_linux;

#[cfg(windows)]
pub mod net_pcap;

#[cfg(windows)]
pub mod net_winsock;
