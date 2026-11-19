//! dsync#131 — the pinned-interface startup wait, driven with a scripted NIC, a scripted
//! open, and a clock that advances one second per pause.

use super::*;
use crate::boot_ready::{ReadyGate, ReadyReason};
use anyhow::anyhow;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

const RIG: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 204);

#[derive(Clone, Default)]
struct Counting(Rc<Cell<u32>>);

impl ClockAlarmNotifier for Counting {
    fn notify(&self, _title: &str, _message: &str) {
        self.0.set(self.0.get() + 1);
    }
}

/// What one run saw: the result, the alarm reasons published at each look, the looks made, the
/// notifications, and the final `/status`.
struct Run {
    opened: Option<(u32, Ipv4Addr)>,
    reasons: Vec<Option<String>>,
    looks: u32,
    notified: u32,
    status: SyncStatus,
}

/// Run the wait over a script: each look pops one lookup (the last repeats) and, when the wait
/// opens, one open result (the last repeats). The clock advances 1 s per pause. `stop_after`
/// clears `running` after that many looks.
fn run(
    lookups: &[PinnedLookup],
    opens: Vec<Result<u32, &'static str>>,
    stop_after: Option<u32>,
) -> Run {
    let status = RwLock::new(SyncStatus::default());
    let running = AtomicBool::new(true);
    let notifier = Counting::default();
    let seconds = Cell::new(0u64);
    let base = Instant::now();
    let looks = Cell::new(0u32);
    let reasons = RefCell::new(Vec::new());
    let mut lookups: VecDeque<PinnedLookup> = lookups.iter().copied().collect();
    let mut opens: VecDeque<Result<u32, &'static str>> = opens.into();
    let wait = PinnedWait {
        name: "enp56s0",
        status: &status,
        alarm_interval_s: 60,
        config_problem: None,
        notifier: notifier.clone(),
        running: &running,
        waiting: &mut |_, _| {},
    };
    let opened = wait.run(
        || {
            looks.set(looks.get() + 1);
            if Some(looks.get()) == stop_after {
                running.store(false, Ordering::SeqCst);
            }
            if lookups.len() > 1 {
                lookups.pop_front().expect("scripted")
            } else {
                lookups[0]
            }
        },
        |_ip| {
            let next = if opens.len() > 1 {
                opens.pop_front().expect("scripted")
            } else {
                opens[0]
            };
            next.map_err(|e| anyhow!(e))
        },
        || {
            let s = status.read().expect("status");
            reasons
                .borrow_mut()
                .push(s.clock_alarm.active.then(|| s.clock_alarm.reason.clone()));
            seconds.set(seconds.get() + 1);
        },
        || base + Duration::from_secs(seconds.get()),
    );
    let status = status.read().expect("status").clone();
    Run {
        opened,
        reasons: reasons.into_inner(),
        looks: looks.get(),
        notified: notifier.0.get(),
        status,
    }
}

#[test]
fn a_missing_pinned_nic_alarms_at_once_then_opens_and_clears_the_alarm_131() {
    let r = run(
        &[
            PinnedLookup::Missing,
            PinnedLookup::Missing,
            PinnedLookup::Ready(RIG),
        ],
        vec![Ok(7)],
        None,
    );
    assert_eq!(
        r.opened,
        Some((7, RIG)),
        "opened on the pinned NIC's address"
    );
    assert_eq!(
        r.reasons,
        vec![
            Some("PTP interface enp56s0 is missing".to_string()),
            Some("PTP interface enp56s0 is missing".to_string()),
        ]
    );
    assert_eq!(r.notified, 1, "one notification per cadence");
    assert!(!r.status.clock_alarm.active, "cleared once open");
    assert_eq!(
        r.status.ptp_interface_configured.as_deref(),
        Some("enp56s0")
    );
}

#[test]
fn no_ipv4_is_waited_out_silently_through_the_grace_then_alarms_131() {
    let r = run(&[PinnedLookup::NoIpv4], vec![Ok(1)], Some(14));
    assert_eq!(r.opened, None, "the service stopped while waiting");
    assert_eq!(r.looks, 14);
    // Looks at t = 0..=10 s are within the grace; t = 11 s on alarm.
    assert!(
        r.reasons[..11].iter().all(Option::is_none),
        "{:?}",
        r.reasons
    );
    assert_eq!(
        r.reasons[11].as_deref(),
        Some("PTP interface enp56s0 has no IPv4 address")
    );
    assert!(r.status.clock_alarm.active, "still alarming when stopped");
}

