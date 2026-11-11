"""Untracked files a worker did not create never land in its card's commits (final review I3).

The engine preserves and clears every non-ignored untracked file another worker left before
it switches cards or dispatches a check (on the leftovers ref, with the rest of the
leftovers); the Codex runner and the cancel's partial-work commit commit only tracked
changes and the paths that appeared during the dispatch."""
import json
import os
import unittest

from fakeworld import FakeWorld, card
from gitfixture import run_git, write
from leadhand import dispatch as dsp
from leadhand import engine
from test_dispatch import Base, CARD_BRANCH

CODEX_CARD = "codex gpt-6-luna low"


def leave_untracked(world, ws, a):
    write(ws, {"src/card1/half-done.py": "# card 1's unfinished, uncommitted new file\n"})


class EngineSide(unittest.TestCase):
    def test_parked_cards_untracked_file_does_not_land_via_the_next_codex_card(self):
        w = FakeWorld(self, [card(1, route="claude sonnet low"), card(2, route=CODEX_CARD)],
                      installed=("claude", "codex"), gates=True)
        w.start()
        w.script(1, "implement", {"tamper": leave_untracked, "status": "QUESTION"})
        w.parks[1] = "decision needed: storage format"
        w.script(2, "implement", {"commit": False})      # like real Codex: the runner commits
        w.run()
        run = w.meta()["branch"]
        self.assertEqual([e["card"] for e in w.events("accepted")], [2])
        landed = run_git(w.repo, "diff", "--name-only", w.base, f"refs/heads/{run}").splitlines()
        self.assertEqual(landed, ["src/card2/c2-a1-implement-1.txt"])
        # The file is kept on a leftovers ref, and gone from the workspace.
        found = [e for e in w.events("violation") if e.get("by") == "implementer" and e.get("stage") == "found"]
        self.assertTrue(any("src/card1/half-done.py" in (e.get("untracked") or []) for e in found), found)
        restored = [e for e in w.events("violation") if e.get("by") == "implementer" and e.get("stage") == "restored"]
        kept = [e["saved"] for e in restored if e.get("saved")]
        self.assertTrue(any(run_git(w.repo, "cat-file", "-t", f"{sha}:src/card1/half-done.py") == "blob"
                            for sha in kept if _has(w.repo, f"{sha}:src/card1/half-done.py")), kept)
        self.assertFalse((w.workspace() / "src" / "card1" / "half-done.py").exists())

    def test_cancel_partial_commit_leaves_untracked_files_that_were_there_before(self):
        def build_output(world, ws, a):
            write(ws, {"build/out.txt": "reviewer's build output\n"})

        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "review", {"tamper": build_output, "verdict": "defects"})
        w.act(w.next())                          # implement
        w.act(w.next())                          # review: defects, leaves untracked build output
        fix = w.next()
        self.assertEqual(fix["kind"], "fix")
        worker = w.launch(fix)
        ws = w.workspace()
        write(ws, {"src/card1/fix-partial.txt": "half a fix\n"})
        w.cli("cancel", w.run_dir)
        self.assertEqual(w.next()["action"], "stop_worker")
        event = engine.record(w.run_dir, "result", dispatch_id=fix["dispatch_id"], worker_id=worker,
                              reply="Status: SPENT (time limit)", stopped=True)
        self.assertTrue(event.get("partial_commit"), event)
        files = run_git(w.repo, "show", "--name-only", "--format=", event["partial_commit"]).splitlines()
        self.assertEqual(files, ["src/card1/fix-partial.txt"])
        self.assertTrue((ws / "build" / "out.txt").exists())


def _has(repo, spec):
    try:
        run_git(repo, "cat-file", "-e", spec)
        return True
    except Exception:
        return False


class RunnerSide(Base):
    def setUp(self):
        super().setUp()
        run_git(self.repo, "checkout", "--quiet", "-b", CARD_BRANCH)
        exclude = run_git(self.repo, "rev-parse", "--path-format=absolute", "--git-common-dir")
        os.makedirs(os.path.join(exclude, "info"), exist_ok=True)
        with open(os.path.join(exclude, "info", "exclude"), "w") as f:
            f.write(".leadhand/\n")

    def test_runner_commits_only_tracked_changes_and_files_created_during_the_dispatch(self):
        write(self.repo, {"stray/left by another card.txt": "not this dispatch's\n"})
        req = self.request(kind="implement", branch=CARD_BRANCH)
        path = self.tmp / "scenario.jsonl"
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "write_file", "path": "README.md", "text": "changed\n"},
                 {"action": "write_file", "path": "src/new.py", "text": "NEW = 1\n"},
                 {"action": "last_message", "text": "Status: READY\n"},
                 {"type": "turn.completed", "usage": {"input_tokens": 1, "cached_input_tokens": 0,
                                                       "output_tokens": 1}}]
        path.write_text("".join(json.dumps(x) + "\n" for x in lines))
        os.environ["FAKE_CODEX_SCENARIO"] = str(path)
        result = dsp.dispatch(req)
        self.assertEqual(result.status, "READY")
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        files = run_git(self.repo, "show", "--name-only", "--format=", saved["committed"]).splitlines()
        self.assertEqual(sorted(files), ["README.md", "src/new.py"])
        self.assertTrue((self.repo / "stray" / "left by another card.txt").exists())
        self.assertIn("?? stray/", run_git(self.repo, "status", "--porcelain"))


if __name__ == "__main__":
    unittest.main()
