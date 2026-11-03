//! The in-process PTPv1 follower (dsync#136): an application locks to the grandmaster of the audio network
//! in its own process, with the dsync service's own PTP code, without touching the system clock.
//!
//! ```no_run
//! use dsync_ptp::follower::{Follower, FollowerConfig};
//!
//! let follower = Follower::start(FollowerConfig::on_interface("enp2s0"))?;
//! let clock = follower.clock(); // Clone + Send + Sync: hand it to the real-time thread
//! if let Some(ptp_ns) = clock.now_ptp_ns() {
//!     let state = clock.state();
//!     if state.locked {
//!         // ptp_ns is the grandmaster's PTP time, within the lock bound (50 µs) of the grandmaster line
//!         let _ = ptp_ns;
//!     }
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! # What it is
//!
//! - It never sends (one-way model, ROZHODNUTÉ on #136) and never steers the system clock: it runs
//!   a VIRTUAL clock over the raw monotonic counter ([`crate::mono`]) with the service's pipeline
//!   ([`core::FollowerCore`]): the rate servo acquires, the phase lock holds `e = (t2 − t1) − D_v`,
//!   and `locked` is DSYX [41]'s definition. So the library and the service on one PC give the same
//!   PTP time: both trail the grandmaster by the same one-way receive latency.
//! - Several followers and the dsync service share the PTP ports on one PC: on Linux every
//!   receiver binds 319/320 with `SO_REUSEADDR` and the kernel delivers each multicast datagram to
//!   every member (binding 319 needs `CAP_NET_BIND_SERVICE` or root); on Windows each process has
//!   its own Npcap capture. The Npcap runtime is loaded at run time ([`crate::wpcap`]): the app
//!   needs no SDK to build and no linker flag, and without Npcap `start` returns
//!   [`FollowerError::NpcapMissing`].
//! - [`Follower::start`] says why it failed ([`FollowerError`]): the app logs the reason and
//!   falls back to the service's DSYX.
//! - [`PtpClock::now_ptp_ns`] is a lock-free read that never waits for the writer
//!   ([`crate::seqlock`], double-buffered): one raw counter read and one multiply-add, no
//!   allocation, no lock. The counter read is a vDSO call on current Linux kernels
//!   (`CLOCK_MONOTONIC_RAW` is in the x86-64 vDSO since 5.3; an older kernel makes it a real
//!   syscall) and `QueryPerformanceCounter` on Windows.
//! - A receive thread that stops ticking (stuck or panicked) is never reported locked: readers
//!   see a holdover after [`STALE_AFTER_NS`] without its heartbeat.
//! - A wall-clock step on the PC (the service's date steps) never reaches the follower's timeline:
//!   receive stamps are converted to the raw counter after every receive, and a packet received
//!   across a step is dropped ([`crate::mono::StampConverter`]).
//!
//! # Time bases
//!
//! [`FollowerState::epoch`] changes on a new grandmaster time base, [`FollowerState::reanchors`] on
//! every discontinuity of the published time (a time-base change or a re-alignment): a consumer
//! re-syncs its media clock when either changes.

pub mod core;
mod error;
pub mod model;

pub use self::error::FollowerError;

use self::core::{FollowerCore, PacketVerdict, RxPacket};
use self::model::{ClockModel, MODEL_WORDS};
use crate::gm_filter::GmAllowlist;
use crate::mono::{self, Continuity, StampConverter, TimeBaseReading};
use crate::network::PtpNetwork;
use crate::ptp_rejoin::RejoinSchedule;
use crate::seqlock::{SeqLock, SeqWriter};
use anyhow::Result;
use log::{debug, info, warn};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use self::core::{MIN_SAMPLE_SPACING_NS, STALE_AFTER_NS, WINDOW_SAMPLES};

/// How often the receive thread looks for packets when none is waiting (the service's loop
/// period on Linux).
pub const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Where and from whom to follow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FollowerConfig {
    /// The interface the grandmaster is on (Linux: its name, `enp2s0`; Windows: the
    /// adapter's friendly name or GUID). The follower uses only this NIC (dsync#131).
    pub interface: String,
    /// Accept PTP only from these grandmaster sources (IPs, CIDRs, hostnames; empty = any), the
    /// service's `system.gm_allowlist`.
    pub gm_allowlist: Vec<String>,
}

