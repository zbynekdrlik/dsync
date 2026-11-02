"""Unit tests for scripts/dante_gate.py (dsync #98, the Dante stability gate).

Run: python3 -m unittest discover -s scripts -p 'test_*.py'

The fixture lines are the exact shape `dante-stack-dev clock` v0.4.33 prints (one JSON object per
poll, see the tool's `--help`), recorded against dev1's own dantesync on 2026-11-02.
"""

import json
import os
import unittest

import dante_gate


def poll(t, ready=1, re_anchors=0, re_anchor_ns=None, holdover=0, sample=0, locked=1):
    return json.dumps(
        {
            "t_s": t,
            "ready": ready,
            "k_ns": 1903603336087573 if ready else None,
            "dk_ns": None if t == 0 else -211,
            "off_ns": 0,
            "re_anchors": re_anchors,
            "re_anchor_ns": re_anchor_ns,
            "holdover": holdover,
            "sample": sample,
            "locked": locked,
            "mode": 4,
            "freq_ppb": -22780,
        }
    )


REAL_LINES = [
    '{"t_s":0,"ready":1,"k_ns":1903603336087573,"dk_ns":null,"off_ns":0,"re_anchors":0,'
    '"re_anchor_ns":null,"holdover":0,"sample":0,"locked":1,"mode":4,"freq_ppb":-22780}',
    '{"t_s":1,"ready":1,"k_ns":1903603336087362,"dk_ns":-211,"off_ns":-211,"re_anchors":0,'
    '"re_anchor_ns":null,"holdover":0,"sample":0,"locked":1,"mode":4,"freq_ppb":-22805}',
    '{"t_s":2,"ready":1,"k_ns":1903603336087563,"dk_ns":201,"off_ns":-10,"re_anchors":0,'
    '"re_anchor_ns":null,"holdover":0,"sample":0,"locked":1,"mode":4,"freq_ppb":-22616}',
]


