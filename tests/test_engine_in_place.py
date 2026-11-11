"""An in-place run (`run.worktree = false`) never sweeps or commits the operator's own
untracked files (ruling R48'): the files untracked at start are recorded and left alone."""
import json
import unittest

from fakeworld import FakeWorld, card
from gitfixture import run_git, write

CODEX_CARD = "codex gpt-6-luna low"


class InPlace(unittest.TestCase):
    def test_operators_untracked_files_survive_and_are_never_committed(self):
        w = FakeWorld(self, [card(1), card(2, route=CODEX_CARD)], installed=("claude", "codex"),
                      config="[run]\nworktree = false\n", gates=True)
        write(w.repo, {"my-notes.txt": "operator notes\n", "scratch/todo list.txt": "later\n"})
        w.start()
        self.assertEqual(w.workspace().resolve(), w.repo.resolve())
        recorded = json.loads((w.run_dir / "untracked-at-start.json").read_text())
        self.assertEqual(sorted(recorded["paths"]), ["my-notes.txt", "scratch/todo list.txt"])
        # Codex workers never commit (R39): the runner does, so card 1 (the preset's Codex
        # implementer route) and card 2 both go through the runner's staging.
        w.script(1, "implement", {"commit": False})
        w.script(2, "implement", {"commit": False})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual((w.repo / "my-notes.txt").read_text(), "operator notes\n")
        self.assertEqual((w.repo / "scratch" / "todo list.txt").read_text(), "later\n")
        landed = run_git(w.repo, "diff", "--name-only", w.base, f"refs/heads/{w.meta()['branch']}").splitlines()
        self.assertNotIn("my-notes.txt", landed)
        self.assertNotIn("scratch/todo list.txt", landed)
        swept = [p for e in w.events("violation") for p in e.get("untracked") or []]
        self.assertEqual(swept, [])


if __name__ == "__main__":
    unittest.main()
