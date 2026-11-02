# The DSYX lock bytes: what `[41]` and `[62]` bit 1 mean

The 31900 time server's reply (`"DSYN"` or `"DSYX"`, layout in `src/time_server.rs`) carries two
bytes that consumers gate on. dstack, iemmixer, fohmixer and songplayer read them through the DSYX
contract (`docs/HANDOVER.md`). The same values are `/status.is_locked` and `/status.settled` on
`:8898`, and the tray and the IPC show `is_locked`: one definition.

## `[41]` — locked (dsync #146, since 1.23.0)

`[41]` = 1 means the clock is safe for a 250 µs Dante flow. dstack gates its media-clock ready on
this byte alone; while the stack runs, `[41]` = 0 is a holdover (not a mute), and before its first
ready the stack holds its TX back.

Under the default `system.clock_discipline = "ptp_phase_lock"` it is the phase error
`e = (t2 − t1) − D` (`/status.ptp_phase_error_us`) against two bounds, judged once per PTP window
(0.5 s):

| | Condition |
|---|---|
| Set | the phase lock owns the frequency word and \|e\| ≤ 50 µs for 3 consecutive windows |
| Cleared | \|e\| > 150 µs for 4 consecutive windows; or the phase lock disengages; or PTP is stale (no allowed Sync / Follow_Up for 10 s) |

- Between 50 and 150 µs it keeps its state (hysteresis). Steady `e` noise is a few µs on Linux and
  6-18 µs (standard deviation) on Windows, so it never chatters.
- After the first lock, the boot's phase pull and a date step never move it (a date step moves the
  wall and `D` together, so `e` does not change).
- A grandmaster frequency step large enough to take `e` past 150 µs (a 25 ppm step: about 450 µs)
  clears it once; it is set again once `e` is back within 50 µs (about a minute).
- A cold boot (no `drift.json`) sets it about 30-45 s after the phase lock engages (simulation;
  τ·ln(|e0| / 50 µs) of the boot pull with τ = 20 s, so longer for a larger acquisition drift: 0.7 ms
  is ~53 s); a boot with a good drift file 4-9 s after "Sync established".
- `/status.mode` reads `LOCK` or `NANO` only while `[41]` is 1.

Under `"legacy"` it is the rate servo's lock (5 windows with |rate| < 5 µs/s), as before 1.23.

## `[62]` bit 1 — settled

It only means that the first PTP Sync was accepted after the warmup and PTP is not stale. It says
nothing about the frequency or the phase: read `[41]` for that.
