import contextlib
import fcntl
import io
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from gitfixture import hermetic, make_repo
from leadhand import cli, dispatch as dsp, procs
from leadhand.briefs import worker_prompt
from leadhand.dispatch import (DispatchError, DispatchRequest, DispatchStillRunning, blocking_env,
                               cancel, codex_argv, dispatch)
from leadhand.state import read_attempts, run_paths

ROOT = Path(__file__).resolve().parents[1]
FAKES = ROOT / "tests" / "fakes"
FAKE = FAKES / "codex"
SCENARIOS = ROOT / "tests" / "fixtures" / "scenarios"
CLI = ROOT / "scripts" / "leadhand"
ATTEMPT_FIELDS = {"dispatch", "run", "card", "attempt", "role", "vendor", "model", "effort", "worker_id",
                  "started_at", "ended_at", "steps", "charge_ids", "usage", "usage_known",
                  "cost_estimate", "exit_reason", "status", "blocked_by"}


def wait_for(cond, timeout=15.0, step=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = cond()
        if value:
            return value
        time.sleep(step)
    raise AssertionError(f"timed out after {timeout}s waiting for {cond}")


def alive(pid):
    info = procs.process_info(pid)
    return info is not None and not info.zombie


class Base(unittest.TestCase):
    def setUp(self):
        hermetic(self)
        self.tmp = Path(tempfile.mkdtemp(prefix="lh dispatch "))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = make_repo(self.tmp)                       # "<tmp>/my repo"
        self.run_dir = self.tmp / "docs dir" / "runs" / "2026-11-10-demo run"
        self.run_dir.mkdir(parents=True)
        self.log = self.tmp / "codex log.jsonl"
        self.codex_home = self.tmp / "codex home"
        env = mock.patch.dict(os.environ, {
            "FAKE_CODEX_LOG": str(self.log), "CODEX_HOME": str(self.codex_home),
            "LEADHAND_TESTING": "1", "LEADHAND_CONFIG": str(self.tmp / "no user config.toml"),
            "GH_TOKEN": "parent-gh-token", "GITHUB_TOKEN": "parent-github-token",
            "SSH_AUTH_SOCK": "/nonexistent/agent.sock"})
        env.start()
        self.addCleanup(env.stop)
        for name in ("FAKE_CODEX_SCENARIO", "FAKE_CODEX_DELAY", "FAKE_CODEX_THREAD", "FAKE_CODEX_NO_ROLLOUT",
                     "FAKE_CODEX_IGNORE_SIGINT", "FAKE_CODEX_ORPHAN_CHILD", "LEADHAND_CRASH_AT",
                     "LEADHAND_MINUTE_SECONDS"):
            os.environ.pop(name, None)
        self.addCleanup(self.kill_leftovers)

    def kill_leftovers(self):
        pids = []
        for runner in self.run_dir.glob("workers/*/**/runner.json"):
            data = json.loads(runner.read_text())
            for who in ("runner", "codex"):
                rec = data.get(who) or {}
                if procs.same_process(rec.get("pid"), rec.get("start")):
                    pids.append(rec["pid"])
        pids += [r["child_pid"] for r in self.records("child")]
        for pid in pids:
            with contextlib.suppress(ProcessLookupError, PermissionError):
                os.kill(pid, signal.SIGKILL)

    @staticmethod
    def signal_quietly(pid, sig):
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.kill(pid, sig)

    def scenario(self, name, **env):
        os.environ["FAKE_CODEX_SCENARIO"] = str(SCENARIOS / f"{name}.jsonl")
        for k, v in env.items():
            os.environ[f"FAKE_CODEX_{k.upper()}"] = str(v)

    def request(self, dispatch_id="d1", **kw):
        folder = self.repo / ".leadhand" / "1" / dispatch_id
        folder.mkdir(parents=True, exist_ok=True)
        brief = folder / "brief.md"
        brief.write_text("# Brief\n")
        fields = dict(model="gpt-6-luna", effort="low", role="implementer", brief=brief,
                      report=folder / "report.md", cwd=self.repo, run_dir=self.run_dir, card=1,
                      attempt=1, dispatch_id=dispatch_id, steps=50, minutes=5, network=False,
                      writable_dirs=(), resume=None, codex_bin=str(FAKE))
        fields.update(kw)
        return DispatchRequest(**fields)

    def worker_dir(self, dispatch_id="d1"):
        return self.run_dir / "workers" / dispatch_id

    def records(self, event=None):
        if not self.log.exists():
            return []
        out = [json.loads(l) for l in self.log.read_text().splitlines() if l.strip()]
        return [r for r in out if event is None or r.get("event") == event]

    def runner_json(self, dispatch_id="d1"):
        path = self.worker_dir(dispatch_id) / "runner.json"
        return json.loads(path.read_text()) if path.exists() else None

    def start_detached(self, req):
        with self.assertRaises(DispatchStillRunning):
            dispatch(req, wait=0.2)
        return wait_for(lambda: (self.runner_json(req.dispatch_id) or {}).get("codex"))


class ArgvTests(Base):
    def argv(self, **kw):
        req = self.request(**kw)
        return codex_argv(req, Path("/w d/last message.md"), Path("/repo dir/.git"),
                          {"GIT_TERMINAL_PROMPT": "0"})

    def test_argv_passes_network_false_explicitly(self):
        argv = self.argv(network=False)
        i = argv.index("sandbox_workspace_write.network_access=false")
        self.assertEqual(argv[i - 1], "-c")
        self.assertNotIn("sandbox_workspace_write.network_access=true", argv)

    def test_argv_network_true_when_enabled(self):
        argv = self.argv(network=True)
        self.assertIn("sandbox_workspace_write.network_access=true", argv)
        self.assertNotIn("sandbox_workspace_write.network_access=false", argv)

    def test_argv_workspace_write_with_git_common_dir_and_effort(self):
        extra = self.tmp / "extra dir"
        argv = self.argv(effort="high", writable_dirs=(extra,))
        self.assertEqual(argv[:18], [
            str(FAKE), "exec", "-m", "gpt-6-luna",
            "-c", 'model_reasoning_effort="high"', "-c", 'approval_policy="never"',
            "-c", "sandbox_workspace_write.network_access=false",
            "-C", str(self.repo), "-s", "workspace-write",
            "--add-dir", "/repo dir/.git", "--add-dir", str(extra)])
        self.assertEqual(argv[-4:], ["--json", "-o", "/w d/last message.md", "-"])
        self.assertFalse([a for a in argv if "compact" in a])

    def test_argv_scout_is_read_only(self):
        argv = self.argv(role="scout")
        self.assertEqual(argv[argv.index("-s") + 1], "read-only")
        self.assertNotIn("workspace-write", argv)

    def test_argv_sets_blocking_env_through_shell_policy(self):
        gh_dir = self.tmp / "gh config"
        parent = {"PATH": "/usr/bin", "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "user.name",
                  "GIT_CONFIG_VALUE_0": "Some One"}
        overrides = blocking_env(parent, gh_config_dir=gh_dir)
        self.assertEqual(overrides["GIT_SSH_COMMAND"], "false")
        self.assertEqual(overrides["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(overrides["GH_CONFIG_DIR"], str(gh_dir))
        self.assertEqual(overrides["GIT_CONFIG_COUNT"], "2")
        self.assertEqual(overrides["GIT_CONFIG_KEY_0"], "user.name")
        self.assertEqual((overrides["GIT_CONFIG_KEY_1"], overrides["GIT_CONFIG_VALUE_1"]),
                         ("credential.helper", ""))
        argv = codex_argv(self.request(), Path("/l"), Path("/g"), overrides)
        for name, value in overrides.items():
            i = argv.index(f"shell_environment_policy.set.{name}={json.dumps(value)}")
            self.assertEqual(argv[i - 1], "-c")

        # End to end: the fake Codex sees the same overrides, and no credentials in its env.
        self.scenario("ready")
        dispatch(self.request())
        start = self.records("start")[0]
        policy = start["policy_set"]
        self.assertEqual(policy["GIT_SSH_COMMAND"], "false")
        self.assertEqual(policy["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(policy["GH_CONFIG_DIR"], start["env"]["GH_CONFIG_DIR"])
        count = int(policy["GIT_CONFIG_COUNT"])
        self.assertEqual((policy[f"GIT_CONFIG_KEY_{count - 1}"], policy[f"GIT_CONFIG_VALUE_{count - 1}"]),
                         ("credential.helper", ""))
        for name in ("GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"):
            self.assertNotIn(name, start["env"])
        self.assertEqual(os.listdir(start["env"]["GH_CONFIG_DIR"]), [])

    def test_resume_argv_matches_findings(self):
        fresh = codex_argv(self.request(), Path("/l"), Path("/g"), {})
        resumed = codex_argv(self.request(resume="thread-123"), Path("/l"), Path("/g"), {})
        self.assertEqual(resumed[-3:], ["resume", "thread-123", "-"])
        self.assertEqual(resumed[:-3], fresh[:-1])  # the same options, before `resume`
        self.assertIn("-m", resumed[:resumed.index("resume")])

        self.scenario("ready")
        req = self.request(resume="thread-123")
        result = dispatch(req)
        start = self.records("start")[0]
        self.assertEqual(start["argv"][-3:], ["resume", "thread-123", "-"])
        self.assertEqual(start["prompt"], worker_prompt("implementer", req.brief, req.report))
        self.assertEqual(result.session_id, "thread-123")


class ResultTests(Base):
    def test_ready_reply_parsed_and_attempt_recorded(self):
        self.scenario("ready", thread="11111111-2222-4333-8444-555555555555")
        req = self.request()
        result = dispatch(req)
        self.assertEqual((result.status, result.blocked_by, result.exit_reason), ("READY", "", "completed"))
        self.assertEqual(result.session_id, "11111111-2222-4333-8444-555555555555")
        self.assertEqual(result.steps, 2)
        self.assertTrue(result.reply.startswith("Status: READY"))
        self.assertTrue(result.usage.known)
        self.assertEqual(result.usage.output, 50)
        start = self.records("start")[0]
        self.assertEqual(start["prompt"], worker_prompt("implementer", req.brief, req.report))
        self.assertEqual(Path(start["cwd"]).resolve(), self.repo.resolve())
        self.assertEqual((self.repo / "done.txt").read_text(), "done\n")
        [attempt] = read_attempts(run_paths(self.run_dir))
        self.assertTrue(ATTEMPT_FIELDS <= set(attempt), ATTEMPT_FIELDS - set(attempt))
        self.assertEqual((attempt["dispatch"], attempt["card"], attempt["attempt"], attempt["role"]),
                         ("d1", 1, 1, "implementer"))
        self.assertEqual((attempt["vendor"], attempt["model"], attempt["effort"]), ("codex", "gpt-6-luna", "low"))
        self.assertEqual(attempt["worker_id"], result.session_id)
        self.assertEqual((attempt["status"], attempt["exit_reason"], attempt["steps"]), ("READY", "completed", 2))
        self.assertEqual(attempt["charge_ids"], [f"d1:codex:{result.session_id}"])
        self.assertTrue(attempt["usage_known"])
        self.assertIsNone(attempt["cost_estimate"])  # Codex models are not priced
        self.assertEqual(attempt["run"], self.run_dir.name)
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertEqual(saved["status"], "READY")

    def test_reply_without_status_is_blocked_and_raw_kept(self):
        self.scenario("no-status")
        result = dispatch(self.request())
        self.assertEqual((result.status, result.blocked_by, result.exit_reason), ("BLOCKED", "", "completed"))
        raw = (self.worker_dir() / "reply.raw.md").read_text()
        self.assertEqual(raw, "I looked around and stopped.\nNothing else to say.\n")
        self.assertEqual(read_attempts(run_paths(self.run_dir))[0]["status"], "BLOCKED")

    def test_step_limit_stops_process_and_marks_spent(self):
        self.scenario("many-steps")
        t0 = time.monotonic()
        result = dispatch(self.request(steps=3))
        self.assertLess(time.monotonic() - t0, 20)
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "step_limit"))
        self.assertGreaterEqual(result.steps, 3)
        self.assertLess(result.steps, 9)
        codex_pid = self.runner_json()["codex"]["pid"]
        self.assertFalse(alive(codex_pid))
        self.assertEqual(self.records("exit")[-1]["how"], "sigint")

    def test_time_limit_stops_process_and_marks_spent(self):
        os.environ["LEADHAND_MINUTE_SECONDS"] = "1"
        self.scenario("slow")
        t0 = time.monotonic()
        result = dispatch(self.request(minutes=1))
        self.assertLess(time.monotonic() - t0, 20)
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "time_limit"))
        self.assertFalse(alive(self.runner_json()["codex"]["pid"]))
        self.assertFalse(alive(self.records("child")[0]["child_pid"]))

    def test_usage_limit_is_blocked_by_usage_limit(self):
        self.scenario("usage-limit")
        result = dispatch(self.request())
        self.assertEqual((result.status, result.blocked_by, result.exit_reason),
                         ("BLOCKED", "usage limit", "usage_limit"))
        self.assertEqual(read_attempts(run_paths(self.run_dir))[0]["blocked_by"], "usage limit")

    def test_killed_worker_usage_from_session_or_unknown(self):
        # With a session file: usage comes from it although no turn completed.
        self.scenario("killed", thread="00000000-0000-4000-8000-000000000001")
        result = dispatch(self.request(steps=1))
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "step_limit"))
        self.assertTrue(result.usage.known)
        self.assertGreater(result.usage.input + result.usage.cached, 0)
        charges = [json.loads(l) for l in (self.run_dir / "charges.jsonl").read_text().splitlines()]
        self.assertEqual(charges[0]["charge_id"], "d1:codex:00000000-0000-4000-8000-000000000001")
        # Without one: unknown, never zero.
        self.scenario("killed", thread="00000000-0000-4000-8000-000000000002", no_rollout=1)
        result = dispatch(self.request("d2", steps=1))
        self.assertFalse(result.usage.known)
        attempt = read_attempts(run_paths(self.run_dir))[-1]
        self.assertEqual((attempt["dispatch"], attempt["usage"], attempt["usage_known"], attempt["cost_estimate"]),
                         ("d2", None, False, None))

    def test_missing_codex_binary_is_blocked_environment(self):
        result = dispatch(self.request(codex_bin=str(self.tmp / "no such codex")))
        self.assertEqual((result.status, result.blocked_by, result.exit_reason), ("BLOCKED", "environment", "error"))
        self.assertIn("cannot start", (self.worker_dir() / "codex.stderr").read_text())

    def test_paths_with_spaces(self):
        self.scenario("ready")
        extra = self.tmp / "extra dir"
        extra.mkdir()
        req = self.request(writable_dirs=(extra,))
        result = dispatch(req)
        self.assertEqual(result.status, "READY")
        argv = self.records("start")[0]["argv"]
        self.assertEqual(argv[argv.index("-C") + 1], str(self.repo))
        self.assertIn(str(extra), argv)
        self.assertEqual(argv[argv.index("-o") + 1], str(self.worker_dir() / "last-message.md"))
        self.assertIn(" ", str(self.worker_dir()))
        self.assertTrue((self.worker_dir() / "stream.jsonl").exists())


