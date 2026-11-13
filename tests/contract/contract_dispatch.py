"""Dispatcher scenarios: `dispatch`, its detached runner, `stop-dispatch` and the worker folder.

Converted from tests/test_dispatch.py. Every scenario runs on a run that `start` made (one card,
Claude and Codex installed, so the workspace is a worktree and config.json is the run's
snapshot), with tests/fakes/codex as Codex (LEADHAND_TEST_CODEX_BIN). It calls only
`dispatch ...`, `stop-dispatch RUN_DIR --dispatch ID` and `dispatch-runner WORKER_DIR`, and reads
what they leave: stdout, stderr and exit codes, `workers/<dispatch id>/`, charges.jsonl,
attempts.jsonl, the fake's log (its argv, environment, prompt and processes) and Git.

Where a Python test called `dispatch(req, wait=0.2)` to leave a runner going, `start_detached`
runs the `dispatch` line in the background until its runner has started Codex, then kills that
foreground, as a proxy that has given up waiting is gone. Waits poll for a condition with a
bound; a check that something does not happen watches for it over a stated window (`holds`).
Processes a test starts are identity-checked (harness.same_process) before any signal, and the
world's cleanup kills every process a worker folder records (supervisor.recorded_processes).
"""
import contextlib
import fcntl
import json
import os
import signal
import subprocess
import threading
import time
import unittest
from pathlib import Path

import harness
from gitfixture import run_git
from harness import BIN, FAKES, ROOT, read_json, read_jsonl, run_cli, same_process
from rundir import read_records
from supervisor import FakeSupervisor, card

CODEX = FAKES / "codex"
SCENARIOS = ROOT / "tests" / "fixtures" / "scenarios"
DEFAULT_THREAD = "0f0f0f0f-0000-4000-8000-00000000c0de"     # the fake's thread ID
CARD_BRANCH = "leadhand/demo--card-1"
CARD_BRIEF = "# Brief\n## Card 1: Widget card\n- Card branch: leadhand/demo--card-1\n"
ATTEMPT_FIELDS = {"dispatch", "run", "card", "attempt", "role", "vendor", "model", "effort", "worker_id",
                  "started_at", "ended_at", "steps", "charge_ids", "usage", "usage_known",
                  "cost_estimate", "exit_reason", "status", "blocked_by"}
# What a Codex implementer's prompt adds to the implementer agent's body (ruling R39).
CODEX_NO_COMMIT = (
    "## Commits (Codex workers)\n\n"
    "This overrides the contract's rule to commit on the card branch. Do not run git commit,\n"
    "git add, or any command that writes under .git; leave your changes in the working tree\n"
    "and list the changed files in your report. leadhand commits them on your branch after\n"
    "you finish.")


def wait_for(cond, what: str, timeout: float = 30.0, proc: subprocess.Popen | None = None):
    """Poll `cond` until it returns something true, and return that. Fails after `timeout`
    seconds, or at once when `proc` (a command the test is waiting on) has exited."""
    deadline = time.monotonic() + timeout
    while True:
        value = cond()
        if value:
            return value
        if proc is not None and proc.poll() is not None:
            out, err = proc.communicate()
            raise AssertionError(f"the command exited ({proc.returncode}) while waiting for {what}\n"
                                 f"argv: {proc.args}\nstdout:\n{out}\nstderr:\n{err}")
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out after {timeout:g} s waiting for {what}")
        time.sleep(0.02)


def holds(cond, seconds: float, what: str) -> None:
    """Check that `cond` stays true for a window of `seconds` (something must not happen)."""
    end = time.monotonic() + seconds
    while True:
        if not cond():
            raise AssertionError(f"{what}: no longer true within the {seconds:g} s window")
        if time.monotonic() > end:
            return
        time.sleep(0.02)


def alive(rec: dict) -> bool:
    return same_process(rec.get("pid"), rec.get("start"))


def logged_alive(rec: dict) -> bool:
    """Whether the process a fake Codex log record names (`child_pid` or `escaped_pid`) is still
    alive: judged from the log line's own time, never from anything the binary wrote."""
    return harness.started_by(rec.get("child_pid") or rec.get("escaped_pid"), rec["time"]) is not None


def implementer_prompt(brief: Path, report: Path) -> str:
    """The prompt a Codex implementer gets: the implementer agent's body without its front
    matter, the no-commit rule, then its files."""
    text = (ROOT / "agents" / "implementer.md").read_text(encoding="utf-8")
    if text.startswith("---\n"):
        _, sep, body = text[4:].partition("\n---\n")
        if sep:
            text = body
    return (f"{text.strip()}\n\n{CODEX_NO_COMMIT}\n\n## Your files\n\n- Brief: {brief}\n- Report file: {report}\n\n"
            "Read the brief now, do the work, write the report, then reply.\n")


