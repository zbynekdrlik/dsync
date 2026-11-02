use super::*;

fn reading(mono_ns: i64, wall_minus_mono_ns: i64) -> TimeBaseReading {
    TimeBaseReading {
        mono_ns,
        wall_minus_mono_ns,
    }
}

#[test]
fn the_raw_counter_never_runs_backwards() {
    let mut prev = mono_now_ns();
    for _ in 0..10_000 {
        let now = mono_now_ns();
        assert!(now >= prev);
        prev = now;
    }
}

#[test]
fn a_reading_converts_the_wall_stamp_of_that_instant_back_to_the_counter() {
    let r = read_time_base();
    assert_eq!(r.to_mono(r.mono_ns + r.wall_minus_mono_ns), r.mono_ns);
    // A real wall reading taken now converts to within the read spread of the counter now.
    let w = wall_now_ns();
    let m = mono_now_ns();
    let back = read_time_base().to_mono(w);
    assert!((back - m).abs() < 5_000_000, "{} vs {}", back, m);
}

#[test]
fn a_frequency_word_moves_the_offset_without_reading_as_a_step() {
    let mut c = StampConverter::new();
    assert_eq!(c.observe(reading(0, 1_000)), Continuity::Continuous);
    // 500 ppm over 1 s = 500 µs: still continuous (plus the 20 µs tolerance).
    assert_eq!(
        c.observe(reading(1_000_000_000, 1_000 + 519_000)),
        Continuity::Continuous
    );
    assert_eq!(c.steps(), 0);
}

#[test]
fn a_step_between_two_readings_is_detected_with_its_size() {
    let mut c = StampConverter::new();
    c.observe(reading(0, 5_000));
    // 1 ms later the wall jumped 200 ms forward.
    assert_eq!(
        c.observe(reading(1_000_000, 200_005_000)),
        Continuity::Stepped { by_ns: 200_000_000 }
    );
    // A backward step of 30 µs within 1 ms is beyond 20 µs + 0.5 µs.
    assert_eq!(
        c.observe(reading(2_000_000, 199_975_000)),
        Continuity::Stepped { by_ns: -30_000 }
    );
    assert_eq!(c.steps(), 2);
    // A step under the tolerance is not seen (one sample at most, the median absorbs it).
    assert_eq!(
        c.observe(reading(3_000_000, 199_990_000)),
        Continuity::Continuous
    );
}
