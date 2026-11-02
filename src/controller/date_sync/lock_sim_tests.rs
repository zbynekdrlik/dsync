//! dantesync#146 — DSYX [41] (`/status.is_locked`) under the phase lock, in the closed-loop
//! simulation of the whole controller (`lock_sim.rs`).
//!
//! The contract (ROZHODNUTÉ on #146, dstack confirmed): [41] = 1 means the clock is safe for a
//! 250 µs flow. It is set once the phase lock is engaged and |e| ≤ 50 µs for 3 consecutive
//! windows, and cleared when |e| > 150 µs for 4 consecutive windows, the phase lock disengages, or
//! PTP goes stale. After the first lock a boot and a date step never move it; a grandmaster
//! frequency step that takes e past 150 µs drops it ONCE and it returns once e is back.
//!
//! Measured numbers are printed (`cargo test --lib lock_sim -- --nocapture`) and quoted on #146.
//! The scenarios run one after another in ONE test (the #142 coverage-timing lesson).

use super::lock_sim::*;

/// gen3: the oscillator runs 35 ppm slow against the grandmaster.
const NATURAL_PPM: f64 = -35.0;
const LEARNED_PPM: f64 = -NATURAL_PPM;
/// The bounds of the contract (µs, windows), restated here so the test pins the numbers.
const LOCK_US: f64 = 50.0;
const LOCK_WINDOWS: usize = 3;
const UNLOCK_US: f64 = 150.0;
const UNLOCK_WINDOWS: usize = 4;
/// Per-sample jitter (µs): a Linux kernel-timestamped box, and Windows Npcap (moderate,
/// pessimistic; the #142 figures).
const JITTERS_US: [f64; 3] = [1.0, 10.0, 30.0];
const SEEDS: [u64; 3] = [
    0x5EED_0146_0001_2026,
    0x5EED_0146_0002_2026,
    0x5EED_0146_0003_2026,
];

fn e_abs(o: &Obs) -> f64 {
    o.e_us.map_or(f64::INFINITY, f64::abs)
}

/// The contract, window by window, over the whole log: a lock is entered only after
/// `LOCK_WINDOWS` engaged windows within `LOCK_US`, and a run of `UNLOCK_WINDOWS` windows beyond
/// `UNLOCK_US` never stays reported.
fn assert_contract(label: &str, s: &Sim) {
    for (i, o) in s.log.iter().enumerate() {
        let entered = o.reported && (i == 0 || !s.log[i - 1].reported);
        if entered {
            assert!(i + 1 >= LOCK_WINDOWS, "{label}: locked at window {}", o.n);
            for p in &s.log[i + 1 - LOCK_WINDOWS..=i] {
                assert!(
                    p.engaged && e_abs(p) <= LOCK_US,
                    "{label}: [41] set at window {} after window {} with e {:?} engaged {}",
                    o.n,
                    p.n,
                    p.e_us,
                    p.engaged
                );
            }
        }
        if o.reported {
            assert!(
                o.engaged,
                "{label}: [41] = 1 while disengaged (window {})",
                o.n
            );
            if i + 1 >= UNLOCK_WINDOWS {
                let run = &s.log[i + 1 - UNLOCK_WINDOWS..=i];
                assert!(
                    !run.iter().all(|p| e_abs(p) > UNLOCK_US),
                    "{label}: [41] = 1 at window {} after {} windows beyond {} us (e {:?})",
                    o.n,
                    UNLOCK_WINDOWS,
                    UNLOCK_US,
                    o.e_us
                );
            }
        }
    }
}

fn print_boot(label: &str, s: &Sim) {
    let rate_lock = s.log.iter().find(|o| o.engaged).map(|o| o.n);
    println!(
        "[#146 sim] {label}: engaged at window {:?}, [41] = 1 at window {:?} = {:.1} s after Sync \
         established, |e| at [41] {:.1} us, worst |e| while [41] = 1 {:.1} us, transitions after \
         the first lock {:?}",
        rate_lock,
        s.first_lock(),
        s.first_lock().unwrap_or(0) as f64 * WINDOW_S,
        s.e_at_first_lock_us().unwrap_or(f64::NAN),
        s.worst_locked_e_us(),
        s.transitions_after_first_lock()
    );
}

