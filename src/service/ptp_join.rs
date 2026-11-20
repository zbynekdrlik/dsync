//! The PTP receive path's startup join (moved from `main.rs`, dsync#214): the pinned
//! interface (dsync#131; #212 a list) waited for and opened, or the unpinned default interface
//! with a warning; a failed unpinned open starts the loop unopened (dsync#206).

use anyhow::Result;
#[cfg(windows)]
use log::error;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::thread;
use std::time::{Duration, Instant};

use dsync::config::file::ConfigState;
use dsync::config::SystemConfig;
use dsync::net;
#[cfg(unix)]
use dsync::net_linux;
#[cfg(windows)]
use dsync::net_pcap;
use dsync::status::SyncStatus;

use crate::{poll_ready, Args};

/// The PTP receive path of this platform: kernel-timestamped sockets on Linux, Npcap on Windows.
#[cfg(unix)]
pub(crate) type PlatformNetwork = net_linux::UdpPtpNetwork;
#[cfg(windows)]
pub(crate) type PlatformNetwork = net_pcap::NpcapPtpNetwork;

/// Where `/status` says PTP joined at startup: the pin (or `None`), the interface and its IPv4.
pub(crate) struct StartupJoin {
    pub(crate) network: PlatformNetwork,
    pub(crate) pinned: Option<String>,
    /// dsync#212: the listed name joined (`None` when not pinned).
    pub(crate) pinned_name: Option<String>,
    pub(crate) iface: String,
    pub(crate) ip: std::net::Ipv4Addr,
    /// dsync#206: why the receive path is not open yet (`None` = open).
    pub(crate) not_open: Option<String>,
}

/// Open the PTP receive path for `iface_name` / `iface_ip` (the startup choice; pinned: the
/// listed name chosen). `pinned` (dsync#131; #212: the canonical list) names the only
/// interfaces the backend may use from now on, re-joins included.
fn open_ptp_network(
    iface_name: &str,
    iface_ip: std::net::Ipv4Addr,
    pinned: Option<String>,
    system_config: &SystemConfig,
) -> Result<(PlatformNetwork, String, std::net::Ipv4Addr)> {
    // Linux: the event + general sockets join the PTP group (IGMP) with kernel timestamping
    // (`net_linux`, which logs the "Joined Multicast Groups on ..." line).
    #[cfg(unix)]
    {
        let _ = system_config;
        let network = net_linux::UdpPtpNetwork::join(
            net_linux::KernelSockets,
            iface_name.to_string(),
            iface_ip,
        )?
        .pinned_to(pinned);
        let (iface, ip) = network
            .interface()
            .map(|(name, ip)| (name.to_string(), ip))
            .unwrap_or((iface_name.to_string(), iface_ip));
        Ok((network, iface, ip))
    }

    #[cfg(windows)]
    {
        // Use Npcap with HostHighPrec timestamps (KeQuerySystemTimePrecise)
        // This provides driver-level timestamps that are both precise AND synced with system time.
        //
        // camera-box issue 1073: on a multi-homed box the OS default interface
        // (iface_name/iface_ip above) may be the WRONG NIC (e.g. the mbc side on
        // the stream box), so hand NpcapPtpNetwork the trusted-source allowlist —
        // it picks the capture/IGMP-join interface on the grandmaster's subnet
        // when the allowlist can disambiguate, and otherwise falls back to the
        // default interface (single-homed / no-allowlist boxes are unchanged).
        // dsync#131: a PINNED interface alone selects the device (no subnet rule, so no
        // allowlist resolution), and a failed open is the pinned wait's to report and retry
        // (once per change, never an ERROR per second).
        if let Some(name) = pinned {
            let npcap_net = net_pcap::NpcapPtpNetwork::new(
                iface_name,
                &dsync::gm_filter::GmAllowlist::default(),
                Some((name, iface_ip)),
            )?;
            info!(
                "Using Npcap HostHighPrec timestamps (pinned {})",
                iface_name
            );
            let (iface, ip) = npcap_net.joined();
            let (iface, ip) = (iface.to_string(), ip);
            return Ok((npcap_net, iface, ip));
        }
        let mut gm_allowlist = dsync::gm_filter::GmAllowlist::parse(&system_config.gm_allowlist);
        // dsync#113: resolve hostname entries BEFORE interface selection so a
        // hostname-addressed grandmaster (e.g. video-clock.lan) can pick the
        // capture NIC on the GM's subnet on a multi-homed box, exactly like a
        // literal entry. The controller re-resolves periodically after this.
        if gm_allowlist.has_hostnames() {
            let outcome = gm_allowlist.resolve(&dsync::gm_filter::StdResolver);
            info!(
                "gm_allowlist: startup hostname resolution {:?} (unresolved: {:?})",
                outcome.new_resolved,
                gm_allowlist.unresolved_hostnames()
            );
        }
        match net_pcap::NpcapPtpNetwork::new(iface_name, &gm_allowlist, None) {
            Ok(npcap_net) => {
                info!(
                    "Using Npcap HostHighPrec timestamps (default-interface hint {} / {})",
                    iface_name, iface_ip
                );
                let (iface, ip) = npcap_net.joined();
                let (iface, ip) = (iface.to_string(), ip);
                Ok((npcap_net, iface, ip))
            }
            Err(e) => {
                error!(
                    "Failed to initialize Npcap: {}. Npcap is required on Windows.",
                    e
                );
                Err(e)
            }
        }
    }
}

