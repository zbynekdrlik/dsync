#!/usr/bin/env python3
"""dsync #179: the dev1 watch over every node whose dsync this project owns.

One pass (a `systemd --user` timer runs it every 60 s, `scripts/systemd/dsync-node-watch.*`):
read each node's `http://<addr>:8898/status`, grade it, write the verdicts to
`~/.local/state/dsync-watch/last.json`, and send every ALARM through `airuleset.py notify`.

The grade (`grade()`, pure, unit-tested in `test_node_watch.py`):
  ALARM  `is_locked` false; `mode` not NANO/LOCK; |`ptp_phase_error_us`| >= 50; `ntp_step_storm`;
         `version` not the latest release (upgrade debt, #169); `/status` unreadable.
  SKIP   a traveling node (alex-nb, resolume-pp) while its :22 does not answer: it is away, and
         that is never an alarm (camera-box's rule for the same nodes, dstack 12.11.2026).
  OK     otherwise.

An ALARM re-pings while it lasts: the notify `--dedup-key` carries a 10-minute bucket, so within a
bucket an identical alarm is not repeated and every new bucket pings again.

Usage:
  scripts/node_watch.py            one pass: fetch, grade, record, alert
  scripts/node_watch.py --dry-run  fetch, grade, record and print; never alert

Exit codes: 0 the pass ran (alarms included), 1 an alert could not be sent or the state not written.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass

REPO = "zbynekdrlik/dsync"
STATUS_PORT = 8898
PHASE_LIMIT_US = 50.0
LOCKED_MODES = ("NANO", "LOCK")
REPING_INTERVAL_S = 600
HTTP_TIMEOUT_S = 4.0
SSH_PROBE_TIMEOUT_S = 2.0
STATE_PATH = os.path.expanduser("~/.local/state/dsync-watch/last.json")
AIRULESET = os.path.expanduser("~/devel/airuleset/airuleset.py")


@dataclass(frozen=True)
class Node:
    name: str
    addr: str
    travel: bool = False


# Every dsync node this project owns (#169, #179). Rig and video boxes are camera-box's.
ROSTER = (
    Node("mbc", "mbc.lan"),
    Node("fohabl", "fohabl.lan"),
    Node("iem-snv", "iem-snv.lan"),
    Node("companion-snv", "companion-snv.lan"),
    Node("alex-nb", "10.77.7.109", travel=True),
    Node("resolume-pp", "resolume-pp.lan", travel=True),
)


def grade(status: dict | None, latest: str, travel: bool, ssh_up: bool) -> tuple[str, list[str]]:
    """Grade one node. `status` is the parsed `/status` or None when unreadable."""
    if status is None:
        if travel and not ssh_up:
            return "SKIP", ["away (:22 silent)"]
        return "ALARM", [f"/status unreadable on :{STATUS_PORT}"]
    reasons = []
    if not status.get("is_locked", False):
        reasons.append("not locked")
    mode = status.get("mode", "")
    if mode not in LOCKED_MODES:
        reasons.append(f"mode {mode or '?'}")
    err = status.get("ptp_phase_error_us")
    if err is None:
        reasons.append("no phase error")
    elif abs(err) >= PHASE_LIMIT_US:
        reasons.append(f"|e| {abs(err):.1f} us >= {PHASE_LIMIT_US:.0f}")
    if status.get("ntp_step_storm", False):
        reasons.append("ntp step storm")
    version = status.get("version", "")
    if version != latest:
        reasons.append(f"version {version or '<1.35'} != latest {latest}")
    return ("ALARM" if reasons else "OK"), reasons


def dedup_key(node: str, now: float) -> str:
    return f"dsync-watch-{node}-{int(now // REPING_INTERVAL_S)}"


def alert_text(node: Node, reasons: list[str]) -> str:
    return f"dsync watch: {node.name} ({node.addr}) ALARM: " + "; ".join(reasons)


def fetch_status(addr: str) -> dict | None:
    url = f"http://{addr}:{STATUS_PORT}/status"
    try:
        with urllib.request.urlopen(url, timeout=HTTP_TIMEOUT_S) as resp:
            return json.loads(resp.read())
    except (OSError, ValueError) as exc:
        print(f"{addr}: /status read failed: {exc}", file=sys.stderr)
        return None


def ssh_answers(addr: str) -> bool:
    try:
        with socket.create_connection((addr, 22), timeout=SSH_PROBE_TIMEOUT_S):
            return True
    except OSError:
        return False


LATEST_URL = f"https://github.com/{REPO}/releases/latest"
TAG_PREFIX = f"https://github.com/{REPO}/releases/tag/v"


def version_from_latest_location(location: str | None) -> str:
    """The version named by the `releases/latest` redirect's Location. Raises on anything else."""
    if not location or not location.startswith(TAG_PREFIX):
        raise RuntimeError(f"releases/latest redirected to {location!r}, not a release tag")
    version = location[len(TAG_PREFIX):]
    if not version or not version[0].isdigit():
        raise RuntimeError(f"releases/latest redirected to {location!r}, not a release tag")
    return version


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def latest_release() -> str:
    """The latest published release's version, from the public `releases/latest` redirect: no API,
    no token (the systemd --user unit has no GH_TOKEN; `gh api` exits 4 there). Raises on failure."""
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        opener.open(urllib.request.Request(LATEST_URL, method="HEAD"), timeout=15)
    except urllib.error.HTTPError as exc:
        if exc.code in (301, 302, 303, 307, 308):
            return version_from_latest_location(exc.headers.get("Location"))
        raise
    raise RuntimeError(f"{LATEST_URL} did not redirect")


def send_alert(text: str, key: str) -> None:
    subprocess.run(
        [sys.executable, AIRULESET, "notify", "--body", text, "--dedup-key", key],
        check=True, timeout=60,
    )


def run(dry_run: bool) -> int:
    now = time.time()
    latest = latest_release()
    results = []
    for node in ROSTER:
        status = fetch_status(node.addr)
        ssh_up = ssh_answers(node.addr) if (node.travel and status is None) else True
        verdict, reasons = grade(status, latest, node.travel, ssh_up)
        e = status.get("ptp_phase_error_us") if status else None
        print(f"{node.name}: {verdict} e={e} " + "; ".join(reasons))
        results.append({"node": node.name, "addr": node.addr, "verdict": verdict, "reasons": reasons,
                        "mode": status.get("mode") if status else None, "e_us": e,
                        "version": status.get("version") if status else None})
        if verdict == "ALARM" and not dry_run:
            send_alert(alert_text(node, reasons), dedup_key(node.name, now))
    os.makedirs(os.path.dirname(STATE_PATH), exist_ok=True)
    with open(STATE_PATH, "w") as f:
        json.dump({"ts": int(now), "latest": latest, "nodes": results}, f, indent=1)
    return 0


def main(argv: list[str]) -> int:
    if argv not in ([], ["--dry-run"]):
        print(__doc__, file=sys.stderr)
        return 2
    return run(dry_run=argv == ["--dry-run"])


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
