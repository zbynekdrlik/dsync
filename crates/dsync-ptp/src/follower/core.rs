//! The follower's PTP pipeline, pure (dsync#136): packets with explicit stamps in, the clock
//! model out. No thread, no socket, no clock read — the thread ([`super::Follower`]) is glue, and
//! a simulation drives this deterministically.
//!
//! It is the service's pipeline (`PtpController` in the service crate) on a virtual clock:
//!
//! 1. A Sync is kept with its receive stamp; the Follow_Up with the same sequence id and source
//!    gives `t1` (the grandmaster's precise send time). `t2` = the virtual clock at the Sync's
//!    raw-counter receive stamp.
//! 2. Samples at least [`MIN_SAMPLE_SPACING_NS`] of grandmaster time apart fill a window of
//!    [`WINDOW_SAMPLES`]; its median of `t2 − t1` feeds, as in the service:
//!    - the shared rate servo ([`crate::rate_servo`]), which acquires: its window offset is the
//!      median in µs (only its rate of change matters), its time the window's raw stamp; once the
//!      phase lock owns the word its measurement is decoupled from the phase lock's deviation
//!      ([`WordDeviation`]) and an unstable window does not unlock it (#146);
//!    - the shared phase lock ([`crate::ptp_phase_lock`]), `e = (t2 − t1) − D_v`, its `dt` in
//!      grandmaster time, engaged on the rate servo's lock;
//!    - the shared lock gate ([`crate::lock_gate`]) on `e`: DSYX [41]'s definition.
//! 3. The word (the phase lock's once engaged, else the rate servo's) re-anchors the virtual clock.
//!
//! Differences from the service, each because the virtual clock makes it unnecessary: no warmup
//! and no calibration (they condition the WALL clock's first samples; `t2` here is the raw
//! counter), no date layer (NTP, steps, slews: the follower publishes PTP time and only reads `D`
//! from the wall), and a time-base change also drops the rate servo's previous offset (the
//! service's offset is the mod-1 s phase, which stays bounded; the raw `t2 − t1` here jumps by the
//! base shift and would read as a ±500 ppm rate).

use super::model::{ClockModel, VirtualClock};
use crate::freq_seed::{SeedJudge, SeedVerdict, SEED_JUDGE_WINDOWS};
use crate::gm_filter::GmAllowlist;
use crate::lock_gate::PhaseLockGate;
use crate::phase_window;
use crate::ptp::{PtpV1Control, PtpV1FollowUpBody, PtpV1Header, PtpV1SyncMessageBody};
use crate::ptp_phase_lock::{AnchorEvent, PhaseLockCore};
use crate::ptp_rejoin::REJOIN_AFTER;
use crate::rate_servo::{MonoNs, OffsetMeasurement, RateServo, ServoWord, WordDeviation};
use log::{debug, info, warn};
use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;

/// Samples per window (the service's `filters.sample_window_size` default).
pub const WINDOW_SAMPLES: usize = 4;

/// The least grandmaster time between two samples of a window (the service's effective default:
/// `filters.min_delta_ns` = 1 ms on Linux; 0 on Windows, where the controller falls back to
/// 100 ms). The grandmaster sends a Sync every 250 ms (syncInterval −2), so both take every
/// Sync; the simulations send 8 per second.
#[cfg(not(windows))]
pub const MIN_SAMPLE_SPACING_NS: i64 = 1_000_000;
#[cfg(windows)]
pub const MIN_SAMPLE_SPACING_NS: i64 = 100_000_000;

/// No allowed time message for this long = stale = holdover (the service's `PTP_TIMEOUT_SECS`).
pub const STALE_AFTER_NS: i64 = REJOIN_AFTER.as_nanos() as i64;

/// dsync#136: the follower is SETTLED when, locked, its |e| p90 over the last this many engaged
/// windows (late windows left out) is within [`SETTLED_P90_NS`] and the phase a boot pull still
/// retires is within it too.
pub const SETTLED_WINDOWS: usize = 30;

/// 30 µs. On mbc-snv's Windows Npcap the follower's steady |e| p90 read 20-27 µs (late windows
/// out), and the boot transient dstack must not take over on sat at +20…40 µs.
pub const SETTLED_P90_NS: i64 = 30_000;

/// Pending Syncs kept at most (the service's bound against a flood of unmatched Syncs).
const MAX_PENDING_SYNCS: usize = 200;

