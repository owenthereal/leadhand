import re
import shlex
import unittest

from leadhand.dispatch import CLI_PATH
from leadhand import cli, report, state, status
from runworld import RunWorld

SECTIONS = ["## Needs your attention", "## Result", "## Evidence", "## Unresolved", "## Cards", "## Rulings",
            "## Usage", "## Routing suggestions"]


def finished(test):
    w = RunWorld(test, cards=2)
    d1 = w.dispatch(1)
    w.attempt(d1, 1)
    w.accept(1, sha="1" * 40)
    d2 = w.dispatch(2, kind="implement", vendor="claude", model="sonnet", effort="medium")
    w.attempt(d2, 2, vendor="claude", model="sonnet", effort="medium", status="SPENT")
    w.accept(2, sha="2" * 40)
    w.ev("ruling", card=2, dispatch_id=d2, text="use the small API")
    w.ev("gate_result", gate="tests", examined_sha="2" * 40, passed=True)
    w.ev("outcome", outcome="complete", qualifier="", reasons=[])
    return w


class ReportTests(unittest.TestCase):
    def test_report_outcome_is_line_3_after_title(self):
        w = finished(self)
        lines = report.render_report(w.root, sessions_dir=w.sessions).splitlines()
        self.assertEqual(lines[0], "# Run report: Demo deck")
        self.assertEqual(lines[1], "")
        self.assertEqual(lines[2], "Outcome: complete")

    def test_outcome_qualifier(self):
        w = RunWorld(self, cards=1)
        w.ev("outcome", outcome="blocked", qualifier="paused", reasons=["blocked: paused"])
        self.assertEqual(report.render_report(w.root, sessions_dir=w.sessions).splitlines()[2],
                         "Outcome: blocked (paused)")

    def test_report_sections_in_order(self):
        w = finished(self)
        text = report.render_report(w.root, sessions_dir=w.sessions)
        headings = [l for l in text.splitlines() if l.startswith("## ")]
        self.assertEqual(headings, SECTIONS)
        self.assertIn("leadhand/demo-2", text)
        self.assertIn(str(w.root.parent / "work space"), text)
        self.assertIn("use the small API", text)
        self.assertIn("tests", text.split("## Evidence")[1].split("## Unresolved")[0])
        self.assertIn("2" * 12, text.split("## Evidence")[1].split("## Unresolved")[0])

    def test_incomplete_report_puts_parked_cards_under_attention(self):
        w = RunWorld(self, cards=2)
        w.accept(1)
        w.ev("card_cut", card=2, branch="b2", base="b")
        w.ev("parked", card=2, reason="needs a decision")
        w.ev("outcome", outcome="incomplete", qualifier="", reasons=["card 2 parked: needs a decision"])
        text = report.render_report(w.root, sessions_dir=w.sessions)
        attention = text.split("## Needs your attention")[1].split("## Result")[0]
        self.assertIn("Card 2 parked: needs a decision", attention)
        self.assertEqual(text.splitlines()[2], "Outcome: incomplete")

    def test_report_usage_includes_supervisor_and_proxy_and_unknown_count(self):
        w = finished(self)
        w.charge("s1", "supervisor", tokens=(5000, 500))
        w.charge("p1", "proxy", tokens=(200, 20))
        w.charge("i1", "implementer", source="codex-stream", model="gpt-6.1-terra", possibly_incomplete=True)
        w.attempt("dx", 2, usage_known=False)
        w.rate_limit(20.0, ts="2026-01-02T03:04:05.000Z")
        w.rate_limit(31.0, ts="2026-01-03T03:04:05.000Z", name="rollout-2026-01-03-abc.jsonl")
        # the meter at the start of the run was recorded by an earlier status write
        (w.root / "meter-start.json").write_text('{"percent": 20.0, "observed_at": "2026-01-02T03:04:05.000Z"}')
        text = report.render_report(w.root, sessions_dir=w.sessions)
        usage = text.split("## Usage")[1].split("## Routing suggestions")[0]
        for needle in ("supervisor", "proxy", "implementer", "claude", "codex"):
            self.assertIn(needle, usage)
        self.assertRegex(usage, r"unknown usage: 1")
        self.assertIn("may be undercounted", usage)
        self.assertIn("Claude subscription meter: unknown", usage)
        self.assertIn("20.0%", usage)
        self.assertIn("31.0%", usage)

    def test_routing_counts_worker_attempts_not_proxy(self):
        w = RunWorld(self, cards=1)
        d = w.dispatch(1)
        w.attempt(d, 1, role="implementer", status="READY")
        w.attempt(d, 1, role="proxy", vendor="claude", model="haiku", effort="low", status="READY",
                  recorded_by="engine")
        text = report.render_report(w.root, sessions_dir=w.sessions)
        routing = text.split("## Routing suggestions")[1]
        self.assertIn("implementer · codex gpt-6.1-terra high: 1 READY", routing)
        self.assertNotIn("proxy", routing)

    def test_write_report_and_cli(self):
        w = finished(self)
        p = report.write_report(w.root, sessions_dir=w.sessions)
        self.assertEqual(p, w.paths.report)
        self.assertTrue(p.read_text().startswith("# Run report: Demo deck\n\nOutcome: complete\n"))
        import contextlib, io
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(cli.main(["report", str(w.root)]), 0)
        self.assertIn("report.md", out.getvalue())


