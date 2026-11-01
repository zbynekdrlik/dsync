---
name: sync-status
description: Check current DanteSync synchronization status across all network targets
---

Check the current clock synchronization status across all DanteSync targets using the UDP Time Query API.

## Instructions

Fleet hosts are not in this repository: they are in `~/.config/dsync/TARGETS.md`. dsync
queries no node until camera-box hands over the non-rig list, and never a camera-rig node
(`docs/HANDOVER.md`). `scripts/sync-snapshot.py` without `--hosts` still queries its
built-in list, so always pass `--hosts`.

1. Run the sync-snapshot.py script to get instant status:

   ```bash
   python3 scripts/sync-snapshot.py --hosts <host> ...
   ```

2. The output shows for each host:
   - System time (wall clock)
   - Sync mode (ACQ/PROD/LOCK/NANO)
   - Lock status
   - Round-trip time

3. The offset analysis shows:
   - System time difference (should be < 1ms)
   - Monotonic counter difference (true tick alignment)
   - Grandmaster UUID (real since 1.17; before it every node showed 00:00:00:00:01:00)

4. Flag any issues:
   - Host showing OFFLINE - check firewall/service
   - Mode not LOCK or NANO - still acquiring
   - Offset > 1ms - sync problem
   - Different GM UUIDs - report-only (issue 129): the video and the audio VLAN can hear two
     ports of one clock, and a mixed 1.16/1.17 fleet shows both forms. Record it on issue 129;
     it is a problem only together with a lock or drift issue

## Troubleshooting

If UDP queries fail, read the node's log only through the owner of that node (camera-box for
the rig, via dstack).
