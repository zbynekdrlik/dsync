---
paths:
  - "src/pinned_wait.rs"
  - "src/boot_ready.rs"
  - "src/service/ptp_join.rs"
  - "crates/dsync-ptp/src/net.rs"
  - "crates/dsync-ptp/src/net/pinned_list.rs"
  - "crates/dsync-ptp/src/net/pinned_list/tests.rs"
  - "crates/dsync-ptp/src/net_linux.rs"
  - "src/net_pcap.rs"
  - "crates/dsync-ptp/src/net_pcap.rs"
  - "src/net_winsock.rs"
  - "crates/dsync-ptp/src/gm_filter.rs"
---

# Multi-homed interface selection for the PTP receive path (camera-box issue 1073)

## FIRST: the pinned interface (#131, owner ruling 1.11.2026) overrides everything below

"by defaul dsync musi mat jasne nastavenie na ktoru eth sa ma bindnut, neexistuje ze sa toto
deje nejako nahodne". stream-snv ran a whole production without the grandmaster clock because a USB
NIC (VBAN leg) got its /32 before the rig NIC at boot and was first in `getifaddrs` order.

- `system.interface` / `--interface` (CLI wins, blank = unset; `SystemConfig::pinned_interface`)
  pins the NIC by name. Windows: the adapter's friendly name (`Ethernet`) or its GUID
  (`GetAdaptersAddresses`, feature `Win32_NetworkManagement_Ndis`), case-insensitive, a GUID with
  or without braces / `\Device\NPF_` (`net::classify_pinned_with`). The pure classifier returns
  `Ready(ip) | LinkLocalOnly(ip) | NoIpv4 | Missing`; loopback never counts. A link-local-only
  NIC (an audio-network segment without DHCP — or an APIPA placeholder at boot) is taken by the startup
  only after `net::PINNED_STARTUP_GRACE` (10 s, `net::usable_pinned_ip`), by a re-join at once.
- Pinned startup: `pinned_wait::PinnedWait::run` (every OS boundary injected, unit-tested) looks
  every second until the NIC has an IPv4 AND the receive path OPENS on it — an open failure
  (Windows: the Npcap device list lags the adapter list at boot) is retried, never fatal. The
  clock alarm names the NIC at once when missing, after the grace when without an IPv4 or not
  opening, and once raised follows the NIC's CURRENT state. On open the interface reason ends:
  the snapshot is cleared without a "re-locked" edge (nothing is locked yet), or keeps a #116
  config reason. A pinned Npcap open uses the IPv4 the wait approved
  (`select_pinned_device_at`), skips the allowlist resolution and is not an ERROR per retry.
  Never another NIC.
