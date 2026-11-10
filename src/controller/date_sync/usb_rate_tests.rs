//! dsync#160 (mbc-snv, the 1.29.0 roll, 10.11.2026): the SERVICE on a USB NIC. The phase lock was
//! given the delay floor, but the rate servo still differentiated each window's MEDIAN: on the
//! ASIX's one-sided delay it read ±30-40 µs/s, flipped ACQ/PROD every 30 s and never locked, so
//! the phase lock never engaged (live: `ptp_phase_locked` false, `e` ~+500 µs open-loop, the
//! #142 seed rejected at −107 µs/s). The in-process follower feeds its rate servo the floor and
//! locks on the same model. Driven through the REAL sample path (`process_settled_sync`, the
//! 4-sample window close), closed loop: the frequency word the controller writes moves the
//! offset the next samples see.

use super::lock_sim::Noise;
use super::tests::{phase_lock_config, PL_GM};
use super::*;
use crate::clock::MockSystemClock;
use crate::traits::{MockNtpSource, MockPtpNetwork};
use std::sync::Mutex;

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

/// 8 Syncs a second, as the Dante grandmaster.
const SYNC_NS: i64 = 125_000_000;
/// The oscillator against the grandmaster (mbc's learned word was about −4.5 ppm).
const NATURAL_PPM: f64 = 4.5;

/// mbc's ASIX one-sided receive delay: the per-10 s minimum sits on one line (sd 7.6 µs), while
/// most samples arrive late by up to milliseconds. A sample is on time (only a few µs of jitter)
/// one time in four; otherwise late by an exponential delay of mean 400 µs.
fn usb_delay_us(noise: &mut Noise) -> f64 {
    let jitter = (5.0 * noise.gauss()).abs();
    if noise.uniform() < 0.25 {
        return jitter;
    }
    jitter - 400.0 * noise.uniform().max(1e-9).ln()
}

fn usb_service() -> (Ctl, Arc<Mutex<f64>>) {
    let word = Arc::new(Mutex::new(0.0_f64));
    let w = word.clone();
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(move |factor| {
        *w.lock().expect("word") = (factor - 1.0) * 1_000_000.0;
        Ok(())
    });
    let mut c: Ctl = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        phase_lock_config(),
    );
    c.current_gm_uuid = Some(PL_GM);
    (c, word)
}

#[test]
fn the_service_on_a_usb_nic_locks_its_rate_and_engages_the_phase_lock_160() {
    for seed in [0x5eed_1601_u64, 0x5eed_1602, 0x5eed_1603] {
        let (mut c, word) = usb_service();
        let mut noise = Noise(seed);
        let base_ns: i64 = 1_791_000_000_000_000_000;
        let mut t1: i64 = 5_000 * 1_000_000_000;
        let mut true_offset_us = 0.0_f64;
        let mut locked_at = None;
        let mut worst_e_after_us = 0.0_f64;
        // 4 minutes of Syncs.
        for k in 0..(240 * 8) {
            let applied = *word.lock().expect("word");
            true_offset_us += (NATURAL_PPM + applied) * SYNC_NS as f64 / 1e9;
            t1 += SYNC_NS;
            let rx_us = true_offset_us + usb_delay_us(&mut noise);
            let t2 = t1 + base_ns + (rx_us * 1_000.0).round() as i64;
            let phase = c.calculate_phase_offset(t1, t2);
            c.process_settled_sync(t1, t2, phase);
            let s = k as f64 / 8.0;
            if locked_at.is_none() && c.date_sync.core.engaged() {
                locked_at = Some(s);
            }
            if s >= 180.0 {
                if let Some(e) = c.date_sync.core.last_error_ns() {
                    worst_e_after_us = worst_e_after_us.max((e as f64 / 1_000.0).abs());
                }
            }
        }
        let locked_at = locked_at.unwrap_or_else(|| {
            panic!(
                "seed {seed:#x}: the phase lock never engaged (rate servo locked: {}, smoothed \
                 {:.1} µs/s)",
                c.rate.is_locked, c.rate.smoothed_rate_ppm
            )
        });
        assert!(
            locked_at <= 120.0,
            "seed {seed:#x}: engaged only at {locked_at} s"
        );
        assert!(
            c.date_sync.core.engaged(),
            "seed {seed:#x}: the phase lock dropped again"
        );
        assert!(
            worst_e_after_us <= 50.0,
            "seed {seed:#x}: |e| {worst_e_after_us:.1} µs over the last minute"
        );
    }
}
