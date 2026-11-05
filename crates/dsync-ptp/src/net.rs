use anyhow::{anyhow, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};

#[cfg(unix)]
use nix::sys::socket::{setsockopt, sockopt};

pub fn get_default_interface() -> Result<(String, Ipv4Addr)> {
    let ifaces = if_addrs::get_if_addrs()?;

    let mut best_iface = None;

    for iface in &ifaces {
        // Skip loopback and non-IPv4
        let ip = match iface.addr.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() => ip,
            _ => continue,
        };

        // Skip wireless interfaces if possible
        let name_lower = iface.name.to_lowercase();
        let is_wireless = name_lower.contains("wireless")
            || name_lower.contains("wi-fi")
            || name_lower.contains("wlan");

        // Verify we can actually bind to this IP
        if is_ip_bindable(ip) {
            if !is_wireless {
                return Ok((iface.name.clone(), ip));
            } else if best_iface.is_none() {
                best_iface = Some((iface.name.clone(), ip));
            }
        }
    }

    if let Some(res) = best_iface {
        return Ok(res);
    }

    // Diagnostics
    log::warn!("No suitable IPv4 interface found. Diagnostics:");
    for iface in &ifaces {
        log::warn!(" - Name: {}, Addr: {:?}", iface.name, iface.addr);
    }

    Err(anyhow!("No suitable IPv4 interface found"))
}

/// dsync#131 — what the pinned PTP interface (`system.interface` / `--interface`) is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinnedLookup {
    /// The NIC exists and carries this routable IPv4 (the first one, in listing order).
    Ready(Ipv4Addr),
    /// The NIC carries only a link-local IPv4 (169.254/16): an audio-network segment without DHCP uses
    /// one for good, but it is also what Windows (APIPA) or avahi assign while DHCP has not
    /// answered yet. Startup takes it only after [`PINNED_STARTUP_GRACE`]; a re-join takes it.
    LinkLocalOnly(Ipv4Addr),
    /// The NIC exists but has no usable IPv4 yet (DHCP pending, cable out, an APIPA address).
    NoIpv4,
    /// No NIC of that name.
    Missing,
}

/// One NIC as the OS lists it: every name it answers to (Linux: its name; Windows: the friendly
/// name and the adapter GUID) and its IPv4 addresses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NicSnapshot {
    pub names: Vec<String>,
    pub ipv4: Vec<Ipv4Addr>,
}

/// dsync#131 — classify the pinned NIC `name` among `nics` (pure). A routable IPv4 wins over
/// a link-local one; loopback and 0.0.0.0 never count. Names compare exactly on Linux; on
/// Windows ([`classify_pinned`]) case-insensitively, and a GUID matches with or without its
/// braces or the Npcap `\Device\NPF_` prefix.
pub fn classify_pinned_with(name: &str, nics: &[NicSnapshot], windows_names: bool) -> PinnedLookup {
    let matches = |candidate: &str| {
        if windows_names {
            normalize_windows_name(candidate).eq_ignore_ascii_case(normalize_windows_name(name))
        } else {
            candidate == name
        }
    };
    let mut found = false;
    let mut link_local = None;
    for nic in nics.iter().filter(|n| n.names.iter().any(|n| matches(n))) {
        found = true;
        for ip in nic
            .ipv4
            .iter()
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        {
            if !ip.is_link_local() {
                return PinnedLookup::Ready(*ip);
            }
            link_local.get_or_insert(*ip);
        }
    }
    match (found, link_local) {
        (_, Some(ip)) => PinnedLookup::LinkLocalOnly(ip),
        (true, None) => PinnedLookup::NoIpv4,
        (false, None) => PinnedLookup::Missing,
    }
}

/// A Windows adapter name for comparison: without an Npcap `\Device\NPF_` prefix and without
/// the braces of a GUID.
fn normalize_windows_name(name: &str) -> &str {
    let name = name.trim();
    const NPF: &str = r"\Device\NPF_";
    let name = match name.get(..NPF.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(NPF) => &name[NPF.len()..],
        _ => name,
    };
    name.trim_start_matches('{').trim_end_matches('}')
}

/// dsync#131 — [`classify_pinned_with`] by this platform's naming rules.
pub fn classify_pinned(name: &str, nics: &[NicSnapshot]) -> PinnedLookup {
    classify_pinned_with(name, nics, cfg!(windows))
}