class FixRoundTests(unittest.TestCase):
    def test_unresolved_lists_parked_cards_with_branches_and_open_findings(self):
        w = RunWorld(self, cards=3)
        w.ev("card_cut", card=2, branch="leadhand/demo-2/c2", base="b")
        w.ev("parked", card=2, reason="needs a decision")
        w.ev("card_cut", card=3, branch="leadhand/demo-2/c3", base="b")
        w.ev("verdict_recorded", card=3, verdict="defects", findings="/runs/x/findings.md")
        text = report.render_report(w.root, sessions_dir=w.sessions)
        unresolved = text.split("## Unresolved")[1].split("## Cards")[0]
        self.assertIn("Card 2 (branch leadhand/demo-2/c2) parked", unresolved)
        self.assertIn("needs a decision", unresolved)
        self.assertIn("Card 3 (branch leadhand/demo-2/c3) has open review findings: /runs/x/findings.md",
                      unresolved)

    def test_attention_has_next_action_for_parked_cards(self):
        w = RunWorld(self, cards=1)
        w.ev("card_cut", card=1, branch="b", base="b")
        w.ev("parked", card=1, reason="why")
        text = report.render_report(w.root, sessions_dir=w.sessions)
        expected = (f"- Card 1 parked: why — next: {shlex.quote(str(CLI_PATH))} unpark "
                    f"{shlex.quote(str(w.root))} --card 1 --note '…'")
        self.assertIn(expected, text.split("## Needs your attention")[1].split("## Result")[0].splitlines())

    def test_evidence_shows_last_result_per_gate_and_marks_stale(self):
        w = RunWorld(self, cards=2)
        w.accept(1, sha="1" * 40)
        w.ev("gate_result", gate="tests", examined_sha="1" * 40, passed=False)
        w.ev("gate_result", gate="tests", examined_sha="1" * 40, passed=True)
        w.ev("gate_result", gate="lint", examined_sha="1" * 40, passed=True)
        w.accept(2, sha="2" * 40)
        ev = report.render_report(w.root, sessions_dir=w.sessions).split("## Evidence")[1].split("## Unresolved")[0]
        self.assertEqual(ev.count("- tests:"), 1)
        self.assertIn("- tests: passed on commit " + "1" * 12 + " (stale: the run head is " + "2" * 12 + ")", ev)
        w.ev("gate_result", gate="lint", examined_sha="2" * 40, passed=True)
        ev = report.render_report(w.root, sessions_dir=w.sessions).split("## Evidence")[1].split("## Unresolved")[0]
        self.assertIn("- lint: passed on commit " + "2" * 12 + "\n", ev)

    def test_report_is_written_atomically_without_leftover_temp_files(self):
        w = finished(self)
        report.write_report(w.root, sessions_dir=w.sessions)
        report.write_report(w.root, sessions_dir=w.sessions)
        self.assertEqual([p.name for p in w.root.iterdir() if p.name.endswith(".tmp")], [])

    def test_world_is_hermetic_about_codex_home(self):
        import os
        w = RunWorld(self, cards=1)
        self.assertTrue(os.environ["CODEX_HOME"].startswith(str(w.root.parent)))


if __name__ == "__main__":
    unittest.main()
