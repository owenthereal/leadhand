#!/usr/bin/env python3
"""Launcher scenarios: `run`, which owns a run and starts its Claude Code supervisor, and
`launch-line`, which prints the command that does.

Converted from tests/test_launch.py, with the self-path hook of tests/test_test_hooks.py
(SelfPathHook). Every scenario runs on a run that `start` made (one card, a worktree workspace,
the supervisor route `sonnet low` in the run's config.json), with tests/fakes/claude as the
`claude` found on PATH. It reads what the launch leaves: the fake's log (its argv, working
directory, LEADHAND_* environment, pid and parent, and the signals it got), holder.json,
events.jsonl, run.lock (by taking the flock itself), exit codes and stderr.

Spec contract item 11 (the `claude` argument order, environment, working directory, first
message and settings overlay) and item 12 (every lock is flock(2) on the same files): a test
that holds run.lock holds it with flock, as another leadhand process would.

Where the binary passes or prints its own path (LEADHAND_CLI, the first message, the sandbox
exclusion, the launch line) the path is compared by resolved file (ruling S1-R11); the rest of
the text exactly. `launch-line` prints bare `leadhand` in Rust when the `leadhand` on PATH is the
binary itself, which Python never does, so the world's PATH holds no such `leadhand`.

Whether a supervisor or another process is alive is judged from the fake's own log (pid and the
time of the line) against the process table (harness.started_by, same_process), never from
holder.json alone. Launchers that must outlive a step run in the background in their own process
group; the cleanup kills them, then every process a fake's log names that is still the process
it named. The keep-awake tools on PATH are fakes, so no real `caffeinate` or `systemd-inhibit`
runs. CLAUDECODE is left as inherited: the CLI does not read it.
"""
import contextlib
import errno
import fcntl
import json
import os
import shlex
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

import harness
from gitfixture import commit, run_git
from harness import BIN, FAKES, Result, read_json, read_jsonl, run_cli, same_process, started_by
from rundir import read_records, run_paths
from contract_dispatch import World as DispatchWorld
from supervisor import FakeSupervisor, card

WAIT = 15.0
SUPERVISOR_ROUTE = '[roles.supervisor]\nclaude = "sonnet low"\n'
AGENT = "leadhand:supervisor"
FIRST_MESSAGE_END = ". First invoke the Skill tool with leadhand:run, then follow it."
SPACES_WARNING_TAIL = "the sandbox exclusion may not match; install leadhand under a path without spaces"

# A keep-awake tool that does nothing: the default on the world's PATH.
NOOP_TOOL = "#!/bin/sh\nexit 0\n"
# A keep-awake tool that logs how it was started and runs until SIGTERM.
LOGGING_TOOL = """#!/usr/bin/env python3
import json, os, signal, sys, time
def bye(*_):
    sys.exit(0)
signal.signal(signal.SIGTERM, bye)
with open(os.environ["FAKE_KEEP_AWAKE_LOG"], "a", encoding="utf-8") as f:
    f.write(json.dumps({"time": time.time(), "pid": os.getpid(), "ppid": os.getppid(),
                        "argv": sys.argv}) + "\\n")
time.sleep(60)
"""


def wait_for(cond, what: str, timeout: float = WAIT, proc: subprocess.Popen | None = None):
    """Poll `cond` until it returns something true, and return that. Fails after `timeout`
    seconds, or at once when `proc` (a command the test is waiting on) has exited."""
    deadline = time.monotonic() + timeout
    while True:
        value = cond()
        if value:
            return value
        if proc is not None and proc.poll() is not None:
            raise AssertionError(f"the command exited ({proc.returncode}) while waiting for {what}\n"
                                 f"argv: {proc.args}")
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out after {timeout:g} s waiting for {what}")
        time.sleep(0.02)


def snapshot(root: Path) -> dict:
    """Every path under `root` with its mtime, size and (for a file) content."""
    out = {}
    for path in sorted(root.rglob("*")):
        st = path.lstat()
        out[str(path.relative_to(root))] = (st.st_mtime_ns, st.st_size,
                                            path.read_bytes() if path.is_file() else None)
    return out


def lockable(path: Path) -> bool:
    """Whether an exclusive flock on `path` can be taken now (it is taken and dropped again)."""
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return False
    finally:
        os.close(fd)
    return True


def path_without_self(path: str) -> str:
    """PATH without the directories whose `leadhand` is the binary under test."""
    def is_self(d: str) -> bool:
        exe = os.path.join(d, "leadhand")
        return bool(d) and os.path.isfile(exe) and harness.names_bin(exe)
    return os.pathsep.join(d for d in path.split(os.pathsep) if not is_self(d))