/// dsync#131 — the IPv4 the startup may join the pinned NIC on after waiting `waited`: a
/// routable one at once, a link-local one only after [`PINNED_STARTUP_GRACE`] (a DHCP answer
/// may still replace it), else none.
pub fn usable_pinned_ip(lookup: PinnedLookup, waited: std::time::Duration) -> Option<Ipv4Addr> {
    match lookup {
        PinnedLookup::Ready(ip) => Some(ip),
        PinnedLookup::LinkLocalOnly(ip) if waited > PINNED_STARTUP_GRACE => Some(ip),
        _ => None,
    }
}

/// The NICs of this host now, for [`classify_pinned`] (Linux: `getifaddrs` plus `name` itself
/// when the kernel knows it but it carries no address at all).
#[cfg(not(windows))]
fn nic_snapshots(name: &str) -> Vec<NicSnapshot> {
    let mut nics: Vec<NicSnapshot> = Vec::new();
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                let pos = match nics.iter().position(|n| n.names[0] == iface.name) {
                    Some(pos) => pos,
                    None => {
                        nics.push(NicSnapshot {
                            names: vec![iface.name.clone()],
                            ipv4: Vec::new(),
                        });
                        nics.len() - 1
                    }
                };
                if let IpAddr::V4(ip) = iface.ip() {
                    nics[pos].ipv4.push(ip);
                }
            }
        }
        Err(e) => log::warn!("[NET] cannot list the interfaces: {}", e),
    }
    if !nics.iter().any(|n| n.names[0] == name) && nix::net::if_::if_nametoindex(name).is_ok() {
        nics.push(NicSnapshot {
            names: vec![name.to_string()],
            ipv4: Vec::new(),
        });
    }
    nics
}

/// The NICs of this host now, for [`classify_pinned`]: every adapter `GetAdaptersAddresses`
/// lists (also one with no address), named by its friendly name and its GUID.
#[cfg(windows)]
fn nic_snapshots(_name: &str) -> Vec<NicSnapshot> {
    match windows_adapters() {
        Ok(nics) => nics,
        Err(e) => {
            log::warn!("[NET] cannot list the network adapters: {}", e);
            Vec::new()
        }
    }
}

#[cfg(windows)]
fn windows_adapters() -> Result<Vec<NicSnapshot>> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    // u64 words keep the buffer aligned for IP_ADAPTER_ADDRESSES_LH.
    let mut buf: Vec<u64> = Vec::new();
    let mut rc = ERROR_BUFFER_OVERFLOW;
    for _ in 0..4 {
        buf = vec![0u64; size as usize / 8 + 1];
        rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if rc != ERROR_BUFFER_OVERFLOW {
            break;
        }
    }
    if rc != 0 {
        return Err(anyhow!("GetAdaptersAddresses failed: error {}", rc));
    }
    let mut nics = Vec::new();
    let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !cur.is_null() {
        // SAFETY: a list GetAdaptersAddresses filled into `buf`, which outlives this walk.
        let a = unsafe { &*cur };
        let mut names = Vec::new();
        if let Ok(friendly) = unsafe { a.FriendlyName.to_string() } {
            names.push(friendly);
        }
        if let Ok(guid) = unsafe { a.AdapterName.to_string() } {
            names.push(guid);
        }
        let mut ipv4 = Vec::new();
        let mut ua = a.FirstUnicastAddress;
        while !ua.is_null() {
            let u = unsafe { &*ua };
            let sa = u.Address.lpSockaddr;
            if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                let raw = unsafe { sin.sin_addr.S_un.S_addr };
                ipv4.push(Ipv4Addr::from(u32::from_be(raw)));
            }
            ua = u.Next;
        }
        nics.push(NicSnapshot { names, ipv4 });
        cur = a.Next;
    }
    Ok(nics)
}