/// A boot (cold or seeded) followed for 5 minutes: [41] comes in tens of seconds and never
/// flaps once set.
fn a_boot_reports_lock_only_within_the_bound_and_never_flaps_146() {
    for jitter in JITTERS_US {
        for &noise in &SEEDS {
            for (kind, seed, fast) in [
                ("cold", None, false),
                ("seeded", Some(LEARNED_PPM), true),
                // A seed the judge keeps but a few ppm off (a warmer box): the rate servo hands
                // over up to its 5 us/s lock criterion off, and the continued fit re-seeds.
                ("seeded 4 ppm off", Some(LEARNED_PPM + 4.0), true),
                ("seeded 4 ppm off", Some(LEARNED_PPM - 4.0), true),
                // Further off, still kept by the #142 judge (up to 20 us/s): the rate servo may
                // lock with the phase lock's start 8-9 ppm off, and the phase lock's own retiring
                // of it must not read as a lock loss (review of #146).
                ("seeded 7 ppm off", Some(LEARNED_PPM + 7.0), false),
                ("seeded 7 ppm off", Some(LEARNED_PPM - 7.0), false),
                ("seeded 10 ppm off", Some(LEARNED_PPM + 10.0), false),
                ("seeded 10 ppm off", Some(LEARNED_PPM - 10.0), false),
                ("seeded 15 ppm off", Some(LEARNED_PPM + 15.0), false),
                ("seeded 15 ppm off", Some(LEARNED_PPM - 15.0), false),
            ] {
                // `fast`: a seed within a few ppm, which #142 meant to lock in ~10 s. A seed far off
                // runs on one noise seed only (CPU under coverage, the #142 lesson).
                if !fast && seed.is_some() && noise != SEEDS[0] {
                    continue;
                }
                let label = format!("{kind} boot, {jitter} us jitter, noise {noise:x}");
                let mut s = Sim::new(NATURAL_PPM, seed, noise, jitter);
                // 2 minutes past the lock: past the 20 s re-seed and the PI's ~100 s peak.
                s.run_past_lock(400, 240);
                print_boot(&label, &s);
                assert_contract(&label, &s);
                let first = s
                    .first_lock()
                    .unwrap_or_else(|| panic!("{label}: never locked"));
                assert!(
                    s.transitions_after_first_lock().is_empty(),
                    "{label}: [41] flapped: {:?}",
                    s.transitions_after_first_lock()
                );
                let engaged = s.log.iter().find(|o| o.engaged).expect("engaged").n;
                // The boot pull retires the acquisition drift in tens of seconds (tau 20 s),
                // not minutes: [41] within 60 s of the engagement.
                assert!(
                    (first - engaged) as f64 * WINDOW_S <= 60.0,
                    "{label}: [41] {:.1} s after the engagement",
                    (first - engaged) as f64 * WINDOW_S
                );
                // A seeded boot: within 15 s at Linux / moderate Windows jitter (#142's ~10 s
                // goal); at the pessimistic 30 us the rate servo's acquisition is noisy (its
                // integrator walks a few ppm), and the PI trims that before [41].
                let seeded_bound_s = if jitter <= 10.0 { 15.0 } else { 30.0 };
                if fast {
                    assert!(
                        first as f64 * WINDOW_S <= seeded_bound_s,
                        "{label}: a seeded boot sets [41] at {:.1} s",
                        first as f64 * WINDOW_S
                    );
                }
                // The phase lock owned the word from its first engagement on (no disengagement:
                // the rate criterion no longer drops the boot pull).
                assert!(
                    s.log.iter().skip(engaged).all(|o| o.engaged),
                    "{label}: the phase lock disengaged after window {engaged}"
                );
            }
        }
    }
}

/// A nightly date step (and a backward one) after the lock never moves [41].
fn a_date_step_never_moves_the_lock_146() {
    for jitter in JITTERS_US {
        for delta_ns in [800_000_000_i64, -600_000_000] {
            let label = format!("date step {delta_ns} ns, {jitter} us jitter");
            let mut s = Sim::new(NATURAL_PPM, None, SEEDS[0], jitter);
            s.run_past_lock(400, 60);
            assert!(
                s.log.last().expect("log").reported,
                "{label}: locked before"
            );
            s.date_step(delta_ns);
            s.run(120);
            assert_contract(&label, &s);
            assert!(
                s.transitions_after_first_lock().is_empty(),
                "{label}: {:?}",
                s.transitions_after_first_lock()
            );
        }
    }
}