/// A received datagram with its stamps.
#[derive(Clone, Copy, Debug)]
pub struct RxPacket<'a> {
    pub data: &'a [u8],
    /// The receive stamp on the raw monotonic counter ([`crate::mono`]).
    pub mono_rx_ns: i64,
    /// The same instant on the wall clock (the original software stamp).
    pub wall_rx_ns: i64,
    pub source_ip: Option<Ipv4Addr>,
}

/// What a packet was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketVerdict {
    /// From a source the allowlist refuses: as if it never arrived.
    NotAllowed,
    /// Not the grandmaster's time (a runt, a Delay_Req of another follower, …).
    Other,
    /// A Sync or a Follow_Up from an allowed source: PTP liveness.
    TimeMessage,
}

#[derive(Clone, Copy, Debug)]
struct PendingSync {
    mono_rx_ns: i64,
    wall_rx_ns: i64,
    source_uuid: [u8; 6],
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    /// `t2 − t1` on the virtual clock.
    raw_ns: i64,
    /// `wall_rx − t2`: with `D_v` added, the wall's date offset.
    wall_minus_v_ns: i64,
}

/// The follower's whole PTP state.
pub struct FollowerCore {
    allowlist: GmAllowlist,
    pending: HashMap<u16, PendingSync>,
    sync_source: Option<[u8; 6]>,
    gm_uuid: Option<[u8; 6]>,
    last_time_message_ns: i64,
    stale: bool,
    prev_t1_ns: i64,
    window: Vec<Sample>,
    last_window_t1_ns: Option<i64>,
    rate: RateServo<MonoNs>,
    core: PhaseLockCore,
    gate: PhaseLockGate,
    deviation: WordDeviation,
    clock: VirtualClock,
    date_offset_ns: Option<i64>,
    epoch: u32,
    reanchors: u32,
    holdovers: u32,
    windows: u64,
    straddled: u64,
    /// dsync#136: the frequency seed the app handed in, its judge (until the verdict), and the
    /// words in force over the last two windows (a window's median sits mid-window).
    seed_ppm: Option<f64>,
    seed_judge: Option<SeedJudge>,
    word_last: f64,
    word_before: f64,
    /// dsync#136: |e| (ns) of the last engaged windows the PI took, for `settled`.
    recent_e: VecDeque<i64>,
}

impl FollowerCore {
    /// A follower started at raw counter `started_ns`, accepting PTP from `allowlist`'s sources
    /// (empty = every source).
    pub fn new(allowlist: GmAllowlist, started_ns: i64) -> Self {
        let mut clock = VirtualClock::new();
        // V(m) = m: an arbitrary origin; the phase lock's anchor absorbs it.
        clock.set_rate(started_ns, 0.0);
        FollowerCore {
            allowlist,
            pending: HashMap::new(),
            sync_source: None,
            gm_uuid: None,
            last_time_message_ns: started_ns,
            stale: false,
            prev_t1_ns: 0,
            window: Vec::with_capacity(WINDOW_SAMPLES),
            last_window_t1_ns: None,
            rate: RateServo::new(),
            core: PhaseLockCore::new(),
            gate: PhaseLockGate::new(),
            deviation: WordDeviation::default(),
            clock,
            date_offset_ns: None,
            epoch: 0,
            reanchors: 0,
            holdovers: 0,
            windows: 0,
            straddled: 0,
            seed_ppm: None,
            seed_judge: None,
            word_last: 0.0,
            word_before: 0.0,
            recent_e: VecDeque::with_capacity(SETTLED_WINDOWS),
        }
    }

    /// dsync#136: start from `ppm` (the app's seed) instead of 0: the virtual clock runs it
    /// from now and the rate servo's integrator starts on it. Judged on the first
    /// [`SEED_JUDGE_WINDOWS`] measured windows like the service's `drift.json`; a seed that
    /// belongs to another oscillator is dropped and the follower learns as a cold start does.
    /// A non-finite seed or one beyond ±500 ppm is ignored.
    pub fn with_seed(mut self, ppm: f64) -> Self {
        if !ppm.is_finite() || ppm.abs() > crate::rate_servo::DRIFT_MAX_PPM {
            warn!(
                "[FOLLOWER] frequency seed {ppm} ppm ignored: not a finite value within ±500 ppm"
            );
            return self;
        }
        let (m0, _) = self.clock.anchor();
        self.clock.set_rate(m0, ppm);
        self.rate.drift_baseline_ppm = ppm;
        self.seed_ppm = Some(ppm);
        self.seed_judge = Some(SeedJudge::default());
        self.word_last = ppm;
        self.word_before = ppm;
        info!(
            "[FOLLOWER] seeded {:+.3}ppm; judged on the first {} measured windows",
            ppm, SEED_JUDGE_WINDOWS
        );
        self
    }

