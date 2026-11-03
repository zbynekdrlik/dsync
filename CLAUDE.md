# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Guidelines

- **FOCUSED APPLICATION:** This is NOT a general-purpose all-situations application. This is a highly focused app for our exact audio network synchronization use case. Code that adds complexity for unused features should be removed, not kept "just in case."
- Act as senior Rust, Windows, hardware, and clock-skilled developer
- Use TDD approach and ensure all code has test coverage
- **Critical Self-Review:** Be skeptical of your own conclusions. Before assuming something "doesn't work" or "isn't supported":
  1. Search online documentation and GitHub issues for evidence
  2. Verify from multiple independent sources (official docs, GitHub, Stack Overflow)
  3. Never make assumptions about API behavior without documentation
  4. If debugging, confirm the actual cause before implementing workarounds
  5. When you find contradicting evidence to your assumption, acknowledge the error immediately
- **Study Open Source Code:** When using open source libraries/frameworks:
  1. Read the actual source code to understand internal behavior
  2. Don't rely solely on documentation - verify by reading implementation
  3. If something doesn't work as expected, investigate the source to find why
  4. Consider fixing issues in the library itself rather than working around them
- **No Circular Development:** Never give up on a promising approach after first struggles:
  1. If an approach should theoretically work, investigate WHY it doesn't instead of reverting
  2. Don't cycle between approaches (try A → fail → try B → fail → try A again)
  3. Commit to achieving the goal (e.g., Linux-level 50µs precision) - don't settle for inferior solutions
  4. If a library has issues, consider contributing fixes rather than abandoning it
- **HARD REQUIREMENT - Precision Target <50µs:**
  1. The ONLY acceptable precision for both Linux AND Windows is <50 microseconds
  2. NEVER accept, propose, or implement solutions with worse precision (100ms, 1ms, etc.)
  3. If current approach shows precision worse than 50µs, it is FAILING - do not present it as "working"
  4. Before trying new approaches, RESEARCH how other projects (PTPSync, Meinberg, ptpd) achieve <50µs on Windows
  5. Consult with user before switching approaches - do not endlessly iterate without results
- **Test Quality:** All code must have 100% test coverage with high-quality, complex E2E tests. Never put out a broken version. The `tests/simulation_e2e.rs` contains critical simulation tests that validate the servo and controller behavior under various conditions.
- **CI/CD Verification:** Wait until GitHub Actions CI/CD pipeline has successfully finished (green checkmark) before telling the user to update or run commands. Monitor `gh run view` until completion
- **Deployment:** Never deploy to a node on your own. The rules in `docs/HANDOVER.md` apply: the camera rig belongs to camera-box, and requests come through dstack. Site facts are in `~/.config/dsync/TARGETS.md`.
- **STRICT CI - Fight Regressions:** CI is configured to be maximally strict to prevent regressions:
  1. **Version bump required**: CI blocks merging if Cargo.toml version matches an existing release tag
     - ALWAYS bump version before merging ANY change to master — including docs-only /
       process-only commits (e.g. an `autopilot-log.md` entry) with zero functional change. The
       gate checks the version string only; it does not know or care that your diff is "just
       docs" (#61 confirmed this live: a docs-only follow-up PR still needed its own bump and
       produced a real tagged release).
     - This prevents shipping different code under the same version number
  2. **Coverage threshold**: Minimum 60% project coverage, max 1% drop allowed, new code requires 80% coverage
  3. **Security audit**: Blocks CI on ANY security advisory (`cargo audit --deny warnings`)
     - Known transitive dependency advisories are explicitly ignored with `--ignore RUSTSEC-XXXX` after review
     - When adding ignores, document WHY it doesn't affect our code (e.g., we don't use the affected API)
     - NEW advisories will still fail CI - only reviewed/documented ones are ignored
  4. **All checks must pass**: Lint, tests, build (Linux + Windows), coverage, security - ALL must be green
  5. Never weaken CI checks. If a check fails, fix the code, don't disable the check
  6. When adding features, always add corresponding tests to maintain coverage

## Branch Policy

- **EXACTLY two branches:** `master` and `dev`. No other branches — no feature branches, no fix branches, no release branches.
- **All work happens on `dev`:** Commit directly to `dev`, then open a PR from `dev` to `master` when ready to release.
- **No direct pushes to master:** All changes to `master` must go through a PR merge from `dev`. Branch protection enforces this for all users including admins.
- **Never create additional branches.** If a branch other than `master` or `dev` exists, it is a mistake and should be deleted.

