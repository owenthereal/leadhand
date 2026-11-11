"""The launcher: run ownership, the supervisor's argv, environment and cwd, signals.

Every launch uses tests/fakes/claude by absolute path, never the real `claude`.
Launches that must outlive a test step (or be SIGKILLed) run in their own Python
process through `launch.run`; the rest run in-process.
"""
from __future__ import annotations

import contextlib
import fcntl
import io
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from leadhand import launch, procs
from leadhand.state import append_event, read_events, run_paths, write_json_atomic

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / "scripts" / "leadhand"
LIB = ROOT / "scripts" / "lib"
FAKE_CLAUDE = ROOT / "tests" / "fakes" / "claude"
RUN_ID = "a1b2c3d4e5f6"
WAIT = 15.0


def wait_for(predicate, what: str, timeout: float = WAIT):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {what}")


def snapshot(root: Path) -> dict:
    out = {}
    for path in sorted(root.rglob("*")):
        st = path.lstat()
        rel = str(path.relative_to(root))
        out[rel] = (st.st_mtime_ns, st.st_size, path.read_bytes() if path.is_file() else None)
    return out


class LaunchTestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="lh launch "))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = self.tmp / "my repo"
        self.workspace = self.repo / ".worktrees" / "demo run"
        self.workspace.mkdir(parents=True)
        self.elsewhere = self.tmp / "some other dir"
        self.elsewhere.mkdir()
        self.run_dir = self.tmp / "docs dir" / "runs" / "2026-11-10-demo run"
        self.run_dir.mkdir(parents=True)
        self.paths = run_paths(self.run_dir)
        self.paths.deck.write_text("# Demo deck\n", encoding="utf-8")
        write_json_atomic(self.paths.run_json, {
            "run_id": RUN_ID, "repo": str(self.repo), "workspace": str(self.workspace),
            "deck_source": str(self.tmp / "demo deck.md"), "supervisor_sessions": []})
        append_event(self.paths, "run_started", run_id=RUN_ID, base_sha="0" * 40, branch="lh/demo")
        self.config = self.tmp / "user config.toml"
        self.config.write_text('[roles.supervisor]\nclaude = "sonnet low"\n', encoding="utf-8")
        self.log = self.tmp / "fake claude.log"
        env = {"LEADHAND_CONFIG": str(self.config), "FAKE_CLAUDE_LOG": str(self.log),
               "FAKE_CLAUDE_SLEEP": "0", "FAKE_CLAUDE_EXIT": "0"}
        patcher = mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.procs: list[subprocess.Popen] = []
        self.addCleanup(self._reap)

    def _reap(self):
        for p in self.procs:
            if p.poll() is None:
                p.kill()
            self.close_pipes(p)
        # A fake supervisor orphaned by a killed launcher: stop it too.
        for rec in self.records():
            pid = rec.get("pid")
            if pid and "argv" in rec and procs.process_start_time(pid):
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGKILL)

    # -- helpers ----------------------------------------------------------

    def progress_run(self):
        """Give the run an event beyond run_started, as a run a supervisor already worked."""
        append_event(self.paths, "card_cut", card=1, branch="lh/demo-card-1", base="0" * 40)

    def records(self) -> list[dict]:
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines() if line]

    def starts(self) -> list[dict]:
        return [r for r in self.records() if "argv" in r]

    def run_inprocess(self, target=None, **kw) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = launch.run(target or self.run_dir, claude_bin=str(FAKE_CLAUDE), **kw)
        return code, out.getvalue(), err.getvalue()

    def spawn_launcher(self, target=None, *, cwd=None, sleep="30", **env) -> subprocess.Popen:
        code = ("import sys; from pathlib import Path; from leadhand import launch; "
                "sys.exit(launch.run(Path(sys.argv[1]), claude_bin=sys.argv[2]))")
        full_env = {**os.environ, "PYTHONPATH": str(LIB), "FAKE_CLAUDE_SLEEP": sleep, **env}
        p = subprocess.Popen([sys.executable, "-B", "-c", code, str(target or self.run_dir),
                              str(FAKE_CLAUDE)],
                             cwd=str(cwd or self.elsewhere), env=full_env,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.procs.append(p)
        return p

    @staticmethod
    def close_pipes(p: subprocess.Popen):
        p.wait(timeout=WAIT)
        for f in (p.stdout, p.stderr):
            if f is not None:
                f.close()

    def kill_launcher(self, p: subprocess.Popen):
        """SIGKILL a launcher. Its pipes are closed rather than drained: the
        orphaned fake supervisor still holds their write ends."""
        os.kill(p.pid, signal.SIGKILL)
        self.close_pipes(p)

    def wait_supervisor(self, n: int = 1) -> dict:
        start = wait_for(lambda: len(self.starts()) >= n and self.starts()[n - 1],
                         "the fake supervisor to start")
        wait_for(lambda: self.holder().get("supervisor_pid") == start["pid"],
                 "holder.json to name the supervisor")
        return start

    def holder(self) -> dict:
        try:
            return json.loads(self.paths.holder.read_text(encoding="utf-8"))
        except (FileNotFoundError, ValueError):
            return {}

    def wait_gone(self, pid: int):
        wait_for(lambda: procs.process_start_time(pid) is None, f"pid {pid} to exit")


# --------------------------------------------------------------------------
# argv, settings, first message

class ArgvTests(LaunchTestCase):
    def test_launch_argv_order_and_auto_mode(self):
        argv = launch.launch_argv(claude_bin="/bin/claude", agent="leadhand:supervisor",
                                  first_message="go now", plugin_dir=Path("/p d/plugin"),
                                  settings='{"x": 1}', model="opus", effort="medium")
        self.assertEqual(argv, ["/bin/claude", "--plugin-dir", "/p d/plugin", "--settings", '{"x": 1}',
                                "--agent", "leadhand:supervisor", "--model", "opus",
                                "--effort", "medium", "--permission-mode", "auto", "go now"])
        bare = launch.launch_argv(claude_bin="claude", agent="leadhand:supervisor",
                                  first_message="go", plugin_dir=None, settings=None)
        self.assertEqual(bare, ["claude", "--agent", "leadhand:supervisor",
                                "--permission-mode", "auto", "go"])
        self.assertEqual(launch.SUPERVISOR_AGENT, "leadhand:supervisor")

    def test_run_passes_supervisor_model_effort_and_cli_settings(self):
        code, _, _ = self.run_inprocess(plugin_dir=self.tmp / "plugin dir")
        self.assertEqual(code, 0)
        argv = self.starts()[0]["argv"]
        self.assertEqual(argv[0], str(FAKE_CLAUDE))
        settings = json.loads(argv[argv.index("--settings") + 1])
        self.assertEqual(settings, {"sandbox": {"excludedCommands": [f"{CLI} *"]}})
        self.assertEqual(argv[1:], [
            "--plugin-dir", str(self.tmp / "plugin dir"),
            "--settings", argv[argv.index("--settings") + 1],
            "--agent", "leadhand:supervisor", "--model", "sonnet", "--effort", "low",
            "--permission-mode", "auto", argv[-1]])

    def test_print_mode_adds_headless_stream_flags_before_the_prompt(self):
        argv = launch.launch_argv(claude_bin="claude", agent="leadhand:supervisor",
                                  first_message="go", plugin_dir=None, settings=None, print_mode=True)
        self.assertEqual(argv, ["claude", "--agent", "leadhand:supervisor", "--permission-mode", "auto",
                                "-p", "--output-format", "stream-json", "--verbose", "go"])
        code, _, _ = self.run_inprocess(print_mode=True)
        self.assertEqual(code, 0)
        argv = self.starts()[0]["argv"]
        self.assertEqual(argv[-6:-1], ["auto", "-p", "--output-format", "stream-json", "--verbose"])

    def test_cli_print_option_is_hidden_and_off_by_default(self):
        from leadhand import cli
        parser = cli.build_parser()
        self.assertFalse(parser.parse_args(["run", "deck.md"]).print_mode)
        self.assertTrue(parser.parse_args(["run", "deck.md", "--print"]).print_mode)
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit):
            parser.parse_args(["run", "--help"])
        self.assertNotIn("--print", out.getvalue())

    def test_first_message_names_skill_invocation(self):
        code, _, _ = self.run_inprocess()
        self.assertEqual(code, 0)
        self.assertEqual(self.starts()[0]["argv"][-1],
                         f"Run the leadhand deck in {self.run_dir}. leadhand CLI: {CLI}. "
                         "First invoke the Skill tool with leadhand:run, then follow it.")

    def test_attended_use_warning_printed(self):
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 0)
        self.assertIn("attended", err)
        self.assertIn("excludedCommands", err)

    def test_keep_awake_prefix_per_platform(self):
        with mock.patch("shutil.which", return_value="/usr/bin/systemd-inhibit"):
            self.assertEqual(launch.keep_awake_prefix("linux"),
                             ["systemd-inhibit", "--what=idle", "--why=leadhand run"])
            self.assertEqual(launch.keep_awake_prefix("darwin"), [])
        with mock.patch("shutil.which", return_value=None):
            self.assertEqual(launch.keep_awake_prefix("linux"), [])
        self.assertEqual(launch.keep_awake_prefix("freebsd14"), [])


