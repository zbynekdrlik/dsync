//! dsync#142 — the controller's drift-file glue: the seed at start (written to the clock
//! before the warmup), every fallback to the cold learn with its reason, the save only while
//! LOCKED (every 10 minutes and at shutdown), the rejection, and the `/status` fields.

use super::super::*;
use super::DRIFT_SAVE_INTERVAL;
use crate::clock::MockSystemClock;
use crate::drift_file::{self, DriftRecord};
use crate::traits::{MockNtpSource, MockPtpNetwork};
use std::path::Path;

type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

/// A controller whose clock records every frequency word written (ppm).
fn controller() -> (Ctl, Arc<std::sync::Mutex<Vec<f64>>>) {
    let words = Arc::new(std::sync::Mutex::new(Vec::new()));
    let w = words.clone();
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(move |factor| {
        w.lock().expect("words").push((factor - 1.0) * 1_000_000.0);
        Ok(())
    });
    let mut config = SystemConfig::default();
    config.filters.calibration_samples = 0;
    config.filters.warmup_secs = 0.0;
    let c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    );
    (c, words)
}

fn now_s() -> i64 {
    wall_now_ns() / 1_000_000_000
}

fn write_record(path: &Path, freq_ppm: f64, age_s: i64) {
    drift_file::save(
        path,
        &DriftRecord {
            freq_ppm,
            written_unix_s: now_s() - age_s,
        },
    )
    .expect("saved");
}

fn status(c: &Ctl) -> SyncStatus {
    c.status_shared.read().expect("status").clone()
}

#[test]
fn a_usable_drift_file_seeds_the_servo_and_the_clock_before_the_warmup_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    write_record(&path, 35.25, 600);
    let (mut c, words) = controller();
    c.open_drift_file(&path);

    assert_eq!(c.rate.drift_baseline_ppm, 35.25);
    assert_eq!(c.applied_freq_ppm, 35.25);
    let written = words.lock().expect("words").clone();
    assert_eq!(written.len(), 1, "written to the clock at once");
    assert!((written[0] - 35.25).abs() < 1e-6, "{written:?}");
    assert!(c.freq_seed.judge.is_some(), "judged on the first windows");
    let s = status(&c);
    assert_eq!(s.freq_seed_ppm, Some(35.25));
    assert_eq!(s.freq_seed_source.as_deref(), Some("file"));
    // LOCK is not touched by a seed.
    assert!(!c.rate.is_locked);
    assert_eq!(c.rate.lock_stable_count, 0);
}

#[test]
fn every_unusable_file_falls_back_to_the_cold_learn_and_says_why_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cases: Vec<(&str, Box<dyn Fn(&Path)>, &str)> = vec![
        ("missing", Box::new(|_| {}), "none: no drift file yet"),
        (
            "malformed",
            Box::new(|p| std::fs::write(p, b"{\"version\":1,\"freq").expect("w")),
            "none: not a drift record",
        ),
        (
            "too old",
            Box::new(|p| write_record(p, 35.0, 31 * 86_400)),
            "none: 31 days old",
        ),
        (
            "out of range",
            Box::new(|p| write_record(p, 600.0, 60)),
            "none: 600 ppm is outside",
        ),
        (
            "another version",
            Box::new(|p| {
                std::fs::write(p, b"{\"version\":9,\"freq_ppm\":35.0,\"written_unix_s\":1}")
                    .expect("w")
            }),
            "none: version 9",
        ),
    ];
    for (name, prepare, expected) in cases {
        let path = dir.path().join(format!("{}.json", name.replace(' ', "-")));
        prepare(&path);
        let (mut c, words) = controller();
        c.open_drift_file(&path);
        assert_eq!(c.rate.drift_baseline_ppm, 0.0, "{name}: learn from 0");
        assert!(
            words.lock().expect("words").is_empty(),
            "{name}: no seed written"
        );
        assert!(c.freq_seed.judge.is_none(), "{name}");
        let s = status(&c);
        assert_eq!(s.freq_seed_ppm, None, "{name}");
        let source = s.freq_seed_source.expect("source");
        assert!(source.starts_with(expected), "{name}: {source}");
    }
}

