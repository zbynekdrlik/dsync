use super::*;

const NOW: i64 = 1_793_577_600; // 2026-11-02T00:00:00Z
const MAX: f64 = 500.0;
const DAY: i64 = 86_400;

fn rec(freq_ppm: f64, written_unix_s: i64) -> DriftRecord {
    DriftRecord {
        freq_ppm,
        written_unix_s,
    }
}

#[test]
fn a_record_round_trips_through_the_file_text_142() {
    let r = rec(35.712, NOW - 600);
    assert_eq!(decode(&encode(&r)), Ok(r));
    assert!(encode(&r).contains("\"version\": 1"));
}

#[test]
fn malformed_or_foreign_text_is_not_a_record_142() {
    assert!(decode("").is_err());
    assert!(decode("{\"version\":1}").is_err());
    assert!(decode("{\"version\":1,\"freq_ppm\":\"35\",\"written_unix_s\":1}").is_err());
    let other = "{\"version\":2,\"freq_ppm\":35.0,\"written_unix_s\":1}";
    assert!(decode(other).unwrap_err().contains("version 2"));
}

#[test]
fn the_age_bound_is_thirty_days_and_a_record_from_the_future_is_refused_142() {
    assert_eq!(validate(&rec(35.0, NOW - 30 * DAY), NOW, MAX), Ok(35.0));
    let old = validate(&rec(35.0, NOW - 30 * DAY - 1), NOW, MAX).unwrap_err();
    assert!(old.contains("30 days old"), "{old}");
    // An RTC that is a few hours behind the writer's wall still reads its record.
    assert_eq!(validate(&rec(35.0, NOW + 3 * 3600), NOW, MAX), Ok(35.0));
    let future = validate(&rec(35.0, NOW + DAY + 1), NOW, MAX).unwrap_err();
    assert!(future.contains("future"), "{future}");
    // A dead CMOS battery: the wall says 2000, the record is from 2026.
    assert!(validate(&rec(35.0, NOW), 946_684_800, MAX).is_err());
}

#[test]
fn the_frequency_must_be_finite_and_within_the_servo_range_142() {
    assert_eq!(validate(&rec(-500.0, NOW), NOW, MAX), Ok(-500.0));
    assert!(validate(&rec(500.001, NOW), NOW, MAX).is_err());
    assert!(validate(&rec(-500.001, NOW), NOW, MAX).is_err());
    assert!(validate(&rec(f64::NAN, NOW), NOW, MAX).is_err());
    assert!(validate(&rec(f64::INFINITY, NOW), NOW, MAX).is_err());
}

#[test]
fn load_seed_says_why_it_falls_back_to_the_cold_learn_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    assert_eq!(load_seed(&path, NOW, MAX).unwrap_err(), "no drift file yet");
    std::fs::write(&path, b"{ torn").expect("write");
    assert!(load_seed(&path, NOW, MAX)
        .unwrap_err()
        .starts_with("not a drift record"));
    std::fs::write(&path, encode(&rec(900.0, NOW))).expect("write");
    assert!(load_seed(&path, NOW, MAX).unwrap_err().contains("outside"));
    // A directory where the file should be: unreadable.
    let as_dir = dir.path().join("drift-dir.json");
    std::fs::create_dir(&as_dir).expect("mkdir");
    assert!(load_seed(&as_dir, NOW, MAX)
        .unwrap_err()
        .starts_with("unreadable"));
}

#[test]
fn save_then_load_gives_the_seed_and_leaves_no_temp_file_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    let r = rec(35.25, NOW - 60);
    assert!(save(&path, &r).expect("saved").is_none());
    assert_eq!(load_seed(&path, NOW, MAX), Ok(r));
    assert!(!dir.path().join("drift.json.tmp").exists());
    // A newer save replaces it whole.
    let newer = rec(35.5, NOW);
    save(&path, &newer).expect("saved");
    assert_eq!(load_seed(&path, NOW, MAX), Ok(newer));
}

#[test]
fn a_non_finite_frequency_is_never_written_and_the_old_record_stays_142() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("drift.json");
    save(&path, &rec(35.0, NOW)).expect("saved");
    assert!(save(&path, &rec(f64::NAN, NOW)).is_err());
    assert_eq!(load_seed(&path, NOW, MAX), Ok(rec(35.0, NOW)));
}

/// Feed `rates` (one per window, 0.5 s each) and return the verdict of the last one.
fn judge(rates: &[f64]) -> Option<SeedVerdict> {
    let mut j = SeedJudge::default();
    let mut verdict = None;
    for (n, &r) in rates.iter().enumerate() {
        verdict = j.on_measured(r, 0.5);
        if n + 1 < SEED_JUDGE_WINDOWS {
            assert_eq!(
                verdict, None,
                "no verdict before window {}",
                SEED_JUDGE_WINDOWS
            );
        }
    }
    verdict
}

#[test]
fn the_judge_waits_for_its_windows_then_accepts_a_seed_the_rates_agree_with_142() {
    assert_eq!(SEED_JUDGE_WINDOWS, 4);
    assert_eq!(
        judge(&[1.5, -0.5, 0.8, 0.2]),
        Some(SeedVerdict::Accepted { mean_rate_ppm: 0.5 })
    );
}

#[test]
fn the_judge_drops_a_seed_the_rates_disagree_with_142() {
    assert_eq!(
        judge(&[49.0, 51.0, 50.0, 50.0]),
        Some(SeedVerdict::Rejected {
            mean_rate_ppm: 50.0
        })
    );
    assert!(matches!(
        judge(&[-25.0, -26.0, -24.0, -25.0]),
        Some(SeedVerdict::Rejected { .. })
    ));
    // The bound itself is accepted, beyond it is not.
    assert!(matches!(
        judge(&[20.0; 4]),
        Some(SeedVerdict::Accepted { .. })
    ));
    assert!(matches!(
        judge(&[-20.5; 4]),
        Some(SeedVerdict::Rejected { .. })
    ));
}

#[test]
fn the_judge_is_the_slope_over_its_span_so_a_spike_inside_it_cancels_142() {
    // A median offset 60 µs off in window 2 reads +120 µs/s in window 2 and −120 in window 3.
    assert_eq!(
        judge(&[1.0, 121.0, -119.0, 1.0]),
        Some(SeedVerdict::Accepted { mean_rate_ppm: 1.0 })
    );
    // Windows of different lengths weigh by their length (the offset's slope over the span).
    let mut j = SeedJudge::default();
    j.on_measured(50.0, 1.5);
    j.on_measured(0.0, 0.5);
    j.on_measured(0.0, 0.5);
    // 50 µs/s over 1.5 s of a 3 s span: 25 µs/s (a plain mean of the four would read 12.5).
    assert_eq!(
        j.on_measured(0.0, 0.5),
        Some(SeedVerdict::Rejected {
            mean_rate_ppm: 25.0
        })
    );
}
