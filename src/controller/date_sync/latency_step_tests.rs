//! dsync#220 (dstack#498): the SERVICE reports an arrival-latency step as the in-process
//! follower does (#218). alex-nb's clock source is the service (its USB RTL8153 delivers sparse
//! inbound frames ~1 ms late, promptly once an RX flow runs); its log showed only DSYX [41]
//! flaps. Driven through the REAL sample path (`process_settled_sync`, closed loop), as
//! `usb_rate_tests`: the word the controller writes moves the offset the next samples see.

use super::lock_sim::Noise;
use super::usb_rate_tests::{usb_delay_us, usb_service, Ctl};

/// 8 Syncs a second, as the Dante grandmaster.
const SYNC_NS: i64 = 125_000_000;
const NATURAL_PPM: f64 = 4.5;

/// Drives the service `secs` seconds; `delivery_us(s)` is the NIC's delivery delay at second
/// `s`, `jitter_us` the receive noise on top. Returns the steps counted by `count_from_s` and
/// the steps at the end.
fn drive(
    c: &mut Ctl,
    word: &std::sync::Mutex<f64>,
    seed: u64,
    secs: u32,
    delivery_us: impl Fn(f64) -> f64,
    jitter_us: impl Fn(&mut Noise) -> f64,
    count_from_s: f64,
) -> (u32, u32) {
    let mut noise = Noise(seed);
    let base_ns: i64 = 1_791_000_000_000_000_000;
    let mut t1: i64 = 5_000 * 1_000_000_000;
    let mut true_offset_us = 0.0_f64;
    let mut before = None;
    for k in 0..(secs as i64 * 8) {
        let s = k as f64 / 8.0;
        if before.is_none() && s >= count_from_s {
            before = Some(c.date_sync.arrival_latency_steps());
        }
        let applied = *word.lock().expect("word");
        true_offset_us += (NATURAL_PPM + applied) * SYNC_NS as f64 / 1e9;
        t1 += SYNC_NS;
        let rx_us = true_offset_us + delivery_us(s) + jitter_us(&mut noise);
        let t2 = t1 + base_ns + (rx_us * 1_000.0).round() as i64;
        let phase = c.calculate_phase_offset(t1, t2);
        c.process_settled_sync(t1, t2, phase);
    }
    (before.unwrap_or(0), c.date_sync.arrival_latency_steps())
}

fn clean(noise: &mut Noise) -> f64 {
    (5.0 * noise.gauss()).abs()
}

#[test]
fn an_arrival_latency_step_down_is_counted_once_by_the_service_220() {
    for seed in [0x5eed_2201_u64, 0x5eed_2202] {
        let (mut c, word) = usb_service();
        // Sparse frames 800 µs late for 150 s, then an RX flow: delivered promptly.
        let (before, after) = drive(
            &mut c,
            &word,
            seed,
            210,
            |s| if s < 150.0 { 800.0 } else { 0.0 },
            clean,
            150.0,
        );
        assert!(c.date_sync.core.engaged(), "seed {seed:#x}: not engaged");
        assert_eq!(
            (before, after),
            (0, 1),
            "seed {seed:#x}: steps before / after the 800 µs drop"
        );
    }
}

#[test]
fn an_arrival_latency_step_up_is_counted_once_by_the_service_220() {
    for seed in [0x5eed_2203_u64, 0x5eed_2204] {
        let (mut c, word) = usb_service();
        let (before, after) = drive(
            &mut c,
            &word,
            seed,
            210,
            |s| if s < 150.0 { 0.0 } else { 600.0 },
            clean,
            150.0,
        );
        assert_eq!(
            (before, after),
            (0, 1),
            "seed {seed:#x}: steps before / after the 600 µs rise"
        );
    }
}

#[test]
fn a_usb_nics_receive_jitter_is_no_latency_step_in_the_service_220() {
    for seed in [0x5eed_2205_u64, 0x5eed_2206, 0x5eed_2207] {
        let (mut c, word) = usb_service();
        let (_, after) = drive(&mut c, &word, seed, 240, |_| 0.0, usb_delay_us, 0.0);
        assert!(c.date_sync.core.engaged(), "seed {seed:#x}: not engaged");
        assert_eq!(
            after, 0,
            "seed {seed:#x}: mbc's USB delay model read as a step"
        );
    }
}

#[test]
fn the_status_carries_the_arrival_latency_steps_220() {
    let (mut c, word) = usb_service();
    drive(
        &mut c,
        &word,
        0x5eed_2208,
        210,
        |s| if s < 150.0 { 800.0 } else { 0.0 },
        clean,
        0.0,
    );
    c.update_shared_status();
    let status = c.status_shared.read().expect("status");
    assert_eq!(status.arrival_latency_steps, 1);
}

#[test]
fn a_grandmaster_change_is_no_latency_step_in_the_service_220() {
    for seed in [0x5eed_2209_u64, 0x5eed_220a] {
        let (mut c, word) = usb_service();
        let mut noise = Noise(seed);
        let base_ns: i64 = 1_791_000_000_000_000_000;
        let mut t1: i64 = 5_000 * 1_000_000_000;
        let mut true_offset_us = 0.0_f64;
        for k in 0..(240 * 8) {
            let s = k as f64 / 8.0;
            if k == 150 * 8 {
                // A failover to a grandmaster whose time is 600 µs off the old one (the same
                // line within 1 ms: a continued time base, #158).
                c.current_gm_uuid = Some([0, 0x1d, 0xc1, 0x8b, 0x5f, 0x59]);
                c.on_grandmaster_uuid_change();
            }
            let applied = *word.lock().expect("word");
            true_offset_us += (NATURAL_PPM + applied) * SYNC_NS as f64 / 1e9;
            t1 += SYNC_NS;
            let shift_us = if s >= 150.0 { 600.0 } else { 0.0 };
            let rx_us = true_offset_us + shift_us + clean(&mut noise);
            let t2 = t1 + base_ns + (rx_us * 1_000.0).round() as i64;
            let phase = c.calculate_phase_offset(t1, t2);
            c.process_settled_sync(t1, t2, phase);
        }
        assert!(c.date_sync.core.engaged(), "seed {seed:#x}: not engaged");
        assert_eq!(
            c.date_sync.arrival_latency_steps(),
            0,
            "seed {seed:#x}: a grandmaster change read as a latency step"
        );
    }
}

#[test]
fn a_prompt_run_shorter_than_the_hold_is_no_latency_step_in_the_service_220() {
    for seed in [0x5eed_220b_u64, 0x5eed_220c] {
        let (mut c, word) = usb_service();
        // 800 µs late, then delivered promptly for 1 s (two windows), then late again.
        let (_, after) = drive(
            &mut c,
            &word,
            seed,
            210,
            |s| {
                if (150.0..151.0).contains(&s) {
                    0.0
                } else {
                    800.0
                }
            },
            clean,
            0.0,
        );
        assert_eq!(after, 0, "seed {seed:#x}: a 1 s prompt run read as a step");
    }
}