- The wait never holds systemd READY (#193, camera-box#1483: a missing rig NIC kept lightdm dark
  for the unit's 120 s timeout). The #164 `ReadyGate` starts at process start in `main` and is
  polled through `PinnedWait.waiting` on every look that did not open, so READY goes at 30 s
  with the NIC's state as the reason; the SAME gate then goes into the sync loop (READY once).
  A new startup wait before the loop must take the gate too, or it reopens this hole.
- Pinned re-join (Linux `UdpPtpNetwork`, Npcap `select_pinned_device`): ONLY the pinned NIC — no
  home address, no resolver, no subnet rule — with a typed `net::PinnedInterfaceUnavailable`
  BEFORE closing anything. The controller (which knows the pin from `set_ptp_interface`) turns
  ANY re-join failure of a pinned node into `ClockHealth.interface_problem` (first in the alarm
  precedence) and looks again every 5 s (`PINNED_WAIT_RETRY`) instead of the 30..300 s backoff,
  logging the two WARN lines on a changed reason and every 60th look (DEBUG between).
- Unpinned: everything below, byte-identical, plus a startup WARN that the interface is not
  pinned. `/status`: `ptp_interface_configured`, `ptp_interface`, `ptp_interface_ip` (the last
  successful join).
- The old stance in this file ("do not add a config field pinning the interface — the allowlist
  IS the signal") is SUPERSEDED by the ruling. The subnet rule remains the unpinned fallback.

## A pinned LIST (#212, v1.35.16, camera-box#1545)

Every SNV/PP cambox has two onboard ports; with `enp3s0` pinned and the cable in `enp1s0`, cam3
ran the 19.11.2026 service on NTP. `system.interface` / `--interface` may now name several NICs.

- The pin stays ONE `Option<String>` everywhere, the CANONICAL spec `a,b` (split on `,`, trimmed,
  blanks dropped: `net::canonical_interface_spec`). Config: a string with commas or a JSON array
  of strings (`optional_interface_list`); a non-string element fails the parse (#116 last-good).
  A single name is byte-identical to the #131 pin (same messages, same choice).
- `net::choose_pinned(spec, order, look)` is the ONE decision, `PinnedOrder`:
  - `Startup`: the list order, the first `Ready`, else the first `LinkLocalOnly` (a boot-time
    link-local address may be a DHCP placeholder, #131's grace).
  - `Stay(joined)` (a re-join while the TRUSTED grandmaster still flows on the joined NIC,
    `net::rejoin_order(joined, TrustedPtp::Flowing)`, the caller not stale): the joined NIC if usable — the #191 link-local leave must
    never hop to another port.
  - `MoveOn(joined)` (PTP went silent there): the NEXT usable listed NIC after it, wrapping,
    routable and link-local ALIKE. Review round 1: "routable first" on a stale re-join held a
    dead port with a static address for ever when the live port was link-local; two ports that
    both keep an address alternate on each stale re-join (up to the #112 backoff to come back).
  - Nothing usable: `NoIpv4` (any listed NIC exists) / `Missing`, naming the whole list.
  Consumers: `net::pinned_choice` (startup `pinned_wait::run_list`, Npcap
  `select_pinned_device`, the follower); the Linux re-join walks
  `PtpSocketFactory::lookup_pinned(name)` per listed name. `rejoin_order(joined, trusted)`
  takes the CALLER's `TrustedPtp` (#215, v1.35.17): `Silent` from a stale re-join, `Flowing`
  from the #191 leave — the backend never judges it, it sees foreign grandmasters too.
- The controller's #191 link-local look asks ONLY the joined listed NIC
  (`ptp_liveness.joined_name`, from `set_ptp_interface_name` / `RejoinOutcome::pinned_name`),
  never the whole list: a routable address on the other port is no reason to leave.
- A move = the #112 path: the old port goes silent, stale at 10 s, `rejoin` picks the listed NIC
  that has the IPv4 now; none has one = `PinnedInterfaceUnavailable { name: "a,b" }`, the 5 s
  cadence, alarm "none of the PTP interfaces a,b has an IPv4 address" / "... exists".
- `RejoinOutcome::pinned_name` carries the listed name (on Windows `iface` is the Npcap device);
  `/status.interfaces_configured` (list) + `ptp_interface_name` (additive, schema stays 1).
- A NIC name containing a comma cannot be pinned (none on the fleet).

On a DUAL-HOMED box (e.g. the stream box: rig `Ethernet` 10.77.9.204/24 + mbc `Ethernet 2`
10.77.7.204/24) the PTP receive path must attach BOTH the pcap capture AND the IGMP `224.0.1.129`
join to the NIC that reaches the grandmaster — not whichever NIC the OS enumerates first.

## Where the pieces live

- `net::get_default_interface()` — first non-wireless bindable IPv4 NIC (the OS default). This is the
  FALLBACK / default-interface hint, and on a multi-homed box it can be the WRONG NIC.
- `NpcapPtpNetwork::new(interface_name, gm_allowlist)` (`net_pcap.rs`, Windows-only) — the real
  Windows PTP capture. It calls `find_ptp_capture_device(gm_allowlist, fallback_name)` which
  enumerates pcap devices, asks `gm_filter` which NIC is on the trusted grandmaster subnet, and
  drives BOTH the IGMP join (`device_ipv4` / the matched IP) AND the capture off the chosen device.
- `gm_filter::GmAllowlist::{select_interface, best_interface_matches}` — the PURE selection layer.

## The pattern: reuse the #53 NTP dual-homed selector, don't reinvent

The identical "PTP and NTP live on different subnets on a dual-homed host" problem was already solved
for the NTP transport by `net_pcap::find_device_for_ntp_server` + `ntp_packet::select_ntp_pcap_device`
(dsync#53 — subnet containment, longest-prefix, skip zero-netmask junk adapters). The PTP
interface selection MIRRORS it. If you touch the receive path, follow that pattern; do not add a
config field pinning the interface (rejected — the `gm_allowlist` the operator already sets IS the
signal, zero extra config).

dsync issue 112: the startup AND the PTP re-join select through ONE `select_ptp_device(hint,
allowlist, home)` (startup `home = None`): the trusted-subnet rule, then the device that carries the
home address (where the grandmaster's time was last received), then the name fallback — the pure
order is `net::choose_capture_device`. Linux has no trusted-subnet rule (it uses
`net::get_default_interface`, plus the home address on a re-join): see `ptp-liveness-rejoin.md`'s
known limits.

## The pure/Windows split — test the LOGIC in gm_filter, compile-check the glue

`net_pcap.rs` is `#[cfg(windows)]` — a plain Linux `cargo test` SKIPS it, and it needs real pcap
devices to exercise. So the selection LOGIC lives in `gm_filter.rs` (pure, no pcap, no cfg) and is
fully Linux-unit-tested; `find_ptp_capture_device` is thin glue that only enumerates devices and
delegates. To verify a receive-path change:
- Logic: `cargo test --lib` (the `gm_filter::tests::*interface*` / `*tie*` / `*ambiguous*` tests).
- Windows glue: `cargo check --target x86_64-pc-windows-gnu --bin dsync --tests --lib` (see
  `windows-only-code.md`). It compiles net_pcap but does not run it — CI's Windows leg runs the
  `mod tests`.

## Selection semantics (gm_filter)

- `overlaps` is a SYMMETRIC shorter-mask test, so a trusted prefix matches an interface whether the
  allowlist entry is an exact GM (`10.77.9.184/32` — the rig `/24` interface contains it) or a CIDR
  (`10.77.9.0/24` — the interface's own IP is inside it). A `/0` entry gives no interface signal and
  is ignored for selection (it still counts for source filtering).
- Ranking: most-specific trusted prefix, then longest interface prefix, then first-listed. The
  secondary interface-prefix key is LOAD-BEARING (a `/32` GM entry also "overlaps" a wide `/8` NIC on
  the top bits; only the longer interface prefix keeps the rig `/24` winning).
- `best_interface_matches` returns ALL best-scoring candidates so the caller can DETECT ambiguity;
  `select_interface` is just its `.first()`. `find_ptp_capture_device` treats >1 distinct matched
  DEVICE (an over-broad allowlist spanning two subnets) as ambiguous → keeps the default interface
  (never worse than pre-change) + warns to narrow the allowlist. A unique match is the normal case.

## A dead hostname is an invalid entry (#217, v1.35.20)

pp-cam6 (22.11.2026) ran on NTP before a live service: `gm_allowlist: ["video-clock.lan"]`, a name
PP's DNS does not know, so #113's "a hostname-only list restricts until it resolves" dropped PP's
only GM. Now `GmAllowlist::fails_open()`: after a real resolve pass, no literal prefix and no
resolution (none kept) = unrestricted, a WARN each re-resolve, `/status.gm_allowlist_fail_open`.
- Before the first pass a hostname-only list still restricts; #113's keep-previous holds (a name
  that resolved once never opens the list on a DNS blip).
- A dead name beside a working entry is ignored with an ERROR naming it; the clock alarm's
  "hostname unresolvable" reason fires only while no allowed GM is heard.
- The stale alarm reason names the drop: `<ip> dropped by gm_allowlist [<entries>]`
  (`ClockHealth.allowlist_dropped`, from the controller's `last_gm_dropped_ip`).
- A controller test that must not resolve uses an RFC 6761 `.invalid` name: `PtpController::new`
  resolves with the REAL resolver before a test can inject `MapResolver`.

## Backward compatibility (mandatory)

Empty/unrestricted allowlist, `/0`-only, or no interface on a trusted subnet → the pure selector
returns None/empty → `find_device(fallback_name)` = the historical default-interface behavior,
BYTE-IDENTICAL. Only a multi-homed box WITH a restricting allowlist whose default NIC is off the GM
subnet changes behavior. The Linux socket path (`RealPtpNetwork`) is intentionally NOT changed (the
fleet's only multi-homed box is Windows).

## The IGMP-join socket must bind an EPHEMERAL port, NEVER 319/320 (dsync#109 — DVS coexistence)

On a box that ALSO runs the virtual soundcard (DVS) (the `stream` box, 10.77.9.204), DVS's own `ptp.exe`
needs UDP 319/320 exclusively for its PTP follower. The pcap path's IGMP-membership socket
(`net_pcap::join_multicast`) must therefore bind an **ephemeral port (`net::igmp_join_bind_addr()` →
`0.0.0.0:0`)**, never a PTP port — IGMP group membership is per interface+group, NOT per port, and
pcap's own BPF filter (`dst port 319 or dst port 320`) selects the captured traffic, so the join
socket's bound port is irrelevant. It used to bind 319 AND 320 (one socket each, plus a pointless
`SO_REUSEADDR`); dsync is a boot-time service and bound first, DVS's `ptp.exe` (a login-time app
without `SO_REUSEADDR`) then failed its own bind with `WSAEADDRINUSE` — live on `stream` 2026-09-03
dsync held BOTH 319 and 320 and `ptp.exe` held neither — starving DVS's follower of the PTP
*general* messages (Follow_Up/Delay_Resp) on 320, so its media clock free-ran on the host crystal
(≈−17 ppm off the grandmaster, hidden only by the camera-box ASRC servo). **Never re-add a
fixed-PTP-port bind to any pcap/socket join path.**

- The bind decision is the pure, non-cfg-gated seam `net::igmp_join_bind_addr()` (Linux-CI unit
  tested — port must be 0, never 319/320); `net_pcap.rs` glue only calls it. Follow the same
  split for any new socket that joins a PTP group.
- `net_winsock.rs` (the non-pcap fallback, not currently wired into `main`) genuinely receives on
  319/320, so it MUST bind them — but it now logs a loud `error!` (with the WSA error, incl.
  `WSAEADDRINUSE=10048`, and the likely DVS cause) when a bind fails, instead of a silent Err.
- Acceptance on the `stream` box after deploying the fix: `Get-NetUDPEndpoint -LocalPort 319,320`
  shows ONLY `ptp.exe` on both ports (dsync no longer appears), and the OBS log's
  `asrc: source 'mbc' estimated=` residual collapses from ≈−17 ppm toward 0 (DVS is now disciplined
  by the grandmaster). Read live state via the win-stream MCP (session-agnostic `Get-NetUDPEndpoint`
  / process list / OBS log read). **`ptp.exe` binds its ports only at ITS start** — after the
  dsync restart the ports may sit UNOWNED (nobody on 319/320) until DVS is restarted; that is
  NOT "still broken", it is the DVS side not having retried yet. If `ptp.exe` has not re-bound within
  ~1 min, restart the virtual soundcard (DVS) (it briefly drops the ASIO device stream OBS reads `mbc`
  from — do it off-air and verify OBS audio resumes), THEN re-read the endpoints.
