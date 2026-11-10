"""The launcher: run ownership, the supervisor's argv, environment and cwd, signals.

Every launch uses tests/fakes/claude by absolute path, never the real `claude`.
Launches that must outlive a test step (or be SIGKILLed) run in their own Python
process through `launch.run`; the rest run in-process.
"""
from __future__ import annotations

import contextlib
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


if __name__ == "__main__":
    unittest.main()
