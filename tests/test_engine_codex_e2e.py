"""End to end with a Codex worker that never commits, as the real one (ruling R39, final
review I11, ruling R47): the runner commits its work and the run completes with it."""
import unittest

from fakeworld import FakeWorld, card
from gitfixture import run_git

RUN = "leadhand/demo"
CODEX_CARD = "codex gpt-6-luna low"


class NonCommittingCodex(unittest.TestCase):
    def test_two_card_run_with_gates_completes_with_the_runners_commits(self):
        w = FakeWorld(self, [card(1, route="claude sonnet low"), card(2, depends=[1], route=CODEX_CARD)],
                      installed=("claude", "codex"), gates=True)
        w.start()
        w.script(2, "implement", {"commit": False})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2])
        (codex,) = [e for e in w.events("result_recorded") if e["dispatch_id"] == "c2-a1-implement-1"]
        self.assertTrue(codex.get("committed"), codex)
        head = w.branch_tip(RUN)
        run_git(w.repo, "merge-base", "--is-ancestor", codex["committed"], head)
        body = run_git(w.repo, "log", "-1", "--format=%B", codex["committed"])
        self.assertIn("Leadhand-Dispatch: c2-a1-implement-1", body.splitlines())
        files = run_git(w.repo, "diff", "--name-only", w.base, head).splitlines()
        self.assertEqual(sorted(files), ["src/card1/c1-a1-implement-1.txt", "src/card2/c2-a1-implement-1.txt"])
        gates = {e["gate"]: e for e in w.events("gate_result")}
        self.assertTrue(all(g["passed"] and g["examined_sha"] == head for g in gates.values()), gates)


if __name__ == "__main__":
    unittest.main()
