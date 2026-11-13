"""A scripted world for engine scenarios: tests/fakeworld.py's FakeWorld, driving the binary at
LEADHAND_BIN only through its command line.

`FakeSupervisor` does what the run skill's supervisor does, with fake workers instead of real
ones. Every engine step is a CLI call: `start`, `next`, and `record started|result|ruling|park|
diff-check` with the flags the run skill uses (a result through the dispatch's `reply_file`).

- implement-type kinds (implement, fix, answer, continue, final_fix) write files in the
  workspace, commit them on the checked-out branch (a card branch, or the final fix branch)
  and write a report whose first line is the scripted `Status:`;
- review kinds (review, rereview, final_review, security) write `Verdict: ...` with findings;
- verify kinds (verify, evidence) write `Result: ...`;
- integrate rebases the card branch onto the run tip, resolving conflicts in the card's
  favour, and reports like an implementer (a step's `rebase` turns that off for integrate,
  or on for another implementer kind such as answer).

A step's `tamper(world, ws, action)` runs inside the worker before its report is written.
A Claude worker's step may give `transcript`: the text its transcript gets in place of the one
usage record a worker appends by default (None: the worker writes no transcript at all).

Steps are scripted per (card, kind), as in FakeWorld: run-level dispatches have card None, and
(None, "fix_review") is the review of a fix wave. By default the run is the card loop only
(LEADHAND_TEST_CARD_LOOP_ONLY); `gates=True` runs the real end of run. `started` holds card
dispatches, `run_started` run-level ones.

Claude routes: the world records `started` with a fake agent ID, does the work, appends a usage
record to that agent's fake transcript, writes the reply to the dispatch's reply file and
records the result. Codex routes run the dispatch action's own command line (the `dispatch`
line the proxy would run) with tests/fakes/codex on PATH.

A step's `crash` of "before_commit" or "after_commit" makes the worker die at that point. The
same two points are the harness's own crash points (`crash_point`), which follow the CLI's
LEADHAND_CRASH_* protocol, read from `env`: with LEADHAND_CRASH_AT naming one, the drive dies
there with exit 97, as a crash of the session a Claude worker runs in would. Only a forked
child may die so; in the process that made the world the point raises instead.

`env` is the world's sealed environment (harness.hermetic_env plus the world's own settings);
nothing patches os.environ for leadhand. `advance(seconds)` moves the CLI's clock: the total is
passed as LEADHAND_TEST_CLOCK_OFFSET on every call. Where FakeWorld returned an engine object,
this returns what the CLI wrote: parsed JSON lines (`charges`, `attempts`) or a harness.Result
(`cli`).
"""
from __future__ import annotations

import contextlib
import fcntl
import itertools
import json
import os
import shlex
import signal
import subprocess
import time
from pathlib import Path

from gitfixture import make_repo, run_git, write
from harness import (FAKES, Result, hermetic_env, read_json, read_jsonl, run_argv, run_cli, same_process,
                     started_by, temp_root)
from rundir import read_records, run_paths

IMPLEMENTER_KINDS = ("implement", "fix", "answer", "continue", "final_fix")
REVIEW_KINDS = ("review", "rereview", "final_review", "security")
PROJECT = "-fake-project"
MAIN_SESSION = "fake-main-session"
NO_HOOKS = ("-c", "core.hooksPath=/dev/null")   # gitfixture's identity already turns signing off


def card(number, *, title=None, risk="low", hard=False, depends=(), role="implementer",
         route=None, allowed="src/, tests/"):
    lines = [f"## Card {number}: {title or f'Card number {number}'}",
             f"- Role: {role}",
             f"- Hard: {'yes — touches the core' if hard else 'no'}",
             f"- Risk: {risk}",
             f"- Depends on: {', '.join(map(str, depends)) or 'none'}",
             f"- Allowed paths: {allowed}",
             "- Done when: the card's file exists and the tests pass",
             "- Stop if: the design has to change"]
    if route:
        lines.append(f"- Route: {route}")
    return "\n".join(lines) + "\n"


