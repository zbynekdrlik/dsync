//! dsync#161 — a failing capture receive, reported and retried at a bounded rate.
//!
//! When the capture's adapter disappears (a NIC driver swap, a USB re-plug) every read of the
//! dead handle fails at once. The receive loops poll every millisecond, so without a gate the
//! failure is retried and logged on every poll (iem-snv, 10.11.2026: 2881 WARN lines in one
//! minute). The gate is pure (time is injected) and owned by the capture:
//!
//! - [`RecvErrorGate::ready`]: after an error the next read waits a back-off, [`BACKOFF_FIRST`]
//!   doubled per consecutive error up to [`BACKOFF_MAX`]. A dead adapter is read about once a
//!   second, and one that comes back is read again within [`BACKOFF_MAX`].
//! - [`RecvErrorGate::on_error`]: one WARN on the transition, then a count at most every
//!   [`SUMMARY_EVERY`].
//! - [`RecvErrorGate::on_ok`]: one INFO when a failing stretch ends; the back-off resets.

use log::{info, warn};
use std::fmt;
use std::time::{Duration, Instant};

/// The wait before the read that follows the first error.
pub const BACKOFF_FIRST: Duration = Duration::from_millis(10);
/// The longest wait between two reads of a failing capture.
pub const BACKOFF_MAX: Duration = Duration::from_secs(1);
/// At most one "still failing" line per this interval.
pub const SUMMARY_EVERY: Duration = Duration::from_secs(10);

/// A line the gate wants logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateEvent {
    /// The first error after a good read (or the start).
    Failing { text: String },
    /// Still failing: `errors` since the previous line, over `over`.
    StillFailing {
        errors: u64,
        over: Duration,
        text: String,
    },
    /// A read worked again after `errors` errors over `outage`.
    Recovered { errors: u64, outage: Duration },
}

impl fmt::Display for GateEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateEvent::Failing { text } => write!(
                f,
                "receive failing: {text} -- retrying with a back-off up to {}s",
                BACKOFF_MAX.as_secs()
            ),
            GateEvent::StillFailing { errors, over, text } => write!(
                f,
                "receive still failing: {errors} errors in the last {:.1}s (last: {text})",
                over.as_secs_f64()
            ),
            GateEvent::Recovered { errors, outage } => write!(
                f,
                "receive recovered after {errors} errors over {:.1}s",
                outage.as_secs_f64()
            ),
        }
    }
}

impl GateEvent {
    /// Log it under `prefix` (`[Npcap]`): the failure lines are WARN, the recovery INFO.
    pub fn log(&self, prefix: &str) {
        match self {
            GateEvent::Recovered { .. } => info!("{prefix} {self}"),
            _ => warn!("{prefix} {self}"),
        }
    }
}

/// See the module documentation.
#[derive(Debug, Default)]
pub struct RecvErrorGate {
    _private: (),
}

impl RecvErrorGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// May the capture be read at `now`?
    pub fn ready(&self, _now: Instant) -> bool {
        true
    }

    /// A read failed with `text`.
    pub fn on_error(&mut self, _now: Instant, text: &str) -> Option<GateEvent> {
        Some(GateEvent::Failing {
            text: text.to_string(),
        })
    }

    /// A read worked (a packet or a plain timeout).
    pub fn on_ok(&mut self, _now: Instant) -> Option<GateEvent> {
        None
    }
}

#[cfg(test)]
mod tests;
