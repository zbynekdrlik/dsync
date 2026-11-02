//! dantesync#142 — a window without a previous offset is not a measurement.
//!
//! The rate servo differentiates the window offset: the first window after warmup, and the first
//! after any measurement reset (a clock step, a PTP outage), has nothing to differentiate against.
//! Before 1.22 it was fed to the servo as a measured 0 µs/s: on iemmixer's gen3 box (v1.20.0) it
//! switched the gain mode to PROD at 15.65 s (`[PTP] PROD Drift +0.0us/s`), counted one step
//! toward LOCK and pulled the smoothed rate toward "stable" — then the real ~35 ppm put the servo
//! back in ACQ. Such a window only records the offset now.

use super::*;
use crate::clock::MockSystemClock;
use crate::traits::{MockNtpSource, MockPtpNetwork};

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

fn controller() -> Ctl {
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    let mut config = SystemConfig::default();
    config.filters.calibration_samples = 0;
    config.filters.warmup_secs = 0.0;
    PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    )
}

/// One window 0.5 s after the previous one (4 Syncs at 8 Hz), with the offset `offset_us`.
fn window(c: &mut Ctl, offset_us: f64) {
    if c.rate.last_offset_us.is_some() {
        c.rate.last_offset_time = Some(Instant::now() - Duration::from_millis(500));
    }
    c.apply_self_tuning_servo(offset_us);
}

#[test]
fn the_first_window_after_warmup_changes_no_gain_mode_and_no_lock_count_142() {
    let mut c = controller();
    window(&mut c, 1234.0);

    assert!(
        !c.rate.in_production_mode,
        "a window with no previous offset carries no rate: it must not switch to PROD"
    );
    assert_eq!(
        c.rate.lock_stable_count, 0,
        "a window with no previous offset must not count toward LOCK"
    );
    assert_eq!(
        c.rate.smoothed_rate_ppm, 0.0,
        "a window with no rate must not feed the smoothed rate"
    );
    assert_eq!(c.rate.spike_filter.stats().0, 0, "nor the spike filter");
    assert_eq!(
        c.rate.last_offset_us,
        Some(1234.0),
        "its offset is recorded"
    );
}

#[test]
fn the_first_window_after_a_measurement_reset_changes_no_gain_mode_and_no_lock_count_142() {
    let mut c = controller();
    // Acquiring a +35 µs/s oscillator: two windows, the second a real measurement.
    window(&mut c, 0.0);
    window(&mut c, 17.5);
    assert!(!c.rate.in_production_mode);
    let count = c.rate.lock_stable_count;
    let smoothed = c.rate.smoothed_rate_ppm;
    let filtered = c.rate.spike_filter.stats().0;

    // A clock step resets the measurement; the grace is over by the next window.
    c.reset_ptp_measurement_after_step();
    c.last_ntp_step = None;
    window(&mut c, 9_000.0);

    assert!(
        !c.rate.in_production_mode,
        "the first window after a reset carries no rate: it must not switch to PROD"
    );
    assert_eq!(c.rate.lock_stable_count, count, "nor move the lock count");
    assert_eq!(
        c.rate.smoothed_rate_ppm, smoothed,
        "nor feed the smoothed rate"
    );
    assert_eq!(
        c.rate.spike_filter.stats().0,
        filtered,
        "nor the spike filter"
    );

    // The next window is measured again (against the offset recorded after the reset).
    window(&mut c, 9_017.5);
    assert!(
        c.rate.smoothed_rate_ppm > smoothed,
        "measured: {}",
        c.rate.smoothed_rate_ppm
    );
}

#[test]
fn a_locked_node_keeps_its_lock_count_across_the_first_window_after_a_step_142() {
    let mut c = controller();
    c.rate.is_locked = true;
    c.rate.in_production_mode = true;
    c.rate.lock_stable_count = LOCK_STABLE_COUNT;
    c.reset_ptp_measurement_after_step();
    c.last_ntp_step = None;
    window(&mut c, 42.0);
    assert!(c.rate.is_locked);
    assert_eq!(
        c.rate.lock_stable_count, LOCK_STABLE_COUNT,
        "a window with no rate neither counts up nor down"
    );
}