# --------------------------------------------------------------------------
# run dir, cwd, environment

class ChildTests(LaunchTestCase):
    def test_supervisor_cwd_is_workspace_when_launched_elsewhere(self):
        p = self.spawn_launcher(self.paths.deck, cwd=self.elsewhere, sleep="0")
        out, err = p.communicate(timeout=WAIT)
        self.assertEqual(p.returncode, 0, err)
        start = self.starts()[0]
        self.assertEqual(os.path.realpath(start["cwd"]), os.path.realpath(self.workspace))
        self.assertIn(f"Run the leadhand deck in {self.run_dir}.", start["argv"][-1])

    def test_env_has_run_id_dir_and_cli(self):
        code, _, _ = self.run_inprocess()
        self.assertEqual(code, 0)
        self.assertEqual(self.starts()[0]["env"], {
            "LEADHAND_RUN_ID": RUN_ID, "LEADHAND_RUN_DIR": str(self.run_dir),
            "LEADHAND_CLI": str(CLI)})

    def test_run_accepts_deck_md_path(self):
        code, _, _ = self.run_inprocess(self.paths.deck)
        self.assertEqual(code, 0)
        self.assertEqual(self.starts()[0]["env"]["LEADHAND_RUN_DIR"], str(self.run_dir))

    def test_not_a_run_is_an_error_and_starts_nothing(self):
        code, _, err = self.run_inprocess(self.elsewhere)
        self.assertEqual(code, 1)
        self.assertIn("not a run", err)
        self.assertEqual(self.starts(), [])

    def test_child_exit_code_returned(self):
        with mock.patch.dict(os.environ, {"FAKE_CLAUDE_EXIT": "7"}):
            code, _, _ = self.run_inprocess()
        self.assertEqual(code, 7)

    def test_launch_line_quotes_paths_with_spaces(self):
        plugin = self.tmp / "plugin dir"
        line = launch.launch_line(self.run_dir, plugin_dir=plugin)
        self.assertEqual(shlex.split(line), [str(CLI), "run", str(self.paths.deck),
                                             "--plugin-dir", str(plugin)])
        self.assertEqual(line, shlex.join(shlex.split(line)))
        r = subprocess.run([sys.executable, str(CLI), "launch-line", str(self.paths.deck)],
                           capture_output=True, text=True, cwd=self.elsewhere, timeout=WAIT)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(shlex.split(r.stdout), [str(CLI), "run", str(self.paths.deck)])


