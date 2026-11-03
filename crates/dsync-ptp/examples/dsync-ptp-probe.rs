//! dsync#136 — run a dsync-ptp follower beside the dsync service and compare the two clocks.
//!
//! ```text
//! dsync-ptp-probe --interface <name|IPv4> [--seconds 120] [--service 127.0.0.1:31900]
//! ```
//!
//! Every second it asks the local service for a DSYX reply (UDP 31900) and prints one line:
//!
//! - whether the follower is locked;
//! - the follower's PTP time minus the service's, in µs;
//! - the service's lock byte [41].
//!
//! The service's PTP time is the reply's own `now_ptp_ns` (wall − D in effect,
//! `[96-103]`). The follower is read at the reply's raw-counter instant (`[16-23]` /
//! `[48-55]`): the same QPC / `CLOCK_MONOTONIC_RAW` the follower runs on. The reply's wall and
//! counter are read a few µs apart, so the difference carries that read skew.
//!
//! At the end it prints a summary: time to lock, and the max and p99 of |diff| over the seconds
//! both clocks were locked. Exit codes:
//!
//! - 0: the p99 is within the lock bound (50 µs);
//! - 1: the follower never locked, there was no locked comparison, or the p99 is over the bound;
//! - 2: a bad argument, or the follower did not start (the `FollowerError` kind is printed).

use dsync_ptp::follower::{Follower, FollowerConfig, FollowerError};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const LOCK_BOUND_US: f64 = dsync_ptp::lock_gate::LOCK_BOUND_NS as f64 / 1000.0;

#[derive(Debug, PartialEq)]
struct Args {
    interface: String,
    seconds: u64,
    service: SocketAddr,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut interface = None;
    let mut seconds = 120;
    let mut service: SocketAddr = "127.0.0.1:31900".parse().expect("valid default");
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--interface" => interface = Some(value()?.clone()),
            "--seconds" => {
                seconds = value()?
                    .parse()
                    .ok()
                    .filter(|&s: &u64| s > 0)
                    .ok_or("--seconds needs a positive whole number")?
            }
            "--service" => {
                service = value()?
                    .parse()
                    .map_err(|_| "--service needs host:port, e.g. 127.0.0.1:31900")?
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        interface: interface.ok_or("--interface <name|IPv4> is required")?,
        seconds,
        service,
    })
}

/// The interface to pin: a name as given, an IPv4 resolved to the interface carrying it.
fn resolve_interface(given: &str) -> Result<String, String> {
    match given.parse::<Ipv4Addr>() {
        Ok(ip) => dsync_ptp::net::interface_with_ip(ip)
            .ok_or_else(|| format!("no interface carries {ip}")),
        Err(_) => Ok(given.to_string()),
    }
}

/// What one DSYX reply says.
#[derive(Debug, PartialEq)]
struct ServiceReading {
    /// The reply's raw counter, in ns.
    mono_ns: i64,
    /// The service's PTP time; `None` without a date state (no extension).
    ptp_ns: Option<i64>,
    /// Byte [41].
    locked: bool,
}

fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().expect("8 bytes"))
}

fn parse_reply(reply: &[u8], request_id: u32) -> Option<ServiceReading> {
    if reply.len() < 64 || &reply[0..4] != b"DSYR" || reply[4..8] != request_id.to_be_bytes() {
        return None;
    }
    let counter = be_u64(&reply[16..24]);
    let freq = be_u64(&reply[48..56]).max(1);
    let mono_ns = (counter as i128 * 1_000_000_000 / freq as i128) as i64;
    let ptp_ns = (reply.len() >= 104 && reply[64] != 0).then(|| be_u64(&reply[96..104]) as i64);
    Some(ServiceReading {
        mono_ns,
        ptp_ns,
        locked: reply[41] == 1,
    })
}

fn request(id: u32) -> [u8; 64] {
    let mut r = [0u8; 64];
    r[0..4].copy_from_slice(b"DSYX");
    r[4..8].copy_from_slice(&id.to_be_bytes());
    r
}