impl FollowerConfig {
    pub fn on_interface(interface: impl Into<String>) -> Self {
        FollowerConfig {
            interface: interface.into(),
            gm_allowlist: Vec::new(),
        }
    }
}

/// The follower's state as a reader sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FollowerState {
    /// DSYX [41]: |e| ≤ 50 µs for 3 windows, cleared at > 150 µs for 4, on disengagement, and
    /// while PTP is stale.
    pub locked: bool,
    /// No PTP for 10 s: the clock runs on the learned frequency.
    pub holdover: bool,
    /// The grandmaster clock UUID.
    pub gm_uuid: Option<[u8; 6]>,
    /// `wall = PTP + D` on this PC's wall clock: the fleet date offset where the service runs.
    pub date_offset_ns: Option<i64>,
    /// The phase-lock error `e` of the last window (ns).
    pub offset_ns: Option<i64>,
    /// The virtual clock's frequency against the raw counter (ppm).
    pub freq_ppm: f64,
    pub epoch: u32,
    pub reanchors: u32,
    pub holdovers: u32,
    pub windows: u64,
}

impl From<ClockModel> for FollowerState {
    fn from(m: ClockModel) -> Self {
        FollowerState {
            locked: m.locked,
            holdover: m.holdover,
            gm_uuid: m.gm_uuid,
            date_offset_ns: m.date_offset_ns,
            offset_ns: m.offset_ns,
            freq_ppm: m.rate_ppm,
            epoch: m.epoch,
            reanchors: m.reanchors,
            holdovers: m.holdovers,
            windows: m.windows,
        }
    }
}

/// A reader's handle on the follower's clock: `Clone + Send + Sync`, lock-free.
#[derive(Clone, Debug)]
pub struct PtpClock {
    model: Arc<SeqLock<MODEL_WORDS>>,
    /// The receive thread's last pass on the raw counter ([`DEAD`] once it ended or panicked):
    /// a model whose thread stopped ticking is never reported locked.
    heartbeat: Arc<AtomicI64>,
}

/// The heartbeat of a receive thread that ended (dropped or panicked).
const DEAD: i64 = i64::MIN;

impl PtpClock {
    /// The published model as of the raw-counter instant `mono_ns`: a receive thread silent for
    /// [`STALE_AFTER_NS`] (stuck or dead) makes it a holdover, never locked.
    fn model_at(&self, mono_ns: i64) -> ClockModel {
        let mut model = ClockModel::from_words(self.model.read());
        let beat = self.heartbeat.load(Ordering::Acquire);
        if beat == DEAD || mono_ns.saturating_sub(beat) > STALE_AFTER_NS {
            model.locked = false;
            model.holdover = model.anchored;
        }
        model
    }

    /// The published model now.
    pub fn model(&self) -> ClockModel {
        self.model_at(mono::mono_now_ns())
    }

    /// PTP time now (ns); `None` until the follower has anchored on the grandmaster.
    pub fn now_ptp_ns(&self) -> Option<i64> {
        let m = mono::mono_now_ns();
        self.model_at(m).ptp_at(m)
    }

    /// PTP time at a raw-counter instant ([`mono::mono_now_ns`]), e.g. a stamp the app took.
    pub fn ptp_at_mono_ns(&self, mono_ns: i64) -> Option<i64> {
        self.model().ptp_at(mono_ns)
    }

    /// The state (lock, holdover, grandmaster, offsets, counters).
    pub fn state(&self) -> FollowerState {
        self.model().into()
    }

    /// The receive thread is running (it has not ended or panicked).
    pub fn alive(&self) -> bool {
        self.heartbeat.load(Ordering::Acquire) != DEAD
    }
}

/// Publishes the core's model to the readers.
struct Publisher {
    writer: SeqWriter<MODEL_WORDS>,
    last: [u64; MODEL_WORDS],
}

impl Publisher {
    fn publish(&mut self, model: ClockModel) {
        let words = model.to_words();
        if words != self.last {
            self.writer.write(words);
            self.last = words;
        }
    }
}

