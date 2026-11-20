//! The `dsync` service binary's own modules (dsync#214), split out of `main.rs`. Binary-only:
//! the library (`src/lib.rs`) never declares them.

pub(crate) mod ptp_join;
pub(crate) mod singleton;