class World:
    """A run `start` made, a PATH with the fake `claude` and no-op keep-awake tools, and the
    launches on it. `env` is every command's environment."""

    def __init__(self, test: unittest.TestCase, *, config: str = SUPERVISOR_ROUTE):
        self.test = test
        self.sup = FakeSupervisor(test, [card(1)], config=config)
        self.run_dir = self.sup.start()
        self.paths = run_paths(self.run_dir)
        meta = self.sup.meta()
        self.run_id = meta["run_id"]
        self.workspace = Path(meta["workspace"])
        self.repo = self.sup.repo
        self.tmp = self.sup.tmp
        self.elsewhere = self.tmp / "some other dir"
        self.elsewhere.mkdir()
        self.bindir = self.tmp / "fake bin"
        self.bindir.mkdir()
        (self.bindir / "claude").symlink_to(FAKES / "claude")
        for name in ("caffeinate", "systemd-inhibit"):
            self.tool(name, NOOP_TOOL)
        self.log = self.tmp / "fake claude.log"
        self.keep_awake_log = self.tmp / "keep awake.log"
        self.env = dict(self.sup.env, PATH=f"{self.bindir}{os.pathsep}{path_without_self(self.sup.env['PATH'])}",
                        FAKE_CLAUDE_LOG=str(self.log), FAKE_CLAUDE_SLEEP="0", FAKE_CLAUDE_EXIT="0",
                        FAKE_KEEP_AWAKE_LOG=str(self.keep_awake_log))
        self.background: list[subprocess.Popen] = []
        self._outputs = 0
        test.addCleanup(self.cleanup)

    def tool(self, name: str, body: str) -> None:
        path = self.bindir / name
        path.write_text(body, encoding="utf-8")
        path.chmod(0o755)

    # -- driving ----------------------------------------------------------------

    def progress(self) -> str:
        """Give the run events beyond run_started, as a run a supervisor already worked: `next`
        cuts card 1 and asks for its dispatch. Returns the dispatch ID."""
        a = self.sup.next()
        self.test.assertEqual(a["action"], "dispatch", a)
        return a["dispatch_id"]

    def run(self, *args, target=None, cwd=None, **env) -> Result:
        """`run TARGET ARGS` in the foreground, from elsewhere; `env` adds to the world's."""
        return run_cli("run", target or self.run_dir, *args, env=dict(self.env, **env),
                       cwd=cwd or self.elsewhere, timeout=60)

    def launch_line(self, *args, cwd=None) -> Result:
        return run_cli("launch-line", *args, env=self.env, cwd=cwd or self.elsewhere)

    def popen(self, argv: list, *, env: dict, pass_fds=(), stdin=subprocess.DEVNULL) -> subprocess.Popen:
        """A background command in its own process group, its output in files (an orphaned
        child that inherits them never blocks a pipe); the cleanup kills it."""
        self._outputs += 1
        out = self.tmp / f"background {self._outputs} out"
        err = self.tmp / f"background {self._outputs} err"
        with open(out, "wb") as o, open(err, "wb") as e:
            p = subprocess.Popen([str(a) for a in argv], cwd=self.elsewhere, env=env, stdin=stdin,
                                 stdout=o, stderr=e, pass_fds=pass_fds, process_group=0)
        p.outputs = (out, err)
        self.background.append(p)
        return p

    def launcher(self, target=None, *, sleep: str = "30", **env) -> subprocess.Popen:
        """`run TARGET` in the background with a fake supervisor that sleeps `sleep` seconds."""
        return self.popen([BIN, "run", target or self.run_dir],
                          env=dict(self.env, FAKE_CLAUDE_SLEEP=sleep, **env))

    def finish(self, p: subprocess.Popen, timeout: float = WAIT) -> Result:
        """Wait for a background command and return what it did."""
        p.wait(timeout=timeout)
        out, err = (f.read_text(encoding="utf-8", errors="replace") for f in p.outputs)
        return Result([str(a) for a in p.args], p.returncode, out, err)

    @staticmethod
    def stop(p: subprocess.Popen, sig: int = signal.SIGTERM) -> None:
        """Signal a background command of the test's own while it is still running."""
        if p.poll() is None:
            os.kill(p.pid, sig)

    def hold(self, path: Path):
        """Take an exclusive flock on `path`, as another leadhand process would. Returns
        `release`, which closes the descriptor (dropping the lock) once; the cleanup calls it."""
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
        held = [fd]

        def release() -> None:
            if held:
                os.close(held.pop())
        self.test.addCleanup(release)
        fcntl.flock(fd, fcntl.LOCK_EX)
        return release

    def write_holder(self, holder: dict) -> None:
        self.paths.holder.write_text(json.dumps(holder, indent=2) + "\n", encoding="utf-8")

    # -- reading ----------------------------------------------------------------

    def records(self) -> list[dict]:
        return read_jsonl(self.log) if self.log.exists() else []

    def starts(self) -> list[dict]:
        """The fake supervisors that started, each with `start`, its process start time as the
        process table had it when its log line was written (None once it is gone)."""
        return [dict(r, start=started_by(r["pid"], r["time"])) for r in self.records() if "argv" in r]

    def signals(self, pid: int) -> list[int]:
        return [r["signal"] for r in self.records() if r.get("pid") == pid and "signal" in r]

    def holder(self) -> dict:
        try:
            return read_json(self.paths.holder)
        except (FileNotFoundError, ValueError):
            return {}

    def wait_supervisor(self, n: int = 1, proc: subprocess.Popen | None = None) -> dict:
        """The n-th fake supervisor, once it has started (its log line), is alive by the process
        table, and holder.json names it."""
        def started():
            starts = self.starts()
            return starts[n - 1] if len(starts) >= n and starts[n - 1]["start"] else None
        sup = wait_for(started, "the fake supervisor to start", proc=proc)
        wait_for(lambda: self.holder().get("supervisor_pid") == sup["pid"], "holder.json to name the supervisor",
                 proc=proc)
        return sup

    @staticmethod
    def alive(rec: dict) -> bool:
        return same_process(rec["pid"], rec["start"])

    def wait_gone(self, rec: dict) -> None:
        wait_for(lambda: not self.alive(rec), f"pid {rec['pid']} to exit")

    def events(self, kind=None) -> list[dict]:
        return [e for e in read_records(self.paths.events) if kind is None or e["kind"] == kind]

    def argv(self, n: int = 1) -> list[str]:
        return self.starts()[n - 1]["argv"]

    # -- cleanup ----------------------------------------------------------------

    def cleanup(self) -> None:
        """Kill the background commands, then every process the fakes' logs name that is still
        the process named (it started no later than its log line); wait for them to go."""
        for p in self.background:
            if p.poll() is None:
                p.kill()
            with contextlib.suppress(Exception):
                p.wait(timeout=10)
            if p.stdin is not None:
                with contextlib.suppress(OSError):
                    p.stdin.close()
        killed = []
        for log in (self.log, self.keep_awake_log):
            for rec in (read_jsonl(log) if log.exists() else []):
                pid = rec.get("spawned") or (rec.get("pid") if "argv" in rec else None)
                start = started_by(pid, rec["time"]) if pid else None
                if start is not None:
                    with contextlib.suppress(ProcessLookupError, PermissionError):
                        os.kill(pid, signal.SIGKILL)
                        killed.append((pid, start))
        deadline = time.monotonic() + 5
        while any(same_process(pid, start) for pid, start in killed) and time.monotonic() < deadline:
            time.sleep(0.02)


