"""`run` and `launch-line` in Rust: the conventions for the commands a person types (the run
found from the current directory, `--json`, bare `leadhand` when PATH finds this binary), a
CLI path with a space in it (R28), and the run-lock descriptor, which reaches `claude` alone.
Rust-only.

With no argument, the command takes the run whose worktree contains the current directory,
else the only unfinished run of the repository containing it, else it lists the candidates,
each with its exact command, and exits 1 (spec, "Commands you type"). `launch-line` prints
the binary's absolute path, or bare `leadhand` when the `leadhand` found on PATH is this same
binary (spec, "Calls made in planning"); the Python always prints its path.

The descriptors (spec, bug class 6): every file Rust opens is close-on-exec, and the launcher
clears the flag on run.lock only in `claude`'s pre-exec step, so the keep-awake tool beside it
never holds the run lock; like Python's `Popen(close_fds=True)`, neither child gets anything
else the launcher inherited. The fakes list their open descriptors from /dev/fd (no `lsof`).
"""
import json
import os
import shlex
import sys
import unittest
from pathlib import Path

import harness
from contract_launch import AGENT, FIRST_MESSAGE_END, World
from harness import BIN, read_jsonl, run_argv, run_cli, rust_only, temp_root
from rundir import append_event

ATTENDED = ("leadhand: attended use only. Until the Claude-side run guard lands, keep this session "
            "in view and run it only on fixture and trial repositories.\n"
            "leadhand: the supervisor runs with --permission-mode auto. Its --settings overlay holds one "
            "sandbox.excludedCommands entry, the leadhand CLI ({cli} *); whether it merges with or "
            "replaces your own excludedCommands is not established.\n")
SPACES = ("leadhand: warning: the leadhand CLI path contains whitespace ({cli}); the sandbox exclusion "
          "may not match; install leadhand under a path without spaces.\n")
KEEP_AWAKE = "caffeinate" if sys.platform == "darwin" else "systemd-inhibit"

# Lists this process's open descriptors from /dev/fd, each with the device and inode it names:
# {"fd": n, "dev": d, "ino": i}. The listing's own descriptor is closed again before the fstat.
LIST_FDS = """
def open_fds():
    out = []
    for name in os.listdir("/dev/fd"):
        try:
            st = os.fstat(int(name))
        except OSError:
            continue
        out.append({"fd": int(name), "dev": st.st_dev, "ino": st.st_ino})
    return out
"""
# A fake `claude` that logs its descriptors, its LEADHAND_LOCK_FD and its pid, then waits (at
# most 15 s) for the keep-awake tool's line, so the tool runs while the supervisor does.
FD_CLAUDE = """#!/usr/bin/env python3
import json, os, sys, time
""" + LIST_FDS + """
with open(os.environ["FAKE_CLAUDE_LOG"], "a", encoding="utf-8") as f:
    f.write(json.dumps({"time": time.time(), "pid": os.getpid(), "ppid": os.getppid(), "argv": sys.argv,
                        "lock_fd": os.environ.get("LEADHAND_LOCK_FD"), "fds": open_fds()}) + "\\n")
deadline = time.monotonic() + 15
while not os.path.exists(os.environ["FAKE_KEEP_AWAKE_LOG"]) and time.monotonic() < deadline:
    time.sleep(0.02)
"""
# A keep-awake tool that logs its descriptors, then runs until SIGTERM.
FD_TOOL = """#!/usr/bin/env python3
import json, os, signal, sys, time
""" + LIST_FDS + """
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
fds = open_fds()
with open(os.environ["FAKE_KEEP_AWAKE_LOG"] + ".tmp", "w", encoding="utf-8") as f:
    f.write(json.dumps({"time": time.time(), "pid": os.getpid(), "argv": sys.argv, "fds": fds}) + "\\n")
os.rename(os.environ["FAKE_KEEP_AWAKE_LOG"] + ".tmp", os.environ["FAKE_KEEP_AWAKE_LOG"])
time.sleep(60)
"""


def self_cli(w: World) -> str:
    """The path the binary names itself by: the router's link behind the router (ruling S2-R5),
    else the binary."""
    return w.env.get("LEADHAND_TEST_CLI") or str(BIN)


def line_of(r) -> str:
    r.ok()
    assert r.stdout.endswith("\n") and r.stdout.count("\n") == 1, r
    return r.stdout[:-1]


