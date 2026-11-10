"""A scripted world for engine tests: a temp repo, a deck, and fake workers.

`FakeWorld` drives `engine.next_action` / `engine.record` the way the supervisor
does, with fake workers instead of real ones:

- implement-type kinds (implement, fix, answer, continue) write files in the
  workspace, commit them on the checked-out card branch and write a report whose
  first line is the scripted `Status:`;
- review kinds (review, rereview) write `Verdict: ...` with findings;
- verify kinds write `Result: ...`.

Claude routes run in-process: the world records `started` with a fake agent ID,
does the work, appends a usage record to that agent's fake transcript and
records the result. Codex routes run the dispatch action's own command (the
`leadhand dispatch` line the proxy would run) through the real dispatcher with
tests/fakes/codex on PATH. Every worker the world starts is kept in `started`.

A step's `crash` of "before_commit" or "after_commit" makes the worker die at
that point: nothing after it happens (no commit and/or no report, no result).

Everything runs hermetically: Git through tests/gitfixture.py, configuration
from a temp file (LEADHAND_CONFIG), Claude transcripts under a temp
CLAUDE_CONFIG_DIR, Codex sessions under a temp CODEX_HOME, installed vendors
from LEADHAND_INSTALLED_VENDORS, and the engine's clock offset by `advance`.
"""
from __future__ import annotations

import contextlib
import io
import itertools
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from unittest import mock

from gitfixture import hermetic, make_repo, run_git, write
from leadhand import engine, procs
from leadhand.state import read_attempts, read_events, run_paths
from leadhand.usage import read_charges

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / "scripts" / "leadhand"
FAKES = ROOT / "tests" / "fakes"
IMPLEMENTER_KINDS = ("implement", "fix", "answer", "continue")
REVIEW_KINDS = ("review", "rereview")
PROJECT = "-fake-project"
MAIN_SESSION = "fake-main-session"


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


