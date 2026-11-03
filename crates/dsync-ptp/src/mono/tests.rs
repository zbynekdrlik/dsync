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

/// dsync#136, CI run 41558600507: two followers on one PC read the clocks at different delays
/// after the same packet. While the host's wall slews (here 500 ppm, the most a frequency word
/// allows), a conversion with the reading AFTER the receive is off by slew × delay, so the two
/// disagreed (4.5 µs). Converted with `wall − raw` interpolated at the stamp, both land on the
/// stamp's raw instant whatever their delay.
#[test]
fn a_stamp_converts_to_the_same_raw_instant_whatever_the_read_delay_under_a_slew() {
    const W0: i64 = 1_789_000_000_000_000_000;
    // wall − raw grows 500 ns per ms of raw time.
    let wmm = |m: i64| W0 + m / 2_000;
    let at = |m: i64| reading(m, wmm(m));
    let stamp_mono = 10_000_000;
    let stamp_wall = stamp_mono + wmm(stamp_mono);

    // Follower A reads 0.2 ms after the packet; follower B stalled 15 ms.
    let a = at(10_200_000).to_mono_between(Some(at(9_500_000)), stamp_wall);
    let b = at(25_000_000).to_mono_between(Some(at(9_900_000)), stamp_wall);
    assert!((a - stamp_mono).abs() <= 1, "A off by {}ns", a - stamp_mono);
    assert!((b - stamp_mono).abs() <= 1, "B off by {}ns", b - stamp_mono);
    // The reading-after conversion alone is off by slew × delay (7.5 µs for B).
    assert_eq!(stamp_mono - at(25_000_000).to_mono(stamp_wall), -7_500);
}

#[test]
fn a_stamp_outside_the_bracket_uses_the_nearest_reading_and_no_bracket_the_current_one() {
    const W0: i64 = 1_789_000_000_000_000_000;
    let prev = reading(5_000_000, W0);
    let cur = reading(6_000_000, W0 + 500);
    // A packet queued before the previous reading: the previous reading converts it.
    let early = 4_000_000 + W0;
    assert_eq!(cur.to_mono_between(Some(prev), early), prev.to_mono(early));
    // A stamp at or after the current reading: the current one.
    let late = 6_000_000 + W0 + 500;
    assert_eq!(cur.to_mono_between(Some(prev), late), cur.to_mono(late));
    // No previous reading: the current one.
    assert_eq!(cur.to_mono_between(None, early), cur.to_mono(early));
    // A previous reading not before the current one (no bracket): the current one.
    assert_eq!(cur.to_mono_between(Some(cur), early), cur.to_mono(early));
}
