# Handover: dsync (2026-11-01)

## Status

This repository was `zbynekdrlik/dantesync`, renamed on 2026-11-01 (ticket airuleset#1280). The binary, the service and the config path keep the name `dantesync`. Rename them only when you decide to; the owner set that as low priority, and it touches every node.

Owner rules from 2026-11-01:

- dsync is a critical part of dstack. Every request comes through dstack, which decides it. Your only peer links are dstack and airuleset.
- Audio stability outranks video. A video or frame-sync need never goes in unchecked.
- You do not act on the camera rig unless camera-box asks through dstack. These rules replace "Autonomous Deployment" in `CLAUDE.md`.
- You own code, tests and releases. camera-box pins a released version and deploys it to the rig.

## Machines

- dev1: this project's account, with no sudo and no local Dante network access assumed.
- Rig nodes belong to camera-box. Do not deploy to them.
- Non-rig nodes mbc, fohabl, iem-snv, companion-snv and alex-nb come to you when camera-box hands over the node list. Until that handover you have no fleet access.

## What else acts on the machines

- camera-box operates the fleet today: `DANTESYNC_FLEET`, fleet upgrade, canonical config, clock watchdog, the sudo rule `camera-box-dev1-dantesync-upgrade` and the secret `dantesync-fleet.env`.
- dstack, iemmixer, fohmixer and songplayer applications depend on the clock through the DSYX contract. They reach you only through dstack.

## How to run a test

CI first: `.github/workflows/ci.yml` runs on GitHub-hosted Linux and Windows. Run `cargo test` locally on dev1. Real clock behaviour needs a Dante network node, so ask dstack for a test on hardware.

## Site facts

Private, never in this repository: `~/.config/dsync/TARGETS.md`, mode 600. It holds credentials. Never print it, and never commit it.

The file was removed from the tree in v1.20.1 (issue #108). Its old values stay in git history by the owner's decision of 2026-11-01: they have no security role on the closed network. Do not raise this again.

## Runners

One self-hosted runner: `companion` (labels `dante-gate`, `companion`), for the Dante stability gate (#98, airuleset#1280). `.github/workflows/gate.yml` is the only thing that touches companion, through `/usr/local/sbin/dsync-gate-install`. A release is tagged only after the gate passed on the same tree. See `docs/dante-gate.md`.

## Backlog

Owner-requested:

- A Dante stability gate that every change must pass, including a video-driven change (2026-11-01).
- A versioned library so applications can lock to Dante PTP internally, without the system service (2026-11-01, issue #136).
- Improve the clock only for a measured need (2026-10-26).

First task: review and merge the handover change from `dev`. Merging starts a release, so check what a release does on the nodes first.

Proposed by a session, not yet asked for by the owner: the open issues in this repository. Issue #98 (release equals fleet deployment) conflicts with the rules above and must be rewritten.

## Knowledge left in other projects

- camera-box: 414 files mention dantesync, plus 6 rule files. Ask through dstack.
- dstack: `docs/design/2026-10-30-shared-dante-stack.md`, and `crates/dante-stack/Cargo.toml` names this project.
- iemmixer: it maintained this code until 2026-10-30, and its memory notes cover Dante.