class World:
    """A run `start` made, and the dispatch commands on it.

    `env` is every command's environment: the supervisor's sealed one, the fake Codex, and
    credentials Codex must never see. `scenario` sets the fake's script and knobs for the
    commands that follow, as the Python tests' `scenario` set os.environ."""

    def __init__(self, test: unittest.TestCase):
        self.test = test
        self.sup = FakeSupervisor(test, [card(1)], installed=("claude", "codex"))
        self.run_dir = self.sup.start()
        meta = self.sup.meta()
        self.run_id = meta["run_id"]
        self.ws = Path(meta["workspace"])
        self.repo = self.sup.repo
        self.tmp = self.sup.tmp
        self.log = Path(self.sup.env["FAKE_CODEX_LOG"])
        self.env = dict(self.sup.env, LEADHAND_TEST_CODEX_BIN=str(CODEX),   # R49: Codex from the test env
                        GH_TOKEN="parent-gh-token", GITHUB_TOKEN="parent-github-token",
                        SSH_AUTH_SOCK="/nonexistent/agent.sock")
        self._scenarios = 0

    # -- setup ----------------------------------------------------------------

    def scenario(self, script, **knobs) -> None:
        """The fake's scenario: a name in tests/fixtures/scenarios, or a list of steps; `knobs`
        are its FAKE_CODEX_* settings (thread, delay, no_rollout, orphan_child, ...)."""
        for name in [k for k in self.env if k.startswith("FAKE_CODEX_") and k != "FAKE_CODEX_LOG"]:
            del self.env[name]
        if isinstance(script, str):
            path = SCENARIOS / f"{script}.jsonl"
        else:
            self._scenarios += 1
            path = self.tmp / f"scenario {self._scenarios}.jsonl"
            path.write_text("".join(json.dumps(x) + "\n" for x in script), encoding="utf-8")
        self.env["FAKE_CODEX_SCENARIO"] = str(path)
        for k, v in knobs.items():
            self.env[f"FAKE_CODEX_{k.upper()}"] = str(v)

    def set_config(self, change) -> None:
        """Edit the run's configuration snapshot (config.json), which `dispatch` reads."""
        path = self.run_dir / "config.json"
        data = read_json(path)
        change(data)
        path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")

    def set_limits(self, role: str, **limits) -> None:
        self.set_config(lambda c: c["limits"][role].update(limits))

    def scratch(self, did: str, card_number: int | None = 1, brief_text: str | None = None) -> tuple[Path, Path]:
        """The dispatch's brief and report in its scratch folder, <ws>/.leadhand/<card or run>/<id>/;
        writes the brief (`brief_text`, by default "# Brief") unless the test already has."""
        folder = self.ws / ".leadhand" / ("run" if card_number is None else str(card_number)) / did
        folder.mkdir(parents=True, exist_ok=True)
        brief = folder / "brief.md"
        if brief_text is not None or not brief.exists():
            brief.write_text(brief_text or "# Brief\n", encoding="utf-8")
        return brief, folder / "report.md"

    def args(self, did="d1", *, card=1, attempt=1, role="implementer", model="gpt-6-luna", effort="low",
             resume=None, kind=None, branch=None) -> list:
        brief, report = self.scratch(did, card)
        argv = ["dispatch", "--run", self.run_dir, "--dispatch-id", did,
                *(["--card", card] if card is not None else []), "--attempt", attempt, "--role", role,
                "--model", model, "--effort", effort, "--brief", brief, "--report", report, "--cwd", self.ws]
        for flag, value in (("--resume", resume), ("--kind", kind), ("--branch", branch)):
            if value is not None:
                argv += [flag, value]
        return argv

    def request_json(self, did: str) -> dict:
        """request.json as `dispatch` writes it, for a worker folder a test sets up by hand."""
        brief, report = self.scratch(did)
        return {"model": "gpt-6-luna", "effort": "low", "role": "implementer", "brief": str(brief),
                "report": str(report), "cwd": str(self.ws), "run_dir": str(self.run_dir), "card": 1,
                "attempt": 1, "dispatch_id": did, "steps": 50, "minutes": 5, "network": False,
                "writable_dirs": [], "resume": None, "kind": None, "branch": None}

    def decoy(self) -> subprocess.Popen:
        """An unrelated process of the test's own, which nothing may signal."""
        p = subprocess.Popen(["sleep", "60"], start_new_session=True, stdin=subprocess.DEVNULL)
        self.test.addCleanup(lambda: (p.kill(), p.wait()))
        return p

    # -- commands -------------------------------------------------------------

    def dispatch(self, did="d1", *, timeout: float = 120.0, **request) -> harness.Result:
        return run_cli(*self.args(did, **request), env=self.env, cwd=self.tmp, timeout=timeout)

    def popen(self, did="d1", **request) -> subprocess.Popen:
        """The `dispatch` line in the background, in its own process group; cleanup kills it."""
        p = subprocess.Popen([str(BIN), *(str(a) for a in self.args(did, **request))], cwd=self.tmp,
                             env=dict(self.env), stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, process_group=0)
        self.sup.proxies.append(p)
        return p

    def start_detached(self, did="d1", **request) -> dict:
        """Start the dispatch, wait until its runner has started Codex, then kill the foreground.
        Returns runner.json's Codex record."""
        p = self.popen(did, **request)
        codex = wait_for(lambda: (self.runner_json(did) or {}).get("codex"), f"Codex started for {did}", proc=p)
        p.kill()
        p.communicate(timeout=10)
        return codex

    def stop(self, did="d1") -> harness.Result:
        return run_cli("stop-dispatch", self.run_dir, "--dispatch", did, env=self.env, cwd=self.tmp)

    @staticmethod
    def signal(rec: dict, sig: int) -> None:
        """Signal a recorded process, only while it is still the process recorded."""
        if alive(rec):
            with contextlib.suppress(ProcessLookupError):
                os.kill(rec["pid"], sig)

    # -- reading --------------------------------------------------------------

    def wd(self, did="d1") -> Path:
        return self.run_dir / "workers" / did

    def read(self, did: str, name: str) -> dict | None:
        path = self.wd(did) / name
        return read_json(path) if path.exists() else None

    def runner_json(self, did="d1") -> dict | None:
        return self.read(did, "runner.json")

    def result(self, did="d1") -> dict:
        return read_json(self.wd(did) / "result.json")

    def stopped(self, did="d1") -> dict[int, dict]:
        """result.json's `stopped` processes, by pid."""
        return {r["pid"]: r for r in self.result(did)["stopped"]}

    def records(self, event=None) -> list[dict]:
        records = read_jsonl(self.log) if self.log.exists() else []
        return [r for r in records if event is None or r.get("event") == event]

    def attempts(self) -> list[dict]:
        return read_records(self.run_dir / "attempts.jsonl")

    def charges(self) -> list[dict]:
        return read_records(self.run_dir / "charges.jsonl")

    def age_heartbeat(self, did="d1") -> None:
        old = time.time() - 120
        os.utime(self.wd(did) / "heartbeat", (old, old))

    def git(self, *args) -> str:
        return run_git(self.ws, *args)