# --------------------------------------------------------------------------
# ownership

class OwnershipTests(LaunchTestCase):
    def test_refused_launch_changes_nothing(self):
        self.progress_run()
        first = self.spawn_launcher()
        sup = self.wait_supervisor()
        resumed = [e for e in read_events(self.paths) if e["kind"] == "resumed"]
        self.assertEqual(len(resumed), 1)
        before = snapshot(self.run_dir)
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 3)
        self.assertIn(f"Run already in progress: supervisor pid {sup['pid']} since ", err)
        self.assertIn(f"(run {RUN_ID})", err)
        self.assertEqual(snapshot(self.run_dir), before)
        self.assertEqual(len(self.starts()), 1)
        os.kill(first.pid, signal.SIGTERM)
        first.communicate(timeout=WAIT)

    def test_cli_run_refusal_exits_3(self):
        first = self.spawn_launcher()
        sup = self.wait_supervisor()
        r = subprocess.run([sys.executable, str(CLI), "run", str(self.paths.deck)],
                           capture_output=True, text=True, cwd=self.elsewhere, timeout=WAIT)
        self.assertEqual(r.returncode, 3, r.stderr)
        self.assertIn(f"Run already in progress: supervisor pid {sup['pid']}", r.stderr)
        os.kill(first.pid, signal.SIGTERM)
        first.communicate(timeout=WAIT)

    def test_lock_held_by_supervisor_after_launcher_killed(self):
        launcher = self.spawn_launcher(sleep="3")
        sup = self.wait_supervisor()
        self.kill_launcher(launcher)
        self.assertIsNotNone(procs.process_start_time(sup["pid"]), "fake supervisor died early")
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 3, err)
        self.assertIn(f"supervisor pid {sup['pid']}", err)
        self.wait_gone(sup["pid"])
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        self.assertEqual(len(self.starts()), 2)

    def test_lock_fd_alone_blocks_takeover(self):
        # holder.json names a dead supervisor, but run.lock is still flocked by a live process.
        launcher = self.spawn_launcher(sleep="3")
        sup = self.wait_supervisor()
        self.kill_launcher(launcher)
        write_json_atomic(self.paths.holder, {**self.holder(), "supervisor_start": "not it"})
        code, _, _ = self.run_inprocess()
        self.assertEqual(code, 3)
        self.wait_gone(sup["pid"])

    def test_recorded_live_supervisor_blocks_takeover(self):
        self.paths.lock.write_text("", encoding="utf-8")       # stale: nobody holds the flock
        live = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                stdin=subprocess.DEVNULL)
        self.procs.append(live)
        start = wait_for(lambda: launch.process_start_time(live.pid), "the live process")
        self.assertEqual(start, procs.process_start_time(live.pid))
        write_json_atomic(self.paths.holder, {
            "run_id": RUN_ID, "launcher_pid": 1, "supervisor_pid": live.pid,
            "supervisor_start": start, "started_at": "2026-11-10T00:00:00.000Z"})
        self.progress_run()
        before = snapshot(self.run_dir)
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 3)
        self.assertIn(f"Run already in progress: supervisor pid {live.pid} since "
                      f"2026-11-10T00:00:00.000Z (run {RUN_ID})", err)
        self.assertEqual(snapshot(self.run_dir), before)
        self.assertEqual(self.starts(), [])

        # The same pid with another start time is a reused pid, not the supervisor.
        write_json_atomic(self.paths.holder, {**self.holder(), "supervisor_start": "other"})
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        self.assertEqual(len(self.starts()), 1)

    def test_resumed_appended_only_after_acquire(self):
        self.progress_run()
        seen = []
        real = launch.RunLock.acquire

        def acquire(lock):
            seen.append([e["kind"] for e in read_events(self.paths)])
            return real(lock)

        with mock.patch.object(launch.RunLock, "acquire", acquire):
            code, _, _ = self.run_inprocess()
        self.assertEqual(code, 0)
        self.assertEqual(len(seen), 1)
        self.assertNotIn("resumed", seen[0])
        resumed = [e for e in read_events(self.paths) if e["kind"] == "resumed"]
        self.assertEqual(len(resumed), 1)
        self.assertEqual(resumed[0]["run_id"], RUN_ID)

    def test_first_launch_of_a_fresh_run_appends_no_resumed(self):
        code, _, _ = self.run_inprocess()
        self.assertEqual(code, 0)
        self.assertEqual([e["kind"] for e in read_events(self.paths)], ["run_started"])

    def test_holder_records_launcher_and_supervisor(self):
        launcher = self.spawn_launcher()
        sup = self.wait_supervisor()
        h = self.holder()
        self.assertEqual(h["run_id"], RUN_ID)
        self.assertEqual(h["launcher_pid"], launcher.pid)
        self.assertEqual(h["supervisor_start"], procs.process_start_time(sup["pid"]))
        self.assertTrue(h["started_at"])
        os.kill(launcher.pid, signal.SIGTERM)
        launcher.communicate(timeout=WAIT)

    def test_signals_forwarded_to_child(self):
        for sig in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=sig.name):
                n = len(self.starts()) + 1
                launcher = self.spawn_launcher()
                sup = self.wait_supervisor(n)
                os.kill(launcher.pid, sig)
                _, err = launcher.communicate(timeout=WAIT)
                self.assertEqual(launcher.returncode, 128 + sig, err)
                got = [r for r in self.records() if r.get("pid") == sup["pid"] and "signal" in r]
                self.assertEqual([r["signal"] for r in got], [int(sig)])
        # The lock was released: a new launch owns the run.
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)


