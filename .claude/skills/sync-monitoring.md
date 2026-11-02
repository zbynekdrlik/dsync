---
name: sync-monitoring
description: Monitor and verify dsync clock synchronization across multiple computers. Use when checking sync status, running comparison tests, or verifying NTP/PTP alignment.
---

# dsync Sync Monitoring

## Quick Status Check

```bash
python3 scripts/sync-snapshot.py --hosts <host> ...           # Full comprehensive report
python3 scripts/sync-snapshot.py --hosts <host> ... --brief   # Quick status summary
python3 scripts/sync-snapshot.py --hosts <host> ... --json    # JSON output for scripting
```

## Hosts

Fleet hosts are not in this repository: they are in `~/.config/dsync/TARGETS.md`. dsync
queries no node until camera-box hands over the non-rig list, and never a camera-rig node
(`docs/HANDOVER.md`). `scripts/sync-snapshot.py` without `--hosts` still queries its
built-in list, so always pass `--hosts`.

## Interpreting Results

**Sync Quality Thresholds:**

- `< 1ms`: SYNCED - All clocks aligned
- `1-5ms`: DRIFT - May need investigation
- `> 5ms`: ERROR - Sync problem detected

**NTP measurement quality (dsync#53):** `ntp_offset_us` (status JSON / tray) is a
burst-filtered, RTT-selected MEDIAN (5 raw samples/check, lowest-3-RTT accepted), not a single raw
round trip. Two fields expose whether to TRUST that median: `ntp_spread_us` (max-min across the
accepted samples — large even when the offset itself looks reasonable means the node's NTP
measurement is still noisy) and `ntp_sample_count` (how many samples backed it — lower than 3 means
a partial burst failure). **Never read `ntp_offset_us` alone as "this node is fine" — check
`ntp_spread_us` too.**

The burst+RTT-select+median filter ALONE (v1.8.21) did NOT fix a loaded Windows box's ±20ms
scatter — a live canary proved the contamination lives INSIDE a single 5-sample burst (samples
taken back to back spread by up to 21ms), so no amount of filtering across a burst can recover a
clean measurement; only a Linux client, on the same server at the same instant, stayed tight
(5-32us). The real fix (v1.8.22) is a kernel-timestamped Npcap transport for Windows NTP
(`PcapNtpTransport` in `src/net_pcap.rs`) — the same `HostHighPrec`/`KeQuerySystemTimePrecise()`
technique the PTP receive path already used, now applied to both t1 (our own request leaving the
NIC) and t4 (the server's reply arriving) instead of userspace `SystemTime::now()`. Linux is
unchanged (`rsntp`, already precise). A wide `ntp_spread_us` on an otherwise-locked Windows node
now most likely means either the Npcap transport failed to initialize (permanent rsntp fallback,
logged loudly at startup) or the network/server path itself is genuinely noisy — check the log for
`[NTP] Npcap kernel-timestamped transport unavailable` before assuming it's a CPU/scheduling issue.

**Sync Modes:**

- `ACQ` - Acquiring lock (fast convergence)
- `PROD` - Production mode (stable)
- `LOCK` - Locked (frequency stable < 5us/s)
- `NANO` - Ultra-precise mode (< 0.5us/s)
- `NTP-only` - No PTP, NTP fallback

## Troubleshooting

**OFFLINE host:** Check service running + UDP port 31900 open in firewall

**Large offset:** Check the node's `ntp_server` points at the fleet's NTP master

**Config Locations:**

- Linux: `/etc/dsync/config.json`
- Windows: `C:\ProgramData\dsync\config.json`