class CodexCommandLine(unittest.TestCase):
    """The `codex exec` line the runner starts (spec contract item 10), as the fake logs it
    (its argv after the program), with its environment and prompt."""

    def argv_of(self, w: World, did="d1", **request) -> list[str]:
        w.dispatch(did, **request).ok()
        return w.records("start")[-1]["argv"]

    def test_argv_passes_network_false_explicitly(self):
        w = World(self)
        w.scenario("ready")
        argv = self.argv_of(w)
        i = argv.index("sandbox_workspace_write.network_access=false")
        self.assertEqual(argv[i - 1], "-c")
        self.assertNotIn("sandbox_workspace_write.network_access=true", argv)

    def test_argv_network_true_when_enabled(self):
        w = World(self)
        w.set_config(lambda c: c["codex"].update(network=True))
        w.scenario("ready")
        argv = self.argv_of(w)
        self.assertIn("sandbox_workspace_write.network_access=true", argv)
        self.assertNotIn("sandbox_workspace_write.network_access=false", argv)

    def test_argv_workspace_write_with_git_common_dir_and_effort(self):
        w = World(self)
        extra = w.tmp / "extra dir"
        w.set_config(lambda c: c["codex"].update(writable_dirs=[str(extra)]))
        w.scenario("ready")
        argv = self.argv_of(w, effort="high")
        common = w.git("rev-parse", "--path-format=absolute", "--git-common-dir")
        # The program itself (argv[0]) is LEADHAND_TEST_CODEX_BIN: the fake is what ran.
        self.assertEqual(argv[:17], [
            "exec", "-m", "gpt-6-luna",
            "-c", 'model_reasoning_effort="high"', "-c", 'approval_policy="never"',
            "-c", "sandbox_workspace_write.network_access=false",
            "-C", str(w.ws), "-s", "workspace-write",
            "--add-dir", common, "--add-dir", str(extra)])
        self.assertEqual(argv[-4:], ["--json", "-o", str(w.wd() / "last-message.md"), "-"])
        self.assertFalse([a for a in argv if "compact" in a])

    def test_argv_scout_is_read_only(self):
        w = World(self)
        w.scenario("ready")
        argv = self.argv_of(w, role="scout")
        self.assertEqual(argv[argv.index("-s") + 1], "read-only")
        self.assertNotIn("workspace-write", argv)

    def test_argv_sets_blocking_env_through_shell_policy(self):
        w = World(self)
        w.scenario("ready")
        w.env.update(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="user.name", GIT_CONFIG_VALUE_0="Some One")
        w.dispatch().ok()
        start = w.records("start")[0]
        argv, policy = start["argv"], start["policy_set"]
        self.assertEqual(policy["GIT_SSH_COMMAND"], "false")
        self.assertEqual(policy["GIT_TERMINAL_PROMPT"], "0")
        self.assertEqual(policy["GH_CONFIG_DIR"], start["env"]["GH_CONFIG_DIR"])
        # The parent's GIT_CONFIG_* entries are kept in order, then credential.helper is reset.
        self.assertEqual(policy["GIT_CONFIG_COUNT"], "2")
        self.assertEqual(policy["GIT_CONFIG_KEY_0"], "user.name")
        count = int(policy["GIT_CONFIG_COUNT"])
        self.assertEqual((policy[f"GIT_CONFIG_KEY_{count - 1}"], policy[f"GIT_CONFIG_VALUE_{count - 1}"]),
                         ("credential.helper", ""))
        for name, value in policy.items():
            i = argv.index(f"shell_environment_policy.set.{name}={json.dumps(value)}")
            self.assertEqual(argv[i - 1], "-c")
        for name in ("GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"):
            self.assertNotIn(name, start["env"])
        self.assertEqual(os.listdir(start["env"]["GH_CONFIG_DIR"]), [])

    def test_run_id_passed_through_shell_policy(self):
        w = World(self)
        w.scenario("ready")
        w.dispatch().ok()
        start = w.records("start")[0]
        self.assertEqual(start["policy_set"]["LEADHAND_RUN_ID"], w.run_id)
        self.assertEqual(start["env"]["LEADHAND_RUN_ID"], w.run_id)

    def test_evidence_worker_may_write_the_evidence_base_worktree_only(self):
        w = World(self)
        # The workspace is a worktree in <repo>/.worktrees, so its base worktree is named after it.
        base = w.repo / ".worktrees" / f"{w.ws.name}--base"
        run_git(w.repo, "worktree", "add", "--quiet", "--detach", str(base), "HEAD")
        w.scenario("ready")
        w.dispatch("e1", role="verifier", card=None, kind="evidence").ok()
        w.dispatch("v1", role="verifier", card=None, kind="verify").ok()
        evidence, verify = (r["argv"] for r in w.records("start"))
        dirs = lambda argv: [argv[i + 1] for i, a in enumerate(argv) if a == "--add-dir"]
        self.assertIn(os.path.realpath(base), dirs(evidence))
        self.assertEqual(dirs(verify), [d for d in dirs(evidence) if d != os.path.realpath(base)])
        self.assertNotIn(os.path.realpath(base), [os.path.realpath(d) for d in dirs(verify)])

    def test_evidence_base_worktree_must_be_registered_and_reached_through_no_symlink(self):
        w = World(self)
        base = w.repo / ".worktrees" / f"{w.ws.name}--base"
        other = w.tmp / "other worktree"
        run_git(w.repo, "worktree", "add", "--quiet", "--detach", str(other), "HEAD")
        w.scenario("ready")
        for n, make in enumerate((lambda: base.symlink_to(other, target_is_directory=True),
                                  lambda: (base.unlink(), base.mkdir()))):
            make()
            w.dispatch(f"e{n}", role="verifier", card=None, kind="evidence").ok()
            stderr = (w.wd(f"e{n}") / "codex.stderr").read_text(encoding="utf-8")
            self.assertIn("cannot prepare the Codex worker: refused", stderr)
        self.assertEqual(w.records("start"), [])         # Codex never started

    def test_resume_argv_matches_findings(self):
        w = World(self)
        w.scenario("ready")
        w.dispatch("d1").ok()
        r = w.dispatch("d2", resume="thread-123").ok()
        fresh, resumed = (s["argv"] for s in w.records("start"))
        self.assertEqual(resumed[-3:], ["resume", "thread-123", "-"])
        # The same options, before `resume`: the fresh line with d2's worker folder and tag.
        same = [a.replace(str(w.wd("d1")), str(w.wd("d2"))).replace(f"{w.run_id}:d1:", f"{w.run_id}:d2:")
                for a in fresh]
        self.assertEqual(resumed[:-3], same[:-1])
        self.assertIn("-m", resumed[:resumed.index("resume")])
        start = w.records("start")[1]
        self.assertEqual(start["prompt"], implementer_prompt(*w.scratch("d2")))
        self.assertEqual(r.stdout.splitlines()[-1], "session: thread-123")