/// A running follower: its receive thread lives as long as this value (dropped = stopped and
/// joined).
pub struct Follower {
    clock: PtpClock,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Follower {
    /// Join the PTP group on `config.interface` with this platform's receive path and follow.
    pub fn start(config: FollowerConfig) -> std::result::Result<Follower, FollowerError> {
        let network = open_network(&config)?;
        Self::with_network(network, &config).map_err(FollowerError::Other)
    }

    /// Follow over a given receive path (a test's sockets, or a backend the app opened itself).
    pub fn with_network<N: PtpNetwork + Send + 'static>(
        network: N,
        config: &FollowerConfig,
    ) -> Result<Follower> {
        let mut allowlist = GmAllowlist::parse(&config.gm_allowlist);
        if let Some(reason) = allowlist.unresolvable_reason() {
            warn!("[FOLLOWER] gm_allowlist: {}", reason);
        }
        if allowlist.has_hostnames() {
            allowlist.resolve(&crate::gm_filter::StdResolver);
        }
        let core = FollowerCore::new(allowlist, mono::mono_now_ns());
        let (lock, writer) = SeqLock::new(core.model().to_words());
        let heartbeat = Arc::new(AtomicI64::new(mono::mono_now_ns()));
        let clock = PtpClock {
            model: lock,
            heartbeat: Arc::clone(&heartbeat),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            let publisher = Publisher {
                writer,
                last: core.model().to_words(),
            };
            std::thread::Builder::new()
                .name("dsync-ptp-follower".into())
                .spawn(move || {
                    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(network, core, publisher, &heartbeat, &stop)
                    }));
                    // Ended or panicked: readers see no lock from here on.
                    heartbeat.store(DEAD, Ordering::Release);
                    if ran.is_err() {
                        warn!("[FOLLOWER] the receive thread panicked: the clock is in holdover");
                    }
                })?
        };
        info!(
            "[FOLLOWER] following PTPv1 on {:?} (dsync-ptp {})",
            config.interface,
            crate::VERSION
        );
        Ok(Follower {
            clock,
            stop,
            thread: Some(thread),
        })
    }

    /// A reader's handle (clone it freely).
    pub fn clock(&self) -> PtpClock {
        self.clock.clone()
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                warn!("[FOLLOWER] the receive thread panicked");
            }
        }
    }
}