#[derive(Debug, Default)]
struct Summary {
    seconds: u64,
    lock_after_s: Option<u64>,
    /// |follower − service| in µs, for the seconds both were locked.
    diffs_us: Vec<f64>,
}

impl Summary {
    fn p99_us(&self) -> Option<f64> {
        let mut v = self.diffs_us.clone();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let idx = ((v.len() as f64 * 0.99).ceil() as usize).clamp(1, v.len()) - 1;
        Some(v[idx])
    }

    fn max_us(&self) -> Option<f64> {
        self.diffs_us.iter().copied().reduce(f64::max)
    }

    /// The exit code and the closing line.
    fn verdict(&self) -> (u8, String) {
        let Some(lock) = self.lock_after_s else {
            return (
                1,
                format!("FAIL: the follower never locked in {} s", self.seconds),
            );
        };
        let (Some(p99), Some(max)) = (self.p99_us(), self.max_us()) else {
            return (
                1,
                format!(
                    "FAIL: locked after {lock} s, but no second with both clocks locked and the \
                     service's PTP time known (is the service running, with a date state?)"
                ),
            );
        };
        let line = format!(
            "locked after {lock} s; {} locked comparisons; |diff| max {max:.1} µs, p99 {p99:.1} µs \
             (bound {LOCK_BOUND_US:.0} µs)",
            self.diffs_us.len()
        );
        if p99 <= LOCK_BOUND_US {
            (0, format!("PASS: {line}"))
        } else {
            (1, format!("FAIL: {line}"))
        }
    }
}