# --------------------------------------------------------------------------
# Review follow-ups: R27 keep-awake beside the child, R28 CLI path with spaces,
# guarded forwarding, recorded plugin_dir, busy lock with a dead recorded supervisor

FAKE_INHIBIT = """#!/usr/bin/env python3
import json, os, signal, sys, time
def bye(*_):
    sys.exit(0)
signal.signal(signal.SIGTERM, bye)
with open(os.environ["FAKE_INHIBIT_LOG"], "a", encoding="utf-8") as f:
    f.write(json.dumps({"pid": os.getpid(), "ppid": os.getppid(), "argv": sys.argv}) + "\\n")
time.sleep(60)
"""


class FollowUpTests(LaunchTestCase):
    def test_linux_inhibitor_runs_beside_claude_which_stays_the_direct_child(self):
        bindir = self.tmp / "fake bin"
        bindir.mkdir()
        inhibit = bindir / "systemd-inhibit"
        inhibit.write_text(FAKE_INHIBIT, encoding="utf-8")
        inhibit.chmod(0o755)
        inhibit_log = self.tmp / "inhibit.log"
        env = {"PATH": f"{bindir}{os.pathsep}{os.environ['PATH']}",
               "FAKE_INHIBIT_LOG": str(inhibit_log), "FAKE_CLAUDE_SLEEP": "1"}
        with mock.patch.dict(os.environ, env), mock.patch.object(launch, "PLATFORM", "linux"):
            code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        start = self.starts()[0]
        self.assertEqual(start["argv"][0], str(FAKE_CLAUDE))
        self.assertEqual(start["ppid"], os.getpid())
        self.assertEqual(self.holder()["supervisor_pid"], start["pid"])
        rec = json.loads(inhibit_log.read_text(encoding="utf-8").splitlines()[0])
        self.assertEqual(rec["ppid"], os.getpid())
        self.assertEqual(rec["argv"][1:], ["--what=idle", "--why=leadhand run",
                                           "tail", f"--pid={start['pid']}", "-f", "/dev/null"])
        self.assertIsNone(procs.process_start_time(rec["pid"]), "inhibitor left running")

    def test_cli_path_with_spaces_warns_sandbox_exclusion_may_not_match(self):
        with mock.patch.object(launch, "CLI_PATH", Path("/opt/lead hand/scripts/leadhand")):
            code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        self.assertIn("the sandbox exclusion may not match; install leadhand under a path "
                      "without spaces", err)
        argv = self.starts()[0]["argv"]
        self.assertEqual(json.loads(argv[argv.index("--settings") + 1]),
                         {"sandbox": {"excludedCommands": ["/opt/lead hand/scripts/leadhand *"]}})
        with mock.patch.object(launch, "CLI_PATH", Path("/opt/leadhand/scripts/leadhand")):
            code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        self.assertNotIn("may not match", err)

    def test_signal_after_child_exit_is_not_forwarded(self):
        real_wait = subprocess.Popen.wait
        kills = []

        def wait(popen, *a, **kw):
            code = real_wait(popen, *a, **kw)
            if popen.args and popen.args[0] == str(FAKE_CLAUDE):
                handler = signal.getsignal(signal.SIGTERM)
                with mock.patch("os.kill", side_effect=lambda pid, sig: kills.append((pid, sig))):
                    handler(signal.SIGTERM, None)
            return code

        with mock.patch.object(subprocess.Popen, "wait", wait):
            code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        self.assertEqual(kills, [])

    def test_launch_line_defaults_to_recorded_plugin_dir(self):
        plugin = self.tmp / "dev plugin"
        code, _, err = self.run_inprocess(plugin_dir=plugin)
        self.assertEqual(code, 0, err)
        self.assertEqual(self.holder()["plugin_dir"], str(plugin))
        self.assertEqual(shlex.split(launch.launch_line(self.run_dir)),
                         [str(CLI), "run", str(self.paths.deck), "--plugin-dir", str(plugin)])
        other = self.tmp / "other plugin"
        self.assertEqual(shlex.split(launch.launch_line(self.paths.deck, plugin_dir=other))[-1],
                         str(other))
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        self.assertNotIn("plugin_dir", self.holder())

    def test_busy_lock_with_dead_recorded_supervisor_says_not_running(self):
        gone = subprocess.Popen([sys.executable, "-c", "pass"], stdin=subprocess.DEVNULL)
        gone.wait(timeout=WAIT)
        write_json_atomic(self.paths.holder, {
            "run_id": RUN_ID, "launcher_pid": gone.pid, "supervisor_pid": gone.pid,
            "supervisor_start": "long gone", "started_at": "2026-11-10T00:00:00.000Z"})
        fd = os.open(self.paths.lock, os.O_RDWR | os.O_CREAT, 0o644)
        self.addCleanup(os.close, fd)
        fcntl.flock(fd, fcntl.LOCK_EX)
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 3)
        self.assertIn(f"lock held by another process (supervisor pid {gone.pid} recorded, "
                      "not running)", err)
        self.assertNotIn("since", err)
        self.assertEqual(self.starts(), [])


