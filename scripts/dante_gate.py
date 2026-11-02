#!/usr/bin/env python3
"""dsync #98: the Dante stability gate's verdict and its release check.

Two subcommands, both used by CI (`docs/dante-gate.md`):

verdict <clock.jsonl> --polls N
    Judges the output of dstack's `dante-stack-dev clock --seconds N` (v0.4.33: one JSON object
    per poll, numbers only; see the tool's `--help`). Part 1 pass rule (dstack, 2026-11-02):
    exactly one Ready, then zero ReAnchored and zero Holdover. StepNear (sample 3) is allowed.
      Ready      = a poll whose `ready` turns 1 (the first poll counts when it is already 1).
      ReAnchored = a poll with a non-null `re_anchor_ns` or a non-zero `re_anchors`.
      Holdover   = a poll with `holdover` = 1, EXCEPT on a StepNear poll. dstack's tracker
                   (crates/dante-stack/src/clock.rs, v0.4.33) turns every unusable poll after the
                   anchor into Holdover, StepNear included; the rule allows StepNear, so those are
                   counted apart (`step_near_holdover_polls`) and pass.
      ReAnchored polls are those with a non-null `re_anchor_ns`; `re_anchors` is cumulative, so
      its maximum is reported (and must be 0) rather than counted per poll.
    The log must hold exactly N parseable poll records: a truncated or garbled log fails.

release-check --repo OWNER/NAME --tree TREE
    Exits 0 only when a successful `gate.yml` run, started by `push` or `workflow_dispatch`,
    tested a commit whose git tree equals TREE (the master commit's `HEAD^{tree}`). A `--no-ff`
    merge commit has a new SHA but, when master is an ancestor of dev, the same tree as dev's
    gated head: equal trees are identical files. The candidates are the local commits (all refs)
    with that tree; each is looked up by `head_sha`. A `schedule` run is never evidence: it only
    dispatches the gate on dev and measures nothing. Any API error fails closed.

Exit codes: 0 pass, 1 fail, 2 usage.
"""

import argparse
import json
import os
import subprocess
import sys

REQUIRED_FIELDS = ("ready", "re_anchors", "re_anchor_ns", "holdover", "sample")
SAMPLE_STEP_NEAR = 3
GATE_WORKFLOW = "gate.yml"
# Only these events run build + gate + verdict; a `schedule` run only dispatches one on dev.
GATING_EVENTS = ("push", "workflow_dispatch")


class GhError(Exception):
    """A `gh` or `git` call failed."""


class Verdict:
    def __init__(self, polls_expected):
        self.polls_expected = polls_expected
        self.polls = 0
        self.ready_events = 0
        self.re_anchor_polls = 0
        self.re_anchors_max = 0
        self.holdover_polls = 0
        self.step_near_holdover_polls = 0
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
            "ReAnchored polls: %d, re_anchors max %d (need 0)"
            % (self.re_anchor_polls, self.re_anchors_max),
            "Holdover polls: %d (need 0; %d more on StepNear polls, allowed)"
            % (self.holdover_polls, self.step_near_holdover_polls),
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
        if rec["re_anchor_ns"] is not None:
            v.re_anchor_polls += 1
        v.re_anchors_max = max(v.re_anchors_max, rec["re_anchors"] or 0)
        if rec["holdover"] == 1:
            if rec["sample"] == SAMPLE_STEP_NEAR:
                v.step_near_holdover_polls += 1
            else:
                v.holdover_polls += 1
        v.samples[rec["sample"]] = v.samples.get(rec["sample"], 0) + 1

    if v.polls != polls:
        v.reasons.append("the log holds %d polls, the run asked for %d" % (v.polls, polls))
    if v.ready_events != 1:
        v.reasons.append("%d Ready events, need exactly 1" % v.ready_events)
    if v.re_anchor_polls or v.re_anchors_max:
        v.reasons.append(
            "ReAnchored: %d polls, re_anchors max %d, need 0"
            % (v.re_anchor_polls, v.re_anchors_max)
        )
    if v.holdover_polls:
        v.reasons.append("%d Holdover polls (not on StepNear), need 0" % v.holdover_polls)
    return v


def gated_run(candidates, runs_for):
    """The first candidate commit with a gating run (success, push or dispatch), else None.

    `runs_for(sha)` returns the successful gate.yml runs for that head_sha as dicts with an
    `event`. A lookup error propagates: the caller fails closed.
    """
    seen = set()
    for sha in candidates:
        if sha in seen:
            continue
        seen.add(sha)
        for run in runs_for(sha):
            if run.get("conclusion") == "success" and run.get("event") in GATING_EVENTS:
                return sha, run
    return None


def commits_with_tree(log_text, tree):
    """The commits in `git log --all --format='%H %T'` output whose tree is `tree`."""
    out = []
    for line in log_text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1] == tree:
            out.append(parts[0])
    return out


def run_cmd(args):
    """stdout of `args`; GhError on a non-zero exit."""
    proc = subprocess.run(list(args), capture_output=True, text=True)
    if proc.returncode != 0:
        raise GhError(proc.stderr.strip() or "%s exited %d" % (args[0], proc.returncode))
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


def _release_check_cmd(opts, run):
    try:
        log = run(["git", "log", "--all", "--format=%H %T"])
    except GhError as exc:
        print("release-check: cannot read the git history: %s" % exc)
        return 1
    candidates = commits_with_tree(log, opts.tree)

    def runs_for(sha):
        path = "repos/%s/actions/workflows/%s/runs?head_sha=%s&status=success&per_page=100" % (
            opts.repo,
            GATE_WORKFLOW,
            sha,
        )
        return json.loads(run(["gh", "api", path])).get("workflow_runs", [])

    try:
        match = gated_run(candidates, runs_for)
    except (GhError, ValueError) as exc:
        print("release-check: cannot read %s runs: %s" % (GATE_WORKFLOW, exc))
        return 1
    if match:
        sha, gate = match
        print(
            "release-check: tree %s passed the Dante gate: commit %s, run %s (%s)"
            % (opts.tree, sha, gate.get("html_url", gate.get("id")), gate.get("event"))
        )
        return 0
    print(
        "release-check: no successful push/dispatch %s run tested tree %s "
        "(%d commits with this tree checked).\n"
        "Gate it: before the merge `gh workflow run %s --ref dev`; after the merge "
        "`gh workflow run %s --ref master` (that run tests the merge commit itself). "
        "Then re-run this job." % (GATE_WORKFLOW, opts.tree, len(candidates), GATE_WORKFLOW,
                                   GATE_WORKFLOW)
    )
    return 1


def main(argv=None, run=run_cmd):
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
    return _release_check_cmd(opts, run)


if __name__ == "__main__":
    sys.exit(main())
