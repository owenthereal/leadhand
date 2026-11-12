"""The run branch moves only by acceptance (final review I4).

Every implementer-side dispatch guards the run branch like an integrate step: a worker that
moves it is a violation (preserved, restored, the step re-run). Each step and the outcome
also check that the run head is the last accepted commit (else the base); a mismatch no
guard explains fails the run (unreconciled)."""
import unittest

from fakeworld import FakeWorld, card
from gitfixture import run_git, write
from leadhand import engine

RUN = "leadhand/demo"


def stray(world, ws, a):
    """The worker also commits on the run branch, then returns to its card branch."""
    current = run_git(ws, "symbolic-ref", "--short", "HEAD")
    run_git(ws, "checkout", "--quiet", RUN)
    write(ws, {"src/stray.txt": "unreviewed\n"})
    run_git(ws, "add", "--", "src/stray.txt")
    run_git(ws, "commit", "--quiet", "-m", "stray commit on the run branch")
    run_git(ws, "checkout", "--quiet", current)


def run_log(w):
    return run_git(w.repo, "log", "--format=%s", f"{w.base}..refs/heads/{RUN}").splitlines()


class ImplementerGuard(unittest.TestCase):
    def test_implementer_commit_on_the_run_branch_is_a_violation(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"tamper": stray, "status": "QUESTION"})
        w.parks[1] = "decision needed: storage format"
        w.run()
        found = [e for e in w.events("violation") if e.get("stage") == "found" and e.get("by") == "check"]
        self.assertEqual([e["dispatch_id"] for e in found], ["c1-a1-implement-1"])
        self.assertIn(f"refs/heads/{RUN}", found[0]["refs"])
        kept = found[0]["preserved"][f"refs/heads/{RUN}"]
        self.assertEqual(run_git(w.repo, "log", "-1", "--format=%s", kept), "stray commit on the run branch")
        self.assertNotIn("stray commit on the run branch", run_log(w))
        self.assertIn("c1-a1-implement-2", w.dispatch_ids())
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])
        (done,) = w.events("outcome")
        self.assertEqual(done["outcome"], "complete", done["reasons"])


class RunHeadCheck(unittest.TestCase):
    def test_run_branch_moved_between_dispatches_fails_the_run(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        a = w.next()
        w.act(a)
        self.assertEqual(w.next()["action"], "answer")
        engine.record(w.run_dir, "park", card=1, reason="operator: skip")
        # Someone moves the run branch while no worker is out.
        ws = w.workspace()
        run_git(ws, "update-ref", f"refs/heads/{RUN}", _commit_on(w.repo, w.base))
        done = w.next()
        self.assertEqual((done["action"], done["outcome"]), ("done", "failed"), done)
        self.assertTrue(any("run branch" in r for r in done["reasons"]), done["reasons"])

    def test_outcome_checks_the_run_head_after_a_cancel(self):
        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        stray(w, w.workspace(), a)               # the worker commits on the run branch, then is cancelled
        w.cli("cancel", w.run_dir)
        self.assertEqual(w.next()["action"], "stop_worker")
        engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                      reply="Status: SPENT (time limit)", stopped=True)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"]), ("done", "failed"), done)


def _commit_on(repo, parent):
    """A commit with `parent` as its parent, made without touching any branch or worktree."""
    tree = run_git(repo, "rev-parse", f"{parent}^{{tree}}")
    return run_git(repo, "commit-tree", tree, "-p", parent, "-m", "outside commit")


if __name__ == "__main__":
    unittest.main()
