//! dantesync#146 — the reported lock's controller glue: the decoupled rate, the one definition of
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
    c.date_sync.word_deviation_ppm = [10.0, 30.0];
    assert_eq!(c.decouple_phase_lock_rate(25.0), 5.0);
    // The newest deviation goes last; the oldest falls out.
    c.date_sync.note_word_deviation(None);
    assert_eq!(c.date_sync.word_deviation_ppm, [30.0, 0.0]);
    assert_eq!(c.decouple_phase_lock_rate(25.0), 10.0);
}

#[test]
fn a_word_deviation_is_measured_from_the_integrator_146() {
    let mut c = controller(phase_lock_config());
    // Engage the core with the rate servo's word +20 ppm: its integrator starts there.
    c.date_sync.core.on_window(0, true, 20.0, 0.5);
    let i = c.date_sync.core.integrator_ppm();
    c.date_sync.note_word_deviation(Some(i - 14.0));
    assert!((c.date_sync.word_deviation_ppm[1] + 14.0).abs() < 1e-9);
}

#[test]
fn the_legacy_discipline_keeps_the_rate_servo_and_its_lock_146() {
    let mut c = controller(legacy());
    c.date_sync.word_deviation_ppm = [10.0, 30.0];
    assert_eq!(c.decouple_phase_lock_rate(25.0), 25.0, "never decoupled");
    c.is_locked = true;
    assert_eq!(published(&c), (true, "LOCK".to_string()));
}

#[test]
fn under_the_phase_lock_the_rate_lock_alone_is_not_reported_146() {
    let mut c = controller(phase_lock_config());
    c.is_locked = true;
    assert_eq!(
        published(&c),
        (false, "PROD".to_string()),
        "rate-locked, phase not yet within the bound"
    );
    c.in_nano_mode = true;
    assert_eq!(published(&c).1, "PROD", "no NANO without the reported lock");
    // The phase lock engaged and within the bound for the lock count: reported.
    c.date_sync.core.on_window(0, true, 0.0, 0.5);
    for _ in 0..LOCK_WINDOWS {
        c.judge_reported_lock(Some(1_000));
    }
    assert_eq!(published(&c), (true, "NANO".to_string()));
    c.in_nano_mode = false;
    assert_eq!(published(&c), (true, "LOCK".to_string()));
}

#[test]
fn the_ptp_offline_edge_clears_the_reported_lock_and_the_deviation_146() {
    let mut c = controller(phase_lock_config());
    c.is_locked = true;
    c.date_sync.core.on_window(0, true, 0.0, 0.5);
    for _ in 0..LOCK_WINDOWS {
        c.judge_reported_lock(Some(0));
    }
    c.date_sync.word_deviation_ppm = [3.0, 4.0];
    assert!(c.reported_lock());
    c.on_ptp_offline_edge();
    assert!(!c.reported_lock());
    assert!(!c.date_sync.core.engaged());
    assert_eq!(c.date_sync.word_deviation_ppm, [0.0; 2]);
    // The rate servo's own lock is kept through the outage (#112): only the report clears.
    assert!(c.is_locked);
}
