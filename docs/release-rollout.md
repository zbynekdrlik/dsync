# Release rollout order

dsync builds and releases. It does not deploy. camera-box pins a released version
(`DANTESYNC_VERSION_PIN`) and rolls it to the nodes; see `docs/HANDOVER.md`. This file records
the order and the checks a rollout needs, release by release, for whoever deploys. Hosts,
addresses, transports and credentials are not in this repository: they are in
`~/.config/dsync/TARGETS.md` and camera-box's own operations notes.

## The order

1. **Release.** Bump `Cargo.toml`, merge `dev` into `master`. CI's `auto-release` job tags
   `vX.Y.Z` and `release.yml` publishes `dantesync-linux-amd64`, `dantesync-windows-amd64.exe`,
   `dantesync-tray-windows-amd64.exe` and `dantesync-tray-linux-amd64`, each with a `.sha256`.
   A failure on either platform publishes nothing.
2. **Verify the assets once** against their `.sha256` files before any node gets them.
3. **One canary node first, never the NTP master.** Check both its `/status` on port 8898 and
   that the clock still syncs: the journal shows the normal `[PTP] PROD` then `[PTP] LOCK`
   progression (30-40 s after a restart is normal), then (1.23+) `[LOCK] === LOCKED === DSYX [41] = 1`
   once the phase error is within 50 µs (seconds later with a drift file, ~30-55 s later without
   one). Go on only once the canary is locked.
4. **The rest of the fleet, the NTP master placed as the release notes below require.** The
   NTP master is the node every other node's `ntp_server` points at.
5. **Final proof:** every node's `/status` returns 200 with `is_locked: true`.

## Per-release notes for step 4