class Results(unittest.TestCase):
    """How a dispatch ends: result.json, the reply `dispatch` prints, the attempt record."""

    def test_ready_reply_parsed_and_attempt_recorded(self):
        w = World(self)
        thread = "11111111-2222-4333-8444-555555555555"
        w.scenario("ready", thread=thread)
        r = w.dispatch().ok()
        lines = r.stdout.splitlines()
        saved = w.result()
        self.assertEqual((saved["status"], saved["blocked_by"], saved["exit_reason"]), ("READY", "", "completed"))
        self.assertEqual(saved["session_id"], thread)
        self.assertEqual(lines[-1], f"session: {thread}")
        self.assertEqual(saved["steps"], 2)
        self.assertTrue(saved["reply"].startswith("Status: READY"))
        self.assertEqual(lines[0], "Status: READY")
        self.assertTrue(saved["usage_known"])
        self.assertEqual(saved["usage"]["output"], 50)
        start = w.records("start")[0]
        self.assertEqual(start["prompt"], implementer_prompt(*w.scratch("d1")))
        self.assertEqual(Path(start["cwd"]).resolve(), w.ws.resolve())
        self.assertEqual((w.ws / "done.txt").read_text(encoding="utf-8"), "done\n")
        [attempt] = w.attempts()
        self.assertTrue(ATTEMPT_FIELDS <= set(attempt), ATTEMPT_FIELDS - set(attempt))
        self.assertEqual((attempt["dispatch"], attempt["card"], attempt["attempt"], attempt["role"]),
                         ("d1", 1, 1, "implementer"))
        self.assertEqual((attempt["vendor"], attempt["model"], attempt["effort"]), ("codex", "gpt-6-luna", "low"))
        self.assertEqual(attempt["worker_id"], thread)
        self.assertEqual((attempt["status"], attempt["exit_reason"], attempt["steps"]), ("READY", "completed", 2))
        self.assertEqual(attempt["charge_ids"], [f"d1:codex:{thread}"])
        self.assertTrue(attempt["usage_known"])
        self.assertIsNone(attempt["cost_estimate"])  # Codex models are not priced
        self.assertEqual(attempt["run"], w.run_id)

    def test_reply_without_status_is_blocked_and_raw_kept(self):
        w = World(self)
        w.scenario("no-status")
        w.dispatch().ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["blocked_by"], saved["exit_reason"]), ("BLOCKED", "", "completed"))
        raw = (w.wd() / "reply.raw.md").read_text(encoding="utf-8")
        self.assertEqual(raw, "I looked around and stopped.\nNothing else to say.\n")
        self.assertEqual(w.attempts()[0]["status"], "BLOCKED")

    def test_step_limit_stops_process_and_marks_spent(self):
        w = World(self)
        w.set_limits("implementer", steps=3)
        w.scenario("many-steps")
        t0 = time.monotonic()
        w.dispatch().ok()
        self.assertLess(time.monotonic() - t0, 20)
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "step_limit"))
        self.assertGreaterEqual(saved["steps"], 3)
        self.assertLess(saved["steps"], 9)
        self.assertFalse(alive(w.runner_json()["codex"]))
        self.assertEqual(w.records("exit")[-1]["how"], "sigint")

    def test_time_limit_stops_process_and_marks_spent(self):
        w = World(self)
        w.set_limits("implementer", minutes=1)
        w.env["LEADHAND_MINUTE_SECONDS"] = "1"
        w.scenario("slow")
        t0 = time.monotonic()
        w.dispatch().ok()
        self.assertLess(time.monotonic() - t0, 20)
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "time_limit"))
        self.assertFalse(alive(w.runner_json()["codex"]))
        child = w.records("child")[0]
        self.assertIn(child["child_pid"], w.stopped())
        self.assertFalse(logged_alive(child))

    def test_usage_limit_is_blocked_by_usage_limit(self):
        w = World(self)
        w.scenario("usage-limit")
        w.dispatch().ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["blocked_by"], saved["exit_reason"]),
                         ("BLOCKED", "usage limit", "usage_limit"))
        self.assertEqual(w.attempts()[0]["blocked_by"], "usage limit")

    def test_usage_limit_text_does_not_override_clean_ready_reply(self):
        w = World(self)
        w.scenario("ready-limit-warning")
        w.dispatch().ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["blocked_by"], saved["exit_reason"]), ("READY", "", "completed"))

    def test_missing_codex_binary_is_blocked_environment(self):
        w = World(self)
        w.env["LEADHAND_TEST_CODEX_BIN"] = str(w.tmp / "no such codex")
        w.dispatch().ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["blocked_by"], saved["exit_reason"]),
                         ("BLOCKED", "environment", "error"))
        self.assertIn("cannot start", (w.wd() / "codex.stderr").read_text(encoding="utf-8"))

    def test_paths_with_spaces(self):
        w = World(self)
        extra = w.tmp / "extra dir"
        extra.mkdir()
        w.set_config(lambda c: c["codex"].update(writable_dirs=[str(extra)]))
        w.scenario("ready")
        w.dispatch().ok()
        self.assertEqual(w.result()["status"], "READY")
        argv = w.records("start")[0]["argv"]
        self.assertEqual(argv[argv.index("-C") + 1], str(w.ws))
        self.assertIn(str(extra), argv)
        self.assertEqual(argv[argv.index("-o") + 1], str(w.wd() / "last-message.md"))
        self.assertIn(" ", str(w.wd()))
        self.assertTrue((w.wd() / "stream.jsonl").exists())


class Usage(unittest.TestCase):
    """What a dispatch charges (charges.jsonl) and records (attempts.jsonl)."""

    def test_killed_worker_usage_from_session_or_unknown(self):
        w = World(self)
        w.set_limits("implementer", steps=1)
        # With a session file: usage comes from it although no turn completed.
        w.scenario("killed", thread="00000000-0000-4000-8000-000000000001")
        w.dispatch("d1").ok()
        saved = w.result("d1")
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "step_limit"))
        self.assertTrue(saved["usage_known"])
        self.assertGreater(saved["usage"]["input"] + saved["usage"]["cached"], 0)
        self.assertEqual(w.charges()[0]["charge_id"], "d1:codex:00000000-0000-4000-8000-000000000001")
        # Without one: unknown, never zero.
        w.scenario("killed", thread="00000000-0000-4000-8000-000000000002", no_rollout=1)
        w.dispatch("d2").ok()
        self.assertFalse(w.result("d2")["usage_known"])
        attempt = w.attempts()[-1]
        self.assertEqual((attempt["dispatch"], attempt["usage"], attempt["usage_known"], attempt["cost_estimate"]),
                         ("d2", None, False, None))

    def test_stream_usage_is_charged_when_session_missing(self):
        w = World(self)
        thread = "22222222-2222-4333-8444-555555555555"
        w.scenario("ready", thread=thread)
        w.dispatch().ok()
        [c] = w.charges()
        self.assertEqual((c["charge_id"], c["source"], c["key"]), (f"d1:codex:{thread}", "codex-stream", thread))
        self.assertTrue(c["usage"]["known"])
        [attempt] = w.attempts()
        self.assertEqual(attempt["usage"], c["usage"])
        self.assertEqual(c["usage"]["output"], w.result()["usage"]["output"])


