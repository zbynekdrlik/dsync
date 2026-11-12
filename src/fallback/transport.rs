//! dsync#129 slice 2 — the fallback source's UDP sockets: a receive socket joined to the private
//! group on the PTP interface, and a send socket that sends only to `wire::destination()`.
//!
//! Receive stamps come from a thread of its own blocked on the read: the kernel's software stamp
//! on Linux (`SO_TIMESTAMPNS`, read through the PTP path's own `net::recv_with_timestamp`); on
//! Windows the precise wall right after the blocked read wakes. Not from the sync loop: on a
//! network without PTP the Windows loop waits ~1 ms in the Npcap read every iteration, which
//! would make the stamp ms-late, uniformly (review of #129). Both remaining errors are one-sided
//! late stamps, which the follower's delay floor removes.

use super::wire;
use anyhow::{anyhow, Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

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

/// Queued datagrams beyond this are dropped (a flood never grows memory).
const QUEUE: usize = 1024;

/// The receive thread wakes this often to see whether the transport was dropped.
const RX_POLL: Duration = Duration::from_millis(200);

/// Read errors in a row before the receive thread backs off (a dead socket, not one bad datagram).
const ERRORS_BEFORE_BACKOFF: u32 = 16;

pub struct UdpFallbackTransport {
    tx: UdpSocket,
    dest: SocketAddrV4,
    rx: Receiver<std::result::Result<Received, String>>,
    stop: Arc<AtomicBool>,
    /// Detached on drop (see `Drop`).
    thread: Option<JoinHandle<()>>,
}

impl UdpFallbackTransport {
    /// The production sockets: `wire::GROUP`:`wire::PORT` on the interface with `iface_ip`, its
    /// own datagrams not looped back.
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
        let rx_socket = open_receiver(group, port, iface_ip)?;
        let tx = open_sender(iface_ip, looped)?;
        let (queue_tx, queue_rx) = sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("fallback-rx".to_string())
            .spawn(move || receive_loop(rx_socket, queue_tx, thread_stop))
            .context("spawn the fallback receive thread")?;
        Ok(UdpFallbackTransport {
            tx,
            dest: SocketAddrV4::new(group, port),
            rx: queue_rx,
            stop,
            thread: Some(thread),
        })
    }
}

/// The receive socket: joined to `group` on `iface_ip`, blocking with a short timeout (its
/// own thread stamps each datagram the moment the read returns). On Linux it is bound to the
/// GROUP address, so a unicast datagram to the port is never delivered; Windows cannot bind a
/// multicast address (`0.0.0.0`).
fn open_receiver(group: Ipv4Addr, port: u16, iface_ip: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    let bind_ip = group;
    #[cfg(not(target_os = "linux"))]
    let bind_ip = Ipv4Addr::UNSPECIFIED;
    socket
        .bind(&SocketAddrV4::new(bind_ip, port).into())
        .with_context(|| format!("bind {bind_ip}:{port}"))?;
    socket
        .join_multicast_v4(&group, &iface_ip)
        .with_context(|| format!("join {group} on {iface_ip}"))?;
    // Only the joined interface (see `net::create_multicast_socket`).
    #[cfg(target_os = "linux")]
    socket.set_multicast_all_v4(false)?;
    socket.set_read_timeout(Some(RX_POLL))?;
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
    Ok(socket)
}

/// The send socket: an ephemeral port on the interface, the LAN segment only (TTL 1).
fn open_sender(iface_ip: Ipv4Addr, looped: bool) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket
        .bind(&SocketAddrV4::new(iface_ip, 0).into())
        .with_context(|| format!("bind {iface_ip}:0"))?;
    socket.set_multicast_if_v4(&iface_ip)?;
    socket.set_multicast_loop_v4(looped)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// The receive thread. A blocked read wakes on the datagram's arrival (no loop period, no timer
/// quantum), so the stamp it takes right after is late only by the wake-up: the kernel's own
/// software stamp on Linux (`SO_TIMESTAMPNS`), the precise wall right after the wake on
/// Windows. Ends when the transport is dropped.
fn receive_loop(
    socket: UdpSocket,
    queue: SyncSender<std::result::Result<Received, String>>,
    stop: Arc<AtomicBool>,
) {
    // Far above a Sync (32 bytes): an oversize datagram fails the read on Windows (`WSAEMSGSIZE`).
    let mut buf = [0u8; 2048];
    let mut dropped: u64 = 0;
    let mut errors_in_a_row: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let item = match crate::net::recv_with_timestamp(&socket, &mut buf) {
            Ok(Some((size, rx, from))) => {
                errors_in_a_row = 0;
                Ok(Received {
                    bytes: buf[..size].to_vec(),
                    rx,
                    from,
                })
            }
            Ok(None) => continue,
            Err(e) if is_timeout(&e) => continue,
            Err(e) => {
                // One bad datagram (oversize, a reset) is dropped and the next read is at once:
                // a pause here would make the next Sync's stamp late. Only a socket that keeps
                // failing is backed off.
                errors_in_a_row = errors_in_a_row.saturating_add(1);
                if errors_in_a_row >= ERRORS_BEFORE_BACKOFF {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(format!("{e:#}"))
            }
        };
        match queue.try_send(item) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                dropped += 1;
                if dropped.is_power_of_two() {
                    log::warn!(
                        "[FALLBACK] receive queue full: {dropped} datagram(s) dropped (a flood on \
                         {}:{})",
                        wire::GROUP,
                        wire::PORT
                    );
                }
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
}

/// A read that timed out (no datagram within `RX_POLL`): `EAGAIN` is already `Ok(None)` on
/// unix; Windows reports `WSAETIMEDOUT` as `TimedOut`.
fn is_timeout(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        )
    })
}

impl Drop for UdpFallbackTransport {
    /// The receive thread ends within `RX_POLL`; it is not waited for (a re-join replaces the
    /// transport from the sync loop, which must not stall).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.thread.take());
    }
}

impl FallbackTransport for UdpFallbackTransport {
    fn send(&mut self, datagram: &[u8]) -> Result<()> {
        self.tx.send_to(datagram, self.dest)?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Received>> {
        match self.rx.try_recv() {
            Ok(Ok(r)) => Ok(Some(r)),
            Ok(Err(e)) => Err(anyhow!("receive failed: {e}")),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(anyhow!("the fallback receive thread ended")),
        }
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