# --------------------------------------------------------------------------
# R41(a): CLI children of the supervisor drop the inherited run.lock descriptor

CLI_CHILD = """import json, os, sys, time
before = os.environ.get("LEADHAND_LOCK_FD")
from leadhand import cli
cli.main(["version"])
with open(sys.argv[1], "a", encoding="utf-8") as f:
    f.write(json.dumps({"pid": os.getpid(), "cli_child": True, "lock_fd_env": before,
                        "after": os.environ.get("LEADHAND_LOCK_FD")}) + "\\n")
time.sleep(60)
"""


class InheritedLockTests(LaunchTestCase):
    def test_relaunch_succeeds_while_a_cli_child_of_the_killed_supervisor_runs(self):
        child_log = self.tmp / "cli child.log"
        spawn = json.dumps([sys.executable, "-B", "-c", CLI_CHILD, str(child_log)])
        launcher = self.spawn_launcher(sleep="60", FAKE_CLAUDE_SPAWN=spawn)
        sup = self.wait_supervisor()
        rec = wait_for(lambda: child_log.exists() and child_log.read_text(encoding="utf-8")
                       and json.loads(child_log.read_text(encoding="utf-8").splitlines()[0]),
                       "the CLI child to run")
        self.addCleanup(lambda: procs.process_start_time(rec["pid"])
                        and os.kill(rec["pid"], signal.SIGKILL))
        self.assertTrue(rec["lock_fd_env"], "launcher did not export LEADHAND_LOCK_FD")
        self.assertIsNone(rec["after"])
        self.kill_launcher(launcher)
        os.kill(sup["pid"], signal.SIGKILL)
        self.wait_gone(sup["pid"])
        self.assertIsNotNone(procs.process_start_time(rec["pid"]), "the CLI child exited early")
        code, _, err = self.run_inprocess()
        self.assertEqual(code, 0, err)
        self.assertIsNotNone(procs.process_start_time(rec["pid"]))

    def _main_with(self, fd: int, run_dir: Path | None):
        env = {"LEADHAND_LOCK_FD": str(fd)}
        if run_dir is not None:
            env["LEADHAND_RUN_DIR"] = str(run_dir)
        with mock.patch.dict(os.environ, env), contextlib.redirect_stdout(io.StringIO()):
            from leadhand import cli
            self.assertEqual(cli.main(["version"]), 0)
            return "LEADHAND_LOCK_FD" in os.environ

    def test_cli_closes_inherited_run_lock_fd(self):
        fd = os.open(self.paths.lock, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            still_set = self._main_with(fd, self.run_dir)
            self.assertFalse(still_set)
            with self.assertRaises(OSError):
                os.fstat(fd)
        except BaseException:
            with contextlib.suppress(OSError):
                os.close(fd)
            raise

    def test_cli_leaves_an_unrelated_fd_open(self):
        other = self.tmp / "unrelated.txt"
        other.write_text("x", encoding="utf-8")
        fd = os.open(other, os.O_RDONLY)
        self.addCleanup(os.close, fd)
        self.assertFalse(self._main_with(fd, self.run_dir))
        os.fstat(fd)                                   # still open
        # A run.lock fd, but no LEADHAND_RUN_DIR to verify it against: left open too.
        lock_fd = os.open(self.paths.lock, os.O_RDWR | os.O_CREAT, 0o644)
        self.addCleanup(os.close, lock_fd)
        self._main_with(lock_fd, None)
        os.fstat(lock_fd)
        # Not a number, or not open: ignored.
        self._main_with(-1, self.run_dir)
        with mock.patch.dict(os.environ, {"LEADHAND_LOCK_FD": "nope"}), \
                contextlib.redirect_stdout(io.StringIO()):
            from leadhand import cli
            self.assertEqual(cli.main(["version"]), 0)


if __name__ == "__main__":
    unittest.main()