class RunnerLifecycle(unittest.TestCase):
    """The detached runner: who owns a dispatch, what a repeated `dispatch` line attaches to, and
    the exit codes (0 with the reply; 1 with `error: `; 3 for a second runner)."""

    def test_runner_survives_foreground_death_and_records_result(self):
        w = World(self)
        w.scenario("ready", delay=2)
        fg = w.popen()
        wait_for(lambda: (w.runner_json() or {}).get("codex"), "Codex started", proc=fg)
        os.killpg(fg.pid, signal.SIGKILL)         # the foreground's own group: never the runner's session
        fg.communicate(timeout=10)
        wait_for(lambda: (w.wd() / "result.json").exists(), "result.json", timeout=20)
        self.assertEqual(w.result()["status"], "READY")
        [attempt] = w.attempts()
        self.assertEqual(attempt["dispatch"], "d1")
        # Limits came from the run's config (the default preset's implementer limits).
        request = w.read("d1", "request.json")
        self.assertEqual((request["steps"], request["minutes"], request["network"]), (150, 60, False))

    def test_second_dispatch_attaches_to_live_runner(self):
        w = World(self)
        w.scenario("ready", delay=2)
        first = w.start_detached()
        runner = w.runner_json()["runner"]
        w.dispatch().ok()
        self.assertEqual(w.result()["status"], "READY")
        self.assertEqual(len(w.records("start")), 1)
        self.assertEqual(w.runner_json()["runner"], runner)
        self.assertEqual(w.runner_json()["codex"], first)

    def test_repeated_dispatch_returns_existing_result(self):
        w = World(self)
        w.scenario("ready")
        first = w.dispatch().ok()
        second = w.dispatch().ok()
        self.assertEqual(first.stdout, second.stdout)
        self.assertEqual(len(w.records("start")), 1)
        self.assertEqual(len(w.attempts()), 1)

    def test_cli_prints_reply_and_session_or_still_running(self):
        # The first call is left before its runner finishes. (Its exit 75 and `still running` line,
        # after the foreground's 540 s wait, are test_foreground_wait_ends_with_exit_75_….)
        w = World(self)
        w.scenario("ready", delay=1)
        w.start_detached()
        r = w.dispatch().ok()
        lines = r.stdout.splitlines()
        self.assertEqual(lines[0], "Status: READY")
        self.assertEqual(lines[-1], f"session: {DEFAULT_THREAD}")
        self.assertEqual(len(w.records("start")), 1)

    def test_foreground_wait_ends_with_exit_75_and_the_still_running_line(self):
        # Waits the real 540 s FOREGROUND_WAIT: no test hook shortens it (ruling S1-R12).
        w = World(self)
        w.scenario("ready", delay=600)          # Codex outlasts the foreground wait
        r = w.dispatch(timeout=700)
        self.assertEqual(r.code, 75, r)
        self.assertEqual(r.stdout.splitlines(), ["still running: run the same command again (dispatch d1)"])
        self.assertFalse((w.wd() / "result.json").exists())
        runner = w.runner_json()
        self.assertTrue(alive(runner["runner"]))
        self.assertTrue(alive(runner["codex"]))
        self.assertEqual(len(w.records("start")), 1)

    def test_concurrent_dispatches_start_one_runner(self):
        w = World(self)
        w.scenario("ready", delay=2)
        procs = [w.popen() for _ in range(2)]
        outs = [p.communicate(timeout=60) for p in procs]
        for p, (out, err) in zip(procs, outs):
            self.assertEqual(p.returncode, 0, err)
            self.assertEqual(out.splitlines()[0], "Status: READY")
        self.assertEqual(len(w.records("start")), 1)
        self.assertEqual(len(w.attempts()), 1)
        self.assertFalse((w.wd() / "superseded").exists())

    def test_second_runner_refuses_while_first_holds_lock(self):
        w = World(self)
        w.scenario("slow")
        w.start_detached()
        before = (w.wd() / "runner.json").read_text(encoding="utf-8")
        r = run_cli("dispatch-runner", w.wd(), env=w.env, cwd=w.tmp)
        self.assertEqual(r.code, 3, r)
        self.assertEqual((w.wd() / "runner.json").read_text(encoding="utf-8"), before)
        self.assertEqual(len(w.records("start")), 1)

    def test_runner_record_without_start_time_is_not_dead_while_lock_held(self):
        # A runner whose start time could not be read still owns the dispatch while it holds
        # runner.lock: `dispatch` waits on it and replaces it only once the lock is free.
        w = World(self)
        w.scenario("ready")
        decoy = w.decoy()
        wd = w.wd()
        wd.mkdir(parents=True)
        record = {"runner": {"pid": decoy.pid, "pgid": decoy.pid, "start": None}, "codex": None, "generation": 1}
        (wd / "runner.json").write_text(json.dumps(record), encoding="utf-8")
        (wd / "heartbeat").touch()
        fd = os.open(wd / "runner.lock", os.O_RDWR | os.O_CREAT, 0o644)
        held = [fd]
        self.addCleanup(lambda: [os.close(f) for f in held])
        fcntl.flock(fd, fcntl.LOCK_EX)
        fg = w.popen()
        wait_for(lambda: (wd / "request.json").exists(), "dispatch to write request.json", proc=fg)
        holds(lambda: fg.poll() is None and not (wd / "spawn.json").exists() and not (wd / "superseded").exists(),
              1.0, "dispatch waits on the runner holding runner.lock")
        self.assertEqual(w.records("start"), [])
        held.remove(fd)
        os.close(fd)
        out, err = fg.communicate(timeout=60)
        self.assertEqual(fg.returncode, 0, err)
        self.assertEqual(out.splitlines()[0], "Status: READY")
        self.assertEqual(read_json(wd / "superseded" / "1" / "runner.json"), record)
        self.assertIsNone(decoy.poll())

    def test_launch_intent_recorded_before_codex_starts(self):
        w = World(self)
        w.scenario("ready")
        w.env["LEADHAND_CRASH_AT"] = "before_worker_launch"
        r = w.dispatch()
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        runner = w.runner_json()
        self.assertIsNone(runner["codex"])
        self.assertTrue(runner.get("launching"))
        self.assertEqual(w.records("start"), [])

    def test_ready_result_landing_during_replacement_cancel_is_kept(self):
        w = World(self)
        w.scenario("ready", delay=1)
        codex = w.start_detached()
        runner = w.runner_json()["runner"]
        w.signal(runner, signal.SIGSTOP)
        self.addCleanup(w.signal, runner, signal.SIGCONT)
        wait_for(lambda: [r for r in w.records("exit") if r["pid"] == codex["pid"]], "Codex to exit")
        w.age_heartbeat()
        wd, done = w.wd(), threading.Event()

        def resume_when_cancel_requested():
            end = time.monotonic() + 60
            while not done.is_set() and not (wd / "cancel").exists() and time.monotonic() < end:
                time.sleep(0.01)
            w.signal(runner, signal.SIGCONT)

        t = threading.Thread(target=resume_when_cancel_requested, daemon=True)
        t.start()
        try:
            w.dispatch().ok()
        finally:
            done.set()
            t.join(70)
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("READY", "completed"))
        self.assertEqual(len(w.records("start")), 1)
        self.assertFalse((wd / "superseded").exists())

    def test_heartbeat_refreshed_before_second_look_is_not_hung(self):
        w = World(self)
        w.scenario("slow")
        w.start_detached()
        runner = w.runner_json()["runner"]
        w.signal(runner, signal.SIGSTOP)
        self.addCleanup(w.signal, runner, signal.SIGCONT)
        w.age_heartbeat()
        wd = w.wd()
        fg = w.popen()

        def untouched():
            return (fg.poll() is None and not (wd / "cancel").exists()
                    and (w.runner_json() or {}).get("runner") == runner)

        # The runner wakes 2 s after the second `dispatch` started: after that command's first look
        # found the heartbeat stale, and well inside the 5 s it waits before calling it hung. It is
        # still running, untouched, 8 s after it started (the Python test's wait).
        holds(untouched, 2.0, "the stale runner is left alone before it wakes")
        w.signal(runner, signal.SIGCONT)
        holds(untouched, 6.0, "the runner that refreshed its heartbeat is left alone")
        self.assertTrue(alive(runner))
        self.assertEqual(len(w.records("start")), 1)