fn kind(e: &FollowerError) -> &'static str {
    match e {
        FollowerError::NpcapMissing { .. } => "NpcapMissing",
        FollowerError::PermissionDenied { .. } => "PermissionDenied",
        FollowerError::InterfaceNotFound { .. } => "InterfaceNotFound",
        _ => "Other",
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&raw).and_then(|a| Ok((resolve_interface(&a.interface)?, a))) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("dsync-ptp-probe: {e}");
            eprintln!(
                "usage: dsync-ptp-probe --interface <name|IPv4> [--seconds N] [--service host:port]"
            );
            return ExitCode::from(2);
        }
    };
    let (interface, args) = args;
    println!(
        "dsync-ptp-probe {} on {interface:?} for {} s, service {}",
        dsync_ptp::VERSION,
        args.seconds,
        args.service
    );
    let follower = match Follower::start(FollowerConfig::on_interface(interface.clone())) {
        Ok(f) => f,
        Err(e) => {
            println!("START FAILED: {}: {e}", kind(&e));
            return ExitCode::from(2);
        }
    };
    let clock = follower.clock();
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            println!("START FAILED: UDP socket: {e}");
            return ExitCode::from(2);
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(300)));

    let start = Instant::now();
    let mut summary = Summary {
        seconds: args.seconds,
        ..Summary::default()
    };
    let mut buf = [0u8; 256];
    for t in 1..=args.seconds {
        std::thread::sleep(
            (start + Duration::from_secs(t)).saturating_duration_since(Instant::now()),
        );
        let state = clock.state();
        if state.locked && summary.lock_after_s.is_none() {
            summary.lock_after_s = Some(t);
        }
        let id = t as u32;
        let reading = socket
            .send_to(&request(id), args.service)
            .ok()
            .and_then(|_| socket.recv(&mut buf).ok())
            .and_then(|n| parse_reply(&buf[..n], id));
        let (diff, svc) = match &reading {
            Some(r) => (
                r.ptp_ns
                    .zip(clock.ptp_at_mono_ns(r.mono_ns))
                    .map(|(svc, own)| (own - svc) as f64 / 1000.0),
                if r.locked { "1" } else { "0" },
            ),
            None => (None, "-"),
        };
        if let (Some(d), true, Some(r)) = (diff, state.locked, &reading) {
            if r.locked {
                summary.diffs_us.push(d.abs());
            }
        }
        println!(
            "t={t:>4}s follower_locked={} diff_us={} service_locked={svc}",
            u8::from(state.locked),
            diff.map_or("-".to_string(), |d| format!("{d:+.1}")),
        );
    }
    drop(follower);
    let (code, line) = summary.verdict();
    println!("{line}");
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_arguments_parse_with_defaults_and_refuse_garbage_136() {
        let a = parse_args(&s(&["--interface", "Ethernet"])).unwrap();
        assert_eq!(a.interface, "Ethernet");
        assert_eq!(a.seconds, 120);
        assert_eq!(a.service, "127.0.0.1:31900".parse().unwrap());
        let a = parse_args(&s(&["--seconds", "30", "--interface", "10.77.9.204"])).unwrap();
        assert_eq!((a.seconds, a.interface.as_str()), (30, "10.77.9.204"));
        assert!(parse_args(&s(&[])).is_err());
        assert!(parse_args(&s(&["--interface", "x", "--seconds", "0"])).is_err());
        assert!(parse_args(&s(&["--interface", "x", "--seconds", "ten"])).is_err());
        assert!(parse_args(&s(&["--interface"])).is_err());
        assert!(parse_args(&s(&["--interface", "x", "--bogus"])).is_err());
    }

    fn reply(id: u32, counter: u64, freq: u64, locked: u8, now_ptp: Option<i64>) -> Vec<u8> {
        let mut r = vec![0u8; 120];
        r[0..4].copy_from_slice(b"DSYR");
        r[4..8].copy_from_slice(&id.to_be_bytes());
        r[16..24].copy_from_slice(&counter.to_be_bytes());
        r[41] = locked;
        r[48..56].copy_from_slice(&freq.to_be_bytes());
        if let Some(p) = now_ptp {
            r[64] = 3;
            r[96..104].copy_from_slice(&p.to_be_bytes());
        }
        r
    }

    #[test]
    fn a_dsyx_reply_gives_the_counter_instant_the_ptp_time_and_the_lock_136() {
        // A 10 MHz QPC at 2.5 s
        let r = parse_reply(&reply(7, 25_000_000, 10_000_000, 1, Some(123_456_789)), 7).unwrap();
        assert_eq!(
            r,
            ServiceReading {
                mono_ns: 2_500_000_000,
                ptp_ns: Some(123_456_789),
                locked: true
            }
        );
        // No date state: no PTP time.
        let r = parse_reply(&reply(7, 1, 1_000_000_000, 0, None), 7).unwrap();
        assert_eq!((r.ptp_ns, r.locked), (None, false));
        // Another id, a short reply, another magic: not ours.
        assert!(parse_reply(&reply(8, 1, 1, 1, None), 7).is_none());
        assert!(parse_reply(&reply(7, 1, 1, 1, None)[..63], 7).is_none());
        let mut bad = reply(7, 1, 1, 1, None);
        bad[0] = b'X';
        assert!(parse_reply(&bad, 7).is_none());
    }

    #[test]
    fn the_verdict_fails_without_a_lock_without_comparisons_or_over_the_bound_136() {
        let never = Summary {
            seconds: 60,
            ..Summary::default()
        };
        assert_eq!(never.verdict().0, 1);
        let no_cmp = Summary {
            seconds: 60,
            lock_after_s: Some(20),
            diffs_us: vec![],
        };
        assert_eq!(no_cmp.verdict().0, 1);
        let mut good = Summary {
            seconds: 200,
            lock_after_s: Some(20),
            diffs_us: (0..100).map(|i| i as f64 * 0.4).collect(), // 0 … 39.6 µs
        };
        let (code, line) = good.verdict();
        assert_eq!(code, 0, "{line}");
        assert!(
            line.contains("p99 39.2") && line.contains("max 39.6"),
            "{line}"
        );
        // Two outliers in 100 move the p99 over the bound.
        good.diffs_us[98] = 70.0;
        good.diffs_us[99] = 80.0;
        let (code, line) = good.verdict();
        assert_eq!(code, 1, "{line}");
    }
}
