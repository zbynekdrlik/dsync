---
paths:
  - "crates/dsync-ptp/**"
  - "tests/library_acceptance.rs"
  - "Cargo.toml"
---

# The library form: `crates/dsync-ptp` and the in-process follower (dsync#136, v1.24)

The owner ruled on 2026-11-02 ("build it"): dsync is the single maintainer of the PTPv1 code,
and apps lock to it in their own process. A fix in dsync must reach the embedded part. The design
comment on #136 has the full architecture and the crates that were rejected, with evidence:
statime and rtime-ptp are PTPv2-only, and statime also needs a delay exchange and has no Windows
network layer.

## Layout: one codebase, the service re-exports

- The cargo workspace has two members: the root package `dsync` (the service and its bins)
  and `crates/dsync-ptp`. Consumers pin the crate as
  `dsync-ptp = { git = "https://github.com/zbynekdrlik/dsync", tag = "vX.Y.Z" }`.
- The crate version equals the service version. A test pins it:
  `the_follower_library_runs_the_service_window_defaults_and_its_version_136` in
  `src/config/tests.rs`. Bump both `Cargo.toml`s together. The CI version check reads only the
  root.
- The moved modules are re-exported in `src/lib.rs` (`pub use dsync_ptp::ptp;` …), in `traits.rs`
  (`PtpNetwork`, `RejoinOutcome`), in `net_pcap.rs` (`pub use dsync_ptp::net_pcap::*`, the NTP
  transport stays in the service) and in `ntp_packet.rs` (`parse_udp_frame`). A path like
  `crate::ptp_phase_lock::…` in the service still works. EDIT the code in the crate.
- `MockPtpNetwork` comes from the crate's `mock` feature. Only the service's dev-dependency enables
  it, so a consumer never links mockall.
- A test inside the crate cannot use the service. The service + library acceptance test lives in
  the ROOT `tests/library_acceptance.rs`.
- CI runs clippy, test and tarpaulin with `--workspace`. Without it the moved code would drop out
  of the lint, the tests and the coverage.

## The rate servo is shared, step by step

`dsync_ptp::rate_servo::RateServo<T>` holds the old controller fields. The controller reaches them
as `self.rate.is_locked`, `c.rate.drift_baseline_ppm`, and so on. Its four steps are the former
blocks of `apply_self_tuning_servo`, called in that order:

1. `measure_offset`
2. `measure`
3. `update_modes`
4. `word`

The service-only hooks run between the steps, in the controller: the legacy phase_slew decoupling,
the #142 seed judge and the slew de-slew. The #146 decoupling (`WordDeviation`) and the hold
(`word(.., hold_lock)`) are shared. A servo fix goes into the crate, and both consumers get it.

The servo is generic over its time stamp. `Instant` in the service keeps the controller's
arithmetic bit-for-bit. `MonoNs` lets the follower and its simulations run without wall time.

## The follower: a virtual clock over the RAW counter

- `V(m) = v₀ + (m − m₀)(1 + f)` over `CLOCK_MONOTONIC_RAW` (Linux) or QPC (Windows). It is never
  `CLOCK_MONOTONIC`: `adjtimex` slews that one, so the follower would read the service's own word
  as a disturbance.
- The published PTP time is `V − D_v`. This is the same quantity as the service's `wall − D`:
  both hold `t2 − t1` at their anchor, so both trail the grandmaster by the same receive latency.
- **Every receiver on one PC gets the SAME kernel stamp.** Multicast delivery clones one skb.
  Two followers fed the same packets differ only by float rounding (their virtual clocks have
  different origins): the acceptance test reads 0.0 µs apart and asserts ≤ 1 µs.
  So the follower-vs-service spread is the algorithm's difference, not receive jitter. It reads
  27 µs worst and 14 µs mean in the 30 s right after all three lock, while the boot pulls run.
- Stamp conversion (`mono::StampConverter`):
  - After EVERY receive, read raw / wall / raw. Convert with `wall − raw` INTERPOLATED at the
    stamp between the previous reading and this one (`TimeBaseReading::to_mono_between`, v1.27.1).
    Converting with the after-reading alone is off by the host slew × the read delay, so two
    followers disagreed by 4.5 µs on a CI runner with a slewing wall clock (run 41558600507),
    while dev1 read 0.0 µs. A follower-vs-follower failure that only CI shows = a host-slew bug,
    not noise.
  - `wall − raw` moves only by the clock owner's word (≤ 500 ppm).
  - Beyond 20 µs + 500 ppm of the elapsed time, it is a step: drop that packet and the pending
    Syncs.
  - A reading taken BEFORE the receive is wrong for a queued packet. The step may land between
    that reading and the packet's arrival.