class Cancellation(unittest.TestCase):
    """`stop-dispatch`, a cancel request, and a hung runner's replacement."""

    def test_signal_only_after_pid_and_start_time_match(self):
        w = World(self)
        decoy = w.decoy()
        wrong = {"pid": decoy.pid, "pgid": decoy.pid, "start": harness._start_time(decoy.pid) + "-not"}
        wd = w.wd()
        wd.mkdir(parents=True)
        (wd / "request.json").write_text(json.dumps(w.request_json("d1")), encoding="utf-8")
        (wd / "runner.json").write_text(json.dumps({"runner": wrong, "codex": wrong, "generation": 1}),
                                        encoding="utf-8")
        (wd / "procs.json").write_text(json.dumps({"descendants": [wrong]}), encoding="utf-8")
        w.stop().ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "cancelled"))
        holds(lambda: decoy.poll() is None, 0.3, "a process whose start time did not match was not signalled")
        # dispatch treats the mismatched runner as gone: a fresh dispatch id runs normally.
        w.scenario("ready")
        wd2 = w.wd("d2")
        wd2.mkdir(parents=True)
        (wd2 / "runner.json").write_text(json.dumps({"runner": wrong, "codex": wrong, "generation": 1}),
                                         encoding="utf-8")
        w.dispatch("d2").ok()
        self.assertEqual(w.result("d2")["status"], "READY")
        self.assertIsNone(decoy.poll())

    def test_cancel_detached_worker_stops_every_process_and_writes_nothing_after(self):
        w = World(self)
        w.scenario("slow")
        codex = w.start_detached()
        child = wait_for(lambda: w.records("child"), "the tool child")[0]
        progress = w.ws / "progress.log"
        wait_for(lambda: progress.exists() and progress.stat().st_size > 0, "progress.log")
        r = w.stop().ok()
        self.assertEqual(r.stdout.splitlines()[0], "Status: SPENT")
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "cancelled"))
        self.assertFalse(alive(codex))
        self.assertIn(child["child_pid"], w.stopped())
        self.assertFalse(logged_alive(child))
        before = progress.stat()
        holds(lambda: (lambda s: (s.st_size, s.st_mtime_ns) == (before.st_size, before.st_mtime_ns))(progress.stat()),
              1.5, "nothing writes progress.log after the cancel")
        self.assertEqual(w.attempts()[0]["exit_reason"], "cancelled")

    def test_stale_heartbeat_on_live_runner_is_cancelled_not_replaced_alongside(self):
        w = World(self)
        w.scenario("slow")
        first_codex = w.start_detached()
        runner = w.runner_json()["runner"]
        w.signal(runner, signal.SIGSTOP)              # hung: alive, but no heartbeat
        self.addCleanup(w.signal, runner, signal.SIGCONT)
        w.age_heartbeat()
        w.scenario("ready")
        w.dispatch().ok()
        self.assertEqual(w.result()["status"], "READY")
        self.assertFalse(alive(runner))
        self.assertFalse(alive(first_codex))
        superseded = read_json(w.wd() / "superseded" / "1" / "result.json")
        self.assertEqual((superseded["status"], superseded["exit_reason"]), ("SPENT", "cancelled"))
        second = w.runner_json()
        self.assertNotEqual(second["runner"]["pid"], runner["pid"])
        self.assertGreaterEqual(second["started_wall"], superseded["confirmed_at"])
        starts = w.records("start")
        exits = [r for r in w.records("exit") if r["pid"] == first_codex["pid"]]
        self.assertEqual(len(starts), 2)
        self.assertTrue(exits)
        self.assertGreaterEqual(starts[1]["time"], exits[0]["time"])
        self.assertEqual(len(w.attempts()), 2)

    def test_cancel_requested_during_orphan_is_not_resurrected(self):
        w = World(self)
        w.scenario("slow")
        codex = w.start_detached()
        runner = w.runner_json()["runner"]
        w.signal(runner, signal.SIGKILL)
        wait_for(lambda: not alive(runner), "the runner to die")
        # stop-dispatch has made its request (step 1) and is still stopping Codex.
        (w.wd() / "cancel").write_text(json.dumps({"requested_at": "earlier"}), encoding="utf-8")
        w.dispatch().ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "cancelled"))
        self.assertFalse(alive(codex))
        self.assertEqual(len(w.records("start")), 1)
        self.assertFalse((w.wd() / "superseded").exists())