class FakeWorld:
    def __init__(self, test, cards, *, config="", installed=("claude",), title="Demo deck"):
        hermetic(test)
        self.test = test
        self.tmp = Path(tempfile.mkdtemp(prefix="lh engine "))
        test.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = make_repo(self.tmp)                     # "<tmp>/my repo"
        self.docs = self.tmp / "docs dir"
        self.claude_home = self.tmp / "claude home"
        self.codex_home = self.tmp / "codex home"
        self.config_file = self.tmp / "user config.toml"
        self.config_file.write_text(f'docs_dir = "{self.docs}"\n{config}', encoding="utf-8")
        env = mock.patch.dict(os.environ, {
            "LEADHAND_TESTING": "1", "LEADHAND_CONFIG": str(self.config_file),
            "LEADHAND_INSTALLED_VENDORS": ",".join(installed),
            "CLAUDE_CONFIG_DIR": str(self.claude_home), "CODEX_HOME": str(self.codex_home),
            "FAKE_CODEX_LOG": str(self.tmp / "codex log.jsonl")})
        env.start()
        test.addCleanup(env.stop)
        for name in ("CLAUDE_CODE_SESSION_ID", "LEADHAND_CRASH_AT", "LEADHAND_MINUTE_SECONDS",
                     "FAKE_CODEX_SCENARIO", "FAKE_CODEX_DELAY", "FAKE_CODEX_THREAD",
                     "FAKE_CODEX_NO_ROLLOUT", "FAKE_CODEX_IGNORE_SIGINT", "FAKE_CODEX_ORPHAN_CHILD"):
            os.environ.pop(name, None)
        self.offset = 0.0
        clock = mock.patch.object(engine, "_now", lambda: time.time() + self.offset)
        clock.start()
        test.addCleanup(clock.stop)
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
        self.proxies: list[subprocess.Popen] = []
        self._ids = itertools.count(1)
        self._messages = itertools.count(1)
        test.addCleanup(self.kill_leftovers)

    # -- setup ----------------------------------------------------------------

    def write_deck(self, cards, *, title="Demo deck", branch="leadhand/demo"):
        text = (f"# Deck: {title}\nSource: docs/plan.md\nBase: main @ {self.base}\n"
                f"Branch: {branch}\nBuild: true\nTest: true\n\n" + "\n".join(cards))
        self.deck_path.write_text(text, encoding="utf-8")

    def start(self, **kw) -> Path:
        with contextlib.redirect_stdout(io.StringIO()):
            self.run_dir = engine.start(self.deck_path, repo=self.repo, **kw)
        return self.run_dir

    def script(self, card_number: int, kind: str, *steps: dict) -> None:
        self.steps.setdefault((card_number, kind), []).extend(steps)

    def advance(self, seconds: float) -> None:
        self.offset += seconds

    def session(self, session_id: str, messages: int = 1) -> Path:
        """Point CLAUDE_CODE_SESSION_ID at a supervisor transcript and append messages to it."""
        os.environ["CLAUDE_CODE_SESSION_ID"] = session_id
        path = self.claude_home / "projects" / PROJECT / f"{session_id}.jsonl"
        for _ in range(messages):
            self._append_message(path, "claude-opus-4")
        return path

    # -- reading --------------------------------------------------------------

    @property
    def paths(self):
        return run_paths(self.run_dir)

    def meta(self) -> dict:
        return json.loads(self.paths.run_json.read_text(encoding="utf-8"))

    def workspace(self) -> Path:
        return Path(self.meta()["workspace"])

    def events(self, kind=None) -> list[dict]:
        return [e for e in read_events(self.paths) if kind is None or e["kind"] == kind]

    def charges(self):
        return read_charges(self.paths)

    def attempts(self):
        return [a for a in read_attempts(self.paths) if a.get("recorded_by") == "engine"]

    def dispatch_ids(self) -> list[str]:
        return [a["dispatch_id"] for a in self.started]

    def branch_tip(self, branch: str) -> str:
        return run_git(self.repo, "rev-parse", f"refs/heads/{branch}")

    # -- driving --------------------------------------------------------------

    def next(self) -> dict:
        return engine.next_action(self.run_dir)

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
                engine.record(self.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                              reply=reply)
        elif a["action"] == "answer":
            n = a["card"]
            if n in self.parks:
                engine.record(self.run_dir, "park", card=n, reason=self.parks[n])
            else:
                engine.record(self.run_dir, "ruling", card=n, text=self.rulings[n].pop(0))
        elif a["action"] == "check_diff":
            ok, note = (self.diff_checks.get(a["card"]) or [(True, "test-only fix looks right")]).pop(0)
            engine.record(self.run_dir, "diff-check", card=a["card"], ok=ok, note=note)
        else:
            raise AssertionError(f"unexpected action in a synchronous world: {a}")

    def launch(self, a: dict) -> str:
        """Start the dispatch's worker: record `started` with its agent ID."""
        if a["resume"]:
            worker = a["resume"]["worker_id"]
        else:
            worker = f"agent{next(self._ids):04d}"
        engine.record(self.run_dir, "started", dispatch_id=a["dispatch_id"], worker_id=worker)
        self.started.append(a)
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
        queue = self.steps.get((a["card"], a["kind"])) or []
        return queue.pop(0) if queue else {}

    def report_text(self, a: dict, step: dict) -> str:
        kind = a["kind"]
        if "status" in step or kind in IMPLEMENTER_KINDS:
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
        if a["kind"] in IMPLEMENTER_KINDS:
            files = step.get("files") or {f"src/card{a['card']}/{a['dispatch_id']}.txt": f"{a['dispatch_id']}\n"}
            write(ws, files)
            if step.get("crash") == "before_commit":
                return None
            run_git(ws, "add", "--", *files)
            run_git(ws, "commit", "--quiet", "-m", f"work for {a['dispatch_id']}")
            if step.get("crash") == "after_commit":
                return None
        report = Path(a["report"])
        text = self.report_text(a, step)
        if step.get("report", True):
            report.parent.mkdir(parents=True, exist_ok=True)
            report.write_text(text, encoding="utf-8")
        self._append_message(self.subagent_transcript(worker), "claude-sonnet-4")
        return step.get("reply") or f"{text.splitlines()[0]}\nReport: {report}"

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
            files = step.get("files") or {f"src/card{a['card']}/{a['dispatch_id']}.txt": f"{a['dispatch_id']}\n"}
            lines += [{"action": "write_file", "path": p, "text": t} for p, t in files.items()]
            lines.append({"action": "commit", "message": f"work for {a['dispatch_id']}"})
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
        argv = shlex.split(a["message"])
        if argv and argv[0] == str(CLI):
            argv = [sys.executable, "-B", *argv]
        env = dict(os.environ, PATH=f"{FAKES}{os.pathsep}{os.environ['PATH']}",
                   FAKE_CODEX_SCENARIO=str(self.codex_scenario(a, step)),
                   FAKE_CODEX_THREAD=f"0f0f0f0f-0000-4000-8000-{next(self._ids):012d}")
        return argv, env

    def run_proxy(self, a: dict, step: dict) -> str:
        """What the leadhand:codex proxy does: run the command, again while it is still running."""
        argv, env = self.proxy_argv_env(a, step)
        while True:
            r = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=180,
                               cwd=self.tmp, stdin=subprocess.DEVNULL)
            if r.returncode != 75:
                return r.stdout + r.stderr

    def start_proxy(self, a: dict, step: dict) -> subprocess.Popen:
        argv, env = self.proxy_argv_env(a, step)
        p = subprocess.Popen(argv, env=env, cwd=self.tmp, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.proxies.append(p)
        return p

    # -- the CLI --------------------------------------------------------------

    def cli(self, *args, crash: str | None = None, stdin: str | None = None,
            env: dict | None = None) -> subprocess.CompletedProcess:
        full = dict(os.environ, **(env or {}))
        full.pop("LEADHAND_CRASH_AT", None)
        if crash:
            full["LEADHAND_CRASH_AT"] = crash
        return subprocess.run([sys.executable, "-B", str(CLI), *map(str, args)], env=full, cwd=self.tmp,
                              input=stdin, capture_output=True, text=True, timeout=120)

    # -- cleanup --------------------------------------------------------------

    def kill_leftovers(self) -> None:
        for p in self.proxies:
            if p.poll() is None:
                p.kill()
            with contextlib.suppress(Exception):
                p.communicate(timeout=10)
        if self.run_dir is None:
            return
        for runner in self.run_dir.glob("workers/*/runner.json"):
            with contextlib.suppress(ValueError, OSError):
                data = json.loads(runner.read_text())
                for who in ("runner", "codex"):
                    rec = data.get(who) or {}
                    if procs.same_process(rec.get("pid"), rec.get("start")):
                        with contextlib.suppress(ProcessLookupError, PermissionError):
                            os.kill(rec["pid"], signal.SIGKILL)
