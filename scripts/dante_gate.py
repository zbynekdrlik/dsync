#!/usr/bin/env python3
"""dsync #98: the Dante stability gate's verdict and its release check.

Two subcommands, both used by CI (`docs/dante-gate.md`):

verdict <clock.jsonl> --polls N
    Judges the output of dstack's `dante-stack-dev clock --seconds N` (v0.4.33: one JSON object
    per poll, numbers only; see the tool's `--help`). Part 1 pass rule (dstack, 2026-11-02):
    exactly one Ready, then zero ReAnchored and zero Holdover. StepNear (sample 3) is allowed.
      Ready      = a poll whose `ready` turns 1 (the first poll counts when it is already 1).
      ReAnchored = a poll with a non-null `re_anchor_ns` or a non-zero `re_anchors`.
      Holdover   = a poll with `holdover` = 1.
    The log must hold exactly N parseable poll records: a truncated or garbled log fails.

release-check --repo OWNER/NAME --tree TREE
    Exits 0 only when a successful `gate.yml` run tested a commit whose git tree equals TREE
    (the master commit's `HEAD^{tree}`). A `--no-ff` merge commit has a new SHA but, when
    master is an ancestor of dev, the same tree as dev's gated head: equal trees are identical
    files. Any API failure listing the runs fails closed.

Exit codes: 0 pass, 1 fail, 2 usage.
"""

import argparse
import json
import os
import subprocess
import sys

REQUIRED_FIELDS = ("ready", "re_anchors", "re_anchor_ns", "holdover", "sample")
GATE_WORKFLOW = "gate.yml"


class GhError(Exception):
    """A `gh api` call failed."""


class Verdict:
    def __init__(self, polls_expected):
        self.polls_expected = polls_expected
        self.polls = 0
        self.ready_events = 0
        self.re_anchor_polls = 0
        self.holdover_polls = 0
        self.samples = {}
        self.reasons = []

    @property
    def passed(self):
        return not self.reasons

    def report(self):
        lines = [
            "Dante gate verdict: %s" % ("PASS" if self.passed else "FAIL"),
            "Polls: %d of %d" % (self.polls, self.polls_expected),
            "Ready events: %d (need exactly 1)" % self.ready_events,
            "ReAnchored polls: %d (need 0)" % self.re_anchor_polls,
            "Holdover polls: %d (need 0)" % self.holdover_polls,
            "Samples by code (0 usable, 1 not locked, 2 no date offset, 3 step near, "
            "4 slew, 5 no reply): %s"
            % ", ".join("%d=%d" % (k, v) for k, v in sorted(self.samples.items())),
        ]
        lines += ["Reason: %s" % r for r in self.reasons]
        return "\n".join(lines)


def _record(line):
    """The poll record on `line`, or None when it is not one."""
    try:
        rec = json.loads(line)
    except ValueError:
        return None
    if not isinstance(rec, dict) or any(f not in rec for f in REQUIRED_FIELDS):
        return None
    return rec


def verdict(lines, polls):
    """Apply the part 1 pass rule to the tool's output lines."""
    v = Verdict(polls)
    prev_ready = 0
    for n, line in enumerate(lines, start=1):
        rec = _record(line)
        if rec is None:
            v.reasons.append("line %d is not a poll record: %r" % (n, line[:200]))
            continue
        v.polls += 1
        ready = rec["ready"]
        if ready == 1 and prev_ready != 1:
            v.ready_events += 1
        prev_ready = ready
        if rec["re_anchor_ns"] is not None or rec["re_anchors"]:
            v.re_anchor_polls += 1
        if rec["holdover"] == 1:
            v.holdover_polls += 1
        v.samples[rec["sample"]] = v.samples.get(rec["sample"], 0) + 1

    if v.polls != polls:
        v.reasons.append("the log holds %d polls, the run asked for %d" % (v.polls, polls))
    if v.ready_events != 1:
        v.reasons.append("%d Ready events, need exactly 1" % v.ready_events)
    if v.re_anchor_polls:
        v.reasons.append("%d ReAnchored polls, need 0" % v.re_anchor_polls)
    if v.holdover_polls:
        v.reasons.append("%d Holdover polls, need 0" % v.holdover_polls)
    return v


def tree_gated(tree, gated_shas, tree_of):
    """The first gated commit whose tree equals `tree`, else None.

    A commit that cannot be read (deleted, an API error) is skipped: it can never match.
    """
    seen = set()
    for sha in gated_shas:
        if sha in seen:
            continue
        seen.add(sha)
        try:
            sha_tree = tree_of(sha)
        except GhError as exc:
            print("skip %s: %s" % (sha, exc), file=sys.stderr)
            continue
        if sha_tree == tree:
            return sha
    return None


def run_gh(args):
    """`gh <args>` stdout; GhError on a non-zero exit."""
    proc = subprocess.run(["gh"] + list(args), capture_output=True, text=True)
    if proc.returncode != 0:
        raise GhError(proc.stderr.strip() or "gh exited %d" % proc.returncode)
    return proc.stdout


def _verdict_cmd(opts):
    try:
        with open(opts.clock_log, encoding="utf-8") as fh:
            lines = fh.read().splitlines()
    except OSError as exc:
        print("Dante gate verdict: FAIL\nReason: cannot read %s: %s" % (opts.clock_log, exc))
        return 1
    v = verdict(lines, opts.polls)
    text = v.report()
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write("```\n%s\n```\n" % text)
    return 0 if v.passed else 1


def _release_check_cmd(opts, gh):
    runs_path = "repos/%s/actions/workflows/%s/runs?status=success&per_page=100" % (
        opts.repo,
        GATE_WORKFLOW,
    )
    try:
        out = gh(["api", runs_path, "--jq", ".workflow_runs[].head_sha"])
    except GhError as exc:
        print("release-check: cannot list %s runs: %s" % (GATE_WORKFLOW, exc))
        return 1
    shas = [s for s in out.split() if s]

    def tree_of(sha):
        return gh(["api", "repos/%s/git/commits/%s" % (opts.repo, sha), "--jq", ".tree.sha"]).strip()

    match = tree_gated(opts.tree, shas, tree_of)
    if match:
        print("release-check: tree %s passed the Dante gate (commit %s)" % (opts.tree, match))
        return 0
    print(
        "release-check: no successful %s run tested tree %s (%d successful runs checked).\n"
        "Gate this tree on dev (`gh workflow run %s --ref dev`), then re-run this job."
        % (GATE_WORKFLOW, opts.tree, len(set(shas)), GATE_WORKFLOW)
    )
    return 1


def main(argv=None, gh=run_gh):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_verdict = sub.add_parser("verdict", help="judge a dante-stack-dev clock log")
    p_verdict.add_argument("clock_log")
    p_verdict.add_argument("--polls", type=int, required=True)
    p_release = sub.add_parser("release-check", help="is this tree gated?")
    p_release.add_argument("--repo", required=True)
    p_release.add_argument("--tree", required=True)
    opts = parser.parse_args(argv)
    if opts.cmd == "verdict":
        return _verdict_cmd(opts)
    return _release_check_cmd(opts, gh)


if __name__ == "__main__":
    sys.exit(main())