class VerdictTests(unittest.TestCase):
    def test_real_tool_output_passes(self):
        v = dante_gate.verdict(REAL_LINES, polls=3)
        self.assertTrue(v.passed, v.reasons)
        self.assertEqual(v.ready_events, 1)
        self.assertEqual(v.re_anchor_polls, 0)
        self.assertEqual(v.holdover_polls, 0)

    def test_acquisition_then_one_ready_passes(self):
        # A restarted dantesync: no reply, then not locked, then Ready once.
        lines = [poll(0, ready=0, sample=5), poll(1, ready=0, sample=1, locked=0)]
        lines += [poll(t) for t in range(2, 10)]
        v = dante_gate.verdict(lines, polls=10)
        self.assertTrue(v.passed, v.reasons)
        self.assertEqual(v.ready_events, 1)
        self.assertEqual(v.samples, {5: 1, 1: 1, 0: 8})

    def test_step_near_samples_are_allowed(self):
        lines = [poll(0), poll(1, sample=3), poll(2, sample=3), poll(3)]
        self.assertTrue(dante_gate.verdict(lines, polls=4).passed)

    def test_step_near_holdover_is_allowed(self):
        # dstack's tracker (clock.rs v0.4.33, Tracker::feed) turns every unusable poll after the
        # anchor into Holdover, StepNear included. The pass rule allows StepNear.
        lines = [poll(0), poll(1, sample=3, holdover=1), poll(2, sample=3, holdover=1), poll(3)]
        v = dante_gate.verdict(lines, polls=4)
        self.assertTrue(v.passed, v.reasons)
        self.assertEqual(v.holdover_polls, 0)
        self.assertEqual(v.step_near_holdover_polls, 2)

    def test_a_slew_holdover_fails(self):
        lines = [poll(0), poll(1, sample=4, holdover=1), poll(2)]
        self.assertFalse(dante_gate.verdict(lines, polls=3).passed)

    def test_never_ready_fails(self):
        lines = [poll(t, ready=0, sample=1, locked=0) for t in range(5)]
        v = dante_gate.verdict(lines, polls=5)
        self.assertFalse(v.passed)
        self.assertEqual(v.ready_events, 0)
        self.assertTrue(any("Ready" in r for r in v.reasons), v.reasons)

    def test_ready_lost_and_regained_fails(self):
        lines = [poll(0), poll(1, ready=0), poll(2)]
        v = dante_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        self.assertEqual(v.ready_events, 2)

    def test_a_re_anchor_fails(self):
        lines = [poll(0), poll(1, re_anchors=1, re_anchor_ns=48211), poll(2, re_anchors=1)]
        v = dante_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        # re_anchors is cumulative: one re-anchor, not one per later poll.
        self.assertEqual(v.re_anchor_polls, 1)
        self.assertEqual(v.re_anchors_max, 1)
        self.assertTrue(any("ReAnchored" in r for r in v.reasons), v.reasons)

    def test_a_re_anchor_counter_alone_fails(self):
        # Even if a line's own re_anchor_ns were null, a non-zero counter is a re-anchor.
        lines = [poll(0), poll(1, re_anchors=1)]
        self.assertFalse(dante_gate.verdict(lines, polls=2).passed)

    def test_a_holdover_fails(self):
        lines = [poll(0), poll(1, holdover=1, sample=5), poll(2)]
        v = dante_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        self.assertEqual(v.holdover_polls, 1)
        self.assertTrue(any("Holdover" in r for r in v.reasons), v.reasons)

    def test_a_truncated_log_fails(self):
        v = dante_gate.verdict(REAL_LINES[:2], polls=3)
        self.assertFalse(v.passed)
        self.assertTrue(any("2 polls" in r for r in v.reasons), v.reasons)

    def test_an_empty_log_fails(self):
        v = dante_gate.verdict([], polls=3600)
        self.assertFalse(v.passed)

    def test_a_garbled_line_fails(self):
        lines = [REAL_LINES[0], "thread panicked at src/clock.rs", REAL_LINES[2]]
        v = dante_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        self.assertTrue(any("line 2" in r for r in v.reasons), v.reasons)

    def test_a_record_missing_a_field_fails(self):
        rec = json.loads(REAL_LINES[1])
        del rec["holdover"]
        v = dante_gate.verdict([REAL_LINES[0], json.dumps(rec), REAL_LINES[2]], polls=3)
        self.assertFalse(v.passed)
        self.assertTrue(any("line 2" in r for r in v.reasons), v.reasons)

    def test_a_json_non_object_line_fails(self):
        v = dante_gate.verdict([REAL_LINES[0], "[1, 2]", REAL_LINES[2]], polls=3)
        self.assertFalse(v.passed)

    def test_the_report_names_the_counts(self):
        text = dante_gate.verdict(REAL_LINES, polls=3).report()
        self.assertIn("PASS", text)
        self.assertIn("Ready events: 1", text)


class VerdictCliTests(unittest.TestCase):
    def _run(self, lines, polls):
        import os
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "clock.jsonl")
            with open(path, "w") as fh:
                fh.write("\n".join(lines) + "\n")
            return dante_gate.main(["verdict", path, "--polls", str(polls)])

    def test_cli_exit_zero_on_pass(self):
        self.assertEqual(self._run(REAL_LINES, 3), 0)

    def test_cli_exit_one_on_fail(self):
        self.assertEqual(self._run(REAL_LINES[:1], 3), 1)

    def test_cli_exit_one_on_missing_file(self):
        self.assertEqual(dante_gate.main(["verdict", "/nonexistent/clock.jsonl", "--polls", "3"]), 1)