- Differences from the service pipeline, each justified on the virtual clock:
  - no warmup and no calibration;
  - no date layer;
  - a time-base change also drops the rate servo's previous offset. The raw `t2 − t1` jumps by the
    base shift, whereas the service's mod-1 s phase stays bounded.
- `locked` is [41]'s definition: the shared `PhaseLockGate`, cleared while stale.
  - Lock is SET at |e| ≤ 50 µs, and the boot pull keeps retiring the rest after it.
  - A test must not assert "≤ 50 µs from the lock on". The model reads up to ~50 µs + the mean
    receive jitter for the first windows, ≤ 20 µs once settled.
  - A rebase keeps the lock (e = 0 at the re-anchor).
  - A 10 ppm grandmaster frequency change drops it once and it returns, as on the service.
- `epoch` counts time-base changes (`Rebased`). `reanchors` counts every discontinuity
  (`Rebased` + `Realigned`). A consumer re-syncs on either.
- Holdover: no allowed time message for `REJOIN_AFTER` (10 s). The follower then holds the
  integrator, drops the measurement and clears the gate. The re-join follows `ptp_rejoin`'s
  schedule.

## Testing it

- Pure: `follower/core/tests.rs` is a closed-loop world. The grandmaster and the raw counter each
  run at their own rate, each receiver has its own seeded jitter, and the line is judged at the
  latency.
  - **Keep the PTP origin timestamps positive.** `ptp_tx` clamps a negative time to 0 (it is
    u32 seconds on the wire). A grandmaster change built as `base − 4 days` from a 1 h uptime
    produced `t1 = 0` and a nonsense 25 ms/s error.
- Real sockets: 319/320 need root, so the tests map 319 to `base` and 320 to `base + 1`
  (unprivileged ports) through a `PtpSocketFactory`. Everything else is the production
  `net::create_multicast_socket`. Multicast on `lo` works with the sender's
  `IP_MULTICAST_IF = 127.0.0.1`, loop on and TTL 0 (TTL 0 never leaves the host).
- The acceptance test drives the real `PtpController` through `process_loop_iteration`. A test
  must never steer the host clock, so the controller's `SystemClock` is a virtual WALL over the raw
  counter, and its `PtpNetwork` moves each kernel stamp onto that wall. All three receivers lock
  in ~52 s. The test takes ~82 s.
- Windows: `cargo check --target x86_64-pc-windows-gnu --workspace --tests --lib --examples`.
  CI's Windows leg runs `cargo test --workspace` with NO Npcap SDK or runtime installed.

## Windows: Npcap at run time, a typed start error, the probe (v1.27)

- `wpcap.rs` is the ONE owner of the Npcap API (the PTP capture and the service's NTP capture).
  It loads `System32\Npcap\wpcap.dll` with `LOAD_WITH_ALTERED_SEARCH_PATH` (so `Packet.dll`
  resolves beside it), then the default search, and resolves 15 functions with `GetProcAddress`.
  No `pcap` crate, no SDK, no `/DELAYLOAD`. A new libpcap call is a new `Api` field + one
  `resolve!` line. `struct timeval` is two 32-bit longs on Windows (a layout test pins it).
- `pcap_activate` returning a WARNING is an error, as the `pcap` crate did: a capture without
  `HOST_HIPREC` stamps is not one the clock can use.
- `Follower::start` → `FollowerError`. The interface is judged first (`InterfaceNotFound`), then
  Npcap (`NpcapMissing`), then the open (`from_open` reads the anyhow chain: an
  `io::ErrorKind::PermissionDenied` anywhere is `PermissionDenied`). A loopback never counts as a
  pinned interface (#131), so a test of the Linux bind uses `net::get_default_interface()`.
- `examples/dsync-ptp-probe.rs` (`test = true` in `[[example]]`, so its tests run in
  `cargo test`) is a release asset on Windows. It compares the follower at the DSYX reply's raw
  counter instant (`[16-23]` / `[48-55]`) with the reply's `now_ptp_ns` (`[96-103]`). dsync has
  no fleet access: dstack runs it on a node and posts the output on #136.

## The seqlock: a read counts only if the active index is unchanged after it (dsync#150, v1.26)

`crates/dsync-ptp/src/seqlock.rs` is double-buffered with one writer. A reader loads `active`,
then reads that slot. If the reader stalls, the writer can publish the other slot and then
COMPLETE a newer write into the reader's slot, not yet published. The slot's own sequence is even
and stable, so a per-slot check passes, and the reader returns a state newer than the published
one. Its next read returns the older published state: time goes back. `try_read_at` therefore
re-loads `active` after the slot read and retries when it moved. The regression test
`a_stale_index_never_returns_a_complete_but_unpublished_write` drives that interleaving
deterministically. The concurrent stress test only caught it on CI's slower Coverage runner; to
reproduce locally, pin it to two cores (`taskset -c 0,1`) and loop it.
