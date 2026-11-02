//! The rate servo's window span is the grandmaster's time, not the time the window is processed
//! (#147 lane, the #146 follow-up (1)): the offset it differentiates is in PTP time, so a window
//! the loop reaches late must still read `Δoffset / GM span`. Before 1.25 the span was
//! `Instant::now()` at processing: the e2e simulations ran every window in under 0.1 s of wall
//! time (no measurement at all), and a pause under tarpaulin divided a simulated offset by a real
//! span — `test_auto_adaptive_drift_rates` hit the ±500 ppm clamp (PR #149 Coverage, 2/2).

use super::*;
use crate::clock::MockSystemClock;
use crate::traits::{MockNtpSource, MockPtpNetwork};

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

fn controller() -> Ctl {
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    let mut config = SystemConfig::default();
    config.clock_discipline = crate::config::CLOCK_DISCIPLINE_LEGACY.to_string();
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

/// One window whose four samples all read `offset_ns`, closed at grandmaster time `t1_ns`.
fn close_window(c: &mut Ctl, offset_ns: i64, t1_ns: i64) {
    c.sample_window = vec![offset_ns; 4];
    c.process_sample_window(t1_ns);
}

#[test]
fn a_window_processed_late_measures_its_rate_over_the_grandmaster_span() {
    let mut c = controller();
    close_window(&mut c, 0, 1_000_000_000);
    // The loop reaches the next window only 0.2 s later in wall time, while the grandmaster's
    // windows are 0.5 s apart: the 50 µs the offset moved are 100 µs/s, not 250.
    std::thread::sleep(Duration::from_millis(200));
    close_window(&mut c, 50_000, 1_500_000_000);

    // The same two windows through the shared servo with their true 0.5 s span.
    let mut reference = crate::rate_servo::RateServo::<crate::rate_servo::MonoNs>::new();
    let at = crate::rate_servo::MonoNs;
    reference.measure_offset(0.0, at(1_000_000_000));
    let m = reference.measure_offset(50.0, at(1_500_000_000));
    assert_eq!(m.raw_rate_ppm, 100.0, "50 µs over 0.5 s");
    let expected = reference.measure(m.raw_rate_ppm, || at(1_500_000_000));

    let rate = c.rate.smoothed_rate_ppm;
    assert!(
        (rate - expected).abs() < 1e-9,
        "a window the loop reaches late must read its rate over the grandmaster span: \
         smoothed {rate:.3} ppm, expected {expected:.3} ppm (100 ppm raw)"
    );
}