## Build & CI

`ci.yml` runs on every PR: Lint (`cargo clippy -- -D warnings -A dead_code`, NOT `--all-targets`),
`cargo test --lib` + `--test '*'`, and a real Linux+Windows compile. `.github/workflows/release.yml`
builds and publishes `dsync-linux-amd64`, `dsync-windows-amd64.exe` and
`dsync-tray-windows-amd64.exe` on every tag push, all-or-nothing via `needs: build` (a failure
on either platform skips `publish` — no release with a missing asset).

- `clippy::doc-lazy-continuation` fires when a wrapped `///` doc line starts with a markdown list
  marker (`+`/`-`/`*` or `N.`) — keep list markers off wrapped-line starts.
- `scripts/purge-target.sh` (a `pre-push` hook via `scripts/install-git-hooks.sh`) purges `target/`
  past a size budget (cargo has no `target/` GC, rust-lang/cargo#5026); it skips while the
  dsync daemon/tray is running.

**Windows builds need no Npcap SDK** since 1.27 (#136): `crates/dsync-ptp/src/wpcap.rs` loads `wpcap.dll` at run time.

## Playbook router

- stability gate (#98): `gate.yml` on the `companion` runner, the pass rule, and the
  tree-hash release coupling in `auto-release`. The gate must be green on dev's tree BEFORE merging
  to master, or the release is refused → `docs/stability-gate.md`
- The library form (#136, v1.24): `crates/dsync-ptp` holds the shared PTP code and the in-process
  follower. It covers the workspace and re-export layout, the extracted `RateServo` steps, the
  virtual clock over the RAW counter, the stamp conversion that drops a packet straddling a wall
  step, why followers on one PC read bit-identical times, and the real-socket acceptance test
  beside the service → `.claude/rules/ptp-library.md` (auto-loads on `crates/dsync-ptp/**`)
- Windows-only (`#[cfg(windows)]`) code — what CI actually verifies + a free local compile-check
  → `.claude/rules/windows-only-code.md` (auto-loads on its `paths:`)
- Editing `.github/workflows/*.yml` — local `actionlint` validation, and why a workflow being
  syntactically valid doesn't prove a new step actually works at runtime → `.claude/rules/github-workflows.md`
  (auto-loads on its `paths:`)
- Shell-script testing (`scripts/**`) — the source-guard + curated-PATH conventions from #61's
  purge-target backstop, and a clippy-scope gotcha → `.claude/rules/shell-script-testing.md`
  (auto-loads on its `paths:`)
- Clock discipline + how to test a control loop (closed-loop mocks vs constant ones, MAD models
  jitter not a drift ramp, Instant vs SystemTime on a daemon that steps its own clock, the
  additive-only `/status` contract) → `.claude/rules/clock-discipline-and-testing.md`
- PTP phase lock + fleet date offset (#117/#88 — rate AND phase from PTP, the NTP master's
  coordinated date announce on 31900, the #119 backward correction as a decoupled coordinated
  SLEW, the v1.11 MICRO-corrections (≤ 500 µs per 20 s beyond a 2 ms dead band, decided on the
  master's loop from a robust drift+level estimate; a large step only beyond 2 × the step bound),
  the decoupling proof by the bit-identical two-clock bench,
  the local fallback, seeding every simulated noise source; v1.12: the date corrected ONCE A NIGHT by default, `correction = "daily"`,
  the micro mode kept byte-identical, the master's own PTP outage in daily mode; v1.14: a
  grandmaster FREQUENCY step followed in seconds — the open-loop-phase detector, the linearity
  test and the shifted slope against level shifts, the integrator re-seed, the decaying phase pull
  and its applied-rate transient; v1.15: a master restart keeps the fleet date — the saved
  authority state, the restore judged at the first PTP window, the follower hold, the loopback
  `POST /date/step`; v1.19: the fleet date placed at the FIRST PTP window with one step, the boot
  NTP reading off the loop, the first engagement holding the early anchor through the pull) →
  `.claude/rules/clock-discipline-and-testing.md` (auto-loads on
  `crates/dsync-ptp/src/ptp_phase_lock.rs` / `src/date_offset.rs` / `src/time_server.rs` / their `tests.rs` /
  `tests/two_clock_bench.rs` / `tests/simulation_e2e.rs`)
- DSYX [41] = the phase lock within the bound (#146, v1.23 — `crates/dsync-ptp/src/lock_gate.rs` 50 µs in / 150 µs
  out, the rate servo decoupled from the phase lock's own deviation, the boot fit and its once-only
  re-seed, the whole-controller closed-loop sim) → `.claude/rules/clock-discipline-and-testing.md`
  (auto-loads on `crates/dsync-ptp/src/lock_gate.rs` / `crates/dsync-ptp/src/ptp_phase_lock/boot_fit.rs` / `controller/date_sync/lock_*`)
- Cold-boot acquisition + the drift file (#142, v1.22 — a window without a previous offset is not a
  measurement; `drift.json` saved only while LOCKED, the seed judged against the SEED itself on the
  first 4 measured windows; ctrlc `termination` so a systemd stop runs the shutdown save) →
  `.claude/rules/clock-discipline-and-testing.md` (auto-loads on `src/drift_file.rs` /
  `src/controller/freq_seed.rs`)
- Phase-slew PI servo (#97, LEGACY discipline only since #117 — the feed-forward decoupling sign invariant, the deadbeat gain cap for
  the client's slow cadence, the I-deadband, verify-by-simulation, the canary re-tighten proof) →
  `.claude/rules/phase-slew-servo.md` (auto-loads on `src/phase_slew.rs`)
- Adding a config key / loading `config.json` (the `serde_json` `IndexMut` startup-panic trap,
  serde defaults, flooring nonsense values, #116: a failed parse never writes the file, the
  last-good copy + alarm, BOM decoding; #147: the state dir is resolved by `src/state_dir.rs`, never
  copied, never a fresh `const` path) → `.claude/rules/config-migration.md`
- PTP receive interface selection (#131: `system.interface` / `--interface` PINS the NIC on every
  platform — wait for it, never another NIC, alarm naming it; unpinned: net.rs/net_pcap.rs/gm_filter.rs — pick the NIC on the
  trusted GM subnet, reuse the #53 selector, pure-logic-in-gm_filter vs Windows-glue split, overlap
  semantics + ambiguity guard) → `.claude/rules/multi-homed-interface-selection.md`
- PTP liveness (#112 — one definition of "PTP is stale", the honest `/status` while stale
  (`is_locked=false`, `mode="NTP-only"`, `last_ptp_rx_age_s`/`ptp_rx_pps`/`rejoin`), the
  `PtpNetwork::rejoin` contract (re-open only the receive path, close first, the home address
  first then the startup selection, never a clock/servo/date touch), why the home address (a
  re-plugged NIC's new ifindex loses to tailscale/docker in the listing order), the one
  `ptp::is_time_message` predicate for liveness / `gm_source_ip` / the home, the pure
  10→30→60→120→300 s schedule, the finding that `D` is anchored once per process, the
  per-socket-loopback-address test trick) →
  `.claude/rules/ptp-liveness-rejoin.md` (auto-loads on its `paths:`)
- PTPv1 wire format + the grandmaster identity (#129 — the 40-byte header proven on real PTPv1
  bytes in `crates/dsync-ptp/tests/fixtures/ptpv1/`, the Follow_Up offset that was right only by accident, the
  pre-1.17 misread constant `00:00:00:00:01:00` and its three consumers (the restart restore, the
  report-only follower check, the constant kept on the 31900 wire), report-only sender fields,
  a read-only tcpdump capture on dev1) →
  `.claude/rules/ptpv1-wire-format.md` (auto-loads on its `paths:`)
- DSCP marking of timesync sockets (#52 — which sockets are markable per platform: Linux ntp_server
  reply YES, Linux rsntp client NO handle, Windows filters IP_TOS; the `dscp<<2` TOS-byte math;
  fail-open; socket2 0.5.10 set_tos) → `.claude/rules/dscp-marking.md` (auto-loads on its `paths:`)

## GOTCHA — `gh pr edit --body-file`/`--body` fails with a GraphQL "Projects (classic)" error

`gh pr edit <N> --body-file <file>` on this repo fails with `GraphQL: Projects (classic) is being
deprecated... (repository.pullRequest.projectCards)` and exit code 1 — `gh`'s GraphQL mutation for
editing a PR fetches a legacy `projectCards` field even when you never touch project cards, and
this repo/org still has that field wired up. **The body is silently NOT updated** when this
happens — always re-read the body afterward to confirm, since the CLI's own error output alone
doesn't make the silent no-op obvious. The direct REST PATCH sidesteps the broken GraphQL response
and works every time:

```bash
gh api repos/zbynekdrlik/dsync/pulls/<N> -X PATCH -F body=@/path/to/new-body.md
```

Same for `gh issue view <N>` (also fails the same way — use
`gh api repos/zbynekdrlik/dsync/issues/<N> --jq '{title,body,state}'` instead).

**`gh issue comment <N> -F <file>` (POSTING a new comment) is unaffected** — confirmed working
live (#61) with no GraphQL error. Only editing/viewing an existing PR/issue body hits the
`projectCards` bug above; posting a fresh comment does not touch that field at all.

## GOTCHA — the design/validated/reviewed-comment gate's repo resolution ignores an in-command `cd`

The airuleset design-gate hook (`hooks/post-record-design-comment.sh`, external to this repo) that
classifies a posted `gh issue comment` as a design/validated/reviewed marker resolves which repo
the comment belongs to from the **session's own ambient working directory**, not from a `cd
<path> &&` prefix inside the SAME Bash command. A worker whose session launched in a sibling repo
(e.g. `camera-box`) and runs `cd /home/dsync/devel/dsync && gh issue comment 71 -F
body.md` — even though the comment genuinely posts to the right repo (confirmed by the returned
`.../dsync/issues/71#issuecomment-...` URL) — has that comment silently classified against
the WRONG repo (or not at all), because the hook's own `cwd` metadata never followed the `cd`.
The commit-blocking gate (`hooks/block-commit-without-design.sh`) then fires as if no design
comment exists at all, even though one is genuinely posted and readable on the issue.

**This recurred across two separate work cycles** (issue 68's cycle, and issue 71's cycle) before
being promoted here — it is not a one-off. **Always pass `-R zbynekdrlik/dsync` explicitly on
every `gh issue comment` call that posts a design/validated/reviewed marker**, regardless of
whether the session is already `cd`'d into this repo:

```bash
gh issue comment <N> -R zbynekdrlik/dsync -F body.md
```

`-R` makes the hook use the explicit repo directly, bypassing the ambient-cwd resolution entirely.
**Use the CURRENT repo name `zbynekdrlik/dsync` (renamed in the #146 cycle): the hook names
its markers after the `-R` repo's NAME, and the commit gate looks for `dsync` markers, so
`-R` with the OLD name (GitHub still redirects it) posts the comment but records the marker
under the wrong name and the commit stays blocked.** The commit gate also wants, for a
non-trivial ticket, numbered approaches (`Approach 1 (chosen)` … `Approach 3`) and an
`Architektúra:` line that names the structure AND the framework (or why none fits); a design
comment without them is rejected with exactly that message.
If a commit is blocked despite a comment you're sure is posted, check
`ls ~/.claude/design-posted/ ~/.claude/validated-posted/ ~/.claude/reviewed-posted/ | grep
dsync` for the missing marker, then simply repost the SAME comment with `-R` explicit (a
duplicate comment on the issue thread is a harmless cosmetic cost next to a stuck commit gate) —
each `gh issue comment` call only ever classifies its own invocation's LATEST fresh comment on
that issue, so post design/validated/reviewed comments as SEPARATE Bash calls too, never batched
together in one command with `&&`.

**The SAME family of gotcha hits `git commit` itself too (issue 80's cycle), with a different,
narrower fix.** `hooks/block-commit-without-design.sh`'s own `resolve_work_cwd()` DOES honor an
inline `cd <path> &&`/`cd <path>;` — but ONLY when it is the LITERAL FIRST statement of the SAME
Bash tool call the `git commit` itself runs in. A shape like:

```bash
cd /home/dsync/devel/dsync && git add file1 file2
git commit -m "$(cat <<'EOF'
...
EOF
)"
```

— i.e. `cd && git add` on one line, then a NEWLINE, then `git commit` on the next line — still
executes correctly (both commands genuinely run against the right repo; shell `cd` state persists
across newline-separated statements within one Bash call) but gets BLOCKED anyway: the hook's own
repo resolution apparently does not associate a `cd` that preceded an EARLIER, different statement
with a `git commit` appearing later in the same multi-line call. The fix is to chain the `cd`
directly through to the `git commit` invocation itself via literal `&&`, all on one shell line (a
heredoc's OWN body lines are part of the SAME statement, not new top-level ones, so they don't
break this):

```bash
cd /home/dsync/devel/dsync && git add file1 file2 && git commit -m "$(cat <<'EOF'
...
EOF
)"
```

If a commit is blocked and reports the WRONG repo name in its own error message (e.g. "no design
comment posted yet for #N (repo camera-box)" while working in dsync), check `pwd` first — a
worker's session `cwd` can be a sibling repo's checkout even mid-session, and this shape is the
fix, not `-R` (which `git commit` has no equivalent flag for at all).

## GOTCHA — `block-ungated-issue-filing.sh`'s `-F <file>` resolution reads the RAW command text, a shell `$VAR` in the path never resolves

Cross-repo lesson from filing camera-box#1021 while working this issue (#83): the airuleset
`Scope-gate:`/`Dedup-checked:` enforcement hook (`hooks/block-ungated-issue-filing.sh`, external to
this repo, fires on ANY `gh issue create`/`gh api .../issues` call in ANY project) resolves a
`-F <file>` / `--body-file <file>` argument by reading the **literal, un-expanded text** of the
Bash tool call it receives — it never runs a real shell, so it cannot expand a variable. A command
like:

```bash
SCRATCH=/tmp/some/scratchpad
gh issue create -R owner/repo --title "..." -F "$SCRATCH/body.md"
```

makes the hook see the literal token `$SCRATCH/body.md` (dollar sign and all) as the file path —
not the shell-expanded absolute path — so `os.path.isabs()` on it is `False`, the hook tries
`os.path.join(cwd, "$SCRATCH/body.md")`, that path does not exist, the body resolves to `None`, and
the whole call BLOCKS with `criterion=none dedup="none"` **even when the body file genuinely exists
on disk with correct `Scope-gate:`/`Dedup-checked:` lines** — the block message gives no hint this
is the actual cause (it lists the same generic 7 possible reasons regardless).

**Fix: write the scratch body file (its own separate Bash call, per the global `gh-cli-recipes.md`
atomic-block gotcha) and then reference it in `gh issue create -F` by its LITERAL, fully-expanded
absolute path** — never a `$VAR` interpolation in that specific argument, even though the variable
resolves correctly for every OTHER purpose (the file write itself, `cat`, etc. all work fine since
those genuinely execute in a real shell). This is a narrower instance of the general "hook sees your
command's TEXT, not its runtime-expanded result" class of gotcha already documented above for the
design-gate's `cd` resolution.

## Hardware Constraints (CRITICAL)

**This project implements SOFTWARE-ONLY PTP frequency synchronization:**

- NONE of the target computers have NICs with hardware timestamping support
- Standard consumer/enterprise Ethernet NICs (Intel, Realtek) are used
- DO NOT waste time on approaches requiring hardware timestamping (SIO_TIMESTAMPING, PTP hardware clocks, etc.)
- The goal is to achieve <50µs precision using SOFTWARE timestamps only
- Linux achieves this with kernel-level SO_TIMESTAMPNS - Windows needs equivalent software approach
- Audinate's audio drivers are NOT involved - we use standard Windows/Linux network stack

## Architecture Overview

dsync is a high-precision PTP (Precision Time Protocol) synchronization tool for audio networks. It implements PTPv1 over UDP multicast (ports 319/320) with a hybrid NTP+PTP approach.

### Core Components

Since v1.24 (#136) the PTP code lives in the workspace crate `crates/dsync-ptp`: the files below
marked (crate). The service re-exports them, so `crate::ptp::…` paths are unchanged. Applications
link the same crate's `follower` to lock to the grandmaster in their own process.


- **`main.rs`** - Entry point, CLI parsing, Windows service logic, IPC server setup, and main sync loop orchestration
- **`controller.rs`** - `PtpController<C, N, S>` - Generic controller that coordinates PTP sync. Contains rate-based servo logic, mode transitions (ACQ→PROD→LOCK), and clock adjustments
- **`ptp.rs`** - PTPv1 packet parsing (headers, Sync bodies, FollowUp bodies)
- **`clock/mod.rs`** - `SystemClock` trait with platform-specific implementations:
  - `linux.rs` - Uses `adjtimex` for frequency adjustment
  - `windows.rs` - Uses `SetSystemTimeAdjustmentPrecise` API
- **`traits.rs`** - `NtpSource` and `PtpNetwork` traits (mockable for testing)
- **`config.rs`** - `SystemConfig`, `ServoConfig`, `FilterConfig` - tuning parameters with different defaults for Linux vs Windows
- **`net.rs`** / **`net_linux.rs`** / **`net_pcap.rs`** / **`net_winsock.rs`** - Network utilities (multicast, timestamping) and the platform PTP receive paths (Linux kernel-timestamped UDP, Windows Npcap), each with `PtpNetwork::rejoin` (#112)
- **`ptp_rejoin.rs`** - The pure PTP re-join schedule, the receive-rate window and `/status.rejoin` (#112); wired by `controller/ptp_liveness.rs`
- **`ntp.rs`** - NTP client for UTC alignment
- **`ntp_burst.rs`** - The periodic NTP burst on its own `ntp-burst` thread (#136); `controller/ntp_check.rs` starts it when the check is due and applies its result
- **`sync_loop.rs`** - The sync loop `main` drives the controller through, and the UDP 31900 time server's own `time-server` thread (#136)
- **`status.rs`** - `SyncStatus` struct shared via IPC to tray app (includes `is_locked`, `smoothed_rate_ppm`, `mode`)

### CRITICAL: PTP Time vs UTC Time

**The grandmaster's PTPv1 time is DEVICE UPTIME, not UTC time.** This is fundamental to the architecture:

- The grandmaster clock uses device uptime (time since power-on), NOT real UTC.
- The owner contract (#117): **RATE = the PTPv1 tick only; NTP = date stepping only.**

**The default discipline (`system.clock_discipline = "ptp_phase_lock"`, #117 + #88):**

1. **PTP (the grandmaster)** → `adjust_frequency()`: rate AND phase. Once PTP-locked, `crates/dsync-ptp/src/ptp_phase_lock.rs`
   holds `wall = PTP time + D` with a PI on `e = (t2 − t1) − D`. Every box ticks with the GM and
   shows the same wall time (µs). A GM change re-anchors `D` with no wall step.
2. **NTP (UTC)** → the fleet date offset `D` only. The NTP master alone reads UTC and is the
   authority (`src/date_offset.rs`): when |UTC − wall| > 50 ms it announces a new `D`, ≥ 5 s ahead,
   in the versioned extension of the UDP 31900 time-query reply. Every box `step_clock()`s at
   exactly that instant. Followers never step on their own NTP reading.

Without an authority (older master, other grandmaster, PTP offline) a box falls back to the NTP
step path for the date, still with a pure-PTP rate. `"legacy"` restores the pre-#117 rate-only
servo + NTP steps + `phase_slew`. The old servo was rate-only and `initial_epoch_offset_ns` was
never read: see `.claude/rules/clock-discipline-and-testing.md`.

### Sync Flow

1. NTP sync for initial coarse UTC alignment
2. Join PTP multicast groups (224.0.1.129 on ports 319/320)
3. Process Sync messages → store pending with receive timestamp
4. Match FollowUp messages → calculate phase offset from (T1, T2) pair
5. Lucky packet filter selects minimum offset from sample window
6. PI servo calculates frequency adjustment in PPM
7. Platform clock adjusts system frequency (PTP controls rate only)
8. Periodic NTP checks maintain UTC alignment (NTP controls absolute time)

### Key Design Patterns

- **Generic controller**: `PtpController<C: SystemClock, N: PtpNetwork, S: NtpSource>` allows dependency injection and mocking
- **Lucky packet filtering**: Selects minimum offset from N samples to filter network jitter
- **Platform abstraction**: `clock/mod.rs` re-exports `PlatformClock` based on target OS

### Configuration

Config file locations:

- Linux: `/etc/dsync/config.json`
- Windows: `C:\ProgramData\dsync\config.json`

Since 1.25 (#147) the state dir is `/etc/dsync` (`C:\ProgramData\dsync`). A box installed before
1.25 keeps its old state dir in place until a deployer renames it; `/status.state_dir` names the one
in use (`src/state_dir.rs`).

Key tunable parameters (in `config.rs`):

- Servo gains: `kp`, `ki` (reference only - controller uses adaptive gains)
- Filter settings: `sample_window_size`, `min_delta_ns`, `calibration_samples`, `warmup_secs`

### Binaries

- `dsync` - Main sync daemon/service
- `dsync-tray` - Windows tray application:
  - Dynamic icon with pulsing ring based on drift rate
  - Toast notifications for state transitions (lock/unlock/offline)
  - Service control (Restart/Stop) via menu
  - Reads status via named pipe IPC (`\\.\pipe\dsync`)
