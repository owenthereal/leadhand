"""`status` and `report` in Rust: the run found from the current directory (deliberate change 3)
and the conventions for the commands a person types (`--json`, candidates listed with their
commands). Rust-only.

With no argument, the command takes the run whose worktree contains the current directory, else
the only unfinished run of the repository containing it, else it lists the candidates, each with
its exact command, and exits 1 (spec, "Commands you type"). The reference-only test
`contract_status.StatusWithoutRunDir` pins the Python's old choice: the newest unfinished run
under `docs_dir`, from any repository.
"""
import json
import os
import shlex
import shutil
import unittest

from pathlib import Path

from harness import is_bin_command, read_json, run_cli, rust_only
from rundir import RunDir, append_event
from supervisor import FakeSupervisor, card

KEYS = {"run_id", "deck", "state", "headline", "activity", "progress", "attention", "cards", "usage",
        "updated_at"}


def other_repos_run(test, docs) -> Path:
    """An unfinished run under `docs`/runs that belongs to another repository (RunDir's own,
    which does not exist), updated after everything else there: the run the Python's old
    choice would show."""
    other = RunDir(test, cards=3)
    dest = docs / "runs" / "2099-01-01-elsewhere"
    shutil.copytree(other.root, dest)
    os.utime(dest / "events.jsonl")
    return dest


