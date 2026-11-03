//! The follower's virtual clock and the snapshot it publishes (dsync#136).
//!
//! The virtual clock `V` runs over the raw monotonic counter `m`:
//! `V(m) = v₀ + (m − m₀) · (1 + f·10⁻⁶)`. The servo's frequency word `f` (ppm) is the only input;
//! a new word re-anchors `(m₀, v₀)` at the instant it takes effect, so `V` is continuous. The
//! published PTP time is `V(m) − D_v`, `D_v` being the phase lock's anchor (`t2 − t1` held at the
//! anchor): the same quantity as the service's `wall − D`.

/// `(m − m0)` scaled by `1 + rate_ppm·10⁻⁶`, to the ns. The fractional part is computed in f64 on
/// the span only (seconds to hours: exact to well under a ns).
fn scaled_span(span_ns: i64, rate_ppm: f64) -> i64 {
    span_ns.wrapping_add((span_ns as f64 * rate_ppm * 1e-6).round() as i64)
}

/// The follower's own clock over the raw counter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VirtualClock {
    m0: i64,
    v0: i64,
    rate_ppm: f64,
}

impl VirtualClock {
    /// A clock reading the raw counter itself (`V(m) = m`) until the first word.
    pub fn new() -> Self {
        VirtualClock {
            m0: 0,
            v0: 0,
            rate_ppm: 0.0,
        }
    }

    /// `V` at the raw counter value `m`.
    pub fn at(&self, mono_ns: i64) -> i64 {
        self.v0
            .wrapping_add(scaled_span(mono_ns.wrapping_sub(self.m0), self.rate_ppm))
    }

    /// The frequency word in force (ppm).
    pub fn rate_ppm(&self) -> f64 {
        self.rate_ppm
    }

    /// The anchor `(m₀, v₀)`.
    pub fn anchor(&self) -> (i64, i64) {
        (self.m0, self.v0)
    }

    /// Apply a new word from `mono_ns` on: `V` stays continuous there.
    pub fn set_rate(&mut self, mono_ns: i64, rate_ppm: f64) {
        let v = self.at(mono_ns);
        self.m0 = mono_ns;
        self.v0 = v;
        self.rate_ppm = rate_ppm;
    }
}

impl Default for VirtualClock {
    fn default() -> Self {
        Self::new()
    }
}

/// What a reader sees: the PTP-time model of the raw counter plus the follower's state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClockModel {
    /// The model exists (the phase lock has anchored `D_v`); before that there is no PTP time.
    pub anchored: bool,
    /// DSYX [41]'s definition (#146): |e| ≤ 50 µs for 3 engaged windows, cleared at > 150 µs for
    /// 4, on disengagement, and while PTP is stale.
    pub locked: bool,
    /// No allowed PTP time message for 10 s: the model runs on the learned frequency.
    pub holdover: bool,
    /// The model: `ptp(m) = ptp0 + (m − m0)·(1 + rate·10⁻⁶)`.
    pub m0: i64,
    pub ptp0: i64,
    pub rate_ppm: f64,
    /// The grandmaster clock UUID of the last Sync (`None` before the first).
    pub gm_uuid: Option<[u8; 6]>,
    /// `wall = PTP + D` on this PC's wall clock (the service's fleet date offset where it runs).
    pub date_offset_ns: Option<i64>,
    /// The phase-lock error `e` of the last window (ns).
    pub offset_ns: Option<i64>,
    /// Bumped on every time-base change (a new grandmaster or sync source, a > 1 s jump of its
    /// time): the published time changed base.
    pub epoch: u32,
    /// Every discontinuity of the published time: a time-base change or a re-alignment (the
    /// virtual clock was > 1 ms off and re-anchored instead of slewing).
    pub reanchors: u32,
    /// Entries into holdover.
    pub holdovers: u32,
    /// Closed sample windows.
    pub windows: u64,
    /// dsync#136: locked AND settled — the recent |e| is within [`super::core::SETTLED_P90_NS`]
    /// at p90 over the last [`super::core::SETTLED_WINDOWS`] windows, and no boot pull runs.
    pub settled: bool,
    /// dsync#136: |e| p90 (ns) over the last [`super::core::SETTLED_WINDOWS`] engaged windows,
    /// late windows left out; `None` until that many.
    pub recent_abs_e_ns: Option<i64>,
}

/// The seqlock words of a [`ClockModel`].
pub const MODEL_WORDS: usize = 12;

const F_ANCHORED: u64 = 1;
const F_LOCKED: u64 = 2;
const F_HOLDOVER: u64 = 4;
const F_GM: u64 = 8;
const F_DATE: u64 = 16;
const F_OFFSET: u64 = 32;
const F_SETTLED: u64 = 64;
const F_RECENT: u64 = 128;

