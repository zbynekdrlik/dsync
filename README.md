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
- `--interface <NAME>`: Bind to specific interface (e.g., `eth0`)
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
- Npcap SDK 1.13+ (set `LIB` env var to `npcap-sdk/Lib/x64`)

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

## License
MIT