class OwnershipTests(Base):
    def test_runner_survives_foreground_death_and_records_result(self):
        self.scenario("ready", delay=2)
        req = self.request()
        env = dict(os.environ, PATH=f"{FAKES}{os.pathsep}{os.environ['PATH']}")
        fg = subprocess.Popen(
            [sys.executable, str(CLI), "dispatch", "--run", str(self.run_dir), "--dispatch-id", "d1",
             "--card", "1", "--attempt", "1", "--role", "implementer", "--model", "gpt-6-luna",
             "--effort", "low", "--brief", str(req.brief), "--report", str(req.report), "--cwd", str(self.repo)],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        self.addCleanup(lambda: (fg.kill(), fg.communicate()))
        wait_for(lambda: (self.runner_json() or {}).get("codex"))
        os.killpg(fg.pid, signal.SIGKILL)
        fg.wait(10)
        result = wait_for(lambda: (self.worker_dir() / "result.json").exists(), timeout=20)
        self.assertTrue(result)
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertEqual(saved["status"], "READY")
        [attempt] = read_attempts(run_paths(self.run_dir))
        self.assertEqual(attempt["dispatch"], "d1")
        # Limits came from the run's config (the default preset's implementer limits).
        request = json.loads((self.worker_dir() / "request.json").read_text())
        self.assertEqual((request["steps"], request["minutes"], request["network"]), (150, 60, False))

    def test_second_dispatch_attaches_to_live_runner(self):
        self.scenario("ready", delay=2)
        req = self.request()
        first = self.start_detached(req)
        runner = self.runner_json()["runner"]
        result = dispatch(req)
        self.assertEqual(result.status, "READY")
        self.assertEqual(len(self.records("start")), 1)
        self.assertEqual(self.runner_json()["runner"], runner)
        self.assertEqual(self.runner_json()["codex"], first)

    def test_repeated_dispatch_returns_existing_result(self):
        self.scenario("ready")
        req = self.request()
        first = dispatch(req)
        second = dispatch(req)
        self.assertEqual(first, second)
        self.assertEqual(len(self.records("start")), 1)
        self.assertEqual(len(read_attempts(run_paths(self.run_dir))), 1)

    def test_cli_prints_reply_and_session_or_still_running(self):
        self.scenario("ready", delay=1)
        req = self.request()
        args = ["dispatch", "--run", str(self.run_dir), "--dispatch-id", "d1", "--card", "1", "--attempt", "1",
                "--role", "implementer", "--model", "gpt-6-luna", "--effort", "low", "--brief", str(req.brief),
                "--report", str(req.report), "--cwd", str(self.repo)]
        path = f"{FAKES}{os.pathsep}{os.environ['PATH']}"
        with mock.patch.dict(os.environ, {"PATH": path}), mock.patch.object(dsp, "FOREGROUND_WAIT", 0.2):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = cli.main(args)
        self.assertEqual(code, dsp.EXIT_STILL_RUNNING)
        self.assertIn("still running: run the same command again", out.getvalue())
        with mock.patch.dict(os.environ, {"PATH": path}):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = cli.main(args)
        self.assertEqual(code, 0)
        lines = out.getvalue().splitlines()
        self.assertEqual(lines[0], "Status: READY")
        self.assertEqual(lines[-1], "session: 0f0f0f0f-0000-4000-8000-00000000c0de")
        self.assertEqual(len(self.records("start")), 1)


class CancelTests(Base):
    def test_signal_only_after_pid_and_start_time_match(self):
        decoy = subprocess.Popen(["sleep", "30"], start_new_session=True, stdin=subprocess.DEVNULL)
        self.addCleanup(lambda: (decoy.kill(), decoy.wait()))
        real = procs.process_start_time(decoy.pid)
        wrong = {"pid": decoy.pid, "pgid": decoy.pid, "start": real + "-not"}
        req = self.request()
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        (wd / "request.json").write_text(json.dumps(dsp.request_to_json(req)))
        (wd / "runner.json").write_text(json.dumps({"runner": wrong, "codex": wrong, "generation": 1}))
        (wd / "procs.json").write_text(json.dumps({"descendants": [wrong]}))
        result = cancel(wd, timeout=20)
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "cancelled"))
        time.sleep(0.3)
        self.assertIsNone(decoy.poll(), "a process whose start time did not match was signalled")
        # dispatch treats the mismatched runner as gone: a fresh dispatch id runs normally.
        self.scenario("ready")
        wd2 = self.worker_dir("d2")
        wd2.mkdir(parents=True)
        (wd2 / "runner.json").write_text(json.dumps({"runner": wrong, "codex": wrong, "generation": 1}))
        self.assertEqual(dispatch(self.request("d2")).status, "READY")
        self.assertIsNone(decoy.poll())

    def test_cancel_detached_worker_stops_every_process_and_writes_nothing_after(self):
        self.scenario("slow")
        req = self.request()
        codex = self.start_detached(req)
        child = wait_for(lambda: self.records("child"))[0]["child_pid"]
        progress = self.repo / "progress.log"
        wait_for(lambda: progress.exists() and progress.stat().st_size > 0)
        result = cancel(self.worker_dir())
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "cancelled"))
        self.assertFalse(alive(codex["pid"]))
        self.assertFalse(alive(child))
        before = progress.stat()
        time.sleep(3)
        after = progress.stat()
        self.assertEqual((before.st_size, before.st_mtime_ns), (after.st_size, after.st_mtime_ns))
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "cancelled"))
        self.assertEqual(read_attempts(run_paths(self.run_dir))[0]["exit_reason"], "cancelled")

    def test_stale_heartbeat_on_live_runner_is_cancelled_not_replaced_alongside(self):
        self.scenario("slow")
        req = self.request()
        first_codex = self.start_detached(req)
        first = self.runner_json()
        runner_pid = first["runner"]["pid"]
        os.kill(runner_pid, signal.SIGSTOP)          # hung: alive, but no heartbeat
        self.addCleanup(self.signal_quietly, runner_pid, signal.SIGCONT)
        old = time.time() - 120
        os.utime(self.worker_dir() / "heartbeat", (old, old))
        self.scenario("ready")
        result = dispatch(req)
        self.assertEqual(result.status, "READY")
        self.assertFalse(alive(runner_pid))
        self.assertFalse(alive(first_codex["pid"]))
        superseded = json.loads((self.worker_dir() / "superseded" / "1" / "result.json").read_text())
        self.assertEqual((superseded["status"], superseded["exit_reason"]), ("SPENT", "cancelled"))
        second = self.runner_json()
        self.assertNotEqual(second["runner"]["pid"], runner_pid)
        self.assertGreaterEqual(second["started_wall"], superseded["confirmed_at"])
        starts = self.records("start")
        exits = [r for r in self.records("exit") if r["pid"] == first_codex["pid"]]
        self.assertEqual(len(starts), 2)
        self.assertTrue(exits)
        self.assertGreaterEqual(starts[1]["time"], exits[0]["time"])
        self.assertEqual(len(read_attempts(run_paths(self.run_dir))), 2)

    def test_orphaned_codex_group_is_terminated_before_finalizing(self):
        self.scenario("slow", orphan_child=1)
        req = self.request()
        codex = self.start_detached(req)
        child = wait_for(lambda: self.records("child"))[0]["child_pid"]
        wait_for(lambda: any(d["pid"] == child for d in json.loads(
            (self.worker_dir() / "procs.json").read_text()).get("descendants", []))
            if (self.worker_dir() / "procs.json").exists() else False)
        runner_pid = self.runner_json()["runner"]["pid"]
        os.kill(runner_pid, signal.SIGKILL)
        wait_for(lambda: not alive(runner_pid))
        self.assertTrue(alive(codex["pid"]))
        result = cancel(self.worker_dir())
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "cancelled"))
        self.assertFalse(alive(codex["pid"]))
        self.assertFalse(alive(child))
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertIn(child, [p["pid"] for p in saved["stopped"]])
        self.assertEqual(len(read_attempts(run_paths(self.run_dir))), 1)


