//! dsync#218 (dasio#234): an arrival-latency step, detected and REPORTED only.
//!
//! A software receive stamp carries the NIC's delivery delay. A USB NIC (RTL8153) delivers
//! sparse inbound frames ~1 ms late and promptly once an RX flow runs, so the window error `e`
//! shifts by that delay when the inbound load changes. A real oscillator cannot move `e` by
//! [`STEP_NS`] within [`HELD_WINDOWS`] windows (> 100 ppm), so such a shift, held, is the receive
//! path. The detector never touches the servo; the caller logs what it returns.
//!
//! The baseline is the median of the last [`BASELINE_WINDOWS`] windows. A window beyond
//! [`STEP_NS`] from it starts a run, and [`HELD_WINDOWS`] in a row on the same side fire a step.
//! The run then becomes the baseline. A shorter run is a spike and leaves the baseline alone.

use std::collections::VecDeque;

/// The shift that is a step, not a clock (ns).
pub const STEP_NS: i64 = 300_000;
/// The windows a shift must hold.
pub const HELD_WINDOWS: usize = 3;
/// The windows of the baseline.
pub const BASELINE_WINDOWS: usize = 8;

/// One closed window as the detector sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowReading {
    /// The window's phase error `e` (ns).
    pub e_ns: i64,
    /// The median Sync→Follow_Up arrival gap of the window's samples (ns), context only.
    pub gap_ns: i64,
    /// The window's `t2 − t1` spread, max − min (ns), context only.
    pub spread_ns: i64,
}

/// A detected step: the levels and their context, before and after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyStep {
    pub before: WindowReading,
    pub after: WindowReading,
    /// The steps since start, this one included.
    pub count: u32,
    /// The first step in this direction (the caller warns once per direction).
    pub first_in_direction: bool,
}

impl LatencyStep {
    /// `e` moved by this much (ns).
    pub fn delta_ns(&self) -> i64 {
        self.after.e_ns - self.before.e_ns
    }
}

/// The report line of a step, the same text in the follower and the service (dsync#220): the
/// NIC, the levels before and after, the held windows, the context and the count. `nic` empty =
/// "the PTP interface". The caller adds its prefix and picks the level (warn on
/// `first_in_direction`).
pub fn describe(nic: &str, s: &LatencyStep) -> String {
    let us = |ns: i64| ns as f64 / 1_000.0;
    let nic = if nic.is_empty() {
        "the PTP interface"
    } else {
        nic
    };
    format!(
        "arrival latency stepped on {}: e {:+.0}us -> {:+.0}us (delta {:+.0}us held {} windows); sync->follow_up gap median {:.0}us -> {:.0}us; window spread {:.0}us -> {:.0}us; {} step(s) since start",
        nic,
        us(s.before.e_ns),
        us(s.after.e_ns),
        us(s.delta_ns()),
        HELD_WINDOWS,
        us(s.before.gap_ns),
        us(s.after.gap_ns),
        us(s.before.spread_ns),
        us(s.after.spread_ns),
        s.count
    )
}

/// The sentence a first step in a direction adds to its warning.
pub const WARN_TAIL: &str =
    "A software stamp carries the NIC's delivery delay into the offset; the servo is unchanged.";

#[derive(Debug, Default)]
pub struct LatencyStepDetector {
    history: VecDeque<WindowReading>,
    run: Vec<WindowReading>,
    /// The baseline filled once since the last reset. After a step the run (its new level) is
    /// the baseline at once, so a return right after is judged too.
    primed: bool,
    count: u32,
    fired_up: bool,
    fired_down: bool,
}

fn median(mut v: Vec<i64>) -> i64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// The component-wise median of `readings` (not empty).
fn level<'a>(readings: impl Iterator<Item = &'a WindowReading> + Clone) -> WindowReading {
    WindowReading {
        e_ns: median(readings.clone().map(|r| r.e_ns).collect()),
        gap_ns: median(readings.clone().map(|r| r.gap_ns).collect()),
        spread_ns: median(readings.map(|r| r.spread_ns).collect()),
    }
}