#[test]
fn an_open_that_fails_at_boot_is_retried_never_fatal_131() {
    // Windows: the Npcap device list lags the adapter list right after boot.
    let r = run(
        &[PinnedLookup::Ready(RIG)],
        vec![
            Err("no Npcap device carries 10.77.9.204"),
            Err("again"),
            Ok(3),
        ],
        None,
    );
    assert_eq!(r.opened, Some((3, RIG)));
    assert_eq!(r.looks, 3);
    assert!(r.reasons.iter().all(Option::is_none), "within the grace");
}

#[test]
fn an_open_that_keeps_failing_alarms_after_the_grace_naming_the_nic_131() {
    let r = run(
        &[PinnedLookup::Ready(RIG)],
        vec![Err("Npcap: No such device")],
        Some(13),
    );
    assert_eq!(r.opened, None);
    let last = r.reasons.last().cloned().flatten().expect("an alarm");
    assert_eq!(
        last,
        "PTP interface enp56s0 could not be joined: Npcap: No such device"
    );
}

#[test]
fn a_link_local_only_nic_is_opened_only_after_the_grace_131() {
    let ll = Ipv4Addr::new(169, 254, 1, 2);
    let r = run(&[PinnedLookup::LinkLocalOnly(ll)], vec![Ok(5)], None);
    assert_eq!(r.opened, Some((5, ll)));
    assert_eq!(
        r.looks, 12,
        "t = 11 s is the first look past the 10 s grace"
    );
    assert!(r.reasons.iter().all(Option::is_none), "no alarm meanwhile");
}

#[test]
fn a_stopped_service_ends_the_wait_131() {
    let r = run(&[PinnedLookup::Missing], vec![Ok(1)], Some(1));
    assert_eq!(r.opened, None);
    assert_eq!(r.looks, 1);
}

#[test]
fn a_raised_alarm_follows_the_nic_state_inside_the_grace_131() {
    // Missing (alarm at once), then present without an IPv4 inside the grace: the alarm must not
    // keep saying "missing".
    let r = run(
        &[PinnedLookup::Missing, PinnedLookup::NoIpv4],
        vec![Ok(1)],
        Some(3),
    );
    assert_eq!(
        r.reasons,
        vec![
            Some("PTP interface enp56s0 is missing".to_string()),
            Some("PTP interface enp56s0 has no IPv4 address".to_string()),
        ]
    );
}

#[test]
fn a_config_fallback_keeps_the_alarm_once_the_pinned_nic_opens_131_116() {
    let status = RwLock::new(SyncStatus::default());
    let running = AtomicBool::new(true);
    let seconds = Cell::new(0u64);
    let base = Instant::now();
    let mut looks = vec![PinnedLookup::Ready(RIG), PinnedLookup::Missing];
    let wait = PinnedWait {
        name: "enp56s0",
        status: &status,
        alarm_interval_s: 60,
        config_problem: Some("config.json not loaded (x), running defaults".to_string()),
        notifier: Counting::default(),
        running: &running,
        waiting: &mut |_, _| {},
    };
    let opened = wait.run(
        || looks.pop().expect("scripted"),
        |_| Ok(1u32),
        || seconds.set(seconds.get() + 1),
        || base + Duration::from_secs(seconds.get()),
    );
    assert_eq!(opened, Some((1, RIG)));
    let s = status.read().expect("status");
    assert!(
        s.clock_alarm.active,
        "no false clear while the config fallback runs"
    );
    assert_eq!(
        s.clock_alarm.reason,
        "config.json not loaded (x), running defaults"
    );
}

/// What a wait driven with the real #164 `ReadyGate` saw: the open, the READY reasons with the
/// second they were sent at, the seconds the wait took, and the gate itself.
struct ReadyRun {
    opened: Option<(u32, Ipv4Addr)>,
    readies: Vec<(u64, ReadyReason)>,
    seconds: u64,
    gate: ReadyGate,
    base: Instant,
}

/// The pinned NIC is missing for `missing_for` looks (1 s each), then present; the gate starts
/// with the wait, as it does at process start.
fn ready_run(missing_for: u32) -> ReadyRun {
    let status = RwLock::new(SyncStatus::default());
    let running = AtomicBool::new(true);
    let seconds = Cell::new(0u64);
    let base = Instant::now();
    let looks = Cell::new(0u32);
    let mut gate = ReadyGate::new(base);
    let mut readies = Vec::new();
    let opened = {
        let mut waiting = |at: Instant, state: &str| {
            if let Some(reason) = gate.poll(at, Err(state)) {
                readies.push((at.duration_since(base).as_secs(), reason));
            }
        };
        let wait = PinnedWait {
            name: "enp56s0",
            status: &status,
            alarm_interval_s: 60,
            config_problem: None,
            notifier: Counting::default(),
            running: &running,
            waiting: &mut waiting,
        };
        wait.run(
            || {
                looks.set(looks.get() + 1);
                if looks.get() <= missing_for {
                    PinnedLookup::Missing
                } else {
                    PinnedLookup::Ready(RIG)
                }
            },
            |_| Ok(7u32),
            || seconds.set(seconds.get() + 1),
            || base + Duration::from_secs(seconds.get()),
        )
    };
    ReadyRun {
        opened,
        readies,
        seconds: seconds.get(),
        gate,
        base,
    }
}

