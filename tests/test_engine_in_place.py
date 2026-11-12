"""An in-place run (`run.worktree = false`) never sweeps or commits the operator's own
untracked files (ruling R48'): the files untracked at start are recorded and left alone."""
import json
import unittest

from fakeworld import FakeWorld, card
from gitfixture import run_git, write
from leadhand import engine
from leadhand.state import append_event

CODEX_CARD = "codex gpt-6-luna low"
IN_PLACE = "[run]\nworktree = false\n"


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


class Exclusive(unittest.TestCase):
    """R53: an in-place run claims the checkout; a second in-place run of the repository is
    refused while the first is unfinished. Worktree runs are unaffected."""

    def test_second_in_place_run_is_refused_while_the_first_is_unfinished(self):
        w = FakeWorld(self, [card(1)], config=IN_PLACE)
        first = w.start()
        w.write_deck([card(1)], branch="leadhand/other")
        with self.assertRaises(engine.EngineError) as cm:
            w.start()
        self.assertIn(str(first), str(cm.exception))
        self.assertEqual(run_git(w.repo, "symbolic-ref", "--short", "HEAD"), "leadhand/demo")
        self.assertFalse(run_git(w.repo, "branch", "--list", "leadhand/other"))
        self.assertEqual(sorted(p.name for p in (w.docs / "runs").iterdir()), [first.name])
        with self.assertRaises(engine.EngineError):
            w.start(new=True)

        # A blocked run is still unfinished (it continues); a finished one frees the checkout.
        w.run_dir = first
        append_event(w.paths, "outcome", outcome="blocked", qualifier="paused", reasons=[])
        with self.assertRaises(engine.EngineError):
            w.start()
        append_event(w.paths, "resumed", note="")
        append_event(w.paths, "outcome", outcome="complete", qualifier="", reasons=[])
        second = w.start()
        self.assertNotEqual(second, first)
        self.assertEqual(w.meta()["branch"], "leadhand/other")

    def test_worktree_runs_are_not_held_back_by_an_in_place_run(self):
        w = FakeWorld(self, [card(1)], config=IN_PLACE)
        w.start()
        w.config_file.write_text(f'docs_dir = "{w.docs}"\n', encoding="utf-8")
        w.write_deck([card(1)], branch="leadhand/other")
        second = w.start()
        self.assertEqual(w.run_dir, second)
        self.assertNotEqual(w.workspace().resolve(), w.repo.resolve())


if __name__ == "__main__":
    unittest.main()
