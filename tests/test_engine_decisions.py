"""A run whose every remaining card waits on a decision ends blocked (decision needed), and
`unpark` continues it (final review I2, ruling R44)."""
import os
import unittest

from fakeworld import FAKES, FakeWorld, card
from gitfixture import write
from leadhand import engine, report, status
from leadhand.engine import EngineError

RELAUNCH_ENV = {"PATH": f"{FAKES}{os.pathsep}{os.environ['PATH']}"}


class DecisionNeeded(unittest.TestCase):
    def test_question_parked_on_a_decision_ends_blocked_and_unpark_continues(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "decision needed: which storage format"
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"), done["reasons"])
        self.assertIn("card 1 parked: decision needed: which storage format", done["reasons"])
        self.assertTrue(w.paths.handoff.is_file())
        self.assertEqual([e["card"] for e in w.events("accepted")], [2])
        s = status.build_status(w.run_dir, meter=(None, None))
        self.assertTrue(any("unpark" in a for a in s["attention"]), s["attention"])
        # The operator answers and relaunches: the run continues and completes.
        r = w.cli("unpark", w.run_dir, "--card", "1", "--note", "Use JSON.")
        self.assertEqual(r.returncode, 0, r.stderr)
        del w.parks[1]
        r = w.cli("run", w.run_dir, env=RELAUNCH_ENV)
        self.assertEqual(r.returncode, 0, r.stderr)
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])

    def test_card_waiting_on_a_decision_card_still_ends_blocked(self):
        w = FakeWorld(self, [card(1), card(2, depends=[1]), card(3)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "decision needed: which storage format"
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"), done["reasons"])

    def test_violation_cap_park_ends_blocked_for_a_decision(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": f"rewritten by {a['dispatch_id']}\n"})

        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(1, "review", *({"tamper": tamper} for _ in range(5)))
        done = w.run()[-1]
        (parked,) = w.events("parked")
        self.assertTrue(parked.get("cap_dispatch"))
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"), done["reasons"])
        self.assertTrue(w.paths.handoff.is_file())

    def test_park_that_is_not_a_decision_still_ends_incomplete(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.start()
        a = w.next()
        w.act(a)
        engine.record(w.run_dir, "park", card=1, reason="operator: skip")
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "incomplete", done["reasons"])

    def test_unpark_refuses_on_a_run_it_cannot_reopen_and_hides_the_hint(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.start()
        a = w.next()
        w.act(a)
        engine.record(w.run_dir, "park", card=1, reason="operator: skip")
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "incomplete")
        with self.assertRaises(EngineError) as cm:
            engine.unpark(w.run_dir, card=1, note="go on")
        self.assertIn("incomplete", str(cm.exception))
        self.assertEqual(w.events("unparked"), [])
        r = w.cli("unpark", w.run_dir, "--card", "1", "--note", "go on")
        self.assertEqual(r.returncode, 1)
        s = status.build_status(w.run_dir, meter=(None, None))
        self.assertTrue(any(a.startswith("Card 1 parked: operator: skip") for a in s["attention"]), s["attention"])
        self.assertFalse(any("unpark" in a for a in s["attention"]), s["attention"])
        text = report.render_report(w.run_dir)
        self.assertNotIn(" unpark ", text.split("## Needs your attention")[1].split("## Result")[0])


if __name__ == "__main__":
    unittest.main()