impl LatencyStepDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The steps since start.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// A new time base, a holdover, a port move or an anchor event: the levels before it say
    /// nothing about the ones after. The count and the warned directions stay.
    pub fn reset(&mut self) {
        self.history.clear();
        self.run.clear();
        self.primed = false;
    }

    /// One engaged window. `Some` when it completes a held step.
    pub fn on_window(&mut self, r: WindowReading) -> Option<LatencyStep> {
        if !self.primed {
            self.push_history(r);
            self.primed = self.history.len() == BASELINE_WINDOWS;
            return None;
        }
        let before = level(self.history.iter());
        let d = r.e_ns - before.e_ns;
        if d.abs() <= STEP_NS {
            self.run.clear();
            self.push_history(r);
            return None;
        }
        let same_side = self
            .run
            .first()
            .map_or(true, |f| (f.e_ns - before.e_ns).signum() == d.signum());
        if !same_side {
            self.run.clear();
        }
        self.run.push(r);
        if self.run.len() < HELD_WINDOWS {
            return None;
        }
        let after = level(self.run.iter());
        let up = after.e_ns > before.e_ns;
        let fired = if up {
            &mut self.fired_up
        } else {
            &mut self.fired_down
        };
        let first_in_direction = !*fired;
        *fired = true;
        self.count = self.count.saturating_add(1);
        self.history = self.run.drain(..).collect();
        Some(LatencyStep {
            before,
            after,
            count: self.count,
            first_in_direction,
        })
    }

    fn push_history(&mut self, r: WindowReading) {
        if self.history.len() == BASELINE_WINDOWS {
            self.history.pop_front();
        }
        self.history.push_back(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(e_ns: i64) -> WindowReading {
        WindowReading {
            e_ns,
            gap_ns: 10_000,
            spread_ns: 40_000,
        }
    }

    fn feed(d: &mut LatencyStepDetector, es: &[i64]) -> Vec<LatencyStep> {
        es.iter().filter_map(|&e| d.on_window(w(e))).collect()
    }

    fn quiet() -> Vec<i64> {
        (0..BASELINE_WINDOWS as i64)
            .map(|i| (i % 3 - 1) * 5_000)
            .collect()
    }

    #[test]
    fn a_held_shift_beyond_the_bound_is_one_step_with_its_levels() {
        let mut d = LatencyStepDetector::new();
        assert!(feed(&mut d, &quiet()).is_empty());
        let steps = feed(&mut d, &[-800_000, -790_000, -810_000, -800_000, -805_000]);
        assert_eq!(steps.len(), 1);
        let s = steps[0];
        assert_eq!(s.before.e_ns, 0);
        assert_eq!(s.after.e_ns, -800_000);
        assert_eq!(s.delta_ns(), -800_000);
        assert_eq!((s.count, s.first_in_direction), (1, true));
        assert_eq!(d.count(), 1);
    }

    #[test]
    fn a_spike_shorter_than_the_hold_is_no_step() {
        let mut d = LatencyStepDetector::new();
        feed(&mut d, &quiet());
        assert!(feed(&mut d, &[900_000, 900_000, 0, 0, 900_000, 0]).is_empty());
        assert_eq!(d.count(), 0);
    }

    #[test]
    fn a_shift_within_the_bound_is_no_step() {
        let mut d = LatencyStepDetector::new();
        feed(&mut d, &quiet());
        assert!(feed(&mut d, &[STEP_NS, STEP_NS, STEP_NS, STEP_NS]).is_empty());
    }

    #[test]
    fn a_run_that_changes_side_starts_again() {
        let mut d = LatencyStepDetector::new();
        feed(&mut d, &quiet());
        assert!(feed(&mut d, &[500_000, 500_000, -500_000, -500_000]).is_empty());
        assert_eq!(feed(&mut d, &[-500_000]).len(), 1);
    }

    #[test]
    fn the_new_level_is_the_baseline_and_each_direction_warns_once() {
        let mut d = LatencyStepDetector::new();
        feed(&mut d, &quiet());
        let down = feed(&mut d, &[-800_000; 6]);
        let up = feed(&mut d, &[0; 6]);
        let down2 = feed(&mut d, &[-800_000; 6]);
        assert_eq!((down.len(), up.len(), down2.len()), (1, 1, 1));
        assert!(down[0].first_in_direction && up[0].first_in_direction);
        assert!(!down2[0].first_in_direction);
        assert_eq!(down2[0].count, 3);
    }

    #[test]
    fn no_step_before_the_baseline_is_full_and_a_reset_refills_it() {
        let mut d = LatencyStepDetector::new();
        assert!(feed(&mut d, &[0, 0, 900_000, 900_000, 900_000]).is_empty());
        let mut d = LatencyStepDetector::new();
        feed(&mut d, &quiet());
        feed(&mut d, &[900_000, 900_000]);
        d.reset();
        assert!(feed(&mut d, &[900_000; BASELINE_WINDOWS]).is_empty());
        assert_eq!(d.count(), 0);
    }

    #[test]
    fn the_report_line_names_the_nic_the_levels_and_the_count() {
        let s = LatencyStep {
            before: WindowReading {
                e_ns: 0,
                gap_ns: 12_000,
                spread_ns: 40_000,
            },
            after: WindowReading {
                e_ns: -800_000,
                gap_ns: 9_000,
                spread_ns: 30_000,
            },
            count: 2,
            first_in_direction: true,
        };
        assert_eq!(
            describe("Ethernet 2", &s),
            "arrival latency stepped on Ethernet 2: e +0us -> -800us (delta -800us held 3 windows); sync->follow_up gap median 12us -> 9us; window spread 40us -> 30us; 2 step(s) since start"
        );
        assert!(describe("", &s).starts_with("arrival latency stepped on the PTP interface: "));
    }

    #[test]
    fn the_context_levels_are_the_medians_before_and_after() {
        let mut d = LatencyStepDetector::new();
        for e in quiet() {
            d.on_window(WindowReading {
                e_ns: e,
                gap_ns: 12_000,
                spread_ns: 40_000,
            });
        }
        let mut step = None;
        for _ in 0..HELD_WINDOWS {
            step = d.on_window(WindowReading {
                e_ns: 600_000,
                gap_ns: 1_030_000,
                spread_ns: 900_000,
            });
        }
        let s = step.expect("a step");
        assert_eq!((s.before.gap_ns, s.before.spread_ns), (12_000, 40_000));
        assert_eq!((s.after.gap_ns, s.after.spread_ns), (1_030_000, 900_000));
    }
}
