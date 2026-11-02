# Dante stability gate (#98)

Owner decision, 2026-11-01: every release passes a Dante stability gate on real hardware first.
Part 1 (this file) is live. Part 2 waits on the owner (see below).

## Where and who

- Box: `companion` (companion-snv). The owner chose it on 2026-11-02; mbc was dropped because it
  carries the final stream mastering.
- Runner: labels `dante-gate` and `companion`, with no default labels. It runs as `dsync-runner`
  (Nice 10, CPUWeight 20, MemoryMax 4G). airuleset provisioned it (airuleset#1280).
- Root access is one helper, `/usr/local/sbin/dsync-gate-install`:
  - `<binary>` installs the candidate and restarts `dantesync`.
  - `--restore` puts the previous binary back.
  - Exit 3 means the rig lease is held (a production). The helper refuses, and so does the gate.
    `--restore` under a held lease also exits 3, but it QUEUES the restore. A root timer applies it
    about a minute after the lease ends.
  - Exit 6: a queued restore is pending, so the helper refuses a new install. The gate fails with
    "previous restore still queued".
  - Exit 5: a bad file. Exit 0: done (`--restore` on a free lease included).
  - After a restore, `.gate-prev` is removed, so the next install saves the binary that is current
    then.
  - The held-lease paths have not been exercised live yet (airuleset, 2026-11-02).
- dsync never deploys to nodes. This job and its helper are dsync's only touch of companion. No
  ssh.

## Workflow: `.github/workflows/gate.yml`

Triggers:

- `push` to `dev`. This is how a release candidate gets gated.
- A nightly `schedule` at 03:00 UTC. That is after dsync's nightly date window (`daily_step_utc`,
  default 02:00 UTC), so the hour measures steady state.
  - A scheduled run always checks out the default branch, so the cron job only dispatches
    `gate.yml` on `dev`.
  - It dispatches nothing when dev's head already passed. Every gate run restarts `dantesync` on
    companion.
  - A `schedule` run is never release evidence.
- `workflow_dispatch`, for example `gh workflow run gate.yml --ref dev`.
- **Never `pull_request` or `pull_request_target`.** The repo is public, so a fork PR must never run
  code on the self-hosted runner.

**The real control against fork PRs is the repo setting, not this file.**

- A fork PR runs its own copy of the workflows. `ci.yml` runs on `pull_request`, so a fork could
  write `runs-on: [dante-gate, companion]` into it.
