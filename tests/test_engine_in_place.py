"""An in-place run (`run.worktree = false`) never sweeps or commits the operator's own
untracked files (ruling R48'): the files untracked at start are recorded and left alone."""
import contextlib
import io
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path
from unittest import mock

from fakeworld import FakeWorld, card
from gitfixture import run_git, write
from leadhand import engine
from leadhand.state import append_event, run_paths

CODEX_CARD = "codex gpt-6-luna low"
IN_PLACE = "[run]\nworktree = false\n"
FAKE_CLAUDE = Path(__file__).resolve().parent / "fakes" / "claude"
LIB = Path(__file__).resolve().parents[1] / "scripts" / "lib"


def supervise(test, run_dir: Path) -> subprocess.Popen:
    """Launch the run (`leadhand run`) with tests/fakes/claude as a supervisor that sleeps
    60 s; it is stopped at cleanup. Returns once holder.json names the supervisor."""
    code = ("import sys; from pathlib import Path; from leadhand import launch; "
            "sys.exit(launch.run(Path(sys.argv[1]), claude_bin=sys.argv[2]))")
    err = run_dir.parent / f"{run_dir.name} launcher.err"
    with open(err, "wb") as f:
        p = subprocess.Popen([sys.executable, "-B", "-c", code, str(run_dir), str(FAKE_CLAUDE)],
                             env={**os.environ, "PYTHONPATH": str(LIB), "FAKE_CLAUDE_SLEEP": "60"},
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=f)

    def stop():
        if p.poll() is None:
            p.terminate()
        p.wait(timeout=15)
    test.addCleanup(stop)
    holder = run_paths(run_dir).holder
    deadline = time.monotonic() + 15
    while not (holder.exists() and json.loads(holder.read_text(encoding="utf-8")).get("supervisor_pid")):
        if p.poll() is not None or time.monotonic() > deadline:
            raise AssertionError(f"the supervisor of {run_dir} did not start: {err.read_text()}")
        time.sleep(0.02)
    return p


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

    def test_undecodable_untracked_filename_survives_start(self):
        # Git lists a non-UTF-8 name surrogateescape-decoded; run state must still be written.
        w = FakeWorld(self, [card(1)], config=IN_PLACE)
        name = b"notes-\xff.txt".decode("utf-8", "surrogateescape")
        try:
            (w.repo / name).write_text("operator notes\n", encoding="utf-8")
            listing = contextlib.nullcontext()
        except OSError:
            # The file system only takes UTF-8 names (APFS): have Git's listing carry it instead.
            real = engine.untracked_files
            listing = mock.patch.object(engine, "untracked_files", lambda ws: [*real(ws), name])
        with listing:
            w.start()
        recorded = json.loads((w.run_dir / "untracked-at-start.json").read_text(encoding="utf-8"))
        self.assertEqual(recorded["paths"], [name])
        self.assertEqual(w.meta()["branch"], "leadhand/demo")
        self.assertEqual(w.next()["action"], "dispatch")

    def test_json_output_escapes_undecodable_names(self):
        # A surrogate-escaped name (Git's listing of a non-UTF-8 file name) must not break the
        # CLI's JSON output on a strict UTF-8 stdout.
        name = b"notes-\xff.txt".decode("utf-8", "surrogateescape")
        raw = io.BytesIO()
        out = io.TextIOWrapper(raw, encoding="utf-8", errors="strict")
        with contextlib.redirect_stdout(out):
            engine._print_json({"path": name, "title": "caf\u00e9"})
        out.flush()
        self.assertEqual(json.loads(raw.getvalue()), {"path": name, "title": "caf\u00e9"})


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

    def test_in_place_start_is_refused_while_a_run_in_another_runs_folder_is_supervised(self):
        w = FakeWorld(self, [card(1)], config=IN_PLACE)
        first = w.start()
        launcher = supervise(self, first)
        other_docs = w.tmp / "other docs"
        w.config_file.write_text(f'docs_dir = "{other_docs}"\n{IN_PLACE}', encoding="utf-8")
        w.write_deck([card(1)], branch="leadhand/other")
        with self.assertRaises(engine.EngineError) as cm:
            w.start()
        self.assertIn(f"another in-place run of {w.repo.resolve()} is being supervised: {first}",
                      str(cm.exception))
        self.assertEqual(run_git(w.repo, "symbolic-ref", "--short", "HEAD"), "leadhand/demo")
        self.assertFalse(run_git(w.repo, "branch", "--list", "leadhand/other"))
        self.assertFalse(other_docs.exists())

        # Once its supervisor stops, the checkout is free again.
        launcher.terminate()
        launcher.wait(timeout=15)
        second = w.start()
        self.assertTrue(str(second).startswith(str(other_docs)))
        self.assertEqual(run_git(w.repo, "symbolic-ref", "--short", "HEAD"), "leadhand/other")

    def test_start_returns_the_unfinished_run_while_its_own_supervisor_holds_the_checkout(self):
        w = FakeWorld(self, [card(1)], config=IN_PLACE)
        first = w.start()
        supervise(self, first)
        self.assertEqual(w.start(), first)

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