/// The receive thread: a receive pass, the tick, the re-join while stale, the publish, the
/// heartbeat; every [`POLL_INTERVAL`].
fn run<N: PtpNetwork>(
    mut network: N,
    mut core: FollowerCore,
    mut publisher: Publisher,
    heartbeat: &AtomicI64,
    stop: &AtomicBool,
) {
    let mut rx = Receiver::new();
    let mut schedule = RejoinSchedule::new();
    let mut last_rx = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if rx.pass(&mut network, &mut core, &mut mono::read_time_base) {
            last_rx = Instant::now();
            schedule.reset();
        }
        let now_mono = mono::mono_now_ns();
        core.on_tick(now_mono);
        let now = Instant::now();
        if schedule.due(last_rx, now) {
            let attempt = schedule.record_attempt(now);
            match network.rejoin() {
                Ok(outcome) => info!(
                    "[FOLLOWER] re-joined the PTP group on {} ({}), attempt {}",
                    outcome.iface, outcome.ip, attempt
                ),
                Err(e) => warn!("[FOLLOWER] re-join attempt {} failed: {}", attempt, e),
            }
        }
        publisher.publish(core.model());
        heartbeat.store(now_mono, Ordering::Release);
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The receive side of the thread: every receive followed by a reading of both clocks, and the
/// stamp converted with `wall − raw` interpolated between the previous reading and this one.
struct Receiver {
    stamps: StampConverter,
    warned_at: Option<Instant>,
    /// A step was revealed by the closing read of the last pass (after an empty receive or an
    /// error): a packet stamped before it may be the first of the next pass.
    step_carried: bool,
}

impl Receiver {
    fn new() -> Self {
        let mut stamps = StampConverter::new();
        stamps.observe(mono::read_time_base());
        Receiver {
            stamps,
            warned_at: None,
            step_carried: false,
        }
    }

    /// Drain every waiting packet into `core`; `true` when a time message arrived.
    ///
    /// A wall step is seen by the first reading after it, which is the first receive of the
    /// pass that follows it. Every packet of that pass may have been received (and stamped)
    /// before the step yet be converted after it, so the WHOLE pass is dropped, not only the
    /// packet that revealed the step: packets queued across a step (a stalled thread) would
    /// otherwise each be off by the step. A step revealed by the pass's CLOSING read (after the
    /// empty receive, or an error) also drops the next pass: a packet that arrived between that
    /// receive and the step is read there, under readings that look continuous.
    fn pass<N: PtpNetwork>(
        &mut self,
        network: &mut N,
        core: &mut FollowerCore,
        read: &mut impl FnMut() -> TimeBaseReading,
    ) -> bool {
        let mut stepped = std::mem::take(&mut self.step_carried);
        let mut time_message = false;
        loop {
            let received = network.recv_packet();
            let prev = self.stamps.last();
            let reading = read();
            if let Continuity::Stepped { by_ns } = self.stamps.observe(reading) {
                debug!(
                    "[FOLLOWER] the wall clock stepped {:+}ns: this pass is dropped",
                    by_ns
                );
                core.on_wall_step();
                stepped = true;
                self.step_carried = !matches!(received, Ok(Some(_)));
            }
            match received {
                Ok(Some((data, size, wall_rx, source_ip))) => {
                    if stepped {
                        core.note_straddling_packet();
                        continue;
                    }
                    let Ok(since_epoch) = wall_rx.duration_since(std::time::UNIX_EPOCH) else {
                        continue;
                    };
                    let wall_rx_ns = since_epoch.as_nanos() as i64;
                    let verdict = core.on_packet(RxPacket {
                        data: &data[..size.min(data.len())],
                        mono_rx_ns: reading.to_mono_between(prev, wall_rx_ns),
                        wall_rx_ns,
                        source_ip,
                    });
                    time_message |= verdict == PacketVerdict::TimeMessage;
                }
                Ok(None) => break,
                Err(e) => {
                    if self
                        .warned_at
                        .map_or(true, |t| t.elapsed() >= Duration::from_secs(30))
                    {
                        warn!("[FOLLOWER] receive failed: {}", e);
                        self.warned_at = Some(Instant::now());
                    }
                    break;
                }
            }
        }
        time_message
    }
}

/// The interface first (the most specific reason), then the platform's receive path.
fn interface_ip(config: &FollowerConfig) -> std::result::Result<std::net::Ipv4Addr, FollowerError> {
    pinned_ip(&config.interface).map_err(|cause| FollowerError::InterfaceNotFound {
        interface: config.interface.clone(),
        cause,
    })
}

#[cfg(unix)]
fn open_network(
    config: &FollowerConfig,
) -> std::result::Result<impl PtpNetwork + Send + 'static, FollowerError> {
    use crate::net_linux::{KernelSockets, UdpPtpNetwork};
    let ip = interface_ip(config)?;
    let network = UdpPtpNetwork::join(KernelSockets, config.interface.clone(), ip)
        .map_err(FollowerError::from_open)?;
    Ok(network.pinned_to(Some(config.interface.clone())))
}

#[cfg(windows)]
fn open_network(
    config: &FollowerConfig,
) -> std::result::Result<impl PtpNetwork + Send + 'static, FollowerError> {
    let ip = interface_ip(config)?;
    crate::wpcap::available().map_err(|missing| FollowerError::NpcapMissing {
        detail: missing.detail,
    })?;
    crate::net_pcap::NpcapPtpNetwork::new(
        &config.interface,
        &GmAllowlist::default(),
        Some((config.interface.clone(), ip)),
    )
    .map_err(FollowerError::from_open)
}

/// The pinned interface's IPv4 now (a link-local one counts: an audio network segment without DHCP).
fn pinned_ip(name: &str) -> Result<std::net::Ipv4Addr> {
    use crate::net::PinnedLookup;
    match crate::net::pinned_interface(name) {
        PinnedLookup::Ready(ip) | PinnedLookup::LinkLocalOnly(ip) => Ok(ip),
        other => Err(
            match crate::net::PinnedInterfaceUnavailable::from_lookup(name, other) {
                Some(unavailable) => unavailable.into(),
                None => anyhow::anyhow!("interface {:?} has no usable IPv4", name),
            },
        ),
    }
}

#[cfg(test)]
mod tests;