@rust_only(uses=("run", "launch-line"))
class LaunchLine(unittest.TestCase):
    def setUp(self):
        self.w = World(self)
        self.deck = str(self.w.paths.deck)

    def test_launch_line_without_argument_selects_by_cwd(self):
        w = self.w
        want = line_of(w.launch_line(w.run_dir))
        self.assertTrue(harness.is_bin_command(want, "", " " + shlex.join(["run", self.deck])), want)
        # From inside the run's worktree (its top or a subdirectory), and from the repository,
        # whose only unfinished run it is.
        deep = w.workspace / "deep dir"
        deep.mkdir()
        for cwd in (w.workspace, deep, w.repo):
            with self.subTest(cwd=cwd):
                self.assertEqual(line_of(w.launch_line(cwd=cwd)), want)
        # By run ID, directory name and slug, from the repository.
        meta = w.sup.meta()
        for arg in (meta["run_id"], w.run_dir.name, meta["slug"]):
            with self.subTest(arg=arg):
                self.assertEqual(line_of(w.launch_line(arg, cwd=w.repo)), want)
        # `run` finds it the same way: from the worktree it launches this run.
        r = run_cli("run", env=w.env, cwd=deep, timeout=60)
        self.assertEqual(r.code, 0, r)
        self.assertEqual(w.starts()[0]["env"]["LEADHAND_RUN_DIR"], str(w.run_dir))

        # Two unfinished runs of the repository: each candidate is listed with its own command.
        other = w.sup.start(new=True)
        r = w.launch_line(cwd=w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stdout, "", r)
        lines = r.stderr.splitlines()
        self.assertTrue(lines[0].startswith("error: 2 unfinished runs of "), r)
        for run_dir in (w.run_dir, other):
            self.assertTrue(any(harness.is_bin_command(line, "", f" launch-line {shlex.quote(str(run_dir))}")
                                for line in lines[1:]), (run_dir, r))
        # Inside the first run's worktree it is still that run.
        self.assertEqual(line_of(w.launch_line(cwd=deep)), want)

    def test_launch_line_json(self):
        w = self.w
        plugin = w.tmp / "plugin dir"
        for args in ((w.run_dir,), (w.paths.deck, "--plugin-dir", plugin)):
            with self.subTest(args=args):
                text = line_of(w.launch_line(*args))
                r = w.launch_line(*args, "--json").ok()
                self.assertEqual(r.stderr, "", r)
                self.assertEqual(r.stdout.count("\n"), 1, r)
                self.assertTrue(r.stdout.isascii(), r)
                self.assertEqual(r.json(), {"command": text}, r)
        # A run that cannot be found is the usual error, not JSON.
        r = w.launch_line(w.elsewhere, "--json")
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stdout, "", r)
        self.assertTrue(r.stderr.startswith("error: not a run directory or a run's deck.md: "), r)

    def test_launch_line_prints_bare_leadhand_when_path_resolves_here(self):
        w = self.w
        on_path = w.tmp / "on path"
        on_path.mkdir()
        (on_path / "leadhand").symlink_to(self_cli(w))
        self.assertTrue(harness.names_bin(str(on_path / "leadhand")))
        env = dict(w.env, PATH=f"{on_path}{os.pathsep}{w.env['PATH']}")
        r = run_cli("launch-line", w.run_dir, env=env, cwd=w.elsewhere)
        self.assertEqual(r.stdout, shlex.join(["leadhand", "run", self.deck]) + "\n", r)
        r = run_cli("launch-line", w.run_dir, "--json", env=env, cwd=w.elsewhere).ok()
        self.assertEqual(r.json(), {"command": shlex.join(["leadhand", "run", self.deck])}, r)
        # The launch itself still names the binary by its absolute path.
        r = run_cli("run", w.run_dir, env=env, cwd=w.elsewhere, timeout=60)
        self.assertEqual(r.code, 0, r)
        cli = w.starts()[0]["env"]["LEADHAND_CLI"]
        self.assertTrue(os.path.isabs(cli) and harness.names_bin(cli), cli)

        # A `leadhand` on PATH that is some other program: the absolute path, as without one.
        (on_path / "leadhand").unlink()
        (on_path / "leadhand").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        (on_path / "leadhand").chmod(0o755)
        line = line_of(run_cli("launch-line", w.run_dir, env=env, cwd=w.elsewhere))
        self.assertTrue(harness.is_bin_command(line, "", " " + shlex.join(["run", self.deck])), line)
        self.assertNotEqual(shlex.split(line)[0], "leadhand")