class ReleaseCheckTests(unittest.TestCase):
    RUNS = {
        "aaa": [{"conclusion": "success", "event": "push", "id": 1}],
        "bbb": [{"conclusion": "success", "event": "schedule", "id": 2}],
        "ccc": [{"conclusion": "success", "event": "workflow_dispatch", "id": 3}],
        "ddd": [],
        "eee": [{"conclusion": "failure", "event": "push", "id": 5}],
    }

    def runs_for(self, sha):
        return self.RUNS[sha]

    def test_a_push_run_gates_its_commit(self):
        self.assertEqual(dante_gate.gated_run(["ddd", "aaa"], self.runs_for)[0], "aaa")

    def test_a_dispatch_run_gates_its_commit(self):
        self.assertEqual(dante_gate.gated_run(["ccc"], self.runs_for)[0], "ccc")

    def test_a_schedule_run_is_never_evidence(self):
        # A schedule run only dispatches the gate on dev; its build/gate/verdict are skipped and
        # the run still ends `success` on the master head. It must never release that tree.
        self.assertIsNone(dante_gate.gated_run(["bbb"], self.runs_for))

    def test_a_failed_run_is_not_evidence(self):
        self.assertIsNone(dante_gate.gated_run(["eee"], self.runs_for))

    def test_no_candidates_is_refused(self):
        self.assertIsNone(dante_gate.gated_run([], self.runs_for))

    def test_each_sha_is_looked_up_once(self):
        calls = []

        def runs_for(sha):
            calls.append(sha)
            return []

        dante_gate.gated_run(["ddd", "ddd", "eee"], runs_for)
        self.assertEqual(calls, ["ddd", "eee"])

    def test_commits_with_tree(self):
        log = "m1 T1\nd1 T1\nd0 T0\n\ngarbage\n"
        self.assertEqual(dante_gate.commits_with_tree(log, "T1"), ["m1", "d1"])
        self.assertEqual(dante_gate.commits_with_tree(log, "T9"), [])

    def _fake(self, runs_by_sha, log="merge T2\ndevhead T2\nold T1\n"):
        seen = []

        def run(args):
            seen.append(args)
            if args[0] == "git":
                return log
            sha = args[2].split("head_sha=")[1].split("&")[0]
            return json.dumps({"workflow_runs": runs_by_sha.get(sha, [])})

        return run, seen

    def test_release_check_cli_finds_the_gated_dev_head(self):
        run, seen = self._fake({"devhead": [{"conclusion": "success", "event": "push"}]})
        rc = dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run)
        self.assertEqual(rc, 0)
        self.assertEqual(seen[0], ["git", "log", "--all", "--format=%H %T"])
        self.assertIn("repos/o/r/actions/workflows/gate.yml/runs?head_sha=merge", seen[1][2])
        self.assertIn("status=success", seen[1][2])

    def test_release_check_cli_refuses_an_ungated_tree(self):
        run, _ = self._fake({"old": [{"conclusion": "success", "event": "push"}]})
        self.assertEqual(dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)

    def test_release_check_cli_refuses_a_schedule_only_tree(self):
        run, _ = self._fake({"merge": [{"conclusion": "success", "event": "schedule"}]})
        self.assertEqual(dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)

    def test_release_check_fails_closed_on_an_api_error(self):
        def run(args):
            if args[0] == "git":
                return "merge T2\n"
            raise dante_gate.GhError("HTTP 502")

        self.assertEqual(dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)

    def test_release_check_fails_closed_without_git_history(self):
        def run(args):
            raise dante_gate.GhError("not a git repository")

        self.assertEqual(dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)


class GateWorkflowTests(unittest.TestCase):
    """The workflow's own commitments (the rig cannot run in CI, so the text is what is checked)."""

    @classmethod
    def setUpClass(cls):
        path = os.path.join(os.path.dirname(__file__), "..", ".github", "workflows", "gate.yml")
        with open(path, encoding="utf-8") as f:
            cls.text = f.read()
        cls.helper_calls = [
            line.strip()
            for line in cls.text.splitlines()
            if '"$GATE_INSTALL"' in line and not line.strip().startswith("#")
        ]

    def test_every_install_is_cold_146(self):
        # dsync #146 / #98 (2026-11-02): every gate run proves the cold boot, never a seeded one.
        installs = [c for c in self.helper_calls if "--restore" not in c]
        self.assertEqual(len(installs), 1, installs)
        self.assertIn('sudo -n "$GATE_INSTALL" --cold "$PWD/candidate/dantesync"', installs[0])

    def test_there_is_exactly_one_restore_call(self):
        restores = [c for c in self.helper_calls if "--restore" in c]
        self.assertEqual(len(restores), 1, restores)

    def test_never_on_pull_request(self):
        self.assertNotRegex(self.text, r"(?m)^\s*pull_request(_target)?\s*:")


if __name__ == "__main__":
    unittest.main()