class LaunchCase(unittest.TestCase):
    def setUp(self):
        self.w = World(self)

    def first_message_ok(self, text: str, run_dir: Path) -> bool:
        return harness.names_bin_between(text, f"Run the leadhand deck in {run_dir}. leadhand CLI: ",
                                         FIRST_MESSAGE_END)

    def settings_ok(self, settings_json: str) -> None:
        settings = json.loads(settings_json)
        self.assertEqual(list(settings), ["sandbox"])
        self.assertEqual(list(settings["sandbox"]), ["excludedCommands"])
        entries = settings["sandbox"]["excludedCommands"]
        self.assertEqual(len(entries), 1, entries)
        self.assertTrue(harness.names_bin_between(entries[0], "", " *"), entries)

    def claude_ok(self, argv0: str) -> None:
        """argv[0] is the `claude` found on PATH: the fake."""
        self.assertEqual(os.path.realpath(argv0), os.path.realpath(FAKES / "claude"))


# --------------------------------------------------------------------------
# argv, settings, first message

class Argv(LaunchCase):
    def test_argv_order_and_auto_mode(self):
        """Without --plugin-dir: the settings overlay, the supervisor agent, the route's model
        and effort, auto mode, then the first message.
        Covers P1."""
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        argv = self.w.argv()
        self.claude_ok(argv[0])
        self.assertEqual(argv[1:], ["--settings", argv[2], "--agent", AGENT, "--model", "sonnet",
                                    "--effort", "low", "--permission-mode", "auto", argv[-1]])
        self.settings_ok(argv[2])
        self.assertTrue(self.first_message_ok(argv[-1], self.w.run_dir), argv[-1])

    def test_no_supervisor_route_means_no_model_or_effort(self):
        """A run whose configuration snapshot (config.json) has no supervisor role: the bare
        argv, with neither --model nor --effort."""
        config = self.w.run_dir / "config.json"
        data = read_json(config)
        del data["roles"]["supervisor"]
        config.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        argv = self.w.argv()
        self.claude_ok(argv[0])
        self.assertEqual(argv[1:], ["--settings", argv[2], "--agent", AGENT, "--permission-mode", "auto", argv[-1]])
        self.settings_ok(argv[2])
        self.assertTrue(self.first_message_ok(argv[-1], self.w.run_dir), argv[-1])

    def test_run_passes_supervisor_model_effort_and_cli_settings(self):
        """Covers P1 and P5 (the settings overlay's CLI exclusion)."""
        plugin = self.w.tmp / "plugin dir"
        r = self.w.run("--plugin-dir", plugin)
        self.assertEqual(r.code, 0, r)
        argv = self.w.argv()
        self.claude_ok(argv[0])
        settings = argv[argv.index("--settings") + 1]
        self.settings_ok(settings)
        self.assertEqual(argv[1:], [
            "--plugin-dir", str(plugin), "--settings", settings,
            "--agent", AGENT, "--model", "sonnet", "--effort", "low",
            "--permission-mode", "auto", argv[-1]])

    def test_print_mode_adds_headless_stream_flags_before_the_prompt(self):
        r = self.w.run("--print")
        self.assertEqual(r.code, 0, r)
        argv = self.w.argv()
        self.assertEqual(argv[1:], ["--settings", argv[2], "--agent", AGENT, "--model", "sonnet",
                                    "--effort", "low", "--permission-mode", "auto",
                                    "-p", "--output-format", "stream-json", "--verbose", argv[-1]])
        self.assertTrue(self.first_message_ok(argv[-1], self.w.run_dir), argv[-1])

    def test_print_option_is_hidden_and_off_by_default(self):
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertNotIn("-p", self.w.argv())
        self.assertNotIn("--output-format", self.w.argv())
        h = run_cli("run", "--help", env=self.w.env, cwd=self.w.elsewhere).ok()
        self.assertIn("--plugin-dir", h.stdout)
        self.assertNotIn("--print", h.stdout)

    def test_first_message_names_skill_invocation(self):
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertTrue(self.first_message_ok(self.w.argv()[-1], self.w.run_dir), self.w.argv()[-1])

    def test_attended_use_warning_printed(self):
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertIn("attended", r.stderr)
        self.assertIn("excludedCommands", r.stderr)

    def test_cli_path_with_spaces_warns_sandbox_exclusion_may_not_match(self):
        """Covers R28."""
        # The CLI's own path, set through the testing hook as the Python test patched CLI_PATH.
        r = self.w.run(LEADHAND_TEST_CLI="/opt/lead hand/scripts/leadhand")
        self.assertEqual(r.code, 0, r)
        self.assertIn(SPACES_WARNING_TAIL, r.stderr)
        argv = self.w.argv()
        self.assertEqual(json.loads(argv[argv.index("--settings") + 1]),
                         {"sandbox": {"excludedCommands": ["/opt/lead hand/scripts/leadhand *"]}})
        r = self.w.run(LEADHAND_TEST_CLI="/opt/leadhand/scripts/leadhand")
        self.assertEqual(r.code, 0, r)
        self.assertNotIn("may not match", r.stderr)
        argv = self.w.argv(2)
        self.assertEqual(json.loads(argv[argv.index("--settings") + 1]),
                         {"sandbox": {"excludedCommands": ["/opt/leadhand/scripts/leadhand *"]}})