#[test]
fn the_drift_file_is_saved_only_while_locked_every_ten_minutes_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    let (mut c, _) = controller();
    c.open_drift_file(&path);
    c.rate.drift_baseline_ppm = 35.5;

    // Not locked: never saved, no timer.
    c.tick_drift_file();
    assert!(c.freq_seed.next_save.is_none());
    assert!(!path.exists());

    // dsync#146: the rate servo's lock alone is not "locked" — [41] (the phase within the
    // bound) is; an engaged phase lock holds the rate lock while it still slews.
    c.rate.is_locked = true;
    c.tick_drift_file();
    assert!(
        c.freq_seed.next_save.is_none(),
        "rate-locked only: no timer"
    );

    // Locked: the first save is one interval after the lock, not at once.
    c.lock_reported_for_test();
    c.tick_drift_file();
    let due = c.freq_seed.next_save.expect("armed at the lock");
    assert!(due > Instant::now() + DRIFT_SAVE_INTERVAL - Duration::from_secs(5));
    assert!(!path.exists());

    // The interval passed: saved, re-armed.
    c.freq_seed.next_save = Some(Instant::now() - Duration::from_millis(1));
    c.tick_drift_file();
    let saved = drift_file::load_seed(&path, now_s(), 500.0).expect("saved");
    assert_eq!(saved.freq_ppm, 35.5);
    assert!(c.freq_seed.next_save.expect("re-armed") > Instant::now());

    // Lock lost (or PTP offline): the timer stops, nothing more is written.
    c.rate.drift_baseline_ppm = 99.0;
    c.ptp_offline = true;
    c.tick_drift_file();
    assert!(c.freq_seed.next_save.is_none());
    c.ptp_offline = false;
    c.rate.is_locked = false;
    c.clear_reported_lock();
    c.tick_drift_file();
    assert_eq!(
        drift_file::load_seed(&path, now_s(), 500.0)
            .expect("kept")
            .freq_ppm,
        35.5
    );
}

#[test]
fn shutdown_saves_only_a_locked_frequency_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    let (mut c, _) = controller();
    c.open_drift_file(&path);
    c.rate.drift_baseline_ppm = 12.0;
    c.save_drift_file_at_shutdown();
    assert!(!path.exists(), "not locked: nothing saved");

    c.rate.is_locked = true;
    c.save_drift_file_at_shutdown();
    assert!(
        !path.exists(),
        "rate-locked only, [41] not set (#146): nothing saved"
    );

    c.lock_reported_for_test();
    c.ptp_offline = true;
    c.save_drift_file_at_shutdown();
    assert!(!path.exists(), "PTP offline: not a locked value");

    c.ptp_offline = false;
    c.save_drift_file_at_shutdown();
    let saved = drift_file::load_seed(&path, now_s(), 500.0).expect("saved");
    assert_eq!(saved.freq_ppm, 12.0);
    assert!(!dir.path().join("drift.json.tmp").exists());
}

#[test]
fn a_controller_without_a_drift_file_never_writes_one_142() {
    let (mut c, _) = controller();
    c.rate.is_locked = true;
    c.lock_reported_for_test();
    c.tick_drift_file();
    c.save_drift_file_at_shutdown();
    assert!(c.freq_seed.next_save.is_none());
    assert_eq!(status(&c).freq_seed_source, None);
}

/// One window 0.5 s after the previous one.
fn window(c: &mut Ctl, offset_us: f64) {
    if c.rate.last_offset_us.is_some() {
        c.rate.last_offset_time = Some(Instant::now() - Duration::from_millis(500));
    }
    c.apply_self_tuning_servo(offset_us);
}