    /// dsync#136: the seed's judge. Returns what to take off this window's raw rate: 0, or at a
    /// rejection the word in force (the servo is put where a cold start would be).
    fn judge_seed(&mut self, raw_rate_ppm: f64, dt_s: f64) -> f64 {
        let seed = self.seed_ppm.unwrap_or(0.0);
        let word_ppm = (self.word_before + self.word_last) / 2.0;
        let Some(judge) = self.seed_judge.as_mut() else {
            return 0.0;
        };
        let Some(verdict) = judge.on_measured(raw_rate_ppm - (word_ppm - seed), dt_s) else {
            return 0.0;
        };
        self.seed_judge = None;
        match verdict {
            SeedVerdict::Accepted { mean_rate_ppm } => {
                info!(
                    "[FOLLOWER] seed {:+.3}ppm accepted: the oscillator runs {:+.1}us/s off it",
                    seed, mean_rate_ppm
                );
                0.0
            }
            SeedVerdict::Rejected { mean_rate_ppm } => {
                warn!(
                    "[FOLLOWER] seed {:+.3}ppm REJECTED: the oscillator runs {:+.1}us/s off it \
                     (another oscillator?) — learning the frequency from 0",
                    seed, mean_rate_ppm
                );
                self.rate.drop_seed(word_ppm);
                word_ppm
            }
        }
    }

    /// dsync#136: settled = locked, not stale, a pull left within [`SETTLED_P90_NS`], and the
    /// recent |e| p90 within
    /// [`SETTLED_P90_NS`] over [`SETTLED_WINDOWS`] windows.
    fn recent_abs_e_ns(&self) -> Option<i64> {
        if self.recent_e.len() < SETTLED_WINDOWS {
            return None;
        }
        let mut v: Vec<i64> = self.recent_e.iter().copied().collect();
        v.sort_unstable();
        let idx = ((v.len() as f64 * 0.9).ceil() as usize).clamp(1, v.len()) - 1;
        Some(v[idx])
    }

    /// The model a reader sees now.
    pub fn model(&self) -> ClockModel {
        let (m0, v0) = self.clock.anchor();
        let anchor = self.core.anchor_ns();
        ClockModel {
            anchored: anchor.is_some(),
            locked: self.gate.locked() && !self.stale,
            holdover: self.stale && anchor.is_some(),
            m0,
            ptp0: v0.wrapping_sub(anchor.unwrap_or(0)),
            rate_ppm: self.clock.rate_ppm(),
            gm_uuid: self.gm_uuid,
            date_offset_ns: self.date_offset_ns,
            offset_ns: self.core.last_error_ns(),
            epoch: self.epoch,
            reanchors: self.reanchors,
            holdovers: self.holdovers,
            windows: self.windows,
            settled: self.gate.locked()
                && !self.stale
                && self.core.pull_us().abs() * 1_000.0 <= SETTLED_P90_NS as f64
                && self
                    .recent_abs_e_ns()
                    .is_some_and(|p90| p90 <= SETTLED_P90_NS),
            recent_abs_e_ns: self.recent_abs_e_ns(),
        }
    }

    /// Samples dropped because the wall clock stepped while they were received.
    pub fn straddled(&self) -> u64 {
        self.straddled
    }

    /// The rate servo's lock (acquisition), for diagnostics and tests.
    pub fn rate_locked(&self) -> bool {
        self.rate.is_locked
    }

    /// The phase lock owns the frequency word.
    pub fn engaged(&self) -> bool {
        self.core.engaged()
    }

