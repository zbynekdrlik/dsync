//! dsync#129 slice 2 — the fallback source's UDP socket: one socket per node, joined to the
//! private group on the PTP interface, sending only to `wire::destination()`.
//!
//! Receive stamps: the kernel's software stamp on Linux (`SO_TIMESTAMPNS`, read through the PTP
//! path's own `net::recv_with_timestamp`); on Windows the precise wall right after the
//! non-blocking read (the loop polls every 50 µs). Both errors are one-sided late stamps, which
//! the follower's delay floor removes.

use super::wire;
use anyhow::{anyhow, Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::SystemTime;

/// One datagram as received.
#[derive(Debug, Clone)]
pub struct Received {
    pub bytes: Vec<u8>,
    pub rx: SystemTime,
    pub from: Option<Ipv4Addr>,
}

/// The fallback source's network path (a trait so the controller tests run without sockets).
pub trait FallbackTransport: Send {
    /// Multicast one datagram to the private group.
    fn send(&mut self, datagram: &[u8]) -> Result<()>;
    /// The next received datagram, `None` when none is waiting (non-blocking).
    fn recv(&mut self) -> Result<Option<Received>>;
}

pub struct UdpFallbackTransport {
    socket: UdpSocket,
    dest: SocketAddrV4,
}

impl UdpFallbackTransport {
    /// The production socket: `wire::GROUP`:`wire::PORT` on the interface with `iface_ip`, its own
    /// datagrams not looped back.
    pub fn open(iface_ip: Ipv4Addr) -> Result<Self> {
        Self::open_on(wire::GROUP, wire::PORT, iface_ip, false)
    }

    /// The never-send pin, part 2: no socket is ever opened on the PTP group or a PTP port.
    fn open_on(group: Ipv4Addr, port: u16, iface_ip: Ipv4Addr, looped: bool) -> Result<Self> {
        if group == Ipv4Addr::new(224, 0, 1, 129)
            || port == crate::ptp::PTP_EVENT_PORT
            || port == crate::ptp::PTP_GENERAL_PORT
        {
            return Err(anyhow!(
                "the fallback time source never uses the PTP group or ports ({group}:{port})"
            ));
        }
        if !group.is_multicast() {
            return Err(anyhow!("{group} is not a multicast group"));
        }
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket
            .bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
            .with_context(|| format!("bind 0.0.0.0:{port}"))?;
        socket
            .join_multicast_v4(&group, &iface_ip)
            .with_context(|| format!("join {group} on {iface_ip}"))?;
        // Only the joined interface (see `net::create_multicast_socket`).
        #[cfg(target_os = "linux")]
        socket.set_multicast_all_v4(false)?;
        socket.set_multicast_if_v4(&iface_ip)?;
        socket.set_multicast_loop_v4(looped)?;
        // The LAN segment only.
        socket.set_multicast_ttl_v4(1)?;
        socket.set_nonblocking(true)?;
        let socket: UdpSocket = socket.into();
        #[cfg(unix)]
        {
            use nix::sys::socket::{setsockopt, sockopt};
            if let Err(e) = setsockopt(&socket, sockopt::ReceiveTimestampns, &true) {
                log::warn!(
                    "[FALLBACK] kernel receive timestamps (SO_TIMESTAMPNS) unavailable: {e} -- \
                     user-space stamps"
                );
            }
        }
        Ok(UdpFallbackTransport {
            socket,
            dest: SocketAddrV4::new(group, port),
        })
    }
}

impl FallbackTransport for UdpFallbackTransport {
    fn send(&mut self, datagram: &[u8]) -> Result<()> {
        self.socket.send_to(datagram, self.dest)?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Received>> {
        let mut buf = [0u8; 256];
        Ok(
            crate::net::recv_with_timestamp(&self.socket, &mut buf)?.map(|(size, rx, from)| {
                Received {
                    bytes: buf[..size].to_vec(),
                    rx,
                    from,
                }
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_socket_is_opened_on_the_ptp_group_or_ports_129() {
        let lo = Ipv4Addr::LOCALHOST;
        for (group, port) in [
            (Ipv4Addr::new(224, 0, 1, 129), 31910),
            (wire::GROUP, 319),
            (wire::GROUP, 320),
            (Ipv4Addr::new(224, 0, 1, 129), 319),
        ] {
            let err = UdpFallbackTransport::open_on(group, port, lo, true)
                .err()
                .unwrap_or_else(|| panic!("{group}:{port} opened"));
            assert!(err.to_string().contains("never"), "{err}");
        }
        assert!(
            UdpFallbackTransport::open_on(Ipv4Addr::new(10, 0, 0, 1), 31999, lo, true).is_err()
        );
    }

    /// Real sockets over loopback multicast (Linux: `lo` carries multicast with the interface
    /// pinned to 127.0.0.1 and the loop on). A port of its own per test (they run in parallel).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_datagram_crosses_the_group_with_a_kernel_receive_stamp_129() {
        let group = Ipv4Addr::new(239, 255, 77, 9);
        let port = 31917;
        let mut rx = UdpFallbackTransport::open_on(group, port, Ipv4Addr::LOCALHOST, true)
            .expect("receiver");
        let mut tx =
            UdpFallbackTransport::open_on(group, port, Ipv4Addr::LOCALHOST, true).expect("sender");
        let sync = wire::FallbackSync {
            priority: 7,
            node: wire::node_id(Ipv4Addr::LOCALHOST),
            seq: 3,
            epoch: 9,
            t1_ns: 42,
        };
        let before = SystemTime::now();
        tx.send(&sync.encode()).expect("sent");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = loop {
            if let Some(r) = rx.recv().expect("recv") {
                break r;
            }
            assert!(std::time::Instant::now() < deadline, "nothing arrived");
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(wire::FallbackSync::decode(&got.bytes), Some(sync));
        assert_eq!(got.from, Some(Ipv4Addr::LOCALHOST));
        let after = SystemTime::now();
        assert!(got.rx >= before && got.rx <= after, "stamp {:?}", got.rx);
    }
}
