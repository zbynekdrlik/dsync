use super::*;

/// One window through the four steps, as both consumers call them (no hooks between).
fn window(servo: &mut RateServo<MonoNs>, offset_us: f64, at_s: f64, hold: bool) -> ServoWord {
    let now = MonoNs((at_s * 1e9) as i64);
    let m = servo.measure_offset(offset_us, now);
    let rate = if m.measured {
        servo.measure(m.raw_rate_ppm.clamp(-500.0, 500.0), || now)
    } else {
        servo.smoothed_rate_ppm
    };
    if m.measured {
        servo.update_modes(rate);
    }
    servo.word(rate, m.measured, hold)
}

#[test]
fn the_monotonic_stamp_measures_a_span_in_seconds_and_never_backwards() {
    assert_eq!(MonoNs(2_500_000_000).secs_since(MonoNs(1_000_000_000)), 1.5);
    assert_eq!(MonoNs(1).secs_since(MonoNs(5)), 0.0);
}

#[test]
fn the_grandmaster_stamp_measures_a_span_in_seconds_and_never_backwards() {
    assert_eq!(GmNs(2_500_000_000).secs_since(GmNs(1_000_000_000)), 1.5);
    assert_eq!(
        GmNs(1).secs_since(GmNs(5)),
        0.0,
        "a GM time that went back: span 0"
    );
}

#[test]
fn the_first_window_has_no_previous_offset_and_is_no_measurement_142() {
    let mut servo = RateServo::<MonoNs>::new();
    let m = servo.measure_offset(123.0, MonoNs(0));
    assert!(!m.measured);
    assert_eq!(m.raw_rate_ppm, 0.0);
    assert_eq!(m.dt_secs, 1.0);
    let w = servo.word(0.0, m.measured, false);
    assert_eq!(
        servo.lock_stable_count, 0,
        "no lock evidence without a rate"
    );
    assert_eq!(w.phase_name, "ACQ");
    let m = servo.measure_offset(133.0, MonoNs(500_000_000));
    assert!(m.measured);
    assert!(
        (m.raw_rate_ppm - 20.0).abs() < 1e-9,
        "10 µs in 0.5 s = 20 µs/s"
    );
}

#[test]
fn a_window_shorter_than_a_tenth_of_a_second_carries_the_smoothed_rate() {
    let mut servo = RateServo::<MonoNs>::new();
    servo.measure_offset(0.0, MonoNs(0));
    servo.smoothed_rate_ppm = 7.5;
    let m = servo.measure_offset(1_000.0, MonoNs(50_000_000));
    assert!(m.measured);
    assert_eq!(m.raw_rate_ppm, 7.5);
}

#[test]
fn a_stable_offset_locks_after_the_stable_count_of_measured_windows() {
    let mut servo = RateServo::<MonoNs>::new();
    for i in 0..LOCK_STABLE_COUNT {
        window(&mut servo, 40.0, i as f64 * 0.5, false);
        assert!(!servo.is_locked, "locked after {} windows", i + 1);
    }
    // The first window was not a measurement: one more locks.
    window(&mut servo, 40.0, LOCK_STABLE_COUNT as f64 * 0.5, false);
    assert!(servo.is_locked);
    assert!(servo.in_production_mode);
    assert_eq!(servo.status_name("PROD"), "LOCK");
}

/// A fast clock grows the offset; the servo learns a NEGATIVE word (offset = local − master).
#[test]
fn a_growing_offset_drives_the_word_negative_and_the_integrator_learns_it() {
    let mut servo = RateServo::<MonoNs>::new();
    let mut offset = 0.0;
    let mut last = ServoWord {
        word_ppm: 0.0,
        phase_name: "ACQ",
    };
    // A closed loop: the clock runs +30 ppm fast, minus whatever word the servo applies.
    for i in 0..400 {
        last = window(&mut servo, offset, i as f64 * 0.5, false);
        offset += (30.0 + last.word_ppm) * 0.5;
    }
    assert!(
        (last.word_ppm + 30.0).abs() < 1.0,
        "the word follows the oscillator: {}",
        last.word_ppm
    );
    assert!(
        (servo.drift_baseline_ppm + 30.0).abs() < 1.0,
        "the integrator learned it: {}",
        servo.drift_baseline_ppm
    );
    assert!(servo.is_locked);
}