- **From v1.9.0 (issues #88/#117): the fleet's NTP MASTER goes right after the canary**, before
  every other box. The master is the box every other box's `ntp_server` points at, i.e. the
  one running NTP server mode. Two reasons. Only an upgraded master publishes the fleet date
  offset, so until then every upgraded follower runs its local NTP date path. And an OLDER
  WINDOWS master cannot read the padded `"DSYX"` request (its 8-byte read buffer fails with
  `WSAEMSGSIZE`), so it logs a socket error per follower poll: 60 in each follower's first
  minute, then 2 a minute.
- Verify the master: `curl http://<master>:8898/status` shows `date_authority: "master"` and
  a `date_offset_ns`. Verify a follower after it: `date_authority: "follower"` with the same
  `date_offset_seq`.
- **From v1.10.0 (issue #119, backward corrections are SLEWED) the NTP master goes LAST** (the
  v1.9.0 master-first rule was about moving onto the authority at all; a 1.9.0 fleet already
  has one). A 1.10.0 master announces a backward correction as a slew in extension v2, and a
  1.9.0 follower reads only its v1 part: a backward STEP at the slew's start. While any box is
  still ≤ 1.9.0 it would split off the slewing fleet by up to the correction (≤ ~50 ms) for the
  slew's duration (~500 s). A 1.9.0 master with 1.10.0 followers publishes v1 announces, which
  every box applies as today's steps: uniform. Verify on the master after it:
  `date_slew_ppm` / `date_slew_active` / `date_slew_remaining_ms` appear in `/status`, and at
  the next backward correction the journal shows `[DATE] slew START` then `[DATE] slew DONE`
  on every box and no `[DATE] stepped -…` (a backward step appears only after a
  `date correction too large to slew` warning: a correction beyond 2 × the step bound).
- **From v1.11.0 (issue #119 follow-up, MICRO-corrections) the NTP master still goes LAST.**
  A 1.11.0 master corrects the date in increments of ≤ 500 µs (a step forward, a slew backward,
  about three a minute while the date is beyond its 2 ms dead band) and marks them MICRO in
  extension v3; a 1.10.0 follower ignores the flag and applies them as plain steps / slews (it
  only logs each one at info). Verify on the master after it: `/status` shows
  `date_correction_rate_ms_per_min` (≈ the grandmaster-vs-UTC drift once settled, e.g. ≈ 1.06),
  `date_micro_last_us` within ±500 and `date_correction_falling_behind: false`; the journal of
  every box shows `[DATE] stepped +…us (micro, seq N)` / `[DATE] micro-slew done` and never a
  `[DATE] stepped` above 500 µs (only after a `date correction too large …` warning: an error
  beyond 2 × the step bound).
- **From v1.12.0 (issue #119, the NIGHTLY step, `correction = "daily"` by default).** Only
  the master's behaviour changed. The nightly step is a plain coordinated step (extension v3,
  not micro), which every ≥ 1.9.0 follower applies in either direction. The order does not
  matter; keep the master LAST as for 1.11. Leave `system.date_offset.correction` out of the
  master's config (daily is the default). `"micro"` restores 1.11.
  - **Roll check on the master:** `/status` shows `date_correction_mode: "daily"`,
    `date_daily_next_utc` = the next `…T02:00:00Z`, and `date_correction_falling_behind` and
    `date_micro_paused` both `false`. The journal shows `[DATE] this NTP master is the fleet
    DATE-OFFSET AUTHORITY … corrected ONCE A NIGHT`, and no `[DATE] AUTHORITY:
    micro-correction` line.
  - **By day:** no `[DATE] stepped` on any box. The master's `date_offset_error_ms` grows at
    the grandmaster's drift (≈ +1.06 ms/min on the rig) and is no longer a health bound.
  - **After the first night:** the master's journal shows ONE `[DATE] AUTHORITY: nightly date
    step ±… ms` at the window. Every box shows one `[DATE] stepped ±…us (coordinated, seq N)`
    with the same seq. `date_steps_late` is 0, and `date_daily_last_step_ts` /
    `date_daily_last_step_ms` are set.
  - **Pre-announce check:** before the step lands, `date_step_pending_ns` (signed) and
    `date_step_due_in_ms` are set during its 10 s lead.
  - **A `nightly date step SKIPPED` line** means there were not enough fresh UTC readings (six
    in 5 minutes) for the whole window. The next night steps both days.
  - **A `nightly date step MISSED` line** means another date change was in flight all window,
    or the daemon stalled. An emergency step that jumps over a window is not reported. Only an error beyond `daily_emergency_ms` (5 s) is stepped by
    day, and it logs `date correction beyond the emergency cap`.
- **From v1.15.0 (issue #126, a master restart keeps the fleet date): followers first, the NTP
  master LAST, within ~40 minutes after the nightly step (02:00 UTC).** 1.14 saved nothing, so
  the FIRST start of a 1.15 master has no saved state and takes the old path once: the boot step
  to UTC (only if its error is > 50 ms) and a new session at seq 1. Right after the night's step
  the error is < 50 ms (+1.06 ms/min on the rig), so there is no boot step and every follower
  absorbs the new session (≤ 100 µs) instead of stepping; and upgraded followers HOLD the
  fleet date through the master's restart instead of stepping on their own NTP 30 s in.
  - **Roll check on the master:** its journal shows `[DATE] no saved fleet date offset at
    /etc/dantesync/date-offset.json` on this first start, then the file exists (it is written
    on every change of the authority). No follower journal shows `[DATE] stepped` around it.
  - **The acceptance (a daytime restart, after that):** `systemctl restart dantesync` on the
    master. Its journal: `[DATE] saved fleet date offset found … the boot step is skipped`,
    `[DATE] boot step of … SKIPPED`, then at its first lock `[DATE] RESTORED the fleet date
    offset …`; `/status` `date_offset_restored: true` with the SAME `date_offset_seq` and
    `date_offset_ns`, `date_offset_error_ms` unchanged apart from drift. Every follower: `[DATE]
    no applicable date-offset authority reply for 30s — HOLDING …`, `date_authority: "holding"`
    with `date_authority_hold_age_s` while the master is away, then `… heard again after
    holding …` and `"follower"`; NO `[DATE] stepped` and no `[StepClock]` anywhere.
  - **The coordinated step on request** (acceptance tests only): on the MASTER itself,
    `curl -s -X POST -H 'X-DanteSync-Step: 1' http://127.0.0.1:8898/date/step` → `202` +
    `{"accepted":true,"amount_ns":…,"land_ptp_ns":…,"due_in_ms":…,"seq":…}`; every box then logs
    ONE `[DATE] stepped +…us (coordinated, seq N)` at the instant. Refused with a reason (`409`)
    while another change is in flight, without a settled UTC estimate, or for a fleet ahead of
    UTC (never a backward step); `403` from a non-loopback peer, without the header or with a
    non-loopback `Host` (curl's own `127.0.0.1:8898` is fine); `503` = the loop did not take it
    within 3 s, the request was abandoned and NOTHING was announced; `500` with
    `"accepted": null` = the loop took it and gave no answer within 30 s — UNKNOWN, read
    `/status.date_step_trigger_last` (and the log) before asking again. A pre-1.15 build answers
    `200` + the status JSON: key on `"accepted"`. `/status.date_step_trigger_last` records the
    last one.
  - **Rolling the MASTER back to a pre-1.15 build: delete `/etc/dantesync/date-offset.json`
    (`C:\ProgramData\DanteSync\date-offset.json`) in the same step.** 1.14 does not know the
    file and starts a new session; a 1.15 installed again within a day would restore the
    session from BEFORE the rollback (same grandmaster, under a day old, within the 5 s cap)
    and every follower would re-join it with a step at its own poll. A node that starts as a
    non-master removes the file itself.
  - **camera-box:** its watchdog / handover grading of `date_authority` must accept
    `"holding"` (a follower whose master is restarting), and `DANTESYNC_VERSION_PIN` advances;
    its fleet upgrader should delete the file on a master downgrade.
- **From v1.16.0 (issue #119, the nightly step rounded to a grid-preserving quantum).** Only
  the master's behaviour changes: the nightly step is the UTC error rounded to a whole
  `system.date_offset.daily_step_quantum_ms` (200 by default; leave the key out). Followers
  apply what it announces, as before. A 1.15+ master restores its saved date offset on the
  upgrade's restart, so the fleet date does not move.
  - **Roll check on the master:** `/status` shows `date_daily_step_quantum_ms: 200` (`null` on
    every follower and in micro mode). The startup line reads `… one coordinated step of the
    UTC error rounded to whole 200 ms …`. With no startup warning naming
    `daily_step_quantum_ms` (a value that does not divide 1000 ms falls back to 200, loudly).
  - **After the first 1.16 night:** the journal line is `[DATE] AUTHORITY: nightly date step
    +1600.000 ms (the UTC error +1543.161 ms rounded to whole 200 ms, a coordinated step) …`.
    `date_daily_last_step_ms` is a whole multiple of 200, and `date_offset_error_ms` right
    after the step is within ±100 ms (the remainder, measured again the next night). An error
    with |error| < 100 ms (either sign; exactly ±100 ms steps ±200 ms) logs `nightly date step
    not needed` instead. Until that first 1.16 night a 1.15 master upgraded to 1.16 still
    reports its last unrounded step (`date_daily_last_step_ms`, restored from 1.15).
  - **camera-box:** its daily grading reads `date_daily_step_quantum_ms` additively, and
    `DANTESYNC_VERSION_PIN` advances (the camera-box supervisor's step, under the rig lease).
- **From v1.17.0 (issue #129 slice 0, the real grandmaster identity; report-only).** Any
  upgrade order: the master keeps announcing the old constant on 31900 and a 1.17 follower no
  longer compares the GM UUID, so no node's date-offset adoption changes. A 1.16 master's
  `date-offset.json` restores on the upgrade's restart.
  - **Roll check on every node:** `/status.gm_uuid` is a real Audinate UUID (`[0,29,193,…]`,
    never `[0,0,0,0,1,0]`). `ptp_version: 1`, `ptp_subdomain: "_DFLT"`, `ptp_source_uuid`
    set. The journal has one `PTP sender: PTPv1 …` line and the `Grandmaster UUID: … (stratum
    …, identifier "…", variance …, preferred …)` line.
  - **Record on issue #129:** each node's `gm_uuid`, `gm_source_ip`, `ptp_subdomain` and
    `ptp_source_uuid`, video VLAN AND audio VLAN (mbc, fohabl). That table decides the
    enforced filter and whether the two VLANs share one clock identity.
  - **Rollback of the MASTER below 1.17:** delete `date-offset.json` with it (a 1.17 master
    saves the real grandmaster, which a 1.16 master refuses: a boot step to UTC).
  - **camera-box:** `DANTESYNC_VERSION_PIN` advances. A consumer that grades `gm_uuid` sees it
    change from the constant to the real UUID once per node.
