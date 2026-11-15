"""`pause`, `cancel` and `unpark` in Rust: the conventions for the commands a person types. Rust-only.

Each finds its run as `status` does (spec, "Commands you type"): by its directory, its `deck.md`, its run
ID, its directory's name or its slug, or with nothing named, the run whose worktree contains the current
directory, else the only unfinished run of the repository containing it, else it lists the candidates, each
with its exact command, and exits 1. Each takes `--json`: the same information as the text output, as one
JSON object. An error's first line is `error: ...`; when one command fixes it, that command is alone on the
last line.

The flags, events and texts themselves are the Python's (contract_recovery, contract_finish): a Python
process may still read them in a mixed configuration.
"""
import json
import shlex
import unittest

from contract_rust_status import other_repos_run
from harness import is_bin_command, run_cli, rust_only
from rundir import append_event
from supervisor import FakeSupervisor, card

PAUSE_HEAD = ("pause requested: running workers finish, nothing new starts, and the run ends blocked (paused) "
              "with a handoff. Continue it later with: ")
CANCEL_HEAD = ("cancel requested: every running worker is stopped and the run ends incomplete (cancelled); "
               "partial work stays on the card branches. If no supervisor is running, ")
CANCEL_TAIL = " finishes the cancel."
NOTE = "Use JSON; it is simpler."


def flag_text(request: str, run_dir) -> tuple[str, str]:
    """What the text output of `request` (pause or cancel) is around the CLI's own path."""
    run = f" run {shlex.quote(str(run_dir))}"
    return (PAUSE_HEAD, run) if request == "pause" else (CANCEL_HEAD, run + CANCEL_TAIL)


def parked_world(test: unittest.TestCase) -> FakeSupervisor:
    """A run whose card 1 is dispatched, then parked by the supervisor."""
    w = FakeSupervisor(test, [card(1), card(2)])
    w.start()
    w.next()
    w.cli("record", w.run_dir, "park", "--card", "1", "--reason", "hold").ok()
    return w