- What stops it: fork PR workflows need a maintainer's approval on `zbynekdrlik/dsync`. airuleset
  set this when it provisioned the runner (airuleset#1280, 2026-11-02).
- Never approve a fork PR's workflow run that touches `.github/`.
- The security controls are exactly these two: the triggers and the fork approval.
- airuleset rejected an environment-variable check in the helper: sudo strips the variable, so the
  check adds no boundary.

Jobs:

1. **build** (`ubuntu-latest`): builds the daemon with `cargo build --release --bin dantesync`, the
   same as `release.yml`. It downloads dstack's pinned `dante-stack-dev-v0.4.33-linux-x86_64` and
   fails unless its sha256 is `1ce9bf89720cf4ef7df6a722e5f7bef1df775771832cd772590addc5d1458a76`.
2. **gate** (companion, concurrency group `dante-gate`, `cancel-in-progress: false`, timeout 90
   min):
   - checks the sha256 again;
   - installs the candidate through the helper;
   - runs `dante-stack-dev clock --seconds 3600` against the box's own time server
     (127.0.0.1:31900). It only reads, and never opens UDP 319 or 320;
   - samples `http://127.0.0.1:8898/status` every 10 s;
   - uploads `clock.jsonl`, `clock.stderr` and `status.jsonl` as the `gate-logs` artifact;
   - runs `--restore` last whenever an install was attempted, also after a failure or a cancel.
     It does not run after an install refusal (exit 3 or 6), because nothing was installed.
3. **verdict** (`ubuntu-latest`): `scripts/dante_gate.py verdict gate-logs/clock.jsonl --polls
   3600`.

GitHub keeps one pending run per concurrency group. A third run that queues behind a running gate
replaces the older pending one, so only the newest candidate waits.

## Pass rule (part 1, dstack 2026-11-02)

`dante-stack-dev clock` v0.4.33 prints one JSON object per poll (`ready`, `re_anchors`,
`re_anchor_ns`, `holdover`, `sample`, `locked`, `mode`, `freq_ppb`, ...; see its `--help`). Over
the whole run:

- **exactly one `Ready`**: a poll whose `ready` turns 1. The first poll counts when it is already 1.
  Never ready fails, and ready lost and regained fails;
- **zero `ReAnchored`**: no poll with a non-null `re_anchor_ns` or a non-zero `re_anchors`;
- **zero `Holdover`**: no poll with `holdover` = 1, except a StepNear poll;
- `StepNear` (`sample` 3, a date step within 2 s) is fine. dstack's tracker turns every unusable
  poll after the anchor into Holdover, StepNear included (`Tracker::feed` in
  `crates/dante-stack/src/clock.rs`, v0.4.33). So a holdover on a StepNear poll is counted apart and
  passes. A slew (`sample` 4) holdover fails. The other sample codes are only counted in the
  report;
- **a complete log**: exactly 3600 parseable poll records, and the tool exits 0.

Tests: `scripts/test_dante_gate.py`, run in CI's Test job (`python3 -m unittest discover -s scripts
-p 'test_*.py'`).

## Release coupling

`auto-release` in `ci.yml` runs `scripts/dante_gate.py release-check` before it creates a tag. It
takes every local commit whose tree equals the master commit's tree (`git log --all --format='%H
%T'`) and looks each one up (`gate.yml` runs with `head_sha=<sha>`). It passes only on a
successful run started by `push` or `workflow_dispatch`.

The PR's merge commit has a new SHA. When master is an ancestor of dev (the normal flow), its tree
is identical to dev's gated head, and equal trees are identical files. If master ever diverges, the
trees differ, and the release waits for a gate on the merged tree. Any API error listing the runs
fails closed.

So the order is:

1. push to `dev`;
2. wait for a green `gate.yml` run on that push;
3. merge the PR.

A commit on `dev` after the gate changes the tree, so the gate must run again before the merge.

**Gate refused or failed** (the lease, the rig, a real regression): fix it, or wait for the lease
to be free. Then gate the tree again:

- Before the merge: `gh workflow run gate.yml --ref dev`.
- After the merge: `gh workflow run gate.yml --ref master`. That run's `head_sha` is the merge
  commit itself. Dev may have moved on already, so a dev run might test a different tree.
- Then re-run master CI's `auto-release` job. The check comes before the tag step, so a rerun always
  checks again.

**Gated binary and released binary.** `release.yml` rebuilds the same tree at tag time, with the
`stable` toolchain of that day. The gate run's step summary records the candidate's version,
`rustc -V`, commit, tree and sha256, so a release can be compared with what was gated.

**Restore queued (exit 3 on `--restore`).** A production took the lease during the hour.

- The helper queued the restore. A root timer applies it about a minute after the lease ends, so
  no human action is needed.
- The job fails with "restore queued".
- Until the restore runs, a new gate run gets exit 6 ("previous restore still queued").

## Known side effects (camera-box conditions)

- Companion (the application) is never restarted, only `dantesync` through the helper.
- No run during a production: the rig lease (exit 3) and the night schedule cover this.
- The `dantesync` restart may page camera-box's `dantesync-clock-alert-watchdog`. This is known and
  accepted. The gate leaves the watchdog alone.

## Part 2 (open, waits on the owner)

Part 2 reads real Dante receivers' heartbeat records (`224.0.0.233:8708`, records `0x8003` and
`0x8004`). It needs 0 new late, and a per-second peak under the flow latency minus 100 µs. It also
needs a receiver subscription on a device that is not ours, so it goes through dstack to the owner
first. It is tracked on #98.
