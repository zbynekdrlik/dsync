//! camera-box issue 1372 — the controller wiring of the phase lock's grandmaster FREQUENCY-STEP
//! follow: the word follows the step within seconds, `/status` publishes the event (count, size,
//! time), and the fleet `D` and its seq stay where they were. The clock is never stepped (the mock
//! has no `step_clock` expectation, so any step panics).

use super::tests::{phase_lock_config, PL_GM, PL_PTP_NOW_NS};
use super::*;
use crate::clock::MockSystemClock;
use crate::traits::{MockNtpSource, MockPtpNetwork};

#[test]
fn a_grandmaster_frequency_step_is_followed_published_and_never_moves_d_1372() {
    let words = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let cap = words.clone();
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(move |factor| {
        cap.lock().expect("cap").push((factor - 1.0) * 1e6);
        Ok(())
    });
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        phase_lock_config(),
    );
    // The NTP master (the date-offset authority), locked, the word handed over at 0 ppm.
    c.configure_ntp_server_mode(100_000);
    c.current_gm_uuid = Some(PL_GM);
    c.rate.is_locked = true;
    let d = wall_now_ns() - PL_PTP_NOW_NS;

    // A box whose oscillator the word already holds (0 ppm); after 30 s the grandmaster speeds up
    // by 25 ppm: `e = t2 − t1 − D` then falls at 25 µs/s until the word follows.
    let step_at = 60;
    let mut e_ns = 0.0f64;
    let mut before = None;
    let mut followed_at = None;
    for w in 0..(step_at + 300) {
        c.date_sync.pending_median_ns = Some(d + e_ns.round() as i64);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + w * 500_000_000;
        c.servo_window_after(0.0, 0.5);
        if w == step_at - 1 {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert_eq!(st.freq_steps, 0, "nothing to follow before the step");
            assert_eq!(st.last_freq_step_ppm, None);
            before = Some((st.date_offset_ns, st.date_offset_seq));
        }
        if followed_at.is_none() && c.date_sync.core.freq_steps() == 1 {
            followed_at = Some(w);
        }
        let word = *words
            .lock()
            .expect("cap")
            .last()
            .expect("a word per window");
        let gm_ppm = if w >= step_at { 25.0 } else { 0.0 };
        e_ns += (word - gm_ppm) * 0.5 * 1_000.0;
    }
    let followed_at = followed_at.expect("the step is followed");
    assert!(
        (followed_at - step_at + 1) as f64 * 0.5 <= 30.0,
        "followed {} windows after the step",
        followed_at - step_at + 1
    );
    let last_word = *words.lock().expect("cap").last().expect("a word");
    assert!(
        (last_word - 25.0).abs() < 1.0,
        "the word follows the grandmaster: {last_word}"
    );

    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.freq_steps, 1);
    let step = st.last_freq_step_ppm.expect("published");
    assert!((step - 25.0).abs() < 1.0, "the step followed: {step}");
    let ts = st.last_freq_step_ts.expect("published") as i64;
    assert!(
        (ts - wall_now_ns() / 1_000_000_000).abs() <= 5,
        "stamped now: {ts}"
    );
    // D and its seq: exactly as before the step.
    assert_eq!(Some((st.date_offset_ns, st.date_offset_seq)), before);
    assert_eq!(st.date_offset_ns, Some(d));
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
    assert!(st.ptp_phase_locked);
}

/// dsync#204 — the controller wiring of the excursion boost: a local-oscillator ramp of 6 ppm over
/// 2 minutes (resolume-pp's load, 19.11.2026) arms the boost, `/status` publishes the count and the
/// gain in force, the phase stays within 50 µs, and the fleet `D` never moves.
#[test]
fn a_local_oscillator_ramp_arms_the_boost_and_is_published_204() {
    let words = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let cap = words.clone();
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(move |factor| {
        cap.lock().expect("cap").push((factor - 1.0) * 1e6);
        Ok(())
    });
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        phase_lock_config(),
    );
    c.configure_ntp_server_mode(100_000);
    c.current_gm_uuid = Some(PL_GM);
    c.rate.is_locked = true;
    let d = wall_now_ns() - PL_PTP_NOW_NS;

    let ramp_at = 720; // past the excursion holdoff after the engagement
    let mut e_ns = 0.0f64;
    let mut worst_us = 0.0f64;
    let mut gain_seen = 1.0f64;
    for w in 0..(ramp_at + 1_200) {
        c.date_sync.pending_median_ns = Some(d + e_ns.round() as i64);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + w * 500_000_000;
        c.servo_window_after(0.0, 0.5);
        if w == ramp_at - 1 {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert_eq!(st.ptp_phase_excursion_boosts, 0, "nothing before the ramp");
            assert_eq!(st.ptp_phase_gain, Some(1.0), "the plain PI before the ramp");
        }
        if w >= ramp_at {
            worst_us = worst_us.max(e_ns.abs() / 1_000.0);
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            gain_seen = gain_seen.max(st.ptp_phase_gain.expect("engaged"));
        }
        let word = *words
            .lock()
            .expect("cap")
            .last()
            .expect("a word per window");
        // The oscillator runs fast by 6 ppm more over 2 minutes: the word must go down by 6 ppm.
        let t = (w - ramp_at).max(0) as f64 * 0.5;
        let osc_ppm = 6.0 * (t / 120.0).clamp(0.0, 1.0);
        e_ns += (word + osc_ppm) * 0.5 * 1_000.0;
    }
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert!(
        st.ptp_phase_excursion_boosts >= 1,
        "the excursion armed the boost"
    );
    assert!(gain_seen > 3.9, "the boost reached x4: {gain_seen}");
    assert!(worst_us < 50.0, "the phase held: {worst_us} us");
    assert_eq!(st.freq_steps, 0, "a ramp is not a step");
    assert_eq!(st.date_offset_ns, Some(d));
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
}