#[test]
fn a_seed_the_first_measured_windows_disagree_with_is_dropped_before_any_lock_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    write_record(&path, 85.0, 600);
    let (mut c, _) = controller();
    c.open_drift_file(&path);

    // The oscillator is 50 ppm away from the seed: the offset runs at +50 µs/s against it.
    let mut offset = 0.0;
    window(&mut c, offset); // no rate yet
    for n in 0..drift_file::SEED_JUDGE_WINDOWS {
        assert!(c.freq_seed.judge.is_some());
        let word = c.applied_freq_ppm;
        offset += (50.0 + (word - 85.0)) * 0.5;
        if n + 1 == drift_file::SEED_JUDGE_WINDOWS {
            // Whatever the lock count reached against the seed, a rejection starts it over.
            c.rate.lock_stable_count = 3;
        }
        window(&mut c, offset);
    }
    assert!(c.freq_seed.judge.is_none(), "judged");
    let source = status(&c).freq_seed_source.expect("source");
    assert!(source.starts_with("rejected: "), "{source}");
    assert_eq!(c.rate.lock_stable_count, 0, "the lock count starts over");
    assert_eq!(
        c.rate.spike_filter.window_len(),
        1,
        "the spike filter starts over (only the rebased window in it)"
    );
    assert_eq!(
        c.rate.jitter_estimator.sample_count(),
        1,
        "and the jitter estimate (only the rebased window)"
    );
    assert!(source.contains("us/s off the seed"), "{source}");
    assert_eq!(
        status(&c).freq_seed_ppm,
        Some(85.0),
        "the rejected seed stays visible"
    );
    assert!(
        c.rate.drift_baseline_ppm.abs() < 10.0,
        "back to the cold learn (one window's integrator step): {}",
        c.rate.drift_baseline_ppm
    );
    // Rebased to a word of 0: the oscillator needs +35 (85 − 50), so the integrator's first step
    // from 0 goes up, and the smoothed rate reads the oscillator against 0 (about −35).
    assert!(
        c.rate.drift_baseline_ppm > 0.0,
        "{}",
        c.rate.drift_baseline_ppm
    );
    assert!(
        c.rate.smoothed_rate_ppm < -20.0,
        "{}",
        c.rate.smoothed_rate_ppm
    );
    assert!(!c.rate.is_locked);
}

#[test]
fn a_seed_the_first_measured_windows_agree_with_is_kept_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    write_record(&path, 35.0, 600);
    let (mut c, _) = controller();
    c.open_drift_file(&path);
    let mut offset = 0.0;
    window(&mut c, offset);
    for _ in 0..drift_file::SEED_JUDGE_WINDOWS {
        offset += (1.5 + (c.applied_freq_ppm - 35.0)) * 0.5;
        window(&mut c, offset);
    }
    assert!(c.freq_seed.judge.is_none(), "judged");
    assert_eq!(status(&c).freq_seed_source.as_deref(), Some("file"));
    assert!((c.rate.drift_baseline_ppm - 35.0).abs() < 1.0);
}

#[test]
fn the_judge_charges_each_window_half_to_the_previous_word_and_half_to_the_last_142() {
    // A window's median sits mid-window: its rate spans half the previous window (under
    // `word_before`) and half this one (under `word_last`). With the seed 35 between a word of 5
    // and one of 65, a raw rate of 0 is the oscillator exactly at the seed. Charged to the last
    // word alone it would read −30 µs/s off the seed, to the previous word alone +30: both a
    // rejection.
    fn verdict(raw_rate_ppm: f64) -> String {
        let (mut c, _) = controller();
        c.freq_seed.seed_ppm = Some(35.0);
        c.freq_seed.source = "file".to_string();
        c.freq_seed.judge = Some(crate::drift_file::SeedJudge::default());
        for _ in 0..drift_file::SEED_JUDGE_WINDOWS {
            c.freq_seed.word_before = 5.0;
            c.freq_seed.word_last = 65.0;
            c.judge_freq_seed(raw_rate_ppm, 0.5);
        }
        assert!(c.freq_seed.judge.is_none(), "judged");
        c.freq_seed.source.clone()
    }
    assert_eq!(verdict(0.0), "file");
    assert_eq!(verdict(19.0), "file");
    assert!(verdict(21.0).contains("+21.0 us/s"), "{}", verdict(21.0));
    assert!(verdict(-21.0).contains("-21.0 us/s"), "{}", verdict(-21.0));
}

#[test]
fn a_failing_save_warns_once_and_recovers_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut c, _) = controller();
    c.rate.is_locked = true;
    c.lock_reported_for_test();
    c.rate.drift_baseline_ppm = 20.0;
    // A directory that does not exist (a read-only root answers EROFS the same way: no file).
    let missing = dir.path().join("no-such-dir").join("drift.json");
    c.open_drift_file(&missing);
    c.save_drift_file_at_shutdown();
    assert!(
        c.freq_seed.save_failing,
        "the failure is remembered (warned once)"
    );
    c.save_drift_file_at_shutdown();
    assert!(c.freq_seed.save_failing, "still failing (logged at debug)");

    let path = dir.path().join("drift.json");
    c.freq_seed.path = Some(path.clone());
    c.save_drift_file_at_shutdown();
    assert!(!c.freq_seed.save_failing, "saving works again");
    assert_eq!(
        drift_file::load_seed(&path, now_s(), 500.0)
            .expect("saved")
            .freq_ppm,
        20.0
    );
}
