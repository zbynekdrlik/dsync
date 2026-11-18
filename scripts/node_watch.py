#!/usr/bin/env python3
"""dsync #179: the dev1 watch over every node whose dsync this project owns.

One pass (a `systemd --user` timer runs it every 60 s, `scripts/systemd/dsync-node-watch.*`):
read each node's `http://<addr>:8898/status`, grade it, write the verdicts to
`~/.local/state/dsync-watch/last.json`, and send every ALARM through `airuleset.py notify`.

The grade (`grade()`, pure, unit-tested in `test_node_watch.py`):
  ALARM  `is_locked` false; `mode` not NANO/LOCK; |`ptp_phase_error_us`| >= 50; `ntp_step_storm`;
         `version` older than the latest release whose CODE changed (upgrade debt, #169, #186);
         `/status` unreadable;
         `gm_source_ip` not the node's expected audio grandmaster for >= GM_HOLD_S (#188; the
         expected GMs live in the private `~/.config/dsync/watch.json`, never in the repo, and an
         unreadable config alarms on every node).
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
WATCH_CONFIG = os.path.expanduser("~/.config/dsync/watch.json")
GM_HOLD_S = 60.0


@dataclass(frozen=True)
class Node:
    name: str
    addr: str
    travel: bool = False


# Every dsync node this project owns (#169, #179). Rig and video boxes are camera-box's.
ROSTER = (
    Node("mbc", "mbc.lan"),
    Node("fohabl", "fohabl.lan"),
    Node("companion-snv", "companion-snv.lan"),
    Node("iemmixer", "iem-gen3.lan"),  # #183: the FOH rack's gen3 box (dstack #260)
    Node("dsg", "dsg.lan"),  # dasio's PC, at PP since 18.11 (SoundGrid<->Dante bridge): dsg.lan -> 10.76.8.118 (#198, #202)
    Node("alex-nb", "10.77.7.109", travel=True),
    Node("resolume-pp", "resolume-pp.lan", travel=True),
    Node("linux-pp", "linux-pp.lan"),  # #202: PP's presenter (companion-snv's twin), dsync since 18.11
)


def grade(status: dict | None, latest: str, travel: bool, ssh_up: bool,
          code_debt=None) -> tuple[str, list[str]]:
    """Grade one node. `status` is the parsed `/status` or None when unreadable.

    `code_debt(have, latest)` (#186) is asked only for a version older than `latest`: True when
    the code differs between the two releases. Without it every older version is debt."""
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
    if not _at_least(version, latest):
        if code_debt is None:
            reasons.append(f"version {version or '<1.35'} < latest {latest}")
        elif not version or code_debt(version, latest):
            reasons.append(f"version {version or '<1.35'} < latest code {latest}")
    return ("ALARM" if reasons else "OK"), reasons


def load_expected_gm(path: str) -> dict[str, str]:
    """#188: the per-node expected GM address, `{"expected_gm": {"<node>": "<ip>", ...}}`.
    Raises OSError when the file cannot be read and ValueError when it is not that shape."""
    with open(path) as f:
        data = json.load(f)
    gms = data.get("expected_gm") if isinstance(data, dict) else None
    if not isinstance(gms, dict) or not all(
            isinstance(k, str) and isinstance(v, str) for k, v in gms.items()):
        raise ValueError(f"{path}: expected {{\"expected_gm\": {{node: ip}}}}")
    return gms


def gm_hold(status: dict | None, expected: str | None, since: float | None,
            now: float) -> tuple[float | None, str | None]:
    """#188: (the new mismatch-start time, the alarm reason or None). A node without an expected
    GM or without a readable `/status` is not checked and its timer clears."""
    if expected is None or status is None:
        return None, None
    actual = status.get("gm_source_ip")
    if actual == expected:
        return None, None
    since = now if since is None else since
    held = now - since
    if held < GM_HOLD_S:
        return since, None
    return since, f"GM {actual or 'none'} != expected {expected} for {held:.0f} s"


def with_gm_reason(graded: tuple[str, list[str]], reason: str | None) -> tuple[str, list[str]]:
    """Add the #188 GM reason to a grade: any reason makes it an ALARM."""
    _, reasons = graded
    if reason is None:
        return graded
    return "ALARM", [*reasons, reason]


def _previous_gm_since(path: str) -> dict[str, float]:
    """The mismatch start per node from the last pass's state file (none on a first pass)."""
    try:
        with open(path) as f:
            nodes = json.load(f).get("nodes", [])
    except FileNotFoundError:
        return {}
    except (OSError, ValueError) as exc:
        print(f"{path}: previous state unreadable, GM timers restart: {exc}", file=sys.stderr)
        return {}
    return {n["node"]: n["gm_mismatch_since"] for n in nodes
            if isinstance(n, dict) and n.get("gm_mismatch_since") is not None}


def _version_tuple(version: str) -> tuple[int, ...] | None:
    try:
        return tuple(int(p) for p in version.split("."))
    except ValueError:
        return None


def _at_least(version: str, latest: str) -> bool:
    """True when `version` is the latest release or newer (the gate's dev candidate on companion
    is newer, not debt). An empty or unparseable version is debt."""
    have, want = _version_tuple(version), _version_tuple(latest)
    return have is not None and want is not None and have >= want


# #186: what ships in the binary. Everything else (scripts/, docs/, .claude/, README, .github/)
# can change in a release without changing what a node runs.
CODE_PREFIXES = ("src/", "crates/")
CODE_FILES = ("build.rs", "Cargo.toml", "Cargo.lock")
_OWN_PACKAGES = ("dsync", "dsync-ptp")


def _is_code_path(path: str) -> bool:
    return path.startswith(CODE_PREFIXES) or path in CODE_FILES


def normalize_cargo(path: str, text: str) -> str:
    """A Cargo file without our own version lines (#186): the release bump is not code.

    `Cargo.toml` / `crates/*/Cargo.toml`: the `[package]` `version = "…"` line (a dependency's
    version is inline, never a bare `version =` line). `Cargo.lock`: the `version` that follows
    `name = "dsync"` / `name = "dsync-ptp"`; every other package's version stays (a dependency
    bump IS code)."""
    out, own = [], False
    for line in text.splitlines():
        if path.endswith("Cargo.lock"):
            if line.startswith("name = "):
                own = line.split("=", 1)[1].strip().strip('"') in _OWN_PACKAGES
            elif own and line.startswith("version = "):
                own = False
                continue
        elif line.startswith("version = "):
            continue
        out.append(line)
    return "\n".join(out)


def code_changed(changes) -> bool:
    """True when any (path, old, new) change touches what ships in the binary (#186)."""
    for path, old, new in changes:
        if not _is_code_path(path):
            continue
        if path.endswith(("Cargo.toml", "Cargo.lock")):
            if normalize_cargo(path, old or "") != normalize_cargo(path, new or ""):
                return True
        elif old != new:
            return True
    return False


def _git(repo: str, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True,
                          check=check, timeout=60)


def _show(repo: str, tag: str, path: str) -> str | None:
    r = _git(repo, "show", f"{tag}:{path}", check=False)
    return r.stdout if r.returncode == 0 else None


def code_debt(repo: str, have: str, latest: str) -> bool:
    """#186: does the code differ between release `have` and release `latest`? Read from the
    repo's own tags (fetched when `v<latest>` is missing). An unknown `have` tag is debt.
    Raises on a git failure; the caller falls back to the plain version compare."""
    old, new = f"v{have}", f"v{latest}"
    if _git(repo, "rev-parse", "-q", "--verify", f"refs/tags/{new}", check=False).returncode:
        _git(repo, "fetch", "-q", "--tags", "origin")
    if _git(repo, "rev-parse", "-q", "--verify", f"refs/tags/{old}", check=False).returncode:
        return True
    names = _git(repo, "diff", "--name-only", old, new).stdout.split()
    return code_changed((p, _show(repo, old, p), _show(repo, new, p))
                        for p in names if _is_code_path(p))


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


REPO_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _code_debt_or_plain(have: str, latest: str) -> bool:
    """`code_debt` on this checkout; a git failure is logged and counts as debt (the plain
    version compare: an alarm, never a silent OK)."""
    try:
        return code_debt(REPO_DIR, have, latest)
    except (OSError, subprocess.SubprocessError) as exc:
        print(f"code diff v{have}..v{latest} failed, grading the plain version: {exc}",
              file=sys.stderr)
        return True


def run(dry_run: bool) -> int:
    now = time.time()
    latest = latest_release()
    debt = _code_debt_or_plain
    try:
        expected_gm, config_error = load_expected_gm(WATCH_CONFIG), None
    except (OSError, ValueError) as exc:
        expected_gm, config_error = {}, f"expected GM config unreadable: {exc}"
        print(config_error, file=sys.stderr)
    previous = _previous_gm_since(STATE_PATH)
    results = []
    for node in ROSTER:
        status = fetch_status(node.addr)
        ssh_up = ssh_answers(node.addr) if (node.travel and status is None) else True
        graded = grade(status, latest, node.travel, ssh_up, debt)
        since, gm_reason = gm_hold(status, expected_gm.get(node.name), previous.get(node.name), now)
        verdict, reasons = with_gm_reason(graded, gm_reason or config_error)
        e = status.get("ptp_phase_error_us") if status else None
        print(f"{node.name}: {verdict} e={e} " + "; ".join(reasons))
        results.append({"node": node.name, "addr": node.addr, "verdict": verdict, "reasons": reasons,
                        "mode": status.get("mode") if status else None, "e_us": e,
                        "version": status.get("version") if status else None,
                        "gm": status.get("gm_source_ip") if status else None,
                        "gm_mismatch_since": since})
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
