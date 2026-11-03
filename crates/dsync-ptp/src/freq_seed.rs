//! dsync#142 / dsync#136 — judging a frequency seed on the first measured windows. The service
//! seeds from its `drift.json`; the in-process follower from the value its app hands in
//! (`FollowerConfig::with_frequency_seed`). Both judge it the same way and drop a seed that
//! belongs to another oscillator ([`crate::rate_servo::RateServo::drop_seed`]).

/// How many MEASURED windows judge a seed. Fewer than `LOCK_STABLE_COUNT` (5), so a seed is
/// always accepted or dropped before the servo can lock on it.
pub const SEED_JUDGE_WINDOWS: usize = 4;

/// A seed is dropped when the oscillator's mean rate against it over the first
/// [`SEED_JUDGE_WINDOWS`] measured windows is beyond this (µs/s = ppm off the seed): the rate at
/// which the servo leaves PROD for ACQ. A disagreement this large means the seed belongs to
/// another oscillator (a moved disk, new hardware), not to a warmer or colder one; the servo then
/// learns from 0 as a cold start does. A smaller error is kept and corrected by the servo from the
/// seed.
pub const SEED_REJECT_RATE_PPM: f64 = 20.0;

/// The verdict on a seed after [`SEED_JUDGE_WINDOWS`] measured windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeedVerdict {
    /// The windows agree with the seed: their mean rate against it (ppm).
    Accepted { mean_rate_ppm: f64 },
    /// They do not: their mean rate against it, beyond [`SEED_REJECT_RATE_PPM`].
    Rejected { mean_rate_ppm: f64 },
}

/// Judges a seed on the first measured windows' rates against the seed (µs/s = ppm: what the
/// oscillator shows with the seed alone as its word), weighted by each window's length. Each
/// window's rate is (its offset − the previous window's) / dt, so the weighted mean telescopes to
/// the offset's slope over the whole span (the controller judges only windows with a measured
/// rate, never one shorter than 0.1 s, which carries the smoothed rate instead): only the first and the last window's jitter remain,
/// divided by the span (2 s), and a spike inside the span cancels out. A per-window median of
/// three would keep each window's own jitter (on a Windows box tens of µs/s per window).
#[derive(Clone, Debug, Default)]
pub struct SeedJudge {
    windows: usize,
    weighted_sum: f64,
    span_s: f64,
}

impl SeedJudge {
    /// A measured window's rate against the seed and its length; `Some` once the judge has its
    /// windows (then it is done).
    pub fn on_measured(&mut self, rate_off_seed_ppm: f64, dt_s: f64) -> Option<SeedVerdict> {
        self.windows += 1;
        self.weighted_sum += rate_off_seed_ppm * dt_s;
        self.span_s += dt_s;
        if self.windows < SEED_JUDGE_WINDOWS {
            return None;
        }
        let mean_rate_ppm = if self.span_s > 0.0 {
            self.weighted_sum / self.span_s
        } else {
            0.0
        };
        Some(if mean_rate_ppm.abs() > SEED_REJECT_RATE_PPM {
            SeedVerdict::Rejected { mean_rate_ppm }
        } else {
            SeedVerdict::Accepted { mean_rate_ppm }
        })
    }
}