# --------------------------------------------------------------------------
# run dir, cwd, environment, exit code

class Child(LaunchCase):
    def test_supervisor_cwd_is_workspace_when_launched_elsewhere(self):
        r = self.w.run(target=self.w.paths.deck, cwd=self.w.elsewhere)
        self.assertEqual(r.code, 0, r)
        start = self.w.starts()[0]
        self.assertEqual(os.path.realpath(start["cwd"]), os.path.realpath(self.w.workspace))
        self.assertIn(f"Run the leadhand deck in {self.w.run_dir}.", start["argv"][-1])

    def test_env_has_run_id_dir_and_cli(self):
        """Covers P3 (LEADHAND_RUN_ID reaches the supervisor's session)."""
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        env = self.w.starts()[0]["env"]
        self.assertEqual({k: env[k] for k in ("LEADHAND_RUN_ID", "LEADHAND_RUN_DIR")},
                         {"LEADHAND_RUN_ID": self.w.run_id, "LEADHAND_RUN_DIR": str(self.w.run_dir)})
        self.assertTrue(env["LEADHAND_CLI"] and harness.names_bin(env["LEADHAND_CLI"]), env)

    def test_run_accepts_deck_md_path(self):
        """Covers R24."""
        r = self.w.run(target=self.w.paths.deck)
        self.assertEqual(r.code, 0, r)
        self.assertEqual(self.w.starts()[0]["env"]["LEADHAND_RUN_DIR"], str(self.w.run_dir))

    def test_not_a_run_is_an_error_and_starts_nothing(self):
        r = self.w.run(target=self.w.elsewhere)
        self.assertEqual(r.code, 1, r)
        self.assertIn("not a run", r.stderr)
        self.assertEqual(self.w.starts(), [])

    def test_child_exit_code_returned(self):
        r = self.w.run(FAKE_CLAUDE_EXIT="7")
        self.assertEqual(r.code, 7, r)
        self.assertEqual(len(self.w.starts()), 1)

    def test_a_worktree_run_launch_is_refused_while_the_repository_is_sparse(self):
        commit(self.w.repo, {"far/away.txt": "far\n"}, "far")
        run_git(self.w.repo, "sparse-checkout", "set", "--no-cone", "/README.md")
        r = self.w.run()
        self.assertEqual(r.code, 1, r)
        self.assertIn("sparse checkouts are not supported", r.stderr)
        self.assertEqual(self.w.starts(), [])


# --------------------------------------------------------------------------
# launch-line

class LaunchLine(LaunchCase):
    def line(self, r: Result) -> str:
        r.ok()
        self.assertTrue(r.stdout.endswith("\n") and r.stdout.count("\n") == 1, r)
        return r.stdout[:-1]

    def test_launch_line_quotes_paths_with_spaces(self):
        plugin = self.w.tmp / "plugin dir"
        deck = str(self.w.paths.deck)
        line = self.line(self.w.launch_line(self.w.run_dir, "--plugin-dir", plugin))
        self.assertTrue(harness.is_bin_command(line, "", " " + shlex.join(["run", deck, "--plugin-dir", str(plugin)])),
                        line)
        line = self.line(self.w.launch_line(deck))
        self.assertTrue(harness.is_bin_command(line, "", " " + shlex.join(["run", deck])), line)

    def test_launch_line_defaults_to_recorded_plugin_dir(self):
        plugin = self.w.tmp / "dev plugin"
        deck = str(self.w.paths.deck)
        r = self.w.run("--plugin-dir", plugin)
        self.assertEqual(r.code, 0, r)
        self.assertEqual(self.w.holder()["plugin_dir"], str(plugin))
        line = self.line(self.w.launch_line(self.w.run_dir))
        self.assertTrue(harness.is_bin_command(line, "", " " + shlex.join(["run", deck, "--plugin-dir", str(plugin)])),
                        line)
        other = self.w.tmp / "other plugin"
        line = self.line(self.w.launch_line(deck, "--plugin-dir", other))
        self.assertTrue(harness.is_bin_command(line, "", " " + shlex.join(["run", deck, "--plugin-dir", str(other)])),
                        line)
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertNotIn("plugin_dir", self.w.holder())
        self.assertEqual(self.w.holder()["supervisor_pid"], self.w.starts()[1]["pid"])