@rust_only(uses=("status", "report"))
class StatusFindsTheRun(unittest.TestCase):
    def status(self, w, *args, cwd):
        return run_cli("status", *args, env=w.env, cwd=cwd)

    def test_status_without_argument_picks_the_current_repos_only_unfinished_run(self):
        w = FakeSupervisor(self, [card(1, title="Mine")])
        run_dir = w.start()
        elsewhere = other_repos_run(self, w.docs)
        named = self.status(w, run_dir, cwd=w.tmp).ok()
        # From the repository's top and from a subdirectory of it, never the other repository's
        # newer run.
        sub = w.repo / "src deep"
        sub.mkdir()
        for cwd in (w.repo, sub):
            with self.subTest(cwd=cwd):
                r = self.status(w, cwd=cwd).ok()
                self.assertEqual(r.stdout, named.stdout, r)
                self.assertIn("Card 1: Mine · ", r.stdout, r)
        self.assertNotEqual(self.status(w, elsewhere, cwd=w.tmp).ok().stdout, named.stdout)

        # A finished run of the same repository is not a candidate.
        append_event(run_dir, "outcome", outcome="complete", qualifier="", reasons=[])
        r = self.status(w, cwd=w.repo)
        self.assertEqual(r.code, 1, r)
        lines = r.stderr.splitlines()
        self.assertTrue(lines[0].startswith("error: no unfinished run of "), r)
        self.assertEqual(r.stdout, "", r)

    def test_status_lists_candidates_when_ambiguous(self):
        w = FakeSupervisor(self, [card(1)])
        first = w.start()
        second = w.start(new=True)
        self.assertNotEqual(first, second)
        r = self.status(w, cwd=w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stdout, "", r)
        lines = r.stderr.splitlines()
        self.assertTrue(lines[0].startswith("error: 2 unfinished runs of "), r)
        # Each candidate's own command, alone on its line, ready to copy, and nothing else.
        commands = lines[1:]
        self.assertEqual(len(commands), 2, r)
        for run_dir in (first, second):
            self.assertTrue(any(is_bin_command(line, "", f" status {shlex.quote(str(run_dir))}")
                                for line in commands), (run_dir, r))
        # The command works as printed.
        for line in commands:
            argv = shlex.split(line)
            done = run_cli(*argv[1:], env=w.env, cwd=w.tmp)
            self.assertEqual(done.code, 0, done)

        # `report` lists its own commands.
        r = run_cli("report", env=w.env, cwd=w.repo)
        self.assertEqual(r.code, 1, r)
        for run_dir in (first, second):
            self.assertTrue(any(is_bin_command(line, "", f" report {shlex.quote(str(run_dir))}")
                                for line in r.stderr.splitlines()[1:]), (run_dir, r))

    def test_status_inside_a_worktree_finds_its_run_by_scan(self):
        w = FakeSupervisor(self, [card(1, title="First run")])
        first = w.start()
        first_ws = w.workspace()
        w.write_deck([card(1, title="Second run")], title="Other deck")
        second = w.start(new=True)
        second_ws = w.workspace()
        self.assertNotEqual(first_ws, second_ws)
        # From the repository both runs are candidates; from inside a run's worktree (its top or
        # a subdirectory), that run.
        self.assertEqual(self.status(w, cwd=w.repo).code, 1)
        for run_dir, ws, title in ((first, first_ws, "First run"), (second, second_ws, "Second run")):
            deep = ws / "deep dir"
            deep.mkdir(exist_ok=True)
            for cwd in (ws, deep):
                with self.subTest(cwd=cwd):
                    r = self.status(w, cwd=cwd).ok()
                    self.assertEqual(r.stdout, self.status(w, run_dir, cwd=w.tmp).ok().stdout, r)
                    self.assertIn(f"Card 1: {title} · ", r.stdout, r)
            r = run_cli("report", env=w.env, cwd=deep).ok()
            self.assertEqual(r.stdout, f"{run_dir / 'report.md'}\n", r)

    def test_status_names_a_run_by_id_name_slug_or_deck(self):
        w = FakeSupervisor(self, [card(1)])
        run_dir = w.start()
        want = self.status(w, run_dir, cwd=w.tmp).ok().stdout
        meta = w.meta()
        for arg in (meta["run_id"], run_dir.name, meta["slug"], run_dir / "deck.md"):
            with self.subTest(arg=arg):
                r = self.status(w, arg, cwd=w.repo).ok()
                self.assertEqual(r.stdout, want, r)
        r = self.status(w, "no-such-run", cwd=w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: no run named no-such-run"), r)


@rust_only(uses=("status", "report"))
class Json(unittest.TestCase):
    def test_status_json(self):
        reason = "needs a décision \U0001f600"
        w = RunDir(self, cards=3)
        w.accept(1)
        w.ev("card_cut", card=2, branch="leadhand/demo-2/c2", base="b")
        w.ev("parked", card=2, reason=reason)
        w.dispatch(3)
        w.charge("c1", "implementer", tokens=(1000, 100))
        text = run_cli("status", w.root, env=w.env, cwd=w.root.parent).ok()
        r = run_cli("status", w.root, "--json", env=w.env, cwd=w.root.parent).ok()
        self.assertEqual(r.stderr, "", r)
        self.assertEqual(r.stdout.count("\n"), 1, r)
        self.assertTrue(r.stdout.isascii(), r)            # non-ASCII text as \u escapes
        s = r.json()
        self.assertEqual(set(s), KEYS, r)
        # The same information as the text output.
        lines = text.stdout.splitlines()
        self.assertEqual(s["headline"], lines[0])
        self.assertEqual(s["attention"], lines[1:1 + len(s["attention"])])
        self.assertTrue(s["attention"][0].startswith(f"Card 2 parked: {reason} — next: "), s)
        self.assertEqual((s["run_id"], s["deck"], s["state"]), ("abc123", "Demo deck", "running"))
        self.assertEqual(s["progress"], {"accepted": 1, "total": 3})
        self.assertEqual(s["cards"][1], {"number": 2, "title": "Card number 2", "state": "parked",
                                         "attempt": 0, "route": "", "note": reason})
        self.assertEqual(s["cards"][2]["route"], "codex gpt-6.1-terra high")
        self.assertEqual(set(s["usage"]), {"estimated_usd", "unknown_attempts", "by_role", "codex_meter"})
        self.assertEqual(s["usage"]["by_role"]["implementer"]["input"], 1000)
        self.assertEqual(s["usage"]["codex_meter"], {"percent": None, "observed_at": None})

    def test_status_json_matches_the_status_json_a_checkpoint_writes(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        run_dir = w.start()
        w.next()
        written = read_json(w.paths.status)
        s = run_cli("status", run_dir, "--json", env=w.env, cwd=w.tmp).ok().json()
        written.pop("updated_at")
        s.pop("updated_at")
        self.assertEqual(s, written)

    def test_report_json(self):
        w = RunDir(self, cards=1)
        w.accept(1)
        w.ev("outcome", outcome="complete", qualifier="", reasons=[])
        text = run_cli("report", w.root, env=w.env, cwd=w.root.parent).ok()
        self.assertEqual(text.stdout, f"{w.paths.report}\n", text)
        w.paths.report.unlink()
        r = run_cli("report", w.root, "--json", env=w.env, cwd=w.root.parent).ok()
        self.assertEqual(r.stderr, "", r)
        self.assertEqual(r.json(), {"report": str(w.paths.report)}, r)
        self.assertTrue(w.paths.report.read_text(encoding="utf-8")
                        .startswith("# Run report: Demo deck\n\nOutcome: complete\n"))
        self.assertEqual(json.loads(r.stdout), r.json())


if __name__ == "__main__":
    unittest.main()