class FakeSupervisor:
    def __init__(self, test, cards, *, config="", installed=("claude",), title="Demo deck", gates=False):
        self.test = test
        self.tmp = temp_root(test, "lh engine ")
        self.env = hermetic_env(test, self.tmp, installed=installed, config_text=config)
        self.env["FAKE_CODEX_LOG"] = str(self.tmp / "codex log.jsonl")
        if not gates:
            self.env["LEADHAND_TEST_CARD_LOOP_ONLY"] = "1"
        self.repo = make_repo(self.tmp)                     # "<tmp>/my repo"
        self.docs = self.tmp / "docs dir"
        self.claude_home = Path(self.env["CLAUDE_CONFIG_DIR"])
        self.codex_home = Path(self.env["CODEX_HOME"])
        self.config_file = Path(self.env["LEADHAND_CONFIG"])
        self.offset = 0.0
        self.deck_path = self.tmp / "decks dir" / "demo deck.md"
        self.deck_path.parent.mkdir()
        self.base = run_git(self.repo, "rev-parse", "HEAD")
        self.write_deck(cards, title=title)
        self.run_dir: Path | None = None
        self.steps: dict[tuple[int, str], list[dict]] = {}
        self.rulings: dict[int, list[str]] = {}
        self.parks: dict[int, str] = {}
        self.diff_checks: dict[int, list[tuple[bool, str]]] = {}
        self.started: list[dict] = []
        self.run_started: list[dict] = []
        self.proxies: list[subprocess.Popen] = []
        self.launch_log: Path | None = None
        self._ids = itertools.count(1)
        self._messages = itertools.count(1)
        self._owner = os.getpid()
        test.addCleanup(self.kill_leftovers)

    # -- setup ----------------------------------------------------------------

    def write_deck(self, cards, *, title="Demo deck", branch="leadhand/demo"):
        text = (f"# Deck: {title}\nSource: docs/plan.md\nBase: main @ {self.base}\n"
                f"Branch: {branch}\nBuild: true\nTest: true\n\n" + "\n".join(cards))
        self.deck_path.write_text(text, encoding="utf-8")

    def start(self, *, new: bool = False) -> Path:
        """`start` the deck; the run directory is the one it prints (`run:`, or `existing run:`)."""
        r = self._cli("start", self.deck_path, "--repo", self.repo, *(["--new"] if new else [])).ok()
        for line in r.stdout.splitlines():
            for prefix in ("run: ", "existing run: "):
                if line.startswith(prefix):
                    self.run_dir = Path(line[len(prefix):])
                    return self.run_dir
        raise AssertionError(f"leadhand start printed no `run: <dir>` line\n{r}")

    def script(self, card_number: int, kind: str, *steps: dict) -> None:
        self.steps.setdefault((card_number, kind), []).extend(steps)

    def advance(self, seconds: float) -> None:
        self.offset += seconds

    def session(self, session_id: str, messages: int = 1) -> Path:
        """Point CLAUDE_CODE_SESSION_ID at a supervisor transcript and append messages to it."""
        self.env["CLAUDE_CODE_SESSION_ID"] = session_id
        path = self.claude_home / "projects" / PROJECT / f"{session_id}.jsonl"
        for _ in range(messages):
            self._append_message(path, "claude-opus-4")
        return path

    # -- reading --------------------------------------------------------------

    @property
    def paths(self):
        if self.run_dir is None:
            raise AssertionError("no run yet: call start() first")
        return run_paths(self.run_dir)

    def meta(self) -> dict:
        return read_json(self.paths.run_json)

    def workspace(self) -> Path:
        return Path(self.meta()["workspace"])

    def events(self, kind=None) -> list[dict]:
        return [e for e in read_records(self.paths.events) if kind is None or e["kind"] == kind]

    def charges(self) -> list[dict]:
        return read_records(self.paths.charges)

    def attempts(self) -> list[dict]:
        return [a for a in read_records(self.paths.attempts) if a.get("recorded_by") == "engine"]

    def dispatch_ids(self) -> list[str]:
        return [a["dispatch_id"] for a in self.started]

    def run_dispatch_ids(self) -> list[str]:
        return [a["dispatch_id"] for a in self.run_started]

    def branch_tip(self, branch: str) -> str:
        return run_git(self.repo, "rev-parse", f"refs/heads/{branch}")

    # -- driving --------------------------------------------------------------

    def next(self) -> dict:
        return self._cli("next", self.run_dir).ok().json()

    def run(self, limit: int = 80) -> list[dict]:
        actions = []
        for _ in range(limit):
            a = self.next()
            actions.append(a)
            if a["action"] == "done":
                return actions
            self.act(a)
        raise AssertionError(f"the run did not finish in {limit} actions: {actions[-3:]}")

    def act(self, a: dict) -> None:
        if a["action"] == "dispatch":
            worker = self.launch(a)
            reply = self.perform(a, worker)
            if reply is not None:
                reply_file = Path(a["reply_file"])
                reply_file.parent.mkdir(parents=True, exist_ok=True)
                reply_file.write_text(reply, encoding="utf-8")
                self._record("result", "--dispatch", a["dispatch_id"], "--worker", worker,
                             "--reply-file", reply_file)
        elif a["action"] == "answer":
            n = a["card"]
            if n in self.parks:
                self._record("park", "--card", n, "--reason", self.parks[n])
            else:
                self._record("ruling", "--card", n, "--text", self.rulings[n].pop(0))
        elif a["action"] == "check_diff":
            ok, note = (self.diff_checks.get(a["card"]) or [(True, "test-only fix looks right")]).pop(0)
            self._record("diff-check", "--card", a["card"], "--ok", "yes" if ok else "no", "--note", note)
        else:
            raise AssertionError(f"unexpected action in a synchronous world: {a}")

    def launch(self, a: dict) -> str:
        """Start the dispatch's worker: record `started` with its agent ID."""
        if a["resume"]:
            worker = a["resume"]["worker_id"]
        else:
            worker = f"agent{next(self._ids):04d}"
        self._record("started", "--dispatch", a["dispatch_id"], "--worker", worker)
        (self.started if a["card"] is not None else self.run_started).append(a)
        if self.launch_log is not None:
            with open(self.launch_log, "a", encoding="utf-8") as f:
                f.write(a["dispatch_id"] + "\n")
        return worker

    def perform(self, a: dict, worker: str) -> str | None:
        """Run the worker to its end and return its reply (None if it died)."""
        if a["agent"] == "leadhand:codex":
            step = self._take(a)
            reply = self.run_proxy(a, step)
            self._append_message(self.subagent_transcript(worker), "claude-haiku-4")
            return reply
        return self.claude_worker(a, worker)

    # -- workers --------------------------------------------------------------

    def _take(self, a: dict) -> dict:
        kind = "fix_review" if a["card"] is None and a["kind"] == "final_review" and a.get("wave") else a["kind"]
        queue = self.steps.get((a["card"], kind)) or []
        return queue.pop(0) if queue else {}

    def report_text(self, a: dict, step: dict) -> str:
        kind = a["kind"]
        if "status" in step or kind in IMPLEMENTER_KINDS or kind == "integrate":
            status = step.get("status", "READY")
            text = f"Status: {status}\n"
            if step.get("blocked_by"):
                text += f"Blocked by: {step['blocked_by']}\n"
            text += f"## Summary\nFake work for {a['dispatch_id']}.\n## Commits\n## Tests\nfake: pass\n"
            if status == "QUESTION":
                text += ("## Question\nWhich storage format should widgets use?\nOptions:\n"
                         "- A: JSON\n- B: SQLite\nRecommendation: A, because it is simpler.\n")
            return text + "## Concerns\nNone\n"
        if kind in REVIEW_KINDS:
            verdict = step.get("verdict", "clean")
            findings = step.get("findings")
            if findings is None:
                findings = [] if verdict == "clean" else ["[high] src/widget.py:1 — wrong — fix it"]
            body = "\n".join(f"{i}. {f}" for i, f in enumerate(findings, 1)) or "None"
            return f"Verdict: {verdict}\n## Findings\n{body}\n## Notes\nRead the diff.\n"
        return f"Result: {step.get('result', 'pass')}\n## Checks\nfake\n"

    def claude_worker(self, a: dict, worker: str) -> str | None:
        step = self._take(a)
        ws = self.workspace()
        if step.get("rebase", a["kind"] == "integrate"):
            run_tip = run_git(ws, "rev-parse", f"refs/heads/{self.meta()['branch']}")
            run_git(ws, *NO_HOOKS, "-c", "core.editor=true", "rebase", "--quiet", "-X", "theirs", run_tip)
        if a["kind"] in IMPLEMENTER_KINDS:
            files = step.get("files") or {self.default_file(a): f"{self.work_label(a)}\n"}
            write(ws, files)
            self.crash_point("before_commit")
            if step.get("crash") == "before_commit":
                return None
            run_git(ws, "add", "--", *files)
            if run_git(ws, "diff", "--cached", "--name-only"):
                run_git(ws, *NO_HOOKS, "commit", "--quiet", "-m", f"work for {self.work_label(a)}")
            self.crash_point("after_commit")
            if step.get("crash") == "after_commit":
                return None
        if step.get("tamper"):
            step["tamper"](self, ws, a)
        report = Path(a["report"])
        text = self.report_text(a, step)
        if step.get("report", True):
            report.parent.mkdir(parents=True, exist_ok=True)
            report.write_text(text, encoding="utf-8")
        if "transcript" not in step:
            self._append_message(self.subagent_transcript(worker), "claude-sonnet-4")
        elif step["transcript"] is not None:
            path = self.subagent_transcript(worker)
            path.parent.mkdir(parents=True, exist_ok=True)
            with open(path, "a", encoding="utf-8") as f:
                f.write(step["transcript"])
        return step.get("reply") or f"{text.splitlines()[0]}\nReport: {report}"

    def work_label(self, a: dict) -> str:
        return a["dispatch_id"]

    def default_file(self, a: dict) -> str:
        folder = "final" if a["card"] is None else f"card{a['card']}"
        return f"src/{folder}/{self.work_label(a)}.txt"

    def subagent_transcript(self, worker: str) -> Path:
        return (self.claude_home / "projects" / PROJECT / MAIN_SESSION / "subagents"
                / f"agent-{worker}.jsonl")

    def _append_message(self, path: Path, model: str) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        rec = {"type": "assistant", "message": {
            "id": f"msg_{next(self._messages):05d}", "model": model,
            "usage": {"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 0,
                      "cache_creation_input_tokens": 0}}}
        with open(path, "a", encoding="utf-8") as f:
            f.write(json.dumps(rec) + "\n")

    def codex_scenario(self, a: dict, step: dict) -> Path:
        lines: list[dict] = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"}]
        if step.get("sleep"):
            lines.append({"action": "sleep", "seconds": step["sleep"]})
        if a["kind"] in IMPLEMENTER_KINDS:
            files = step.get("files") or {self.default_file(a): f"{self.work_label(a)}\n"}
            lines += [{"action": "write_file", "path": p, "text": t} for p, t in files.items()]
            if step.get("commit", True):         # False: leave it to the runner, like real Codex
                lines.append({"action": "commit", "message": f"work for {self.work_label(a)}"})
        text = self.report_text(a, step)
        reply = step.get("reply") or f"{text.splitlines()[0]}\nReport: {a['report']}"
        if step.get("report", True):
            lines.append({"action": "write_file", "path": a["report"], "text": text})
        lines += [{"type": "item.completed", "item": {"id": "item_9", "type": "agent_message", "text": reply}},
                  {"action": "last_message", "text": reply},
                  {"type": "turn.completed", "usage": {"input_tokens": 1000, "cached_input_tokens": 100,
                                                        "cache_write_input_tokens": 0, "output_tokens": 40,
                                                        "reasoning_output_tokens": 5}}]
        path = self.tmp / f"scenario {a['dispatch_id']}.jsonl"
        path.write_text("".join(json.dumps(x) + "\n" for x in lines), encoding="utf-8")
        return path

    def proxy_argv_env(self, a: dict, step: dict) -> tuple[list[str], dict]:
        """The dispatch action's own command line, as is, and its environment."""
        argv = shlex.split(a["message"])
        env = dict(self._env(), PATH=f"{FAKES}{os.pathsep}{self.env['PATH']}",
                   FAKE_CODEX_SCENARIO=str(self.codex_scenario(a, step)),
                   FAKE_CODEX_THREAD=f"0f0f0f0f-0000-4000-8000-{next(self._ids):012d}")
        return argv, env

    def proxy(self, a: dict, step: dict, *, cwd: Path | None = None, timeout: float = 180.0,
              attempts: int = 20) -> Result:
        """What the leadhand:codex proxy does: run the command, again while it is still running
        (exit 75), from `cwd` (default the world's tmp). Bounded: all the runs together get
        `timeout` seconds, and at most `attempts` runs; past either, a run still exiting 75
        fails the test with the last result."""
        argv, env = self.proxy_argv_env(a, step)
        deadline = time.monotonic() + timeout
        for _ in range(attempts):
            left = deadline - time.monotonic()
            if left <= 0:
                break
            r = run_argv(argv, env=env, cwd=cwd or self.tmp, timeout=left)
            if r.code != 75:
                return r
        raise AssertionError(f"the dispatch line was still running (exit 75) after {attempts} runs or "
                             f"{timeout:g} s\n{r}")

    def run_proxy(self, a: dict, step: dict, *, cwd: Path | None = None) -> str:
        """`proxy`, returning what the proxy hands back: the command's stdout and stderr."""
        r = self.proxy(a, step, cwd=cwd)
        return r.stdout + r.stderr

    def start_proxy(self, a: dict, step: dict) -> subprocess.Popen:
        argv, env = self.proxy_argv_env(a, step)
        p = subprocess.Popen(argv, env=env, cwd=self.tmp, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.proxies.append(p)
        return p

    def crash_point(self, name: str) -> None:
        """The harness's crash point `name`, on the CLI's protocol (state.crash_point), read from
        `env`: log every point passed to LEADHAND_CRASH_LOG; when LEADHAND_CRASH_AT names this one,
        count it in LEADHAND_CRASH_COUNTER under flock, past LEADHAND_CRASH_SKIP passes SIGKILL
        LEADHAND_CRASH_KILL_PGID and exit 97 at once."""
        env = self.env
        if env.get("LEADHAND_TESTING") != "1":
            return
        log = env.get("LEADHAND_CRASH_LOG")
        if log:
            fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
            try:
                os.write(fd, f"{name}\n".encode())
            finally:
                os.close(fd)
        if env.get("LEADHAND_CRASH_AT") != name:
            return
        counter = env.get("LEADHAND_CRASH_COUNTER")
        if counter:
            fd = os.open(counter, os.O_RDWR | os.O_CREAT, 0o644)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX)
                seen = int(os.pread(fd, 64, 0).decode().strip() or 0) + 1
                os.ftruncate(fd, 0)
                os.pwrite(fd, f"{seen}\n".encode(), 0)
                os.fsync(fd)
            finally:
                os.close(fd)
            if seen <= int(env.get("LEADHAND_CRASH_SKIP") or 0):
                return
        if os.getpid() == self._owner:
            raise AssertionError(f"crash point {name} reached in the test process itself; "
                                 "drive the world in a forked child to crash there")
        group = env.get("LEADHAND_CRASH_KILL_PGID")
        if group:
            with contextlib.suppress(OSError, ValueError):
                os.killpg(int(group), signal.SIGKILL)
        os._exit(97)

    # -- the CLI --------------------------------------------------------------

    def _env(self) -> dict:
        return dict(self.env, LEADHAND_TEST_CLOCK_OFFSET=str(self.offset))

    def _cli(self, *args) -> Result:
        return run_cli(*args, env=self._env(), cwd=self.tmp)

    def _record(self, kind: str, *flags) -> dict:
        return self._cli("record", self.run_dir, kind, *flags).ok().json()

    def cli(self, *args, crash: str | None = None, stdin: str | None = None, env: dict | None = None) -> Result:
        full = dict(self._env(), **(env or {}))
        full.pop("LEADHAND_CRASH_AT", None)
        if crash:
            full["LEADHAND_CRASH_AT"] = crash
        return run_cli(*args, env=full, cwd=self.tmp, stdin=stdin)

    # -- cleanup --------------------------------------------------------------

    def kill_leftovers(self) -> None:
        """Kill the proxies this world started, then every process a worker folder records that
        is still the process it names (recorded_processes): runners first, so none starts another
        Codex, then the rest. Then the fake Codex processes, tool children and escaped writers its
        log names (a runner may not have recorded them yet), each only if it started no later than
        the log line naming it. Wait (briefly) for them all to be gone."""
        for p in self.proxies:
            if p.poll() is None:
                p.kill()
            with contextlib.suppress(Exception):
                p.communicate(timeout=10)
        if self.run_dir is not None:
            kill_run_leftovers(self.run_dir, Path(self.env["FAKE_CODEX_LOG"]))


