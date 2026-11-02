use super::*;

fn feed(g: &mut PhaseLockGate, e_ns: i64, n: usize) -> Vec<GateEvent> {
    (0..n)
        .filter_map(|_| g.on_window(true, Some(e_ns)))
        .collect()
}

#[test]
fn it_locks_after_three_windows_within_fifty_us_146() {
    let mut g = PhaseLockGate::new();
    assert!(feed(&mut g, LOCK_BOUND_NS, LOCK_WINDOWS - 1).is_empty());
    assert!(!g.locked());
    assert_eq!(
        g.on_window(true, Some(-LOCK_BOUND_NS)),
        Some(GateEvent::Locked {
            error_ns: -LOCK_BOUND_NS
        })
    );
    assert!(g.locked());
}

#[test]
fn a_window_beyond_fifty_us_restarts_the_lock_count_146() {
    let mut g = PhaseLockGate::new();
    feed(&mut g, 0, LOCK_WINDOWS - 1);
    assert_eq!(g.on_window(true, Some(LOCK_BOUND_NS + 1)), None);
    assert!(feed(&mut g, 0, LOCK_WINDOWS - 1).is_empty(), "from scratch");
    assert!(!g.locked());
    assert_eq!(feed(&mut g, 0, 1).len(), 1);
}

#[test]
fn it_never_locks_while_disengaged_or_unanchored_146() {
    let mut g = PhaseLockGate::new();
    for _ in 0..10 {
        assert_eq!(g.on_window(false, Some(0)), None);
        assert_eq!(g.on_window(true, None), None);
    }
    assert!(!g.locked());
    // A disengaged window in the middle of a count restarts it.
    feed(&mut g, 0, LOCK_WINDOWS - 1);
    g.on_window(false, Some(0));
    assert!(feed(&mut g, 0, LOCK_WINDOWS - 1).is_empty());
}

#[test]
fn between_the_bounds_it_holds_either_state_146() {
    // Hysteresis: 100 µs neither locks an unlocked gate nor unlocks a locked one.
    let mut g = PhaseLockGate::new();
    assert!(feed(&mut g, 100_000, 50).is_empty());
    assert!(!g.locked());
    feed(&mut g, 0, LOCK_WINDOWS);
    assert!(g.locked());
    assert!(
        feed(&mut g, -UNLOCK_BOUND_NS, 50).is_empty(),
        "at the bound: held"
    );
    assert!(g.locked());
}

#[test]
fn it_unlocks_after_four_windows_beyond_one_fifty_us_146() {
    let mut g = PhaseLockGate::new();
    feed(&mut g, 0, LOCK_WINDOWS);
    assert!(feed(&mut g, UNLOCK_BOUND_NS + 1, UNLOCK_WINDOWS - 1).is_empty());
    assert!(g.locked(), "three windows out: still locked");
    assert_eq!(
        g.on_window(true, Some(-450_000)),
        Some(GateEvent::PhaseLost { error_ns: -450_000 })
    );
    assert!(!g.locked());
    // Back within 50 µs: locked again after the full count.
    assert!(feed(&mut g, 10_000, LOCK_WINDOWS - 1).is_empty());
    assert_eq!(feed(&mut g, 10_000, 1).len(), 1);
}

#[test]
fn one_window_back_inside_restarts_the_unlock_count_146() {
    let mut g = PhaseLockGate::new();
    feed(&mut g, 0, LOCK_WINDOWS);
    for _ in 0..20 {
        assert!(feed(&mut g, 400_000, UNLOCK_WINDOWS - 1).is_empty());
        assert_eq!(g.on_window(true, Some(120_000)), None);
    }
    assert!(g.locked());
}

#[test]
fn disengaging_unlocks_at_once_and_reset_reports_only_a_real_change_146() {
    let mut g = PhaseLockGate::new();
    assert_eq!(g.reset(), None, "nothing to clear");
    feed(&mut g, 0, LOCK_WINDOWS);
    assert_eq!(g.on_window(false, Some(0)), Some(GateEvent::Disengaged));
    assert!(!g.locked());
    feed(&mut g, 0, LOCK_WINDOWS);
    assert_eq!(g.reset(), Some(GateEvent::Disengaged));
    assert!(!g.locked());
    assert!(
        feed(&mut g, 0, LOCK_WINDOWS - 1).is_empty(),
        "counts from scratch"
    );
}

#[test]
fn an_extreme_error_does_not_overflow_146() {
    let mut g = PhaseLockGate::new();
    feed(&mut g, 0, LOCK_WINDOWS);
    assert!(feed(&mut g, i64::MIN, UNLOCK_WINDOWS - 1).is_empty());
    assert!(matches!(
        g.on_window(true, Some(i64::MIN)),
        Some(GateEvent::PhaseLost { .. })
    ));
}