    /// One received datagram.
    pub fn on_packet(&mut self, p: RxPacket<'_>) -> PacketVerdict {
        if let Some(ip) = p.source_ip {
            if !self.allowlist.allows(ip) {
                return PacketVerdict::NotAllowed;
            }
        }
        if !crate::ptp::is_time_message(p.data) {
            return PacketVerdict::Other;
        }
        self.note_time_message(p.mono_rx_ns);
        let Ok(header) = PtpV1Header::parse(p.data) else {
            return PacketVerdict::Other;
        };
        match header.message_type {
            PtpV1Control::Sync => self.on_sync(&header, p),
            PtpV1Control::FollowUp => self.on_follow_up(&header, p.data),
            _ => {}
        }
        PacketVerdict::TimeMessage
    }

    /// The wall clock was stepped since the last receive: the Syncs waiting for their Follow_Up
    /// are dropped (their pairing would straddle the step). The packets of the receive pass that
    /// saw it are dropped by the caller ([`FollowerCore::note_straddling_packet`]).
    pub fn on_wall_step(&mut self) {
        self.straddled += self.pending.len() as u64;
        self.pending.clear();
    }

    /// A packet whose stamp may be on either side of a wall step was dropped.
    pub fn note_straddling_packet(&mut self) {
        self.straddled += 1;
    }

    /// The clock tick: the staleness edge (holdover).
    pub fn on_tick(&mut self, mono_now_ns: i64) {
        let silent = mono_now_ns.saturating_sub(self.last_time_message_ns) > STALE_AFTER_NS;
        if silent && !self.stale {
            self.enter_holdover(mono_now_ns);
        }
    }

    fn note_time_message(&mut self, mono_rx_ns: i64) {
        self.last_time_message_ns = self.last_time_message_ns.max(mono_rx_ns);
        if self.stale {
            self.stale = false;
            info!("[FOLLOWER] PTP is back: leaving holdover");
        }
    }

    /// No PTP for [`STALE_AFTER_NS`]: drop every pre-outage measurement, hold the learned
    /// frequency (not the last word with its P term), clear the lock (the service's offline edge).
    fn enter_holdover(&mut self, mono_now_ns: i64) {
        self.stale = true;
        if self.core.anchor_ns().is_some() {
            self.holdovers = self.holdovers.saturating_add(1);
        }
        self.drop_measurement();
        let outage = phase_window::on_outage(&mut self.core, &mut self.gate, &mut self.deviation);
        if let Some(word) = outage.held_ppm {
            self.rate.drift_baseline_ppm = word;
            self.clock.set_rate(mono_now_ns, word);
        }
        warn!(
            "[FOLLOWER] no PTP for {}s: holdover on the learned {:+.3}ppm",
            STALE_AFTER_NS / 1_000_000_000,
            self.clock.rate_ppm()
        );
    }

    fn drop_measurement(&mut self) {
        self.recent_e.clear();
        self.pending.clear();
        self.window.clear();
        self.prev_t1_ns = 0;
        self.last_window_t1_ns = None;
        self.rate.reset_measurement();
    }

    /// The PTP sender may carry another time base: re-anchor `D_v` on the next window, never
    /// mixing samples of two bases.
    fn on_time_base_change(&mut self) {
        self.drop_measurement();
        self.core.request_rebase();
    }

    fn on_sync(&mut self, header: &PtpV1Header, p: RxPacket<'_>) {
        let source = header.source_uuid;
        match self.sync_source {
            Some(current) if current != source => {
                warn!(
                    "[FOLLOWER] sync source changed: {:02x?} -> {:02x?}",
                    current, source
                );
                self.sync_source = Some(source);
                self.on_time_base_change();
            }
            None => self.sync_source = Some(source),
            _ => {}
        }
        // A new grandmaster is a new time base: judged BEFORE this Sync is kept, so the change
        // never drops the new grandmaster's first Sync.
        if let Ok(body) = PtpV1SyncMessageBody::parse(&p.data[PtpV1Header::SIZE..]) {
            let uuid = body.grandmaster_clock_uuid;
            match self.gm_uuid {
                Some(current) if current != uuid => {
                    warn!(
                        "[FOLLOWER] grandmaster changed: {:02x?} -> {:02x?}",
                        current, uuid
                    );
                    self.gm_uuid = Some(uuid);
                    self.on_time_base_change();
                }
                None => {
                    info!("[FOLLOWER] grandmaster {:02x?}", uuid);
                    self.gm_uuid = Some(uuid);
                }
                _ => {}
            }
        }
        if self.pending.len() >= MAX_PENDING_SYNCS {
            let newest = p.mono_rx_ns;
            self.pending
                .retain(|_, s| newest.saturating_sub(s.mono_rx_ns) < 5_000_000_000);
            if self.pending.len() >= MAX_PENDING_SYNCS {
                return;
            }
        }
        self.pending.insert(
            header.sequence_id,
            PendingSync {
                mono_rx_ns: p.mono_rx_ns,
                wall_rx_ns: p.wall_rx_ns,
                source_uuid: source,
            },
        );
    }

