# dsync

A high-precision PTP (Precision Time Protocol) synchronization tool optimized for audio networks, written in Rust.

## Features

### Core Sync
- **PTPv1 Support:** Syncs with grandmasters (PTPv1/UDP 319/320)
- **Hybrid Mode:** Uses NTP for UTC alignment + PTP for microsecond-precision frequency adjustment
- **Cross-Platform:** Runs on Linux and Windows as a system service
- **Rate-Based Servo:** Adaptive frequency control targeting <5µs/s drift rate
- **Lucky Packet Filtering:** Minimizes network jitter effects

### Windows Tray App
- **Dynamic Icon:** Pulsing ring indicates drift rate (green=locked, yellow=acquiring, red=offline)
- **Toast Notifications:** Alerts for lock achieved, lock lost, service online/offline
- **Service Control:** Restart/Stop service directly from tray menu
- **Live Status:** Tooltip shows drift rate, frequency adjustment, NTP offset

### Linux Tray Icon
- **Same states as Windows:** green = LOCK, cyan = NANO, yellow = acquiring, orange = NTP-only
  (PTP lost), red = the service does not answer (never a stale green)
- **Menu:** status, mode and PTP master lines (read-only) plus Quit
- **Source:** polls the daemon's `http://127.0.0.1:8898/status` once a second (the port from
  `http_status.port` in `/etc/dsync/config.json`; `http_status.enabled` must stay on, the
  default); one instance per user
- **Run:** `dsync-tray` (release asset `dsync-tray-linux-amd64`) inside an X session whose
  panel has a system tray (e.g. tint2). Runtime packages: `libgtk-3-0`,
  `libayatana-appindicator3-1`, `libxdo3`. The NO PTP CLOCK alarm is notified by the daemon
  itself (`notify-send`), so the Linux tray shows no notifications.

## Library: an in-process PTPv1 follower (`dsync-ptp`)

Applications that must lock to the grandmaster in their own process use the library
crate `crates/dsync-ptp`. It holds the service's own PTP code: the PTPv1 parser, the receive
paths, the rate servo, the phase lock and the lock gate. A fix lands once for both.

```toml
dsync-ptp = { git = "https://github.com/zbynekdrlik/dsync", tag = "v1.24.0" }
```

```rust
let follower = dsync_ptp::follower::Follower::start(
    dsync_ptp::follower::FollowerConfig::on_interface("enp2s0"),
)?;
let clock = follower.clock(); // Clone + Send + Sync
let ptp_ns = clock.now_ptp_ns(); // lock-free; None until anchored
let state = clock.state(); // locked (DSYX [41]), holdover, gm_uuid, date_offset_ns, epoch, ...
```

- It never sends and never touches the system clock. It runs a virtual clock over
  `CLOCK_MONOTONIC_RAW` (Linux) or QPC (Windows).
- On Linux it shares UDP 319/320 with the service and other followers (`SO_REUSEADDR`). The app
  needs `CAP_NET_BIND_SERVICE` (or root) to bind 319.
- On Windows it captures with Npcap, which must be installed.
- On one PC, the service and the library give the same PTP time within the lock bound
  (`tests/library_acceptance.rs`).

## Installation

### Linux
```bash
curl -sSL https://raw.githubusercontent.com/zbynekdrlik/dsync/master/install.sh | sudo bash
```

