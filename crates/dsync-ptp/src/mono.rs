//! The in-process follower's time base: the PC's RAW monotonic counter, and the conversion of a
//! software receive stamp (wall clock) onto it (dsync#136).
//!
//! # Why the raw counter
//!
//! The follower runs a VIRTUAL clock and never touches the system clock. Its counter must not be
//! moved by the clock owner of the PC either — usually the dsync service on the same box:
//!
//! - Linux: `CLOCK_MONOTONIC_RAW`. `CLOCK_MONOTONIC` is slewed by `adjtimex` frequency changes
//!   (the service writes one every window); the raw clock is the oscillator itself, so the
//!   follower's loop never sees the service's.
//! - Windows: QueryPerformanceCounter, which `SetSystemTimeAdjustmentPrecise` does not steer.
//!
//! # Converting a receive stamp
//!
//! A software RX stamp arrives in the WALL clock (`SO_TIMESTAMPNS` = `CLOCK_REALTIME`; Npcap
//! HostHighPrec = `KeQuerySystemTimePrecise`), which the clock owner steers AND steps. After every
//! receive the follower reads both clocks back to back ([`read_time_base`]: raw, wall, raw — re-read
//! when preempted) and converts with that reading: `mono_rx = wall_rx − (wall − raw)`.
//!
//! `wall − raw` moves only at the clock owner's frequency word (≤ 500 ppm, µs per second) — or by
//! a STEP. [`StampConverter`] compares each reading with the previous one: beyond
//! [`STEP_TOLERANCE_NS`] + 500 ppm of the elapsed raw time, a step landed between the two
//! readings, and the packet received between them may carry either side's stamp. That sample is
//! dropped (it straddles the step). A step smaller than the tolerance can move one sample by less
//! than the tolerance, which the window median absorbs.

/// A step of the wall clock shorter than this (plus the slew allowance) is not detected; the
/// window median absorbs a single sample moved by that much.
pub const STEP_TOLERANCE_NS: i64 = 20_000;

/// The largest frequency word a clock owner applies (the service's `DRIFT_MAX_PPM`): `wall − raw`
/// may move this fast without a step.
pub const MAX_SLEW_PPM: i64 = 500;

/// A sandwiched read whose two raw reads are further apart than this was preempted: read again.
pub const MAX_READ_SPREAD_NS: i64 = 10_000;

/// The raw monotonic counter now (ns, arbitrary origin, never moved by the clock owner).
pub fn mono_now_ns() -> i64 {
    imp::mono_now_ns()
}

/// The wall clock now (ns since the Unix epoch, as the receive stamps carry it).
pub fn wall_now_ns() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

/// One reading of both clocks, taken as close together as the OS allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeBaseReading {
    /// The raw counter at the wall read (the midpoint of the two raw reads around it).
    pub mono_ns: i64,
    /// `wall − raw` at that instant (ns).
    pub wall_minus_mono_ns: i64,
}

impl TimeBaseReading {
    /// A wall stamp converted with this reading.
    pub fn to_mono(&self, wall_ns: i64) -> i64 {
        wall_ns.wrapping_sub(self.wall_minus_mono_ns)
    }
}

/// Read both clocks: raw, wall, raw; re-read (up to 3 times) when the two raw reads are more than
/// [`MAX_READ_SPREAD_NS`] apart (the thread was preempted between them). The tightest attempt wins.
pub fn read_time_base() -> TimeBaseReading {
    let mut best: Option<(i64, TimeBaseReading)> = None;
    for _ in 0..4 {
        let a = mono_now_ns();
        let w = wall_now_ns();
        let b = mono_now_ns();
        let spread = b - a;
        let mid = a + spread / 2;
        let reading = TimeBaseReading {
            mono_ns: mid,
            wall_minus_mono_ns: w.wrapping_sub(mid),
        };
        if best.map_or(true, |(s, _)| spread < s) {
            best = Some((spread, reading));
        }
        if spread <= MAX_READ_SPREAD_NS {
            break;
        }
    }
    best.map(|(_, r)| r).expect("at least one reading")
}

/// Whether the wall clock moved continuously between two readings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Continuity {
    /// The first reading, or `wall − raw` moved no more than a frequency word can.
    Continuous,
    /// The wall clock was stepped by about this much (ns) since the previous reading.
    Stepped { by_ns: i64 },
}

/// Tracks `wall − raw` from reading to reading and tells a step (pure: readings in, verdicts out).
#[derive(Clone, Debug, Default)]
pub struct StampConverter {
    last: Option<TimeBaseReading>,
    steps: u64,
}

impl StampConverter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Steps seen so far.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Take a new reading: [`Continuity::Stepped`] when `wall − raw` moved by more than
    /// [`STEP_TOLERANCE_NS`] + [`MAX_SLEW_PPM`] of the raw time since the previous one.
    pub fn observe(&mut self, reading: TimeBaseReading) -> Continuity {
        let verdict = match self.last {
            None => Continuity::Continuous,
            Some(prev) => {
                let moved = reading
                    .wall_minus_mono_ns
                    .wrapping_sub(prev.wall_minus_mono_ns);
                let elapsed = reading.mono_ns.saturating_sub(prev.mono_ns).max(0);
                let allowance = STEP_TOLERANCE_NS + elapsed / (1_000_000 / MAX_SLEW_PPM);
                if moved.abs() > allowance {
                    self.steps += 1;
                    Continuity::Stepped { by_ns: moved }
                } else {
                    Continuity::Continuous
                }
            }
        };
        self.last = Some(reading);
        verdict
    }
}

#[cfg(unix)]
mod imp {
    pub fn mono_now_ns() -> i64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec; CLOCK_MONOTONIC_RAW is a valid clock id on
        // every supported unix (Linux since 2.6.28). The call cannot fail with these arguments.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let id = libc::CLOCK_MONOTONIC_RAW;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let id = libc::CLOCK_MONOTONIC;
        let rc = unsafe { libc::clock_gettime(id, &mut ts) };
        debug_assert_eq!(rc, 0);
        // `time_t` / `c_long` are i64 here but narrower on some unix targets.
        #[allow(clippy::unnecessary_cast)]
        let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
        ns
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    fn frequency() -> i64 {
        static FREQ: OnceLock<i64> = OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: a valid out pointer; the call cannot fail on Windows XP and later.
            let _ = unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        })
    }

    pub fn mono_now_ns() -> i64 {
        let mut c = 0i64;
        // SAFETY: a valid out pointer; the call cannot fail on Windows XP and later.
        let _ = unsafe { QueryPerformanceCounter(&mut c) };
        (c as i128 * 1_000_000_000 / frequency() as i128) as i64
    }
}

#[cfg(test)]
mod tests;