/// dsync#131 — how long a pinned NIC may sit without an IPv4 at startup before the clock
/// alarm names it: the PTP staleness bound (`controller::PTP_TIMEOUT_SECS`), so a normal boot
/// (DHCP / NetworkManager bringing the NIC up) never alarms, a dead one does like any lost clock.
pub const PINNED_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// dsync#131 — the alarm-worthy problem of a pinned NIC while startup waits for it (pure): a
/// MISSING NIC at once, one without an IPv4 only after [`PINNED_STARTUP_GRACE`]; `None` when it
/// is ready or still within the grace.
pub fn startup_wait_problem(
    name: &str,
    lookup: PinnedLookup,
    waited: std::time::Duration,
) -> Option<PinnedInterfaceUnavailable> {
    match lookup {
        PinnedLookup::NoIpv4 if waited <= PINNED_STARTUP_GRACE => None,
        // Taken after the grace (`usable_pinned_ip`); waiting for DHCP until then is no fault.
        PinnedLookup::LinkLocalOnly(_) => None,
        other => PinnedInterfaceUnavailable::from_lookup(name, other),
    }
}

/// dsync#131 — the pinned PTP interface `name` now.
pub fn pinned_interface(name: &str) -> PinnedLookup {
    classify_pinned(name, &nic_snapshots(name))
}

/// dsync#131 — the pinned PTP interface cannot be joined now. Returned (inside an `anyhow`
/// error) by every backend's `PtpNetwork::rejoin` BEFORE it closes anything; the controller
/// downcasts it into the clock alarm's reason and retries quickly while it lasts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedInterfaceUnavailable {
    pub name: String,
    /// `true` = no NIC of that name; `false` = it exists without a usable IPv4.
    pub missing: bool,
}

impl PinnedInterfaceUnavailable {
    /// The pin `name` is not usable per `lookup`; `None` when it carries an IPv4 (a re-join
    /// takes a link-local one too: the NIC is up and that is its address now).
    pub fn from_lookup(name: &str, lookup: PinnedLookup) -> Option<Self> {
        match lookup {
            PinnedLookup::Ready(_) | PinnedLookup::LinkLocalOnly(_) => None,
            PinnedLookup::NoIpv4 => Some(Self {
                name: name.to_string(),
                missing: false,
            }),
            PinnedLookup::Missing => Some(Self {
                name: name.to_string(),
                missing: true,
            }),
        }
    }

    /// The clock alarm's reason, naming the NIC.
    pub fn alarm_reason(&self) -> String {
        if self.missing {
            format!("PTP interface {} is missing", self.name)
        } else {
            format!("PTP interface {} has no IPv4 address", self.name)
        }
    }
}

impl std::fmt::Display for PinnedInterfaceUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pinned {} -- waiting for it, never another NIC",
            self.alarm_reason()
        )
    }
}

impl std::error::Error for PinnedInterfaceUnavailable {}

/// dsync#112 — the name of the interface that carries exactly `ip` now (never loopback), if
/// any. The PTP re-join looks for the NIC it last received PTP on this way: a NIC that is re-plugged
/// (USB) comes back with a new, higher ifindex, often under a new name, so the listing-ordered
/// [`get_default_interface`] may put another interface (tailscale, docker, a bridge) first.
pub fn interface_with_ip(ip: Ipv4Addr) -> Option<String> {
    if ip.is_loopback() {
        return None;
    }
    let ifaces = if_addrs::get_if_addrs().ok()?;
    ifaces
        .into_iter()
        .find(|iface| iface.ip() == IpAddr::V4(ip))
        .map(|iface| iface.name)
}

/// dsync#112 — the Windows PTP capture choice, pure so Linux CI tests it. `by_rule` is
/// `net_pcap::find_ptp_capture_device`'s answer: `Ok((device, Some(ip)))` when the trusted
/// grandmaster subnet of `gm_allowlist` picks one device (camera-box issue 1073; it always wins),
/// `Ok((device, None))` for the name fallback. `home` is the device that carries the address PTP
/// was last received on, when a re-join looks for one (startup passes `None`): after a NIC swap
/// the adapter comes back as another device under another name, so it comes before the name
/// fallback. Returns the device and the address to join on (`None` = the device's first IPv4).
pub fn choose_capture_device<D>(
    by_rule: Result<(D, Option<Ipv4Addr>)>,
    home: Option<(D, Ipv4Addr)>,
) -> Result<(D, Option<Ipv4Addr>)> {
    match by_rule {
        Ok((device, Some(ip))) => Ok((device, Some(ip))),
        by_name => match home {
            Some((device, ip)) => Ok((device, Some(ip))),
            None => by_name,
        },
    }
}