/// dsync#170: the phase lock engaged on its fit before this servo counted a lock: the servo takes
/// it as its own, with a full count, and keeps it while held; a lock already counted keeps its
/// count.
#[test]
fn the_phase_locks_engagement_is_adopted_as_the_rate_lock_170() {
    let mut s = RateServo::<MonoNs>::new();
    s.word(12.0, true, false);
    assert!(!s.is_locked);
    s.adopt_lock();
    assert!(s.is_locked);
    assert_eq!(s.lock_stable_count, LOCK_STABLE_COUNT);
    for _ in 0..10 {
        s.word(40.0, true, true);
    }
    assert!(s.is_locked, "held while the phase lock owns the word");
    let mut counted = RateServo::<MonoNs>::new();
    counted.is_locked = true;
    counted.lock_stable_count = LOCK_STABLE_COUNT + 7;
    counted.adopt_lock();
    assert_eq!(counted.lock_stable_count, LOCK_STABLE_COUNT + 7);
}

#[test]
fn an_unstable_window_unlocks_gradually_unless_the_phase_lock_holds_it_146() {
    let locked = || {
        let mut s = RateServo::<MonoNs>::new();
        s.is_locked = true;
        s.lock_stable_count = 2;
        s
    };
    let mut held = locked();
    held.word(40.0, true, true);
    held.word(40.0, true, true);
    held.word(40.0, true, true);
    assert!(held.is_locked, "held while the phase lock owns the word");
    assert_eq!(held.lock_stable_count, 2);

    let mut free = locked();
    free.word(40.0, true, false);
    assert!(free.is_locked, "gradual: one window takes one count");
    free.word(40.0, true, false);
    assert!(!free.is_locked);
}

#[test]
fn nano_needs_the_sustain_count_while_locked_and_exits_with_hysteresis() {
    let mut servo = RateServo::<MonoNs>::new();
    servo.update_modes(0.1);
    assert_eq!(servo.nano_sustain_count, 0, "never NANO before the lock");
    servo.is_locked = true;
    for _ in 0..NANO_SUSTAIN_COUNT {
        servo.update_modes(0.1);
    }
    assert!(servo.in_nano_mode);
    assert_eq!(servo.status_name("PROD"), "NANO");
    for _ in 0..NANO_EXIT_COUNT - 1 {
        servo.update_modes(2.0);
    }
    assert!(servo.in_nano_mode, "hysteresis");
    servo.update_modes(2.0);
    assert!(!servo.in_nano_mode);
}

#[test]
fn the_nano_deadband_ignores_a_noise_floor_rate() {
    let mut servo = RateServo::<MonoNs>::new();
    servo.in_nano_mode = true;
    servo.drift_baseline_ppm = 3.0;
    let w = servo.word(0.05, true, false);
    assert_eq!(w.word_ppm, 3.0);
    assert_eq!(w.phase_name, "NANO");
}

#[test]
fn the_word_and_the_integrator_are_clamped_to_the_drift_bound() {
    let mut servo = RateServo::<MonoNs>::new();
    servo.drift_baseline_ppm = DRIFT_MAX_PPM;
    let w = servo.word(-400.0, true, false);
    assert_eq!(servo.drift_baseline_ppm, DRIFT_MAX_PPM);
    assert_eq!(w.word_ppm, DRIFT_MAX_PPM);
}

#[test]
fn the_word_deviation_is_the_average_of_the_last_two_words_off_the_integrator_146() {
    let mut d = WordDeviation::default();
    d.note(Some(12.0), 10.0);
    assert_eq!(d.ppm, [0.0, 2.0]);
    d.note(Some(16.0), 10.0);
    assert_eq!(d.span_ppm(), 4.0);
    d.note(None, 10.0);
    assert_eq!(d.ppm, [6.0, 0.0]);
    d.clear();
    assert_eq!(d.span_ppm(), 0.0);
}
