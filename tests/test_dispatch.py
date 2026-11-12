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

from gitfixture import hermetic, make_repo, run_git
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
        # The run the dispatch commands are anchored to (security review H2): run_id is the
        # folder's name, as before run.json existed here.
        (self.run_dir / "run.json").write_text(json.dumps(
            {"run_id": self.run_dir.name, "repo": str(self.repo), "workspace": str(self.repo)}))
        self.log = self.tmp / "codex log.jsonl"
        self.codex_home = self.tmp / "codex home"
        env = mock.patch.dict(os.environ, {
            "FAKE_CODEX_LOG": str(self.log), "CODEX_HOME": str(self.codex_home),
            "LEADHAND_TESTING": "1", "LEADHAND_CONFIG": str(self.tmp / "no user config.toml"),
            "LEADHAND_TEST_CODEX_BIN": str(FAKE),       # R49: the fake Codex comes from the test environment
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
                      writable_dirs=(), resume=None)
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

    def test_evidence_worker_may_write_the_evidence_base_worktree_only(self):
        # The run is in place (workspace == repo), so its base worktree is named after the run dir.
        base = self.repo / ".worktrees" / f"{self.run_dir.name}--base"
        run_git(self.repo, "worktree", "add", "--quiet", "--detach", str(base), "HEAD")
        self.scenario("ready")
        dispatch(self.request("e1", role="verifier", card=None, kind="evidence"))
        dispatch(self.request("v1", role="verifier", card=None, kind="verify"))
        evidence, verify = (r["argv"] for r in self.records("start"))
        dirs = lambda argv: [argv[i + 1] for i, a in enumerate(argv) if a == "--add-dir"]
        self.assertIn(os.path.realpath(base), dirs(evidence))
        self.assertEqual(dirs(verify), [d for d in dirs(evidence) if d != os.path.realpath(base)])
        self.assertNotIn(os.path.realpath(base), [os.path.realpath(d) for d in dirs(verify)])

    def test_evidence_base_worktree_must_be_registered_and_reached_through_no_symlink(self):
        base = self.repo / ".worktrees" / f"{self.run_dir.name}--base"
        other = self.tmp / "other worktree"
        run_git(self.repo, "worktree", "add", "--quiet", "--detach", str(other), "HEAD")
        base.parent.mkdir()
        self.scenario("ready")
        for n, make in enumerate((lambda: base.symlink_to(other, target_is_directory=True),
                                  lambda: (base.unlink(), base.mkdir()))):
            make()
            dispatch(self.request(f"e{n}", role="verifier", card=None, kind="evidence"))
            stderr = (self.worker_dir(f"e{n}") / dsp.STDERR).read_text()
            self.assertIn("cannot prepare the Codex worker: refused", stderr)
        self.assertEqual(self.records("start"), [])         # Codex never started

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
        os.environ["LEADHAND_TEST_CODEX_BIN"] = str(self.tmp / "no such codex")
        result = dispatch(self.request())
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


class EscapedDescendantTests(Base):
    """Codex review round 1 (M4): a process that leaves Codex's tree (double fork, new session)
    just before Codex exits is found by the dispatch tag in its environment and stopped
    before the result is written."""

    def test_double_forked_child_is_stopped_before_finalizing(self):
        self.scenario("ready-escaped-writer")
        req = self.request()
        result = dispatch(req)
        (rec,) = self.records("escaped")
        pid = rec["escaped_pid"]
        self.addCleanup(self.signal_quietly, pid, signal.SIGKILL)
        self.assertEqual(result.status, "READY")
        self.assertFalse(alive(pid), "the escaped writer outlived the dispatch")
        log = self.repo / "escaped.log"
        size = log.stat().st_size
        time.sleep(0.5)
        self.assertEqual(log.stat().st_size, size, "the escaped writer wrote after finalize")
        tag = self.runner_json()["tag"]
        self.assertEqual(tag, f"{self.run_dir.name}:{req.dispatch_id}:1")
        start = self.records("start")[0]
        self.assertEqual(start["env"]["LEADHAND_DISPATCH_TAG"], tag)
        self.assertEqual(start["policy_set"]["LEADHAND_DISPATCH_TAG"], tag)
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertIn(pid, [p["pid"] for p in saved["stopped"]])


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


CARD_BRANCH = "leadhand/demo--card-1"


class RunnerCommitTests(Base):
    """Ruling R39: Codex cannot write the Git directory, so the runner commits its work."""

    def setUp(self):
        super().setUp()
        run_git(self.repo, "checkout", "--quiet", "-b", CARD_BRANCH)
        self.base_sha = run_git(self.repo, "rev-parse", "HEAD")
        exclude = Path(run_git(self.repo, "rev-parse", "--path-format=absolute", "--git-common-dir")) / "info" / "exclude"
        exclude.parent.mkdir(parents=True, exist_ok=True)
        exclude.write_text(".leadhand/\n")
        # The runner's commit must be unsigned and skip hooks, whatever the repo says.
        run_git(self.repo, "config", "commit.gpgsign", "true")
        run_git(self.repo, "config", "gpg.program", "false")
        hook = Path(run_git(self.repo, "rev-parse", "--path-format=absolute", "--git-path", "hooks")) / "pre-commit"
        hook.parent.mkdir(parents=True, exist_ok=True)
        hook.write_text("#!/bin/sh\nexit 1\n")
        hook.chmod(0o755)

    def commit_request(self, dispatch_id="d1", **kw):
        req = self.request(dispatch_id, kind="implement", branch=CARD_BRANCH, **kw)
        req.brief.write_text("# Brief\n## Card 1: Widget card\n- Card branch: leadhand/demo--card-1\n")
        return req

    def write_scenario(self, lines):
        path = self.tmp / "scenario.jsonl"
        path.write_text("".join(json.dumps(x) + "\n" for x in lines))
        os.environ["FAKE_CODEX_SCENARIO"] = str(path)

    def edits(self, req, *, report=True, steps=0, status="READY"):
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"},
                 {"action": "write_file", "path": "notes/new file.txt", "text": "untracked\n"}]
        if report:
            lines.append({"action": "write_file", "path": str(req.report),
                          "text": f"Status: {status}\n## Summary\nAdded the widget constant.\nMore.\n"})
        for i in range(steps):
            lines += [{"type": "item.completed", "item": {"id": f"c{i}", "type": "command_execution",
                                                          "status": "completed"}},
                      {"action": "sleep", "seconds": 0.3}]
        if steps:
            lines.append({"action": "sleep", "seconds": 30})
        lines += [{"action": "last_message", "text": f"Status: {status}\n"},
                  {"type": "turn.completed", "usage": {"input_tokens": 10, "cached_input_tokens": 0,
                                                        "output_tokens": 1}}]
        self.write_scenario(lines)

    def head_files(self):
        return run_git(self.repo, "show", "--name-only", "--format=", "HEAD").splitlines()

    def test_runner_commits_codex_work_on_card_branch(self):
        req = self.commit_request()
        self.edits(req)
        result = dispatch(req)
        self.assertEqual(result.status, "READY")
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        head = run_git(self.repo, "rev-parse", "HEAD")
        self.assertEqual(saved["committed"], head)
        self.assertEqual(run_git(self.repo, "rev-parse", f"refs/heads/{CARD_BRANCH}"), head)
        self.assertEqual(run_git(self.repo, "rev-parse", "HEAD^"), self.base_sha)
        self.assertEqual(run_git(self.repo, "log", "-1", "--format=%s"), "implement card 1: Added the widget constant.")
        self.assertIn("Leadhand-Dispatch: d1", run_git(self.repo, "log", "-1", "--format=%B"))
        self.assertNotIn("gpgsig", run_git(self.repo, "cat-file", "commit", "HEAD"))
        self.assertEqual(sorted(self.head_files()), ["notes/new file.txt", "src/widget.py"])
        self.assertEqual(run_git(self.repo, "status", "--porcelain"), "")
        self.assertTrue(req.report.exists())         # the scratch folder stays, untracked and ignored

    def test_runner_commits_partial_work_when_spent(self):
        req = self.commit_request(steps=2)
        self.edits(req, report=False, steps=5)
        result = dispatch(req)
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "step_limit"))
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertEqual(saved["committed"], run_git(self.repo, "rev-parse", "HEAD"))
        self.assertEqual(run_git(self.repo, "log", "-1", "--format=%s"), "implement card 1: Widget card")
        self.assertIn("src/widget.py", self.head_files())

    def test_commit_intent_restart_does_not_double_commit(self):
        req = self.commit_request()
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        (self.repo / "src").mkdir()
        (self.repo / "src" / "widget.py").write_text("WIDGET = 1\n")
        first = dsp._commit_work(wd, req)
        sha = run_git(self.repo, "rev-parse", "HEAD")
        self.assertEqual(first["committed"], sha)
        self.assertTrue((wd / "commit.json").exists())
        # A restart after the commit (intent there, HEAD carries the trailer): nothing new.
        again = dsp._commit_work(wd, req)
        self.assertEqual(again["committed"], sha)
        self.assertEqual(run_git(self.repo, "rev-parse", "HEAD"), sha)
        self.assertEqual(run_git(self.repo, "rev-list", "--count", f"{self.base_sha}..HEAD"), "1")
        # A restart after the intent but before the commit: it commits.
        run_git(self.repo, "reset", "--quiet", "--soft", self.base_sha)
        late = dsp._commit_work(wd, req)
        self.assertNotEqual(late["committed"], self.base_sha)
        self.assertEqual(run_git(self.repo, "rev-list", "--count", f"{self.base_sha}..HEAD"), "1")

    def test_runner_commits_a_new_file_in_a_subdirectory_workspace(self):
        # An in-place run started with --repo <checkout>/src: Git lists untracked files from
        # the checkout's top ("src/new.py"), and the workspace is src.
        (self.repo / "src").mkdir()
        (self.repo / "src" / "a.py").write_text("A = 1\n")
        run_git(self.repo, "add", "src/a.py")
        run_git(self.repo, "commit", "--quiet", "--no-verify", "-m", "src")     # the fixture's hook refuses
        self.base_sha = run_git(self.repo, "rev-parse", "HEAD")
        sub = self.repo / "src"
        (self.run_dir / "run.json").write_text(json.dumps(
            {"run_id": self.run_dir.name, "repo": str(sub), "workspace": str(sub)}))
        folder = sub / ".leadhand" / "1" / "d1"
        folder.mkdir(parents=True)
        (folder / "brief.md").write_text("# Brief\n## Card 1: Widget card\n")
        req = self.request(cwd=sub, brief=folder / "brief.md", report=folder / "report.md",
                           kind="implement", branch=CARD_BRANCH)
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        (sub / "new.py").write_text("NEW = 1\n")
        (sub / "a.py").write_text("A = 2\n")
        out = dsp._commit_work(wd, req)
        self.assertEqual(out.get("committed"), run_git(self.repo, "rev-parse", "HEAD"), out)
        self.assertEqual(sorted(self.head_files()), ["src/a.py", "src/new.py"])

    def test_untracked_to_take_fences_the_run_folder_from_the_checkout_top(self):
        # Untracked paths are named from the checkout's top, so the fence is too, even when the
        # workspace is a subdirectory and the docs folder lies beside it in the checkout.
        sub = self.repo / "src"
        sub.mkdir()
        run_dir = self.repo / "notes" / "runs" / "2026-11-10-demo run"
        run_dir.mkdir(parents=True)
        paths = ["notes/runs/2026-11-10-demo run/run.json", "notes/runs.md", "src/new.py"]
        self.assertEqual(dsp.untracked_to_take(run_dir, sub, paths), ["src/new.py"])

    def test_failed_commit_is_blocked_environment_with_reason(self):
        for name in ("EMAIL", "GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"):
            os.environ.pop(name, None)
        run_git(self.repo, "config", "--unset", "user.name")
        run_git(self.repo, "config", "--unset", "user.email")
        run_git(self.repo, "config", "user.useConfigOnly", "true")
        req = self.commit_request()
        self.edits(req)
        result = dispatch(req)
        self.assertEqual((result.status, result.blocked_by, result.exit_reason), ("BLOCKED", "environment", "error"))
        self.assertIn("could not commit", result.reply)
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertIsNone(saved["committed"])
        self.assertTrue(saved["commit_error"])
        self.assertIn(saved["commit_error"].splitlines()[0][:40], result.reply)
        self.assertEqual(run_git(self.repo, "rev-parse", "HEAD"), self.base_sha)
        self.assertIn("src/", run_git(self.repo, "status", "--porcelain"))
        self.assertEqual(read_attempts(run_paths(self.run_dir))[0]["status"], "BLOCKED")

    def test_unlocatable_git_path_is_an_error_not_an_operation(self):
        req = self.commit_request()
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        (self.repo / "src").mkdir()
        (self.repo / "src" / "widget.py").write_text("WIDGET = 1\n")
        real = dsp._git

        def failing(ws, *args, **kw):
            if "--git-path" in args:
                return subprocess.CompletedProcess(["git", *args], 128, "", "fatal: broken repository\n")
            return real(ws, *args, **kw)

        with mock.patch.object(dsp, "_git", failing):
            out = dsp._commit_work(wd, req)
        self.assertIsNone(out["committed"])
        self.assertNotIn("in progress", out.get("commit_skipped") or "")
        self.assertIn("broken repository", out["commit_error"])

    def test_commit_respects_the_cancel_deadline(self):
        req = self.commit_request()
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        (self.repo / "src").mkdir()
        (self.repo / "src" / "widget.py").write_text("WIDGET = 1\n")
        out = dsp._commit_work(wd, req, deadline=time.monotonic() - 1)
        self.assertIsNone(out["committed"])
        self.assertIn("deadline", out["commit_error"])
        self.assertEqual(run_git(self.repo, "rev-parse", "HEAD"), self.base_sha)

    def test_externally_killed_codex_is_possibly_incomplete_and_partial_work_committed(self):
        req = self.commit_request()
        self.write_scenario([{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                             {"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"},
                             {"type": "item.completed", "item": {"id": "c1", "type": "command_execution",
                                                                 "status": "completed"}},
                             {"action": "sleep", "seconds": 60}])
        codex = self.start_detached(req)
        wait_for(lambda: (self.repo / "src" / "widget.py").exists())
        os.kill(codex["pid"], signal.SIGKILL)          # from outside, like Task 22
        result = dispatch(req, wait=30)
        self.assertEqual((result.status, result.exit_reason), ("SPENT", "interrupted"))
        [c] = [json.loads(l) for l in (self.run_dir / "charges.jsonl").read_text().splitlines()]
        self.assertTrue(c.get("possibly_incomplete"))
        [attempt] = read_attempts(run_paths(self.run_dir))
        self.assertTrue(attempt["possibly_incomplete"])
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertTrue(saved["possibly_incomplete"])
        self.assertEqual(saved["committed"], run_git(self.repo, "rev-parse", "HEAD"))
        self.assertIn("src/widget.py", self.head_files())

    def test_exit_without_final_turn_completed_is_possibly_incomplete(self):
        req = self.commit_request()
        self.write_scenario([{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                             {"type": "turn.completed", "usage": {"input_tokens": 5, "cached_input_tokens": 0,
                                                                  "output_tokens": 1}},
                             {"type": "turn.started"},
                             {"action": "last_message", "text": "Status: READY\n"}])
        result = dispatch(req)
        self.assertEqual(result.status, "READY")
        [c] = [json.loads(l) for l in (self.run_dir / "charges.jsonl").read_text().splitlines()]
        self.assertTrue(c.get("possibly_incomplete"))
        self.assertTrue(read_attempts(run_paths(self.run_dir))[0]["possibly_incomplete"])

    def test_complete_run_is_not_possibly_incomplete(self):
        req = self.commit_request()
        self.edits(req)
        dispatch(req)
        [c] = [json.loads(l) for l in (self.run_dir / "charges.jsonl").read_text().splitlines()]
        self.assertFalse(c.get("possibly_incomplete"))
        self.assertFalse(read_attempts(run_paths(self.run_dir))[0]["possibly_incomplete"])

    def test_wrong_branch_is_not_committed(self):
        req = self.commit_request()
        run_git(self.repo, "checkout", "--quiet", "-b", "somewhere-else")
        self.edits(req)
        result = dispatch(req)
        self.assertEqual((result.status, result.blocked_by), ("BLOCKED", "environment"))
        saved = json.loads((self.worker_dir() / "result.json").read_text())
        self.assertIsNone(saved["committed"])
        self.assertIn("somewhere-else", saved["commit_skipped"])
        self.assertEqual(run_git(self.repo, "rev-parse", "HEAD"), self.base_sha)
        self.assertIn("src/", run_git(self.repo, "status", "--porcelain"))

    def test_operation_in_progress_is_not_committed(self):
        req = self.commit_request()
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        (self.repo / "src").mkdir()
        (self.repo / "src" / "widget.py").write_text("WIDGET = 1\n")
        merge_head = Path(run_git(self.repo, "rev-parse", "--path-format=absolute", "--git-path", "MERGE_HEAD"))
        merge_head.write_text(self.base_sha + "\n")
        out = dsp._commit_work(wd, req)
        self.assertIsNone(out["committed"])
        self.assertIn("in progress", out["commit_skipped"])
        self.assertEqual(run_git(self.repo, "rev-parse", "HEAD"), self.base_sha)

    def test_check_dispatch_is_not_committed(self):
        req = self.request("r1", kind="review", branch=CARD_BRANCH)
        self.edits(req)
        dispatch(req)
        saved = json.loads((self.worker_dir("r1") / "result.json").read_text())
        self.assertIsNone(saved["committed"])
        self.assertEqual(run_git(self.repo, "rev-parse", "HEAD"), self.base_sha)

    def test_cli_passes_kind_and_branch(self):
        req = self.commit_request()
        args = ["dispatch", "--run", str(self.run_dir), "--dispatch-id", "d1", "--card", "1", "--attempt", "1",
                "--role", "implementer", "--model", "gpt-6-luna", "--effort", "low", "--brief", str(req.brief),
                "--report", str(req.report), "--cwd", str(self.repo), "--kind", "implement",
                "--branch", CARD_BRANCH]
        self.edits(req)
        with mock.patch.dict(os.environ, {"PATH": f"{FAKES}{os.pathsep}{os.environ['PATH']}"}), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cli.main(args), 0)
        request = json.loads((self.worker_dir() / "request.json").read_text())
        self.assertEqual((request["kind"], request["branch"]), ("implement", CARD_BRANCH))
        self.assertEqual(json.loads((self.worker_dir() / "result.json").read_text())["committed"],
                         run_git(self.repo, "rev-parse", "HEAD"))


if __name__ == "__main__":
    unittest.main()