class ProcessControl(unittest.TestCase):
    """Codex's processes outlive neither the runner nor the dispatch."""

    def test_orphaned_codex_group_is_terminated_before_finalizing(self):
        w = World(self)
        w.scenario("slow", orphan_child=1)
        codex = w.start_detached()
        child_rec = wait_for(lambda: w.records("child"), "the tool child")[0]
        child = child_rec["child_pid"]
        wait_for(lambda: any(d["pid"] == child for d in (w.read("d1", "procs.json") or {}).get("descendants", [])),
                 "procs.json to list the tool child")
        runner = w.runner_json()["runner"]
        w.signal(runner, signal.SIGKILL)
        wait_for(lambda: not alive(runner), "the runner to die")
        self.assertTrue(alive(codex))
        w.stop().ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "cancelled"))
        self.assertFalse(alive(codex))
        self.assertIn(child, [p["pid"] for p in saved["stopped"]])
        self.assertFalse(logged_alive(child_rec))
        self.assertEqual(len(w.attempts()), 1)

    def test_double_forked_child_is_stopped_before_finalizing(self):
        # Codex review round 1 (M4): a process that leaves Codex's tree (double fork, new session)
        # just before Codex exits is found by the dispatch tag in its environment and stopped
        # before the result is written.
        w = World(self)
        w.scenario("ready-escaped-writer")
        w.dispatch().ok()
        (rec,) = w.records("escaped")
        pid = rec["escaped_pid"]
        self.assertEqual(w.result()["status"], "READY")
        self.assertIn(pid, w.stopped())
        self.assertFalse(logged_alive(rec), "the escaped writer outlived the dispatch")
        log = w.ws / "escaped.log"
        size = log.stat().st_size
        holds(lambda: log.stat().st_size == size, 0.5, "the escaped writer writes nothing after finalize")
        tag = w.runner_json()["tag"]
        self.assertEqual(tag, f"{w.run_id}:d1:1")
        start = w.records("start")[0]
        self.assertEqual(start["env"]["LEADHAND_DISPATCH_TAG"], tag)
        self.assertEqual(start["policy_set"]["LEADHAND_DISPATCH_TAG"], tag)


