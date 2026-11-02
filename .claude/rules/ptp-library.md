---
paths:
  - "crates/dsync-ptp/**"
  - "tests/library_acceptance.rs"
  - "Cargo.toml"
---

# The library form: `crates/dsync-ptp` and the in-process follower (dantesync#136, v1.24)

The owner ruled on 2026-11-02 ("build it"): dsync is the single maintainer of the Dante PTP code,
and apps lock to it in their own process. A fix in dsync must reach the embedded part. The design
comment on #136 has the full architecture and the crates that were rejected, with evidence:
statime and rtime-ptp are PTPv2-only, and statime also needs a delay exchange and has no Windows
network layer.

## Layout: one codebase, the service re-exports

- The cargo workspace has two members: the root package `dantesync` (the service and its bins)
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
  Two followers fed the same packets are bit-identical: the acceptance test reads 0.0 µs apart.
  So the follower-vs-service spread is the algorithm's difference, not receive jitter. It reads
  27 µs worst and 14 µs mean in the 30 s right after all three lock, while the boot pulls run.
- Stamp conversion (`mono::StampConverter`):
  - After EVERY receive, read raw / wall / raw and convert with that reading.
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
- Windows: `cargo check --target x86_64-pc-windows-gnu --workspace --tests --lib`. CI's Windows leg
  runs `cargo test --workspace`. The crate's `build.rs` delay-loads `wpcap.dll` for its own test
  binaries. A consumer app needs the Npcap runtime, and should delay-load `wpcap.dll` the same way.
