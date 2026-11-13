"""dsync #179: the node watch's pure grade, dedup bucket and roster."""

import os
import subprocess
import tempfile
import unittest

import node_watch as w

GOOD = {"is_locked": True, "mode": "NANO", "ptp_phase_error_us": -3.2,
        "ntp_step_storm": False, "version": "1.35.0"}


def with_(**kw):
    s = dict(GOOD)
    s.update(kw)
    return s


class Grade(unittest.TestCase):
    def test_a_locked_current_node_is_ok(self):
        self.assertEqual(w.grade(GOOD, "1.35.0", False, True), ("OK", []))
        self.assertEqual(w.grade(with_(mode="LOCK"), "1.35.0", False, True)[0], "OK")

    def test_each_fault_alarms_with_its_reason(self):
        cases = [
            (with_(is_locked=False), "not locked"),
            (with_(mode="ACQ"), "mode ACQ"),
            (with_(ptp_phase_error_us=50.0), "|e| 50.0 us >= 50"),
            (with_(ptp_phase_error_us=-61.5), "|e| 61.5 us >= 50"),
            (with_(ptp_phase_error_us=None), "no phase error"),
            (with_(ntp_step_storm=True), "ntp step storm"),
            (with_(version="1.34.0"), "version 1.34.0 < latest 1.35.0"),
        ]
        for status, reason in cases:
            with self.subTest(reason=reason):
                self.assertEqual(w.grade(status, "1.35.0", False, True), ("ALARM", [reason]))

    def test_a_newer_candidate_is_not_upgrade_debt(self):
        """#179: the stability gate installs the dev candidate (newer than the latest release) on
        companion for an hour; that is not debt, only an older version is."""
        self.assertEqual(w.grade(with_(version="1.35.1"), "1.35.0", False, True), ("OK", []))
        self.assertEqual(w.grade(with_(version="1.36.0"), "1.35.10", False, True), ("OK", []))
        self.assertEqual(w.grade(with_(version="1.35.9"), "1.35.10", False, True),
                         ("ALARM", ["version 1.35.9 < latest 1.35.10"]))

    def test_just_inside_the_bound_is_ok(self):
        self.assertEqual(w.grade(with_(ptp_phase_error_us=49.99), "1.35.0", False, True)[0], "OK")

    def test_a_pre_135_node_without_a_version_field_is_upgrade_debt(self):
        s = dict(GOOD)
        del s["version"]
        self.assertEqual(w.grade(s, "1.35.0", False, True),
                         ("ALARM", ["version <1.35 < latest 1.35.0"]))

    def test_unreadable_fixed_node_alarms(self):
        self.assertEqual(w.grade(None, "1.35.0", False, True),
                         ("ALARM", ["/status unreadable on :8898"]))

    def test_a_traveling_node_away_is_skipped_never_alarmed(self):
        self.assertEqual(w.grade(None, "1.35.0", True, False), ("SKIP", ["away (:22 silent)"]))

    def test_a_traveling_node_present_but_dsync_down_alarms(self):
        self.assertEqual(w.grade(None, "1.35.0", True, True)[0], "ALARM")

    def test_several_faults_are_all_named(self):
        verdict, reasons = w.grade(with_(is_locked=False, mode="ACQ"), "1.35.0", False, True)
        self.assertEqual(verdict, "ALARM")
        self.assertEqual(reasons, ["not locked", "mode ACQ"])


TOML_OLD = '[package]\nname = "dsync"\nversion = "1.35.2"\n\n[dependencies]\nanyhow = "1.0"\n'
TOML_NEW = TOML_OLD.replace('version = "1.35.2"', 'version = "1.35.3"')
LOCK_OLD = ('[[package]]\nname = "anyhow"\nversion = "1.0.90"\n\n'
            '[[package]]\nname = "dsync"\nversion = "1.35.2"\n\n'
            '[[package]]\nname = "dsync-ptp"\nversion = "1.35.2"\n')
LOCK_NEW = LOCK_OLD.replace('"1.35.2"', '"1.35.3"')