class SelfPathHook(LaunchCase):
    """The testing hook LEADHAND_TEST_CLI (converted from tests/test_test_hooks.py): under
    LEADHAND_TESTING=1 it takes the place of the binary's own path wherever the binary names
    itself; without LEADHAND_TESTING=1 it is ignored."""
    ROUTER = "/x/router"

    def test_self_path_override_is_honoured_only_under_leadhand_testing(self):
        """Covers limit L4 (test hooks need LEADHAND_TESTING=1)."""
        deck = str(self.w.paths.deck)
        for testing, honoured in (("1", True), ("0", False)):
            with self.subTest(testing=testing):
                env = dict(self.w.env, LEADHAND_TESTING=testing, LEADHAND_TEST_CLI=self.ROUTER)
                r = run_cli("launch-line", self.w.run_dir, env=env, cwd=self.w.elsewhere).ok()
                words = shlex.split(r.stdout)
                self.assertEqual(words[1:], ["run", deck], r)
                if honoured:
                    self.assertEqual(words[0], self.ROUTER, r)
                else:
                    self.assertTrue(harness.names_bin(words[0]), r)
        # Without the hook, under LEADHAND_TESTING=1, the binary names itself.
        line = self.w.launch_line(self.w.run_dir).ok().stdout
        self.assertTrue(harness.is_bin_command(line[:-1], "", " " + shlex.join(["run", deck])), line)

    def test_launch_and_pause_name_the_override(self):
        r = self.w.run(LEADHAND_TEST_CLI=self.ROUTER)
        self.assertEqual(r.code, 0, r)
        start = self.w.starts()[0]
        self.assertEqual(start["argv"][-1], f"Run the leadhand deck in {self.w.run_dir}. leadhand CLI: "
                                            f"{self.ROUTER}{FIRST_MESSAGE_END}")
        argv = start["argv"]
        self.assertEqual(json.loads(argv[argv.index("--settings") + 1]),
                         {"sandbox": {"excludedCommands": [f"{self.ROUTER} *"]}})
        self.assertEqual(start["env"]["LEADHAND_CLI"], self.ROUTER)
        r = run_cli("pause", self.w.run_dir, env=dict(self.w.env, LEADHAND_TEST_CLI=self.ROUTER),
                    cwd=self.w.elsewhere).ok()
        self.assertTrue(r.stdout.rstrip("\n").endswith(
            f"Continue it later with: {self.ROUTER} run {shlex.quote(str(self.w.run_dir))}"), r)


# --------------------------------------------------------------------------
# ownership: run.lock and holder.json

