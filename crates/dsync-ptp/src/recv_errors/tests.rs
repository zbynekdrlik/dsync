use super::*;

const POLL: Duration = Duration::from_millis(1);

/// What a receive loop polling every millisecond sees: the adapter is gone during each
/// `[from, to)` span (seconds from the start) and the run lasts `total_s`.
struct Run {
    events: Vec<(Duration, GateEvent)>,
    /// `(at, ok)` for every read the gate let through.
    reads: Vec<(Duration, bool)>,
}

fn run(outages: &[(f64, f64)], total_s: f64) -> Run {
    let t0 = Instant::now();
    let mut gate = RecvErrorGate::new();
    let mut out = Run {
        events: Vec::new(),
        reads: Vec::new(),
    };
    let mut t = Duration::ZERO;
    let end = Duration::from_secs_f64(total_s);
    while t < end {
        let now = t0 + t;
        if gate.ready(now) {
            let s = t.as_secs_f64();
            let gone = outages.iter().any(|&(a, b)| s >= a && s < b);
            let event = if gone {
                gate.on_error(now, "The interface disappeared")
            } else {
                gate.on_ok(now)
            };
            out.reads.push((t, !gone));
            if let Some(e) = event {
                out.events.push((t, e));
            }
        }
        t += POLL;
    }
    out
}

fn reads_between(r: &Run, a: f64, b: f64) -> usize {
    r.reads
        .iter()
        .filter(|(t, _)| t.as_secs_f64() >= a && t.as_secs_f64() < b)
        .count()
}

/// iem-snv, 10.11.2026: the NIC gone ~25 s. One WARN at the transition, a count every 10 s,
/// one recovery line — not a line per poll.
#[test]
fn a_25s_outage_logs_the_transition_a_count_per_interval_and_the_recovery_161() {
    let r = run(&[(5.0, 30.0)], 40.0);
    let failing = r
        .events
        .iter()
        .filter(|(_, e)| matches!(e, GateEvent::Failing { .. }))
        .count();
    let still: Vec<_> = r
        .events
        .iter()
        .filter_map(|(t, e)| match e {
            GateEvent::StillFailing { errors, over, .. } => Some((*t, *errors, *over)),
            _ => None,
        })
        .collect();
    let recovered: Vec<_> = r
        .events
        .iter()
        .filter_map(|(t, e)| match e {
            GateEvent::Recovered { errors, outage } => Some((*t, *errors, *outage)),
            _ => None,
        })
        .collect();
    assert!(
        r.events.len() <= 5,
        "{} lines for one outage: {:?}",
        r.events.len(),
        &r.events[..r.events.len().min(8)]
    );
    assert_eq!(failing, 1, "one line at the transition");
    assert!(
        (1..=3).contains(&still.len()),
        "a count every 10 s: {still:?}"
    );
    for (_, errors, over) in &still {
        assert!(*errors > 0, "a summary carries the count");
        assert!(*over >= SUMMARY_EVERY, "at most one per interval: {over:?}");
    }
    assert_eq!(recovered.len(), 1, "one recovery line: {:?}", r.events);
    let (_, errors, outage) = recovered[0];
    let failed_reads = r.reads.iter().filter(|(_, ok)| !ok).count() as u64;
    assert_eq!(errors, failed_reads, "the recovery counts every failed read");
    assert!(
        (24.0..=26.0).contains(&outage.as_secs_f64()),
        "the outage measured from its first error: {outage:?}"
    );
}

/// The dead handle is not hammered: a back-off up to 1 s.
#[test]
fn a_dead_capture_is_read_about_once_a_second_161() {
    let r = run(&[(5.0, 30.0)], 40.0);
    let during = reads_between(&r, 5.0, 30.0);
    assert!(during <= 40, "{during} reads of a dead capture in 25 s");
    // A healthy capture is read on every poll.
    assert!(reads_between(&r, 0.0, 5.0) >= 4_900);
}

/// The adapter came back (PTP back at 13:34:44Z): the next read is within the back-off cap.
#[test]
fn a_returned_adapter_is_read_within_the_backoff_cap_161() {
    let r = run(&[(5.0, 30.0)], 40.0);
    let first_ok = r
        .reads
        .iter()
        .find(|(t, ok)| *ok && t.as_secs_f64() >= 30.0)
        .expect("a read after the outage")
        .0;
    assert!(
        first_ok <= Duration::from_secs(30) + BACKOFF_MAX,
        "first good read at {first_ok:?}"
    );
    // Recovered: every poll reads again.
    assert!(reads_between(&r, 32.0, 40.0) >= 7_900);
}

/// One transient error costs one short wait, a transition line and a recovery line.
#[test]
fn a_single_error_waits_the_first_backoff_only_161() {
    let r = run(&[(1.0, 1.0005)], 2.0);
    let failed: Vec<_> = r.reads.iter().filter(|(_, ok)| !ok).collect();
    assert_eq!(failed.len(), 1);
    let next = r
        .reads
        .iter()
        .find(|(t, _)| *t > failed[0].0)
        .expect("a read after the error")
        .0;
    assert_eq!(next - failed[0].0, BACKOFF_FIRST);
    assert_eq!(r.events.len(), 2, "{:?}", r.events);
}

/// After a recovery the back-off starts from the first step again.
#[test]
fn the_backoff_resets_after_a_recovery_161() {
    let r = run(&[(1.0, 6.0), (10.0, 10.0005)], 12.0);
    let second = r
        .reads
        .iter()
        .find(|(t, ok)| !ok && t.as_secs_f64() >= 10.0)
        .expect("the second outage's error")
        .0;
    let next = r
        .reads
        .iter()
        .find(|(t, _)| *t > second)
        .expect("a read after it")
        .0;
    assert_eq!(next - second, BACKOFF_FIRST);
    let failing = r
        .events
        .iter()
        .filter(|(_, e)| matches!(e, GateEvent::Failing { .. }))
        .count();
    assert_eq!(failing, 2, "each outage has its own transition line");
}

#[test]
fn the_lines_name_the_error_and_the_count_161() {
    let e = GateEvent::StillFailing {
        errors: 12,
        over: Duration::from_secs(10),
        text: "The interface disappeared".into(),
    };
    let s = e.to_string();
    assert!(s.contains("12 errors") && s.contains("The interface disappeared"), "{s}");
    let r = GateEvent::Recovered {
        errors: 31,
        outage: Duration::from_millis(25_300),
    }
    .to_string();
    assert!(r.contains("31 errors") && r.contains("25.3s"), "{r}");
}
