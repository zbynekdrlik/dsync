"""Unit tests for scripts/stability_gate.py (dsync #98, the stability gate).

Run: python3 -m unittest discover -s scripts -p 'test_*.py'

The fixture lines are the exact shape `dante-stack-dev clock` v0.4.33 prints (one JSON object per
poll, see the tool's `--help`), recorded against dev1's own daemon on 2026-11-02.
"""

import json
import os
import unittest

import stability_gate


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
        v = stability_gate.verdict(REAL_LINES, polls=3)
        self.assertTrue(v.passed, v.reasons)
        self.assertEqual(v.ready_events, 1)
        self.assertEqual(v.re_anchor_polls, 0)
        self.assertEqual(v.holdover_polls, 0)

    def test_acquisition_then_one_ready_passes(self):
        # A restarted daemon: no reply, then not locked, then Ready once.
        lines = [poll(0, ready=0, sample=5), poll(1, ready=0, sample=1, locked=0)]
        lines += [poll(t) for t in range(2, 10)]
        v = stability_gate.verdict(lines, polls=10)
        self.assertTrue(v.passed, v.reasons)
        self.assertEqual(v.ready_events, 1)
        self.assertEqual(v.samples, {5: 1, 1: 1, 0: 8})

    def test_step_near_samples_are_allowed(self):
        lines = [poll(0), poll(1, sample=3), poll(2, sample=3), poll(3)]
        self.assertTrue(stability_gate.verdict(lines, polls=4).passed)

    def test_step_near_holdover_is_allowed(self):
        # dstack's tracker (clock.rs v0.4.33, Tracker::feed) turns every unusable poll after the
        # anchor into Holdover, StepNear included. The pass rule allows StepNear.
        lines = [poll(0), poll(1, sample=3, holdover=1), poll(2, sample=3, holdover=1), poll(3)]
        v = stability_gate.verdict(lines, polls=4)
        self.assertTrue(v.passed, v.reasons)
        self.assertEqual(v.holdover_polls, 0)
        self.assertEqual(v.step_near_holdover_polls, 2)

    def test_a_slew_holdover_fails(self):
        lines = [poll(0), poll(1, sample=4, holdover=1), poll(2)]
        self.assertFalse(stability_gate.verdict(lines, polls=3).passed)

    def test_never_ready_fails(self):
        lines = [poll(t, ready=0, sample=1, locked=0) for t in range(5)]
        v = stability_gate.verdict(lines, polls=5)
        self.assertFalse(v.passed)
        self.assertEqual(v.ready_events, 0)
        self.assertTrue(any("Ready" in r for r in v.reasons), v.reasons)

    def test_ready_lost_and_regained_fails(self):
        lines = [poll(0), poll(1, ready=0), poll(2)]
        v = stability_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        self.assertEqual(v.ready_events, 2)

    def test_a_re_anchor_fails(self):
        lines = [poll(0), poll(1, re_anchors=1, re_anchor_ns=48211), poll(2, re_anchors=1)]
        v = stability_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        # re_anchors is cumulative: one re-anchor, not one per later poll.
        self.assertEqual(v.re_anchor_polls, 1)
        self.assertEqual(v.re_anchors_max, 1)
        self.assertTrue(any("ReAnchored" in r for r in v.reasons), v.reasons)

    def test_a_re_anchor_counter_alone_fails(self):
        # Even if a line's own re_anchor_ns were null, a non-zero counter is a re-anchor.
        lines = [poll(0), poll(1, re_anchors=1)]
        self.assertFalse(stability_gate.verdict(lines, polls=2).passed)

    def test_a_holdover_fails(self):
        lines = [poll(0), poll(1, holdover=1, sample=5), poll(2)]
        v = stability_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        self.assertEqual(v.holdover_polls, 1)
        self.assertTrue(any("Holdover" in r for r in v.reasons), v.reasons)

    def test_a_truncated_log_fails(self):
        v = stability_gate.verdict(REAL_LINES[:2], polls=3)
        self.assertFalse(v.passed)
        self.assertTrue(any("2 polls" in r for r in v.reasons), v.reasons)

    def test_an_empty_log_fails(self):
        v = stability_gate.verdict([], polls=3600)
        self.assertFalse(v.passed)

    def test_a_garbled_line_fails(self):
        lines = [REAL_LINES[0], "thread panicked at src/clock.rs", REAL_LINES[2]]
        v = stability_gate.verdict(lines, polls=3)
        self.assertFalse(v.passed)
        self.assertTrue(any("line 2" in r for r in v.reasons), v.reasons)

    def test_a_record_missing_a_field_fails(self):
        rec = json.loads(REAL_LINES[1])
        del rec["holdover"]
        v = stability_gate.verdict([REAL_LINES[0], json.dumps(rec), REAL_LINES[2]], polls=3)
        self.assertFalse(v.passed)
        self.assertTrue(any("line 2" in r for r in v.reasons), v.reasons)

    def test_a_json_non_object_line_fails(self):
        v = stability_gate.verdict([REAL_LINES[0], "[1, 2]", REAL_LINES[2]], polls=3)
        self.assertFalse(v.passed)

    def test_the_report_names_the_counts(self):
        text = stability_gate.verdict(REAL_LINES, polls=3).report()
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
            return stability_gate.main(["verdict", path, "--polls", str(polls)])

    def test_cli_exit_zero_on_pass(self):
        self.assertEqual(self._run(REAL_LINES, 3), 0)

    def test_cli_exit_one_on_fail(self):
        self.assertEqual(self._run(REAL_LINES[:1], 3), 1)

    def test_cli_exit_one_on_missing_file(self):
        self.assertEqual(stability_gate.main(["verdict", "/nonexistent/clock.jsonl", "--polls", "3"]), 1)


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
        self.assertEqual(stability_gate.gated_run(["ddd", "aaa"], self.runs_for)[0], "aaa")

    def test_a_dispatch_run_gates_its_commit(self):
        self.assertEqual(stability_gate.gated_run(["ccc"], self.runs_for)[0], "ccc")

    def test_a_schedule_run_is_never_evidence(self):
        # A schedule run only dispatches the gate on dev; its build/gate/verdict are skipped and
        # the run still ends `success` on the master head. It must never release that tree.
        self.assertIsNone(stability_gate.gated_run(["bbb"], self.runs_for))

    def test_a_failed_run_is_not_evidence(self):
        self.assertIsNone(stability_gate.gated_run(["eee"], self.runs_for))

    def test_no_candidates_is_refused(self):
        self.assertIsNone(stability_gate.gated_run([], self.runs_for))

    def test_each_sha_is_looked_up_once(self):
        calls = []

        def runs_for(sha):
            calls.append(sha)
            return []

        stability_gate.gated_run(["ddd", "ddd", "eee"], runs_for)
        self.assertEqual(calls, ["ddd", "eee"])

    def test_commits_with_tree(self):
        log = "m1 T1\nd1 T1\nd0 T0\n\ngarbage\n"
        self.assertEqual(stability_gate.commits_with_tree(log, "T1"), ["m1", "d1"])
        self.assertEqual(stability_gate.commits_with_tree(log, "T9"), [])

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
        rc = stability_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run)
        self.assertEqual(rc, 0)
        self.assertEqual(seen[0], ["git", "log", "--all", "--format=%H %T"])
        self.assertIn("repos/o/r/actions/workflows/gate.yml/runs?head_sha=merge", seen[1][2])
        self.assertIn("status=success", seen[1][2])

    def test_release_check_cli_refuses_an_ungated_tree(self):
        run, _ = self._fake({"old": [{"conclusion": "success", "event": "push"}]})
        self.assertEqual(stability_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)

    def test_release_check_cli_refuses_a_schedule_only_tree(self):
        run, _ = self._fake({"merge": [{"conclusion": "success", "event": "schedule"}]})
        self.assertEqual(stability_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)

    def test_release_check_fails_closed_on_an_api_error(self):
        def run(args):
            if args[0] == "git":
                return "merge T2\n"
            raise stability_gate.GhError("HTTP 502")

        self.assertEqual(stability_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)

    def test_release_check_fails_closed_without_git_history(self):
        def run(args):
            raise stability_gate.GhError("not a git repository")

        self.assertEqual(stability_gate.main(["release-check", "--repo", "o/r", "--tree", "T2"], run=run), 1)


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
        self.assertIn('sudo -n "$GATE_INSTALL" --cold "$PWD/candidate/dsync"', installs[0])

    def test_the_gate_runs_on_the_renamed_runner_and_builds_dsync_147(self):
        # dsync #147: the runner label is dsync-gate (airuleset added it), the daemon is `dsync`.
        self.assertIn("name: Stability Gate", self.text)
        self.assertIn("runs-on: [dsync-gate, companion]", self.text)
        self.assertIn("cargo build --release --bin dsync\n", self.text)
        self.assertEqual(self.text.count("runs-on: [dsync-gate"), 1)

    def test_the_release_check_looks_up_this_workflow_file_147(self):
        # The release coupling finds gate runs by FILE name: renaming gate.yml would break it.
        path = os.path.join(os.path.dirname(__file__), "..", ".github", "workflows",
                            stability_gate.GATE_WORKFLOW)
        self.assertTrue(os.path.isfile(path), path)
        self.assertEqual(stability_gate.GATE_WORKFLOW, "gate.yml")

    def test_there_is_exactly_one_restore_call(self):
        restores = [c for c in self.helper_calls if "--restore" in c]
        self.assertEqual(len(restores), 1, restores)

    def _restore_step(self):
        """The `run: |` body of the "Restore previous dsync" step, dedented."""
        lines = self.text.splitlines()
        i = next(n for n, l in enumerate(lines) if l.strip() == "- name: Restore previous dsync")
        i = next(n for n in range(i, len(lines)) if lines[n].strip() == "run: |")
        indent = len(lines[i]) - len(lines[i].lstrip()) + 2
        body = []
        for l in lines[i + 1:]:
            if l.strip() and len(l) - len(l.lstrip()) < indent:
                break
            body.append(l[indent:])
        return "\n".join(body) + "\n"

    def _run_restore(self, held_calls, wait_min="45", timer_restores=False):
        """Run the step against a fake helper that exits 3 `held_calls` times, then 0 (#223)."""
        import subprocess
        import tempfile

        with tempfile.TemporaryDirectory() as d:
            pending = os.path.join(d, "pending")
            helper = os.path.join(d, "helper")
            with open(helper, "w") as f:
                f.write(
                    "#!/bin/bash\n"
                    'n=$(cat "$CNT" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$CNT"\n'
                    'if [ "$n" -le "$HELD" ]; then\n'
                    '  if [ "$TIMER" = 1 ] && [ "$n" -gt 1 ]; then rm -f "$RESTORE_PENDING"; exit 3; fi\n'
                    '  touch "$RESTORE_PENDING"; exit 3\n'
                    "fi\n"
                    'rm -f "$RESTORE_PENDING"; exit 0\n'
                )
            bindir = os.path.join(d, "bin")
            os.mkdir(bindir)
            with open(os.path.join(bindir, "sudo"), "w") as f:
                f.write('#!/bin/bash\n[ "$1" = "-n" ] && shift\nexec "$@"\n')
            with open(os.path.join(bindir, "sleep"), "w") as f:
                f.write("#!/bin/bash\nexit 0\n")
            for x in (helper, os.path.join(bindir, "sudo"), os.path.join(bindir, "sleep")):
                os.chmod(x, 0o755)
            step = os.path.join(d, "step.sh")
            with open(step, "w") as f:
                f.write(self._restore_step())
            env = dict(os.environ, GATE_INSTALL=helper, CNT=os.path.join(d, "cnt"),
                       HELD=str(held_calls), TIMER="1" if timer_restores else "0",
                       RESTORE_PENDING=pending, RESTORE_WAIT_MIN=wait_min,
                       GITHUB_STEP_SUMMARY=os.path.join(d, "summary"),
                       PATH=bindir + os.pathsep + os.environ["PATH"])
            r = subprocess.run(["bash", "-e", step], env=env, capture_output=True, text=True)
            with open(os.path.join(d, "cnt")) as f:
                calls = int(f.read())
            return r.returncode, r.stdout, calls

    def test_a_free_lease_restores_at_once_223(self):
        rc, out, calls = self._run_restore(0)
        self.assertEqual((rc, calls), (0, 1), out)
        self.assertIn("previous dsync restored", out)
        self.assertNotIn("Restore queued", out)

    def test_a_held_lease_is_retried_until_it_frees_223(self):
        rc, out, calls = self._run_restore(2)
        self.assertEqual((rc, calls), (0, 3), out)
        self.assertEqual(out.count("::warning title=Restore queued::"), 1, out)
        self.assertIn("(the lease freed)", out)

    def test_the_helper_timer_restoring_first_counts_as_done_223(self):
        rc, out, calls = self._run_restore(99, timer_restores=True)
        self.assertEqual((rc, calls), (0, 2), out)
        self.assertIn("restored by the helper's timer", out)

    def test_a_lease_held_past_the_wait_is_red_223(self):
        rc, out, calls = self._run_restore(99, wait_min="0")
        self.assertEqual(rc, 1, out)
        self.assertIn("::error title=Restore still queued::", out)

    def test_never_on_pull_request(self):
        self.assertNotRegex(self.text, r"(?m)^\s*pull_request(_target)?\s*:")


if __name__ == "__main__":
    unittest.main()
