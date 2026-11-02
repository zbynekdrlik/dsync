//! dsync#146 — the reported lock's controller glue: the decoupled rate, the one definition of
//! the published lock, and the PTP-offline edge. The closed loop is `lock_sim_tests.rs`.

use super::super::tests::phase_lock_config;
use super::*;
use crate::clock::MockSystemClock;
use crate::lock_gate::LOCK_WINDOWS;
use crate::traits::{MockNtpSource, MockPtpNetwork};

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

fn controller(config: SystemConfig) -> Ctl {
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    )
}

fn legacy() -> SystemConfig {
    let mut config = phase_lock_config();
    config.clock_discipline = CLOCK_DISCIPLINE_LEGACY.to_string();
    config
}

fn published(c: &Ctl) -> (bool, String) {
    c.update_shared_status();
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    (st.is_locked, st.mode.clone())
}

#[test]
fn the_rate_is_decoupled_from_the_average_of_the_last_two_deviations_146() {
    let mut c = controller(phase_lock_config());
    assert_eq!(c.decouple_phase_lock_rate(7.5), 7.5, "nothing engaged yet");
    c.date_sync.word_deviation.ppm = [10.0, 30.0];
    assert_eq!(c.decouple_phase_lock_rate(25.0), 5.0);
    // The newest deviation goes last; the oldest falls out.
    c.date_sync.note_word_deviation(None);
    assert_eq!(c.date_sync.word_deviation.ppm, [30.0, 0.0]);
    assert_eq!(c.decouple_phase_lock_rate(25.0), 10.0);
}

#[test]
fn a_word_deviation_is_measured_from_the_integrator_146() {
    let mut c = controller(phase_lock_config());
    // Engage the core with the rate servo's word +20 ppm: its integrator starts there.
    c.date_sync.core.on_window(0, true, 20.0, 0.5);
    let i = c.date_sync.core.integrator_ppm();
    c.date_sync.note_word_deviation(Some(i - 14.0));
    assert!((c.date_sync.word_deviation.ppm[1] + 14.0).abs() < 1e-9);
}

#[test]
fn the_legacy_discipline_keeps_the_rate_servo_and_its_lock_146() {
    let mut c = controller(legacy());
    c.date_sync.word_deviation.ppm = [10.0, 30.0];
    assert_eq!(c.decouple_phase_lock_rate(25.0), 25.0, "never decoupled");
    c.rate.is_locked = true;
    assert_eq!(published(&c), (true, "LOCK".to_string()));
}

#[test]
fn under_the_phase_lock_the_rate_lock_alone_is_not_reported_146() {
    let mut c = controller(phase_lock_config());
    c.rate.is_locked = true;
    assert_eq!(
        published(&c),
        (false, "PROD".to_string()),
        "rate-locked, phase not yet within the bound"
    );
    c.rate.in_nano_mode = true;
    assert_eq!(published(&c).1, "PROD", "no NANO without the reported lock");
    // The phase lock engaged and within the bound for the lock count: reported.
    c.date_sync.core.on_window(0, true, 0.0, 0.5);
    for _ in 0..LOCK_WINDOWS {
        c.judge_reported_lock(Some(1_000));
    }
    assert_eq!(published(&c), (true, "NANO".to_string()));
    c.rate.in_nano_mode = false;
    assert_eq!(published(&c), (true, "LOCK".to_string()));
}

#[test]
fn the_ptp_offline_edge_clears_the_reported_lock_and_the_deviation_146() {
    let mut c = controller(phase_lock_config());
    c.rate.is_locked = true;
    c.date_sync.core.on_window(0, true, 0.0, 0.5);
    for _ in 0..LOCK_WINDOWS {
        c.judge_reported_lock(Some(0));
    }
    c.date_sync.word_deviation.ppm = [3.0, 4.0];
    assert!(c.reported_lock());
    c.on_ptp_offline_edge();
    assert!(!c.reported_lock());
    assert!(!c.date_sync.core.engaged());
    assert_eq!(c.date_sync.word_deviation.ppm, [0.0; 2]);
    // The rate servo's own lock is kept through the outage (#112): only the report clears.
    assert!(c.rate.is_locked);
}