class RunnerCommits(unittest.TestCase):
    """Ruling R39: Codex cannot write the Git directory, so the runner commits its work."""

    def setUp(self):
        self.w = w = World(self)
        w.git("checkout", "--quiet", "-b", CARD_BRANCH)
        self.base_sha = w.git("rev-parse", "HEAD")
        # The runner's commit must be unsigned and skip hooks, whatever the repo says.
        run_git(w.repo, "config", "commit.gpgsign", "true")
        run_git(w.repo, "config", "gpg.program", "false")
        hook = Path(w.git("rev-parse", "--path-format=absolute", "--git-path", "hooks")) / "pre-commit"
        hook.parent.mkdir(parents=True, exist_ok=True)
        hook.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        hook.chmod(0o755)

    def commit_request(self, did="d1") -> dict:
        """The request flags of an implement dispatch on the card branch, with the card's brief."""
        self.w.scratch(did, brief_text=CARD_BRIEF)
        return {"did": did, "kind": "implement", "branch": CARD_BRANCH}

    def report(self, did="d1") -> Path:
        return self.w.scratch(did)[1]

    def edits(self, did="d1", *, report=True, steps=0, status="READY"):
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"},
                 {"action": "write_file", "path": "notes/new file.txt", "text": "untracked\n"}]
        if report:
            lines.append({"action": "write_file", "path": str(self.report(did)),
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
        self.w.scenario(lines)

    def dispatch(self, did="d1", **request) -> harness.Result:
        return self.w.dispatch(did, **request)

    def head(self) -> str:
        return self.w.git("rev-parse", "HEAD")

    def head_files(self) -> list[str]:
        return self.w.git("show", "--name-only", "--format=", "HEAD").splitlines()

    def test_runner_commits_codex_work_on_card_branch(self):
        w = self.w
        req = self.commit_request()
        self.edits()
        self.dispatch(**req).ok()
        saved = w.result()
        self.assertEqual(saved["status"], "READY")
        head = self.head()
        self.assertEqual(saved["committed"], head)
        self.assertEqual(w.git("rev-parse", f"refs/heads/{CARD_BRANCH}"), head)
        self.assertEqual(w.git("rev-parse", "HEAD^"), self.base_sha)
        self.assertEqual(w.git("log", "-1", "--format=%s"), "implement card 1: Added the widget constant.")
        self.assertIn("Leadhand-Dispatch: d1", w.git("log", "-1", "--format=%B"))
        self.assertNotIn("gpgsig", w.git("cat-file", "commit", "HEAD"))
        self.assertEqual(sorted(self.head_files()), ["notes/new file.txt", "src/widget.py"])
        self.assertEqual(w.git("status", "--porcelain"), "")
        self.assertTrue(self.report().exists())         # the scratch folder stays, untracked and ignored

    def test_runner_commits_partial_work_when_spent(self):
        w = self.w
        w.set_limits("implementer", steps=2)
        req = self.commit_request()
        self.edits(report=False, steps=5)
        self.dispatch(**req).ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "step_limit"))
        self.assertEqual(saved["committed"], self.head())
        self.assertEqual(w.git("log", "-1", "--format=%s"), "implement card 1: Widget card")
        self.assertIn("src/widget.py", self.head_files())

    def test_commit_intent_restart_after_the_commit_does_not_commit_again(self):
        # A runner that dies after its commit (the intent in commit.json, HEAD carrying the trailer)
        # is finalized by the next `dispatch`, which reports that commit and makes no other.
        w = self.w
        req = self.commit_request()
        self.edits()
        w.env["LEADHAND_CRASH_AT"] = "after_charge_before_attempt"
        r = self.dispatch(**req)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        sha = self.head()
        self.assertNotEqual(sha, self.base_sha)
        self.assertTrue((w.wd() / "commit.json").exists())
        self.assertFalse((w.wd() / "result.json").exists())
        del w.env["LEADHAND_CRASH_AT"]
        self.dispatch(**req).ok()
        interrupted = read_json(w.wd() / "superseded" / "1" / "result.json")
        self.assertEqual(interrupted["committed"], sha)
        self.assertEqual(self.head(), sha)
        self.assertEqual(w.git("rev-list", "--count", f"{self.base_sha}..HEAD"), "1")

    def test_commit_intent_restart_before_the_commit_still_commits(self):
        # The intent is there but HEAD has not moved: the commit was never made, so it is.
        w = self.w
        req = self.commit_request()
        w.wd().mkdir(parents=True)
        (w.wd() / "commit.json").write_text(json.dumps(
            {"at": "2026-01-01T00:00:00.000Z", "branch": CARD_BRANCH, "head_before": self.base_sha}),
            encoding="utf-8")
        self.edits()
        self.dispatch(**req).ok()
        saved = w.result()
        self.assertNotEqual(saved["committed"], self.base_sha)
        self.assertEqual(saved["committed"], self.head())
        self.assertEqual(w.git("rev-list", "--count", f"{self.base_sha}..HEAD"), "1")

    def test_a_fifo_left_at_the_report_never_hangs_the_commit(self):
        # Codex review 9: the runner reads the report outside the sandbox; a FIFO would block it.
        w = self.w
        req = self.commit_request()
        report = self.report()
        os.mkfifo(report)

        def unblock():
            with contextlib.suppress(OSError):
                os.close(os.open(report, os.O_WRONLY | os.O_NONBLOCK))
        self.addCleanup(unblock)
        self.edits(report=False)
        self.dispatch(**req).ok()
        self.assertEqual(w.result()["committed"], self.head())
        self.assertEqual(w.git("log", "-1", "--format=%s"), "implement card 1: Widget card")

    def test_runner_commits_an_edit_hidden_behind_an_index_flag(self):
        w = self.w
        for flag in ("--assume-unchanged", "--skip-worktree"):
            with self.subTest(flag=flag):
                w.git("reset", "--quiet", "--hard", self.base_sha)
                req = self.commit_request(f"d{flag[2:5]}")
                w.git("update-index", flag, "README.md")
                w.scenario([{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                            {"action": "write_file", "path": "README.md", "text": "edited, hidden from status\n"},
                            {"action": "last_message", "text": "Status: READY\n"}])
                self.dispatch(**req).ok()
                self.assertEqual(w.result(req["did"])["committed"], self.head())
                self.assertEqual(w.git("show", "HEAD:README.md"), "edited, hidden from status")
                self.assertEqual(w.git("ls-files", "-v", "--", "README.md"), "H README.md")

    def test_failed_commit_is_blocked_environment_with_reason(self):
        w = self.w
        for name in ("EMAIL", "GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"):
            w.env.pop(name, None)
        run_git(w.repo, "config", "--unset", "user.name")
        run_git(w.repo, "config", "--unset", "user.email")
        run_git(w.repo, "config", "user.useConfigOnly", "true")
        req = self.commit_request()
        self.edits()
        r = self.dispatch(**req).ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["blocked_by"], saved["exit_reason"]),
                         ("BLOCKED", "environment", "error"))
        self.assertIn("could not commit", saved["reply"])
        self.assertIn("could not commit", r.stdout)
        self.assertIsNone(saved["committed"])
        self.assertTrue(saved["commit_error"])
        self.assertIn(saved["commit_error"].splitlines()[0][:40], saved["reply"])
        self.assertEqual(self.head(), self.base_sha)
        self.assertIn("src/", w.git("status", "--porcelain"))
        self.assertEqual(w.attempts()[0]["status"], "BLOCKED")

    def test_commit_respects_the_cancel_deadline(self):
        # A cancel whose deadline has passed leaves the runner no time to commit.
        w = self.w
        req = self.commit_request()
        w.scenario([{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                    {"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"},
                    {"action": "sleep", "seconds": 60}])
        w.start_detached(**req)
        wait_for(lambda: (w.ws / "src" / "widget.py").exists(), "Codex's edit")
        (w.wd() / "cancel").write_text(json.dumps(
            {"requested_at": "2026-01-01T00:00:00.000Z", "deadline_wall": time.time() - 1}), encoding="utf-8")
        wait_for(lambda: (w.wd() / "result.json").exists(), "result.json")
        saved = w.result()
        self.assertIsNone(saved["committed"])
        self.assertIn("deadline", saved["commit_error"])
        self.assertEqual(self.head(), self.base_sha)

    def test_externally_killed_codex_is_possibly_incomplete_and_partial_work_committed(self):
        w = self.w
        req = self.commit_request()
        w.scenario([{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                    {"action": "write_file", "path": "src/widget.py", "text": "WIDGET = 1\n"},
                    {"type": "item.completed", "item": {"id": "c1", "type": "command_execution",
                                                        "status": "completed"}},
                    {"action": "sleep", "seconds": 60}])
        codex = w.start_detached(**req)
        wait_for(lambda: (w.ws / "src" / "widget.py").exists(), "Codex's edit")
        w.signal(codex, signal.SIGKILL)          # from outside, like Task 22
        self.dispatch(**req).ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["exit_reason"]), ("SPENT", "interrupted"))
        [c] = w.charges()
        self.assertTrue(c.get("possibly_incomplete"))
        [attempt] = w.attempts()
        self.assertTrue(attempt["possibly_incomplete"])
        self.assertTrue(saved["possibly_incomplete"])
        self.assertEqual(saved["committed"], self.head())
        self.assertIn("src/widget.py", self.head_files())

    def test_exit_without_final_turn_completed_is_possibly_incomplete(self):
        w = self.w
        req = self.commit_request()
        w.scenario([{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                    {"type": "turn.completed", "usage": {"input_tokens": 5, "cached_input_tokens": 0,
                                                         "output_tokens": 1}},
                    {"type": "turn.started"},
                    {"action": "last_message", "text": "Status: READY\n"}])
        self.dispatch(**req).ok()
        self.assertEqual(w.result()["status"], "READY")
        [c] = w.charges()
        self.assertTrue(c.get("possibly_incomplete"))
        self.assertTrue(w.attempts()[0]["possibly_incomplete"])

    def test_complete_run_is_not_possibly_incomplete(self):
        w = self.w
        req = self.commit_request()
        self.edits()
        self.dispatch(**req).ok()
        [c] = w.charges()
        self.assertFalse(c.get("possibly_incomplete"))
        self.assertFalse(w.attempts()[0]["possibly_incomplete"])

    def test_wrong_branch_is_not_committed(self):
        w = self.w
        req = self.commit_request()
        w.git("checkout", "--quiet", "-b", "somewhere-else")
        self.edits()
        self.dispatch(**req).ok()
        saved = w.result()
        self.assertEqual((saved["status"], saved["blocked_by"]), ("BLOCKED", "environment"))
        self.assertIsNone(saved["committed"])
        self.assertIn("somewhere-else", saved["commit_skipped"])
        self.assertEqual(self.head(), self.base_sha)
        self.assertIn("src/", w.git("status", "--porcelain"))

    def test_operation_in_progress_is_not_committed(self):
        w = self.w
        req = self.commit_request()
        merge_head = Path(w.git("rev-parse", "--path-format=absolute", "--git-path", "MERGE_HEAD"))
        merge_head.write_text(self.base_sha + "\n", encoding="utf-8")
        self.edits()
        self.dispatch(**req).ok()
        saved = w.result()
        self.assertIsNone(saved["committed"])
        self.assertIn("in progress", saved["commit_skipped"])
        self.assertEqual(self.head(), self.base_sha)

    def test_check_dispatch_is_not_committed(self):
        w = self.w
        self.edits("r1")
        self.dispatch("r1", kind="review", branch=CARD_BRANCH).ok()
        self.assertIsNone(w.result("r1")["committed"])
        self.assertEqual(self.head(), self.base_sha)

    def test_cli_passes_kind_and_branch(self):
        w = self.w
        req = self.commit_request()
        self.edits()
        self.dispatch(**req).ok()
        request = w.read("d1", "request.json")
        self.assertEqual((request["kind"], request["branch"]), ("implement", CARD_BRANCH))
        self.assertEqual(w.result()["committed"], self.head())


if __name__ == "__main__":
    unittest.main()