def kill_run_leftovers(run_dir: Path, codex_log: Path) -> None:
    """kill_leftovers without a world: every live process run_dir's worker folders record
    (runners first), then the Codex processes, tool children and escaped writers `codex_log`
    names. For a run whose world is gone with its process, such as a crash-matrix worker killed
    at its deadline."""
    killed = []

    def kill(rec: dict) -> None:
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.kill(rec["pid"], signal.SIGKILL)
            killed.append(rec)

    for runners_only in (True, False):
        for rec in recorded_processes(run_dir, runners_only=runners_only):
            if same_process(rec.get("pid"), rec.get("start")):
                kill(rec)
    for line in (read_jsonl(codex_log) if codex_log.exists() else []):
        pid = line.get("child_pid") or line.get("escaped_pid")
        if line.get("event") == "start":
            pid = line["pid"]
        start = started_by(pid, line["time"]) if pid else None
        if start is not None:
            kill({"pid": pid, "start": start})
    deadline = time.monotonic() + 5
    while any(same_process(r["pid"], r["start"]) for r in killed) and time.monotonic() < deadline:
        time.sleep(0.02)


def recorded_processes(run_dir: Path, *, runners_only: bool = False) -> list[dict]:
    """The {pid, start} records in the run's worker folders and their superseded generations:
    the runner (runner.json, or spawn.json before it), and unless `runners_only` Codex, its
    recorded descendants (procs.json) and what a result says was stopped (result.json). A
    record is a process only while harness.same_process(pid, start) holds."""
    def read(path: Path) -> dict:
        with contextlib.suppress(ValueError, OSError):
            data = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(data, dict):
                return data
        return {}

    out = []
    for folder in [*run_dir.glob("workers/*"), *run_dir.glob("workers/*/superseded/*")]:
        runner = read(folder / "runner.json")
        out += [runner.get("runner"), read(folder / "spawn.json")]
        if not runners_only:
            out.append(runner.get("codex"))
            out += read(folder / "procs.json").get("descendants") or []
            out += read(folder / "result.json").get("stopped") or []
    return [r for r in out if isinstance(r, dict) and r.get("pid")]