class Ownership(LaunchCase):
    def test_refused_launch_changes_nothing(self):
        self.w.progress()
        first = self.w.launcher()
        sup = self.w.wait_supervisor(proc=first)
        self.assertEqual(len(self.w.events("resumed")), 1)
        before = snapshot(self.w.run_dir)
        r = self.w.run()
        self.assertEqual(r.code, 3, r)
        self.assertIn(f"Run already in progress: supervisor pid {sup['pid']} since ", r.stderr)
        self.assertIn(f"(run {self.w.run_id})", r.stderr)
        self.assertEqual(snapshot(self.w.run_dir), before)
        self.assertEqual(len(self.w.starts()), 1)
        self.assertTrue(self.w.alive(sup))
        self.w.stop(first)
        self.w.finish(first)

    def test_cli_run_refusal_exits_3(self):
        first = self.w.launcher()
        sup = self.w.wait_supervisor(proc=first)
        r = self.w.run(target=self.w.paths.deck)
        self.assertEqual(r.code, 3, r)
        self.assertIn(f"Run already in progress: supervisor pid {sup['pid']}", r.stderr)
        self.w.stop(first)
        self.w.finish(first)

    def test_lock_held_by_supervisor_after_launcher_killed(self):
        launcher = self.w.launcher(sleep="3")
        sup = self.w.wait_supervisor(proc=launcher)
        launcher.kill()
        launcher.wait(timeout=WAIT)
        self.assertTrue(self.w.alive(sup), "fake supervisor died early")
        r = self.w.run()
        self.assertEqual(r.code, 3, r)
        self.assertIn(f"supervisor pid {sup['pid']}", r.stderr)
        self.w.wait_gone(sup)
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(self.w.starts()), 2)

    def test_lock_fd_alone_blocks_takeover(self):
        # holder.json names a dead supervisor, but run.lock is still flocked by a live process:
        # the supervisor, through the descriptor it inherited from its killed launcher.
        launcher = self.w.launcher(sleep="3")
        sup = self.w.wait_supervisor(proc=launcher)
        launcher.kill()
        launcher.wait(timeout=WAIT)
        self.w.write_holder({**self.w.holder(), "supervisor_start": "not it"})
        self.assertTrue(self.w.alive(sup), "fake supervisor died early")
        r = self.w.run()
        self.assertEqual(r.code, 3, r)
        self.assertEqual(len(self.w.starts()), 1)
        self.w.wait_gone(sup)

    def test_recorded_live_supervisor_blocks_takeover(self):
        """Covers limit L3: a relaunch is refused while the recorded supervisor lives."""
        self.w.paths.lock.write_text("", encoding="utf-8")       # stale: nobody holds the flock
        live = self.w.popen(["sleep", "60"], env=self.w.env)
        start = wait_for(lambda: started_by(live.pid, time.time()), "the live process")
        self.w.write_holder({"run_id": self.w.run_id, "launcher_pid": 1, "supervisor_pid": live.pid,
                             "supervisor_start": start, "started_at": "2026-11-10T00:00:00.000Z"})
        self.w.progress()
        before = snapshot(self.w.run_dir)
        r = self.w.run()
        self.assertEqual(r.code, 3, r)
        self.assertIn(f"Run already in progress: supervisor pid {live.pid} since "
                      f"2026-11-10T00:00:00.000Z (run {self.w.run_id})", r.stderr)
        self.assertEqual(snapshot(self.w.run_dir), before)
        self.assertEqual(self.w.starts(), [])

        # The same pid with another start time is a reused pid, not the supervisor.
        self.w.write_holder({**self.w.holder(), "supervisor_start": "other"})
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(self.w.starts()), 1)

    def test_resumed_appended_only_after_acquire(self):
        # While another process holds run.lock the launch is refused and appends nothing;
        # once it owns the run, it appends `resumed`, once.
        self.w.progress()
        release = self.w.hold(self.w.paths.lock)
        kinds = [e["kind"] for e in self.w.events()]
        r = self.w.run()
        self.assertEqual(r.code, 3, r)
        self.assertEqual([e["kind"] for e in self.w.events()], kinds)
        self.assertEqual(self.w.starts(), [])
        release()
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        resumed = self.w.events("resumed")
        self.assertEqual(len(resumed), 1)
        self.assertEqual(resumed[0]["run_id"], self.w.run_id)
        self.assertEqual([e["kind"] for e in self.w.events()], kinds + ["resumed"])

    def test_first_launch_of_a_fresh_run_appends_no_resumed(self):
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(self.w.starts()), 1)
        self.assertEqual([e["kind"] for e in self.w.events()], ["run_started"])

    def test_holder_records_launcher_and_supervisor(self):
        """Covers R7: the recorded start time is the process table's, read without `ps`."""
        launcher = self.w.launcher()
        sup = self.w.wait_supervisor(proc=launcher)
        h = self.w.holder()
        self.assertEqual(h["run_id"], self.w.run_id)
        self.assertEqual(h["launcher_pid"], launcher.pid)
        self.assertEqual(h["supervisor_pid"], sup["pid"])
        self.assertEqual(h["supervisor_start"], sup["start"])      # the process table's, via the fake's log
        self.assertTrue(h["started_at"])
        self.w.stop(launcher)
        self.w.finish(launcher)

    def test_signals_forwarded_to_child(self):
        for sig in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=sig.name):
                n = len(self.w.starts()) + 1
                launcher = self.w.launcher()
                sup = self.w.wait_supervisor(n, proc=launcher)
                self.w.stop(launcher, sig)
                r = self.w.finish(launcher)
                self.assertEqual(r.code, 128 + sig, r)
                self.assertEqual(self.w.signals(sup["pid"]), [int(sig)])
                self.w.wait_gone(sup)
        # The lock was released: a new launch owns the run.
        r = self.w.run()
        self.assertEqual(r.code, 0, r)

    def test_busy_lock_with_dead_recorded_supervisor_says_not_running(self):
        gone = self.w.popen(["true"], env=self.w.env)
        gone.wait(timeout=WAIT)
        self.w.write_holder({"run_id": self.w.run_id, "launcher_pid": gone.pid, "supervisor_pid": gone.pid,
                             "supervisor_start": "long gone", "started_at": "2026-11-10T00:00:00.000Z"})
        self.w.hold(self.w.paths.lock)
        r = self.w.run()
        self.assertEqual(r.code, 3, r)
        self.assertIn(f"lock held by another process (supervisor pid {gone.pid} recorded, not running)",
                      r.stderr)
        self.assertNotIn("since", r.stderr)
        self.assertEqual(self.w.starts(), [])

    def test_another_runs_live_supervisor_does_not_hold_back_a_launch(self):
        # The worktree counterpart of the in-place tests where another run of the same repository
        # was being supervised: ownership is per run, so a second worktree run of the repository
        # launches while the first run's supervisor lives and holds its own run.lock.
        other = self.w.sup.start(new=True)
        self.assertNotEqual(other, self.w.run_dir)
        first = self.w.launcher()
        sup = self.w.wait_supervisor(proc=first)
        r = self.w.run(target=other)
        self.assertEqual(r.code, 0, r)
        second = self.w.starts()[1]
        self.assertEqual(second["env"]["LEADHAND_RUN_DIR"], str(other))
        self.assertEqual(read_json(run_paths(other).holder)["supervisor_pid"], second["pid"])
        self.assertTrue(self.w.alive(sup))
        self.assertFalse(lockable(self.w.paths.lock))           # still the first run's
        self.w.stop(first)
        self.w.finish(first)


# --------------------------------------------------------------------------
# keep-awake beside the supervisor

@unittest.skipUnless(sys.platform == "darwin" or sys.platform.startswith("linux"),
                     "keep-awake runs only on macOS and Linux")