@rust_only(uses=("run", "launch-line"))
class Paths(unittest.TestCase):
    def test_cli_path_with_space_warns_and_quotes(self):
        """Covers R28 and Review Focus 5: a real CLI path with a space in it (a link to the
        binary in a spaced directory) launches, warns, and is quoted wherever it is a word."""
        w = World(self)
        link = temp_root(self, "lh cli ") / "lead hand bin" / "leadhand"
        link.parent.mkdir()
        link.symlink_to(self_cli(w))
        self.assertTrue(harness.names_bin(str(link)))
        w.env["LEADHAND_TEST_CLI"] = cli = str(link)
        deck = str(w.paths.deck)

        r = w.run()
        self.assertEqual(r.code, 0, r)
        self.assertEqual(r.stderr, ATTENDED.format(cli=cli) + SPACES.format(cli=cli), r)
        start = w.starts()[0]
        argv = start["argv"]
        self.assertEqual(argv[1:], ["--settings", argv[2], "--agent", AGENT, "--model", "sonnet",
                                    "--effort", "low", "--permission-mode", "auto", argv[-1]])
        self.assertEqual(json.loads(argv[2]), {"sandbox": {"excludedCommands": [f"{cli} *"]}})
        self.assertEqual(argv[-1], f"Run the leadhand deck in {w.run_dir}. leadhand CLI: {cli}{FIRST_MESSAGE_END}")
        self.assertEqual(start["env"]["LEADHAND_CLI"], cli)

        # The launch line quotes the path as one word, and runs as printed.
        line = line_of(w.launch_line(w.run_dir))
        self.assertEqual(line, shlex.join([cli, "run", deck]))
        self.assertEqual(shlex.split(line), [cli, "run", deck])
        r = run_argv(shlex.split(line), env=w.env, cwd=w.elsewhere, timeout=60)
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(w.starts()), 2)
        self.assertEqual(w.starts()[1]["env"]["LEADHAND_RUN_DIR"], str(w.run_dir))

        # A run that has ended: the refusal names the start command, the spaced path quoted.
        append_event(w.run_dir, "outcome", outcome="complete", qualifier="", reasons=[])
        meta = w.sup.meta()
        r = w.run()
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stderr, "error: this run has ended: complete; only a blocked or unfinished run "
                                   "continues. Start a new run from the deck with: "
                                   + shlex.join([cli, "start", meta["deck_source"], "--repo", meta["repo"]]) + "\n",
                         r)
        self.assertEqual(len(w.starts()), 2)


@unittest.skipUnless(sys.platform == "darwin" or sys.platform.startswith("linux"),
                     "keep-awake runs only on macOS and Linux")
@rust_only(uses=("run", "launch-line"))
class Descriptors(unittest.TestCase):
    def test_lock_fd_reaches_claude_but_not_caffeinate(self):
        """The platform's keep-awake tool (`caffeinate` on macOS, `systemd-inhibit` on Linux) runs
        beside the supervisor without run.lock; `claude` has it, at LEADHAND_LOCK_FD. A
        descriptor the launcher inherited reaches neither; the tool's stdin, stdout and stderr
        are /dev/null."""
        w = World(self)
        (w.bindir / "claude").unlink()
        w.tool("claude", FD_CLAUDE)
        for name in ("caffeinate", "systemd-inhibit"):
            w.tool(name, FD_TOOL)
        stray = w.tmp / "inherited.txt"
        stray.write_text("x", encoding="utf-8")
        fd = os.open(stray, os.O_RDONLY)
        try:
            launcher = w.popen([BIN, "run", w.run_dir], env=w.env, pass_fds=(fd,))
        finally:
            os.close(fd)
        r = w.finish(launcher, timeout=30)
        self.assertEqual(r.code, 0, r)

        lock = os.stat(w.paths.lock)
        lock_id = (lock.st_dev, lock.st_ino)
        stray_st = os.stat(stray)
        stray_id = (stray_st.st_dev, stray_st.st_ino)
        null = os.stat(os.devnull)

        (sup,) = w.records()
        self.assertEqual(sup["ppid"], launcher.pid)
        held = {(d["dev"], d["ino"]): d["fd"] for d in sup["fds"]}
        self.assertIn(lock_id, held, sup)
        self.assertEqual(str(held[lock_id]), sup["lock_fd"], sup)
        self.assertEqual([d["fd"] for d in sup["fds"] if (d["dev"], d["ino"]) == lock_id], [held[lock_id]])
        self.assertNotIn(stray_id, held, sup)

        recs = read_jsonl(w.keep_awake_log)
        self.assertEqual([Path(rec["argv"][0]).name for rec in recs], [KEEP_AWAKE])
        tool = {d["fd"]: (d["dev"], d["ino"]) for d in recs[0]["fds"]}
        self.assertNotIn(lock_id, tool.values(), recs[0])
        self.assertNotIn(stray_id, tool.values(), recs[0])
        self.assertEqual([tool.get(n) for n in (0, 1, 2)], [(null.st_dev, null.st_ino)] * 3, recs[0])
        self.assertIsNone(harness.started_by(recs[0]["pid"], recs[0]["time"]), "keep-awake left running")
        # The launch released the lock once the supervisor exited: the run launches again.
        r = w.run()
        self.assertEqual(r.code, 0, r)


if __name__ == "__main__":
    unittest.main()