@rust_only(uses=("pause", "cancel", "unpark", "status"))
class ControlFindsTheRun(unittest.TestCase):
    def test_pause_and_cancel_without_argument_take_the_current_repos_only_unfinished_run(self):
        for request in ("pause", "cancel"):
            with self.subTest(request=request):
                w = FakeSupervisor(self, [card(1)])
                run_dir = w.start()
                elsewhere = other_repos_run(self, w.docs)
                flag = w.paths.pause_flag if request == "pause" else w.paths.cancel_flag
                sub = w.repo / "src deep"
                sub.mkdir()
                deep = w.workspace() / "deep dir"
                deep.mkdir()
                # From the repository's top, a subdirectory of it, and inside the run's worktree: never
                # the other repository's newer unfinished run.
                for cwd in (w.repo, sub, w.workspace(), deep):
                    with self.subTest(cwd=cwd):
                        r = run_cli(request, env=w.env, cwd=cwd).ok()
                        self.assertEqual(r.stderr, "", r)
                        before, after = flag_text(request, run_dir)
                        self.assertTrue(is_bin_command(r.stdout.rstrip("\n"), before, after), r)
                        self.assertTrue(flag.is_file(), r)
                        self.assertFalse((elsewhere / request).exists())
                        written = json.loads(flag.read_text(encoding="utf-8"))
                        self.assertEqual(set(written), {"requested_at", "by_pid"})
                        flag.unlink()

    def test_unpark_without_argument_takes_the_run_of_the_worktree_it_is_in(self):
        w = parked_world(self)
        deep = w.workspace() / "deep dir"
        deep.mkdir()
        r = run_cli("unpark", "--card", "1", "--note", NOTE, env=w.env, cwd=deep).ok()
        self.assertEqual(r.stdout, "card 1 unparked\n", r)
        (event,) = w.events("unparked")
        self.assertEqual((event["card"], event["note"]), (1, NOTE))

    def test_each_command_names_a_run_by_id_name_slug_or_deck(self):
        w = parked_world(self)
        run_dir = w.run_dir
        meta = w.meta()
        names = (meta["run_id"], run_dir.name, meta["slug"], run_dir / "deck.md")
        for arg in names:
            with self.subTest(arg=arg):
                for request, flag in (("pause", w.paths.pause_flag), ("cancel", w.paths.cancel_flag)):
                    r = run_cli(request, arg, env=w.env, cwd=w.repo).ok()
                    before, after = flag_text(request, run_dir)
                    self.assertTrue(is_bin_command(r.stdout.rstrip("\n"), before, after), r)
                    flag.unlink()
        for arg in names:
            with self.subTest(arg=arg):
                if w.events("unparked"):
                    w.cli("record", w.run_dir, "park", "--card", "1", "--reason", "hold again").ok()
                run_cli("unpark", arg, "--card", "1", "--note", NOTE, env=w.env, cwd=w.repo).ok()
        self.assertEqual(len(w.events("unparked")), len(names))
        r = run_cli("pause", "no-such-run", env=w.env, cwd=w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: no run named no-such-run"), r)

    def test_candidates_are_listed_with_their_own_commands_when_ambiguous(self):
        w = FakeSupervisor(self, [card(1)])
        first = w.start()
        second = w.start(new=True)
        self.assertNotEqual(first, second)
        for request, after in (("pause", ""), ("cancel", ""), ("unpark", " --card 1 --note " + shlex.quote(NOTE))):
            with self.subTest(request=request):
                extra = ["--card", "1", "--note", NOTE] if request == "unpark" else []
                r = run_cli(request, *extra, env=w.env, cwd=w.repo)
                self.assertEqual(r.code, 1, r)
                self.assertEqual(r.stdout, "", r)
                lines = r.stderr.splitlines()
                self.assertTrue(lines[0].startswith("error: 2 unfinished runs of "), r)
                commands = lines[1:]
                self.assertEqual(len(commands), 2, r)
                for run_dir in (first, second):
                    want = f" {request} {shlex.quote(str(run_dir))}{after}"
                    self.assertTrue(any(is_bin_command(line, "", want) for line in commands), (run_dir, r))
                self.assertFalse((first / "pause").exists() or (second / "pause").exists())
                self.assertFalse((first / "cancel").exists() or (second / "cancel").exists())
        # A printed command works as printed.
        r = run_cli("pause", env=w.env, cwd=w.repo)
        argv = shlex.split(r.stderr.splitlines()[1])
        run_cli(*argv[1:], env=w.env, cwd=w.tmp).ok()
        self.assertEqual(sum((d / "pause").exists() for d in (first, second)), 1)


@rust_only(uses=("pause", "cancel", "unpark", "status"))
class Json(unittest.TestCase):
    def assert_one_object(self, r):
        self.assertEqual(r.stderr, "", r)
        self.assertEqual(r.stdout.count("\n"), 1, r)
        self.assertTrue(r.stdout.isascii(), r)
        return r.json()

    def test_pause_and_cancel_json(self):
        for request in ("pause", "cancel"):
            with self.subTest(request=request):
                w = FakeSupervisor(self, [card(1)])
                run_dir = w.start()
                flag = w.paths.pause_flag if request == "pause" else w.paths.cancel_flag
                text = run_cli(request, run_dir, env=w.env, cwd=w.tmp).ok().stdout.rstrip("\n")
                flag.unlink()
                s = self.assert_one_object(run_cli(request, run_dir, "--json", env=w.env, cwd=w.tmp).ok())
                self.assertTrue(flag.is_file())
                self.assertEqual(set(s), {"run", "requested", "command", "message"}, s)
                self.assertEqual((s["run"], s["requested"], s["message"]), (str(run_dir), request, text))
                # The command the text names, alone.
                self.assertTrue(is_bin_command(s["command"], "", f" run {shlex.quote(str(run_dir))}"), s)
                self.assertIn(s["command"], text)

    def test_unpark_json(self):
        note = "Use JSON; keep the décision \U0001f600"
        w = parked_world(self)
        s = self.assert_one_object(run_cli("unpark", w.run_dir, "--card", "1", "--note", f"  {note}\n", "--json",
                                           env=w.env, cwd=w.tmp).ok())
        self.assertEqual(s, {"run": str(w.run_dir), "card": 1, "note": note, "message": "card 1 unparked"})
        (event,) = w.events("unparked")
        self.assertEqual((event["card"], event["note"]), (1, note))

    def test_an_error_with_json_is_still_an_error_line(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.next()
        r = run_cli("unpark", w.run_dir, "--card", "1", "--note", NOTE, "--json", env=w.env, cwd=w.tmp)
        self.assertEqual((r.code, r.stdout), (1, ""), r)
        self.assertEqual(r.stderr, "error: card 1 is not parked (it is requested)\n", r)


@rust_only(uses=("pause", "cancel", "unpark", "status"))
class Errors(unittest.TestCase):
    def test_unpark_on_a_run_that_cannot_continue_ends_with_the_start_command(self):
        w = parked_world(self)
        append_event(w.run_dir, "outcome", outcome="incomplete", qualifier="", reasons=[])
        r = run_cli("unpark", w.run_dir, "--card", "1", "--note", NOTE, env=w.env, cwd=w.tmp)
        self.assertEqual((r.code, r.stdout), (1, ""), r)
        lines = r.stderr.splitlines()
        self.assertEqual(lines[0], "error: this run has ended: incomplete; a parked card can only continue in a "
                                   "blocked or unfinished run. Start a new run from the deck with:", r)
        self.assertEqual(len(lines), 2, r)
        meta = w.meta()
        want = " " + shlex.join(["start", meta["deck_source"], "--repo", meta["repo"]])
        self.assertTrue(is_bin_command(lines[1], "", want), r)
        self.assertEqual(w.events("unparked"), [])
        # The command works as printed: it starts a new run of the deck.
        argv = shlex.split(lines[1])
        r = run_cli(*argv[1:], env=w.env, cwd=w.tmp).ok()
        self.assertIn("run: ", r.stdout, r)

    def test_pause_and_cancel_on_an_ended_run_are_one_error_line(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        append_event(w.run_dir, "outcome", outcome="complete", qualifier="", reasons=[])
        for request in ("pause", "cancel"):
            with self.subTest(request=request):
                r = run_cli(request, w.run_dir, env=w.env, cwd=w.tmp)
                self.assertEqual((r.code, r.stdout), (1, ""), r)
                self.assertEqual(r.stderr, f"error: this run has ended: complete; there is nothing to {request}\n", r)
                self.assertFalse((w.run_dir / request).exists())

    def test_cancel_of_a_blocked_run_is_requested_and_pause_is_refused(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        append_event(w.run_dir, "outcome", outcome="blocked", qualifier="paused", reasons=[])
        r = run_cli("pause", w.run_dir, env=w.env, cwd=w.tmp)
        self.assertEqual(r.stderr, "error: this run has ended: blocked (paused); there is nothing to pause\n", r)
        run_cli("cancel", w.run_dir, env=w.env, cwd=w.tmp).ok()
        self.assertTrue(w.paths.cancel_flag.is_file())

    def test_a_directory_that_is_not_a_run_is_refused(self):
        w = FakeSupervisor(self, [card(1)])
        for request in ("pause", "cancel"):
            with self.subTest(request=request):
                r = run_cli(request, w.tmp, env=w.env, cwd=w.tmp)
                self.assertEqual(r.stderr, f"error: not a run directory (no run.json): {w.tmp}\n", r)


if __name__ == "__main__":
    unittest.main()