class KeepAwake(LaunchCase):
    def test_keep_awake_runs_beside_claude_which_stays_the_direct_child(self):
        """macOS: `caffeinate -i -w <supervisor pid>`; Linux: `systemd-inhibit --what=idle
        --why='leadhand run' tail --pid=<supervisor pid> -f /dev/null`. Either is a child of
        the launcher beside the supervisor, and is gone when the launch returns.
        Covers R27."""
        mine = "caffeinate" if sys.platform == "darwin" else "systemd-inhibit"
        for name in ("caffeinate", "systemd-inhibit"):
            self.w.tool(name, LOGGING_TOOL)
        launcher = self.w.launcher(sleep="1")
        r = self.w.finish(launcher)
        self.assertEqual(r.code, 0, r)
        start = self.w.starts()[0]
        self.claude_ok(start["argv"][0])
        self.assertEqual(start["ppid"], launcher.pid)
        self.assertEqual(self.w.holder()["supervisor_pid"], start["pid"])
        recs = read_jsonl(self.w.keep_awake_log)
        self.assertEqual([Path(r["argv"][0]).name for r in recs], [mine])
        rec = recs[0]
        self.assertEqual(rec["ppid"], launcher.pid)
        if sys.platform == "darwin":
            self.assertEqual(rec["argv"][1:], ["-i", "-w", str(start["pid"])])
        else:
            self.assertEqual(rec["argv"][1:], ["--what=idle", "--why=leadhand run",
                                               "tail", f"--pid={start['pid']}", "-f", "/dev/null"])
        self.assertIsNone(started_by(rec["pid"], rec["time"]), "keep-awake left running")

    def test_only_the_platforms_keep_awake_tool_is_used(self):
        """The other platform's tool on PATH is never started (macOS ignores systemd-inhibit)."""
        other = "systemd-inhibit" if sys.platform == "darwin" else "caffeinate"
        self.w.tool(other, LOGGING_TOOL)
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(self.w.starts()), 1)
        self.assertFalse(self.w.keep_awake_log.exists())


# --------------------------------------------------------------------------
# R41(a): CLI children of the supervisor drop the inherited run.lock descriptor

class InheritedLock(LaunchCase):
    BIG = "x" * (1 << 20)      # far more than a pipe holds: writing it all means it was read

    def started_dispatch(self) -> tuple[Path, str]:
        """A dispatch recorded as started, on the world's run the first time, then on a new run
        of the deck each time (one result each)."""
        if getattr(self, "_used", False):
            self.w.sup.start(new=True)
        self._used = True
        run_dir = self.w.sup.run_dir
        did = self.w.progress()
        self.w.sup.cli("record", run_dir, "started", "--dispatch", did, "--worker", "fake-agent").ok()
        return run_dir, did

    def holding(self, path: Path, *, run_dir: Path | None) -> subprocess.Popen:
        """`record RUN result --dispatch ID --reply -` for a started dispatch, started with a
        descriptor on `path` that holds its exclusive flock, inherited and named in
        LEADHAND_LOCK_FD (and LEADHAND_RUN_DIR when `run_dir`). The test's own copy is closed, so
        the command holds the only one. Returns once the command has read most of its reply from
        stdin, so it is past its startup; it then waits for the rest."""
        target, did = self.started_dispatch()
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            env = dict(self.w.env, LEADHAND_LOCK_FD=str(fd))
            if run_dir is not None:
                env["LEADHAND_RUN_DIR"] = str(run_dir)
            p = self.w.popen([BIN, "record", target, "result", "--dispatch", did, "--worker", "fake-agent",
                              "--reply", "-"], env=env, pass_fds=(fd,), stdin=subprocess.PIPE)
        finally:
            os.close(fd)
        self.write_big(p.stdin.fileno(), p, "the command to read its reply from stdin")
        self.assertIsNone(p.poll(), "the command exited before its stdin was closed")
        return p

    def done(self, p: subprocess.Popen) -> None:
        """Close the command's stdin; it records the result."""
        p.stdin.close()
        self.assertEqual(self.w.finish(p).ok().json()["kind"], "result_recorded")

    def feed(self, fifo: Path, proc: subprocess.Popen):
        """Open `fifo` for writing once a reader has it open (polled while `proc` runs) and write
        BIG into it; returns the open writer (the cleanup closes it). Once the write returns, the
        reader has read most of it, so it is past its startup; it then waits for the rest."""
        def opened():
            try:
                return os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
            except OSError as e:
                if e.errno == errno.ENXIO:          # no reader yet
                    return None
                raise
        fd = wait_for(opened, "the CLI child to open its stdin", proc=proc)
        writer = os.fdopen(fd, "wb")
        self.addCleanup(writer.close)
        self.write_big(fd, proc, "the CLI child to read its reply from the FIFO")
        return writer

    def write_big(self, fd: int, proc: subprocess.Popen, what: str) -> None:
        """Write BIG to `fd` in non-blocking chunks, polled as wait_for(proc=proc) polls: a reader
        that stays alive but stops reading fails the test after WAIT seconds instead of blocking it.
        `fd` is left blocking again."""
        os.set_blocking(fd, False)
        rest = memoryview(self.BIG.encode())

        def written():
            nonlocal rest
            try:
                rest = rest[os.write(fd, rest[:1 << 16]):]
            except BlockingIOError:
                pass
            return not rest
        try:
            wait_for(written, what, proc=proc)
        finally:
            os.set_blocking(fd, True)

    def test_relaunch_succeeds_while_a_cli_child_of_the_killed_supervisor_runs(self):
        """Covers limit L3 (the inherited run lock) and R41(a)."""
        run_dir, did = self.started_dispatch()
        exported = self.w.tmp / "lock fd env"
        fifo = self.w.tmp / "reply fifo"
        os.mkfifo(fifo)
        # The supervisor's long-lived CLI command (`record ... --reply -`, reading its reply from
        # a pipe the test writes), started the way Claude Code starts a Bash command: inheriting
        # every descriptor.
        spawn = json.dumps(["/bin/sh", "-c",
                            'printf %s "${LEADHAND_LOCK_FD-}" > "$0"; '
                            'exec "$1" record "$2" result --dispatch "$3" --worker fake-agent --reply - < "$4"',
                            str(exported), str(BIN), str(run_dir), did, str(fifo)])
        launcher = self.w.launcher(sleep="60", FAKE_CLAUDE_SPAWN=spawn)
        sup = self.w.wait_supervisor(proc=launcher)
        rec = wait_for(lambda: next((r for r in self.w.records() if r.get("spawned")), None), "the CLI child to run")
        child = {"pid": rec["spawned"], "start": started_by(rec["spawned"], rec["time"])}
        self.assertTrue(child["start"], "the CLI child exited at once")
        writer = self.feed(fifo, launcher)
        self.assertTrue(exported.read_text(encoding="utf-8").isdigit(), "launcher did not export LEADHAND_LOCK_FD")
        # The CLI child is past its startup. It closed its copy of the descriptor and never
        # unlocked it: the lock is shared with the launcher and the supervisor, so while they
        # live the run stays locked and a second launch is refused.
        self.assertTrue(self.w.alive(sup) and self.w.alive(child) and launcher.poll() is None)
        self.assertFalse(lockable(self.w.paths.lock), "a CLI child unlocked the live supervisor's run.lock")
        r = self.w.run()
        self.assertEqual(r.code, 3, r)
        self.assertEqual(len(self.w.starts()), 1)
        launcher.kill()
        launcher.wait(timeout=WAIT)
        os.kill(sup["pid"], signal.SIGKILL)
        self.w.wait_gone(sup)
        # The supervisor is gone; its CLI child is not, and does not keep the run locked.
        self.assertTrue(self.w.alive(child), "the CLI child exited early")
        self.assertTrue(lockable(self.w.paths.lock), "the CLI child kept the run locked")
        r = self.w.run()
        self.assertEqual(r.code, 0, r)
        self.assertTrue(self.w.alive(child))
        writer.close()                          # EOF: the CLI child records the result and exits
        self.w.wait_gone(child)

    def test_cli_closes_inherited_run_lock_fd(self):
        """Covers limit L3 and R41(a)."""
        p = self.holding(self.w.paths.lock, run_dir=self.w.run_dir)
        self.assertTrue(lockable(self.w.paths.lock), "the command kept the inherited run.lock descriptor")
        self.assertIsNone(p.poll())
        self.done(p)

    def test_cli_leaves_an_unrelated_fd_open(self):
        other = self.w.tmp / "unrelated.txt"
        other.write_text("x", encoding="utf-8")
        p = self.holding(other, run_dir=self.w.run_dir)
        self.assertFalse(lockable(other), "the command closed an unrelated descriptor")
        self.done(p)
        self.assertTrue(lockable(other))
        # A run.lock descriptor, but no LEADHAND_RUN_DIR to verify it against: left open too.
        p = self.holding(self.w.paths.lock, run_dir=None)
        self.assertFalse(lockable(self.w.paths.lock), "the command closed a descriptor it could not verify")
        self.done(p)
        self.assertTrue(lockable(self.w.paths.lock))
        # Not a number, or not open: ignored.
        version = run_cli("version", env=self.w.env, cwd=self.w.elsewhere).ok().stdout
        self.assertTrue(version)
        for value in ("-1", "nope", "987"):
            with self.subTest(LEADHAND_LOCK_FD=value):
                env = dict(self.w.env, LEADHAND_LOCK_FD=value, LEADHAND_RUN_DIR=str(self.w.run_dir))
                self.assertEqual(run_cli("version", env=env, cwd=self.w.elsewhere).ok().stdout, version)