class CodeDebt(unittest.TestCase):
    """#186 (dstack 13.11): version debt is a node older than the latest CODE release; a
    docs-only release (v1.35.3: rules + the version bump) is no debt."""

    def test_our_own_version_lines_are_not_code(self):
        self.assertFalse(w.code_changed([("Cargo.toml", TOML_OLD, TOML_NEW),
                                         ("crates/dsync-ptp/Cargo.toml", TOML_OLD, TOML_NEW),
                                         ("Cargo.lock", LOCK_OLD, LOCK_NEW),
                                         (".claude/rules/clock-discipline-and-testing.md", "a", "b"),
                                         ("scripts/node_watch.py", "a", "b"),
                                         ("README.md", "a", "b")]))

    def test_source_and_dependency_changes_are_code(self):
        self.assertTrue(w.code_changed([("src/status.rs", "a", "b")]))
        self.assertTrue(w.code_changed([("crates/dsync-ptp/src/ptp_phase_lock.rs", "a", "b")]))
        self.assertTrue(w.code_changed([("Cargo.lock", LOCK_OLD,
                                         LOCK_NEW.replace('"1.0.90"', '"1.0.91"'))]))
        self.assertTrue(w.code_changed([("Cargo.toml", TOML_OLD,
                                         TOML_NEW.replace('anyhow = "1.0"', 'anyhow = "1.1"'))]))

    def test_the_grade_asks_the_code_debt_only_for_an_older_version(self):
        asked = []

        def debt(have, latest):
            asked.append((have, latest))
            return False

        self.assertEqual(w.grade(with_(version="1.35.2"), "1.35.3", False, True, debt), ("OK", []))
        self.assertEqual(asked, [("1.35.2", "1.35.3")])
        self.assertEqual(w.grade(with_(version="1.35.3"), "1.35.3", False, True, debt), ("OK", []))
        self.assertEqual(len(asked), 1, "a current node needs no diff")
        self.assertEqual(w.grade(with_(version="1.35.1"), "1.35.3", False, True,
                                 lambda h, l: True),
                         ("ALARM", ["version 1.35.1 < latest code 1.35.3"]))

    def test_code_debt_reads_the_tags_of_a_real_repo(self):
        with tempfile.TemporaryDirectory() as repo:
            def git(*args):
                subprocess.run(["git", "-C", repo, "-c", "user.name=t", "-c", "user.email=t@t",
                                *args], check=True, capture_output=True)

            def release(ver, files):
                for path, text in files.items():
                    full = os.path.join(repo, path)
                    os.makedirs(os.path.dirname(full), exist_ok=True)
                    with open(full, "w") as f:
                        f.write(text)
                git("add", "-A")
                git("commit", "-qm", ver)
                git("tag", "v" + ver)

            git("init", "-q")
            release("1.0.0", {"Cargo.toml": TOML_OLD, "src/a.rs": "fn a() {}\n"})
            release("1.0.1", {"Cargo.toml": TOML_NEW, "docs/x.md": "docs\n"})
            release("1.0.2", {"Cargo.toml": TOML_NEW.replace("1.35.3", "1.35.4"),
                              "src/a.rs": "fn a() { b() }\n"})
            self.assertFalse(w.code_debt(repo, "1.0.0", "1.0.1"))
            self.assertTrue(w.code_debt(repo, "1.0.1", "1.0.2"))
            self.assertTrue(w.code_debt(repo, "1.0.0", "1.0.2"))
            self.assertTrue(w.code_debt(repo, "0.9.0", "1.0.1"), "an unknown tag is debt")


class Dedup(unittest.TestCase):
    def test_the_key_holds_within_a_bucket_and_changes_on_the_next(self):
        t = 1_000_000 * 600.0
        self.assertEqual(w.dedup_key("mbc", t), w.dedup_key("mbc", t + 599))
        self.assertNotEqual(w.dedup_key("mbc", t), w.dedup_key("mbc", t + 600))
        self.assertNotEqual(w.dedup_key("mbc", t), w.dedup_key("fohabl", t))


class Roster(unittest.TestCase):
    def test_the_roster_is_the_seven_dsync_nodes_and_only_two_travel(self):
        """#183: the iemmixer box (iem-gen3.lan, FOH rack, fixed) is dsync's too."""
        self.assertEqual([n.name for n in w.ROSTER],
                         ["mbc", "fohabl", "iem-snv", "companion-snv", "iemmixer", "alex-nb",
                          "resolume-pp"])
        self.assertEqual(w.ROSTER[4].addr, "iem-gen3.lan")
        self.assertEqual({n.name for n in w.ROSTER if n.travel}, {"alex-nb", "resolume-pp"})

    def test_the_alert_names_node_address_and_reasons(self):
        self.assertEqual(w.alert_text(w.ROSTER[0], ["not locked", "mode ACQ"]),
                         "dsync watch: mbc (mbc.lan) ALARM: not locked; mode ACQ")


class LatestRelease(unittest.TestCase):
    """#179: under systemd the watch has no gh token (gh exits 4), so the latest release comes
    from the public `releases/latest` redirect, which needs no API and no token."""

    def test_the_redirect_location_names_the_latest_version(self):
        self.assertEqual(w.version_from_latest_location(
            "https://github.com/zbynekdrlik/dsync/releases/tag/v1.35.0"), "1.35.0")

    def test_a_location_that_is_not_a_release_tag_fails_loud(self):
        for loc in (None, "", "https://github.com/zbynekdrlik/dsync/releases",
                    "https://github.com/zbynekdrlik/dsync/releases/tag/latest"):
            with self.subTest(loc=loc), self.assertRaises(RuntimeError):
                w.version_from_latest_location(loc)


class Cli(unittest.TestCase):
    def test_unknown_arguments_are_a_usage_error(self):
        self.assertEqual(w.main(["--bogus"]), 2)


if __name__ == "__main__":
    unittest.main()
