"""Unit tests for scripts/dante_gate.py (dsync #98, the Dante stability gate).

Run: python3 -m unittest discover -s scripts -p 'test_*.py'

The fixture lines are the exact shape `dante-stack-dev clock` v0.4.33 prints (one JSON object per
poll, see the tool's `--help`), recorded against dev1's own dantesync on 2026-11-02.
"""

import json
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
        self.assertEqual(v.re_anchor_polls, 2)
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
    TREES = {"aaa": "T1", "bbb": "T2", "ccc": "T2"}

    def tree_of(self, sha):
        return self.TREES[sha]

    def test_a_gated_tree_is_found(self):
        self.assertEqual(dante_gate.tree_gated("T2", ["aaa", "bbb"], self.tree_of), "bbb")

    def test_an_ungated_tree_is_refused(self):
        self.assertIsNone(dante_gate.tree_gated("T9", ["aaa", "bbb"], self.tree_of))

    def test_no_successful_runs_is_refused(self):
        self.assertIsNone(dante_gate.tree_gated("T1", [], self.tree_of))

    def test_a_commit_that_cannot_be_read_is_skipped_not_matched(self):
        def tree_of(sha):
            if sha == "gone":
                raise dante_gate.GhError("HTTP 404")
            return self.TREES[sha]

        self.assertEqual(dante_gate.tree_gated("T1", ["gone", "aaa"], tree_of), "aaa")
        self.assertIsNone(dante_gate.tree_gated("T1", ["gone"], tree_of))

    def test_each_sha_is_read_once(self):
        calls = []

        def tree_of(sha):
            calls.append(sha)
            return self.TREES[sha]

        dante_gate.tree_gated("T9", ["aaa", "aaa", "bbb"], tree_of)
        self.assertEqual(calls, ["aaa", "bbb"])

    def test_release_check_cli_uses_the_github_api(self):
        seen = []

        def gh(args):
            seen.append(args)
            if "actions/workflows/gate.yml/runs" in args[1]:
                return "aaa\nbbb\n"
            return {"aaa": "T1\n", "bbb": "T2\n"}[args[1].rsplit("/", 1)[1]]

        rc = dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], gh=gh)
        self.assertEqual(rc, 0)
        self.assertIn("status=success", seen[0][1])
        self.assertEqual(seen[1][1], "repos/o/r/git/commits/aaa")

        self.assertEqual(dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T7"], gh=gh), 1)

    def test_release_check_fails_closed_when_the_run_list_errors(self):
        def gh(args):
            raise dante_gate.GhError("HTTP 502")

        self.assertEqual(dante_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], gh=gh), 1)


if __name__ == "__main__":
    unittest.main()