class FixRoundTests(Base):
    """Review fix round 1."""

    def cli_args(self, req):
        return [sys.executable, str(CLI), "dispatch", "--run", str(self.run_dir), "--dispatch-id", req.dispatch_id,
                "--card", "1", "--attempt", "1", "--role", "implementer", "--model", "gpt-6-luna",
                "--effort", "low", "--brief", str(req.brief), "--report", str(req.report), "--cwd", str(self.repo)]

    def stop_runner(self):
        pid = self.runner_json()["runner"]["pid"]
        os.kill(pid, signal.SIGSTOP)
        self.addCleanup(self.signal_quietly, pid, signal.SIGCONT)
        return pid

    def age_heartbeat(self):
        old = time.time() - 120
        os.utime(self.worker_dir() / "heartbeat", (old, old))

    def test_run_id_passed_through_shell_policy(self):
        self.scenario("ready")
        dispatch(self.request())
        start = self.records("start")[0]
        self.assertEqual(start["policy_set"]["LEADHAND_RUN_ID"], self.run_dir.name)
        self.assertEqual(start["env"]["LEADHAND_RUN_ID"], self.run_dir.name)

    def test_stream_usage_is_charged_when_session_missing(self):
        thread = "22222222-2222-4333-8444-555555555555"
        self.scenario("ready", thread=thread)
        result = dispatch(self.request())
        [c] = [json.loads(l) for l in (self.run_dir / "charges.jsonl").read_text().splitlines()]
        self.assertEqual((c["charge_id"], c["source"], c["key"]), (f"d1:codex:{thread}", "codex-stream", thread))
        self.assertTrue(c["usage"]["known"])
        [attempt] = read_attempts(run_paths(self.run_dir))
        self.assertEqual(attempt["usage"], c["usage"])
        self.assertEqual(c["usage"]["output"], result.usage.output)

    def test_usage_limit_text_does_not_override_clean_ready_reply(self):
        self.scenario("ready-limit-warning")
        result = dispatch(self.request())
        self.assertEqual((result.status, result.blocked_by, result.exit_reason), ("READY", "", "completed"))

    def test_runner_exception_is_blocked_environment(self):
        self.assertEqual(dsp._classify("error", None, "", [], "")[:3], ("BLOCKED", "environment", "error"))

    def test_unknown_codex_start_time_still_enforced(self):
        p = subprocess.Popen(["sleep", "30"], start_new_session=True, stdin=subprocess.DEVNULL)
        self.addCleanup(lambda: (p.kill(), p.wait()))
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        tracker = dsp._Tracker(wd, {"pid": p.pid, "pgid": p.pid, "start": None}, proc=p)
        self.assertTrue(tracker.codex_alive())
        self.assertEqual(tracker.stop(deadline=time.monotonic() + 15), [])
        self.assertIsNotNone(p.poll())

    def test_runner_record_without_start_time_is_not_dead_while_lock_held(self):
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        (wd / "runner.json").write_text(json.dumps(
            {"runner": {"pid": os.getpid(), "pgid": os.getpgid(0), "start": None}, "codex": None, "generation": 1}))
        (wd / "heartbeat").touch()
        fd = os.open(wd / "runner.lock", os.O_RDWR | os.O_CREAT, 0o644)
        self.addCleanup(os.close, fd)
        fcntl.flock(fd, fcntl.LOCK_EX)
        self.assertEqual(dsp._owner_state(wd), "running")

    def test_launch_intent_recorded_before_codex_starts(self):
        self.scenario("ready")
        os.environ["LEADHAND_CRASH_AT"] = "before_worker_launch"
        with self.assertRaises(DispatchError):
            dispatch(self.request(), wait=20)
        runner = self.runner_json()
        self.assertIsNone(runner["codex"])
        self.assertTrue(runner.get("launching"))
        self.assertEqual(self.records("start"), [])

    def test_launch_rechecks_cancel_immediately_before_starting(self):
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        record = {"runner": {"pid": os.getpid(), "pgid": os.getpgid(0), "start": None}, "codex": None,
                  "generation": 1}
        proc = dsp._launch(wd, record, [str(FAKE)], dict(os.environ), self.repo, cancelled=lambda: True)
        self.assertIsNone(proc)
        self.assertEqual(self.records("start"), [])
        saved = json.loads((wd / "runner.json").read_text())
        self.assertTrue(saved.get("launching"))
        self.assertIsNone(saved["codex"])

    def test_cancel_requested_during_orphan_is_not_resurrected(self):
        self.scenario("slow")
        req = self.request()
        codex = self.start_detached(req)
        runner_pid = self.runner_json()["runner"]["pid"]
        os.kill(runner_pid, signal.SIGKILL)
        wait_for(lambda: not alive(runner_pid))
        # stop-dispatch has made its request (step 1) and is still stopping Codex.
        (self.worker_dir() / "cancel").write_text(json.dumps({"requested_at": "earlier"}))
        result = dispatch(req, wait=20)
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "cancelled"))
        self.assertFalse(alive(codex["pid"]))
        self.assertEqual(len(self.records("start")), 1)
        self.assertFalse((self.worker_dir() / "superseded").exists())

    def test_ready_result_landing_during_replacement_cancel_is_kept(self):
        self.scenario("ready", delay=1)
        req = self.request()
        codex = self.start_detached(req)
        runner_pid = self.stop_runner()
        wait_for(lambda: [r for r in self.records("exit") if r["pid"] == codex["pid"]])
        self.age_heartbeat()
        wd = self.worker_dir()

        def resume_when_cancel_requested():
            with contextlib.suppress(AssertionError):
                wait_for(lambda: (wd / "cancel").exists(), timeout=60, step=0.01)
            self.signal_quietly(runner_pid, signal.SIGCONT)

        t = threading.Thread(target=resume_when_cancel_requested, daemon=True)
        t.start()
        result = dispatch(req, wait=60)
        t.join(70)
        self.assertEqual((result.status, result.exit_reason), ("READY", "completed"))
        self.assertEqual(len(self.records("start")), 1)
        self.assertFalse((wd / "superseded").exists())

    def test_heartbeat_refreshed_before_second_look_is_not_hung(self):
        self.scenario("slow")
        req = self.request()
        self.start_detached(req)
        runner_pid = self.stop_runner()
        self.age_heartbeat()
        timer = threading.Timer(0.5, self.signal_quietly, (runner_pid, signal.SIGCONT))
        timer.start()
        self.addCleanup(timer.cancel)
        with self.assertRaises(DispatchStillRunning):
            dispatch(req, wait=8)
        self.assertFalse((self.worker_dir() / "cancel").exists())
        self.assertEqual(self.runner_json()["runner"]["pid"], runner_pid)
        self.assertTrue(alive(runner_pid))
        self.assertEqual(len(self.records("start")), 1)

    def test_concurrent_dispatches_start_one_runner(self):
        self.scenario("ready", delay=2)
        req = self.request()
        env = dict(os.environ, PATH=f"{FAKES}{os.pathsep}{os.environ['PATH']}")
        procs_ = [subprocess.Popen(self.cli_args(req), env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True) for _ in range(2)]
        outs = [p.communicate(timeout=60) for p in procs_]
        for p, (out, err) in zip(procs_, outs):
            self.assertEqual(p.returncode, 0, err)
            self.assertEqual(out.splitlines()[0], "Status: READY")
        self.assertEqual(len(self.records("start")), 1)
        self.assertEqual(len(read_attempts(run_paths(self.run_dir))), 1)
        self.assertFalse((self.worker_dir() / "superseded").exists())

    def test_second_runner_refuses_while_first_holds_lock(self):
        self.scenario("slow")
        req = self.request()
        self.start_detached(req)
        before = (self.worker_dir() / "runner.json").read_text()
        r = subprocess.run([sys.executable, str(CLI), "dispatch-runner", str(self.worker_dir())],
                           capture_output=True, text=True, timeout=30)
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual((self.worker_dir() / "runner.json").read_text(), before)
        self.assertEqual(len(self.records("start")), 1)


if __name__ == "__main__":
    unittest.main()