    fn on_follow_up(&mut self, header: &PtpV1Header, data: &[u8]) {
        let Ok(body) = PtpV1FollowUpBody::parse(&data[PtpV1Header::SIZE..]) else {
            return;
        };
        let Some(sync) = self.pending.remove(&body.associated_sequence_id) else {
            return;
        };
        if sync.source_uuid != header.source_uuid {
            return;
        }
        let t1 = body.precise_origin_timestamp.to_nanos();
        let t2 = self.clock.at(sync.mono_rx_ns);
        let add = self.prev_t1_ns == 0 || (t1 - self.prev_t1_ns).abs() >= MIN_SAMPLE_SPACING_NS;
        self.prev_t1_ns = t1;
        if !add {
            return;
        }
        self.window.push(Sample {
            raw_ns: t2.wrapping_sub(t1),
            wall_minus_v_ns: sync.wall_rx_ns.wrapping_sub(t2),
        });
        if self.window.len() >= WINDOW_SAMPLES {
            self.close_window(t1, sync.mono_rx_ns);
        }
    }

    fn close_window(&mut self, t1_ns: i64, mono_ns: i64) {
        let mut raw: Vec<i64> = self.window.iter().map(|s| s.raw_ns).collect();
        let mut wall: Vec<i64> = self.window.iter().map(|s| s.wall_minus_v_ns).collect();
        self.window.clear();
        raw.sort_unstable();
        wall.sort_unstable();
        let median = raw[raw.len() / 2];
        let wall_minus_v = wall[wall.len() / 2];
        self.windows += 1;

        // The shared rate servo, in the service's order (the service's PtpController).
        let OffsetMeasurement {
            measured,
            dt_secs,
            raw_rate_ppm,
        } = self
            .rate
            .measure_offset(median as f64 / 1_000.0, MonoNs(mono_ns));
        let raw_rate_ppm = raw_rate_ppm.clamp(-500.0, 500.0);
        let raw_rate_ppm = if measured && dt_secs > 0.1 {
            (raw_rate_ppm - self.deviation.span_ppm()).clamp(-500.0, 500.0)
        } else {
            raw_rate_ppm
        };
        // dsync#136: a seeded start is judged on its first measured windows (the service's
        // order: before the integrator step, so a dropped seed learns from 0 in this window).
        let raw_rate_ppm = if measured && dt_secs > 0.1 {
            raw_rate_ppm - self.judge_seed(raw_rate_ppm, dt_secs)
        } else {
            raw_rate_ppm
        };
        let rate_ppm = if measured {
            self.rate.measure(raw_rate_ppm, || MonoNs(mono_ns))
        } else {
            self.rate.smoothed_rate_ppm
        };
        if measured {
            self.rate.update_modes(rate_ppm);
        }
        let ServoWord { word_ppm, .. } = self.rate.word(rate_ppm, measured, self.core.engaged());

        // The shared phase lock and its window glue (`crate::phase_window`, as the service).
        let dt = phase_window::window_dt(&mut self.last_window_t1_ns, t1_ns, dt_secs);
        let was_engaged = self.core.engaged();
        let reseed_before = self.core.boot_reseed();
        let fit_before = self.core.boot_fit_line();
        let late_before = self.core.late_windows();
        let refine_before = self.core.boot_refine();
        let locked = phase_window::engage_on(self.rate.is_locked, self.stale);
        let out = self.core.on_window(median, locked, word_ppm, dt);
        if let (Some(f), None) = (self.core.boot_refine(), refine_before) {
            info!(
                "[FOLLOWER] boot refinement re-seeded the integrator to {:+.3}ppm (σ {:.2}ppm, {} windows over {:.1}s)",
                f.freq_ppm, f.sigma_ppm, f.points, f.span_s
            );
        }
        let late = self.core.late_windows() > late_before;
        match (self.core.engaged(), out.event, out.error_ns) {
            (true, AnchorEvent::None, Some(e)) if !late => {
                if self.recent_e.len() == SETTLED_WINDOWS {
                    self.recent_e.pop_front();
                }
                self.recent_e.push_back(e.saturating_abs());
            }
            (true, AnchorEvent::None, _) => {}
            _ => self.recent_e.clear(),
        }
        if late {
            info!(
                "[FOLLOWER] late window held: e={:?}ns, the PI keeps {:+.3}ppm ({} held since start)",
                out.error_ns,
                self.core.last_freq_ppm(),
                self.core.late_windows()
            );
        }
        if !was_engaged && self.core.engaged() {
            match self.core.engaged_on_fit() {
                Some(f) => info!(
                    "[FOLLOWER] phase lock engaged on the acquisition fit {:+.3}ppm (σ {:.2}ppm, {} windows over {:.1}s; rate servo word {:+.3}ppm)",
                    f.freq_ppm, f.sigma_ppm, f.points, f.span_s, word_ppm
                ),
                None => info!(
                    "[FOLLOWER] phase lock engaged on the rate servo's word {:+.3}ppm (acquisition fit {})",
                    word_ppm,
                    fit_before.map_or("none".to_string(), |l| format!(
                        "{:+.3}ppm σ {:.2}ppm over {} windows / {:.1}s, refused or too short",
                        -l.slope_ppm, l.sigma_ppm, l.points, l.span_s
                    ))
                ),
            }
        }
        if was_engaged && fit_before.is_some() && self.core.boot_fit_line().is_none() {
            match (self.core.boot_reseed(), reseed_before) {
                (Some(f), None) => info!(
                    "[FOLLOWER] boot fit re-seeded the integrator to {:+.3}ppm (σ {:.2}ppm, {} windows over {:.1}s)",
                    f.freq_ppm, f.sigma_ppm, f.points, f.span_s
                ),
                _ => info!(
                    "[FOLLOWER] boot fit closed without a re-seed (last fit {})",
                    fit_before.map_or("none".to_string(), |l| format!(
                        "{:+.3}ppm σ {:.2}ppm over {} windows / {:.1}s",
                        -l.slope_ppm, l.sigma_ppm, l.points, l.span_s
                    ))
                ),
            }
        }
        match out.event {
            AnchorEvent::None => {}
            AnchorEvent::Anchored { anchor_ns } => {
                info!("[FOLLOWER] anchored: D_v {}ns", anchor_ns);
            }
            AnchorEvent::Rebased { old_ns, new_ns } => {
                self.epoch = self.epoch.wrapping_add(1);
                self.reanchors = self.reanchors.wrapping_add(1);
                info!(
                    "[FOLLOWER] new time base: re-anchored by {:+}ns (epoch {})",
                    new_ns.wrapping_sub(old_ns),
                    self.epoch
                );
            }
            AnchorEvent::Realigned { old_ns, new_ns } => {
                self.reanchors = self.reanchors.wrapping_add(1);
                info!(
                    "[FOLLOWER] re-aligned by {:+}ns (re-anchor {})",
                    new_ns.wrapping_sub(old_ns),
                    self.reanchors
                );
            }
        }
        let close = phase_window::close_window(
            &self.core,
            &mut self.gate,
            &mut self.deviation,
            &out,
            was_engaged,
            word_ppm,
        );
        if let Some(event) = close.gate_event {
            info!("[FOLLOWER] lock gate: {:?}", event);
        }
        if let Some(baseline) = close.baseline_ppm {
            self.rate.drift_baseline_ppm = baseline;
        }
        let applied = close.word_ppm;
        self.clock.set_rate(mono_ns, applied);
        self.word_before = self.word_last;
        self.word_last = applied;
        self.date_offset_ns = self.core.anchor_ns().map(|d| wall_minus_v.wrapping_add(d));
        debug!(
            "[FOLLOWER] window: e={:?}ns word {:+.3}ppm rate {:+.2}us/s rate-lock {} engaged {} locked {} boot-fit {}",
            out.error_ns,
            applied,
            rate_ppm,
            self.rate.is_locked,
            self.core.engaged(),
            self.gate.locked(),
            self.core.boot_fit_line().map_or("-".to_string(), |l| format!(
                "{:+.3}ppm σ {:.2} n {} {:.1}s",
                -l.slope_ppm, l.sigma_ppm, l.points, l.span_s
            ))
        );
    }
}

#[cfg(test)]
mod tests;