### Windows
1. **Prerequisite:** Install [Npcap](https://npcap.com/#download) (Select "Install Npcap in WinPcap API-compatible Mode")
2. Open PowerShell as **Administrator**
3. Run:
```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/dsync/master/install.ps1 | iex
```

## Uninstall

### Windows
```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/dsync/master/uninstall.ps1 | iex
```

## Usage (Manual)
```bash
dsync [OPTIONS]
```
- `--interface <NAME>[,<NAME>…]`: Pin the PTP interface (e.g., `eth0`; overrides `system.interface`).
  A list (`enp1s0,enp3s0`, or `"interface": ["enp1s0", "enp3s0"]` in `config.json`) binds
  whichever listed NIC has an IPv4 and moves to another listed NIC when the cable does; an
  unlisted NIC is never used. Windows takes the adapter's friendly name or GUID.
- `--ntp-server <IP>`: NTP server for initial sync (default: `10.77.8.2`)
- `--skip-ntp`: Skip NTP sync
- `--service`: (Windows Only) Run as a Windows Service

## Build from Source
```bash
cargo build --release
```

The tray needs the `tray` feature: `cargo build --release --features tray --bin dsync-tray`
(on Linux with `libgtk-3-dev libayatana-appindicator3-dev libxdo-dev` installed). The daemon is
built without it and never links gtk.

**Windows Build Requirements:**
- Rust Toolchain (`x86_64-pc-windows-msvc`)
- No Npcap SDK: `wpcap.dll` is loaded at run time (since 1.27). Running needs the Npcap runtime.

## Configuration

Config files:
- Linux: `/etc/dsync/config.json`
- Windows: `C:\ProgramData\dsync\config.json`

Since 1.25 (#147) the state dir is `/etc/dsync` (`C:\ProgramData\dsync`). A box installed before
1.25 keeps its old state dir in place until a deployer renames it; `/status.state_dir` names the one
in use (`src/state_dir.rs`).

Log files:
- Linux: `/var/log/dsync/dsync.log`
- Windows: `C:\ProgramData\dsync\dsync.log`

## Monitoring `/status`

Every node serves its clock state as JSON on `http://<node>:8898/status` (the port is
`http_status.port` in `config.json`). The contract is **additive only**: fields are never renamed
or removed, new fields are added, and a reader must ignore fields it does not know.

`schema` is the contract version (since 1.35.15). It is `1` today and moves only if a field below
ever changes its meaning; adding fields does not move it. A reply without `schema` comes from a
build older than 1.35.15.

The stable fields a monitor reads:

| Field | Type / unit | Meaning |
|---|---|---|
| `version` | string | The running dsync release. Absent before 1.35. Not the contract version. |
| `schema` | integer | The `/status` contract version, `1`. Absent before 1.35.15. |
| `mode` | string | `"ACQ"` acquiring, `"PROD"`, `"LOCK"`, `"NANO"` locked, `"NTP-only"` (no PTP packet for 10 s), `"FALLBACK"` (on the dsync fallback time source). Only `LOCK` and `NANO` are healthy PTP states. |
| `is_locked` | bool | The published lock. Under the default `ptp_phase_lock` discipline: the phase lock within 50 µs (DSYX [41]; it drops beyond 150 µs). `false` while PTP is stale. |
| `ptp_phase_locked` | bool | The PTP phase lock owns the clock's frequency (engaged after acquisition). |
| `ptp_phase_error_us` | number µs, or `null` | **The lock quality**: the phase error of the last PTP window against the fleet time line, ~0 when locked. `null` until anchored. |
| `offset_ns` | integer ns | The raw offset to the grandmaster's PTPv1 time, which is the grandmaster's device UPTIME. **Not an error**: its absolute value is arbitrary and reads the same on every box behind one grandmaster port (e.g. −151.7 ms on the SNV rig while locked). Never alarm on it. |
| `drift_ppm` | number ppm | The frequency correction applied to this box's clock now. |
| `gm_uuid` | 6 bytes, or `null` | The grandmaster's clock UUID from the Sync messages (correct since 1.17). |
| `gm_source_ip` | IPv4 string, or `null` | The address the grandmaster's Sync messages come from. |
| `last_ptp_rx_age_s` | integer s, or `null` | Seconds since the last accepted PTP packet; `null` when none since start. Above 10 the node is PTP-stale. Read it before trusting any PTP field. |
| `ptp_rx_pps` | number packets/s | Accepted PTP packets per second over the last 10 s (a grandmaster sends ~16). `0` while stale. |
| `clock_alarm` | object `{active, since, reason}` | `active` true while the node is NOT PTP-locked to an allowed grandmaster; `since` the unix second the episode began (`null` when inactive); `reason` a human string (`""` when inactive). |
| `fallback_role` | string, or `null` | The fallback time source role: `"off"`, `"candidate"`, `"follower"` or `"source"` (since 1.33). |
| `ptp_interface_ip` | IPv4 string, or `null` | The IPv4 of the interface the PTP receive path joined; `null` until joined. |
| `interfaces_configured` | list of strings, or `null` | The pinned interfaces (`system.interface` / `--interface`), in order; `null` when not pinned. Since 1.35.16. |
| `ptp_interface_name` | string, or `null` | The listed interface the PTP receive path joined, as configured; `null` when not pinned or not joined. Since 1.35.16. |

A healthy node: `mode` is `LOCK` or `NANO`, `is_locked` is true, `|ptp_phase_error_us|` is well
under 50, `last_ptp_rx_age_s` is at most a few seconds and `clock_alarm.active` is false.

## License
MIT
