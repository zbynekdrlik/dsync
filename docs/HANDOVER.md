# Handover: dsync (2026-11-01)

## Status

This repository was renamed to `zbynekdrlik/dsync` on 2026-11-01 (ticket airuleset#1280). Since 1.25.0 (#147, owner 2026-11-02) everything inside follows: the binaries `dsync` and `dsync-tray`, the release assets `dsync-*`, the Linux unit `dsync.service`, the Windows service `dsync`, the pipe `\\.\pipe\dsync` and the state dir `/etc/dsync` (`C:\ProgramData\dsync`). A box installed before 1.25 keeps its old state dir: the daemon uses it in place until a deployer renames it (`src/state_dir.rs`), and `/status.state_dir` says which one runs. The deploying side (camera-box) switches the units; dsync never deploys.

Owner rules from 2026-11-01:

- dsync is a critical part of dstack. Every request comes through dstack, which decides it. Your only peer links are dstack and airuleset.
- Audio stability outranks video. A video or frame-sync need never goes in unchecked.
- You do not act on the camera rig unless camera-box asks through dstack. These rules replace "Autonomous Deployment" in `CLAUDE.md`.
- You own code, tests and releases. camera-box pins a released version and deploys it to the rig.

## Machines

- dev1: this project's account, with no sudo and no local audio network access assumed.
- One writer per box (camera-box's rule with the owner's warning, relayed by dstack 2026-11-03):
  - Rig boxes are camera-box's ONLY: cam1-cam7, strih-snv, strih-pp, stream-snv, stream-pp, the resolume boxes (resolume-snv included) and dev1. No dsync session or script upgrades, restarts or writes config there; camera-box does them one at a time after its E2E.
  - Non-rig boxes are dsync's only: companion-snv, mbc, fohabl (Ableton-FOH), iem-snv, alex-nb. The owner granted MCP access (`linux-presenter`, `win-mbc-snv`, `win-ableton-foh`, `win-iem-snv`, `win-alex-nb`), 2026-11-03.
  - mbc is shared with dstack and iem-snv with dasio: touch only dsync's program there.
- Upgrading a non-rig box (#153): the owner allows NO mix of old and new names, and NTP must point at the strih PC (`strih.lan`).
  - Windows: `install.ps1` (1.27.1+ moves `C:\ProgramData\DanteSync`; 1.27.2+ also removes every DanteTimeSync name).
  - Verify `/status` `state_dir`, `is_locked`, the version, `ntp_server` and that no dante* name of dsync's is left. Audinate's and dante-asio's names are not dsync's.

## What else acts on the machines

- camera-box operates the fleet today: `DANTESYNC_FLEET`, fleet upgrade, canonical config, clock watchdog, the sudo rule `camera-box-dev1-dantesync-upgrade` and the secret `dantesync-fleet.env`.
- dstack, iemmixer, fohmixer and songplayer applications depend on the clock through the DSYX contract. They reach you only through dstack. The lock bytes' meaning (`[41]`, `[62]` bit 1): `docs/dsyx-contract.md`.

## How to run a test

CI first: `.github/workflows/ci.yml` runs on GitHub-hosted Linux and Windows. Run `cargo test` locally on dev1. Real clock behaviour needs an audio network node, so ask dstack for a test on hardware.

## Site facts

Private, never in this repository: `~/.config/dsync/TARGETS.md`, mode 600. It holds credentials. Never print it, and never commit it.

The file was removed from the tree in v1.20.1 (issue #108). Its old values stay in git history by the owner's decision of 2026-11-01: they have no security role on the closed network. Do not raise this again.

## Runners

One self-hosted runner: `companion` (labels `dsync-gate`, `companion`), for the stability gate (#98, airuleset#1280). `.github/workflows/gate.yml` is the only thing that touches companion, through `/usr/local/sbin/dsync-gate-install`. A release is tagged only after the gate passed on the same tree. See `docs/stability-gate.md`.

### Companion access (dsync#147, owner ruling via airuleset, 2026-11-02)

- The gate on companion is dsync's. airuleset no longer maintains `dsync-gate-install`, its sudo rule, its restore timer or the runner.
- dsync reaches companion through `linux-presenter` (`http://100.104.8.125:8796/linux-presenter/mcp`), the MCP server remoteos set up. `fleet` on the same gateway only lists and looks up machines.
- "Pending approval" is Claude Code's project-server trust prompt. Only the owner clears it, through `/mcp` in the dsync session. Never edit the settings to skip it.
- remoteos reads the server's rights (2026-11-02). Until that answer, leave the helper as it is; the gate works with it.
- Conditions for every touch:
  - Read `http://100.104.8.125:8890/rig-lease.json` before each touch.
  - Do nothing while the lease is held or during a production.
  - Never restart Companion itself.
- Known leftover: the helper still installs into the pre-1.25 unit, binary and paths (it swaps the binary inside the old unit). Rename it, or move its job into `gate.yml`, once the access is clear. Then tell airuleset when the old helper, its timer and the runner can be removed.

## Backlog

Owner-requested:

- A stability gate that every change must pass, including a video-driven change (2026-11-01).
- A versioned library so applications can lock to PTPv1 internally, without the system service (2026-11-01, issue #136).
- Improve the clock only for a measured need (2026-10-26).

First task: review and merge the handover change from `dev`. Merging starts a release, so check what a release does on the nodes first.

Proposed by a session, not yet asked for by the owner: the open issues in this repository. Issue #98 (release equals fleet deployment) conflicts with the rules above and must be rewritten.

## Knowledge left in other projects

- camera-box: 414 files mention this project by its pre-1.25 name, plus 6 rule files. Ask through dstack.
- dstack: its shared-stack design doc (2026-10-30) and its stack crate's `Cargo.toml` name this project (dstack owns those names).
- iemmixer: it maintained this code until 2026-10-30, and its memory notes cover the audio network.
