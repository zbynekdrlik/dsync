//! dsync#164 — when the service tells systemd it is READY.
//!
//! A unit ordered `After=dsync.service` under `Type=notify` starts only once dsync sends
//! `READY=1`. dstack (strih-pp, 9.11.2026) needs that to mean "the wall is placed": a boot step
//! landing under an already running OBS moved its wall by −2.2 s. So READY waits for the wall's
//! placement ([`crate::controller::WallPlacement`]), bounded by [`READY_PLACEMENT_BOUND`]: a dead
//! NTP upstream or a missing grandmaster must never hold the dependent units for ever. Pure (time
//! is injected); `main` sends the notification.

use std::time::{Duration, Instant};

/// The longest READY waits for the wall's placement (the boot NTP reading itself gives up after
/// `BOOT_NTP_WAIT`, 15 s; a saved state waits for its first PTP window).
pub const READY_PLACEMENT_BOUND: Duration = Duration::from_secs(30);

/// Why READY was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyReason {
    /// The wall is placed: how.
    Placed(String),
    /// Not placed within [`READY_PLACEMENT_BOUND`]: what was still pending.
    TimedOut(String),
}

impl ReadyReason {
    /// The systemd `STATUS=` text.
    pub fn status_text(&self) -> String {
        match self {
            ReadyReason::Placed(how) => format!("wall placed ({how})"),
            ReadyReason::TimedOut(what) => format!(
                "READY without a placed wall after {} s: {what}",
                READY_PLACEMENT_BOUND.as_secs()
            ),
        }
    }
}

/// Sends READY once: see the module documentation.
#[derive(Debug)]
pub struct ReadyGate {
    started: Instant,
    sent: bool,
}

impl ReadyGate {
    pub fn new(started: Instant) -> Self {
        ReadyGate {
            started,
            sent: false,
        }
    }

    /// Every loop iteration: `Ok(how)` = the wall is placed, `Err(what)` = still pending. Returns
    /// the reason exactly once, when READY is to be sent.
    pub fn poll(&mut self, now: Instant, placement: Result<&str, &str>) -> Option<ReadyReason> {
        let _ = (now, &self.started);
        if self.sent {
            return None;
        }
        self.sent = true;
        Some(match placement {
            Ok(how) => ReadyReason::Placed(how.to_string()),
            Err(what) => ReadyReason::TimedOut(what.to_string()),
        })
    }

    pub fn sent(&self) -> bool {
        self.sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strih-pp, 9.11.2026: READY must not be sent while the boot step is still to come.
    #[test]
    fn ready_waits_for_the_placed_wall_164() {
        let t0 = Instant::now();
        let mut g = ReadyGate::new(t0);
        for s in [0u64, 1, 5, 17] {
            assert_eq!(
                g.poll(t0 + Duration::from_secs(s), Err("boot NTP reading pending")),
                None,
                "{s} s"
            );
        }
        assert!(!g.sent());
        assert_eq!(
            g.poll(t0 + Duration::from_secs(18), Ok("boot step applied")),
            Some(ReadyReason::Placed("boot step applied".into()))
        );
        assert!(g.sent());
        assert_eq!(g.poll(t0 + Duration::from_secs(19), Ok("x")), None, "once");
    }

    /// A dead upstream never holds the dependent units for ever.
    #[test]
    fn ready_is_sent_at_the_bound_with_what_is_pending_164() {
        let t0 = Instant::now();
        let mut g = ReadyGate::new(t0);
        let pending = Err("a saved fleet date offset waits for its first PTP window");
        assert_eq!(
            g.poll(
                t0 + READY_PLACEMENT_BOUND - Duration::from_millis(1),
                pending
            ),
            None
        );
        let r = g
            .poll(t0 + READY_PLACEMENT_BOUND, pending)
            .expect("at the bound");
        assert!(matches!(&r, ReadyReason::TimedOut(w) if w.contains("first PTP window")));
        assert!(r.status_text().contains("without a placed wall"), "{r:?}");
        assert_eq!(g.poll(t0 + READY_PLACEMENT_BOUND * 2, Ok("x")), None);
    }

    #[test]
    fn a_wall_placed_at_once_is_ready_at_once_164() {
        let t0 = Instant::now();
        let mut g = ReadyGate::new(t0);
        let r = g.poll(t0, Ok("no boot step needed")).expect("ready");
        assert_eq!(r.status_text(), "wall placed (no boot step needed)");
    }
}