#[test]
fn the_ptp_offline_edge_restarts_the_acquisition_fit_146() {
    // The fit's open-loop phase needs the true `dt`; after an outage the controller has none.
    let mut c = controller(phase_lock_config());
    for _ in 0..30 {
        c.date_sync.core.on_window(0, false, 0.0, 0.5);
    }
    assert!(c.date_sync.core.boot_fit_points() > 12);
    c.on_ptp_offline_edge();
    assert_eq!(c.date_sync.core.boot_fit_points(), 0);
}

#[test]
fn an_engaged_phase_lock_holds_the_rate_lock_through_an_unstable_window_146() {
    // A seed 7-15 ppm off: the rate the rate servo still measures (decoupled) is the frequency
    // the phase lock is retiring itself; it must not drain the fresh lock count and disengage it.
    let mut c = controller(phase_lock_config());
    c.rate.is_locked = true;
    assert!(!c.phase_lock_holds_rate_lock(), "not engaged yet");
    c.date_sync.core.on_window(0, true, 0.0, 0.5);
    assert!(c.phase_lock_holds_rate_lock());
    let mut legacy_c = controller(legacy());
    legacy_c.rate.is_locked = true;
    legacy_c.date_sync.core.on_window(0, true, 0.0, 0.5);
    assert!(
        !legacy_c.phase_lock_holds_rate_lock(),
        "legacy: the rate lock as before"
    );
}

#[test]
fn an_unstable_measured_window_does_not_unlock_an_engaged_phase_lock_146() {
    // Through the real window path: two windows whose offset moves 100 us in 0.5 s (200 us/s),
    // the phase lock engaged. Without the hold the lock count drops (gradual unlock).
    let mut c = controller(phase_lock_config());
    c.rate.is_locked = true;
    c.rate.lock_stable_count = LOCK_STABLE_COUNT;
    c.date_sync.core.on_window(0, true, 0.0, 0.5);
    for (k, offset_us) in [0.0, 100.0, 200.0].into_iter().enumerate() {
        c.date_sync.pending_median_ns = Some(0);
        c.date_sync.pending_t1_ns = (k as i64 + 1) * 500_000_000;
        c.servo_window_after(offset_us, 0.5);
    }
    assert!(c.date_sync.core.engaged());
    assert!(c.rate.is_locked, "still rate-locked");
    assert_eq!(
        c.rate.lock_stable_count, LOCK_STABLE_COUNT,
        "the count did not move"
    );
}

#[test]
fn the_clock_alarm_sees_an_engaged_phase_lock_without_41_for_two_minutes_as_lost_146() {
    let mut c = controller(phase_lock_config());
    c.rate.is_locked = true;
    c.date_sync.core.on_window(0, true, 0.0, 0.5);
    // Engaged, [41] not (yet) set: the boot pull. Not lost within the grace …
    c.judge_reported_lock(Some(400_000));
    assert!(c.date_sync.unsafe_since.is_some());
    assert!(c.alarm_sees_lock());
    // … lost beyond it (a reference the phase lock cannot hold).
    c.date_sync.unsafe_since = Some(Instant::now() - PHASE_UNSAFE_ALARM_AFTER);
    assert!(!c.alarm_sees_lock());
    let health = c.sample_clock_health();
    assert!(!health.is_locked && !health.mode_locked);
    // [41] set again: healthy at once.
    for _ in 0..LOCK_WINDOWS {
        c.judge_reported_lock(Some(0));
    }
    assert_eq!(c.date_sync.unsafe_since, None);
    assert!(c.alarm_sees_lock());
    // Legacy never arms it.
    let mut l = controller(legacy());
    l.rate.is_locked = true;
    assert!(l.alarm_sees_lock());
    assert!(l.sample_clock_health().is_locked);
}
