//! The in-process Dante PTP follower (dantesync#136): an application locks to the Dante grandmaster
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
//!         // ptp_ns is Dante PTP time, within the lock bound (50 µs) of the grandmaster line
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
//!   its own Npcap capture (the Npcap runtime is required, and the app's binary should delay-load
//!   `wpcap.dll` as the service's `build.rs` does).
//! - [`PtpClock::now_ptp_ns`] is a lock-free read ([`crate::seqlock`]): one raw counter read and
//!   one multiply-add, no allocation, no lock, no syscall beyond the counter.
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
pub mod model;

use self::core::{FollowerCore, PacketVerdict, RxPacket};
use self::model::{ClockModel, MODEL_WORDS};
use crate::gm_filter::GmAllowlist;
use crate::mono::{self, Continuity, StampConverter};
use crate::network::PtpNetwork;
use crate::ptp_rejoin::RejoinSchedule;
use crate::seqlock::{SeqLock, SeqWriter};
use anyhow::Result;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use self::core::{MIN_SAMPLE_SPACING_NS, STALE_AFTER_NS, WINDOW_SAMPLES};

/// How often the receive thread looks for packets when none is waiting (the service's loop
/// period on Linux).
pub const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Where and from whom to follow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FollowerConfig {
    /// The interface the Dante grandmaster is on (Linux: its name, `enp2s0`; Windows: the
    /// adapter's friendly name or GUID). The follower uses only this NIC (dantesync#131).
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
}

impl PtpClock {
    /// The published model.
    pub fn model(&self) -> ClockModel {
        ClockModel::from_words(self.model.read())
    }

    /// PTP time now (ns); `None` until the follower has anchored on the grandmaster.
    pub fn now_ptp_ns(&self) -> Option<i64> {
        let m = mono::mono_now_ns();
        self.model().ptp_at(m)
    }

    /// PTP time at a raw-counter instant ([`mono::mono_now_ns`]), e.g. a stamp the app took.
    pub fn ptp_at_mono_ns(&self, mono_ns: i64) -> Option<i64> {
        self.model().ptp_at(mono_ns)
    }

    /// The state (lock, holdover, grandmaster, offsets, counters).
    pub fn state(&self) -> FollowerState {
        self.model().into()
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
    pub fn start(config: FollowerConfig) -> Result<Follower> {
        let network = open_network(&config)?;
        Self::with_network(network, &config)
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
        let clock = PtpClock { model: lock };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            let publisher = Publisher {
                writer,
                last: core.model().to_words(),
            };
            std::thread::Builder::new()
                .name("dsync-ptp-follower".into())
                .spawn(move || run(network, core, publisher, &stop))?
        };
        info!(
            "[FOLLOWER] following Dante PTP on {:?} (dsync-ptp {})",
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

/// The receive thread: drain the packets, convert each stamp onto the raw counter (dropping one
/// that straddles a wall step), feed the core, tick it, re-join while stale, publish.
fn run<N: PtpNetwork>(
    mut network: N,
    mut core: FollowerCore,
    mut publisher: Publisher,
    stop: &AtomicBool,
) {
    let mut stamps = StampConverter::new();
    stamps.observe(mono::read_time_base());
    let mut schedule = RejoinSchedule::new();
    let mut last_rx = Instant::now();
    let mut warned_at: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        loop {
            let received = network.recv_packet();
            let reading = mono::read_time_base();
            let stepped = matches!(stamps.observe(reading), Continuity::Stepped { .. });
            if stepped {
                core.on_wall_step();
            }
            match received {
                Ok(Some((data, size, wall_rx, source_ip))) => {
                    if stepped {
                        continue;
                    }
                    let wall_rx_ns = match wall_rx.duration_since(std::time::UNIX_EPOCH) {
                        Ok(d) => d.as_nanos() as i64,
                        Err(_) => continue,
                    };
                    let verdict = core.on_packet(RxPacket {
                        data: &data[..size.min(data.len())],
                        mono_rx_ns: reading.to_mono(wall_rx_ns),
                        wall_rx_ns,
                        source_ip,
                    });
                    if verdict == PacketVerdict::TimeMessage {
                        last_rx = Instant::now();
                        schedule.reset();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    if warned_at.map_or(true, |t| t.elapsed() >= Duration::from_secs(30)) {
                        warn!("[FOLLOWER] receive failed: {}", e);
                        warned_at = Some(Instant::now());
                    }
                    break;
                }
            }
        }
        core.on_tick(mono::mono_now_ns());
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
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn open_network(config: &FollowerConfig) -> Result<impl PtpNetwork + Send + 'static> {
    use crate::net_linux::{KernelSockets, UdpPtpNetwork};
    let ip = pinned_ip(&config.interface)?;
    Ok(
        UdpPtpNetwork::join(KernelSockets, config.interface.clone(), ip)?
            .pinned_to(Some(config.interface.clone())),
    )
}

#[cfg(windows)]
fn open_network(config: &FollowerConfig) -> Result<impl PtpNetwork + Send + 'static> {
    let ip = pinned_ip(&config.interface)?;
    crate::net_pcap::NpcapPtpNetwork::new(
        &config.interface,
        &GmAllowlist::default(),
        Some((config.interface.clone(), ip)),
    )
}

/// The pinned interface's IPv4 now (a link-local one counts: a Dante segment without DHCP).
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
