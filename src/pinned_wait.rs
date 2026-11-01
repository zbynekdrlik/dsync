//! dantesync#131 — startup with a PINNED PTP interface: wait for exactly that NIC, open the PTP
//! receive path on it, and never on another NIC.
//!
//! The owner's ruling (1.11.2026): the interface PTP binds to is an explicit setting. stream-snv
//! ran a whole production without the Dante clock because a USB NIC got its address first at
//! boot. With `system.interface` set, startup loops here until the pinned NIC has a usable IPv4
//! AND the receive path opens on it (on Windows the Npcap device list can lag the adapter list
//! at boot, so an open failure is waited out too, never fatal). Meanwhile the controller — which
//! normally drives the clock alarm — does not exist yet, so the alarm is driven here: a MISSING
//! NIC alarms at once, a NIC without an IPv4 or one that does not open yet after
//! [`net::PINNED_STARTUP_GRACE`]. Once open, the alarm REGAINED edge is logged / notified and the
//! `/status` snapshot cleared; the controller takes over from there.
//!
//! Every OS boundary (the lookup, the open, the pause, the clock) is injected, so the loop is
//! unit-tested.

use crate::clock_alarm::{self, ClockAlarm, ClockAlarmNotifier, ClockHealth};
use crate::net::{self, PinnedLookup};
use crate::status::SyncStatus;
use anyhow::Result;
use log::{error, info, warn};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// What the startup sees of the pinned NIC on one look.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Lookup(PinnedLookup),
    OpenFailed(String),
}

/// The injected boundaries and the alarm context of one wait.
pub struct PinnedWait<'a, N: ClockAlarmNotifier> {
    /// The pinned interface name.
    pub name: &'a str,
    /// `/status`, where the pin and the alarm are published.
    pub status: &'a RwLock<SyncStatus>,
    /// The configured alarm cadence (`system.clock_alarm_interval_s`, floored by the alarm).
    pub alarm_interval_s: u64,
    /// `ConfigState::alarm_reason()` (#116): a config fallback stays in the alarm's reasons.
    pub config_problem: Option<String>,
    /// The desktop notifier (`clock_alarm::DesktopNotifier` in production).
    pub notifier: N,
    /// Cleared when the service stops: the wait gives up (`None`).
    pub running: &'a AtomicBool,
}

impl<N: ClockAlarmNotifier> PinnedWait<'_, N> {
    /// Look, open, alarm, pause — until the receive path is open on the pinned NIC (`Some((the
    /// opened path, its IPv4))`) or the service stops (`None`).
    pub fn run<T>(
        self,
        mut lookup: impl FnMut() -> PinnedLookup,
        mut open: impl FnMut(Ipv4Addr) -> Result<T>,
        mut pause: impl FnMut(),
        mut now: impl FnMut() -> Instant,
    ) -> Option<(T, Ipv4Addr)> {
        let name = self.name;
        if let Ok(mut s) = self.status.write() {
            s.ptp_interface_configured = Some(name.to_string());
        }
        let started = now();
        let mut alarm = ClockAlarm::from_interval_secs(self.alarm_interval_s);
        let mut last: Option<Seen> = None;
        loop {
            let at = now();
            let waited = at.saturating_duration_since(started);
            let lookup = lookup();
            let (seen, problem) = match net::usable_pinned_ip(lookup, waited) {
                Some(ip) => match open(ip) {
                    Ok(opened) => {
                        info!(
                            "[NET] pinned PTP interface {} ({}) is open after {:.1} s",
                            name,
                            ip,
                            waited.as_secs_f64()
                        );
                        self.clear_alarm(&mut alarm, at);
                        return Some((opened, ip));
                    }
                    Err(e) => {
                        let text = format!("{e:#}");
                        let problem = (waited > net::PINNED_STARTUP_GRACE)
                            .then(|| format!("PTP interface {name} could not be joined: {text}"));
                        (Seen::OpenFailed(text), problem)
                    }
                },
                None => (
                    Seen::Lookup(lookup),
                    net::startup_wait_problem(name, lookup, waited).map(|p| p.alarm_reason()),
                ),
            };
            if last.as_ref() != Some(&seen) {
                log_seen(name, &seen);
                last = Some(seen);
            }
            if let Some(reason) = problem {
                self.raise_alarm(&mut alarm, reason, at);
            }
            if !self.running.load(Ordering::SeqCst) {
                return None;
            }
            pause();
        }
    }

    fn health(&self, interface_problem: Option<String>) -> ClockHealth {
        ClockHealth {
            is_locked: false,
            mode_locked: false,
            gm_allowed: false,
            ptp_stale: true,
            allowlist_unresolvable: None,
            in_acquisition: false,
            interface_problem,
            config_problem: self.config_problem.clone(),
        }
    }

    fn raise_alarm(&self, alarm: &mut ClockAlarm, reason: String, at: Instant) {
        let tick = clock_alarm::drive(
            alarm,
            &self.health(Some(reason)),
            at,
            epoch_s(),
            &self.notifier,
        );
        self.publish(tick.snapshot);
    }

    /// The pinned NIC is open: a raised alarm ends with its REGAINED edge (log + notifier) and a
    /// clear `/status` snapshot; the controller judges the clock from here on.
    fn clear_alarm(&self, alarm: &mut ClockAlarm, at: Instant) {
        if !alarm.is_active() {
            return;
        }
        let healthy = ClockHealth {
            is_locked: true,
            mode_locked: true,
            gm_allowed: true,
            ptp_stale: false,
            allowlist_unresolvable: None,
            in_acquisition: false,
            interface_problem: None,
            config_problem: None,
        };
        let tick = clock_alarm::drive(alarm, &healthy, at, epoch_s(), &self.notifier);
        self.publish(tick.snapshot);
    }

    fn publish(&self, snapshot: clock_alarm::ClockAlarmStatus) {
        if let Ok(mut s) = self.status.write() {
            s.clock_alarm = snapshot;
            s.clock_alarm_interval_s = self
                .alarm_interval_s
                .max(clock_alarm::CLOCK_ALARM_INTERVAL_FLOOR_S);
        }
    }
}

fn log_seen(name: &str, seen: &Seen) {
    match seen {
        Seen::Lookup(PinnedLookup::Ready(ip)) | Seen::Lookup(PinnedLookup::LinkLocalOnly(ip)) => {
            info!("[NET] pinned PTP interface {} has {}", name, ip)
        }
        Seen::Lookup(PinnedLookup::NoIpv4) => warn!(
            "[NET] pinned PTP interface {} has no IPv4 address yet -- waiting for it (never \
             another NIC)",
            name
        ),
        Seen::Lookup(PinnedLookup::Missing) => error!(
            "[NET] pinned PTP interface {} is missing -- waiting for it (never another NIC)",
            name
        ),
        Seen::OpenFailed(e) => warn!(
            "[NET] pinned PTP interface {} does not open yet: {} -- retrying (never another NIC)",
            name, e
        ),
    }
}

fn epoch_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The production pause between two looks.
pub const LOOK_EVERY: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests;