/// camera-box#1483: a missing rig NIC held READY (and lightdm after it) until systemd's start
/// timeout. READY goes at 30 s with the NIC still missing; the NIC appearing later is joined.
#[test]
fn ready_is_sent_while_the_pinned_nic_is_missing_and_the_nic_joins_after_it_193() {
    let mut r = ready_run(40);
    assert_eq!(
        r.opened,
        Some((7, RIG)),
        "the NIC is joined once it appears"
    );
    assert_eq!(r.seconds, 40, "the join comes at 40 s, after READY");
    assert_eq!(
        r.readies,
        vec![(
            30,
            ReadyReason::TimedOut("PTP interface enp56s0 is missing".to_string())
        )],
        "READY once, at the 30 s bound, naming the missing NIC"
    );
    assert_eq!(
        r.gate
            .poll(r.base + Duration::from_secs(41), Ok("PTP window")),
        None,
        "the sync loop never sends READY a second time"
    );
}

/// A NIC present within the bound leaves READY to the placed wall (#164), and the bound still
/// counts from the start, not from the join.
#[test]
fn a_nic_present_within_the_bound_leaves_ready_to_the_wall_193() {
    let mut r = ready_run(5);
    assert_eq!(r.opened, Some((7, RIG)));
    assert!(r.readies.is_empty(), "no READY during a short wait");
    assert!(!r.gate.sent());
    assert_eq!(
        r.gate.poll(
            r.base + Duration::from_secs(29),
            Err("waiting for the boot date")
        ),
        None
    );
    assert!(matches!(
        r.gate.poll(
            r.base + Duration::from_secs(30),
            Err("waiting for the boot date")
        ),
        Some(ReadyReason::TimedOut(_))
    ));
}

/// dsync#212 — a pinned LIST: the startup waits until a LISTED NIC has an IPv4 (the alarm
/// names the list meanwhile), opens on THAT listed name, and publishes the list.
#[test]
fn a_pinned_list_opens_on_the_listed_nic_that_got_the_ipv4_212() {
    let status = RwLock::new(SyncStatus::default());
    let running = AtomicBool::new(true);
    let seconds = Cell::new(0u64);
    let base = Instant::now();
    let opened_on = RefCell::new(Vec::new());
    let reasons = RefCell::new(Vec::new());
    let spec = "enp1s0,enp3s0";
    let wait = PinnedWait {
        name: spec,
        status: &status,
        alarm_interval_s: 60,
        config_problem: None,
        notifier: Counting::default(),
        running: &running,
        waiting: &mut |_, _| {},
    };
    let opened = wait.run_list(
        || {
            // Neither port has an address for 12 s (past the grace), then the cable's port does.
            let choice = if seconds.get() < 12 {
                PinnedChoice {
                    name: spec.to_string(),
                    lookup: PinnedLookup::NoIpv4,
                }
            } else {
                PinnedChoice {
                    name: "enp3s0".to_string(),
                    lookup: PinnedLookup::Ready(RIG),
                }
            };
            reasons.borrow_mut().push(
                status
                    .read()
                    .expect("status")
                    .clock_alarm
                    .active
                    .then(|| status.read().expect("status").clock_alarm.reason.clone()),
            );
            choice
        },
        |name, ip| {
            opened_on.borrow_mut().push((name.to_string(), ip));
            Ok(5u32)
        },
        || seconds.set(seconds.get() + 1),
        || base + Duration::from_secs(seconds.get()),
    );
    assert_eq!(opened, Some((5, "enp3s0".to_string(), RIG)));
    assert_eq!(*opened_on.borrow(), vec![("enp3s0".to_string(), RIG)]);
    let reasons = reasons.into_inner();
    assert!(
        reasons
            .iter()
            .flatten()
            .all(|r| r.contains("none of the PTP interfaces enp1s0,enp3s0 has an IPv4 address")),
        "{reasons:?}"
    );
    assert!(
        reasons.iter().any(Option::is_some),
        "the alarm named the list past the grace"
    );
    let st = status.read().expect("status");
    assert!(!st.clock_alarm.active, "cleared once open");
    assert_eq!(st.ptp_interface_configured.as_deref(), Some(spec));
    assert_eq!(
        st.interfaces_configured,
        Some(vec!["enp1s0".to_string(), "enp3s0".to_string()])
    );
}