/// A grandmaster frequency step (the v1.14 path): followed, the phase lock never disengages, and
/// [41] drops at most ONCE (only when e really passed 150 µs) and returns once.
fn a_grandmaster_frequency_step_drops_the_lock_at_most_once_146() {
    for jitter in JITTERS_US {
        for step_ppm in [-25.0, 10.0, 4.0] {
            let label = format!("GM frequency step {step_ppm:+} ppm, {jitter} us jitter");
            let mut s = Sim::new(NATURAL_PPM, Some(LEARNED_PPM), SEEDS[1], jitter);
            s.run_past_lock(400, 120);
            assert!(
                s.log.last().expect("log").reported,
                "{label}: locked before"
            );
            let at = s.n;
            s.natural_ppm += step_ppm;
            // A step under `FSTEP_MIN_PPM` is not re-seeded: the ~100 s PI takes it out, back
            // under 50 us only minutes later (camera-box issue 1372) when it dropped [41]: 10
            // minutes for that one, 150 s (past the PI's ~100 s peak) for the others.
            let followed = step_ppm.abs() >= crate::ptp_phase_lock::FSTEP_MIN_PPM;
            s.run(if followed || jitter < 30.0 { 300 } else { 1200 });
            assert_contract(&label, &s);
            let trans = s.transitions_after_first_lock();
            let peak = s.log[at..].iter().map(e_abs).fold(0.0, f64::max);
            println!(
                "[#146 sim] {label}: peak |e| {peak:.0} us, followed {}, [41] transitions {trans:?}",
                s.c.date_sync.core.freq_steps()
            );
            assert!(
                s.log[at..].iter().all(|o| o.engaged),
                "{label}: the phase lock disengaged"
            );
            assert!(
                s.log.last().expect("log").reported,
                "{label}: [41] back once e is back"
            );
            if peak <= UNLOCK_US {
                assert!(trans.is_empty(), "{label}: {trans:?}");
            } else {
                assert!(
                    trans.len() <= 2 && trans.first().map_or(true, |t| !t.1),
                    "{label}: one drop and one return at most: {trans:?}"
                );
            }
        }
    }
}

/// A phase jump while engaged (another writer set the clock, a sub-second time-base jump) is a
/// real loss: [41] drops once, the core re-anchors on the current offset (`Realigned`, as a
/// re-engagement more than 1 ms off always did) and [41] returns within seconds — never minutes
/// of slewing ~140 ppm off the tick (re-review of #146: with the rate lock held while engaged,
/// nothing re-aligned any more).
fn a_phase_jump_while_engaged_is_re_aligned_in_seconds_146() {
    for jitter in JITTERS_US {
        for jump_us in [5_000.0, -500_000.0] {
            let label = format!("phase jump {jump_us} us, {jitter} us jitter");
            let mut s = Sim::new(NATURAL_PPM, Some(LEARNED_PPM), SEEDS[2], jitter);
            s.run_past_lock(400, 120);
            assert!(
                s.log.last().expect("log").reported,
                "{label}: locked before"
            );
            let at = s.n;
            s.jump_us(jump_us);
            s.run(120);
            assert_contract(&label, &s);
            let back = s.log[at..]
                .iter()
                .skip_while(|o| o.reported)
                .find(|o| o.reported)
                .map(|o| (o.n - at) as f64 * WINDOW_S);
            let trans = s.transitions_after_first_lock();
            println!("[#146 sim] {label}: [41] back {back:?} s after the jump, {trans:?}");
            assert!(
                back.is_some_and(|b| b <= 20.0),
                "{label}: [41] back {back:?} s after the jump"
            );
            assert!(
                trans.len() <= 2,
                "{label}: one drop and one return: {trans:?}"
            );
            assert_eq!(
                s.c.date_sync.core.freq_steps(),
                0,
                "{label}: a jump is not a frequency step"
            );
        }
    }
}

#[test]
fn closed_loop_simulation_of_the_reported_lock_146() {
    let _heavy = crate::controller::heavy_test_guard();
    a_boot_reports_lock_only_within_the_bound_and_never_flaps_146();
    a_date_step_never_moves_the_lock_146();
    a_grandmaster_frequency_step_drops_the_lock_at_most_once_146();
    a_phase_jump_while_engaged_is_re_aligned_in_seconds_146();
}