/// dsync#112 — whether [`choose_capture_device`] needs the home device looked up: only on a
/// re-join (`home` is `Some`) whose trusted-subnet rule found no unique device (it would win
/// anyway). Returns the address to look the device up by.
pub fn home_to_look_up<D>(
    by_rule: &Result<(D, Option<Ipv4Addr>)>,
    home: Option<Ipv4Addr>,
) -> Option<Ipv4Addr> {
    match by_rule {
        Ok((_, Some(_))) => None,
        _ => home,
    }
}

fn is_ip_bindable(ip: Ipv4Addr) -> bool {
    let socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let addr = SocketAddrV4::new(ip, 0); // Port 0 (ephemeral)
    socket.bind(&addr.into()).is_ok()
}

pub fn create_multicast_socket(port: u16, interface_ip: Ipv4Addr) -> Result<UdpSocket> {
    // Standard UDP socket creation for TX (Transmission) or legacy RX
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

    let multi_addr: Ipv4Addr = "224.0.1.129".parse()?;
    socket.join_multicast_v4(&multi_addr, &interface_ip)?;

    socket.set_multicast_loop_v4(false)?;
    socket.set_nonblocking(true)?;

    let udp_socket: UdpSocket = socket.into();

    #[cfg(unix)]
    {
        match setsockopt(&udp_socket, sockopt::ReceiveTimestampns, &true) {
            Ok(_) => log::info!("Kernel timestamping (SO_TIMESTAMPNS) enabled."),
            Err(e) => log::warn!("Failed to enable kernel timestamping: {}", e),
        }
    }

    Ok(udp_socket)
}

/// The bind address for the pcap IGMP-join socket (dsync#109).
///
/// The Windows pcap path opens a UDP socket purely to trigger the kernel IGMP
/// membership report for the PTP group `224.0.1.129`. IGMP membership is per
/// interface+group, NOT per port, so the socket's bound source port is
/// irrelevant to the join (and pcap's own BPF filter, not this socket, selects
/// the captured traffic). It must therefore bind an EPHEMERAL port — never a
/// PTP port (319/320), which on a virtual soundcard host belong to DVS's
/// own `ptp.exe`: dsync (a boot-time service) bound them first, so
/// `ptp.exe` (started later, without `SO_REUSEADDR`) failed its bind with
/// WSAEADDRINUSE and its PTP follower was starved of Follow_Up/Delay_Resp on
/// 320.
pub fn igmp_join_bind_addr() -> SocketAddrV4 {
    // dsync#109: bind an EPHEMERAL port (0). The kernel picks a free source
    // port for the join socket; 319/320 stay free for a DVS ptp.exe on the same
    // host. The port is irrelevant to the IGMP membership and to pcap's capture.
    SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)
}

