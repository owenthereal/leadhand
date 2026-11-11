"""`card_review = "by-risk"` never accepts a READY that changed nothing without a review
(final review I8)."""
import unittest

from fakeworld import FakeWorld, card
from gitfixture import write
from leadhand import engine

BY_RISK = '[run]\ncard_review = "by-risk"\n'


class ByRisk(unittest.TestCase):
    def test_ready_at_the_attempt_start_tip_goes_to_review(self):
        w = FakeWorld(self, [card(1, risk="low")], config=BY_RISK)
        w.start()
        a = w.next()
        worker = w.launch(a)
        # The implementer writes a file but never commits it, and says READY.
        write(w.workspace(), {"src/uncommitted.txt": "x\n"})
        engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                      reply="Status: READY\nReport: none")
        nxt = w.next()
        self.assertEqual(w.events("accepted"), [])
        self.assertEqual((nxt["action"], nxt["kind"]), ("dispatch", "review"))

    def test_low_risk_ready_with_commits_is_still_accepted_without_review(self):
        w = FakeWorld(self, [card(1, risk="low")], config=BY_RISK)
        w.start()
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])


if __name__ == "__main__":
    unittest.main()