class LockFdEnvironment(unittest.TestCase):
    """R41(a): every command drops LEADHAND_LOCK_FD from its environment, whatever it decides
    about the descriptor, so nothing it starts inherits the variable. Observed through a Codex
    dispatch: the fake Codex logs its whole environment."""

    def dispatch_with(self, w: DispatchWorld, did: str, path: Path) -> dict:
        """The `dispatch` line run with a descriptor on `path` inherited and named in
        LEADHAND_LOCK_FD, and LEADHAND_RUN_DIR naming the run; returns the fake Codex's
        environment."""
        w.scenario("ready")
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            env = dict(w.env, LEADHAND_LOCK_FD=str(fd), LEADHAND_RUN_DIR=str(w.run_dir))
            p = subprocess.run([str(BIN), *(str(a) for a in w.args(did))], cwd=w.tmp, env=env, pass_fds=(fd,),
                               stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
        finally:
            os.close(fd)
        self.assertEqual(p.returncode, 0, p)
        starts = [r for r in w.records("start") if r["argv"][-3:-1] == ["-o", str(w.wd(did) / "last-message.md")]]
        self.assertEqual(len(starts), 1)
        return starts[0]["env"]

    def test_a_worker_never_inherits_lead_hand_lock_fd(self):
        w = DispatchWorld(self)
        unrelated = w.tmp / "unrelated.txt"
        unrelated.write_text("x", encoding="utf-8")
        for did, path in (("d1", w.run_dir / "run.lock"), ("d2", unrelated)):
            with self.subTest(fd_on=path.name):
                env = self.dispatch_with(w, did, path)
                self.assertEqual(env["LEADHAND_RUN_ID"], w.run_id)       # the worker's env was logged
                self.assertFalse("LEADHAND_LOCK_FD" in env, "the worker inherited LEADHAND_LOCK_FD")


if __name__ == "__main__":
    unittest.main()