/// dsync#206: the backend with NO receive path open (the state a failed re-join leaves), for a
/// startup whose open failed: `recv_packet` reads nothing and the controller's #112 re-join opens
/// it through the same selection as startup.
fn unopened_ptp_network(
    iface_name: &str,
    iface_ip: std::net::Ipv4Addr,
    system_config: &SystemConfig,
) -> PlatformNetwork {
    #[cfg(unix)]
    {
        let _ = system_config;
        net_linux::UdpPtpNetwork::unopened(
            net_linux::KernelSockets,
            iface_name.to_string(),
            iface_ip,
        )
    }
    #[cfg(windows)]
    {
        let gm_allowlist = dsync::gm_filter::GmAllowlist::parse(&system_config.gm_allowlist);
        net_pcap::NpcapPtpNetwork::unopened(iface_name, iface_ip, &gm_allowlist)
    }
}

/// dsync#131 (owner ruling 1.11.2026): the PTP interface is an explicit setting.
///
/// Pinned (`system.interface` / `--interface`): wait for exactly that NIC and open on it —
/// never another NIC — with the clock alarm naming it meanwhile (`pinned_wait`). Unpinned: the
/// previous selection, with a warning. `Ok(None)` = the service stopped while waiting.
pub(crate) fn join_ptp_network(
    args: &Args,
    system_config: &SystemConfig,
    running: &AtomicBool,
    status_shared: &RwLock<SyncStatus>,
    config_state: &ConfigState,
    ready_gate: &mut dsync::boot_ready::ReadyGate,
) -> Result<Option<StartupJoin>> {
    let cli = args
        .interface
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let pinned = system_config.pinned_interface(cli);
    let Some(name) = pinned else {
        warn!(
            "[NET] the PTP interface is NOT pinned (system.interface / --interface unset): \
             joining the first IPv4 interface in listing order — set system.interface to the \
             PTP NIC (dsync#131)"
        );
        let (iface_name, iface_ip) = loop {
            match net::get_default_interface() {
                Ok(res) => break res,
                Err(e) => {
                    if !running.load(Ordering::SeqCst) {
                        return Ok(None);
                    }
                    warn!("Waiting for network interface... ({})", e);
                    thread::sleep(Duration::from_secs(5));
                }
            }
        };
        // dsync#206: a failed open at boot (the Npcap device list lags the adapters) never ends
        // the loop.
        let opened = dsync::startup_open::open_unpinned(
            || open_ptp_network(&iface_name, iface_ip, None, system_config),
            || {
                (
                    unopened_ptp_network(&iface_name, iface_ip, system_config),
                    iface_name.clone(),
                    iface_ip,
                )
            },
        )?;
        let (network, iface, ip) = opened.network;
        return Ok(Some(StartupJoin {
            network,
            pinned: None,
            pinned_name: None,
            iface,
            ip,
            not_open: opened.not_open,
        }));
    };
    info!(
        "[NET] PTP interface pinned to {} ({})",
        name,
        if cli.and_then(net::canonical_interface_spec).is_some() {
            "--interface"
        } else {
            "system.interface"
        }
    );
    let wait = dsync::pinned_wait::PinnedWait {
        name: &name,
        status: status_shared,
        alarm_interval_s: system_config.clock_alarm_interval_s,
        config_problem: config_state.alarm_reason(),
        notifier: dsync::clock_alarm::DesktopNotifier,
        running,
        // dsync#193: READY is due at the bound even while the NIC is still missing.
        waiting: &mut |_, state| {
            poll_ready(
                ready_gate,
                Err(state),
                "waiting for the pinned PTP interface",
            )
        },
    };
    // dsync#212: a list joins the listed NIC that carries an IPv4 now.
    let opened = wait.run_list(
        || net::pinned_choice(&name, net::PinnedOrder::Startup),
        |chosen, ip| open_ptp_network(chosen, ip, Some(name.clone()), system_config),
        || thread::sleep(dsync::pinned_wait::LOOK_EVERY),
        Instant::now,
    );
    Ok(opened.map(|((network, iface, ip), chosen, _)| StartupJoin {
        network,
        pinned: Some(name.clone()),
        pinned_name: Some(chosen),
        iface,
        ip,
        not_open: None,
    }))
}