#[cfg(unix)]
pub fn recv_with_timestamp(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> Result<Option<(usize, std::time::SystemTime, Option<Ipv4Addr>)>> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, SockaddrStorage};
    use nix::sys::time::TimeSpec;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime};

    let fd = sock.as_raw_fd();
    let mut iov = [std::io::IoSliceMut::new(buf)];
    let mut cmsg_buf = nix::cmsg_space!(TimeSpec);

    match recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
        Ok(msg) => {
            let timestamp = msg
                .cmsgs()
                .find_map(|cmsg| {
                    if let ControlMessageOwned::ScmTimestampns(ts) = cmsg {
                        let duration = Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32);
                        Some(SystemTime::UNIX_EPOCH + duration)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(SystemTime::now);

            // Extract source IP from the address field
            let source_ip = msg.address.and_then(|addr| {
                addr.as_sockaddr_in().map(|sin| {
                    let ip = sin.ip();
                    Ipv4Addr::new(
                        ((ip >> 24) & 0xFF) as u8,
                        ((ip >> 16) & 0xFF) as u8,
                        ((ip >> 8) & 0xFF) as u8,
                        (ip & 0xFF) as u8,
                    )
                })
            });

            Ok(Some((msg.bytes, timestamp, source_ip)))
        }
        Err(nix::errno::Errno::EAGAIN) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
pub fn recv_with_timestamp(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> Result<Option<(usize, std::time::SystemTime, Option<Ipv4Addr>)>> {
    match sock.recv_from(buf) {
        Ok((size, addr)) => {
            let source_ip = match addr {
                std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
                _ => None,
            };
            Ok(Some((size, std::time::SystemTime::now(), source_ip)))
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(names: &[&str], ipv4: &[[u8; 4]]) -> NicSnapshot {
        NicSnapshot {
            names: names.iter().map(|n| n.to_string()).collect(),
            ipv4: ipv4.iter().map(|o| Ipv4Addr::from(*o)).collect(),
        }
    }

    #[test]
    fn a_pinned_interface_is_found_by_name_never_by_listing_order_131() {
        // stream-snv on 1.11.2026: the USB NIC got its /32 first and is listed first.
        let nics = [
            nic(&["enx00e04c680185"], &[[10, 77, 7, 204]]),
            nic(&["enp56s0"], &[[10, 77, 9, 204]]),
        ];
        assert_eq!(
            classify_pinned("enp56s0", &nics),
            PinnedLookup::Ready(Ipv4Addr::new(10, 77, 9, 204))
        );
        assert_eq!(classify_pinned("eth9", &nics), PinnedLookup::Missing);
        assert_eq!(
            classify_pinned("enp56", &nics),
            PinnedLookup::Missing,
            "exact name"
        );
    }

    #[test]
    fn a_pinned_interface_without_a_usable_ipv4_is_waited_for_131() {
        // Up but no DHCP yet, or a Windows APIPA address: not usable, never another NIC.
        let nics = [
            nic(&["eth0"], &[[10, 77, 9, 1]]),
            nic(&["Ethernet", "{4A1B-GUID}"], &[[169, 254, 3, 7]]),
            nic(&["enp56s0"], &[]),
        ];
        assert_eq!(classify_pinned("enp56s0", &nics), PinnedLookup::NoIpv4);
        assert_eq!(
            classify_pinned("Ethernet", &nics),
            PinnedLookup::LinkLocalOnly(Ipv4Addr::new(169, 254, 3, 7))
        );
        // A Windows adapter answers to its friendly name and to its GUID.
        let up = [nic(
            &["Ethernet", "{4A1B-GUID}"],
            &[[169, 254, 3, 7], [10, 77, 9, 204]],
        )];
        assert_eq!(
            classify_pinned("{4A1B-GUID}", &up),
            PinnedLookup::Ready(Ipv4Addr::new(10, 77, 9, 204))
        );
    }

    #[test]
    fn a_link_local_only_pinned_nic_is_taken_after_the_grace_131() {
        // An audio-network segment without DHCP lives on 169.254/16 for good; at boot the same address
        // can be a placeholder until DHCP answers.
        let ll = Ipv4Addr::new(169, 254, 9, 9);
        let s = std::time::Duration::from_secs;
        assert_eq!(
            usable_pinned_ip(PinnedLookup::LinkLocalOnly(ll), s(10)),
            None
        );
        assert_eq!(
            usable_pinned_ip(PinnedLookup::LinkLocalOnly(ll), s(11)),
            Some(ll)
        );
        assert_eq!(
            startup_wait_problem("eth0", PinnedLookup::LinkLocalOnly(ll), s(99)),
            None
        );
        let rig = Ipv4Addr::new(10, 77, 9, 204);
        assert_eq!(usable_pinned_ip(PinnedLookup::Ready(rig), s(0)), Some(rig));
        assert_eq!(usable_pinned_ip(PinnedLookup::NoIpv4, s(99)), None);
        assert_eq!(
            PinnedInterfaceUnavailable::from_lookup("eth0", PinnedLookup::LinkLocalOnly(ll)),
            None,
            "a re-join takes the NIC's link-local address"
        );
    }

    #[test]
    fn windows_names_match_case_insensitively_and_a_guid_in_any_spelling_131() {
        let nics = [nic(
            &["Ethernet 2", "{4A1B2C3D-0000-1111-2222-333344445555}"],
            &[[10, 77, 9, 204]],
        )];
        let rig = PinnedLookup::Ready(Ipv4Addr::new(10, 77, 9, 204));
        for pin in [
            "ethernet 2",
            "ETHERNET 2",
            "4a1b2c3d-0000-1111-2222-333344445555",
            "{4A1B2C3D-0000-1111-2222-333344445555}",
            r"\Device\NPF_{4A1B2C3D-0000-1111-2222-333344445555}",
            r"\device\npf_{4a1b2c3d-0000-1111-2222-333344445555}",
        ] {
            assert_eq!(classify_pinned_with(pin, &nics, true), rig, "{pin}");
        }
        assert_eq!(
            classify_pinned_with("Ethernet", &nics, true),
            PinnedLookup::Missing,
            "no prefix match"
        );
        // Linux names are case-sensitive and exact.
        let linux = [nic(&["enp56s0"], &[[10, 77, 9, 204]])];
        assert_eq!(
            classify_pinned_with("ENP56S0", &linux, false),
            PinnedLookup::Missing
        );
    }

    #[test]
    fn startup_alarms_at_once_for_a_missing_nic_and_after_the_grace_for_no_ipv4_131() {
        let s = std::time::Duration::from_secs;
        assert_eq!(
            startup_wait_problem("enp56s0", PinnedLookup::Missing, s(0)),
            Some(PinnedInterfaceUnavailable {
                name: "enp56s0".to_string(),
                missing: true
            })
        );
        assert_eq!(
            startup_wait_problem("enp56s0", PinnedLookup::NoIpv4, s(10)),
            None
        );
        assert_eq!(
            startup_wait_problem("enp56s0", PinnedLookup::NoIpv4, s(11)).map(|p| p.alarm_reason()),
            Some("PTP interface enp56s0 has no IPv4 address".to_string())
        );
        assert_eq!(
            startup_wait_problem("enp56s0", PinnedLookup::Ready(Ipv4Addr::LOCALHOST), s(99)),
            None
        );
    }

    #[test]
    fn the_pinned_error_names_the_nic_and_survives_anyhow_131() {
        let e: anyhow::Error = PinnedInterfaceUnavailable {
            name: "enp56s0".to_string(),
            missing: true,
        }
        .into();
        assert_eq!(
            e.to_string(),
            "pinned PTP interface enp56s0 is missing -- waiting for it, never another NIC"
        );
        assert!(e.downcast_ref::<PinnedInterfaceUnavailable>().is_some());
    }

    #[test]
    fn the_os_lookup_of_a_name_no_host_has_is_missing_131() {
        assert_eq!(
            pinned_interface("dsync-no-such-nic0"),
            PinnedLookup::Missing
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            pinned_interface("lo"),
            PinnedLookup::NoIpv4,
            "loopback only"
        );
    }

    /// Test that get_default_interface filters out loopback addresses
    #[test]
    fn test_get_default_interface_returns_non_loopback() {
        // This test verifies the interface selection logic runs without panic
        // On systems with valid network interfaces, it should succeed
        // On systems without interfaces, it returns an error (which is valid)
        let result = get_default_interface();
        if let Ok((name, ip)) = result {
            assert!(!name.is_empty(), "Interface name should not be empty");
            assert!(!ip.is_loopback(), "Should not return loopback address");
        }
        // Error case is acceptable on minimal test environments
    }

    /// dsync#112: the interface carrying an address is found by that address.
    #[test]
    fn interface_with_ip_finds_the_interface_by_its_address_112() {
        if let Ok((name, ip)) = get_default_interface() {
            assert_eq!(interface_with_ip(ip), Some(name));
        }
        assert_eq!(
            interface_with_ip(Ipv4Addr::new(192, 0, 2, 1)),
            None,
            "TEST-NET-1 is on no interface"
        );
        assert_eq!(
            interface_with_ip(Ipv4Addr::LOCALHOST),
            None,
            "never loopback"
        );
    }

    const HOME: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 204);

    #[test]
    fn the_trusted_subnet_rule_wins_over_the_home_device_112() {
        let rule = Ok(("rig-nic", Some(Ipv4Addr::new(10, 77, 9, 205))));
        let got = choose_capture_device(rule, Some(("old-home-nic", HOME))).expect("a device");
        assert_eq!(got, ("rig-nic", Some(Ipv4Addr::new(10, 77, 9, 205))));
    }

    #[test]
    fn the_home_device_wins_over_the_name_fallback_112() {
        let by_name = Ok(("default-nic", None));
        let got = choose_capture_device(by_name, Some(("swapped-nic", HOME))).expect("a device");
        assert_eq!(got, ("swapped-nic", Some(HOME)));
    }

    #[test]
    fn the_home_device_is_used_when_the_name_fallback_found_nothing_112() {
        let by_name: Result<(&str, Option<Ipv4Addr>)> =
            Err(anyhow!("Interface 'Ethernet' not found"));
        let got = choose_capture_device(by_name, Some(("swapped-nic", HOME))).expect("a device");
        assert_eq!(got, ("swapped-nic", Some(HOME)));
    }

    #[test]
    fn the_home_device_is_looked_up_only_on_a_re_join_without_a_trusted_subnet_match_112() {
        let trusted: Result<(&str, Option<Ipv4Addr>)> =
            Ok(("rig-nic", Some(Ipv4Addr::new(10, 77, 9, 205))));
        let by_name: Result<(&str, Option<Ipv4Addr>)> = Ok(("default-nic", None));
        let none: Result<(&str, Option<Ipv4Addr>)> = Err(anyhow!("not found"));
        assert_eq!(
            home_to_look_up(&trusted, Some(HOME)),
            None,
            "the rule wins anyway"
        );
        assert_eq!(home_to_look_up(&by_name, Some(HOME)), Some(HOME));
        assert_eq!(home_to_look_up(&none, Some(HOME)), Some(HOME));
        assert_eq!(
            home_to_look_up(&by_name, None),
            None,
            "the startup has no home"
        );
        assert_eq!(home_to_look_up(&none, None), None);
    }

    #[test]
    fn without_a_home_device_the_startup_choice_stands_112() {
        let by_name = Ok(("default-nic", None));
        assert_eq!(
            choose_capture_device(by_name, None).expect("a device"),
            ("default-nic", None)
        );
        let none: Result<(&str, Option<Ipv4Addr>)> = Err(anyhow!("Interface 'Ethernet' not found"));
        assert!(
            choose_capture_device(none, None).is_err(),
            "nothing to join: the caller keeps its old capture"
        );
    }

    /// Test is_ip_bindable with loopback (should always work)
    #[test]
    fn test_is_ip_bindable_loopback() {
        // Loopback should always be bindable on any system
        let loopback = Ipv4Addr::new(127, 0, 0, 1);
        assert!(is_ip_bindable(loopback), "Loopback should be bindable");
    }

    /// Test is_ip_bindable with UNSPECIFIED address
    #[test]
    fn test_is_ip_bindable_unspecified() {
        // 0.0.0.0 should be bindable (binds to all interfaces)
        let unspecified = Ipv4Addr::UNSPECIFIED;
        assert!(
            is_ip_bindable(unspecified),
            "UNSPECIFIED (0.0.0.0) should be bindable"
        );
    }

    /// Test PTP multicast address constant
    #[test]
    fn test_ptp_multicast_address() {
        let multi_addr: Ipv4Addr = "224.0.1.129".parse().unwrap();
        assert!(multi_addr.is_multicast(), "PTP address should be multicast");
        assert_eq!(multi_addr.octets(), [224, 0, 1, 129]);
    }

    /// dsync#109: the pcap IGMP-join socket must bind an EPHEMERAL port,
    /// never a PTP port. Binding 319/320 collides with a virtual
    /// soundcard `ptp.exe` on the same host and starves its PTP follower of the
    /// PTP general messages (Follow_Up/Delay_Resp) on 320 — the DVS media clock
    /// then free-runs on the host crystal instead of the grandmaster.
    #[test]
    fn igmp_join_bind_addr_never_uses_a_ptp_port() {
        let addr = igmp_join_bind_addr();
        assert_eq!(
            addr.port(),
            0,
            "IGMP-join socket must bind an ephemeral port (0), not a PTP port"
        );
        assert_ne!(
            addr.port(),
            319,
            "must not bind PTP event port 319 (DVS ptp.exe needs it)"
        );
        assert_ne!(
            addr.port(),
            320,
            "must not bind PTP general port 320 (DVS ptp.exe needs it)"
        );
        assert_eq!(
            *addr.ip(),
            Ipv4Addr::UNSPECIFIED,
            "IGMP-join socket binds the unspecified address"
        );
    }

    /// dsync#109: a real UDP socket bound the way the IGMP join binds lands
    /// on a concrete NON-zero ephemeral port and never on 319/320 — so it can
    /// never collide with an exclusive PTP-port listener (e.g. DVS `ptp.exe`).
    #[test]
    fn igmp_join_socket_binds_a_nonzero_ephemeral_port() {
        let sock =
            UdpSocket::bind(igmp_join_bind_addr()).expect("bind the ephemeral IGMP-join socket");
        let port = sock
            .local_addr()
            .expect("read local_addr of the join socket")
            .port();
        assert_ne!(port, 0, "kernel must assign a concrete ephemeral port");
        assert_ne!(port, 319, "the join socket must never land on PTP port 319");
        assert_ne!(port, 320, "the join socket must never land on PTP port 320");
    }

    /// Test recv_with_timestamp returns None for non-blocking socket with no data
    #[test]
    fn test_recv_with_timestamp_no_data() {
        // Create a simple non-blocking UDP socket
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
            .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .unwrap();

        let udp_socket: UdpSocket = socket.into();
        let mut buf = [0u8; 512];

        let result = recv_with_timestamp(&udp_socket, &mut buf);
        assert!(result.is_ok());
        // Should return None since no data is available
        assert!(result.unwrap().is_none());
    }

    /// Test wireless interface detection keywords
    #[test]
    fn test_wireless_interface_detection() {
        // Test the wireless detection logic used in get_default_interface
        let wireless_names = ["Wireless LAN", "Wi-Fi", "wlan0", "WIRELESS"];
        let wired_names = ["eth0", "Ethernet", "enp3s0", "Local Area Connection"];

        for name in &wireless_names {
            let lower = name.to_lowercase();
            let is_wireless =
                lower.contains("wireless") || lower.contains("wi-fi") || lower.contains("wlan");
            assert!(is_wireless, "{} should be detected as wireless", name);
        }

        for name in &wired_names {
            let lower = name.to_lowercase();
            let is_wireless =
                lower.contains("wireless") || lower.contains("wi-fi") || lower.contains("wlan");
            assert!(!is_wireless, "{} should NOT be detected as wireless", name);
        }
    }

    /// dsync#136 (strih-snv, 5.11.2026): every PTP receiver binds `INADDR_ANY` and joins the
    /// group on ONE interface, but Linux's default `IP_MULTICAST_ALL = 1` also delivers the group
    /// from every OTHER interface some socket on the host joined. On strih the service joined
    /// `enp7s0` and the followers `enp7s0.70`, so each heard both ports of a dual-port
    /// grandmaster. A receiver must hear only the interface it joined (the #131 pin, strict).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_receiver_hears_the_group_only_on_the_interface_it_joined_136() {
        let (_, nic_ip) = get_default_interface().expect("a non-loopback IPv4 interface");
        let port = UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.local_addr())
            .expect("a free port")
            .port();
        let on_lo = create_multicast_socket(port, Ipv4Addr::LOCALHOST).expect("joined on lo");
        let on_nic = create_multicast_socket(port, nic_ip).expect("joined on the NIC");
        let group = Ipv4Addr::new(224, 0, 1, 129);
        let send_out_of = |ip: Ipv4Addr, payload: &[u8]| {
            let s = UdpSocket::bind(SocketAddrV4::new(ip, 0)).expect("sender");
            s.set_multicast_loop_v4(true).expect("loop");
            s.set_multicast_ttl_v4(0)
                .expect("ttl 0: never leaves the host");
            socket2::SockRef::from(&s)
                .set_multicast_if_v4(&ip)
                .expect("multicast if");
            s.send_to(payload, SocketAddrV4::new(group, port))
                .expect("send");
        };
        send_out_of(nic_ip, b"nic");
        send_out_of(Ipv4Addr::LOCALHOST, b"lo");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let heard = |s: &UdpSocket| {
            let mut got = Vec::new();
            let mut buf = [0u8; 16];
            while let Ok(n) = s.recv(&mut buf) {
                got.push(String::from_utf8_lossy(&buf[..n]).into_owned());
            }
            got
        };
        assert_eq!(heard(&on_lo), ["lo"], "the receiver joined on lo");
        assert_eq!(heard(&on_nic), ["nic"], "the receiver joined on the NIC");
    }
}