impl ClockModel {
    /// PTP time (ns) at the raw counter value `mono_ns`; `None` before the anchor.
    pub fn ptp_at(&self, mono_ns: i64) -> Option<i64> {
        self.anchored.then(|| {
            self.ptp0
                .wrapping_add(scaled_span(mono_ns.wrapping_sub(self.m0), self.rate_ppm))
        })
    }

    pub fn to_words(&self) -> [u64; MODEL_WORDS] {
        let mut flags = 0;
        for (on, bit) in [
            (self.anchored, F_ANCHORED),
            (self.locked, F_LOCKED),
            (self.holdover, F_HOLDOVER),
            (self.gm_uuid.is_some(), F_GM),
            (self.date_offset_ns.is_some(), F_DATE),
            (self.offset_ns.is_some(), F_OFFSET),
            (self.settled, F_SETTLED),
            (self.recent_abs_e_ns.is_some(), F_RECENT),
        ] {
            if on {
                flags |= bit;
            }
        }
        let gm = self
            .gm_uuid
            .map_or(0, |u| u.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64));
        [
            flags,
            self.m0 as u64,
            self.ptp0 as u64,
            self.rate_ppm.to_bits(),
            gm,
            self.date_offset_ns.unwrap_or(0) as u64,
            self.offset_ns.unwrap_or(0) as u64,
            self.epoch as u64,
            self.reanchors as u64,
            self.holdovers as u64,
            self.windows,
            self.recent_abs_e_ns.unwrap_or(0) as u64,
        ]
    }

    pub fn from_words(w: [u64; MODEL_WORDS]) -> Self {
        let flags = w[0];
        let on = |bit| flags & bit != 0;
        let gm = (on(F_GM)).then(|| {
            let mut u = [0u8; 6];
            for (i, b) in u.iter_mut().enumerate() {
                *b = (w[4] >> (8 * (5 - i))) as u8;
            }
            u
        });
        ClockModel {
            anchored: on(F_ANCHORED),
            locked: on(F_LOCKED),
            holdover: on(F_HOLDOVER),
            m0: w[1] as i64,
            ptp0: w[2] as i64,
            rate_ppm: f64::from_bits(w[3]),
            gm_uuid: gm,
            date_offset_ns: on(F_DATE).then_some(w[5] as i64),
            offset_ns: on(F_OFFSET).then_some(w[6] as i64),
            epoch: w[7] as u32,
            reanchors: w[8] as u32,
            holdovers: w[9] as u32,
            windows: w[10],
            settled: on(F_SETTLED),
            recent_abs_e_ns: on(F_RECENT).then_some(w[11] as i64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_virtual_clock_is_continuous_across_a_word_change() {
        let mut v = VirtualClock::new();
        assert_eq!(v.at(1_000_000_000), 1_000_000_000);
        v.set_rate(1_000_000_000, 100.0); // +100 ppm from 1 s on
        assert_eq!(v.at(1_000_000_000), 1_000_000_000);
        assert_eq!(v.at(2_000_000_000), 2_000_100_000);
        v.set_rate(2_000_000_000, -50.0);
        assert_eq!(v.at(2_000_000_000), 2_000_100_000);
        assert_eq!(v.at(12_000_000_000), 12_000_100_000 - 500_000);
        assert_eq!(v.rate_ppm(), -50.0);
        assert_eq!(v.anchor(), (2_000_000_000, 2_000_100_000));
    }

    #[test]
    fn a_model_round_trips_through_its_words() {
        let m = ClockModel {
            anchored: true,
            locked: true,
            holdover: false,
            m0: -5,
            ptp0: 1_234_567_890_123,
            rate_ppm: -21.375,
            gm_uuid: Some([0x00, 0x1d, 0xc1, 0xab, 0xcd, 0xef]),
            date_offset_ns: Some(-42),
            offset_ns: Some(-7_000),
            epoch: 3,
            reanchors: 4,
            holdovers: 2,
            windows: 99,
            settled: true,
            recent_abs_e_ns: Some(12_345),
        };
        assert_eq!(ClockModel::from_words(m.to_words()), m);
        let empty = ClockModel::default();
        assert_eq!(ClockModel::from_words(empty.to_words()), empty);
    }

    #[test]
    fn ptp_time_follows_the_model_and_is_absent_before_the_anchor() {
        let mut m = ClockModel {
            m0: 10,
            ptp0: 1_000,
            rate_ppm: 1_000.0, // 0.1 %
            ..ClockModel::default()
        };
        assert_eq!(m.ptp_at(20), None);
        m.anchored = true;
        assert_eq!(m.ptp_at(1_000_010), Some(1_000 + 1_000_000 + 1_000));
        assert_eq!(m.ptp_at(10), Some(1_000));
    }
}
