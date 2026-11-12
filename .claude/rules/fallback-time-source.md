---
paths:
  - "src/fallback/**"
  - "src/controller/fallback.rs"
  - "src/controller/fallback/**"
---

# The dsync fallback time source (dsync#129 slice 2, v1.33)

On a network with NO PTP leader, one dsync node (by `system.fallback.priority`) multicasts its
time line on `239.255.77.1:31910`; the other dsync nodes follow it through the one sample
pipeline. Off by default. The decided rules (dstack ROZHODNUTÉ 2.11.2026 on #129) and the design
are on the ticket (issuecomment-6810218909, updated by issuecomment-6810625921).

## The rules and where each lives

- **Only with no leader:** `Election::tick` starts a source after `SILENCE` (= `PTP_TIMEOUT_SECS`
  through `ptp_rejoin::REJOIN_AFTER`) of no PTP time message from ANY source and no announce,
  plus `(255 − priority) × 20 ms`. The controller reports EVERY PTP time message
  (`ptp::is_time_message`: PTPv1 and PTPv2) BEFORE the gm_allowlist drop.
- **Step down on the first PTP message, no hold:** `process_loop_iteration` calls
  `fallback_note_ptp` right after `recv_packet` and BEFORE `service_fallback`, so a queued PTP
  message is read before the next Sync. The RED test is
  `the_source_steps_down_on_the_first_ptp_message_before_its_next_sync_129`.
- **No pre-emption:** a candidate never starts while any fallback source is live; only two
  sources running at once resolve (the lower rank yields). Rank = priority, then the lower id.
- **The never-send pin:** `wire::destination()` is the only destination, and
  `UdpFallbackTransport::open_on` refuses 224.0.1.129 and ports 319/320.

## What was learned building it

- **Do not stamp a fallback datagram from the sync loop.** With no PTP on the wire the Windows
  loop waits ~1 ms in the Npcap read every iteration, so a user-space stamp taken there is
  uniformly ms-late. The transport stamps on its own thread, blocked on the read.
- **The source's line must not move when its wall does.** It is `wall − D` in effect while
  anchored (`D` moves with every step AND every #119 slew), `wall − base` otherwise (`step_wall`
  adds every step to `base`). So the source keeps its normal NTP / date path: holding its NTP
  steps protected nothing and let its wall drift from UTC (review round 1).
- **The source takes the fleet's date like a follower, though it is offline** (review rounds
  2-3): `service_date_offset` lets it adopt announces (`same_time_base` decides: its `wall − D`
  is the base its followers announce in); an announce inside the absorb tolerance becomes a
  step of the same size on the source (D alone would jump the line); once adopted it takes no
  local NTP step (a step would leave the fleet line and the next announce would Join it back);
  a master that is the source stays on the fleet line like a daily master, in micro mode too
  (`master_holds_fleet_line_offline`, the ONE predicate for every such path: the NTP gate, its
  own increments in `tick_date_authority`, the re-join after a failed step). No outage
  re-align is needed for it when PTP returns: the whole fleet shares the source's free-run, and
  the sync-source change (`GmContinued` / `Rebased`) takes it.
- **A follower is "online" on the fallback.** `ptp_stale_at` stays "no allowed PTP" (re-join,
  clock alarm, receive counters); `time_source_stale_at` adds the followed source (offline edge,
  `/status`). A follower gets no PTP packet, so the fallback service runs the periodic NTP check
  the packet path would have run. The drift file is never saved on a fallback rate.
- **Entry and exit are ordinary sync-source changes** (`soft_reset_for_new_sync_source` + the
  GM-UUID change): a line within 1 ms of `D` is `GmContinued` (D kept), else `Rebased`; neither
  steps the wall. The follower test runs the hand-back through `process_loop_iteration` with
  `step_clock().never()`.
- **A node id is its sender's IPv4** (`D5:FB:` + IP): a datagram whose id is not
  `node_id(from)` is dropped; on Linux the receive socket is bound to the group, so unicast is
  never delivered.
- **Real sockets in tests:** `lo` loops multicast back whatever `IP_MULTICAST_LOOP` says (the
  device itself loops), so "the loop is off" cannot be tested on loopback; the election ignores
  its own id instead.
