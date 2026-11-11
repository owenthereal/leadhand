"""Codex dispatch: a detached runner owns each Codex worker.

`leadhand dispatch` (the foreground command the `leadhand:codex` proxy runs)
starts or attaches to a runner and waits for its result. The runner is a
separate session (`start_new_session`), so it outlives the foreground command,
the proxy and the supervisor; it starts Codex in a new process group as its own
child, enforces the step and time limits, charges usage, appends the attempt
record and writes `result.json` last.

Worker folder `<run>/workers/<dispatch_id>/`:

    request.json     the DispatchRequest (written once by `dispatch`)
    spawn.json       pid and start time of the runner `dispatch` spawned
    runner.json      the runner's own pid/pgid/start, then the launch intent, then Codex's
                     pid/pgid/start (written by the runner)
    heartbeat        touched by the runner every HEARTBEAT_EVERY wall-clock seconds
    procs.json       Codex's live descendants (pid, pgid, start), refreshed while it runs
    stream.jsonl     Codex's `--json` stdout; codex.stderr its stderr
    last-message.md  Codex's `-o` file (the reply)
    reply.raw.md     the reply, kept when it has no parseable status line
    cancel           cancel request (atomic, with the requester's token); stopping.json
                     the runner's acknowledgement
    result.json      the final result (written last)
    runner.out/.err  the runner's own output
    owner.lock       flock held around each ownership decision and each cancel
    runner.lock      flock a runner holds for its whole life (held = a runner is alive)
    gh-config/       the empty GH_CONFIG_DIR the worker gets
    superseded/<n>/  the files of a generation that was cancelled and replaced

Runtime findings applied (probes.md, Tasks 3 and 4):
- Steps are `item.completed` events whose item type is in STEP_ITEM_TYPES.
- Resume is `codex exec <same options> resume <thread_id> -`; the thread ID is
  `thread.started.thread_id`; `-m`/`-c` are honoured on resume.
- Network access is always passed explicitly; an explicit false wins over user config.
- Credentials: Codex 0.167's default environment policy passes KEY/TOKEN names
  through, so removing them from the process environment (`worker_env`) is what
  keeps them away; the blocking variables are also passed through
  `shell_environment_policy.set`, which survives `inherit = "core"` configs.
- No compaction setting is passed: Codex compaction is unverified.
- Stopping: Codex runs tool commands and helper processes in their own process
  groups as its children. Before each signal step the stopper snapshots Codex's
  descendants (pid, pgid, start time) and keeps the union; SIGINT goes to
  Codex's group, SIGTERM after SIGNAL_STEP to Codex's group and every recorded
  group, SIGKILL after SIGNAL_STEP more. Stopped means Codex is gone and no
  recorded process is alive. Nothing is signalled unless its pid and start time
  match the record.
"""
from __future__ import annotations

import contextlib
import fcntl
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
import uuid
from dataclasses import asdict, dataclass, fields
from pathlib import Path
from types import MappingProxyType
from typing import Any, Iterable, Mapping

from leadhand import procs
from leadhand.briefs import parse_report_head, worker_prompt
from leadhand.gitops import GitError, git_common_dir
from leadhand.state import (utc_now, append_attempt, crash_point, read_attempts, run_paths, state_lock,
                            write_json_atomic)
from leadhand.usage import CODEX_STREAM_SOURCE, UNKNOWN, Usage, charge, codex_usage_from_stream, estimate_cost

STEP_ITEM_TYPES: frozenset[str] = frozenset({"command_execution", "file_change"})
CREDENTIAL_VARS = ("GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN",
                   "SSH_AUTH_SOCK", "GIT_ASKPASS", "SSH_ASKPASS")
# The fixed blocking variables. `blocking_env` adds GH_CONFIG_DIR=<empty dir> and the
# GIT_CONFIG_COUNT/KEY/VALUE entries that reset credential.helper after any existing ones.
BLOCKING_ENV: Mapping[str, str] = MappingProxyType({"GIT_SSH_COMMAND": "false", "GIT_TERMINAL_PROMPT": "0"})
CREDENTIAL_HELPER_RESET = ("credential.helper", "")
# Appended to GIT_CONFIG_PARAMETERS (command-line config, read after the GIT_CONFIG_*
# entries): an empty core.askPass stops Git running an askpass program named in the
# user's config, and also stops it falling back to SSH_ASKPASS.
ASKPASS_RESET = "'core.askPass='"

FOREGROUND_WAIT = 540.0     # the proxy's Bash call is capped at 10 minutes
EXIT_STILL_RUNNING = 75
HEARTBEAT_EVERY = 1.0       # wall-clock seconds between heartbeat touches
HEARTBEAT_STALE = 60.0
HUNG_RECHECK = 5.0          # a stale heartbeat must stay unchanged this long before "hung"
CANCEL_TIMEOUT = 30.0
CANCEL_FINISH_MARGIN = 2.0  # of a cancel's deadline, kept for charging and writing the result
RUNNER_LOCK_RETRY = 2.0
SIGNAL_STEP = 5.0           # SIGINT -> SIGTERM -> SIGKILL spacing
KILL_RETRY = 5.0            # after SIGKILL, re-send this often until confirmed
CANCEL_ACK = 5.0            # a runner that does not acknowledge a cancel this fast is hung
SPAWN_GRACE = 60.0          # a spawned runner that has not written runner.json yet counts as starting
POLL = 0.1
SNAPSHOT_EVERY = 1.0
# Roles that share another role's [limits] entry.
LIMITS_ROLE: Mapping[str, str] = MappingProxyType({
    "implementer_hard": "implementer", "reviewer_hard": "reviewer", "final_reviewer": "reviewer"})
CLI_PATH = Path(__file__).resolve().parents[2] / "leadhand"

REQUEST, SPAWN, RUNNER, HEARTBEAT, PROCS = "request.json", "spawn.json", "runner.json", "heartbeat", "procs.json"
STREAM, STDERR, LAST, RAW = "stream.jsonl", "codex.stderr", "last-message.md", "reply.raw.md"
CANCEL, STOPPING, RESULT, FINALIZE_LOCK = "cancel", "stopping.json", "result.json", "finalize.lock"
OWNER_LOCK, RUNNER_LOCK = "owner.lock", "runner.lock"
RUNNER_OUT, RUNNER_ERR, GH_DIR, SUPERSEDED = "runner.out", "runner.err", "gh-config", "superseded"
COMMIT_INTENT = "commit.json"
GENERATION_FILES = (SPAWN, RUNNER, HEARTBEAT, PROCS, STREAM, STDERR, LAST, RAW, CANCEL, STOPPING, RESULT,
                    COMMIT_INTENT,
                    RUNNER_OUT, RUNNER_ERR)

# Codex's workspace-write sandbox keeps the Git directory read-only even when it is an
# --add-dir (end-to-end finding, ruling R39), so Codex workers never commit: the runner
# commits their work for these kinds once Codex is confirmed stopped.
COMMIT_KINDS = frozenset({"implement", "fix", "continue", "answer", "final_fix"})
TRAILER = "Leadhand-Dispatch"
COMMIT_TIMEOUT = 120.0
_GIT_OPERATIONS = ("MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "BISECT_LOG", "rebase-merge", "rebase-apply")
_USAGE_LIMIT = re.compile(r"usage[\s_-]*limit", re.IGNORECASE)
_STATUS_BLOCKED = re.compile(r"^[\s*_`#>]*status[\s*_`]*:[\s*_`]*blocked\b", re.IGNORECASE)
_ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
_REPO_VARS = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY")
_SPAWNED: list[subprocess.Popen] = []
_RUNNER_LOCK_FD: int | None = None


@dataclass(frozen=True)
class DispatchRequest:
    model: str
    effort: str
    role: str
    brief: Path
    report: Path
    cwd: Path
    run_dir: Path
    card: int | None
    attempt: int
    dispatch_id: str
    steps: int
    minutes: int
    network: bool
    writable_dirs: tuple[Path, ...]
    resume: str | None = None
    codex_bin: str = "codex"
    kind: str | None = None        # the dispatch kind; COMMIT_KINDS get their work committed
    branch: str | None = None      # the branch the work must be committed on


@dataclass(frozen=True)
class DispatchResult:
    status: str
    blocked_by: str
    session_id: str | None
    reply: str
    steps: int
    usage: Usage
    exit_reason: str     # completed | step_limit | time_limit | usage_limit | cancelled | error | interrupted


class DispatchError(RuntimeError):
    pass


class DispatchStillRunning(DispatchError):
    """The runner has not finished within the foreground wait; run the same command again."""


# --------------------------------------------------------------------------
# Requests and results on disk

def request_to_json(req: DispatchRequest) -> dict:
    d = asdict(req)
    for key in ("brief", "report", "cwd", "run_dir"):
        d[key] = str(d[key])
    d["writable_dirs"] = [str(p) for p in req.writable_dirs]
    return d


def request_from_json(d: Mapping[str, Any]) -> DispatchRequest:
    d = dict(d)
    for key in ("brief", "report", "cwd", "run_dir"):
        d[key] = Path(d[key])
    d["writable_dirs"] = tuple(Path(p) for p in d.get("writable_dirs") or ())
    return DispatchRequest(**{f.name: d[f.name] for f in fields(DispatchRequest) if f.name in d})


def _read_json(path: Path) -> dict | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def usage_dict(u: Usage) -> dict | None:
    return asdict(u) if u.known else None


def _usage_from(d: Mapping[str, Any] | None) -> Usage:
    if not d:
        return UNKNOWN
    names = {f.name for f in fields(Usage)}
    return Usage(**{k: v for k, v in d.items() if k in names})


def _result_from(d: Mapping[str, Any]) -> DispatchResult:
    return DispatchResult(status=d["status"], blocked_by=d.get("blocked_by") or "",
                          session_id=d.get("session_id"), reply=d.get("reply") or "",
                          steps=int(d.get("steps") or 0), usage=_usage_from(d.get("usage")),
                          exit_reason=d["exit_reason"])


def load_commit(wd: Path) -> dict:
    """The runner's commit outcome from result.json: committed, changes, commit_error,
    commit_skipped (each None when absent)."""
    d = _read_json(Path(wd) / RESULT) or {}
    return {k: d.get(k) for k in ("committed", "changes", "commit_error", "commit_skipped")}


def load_result(wd: Path) -> DispatchResult | None:
    d = _read_json(wd / RESULT)
    return _result_from(d) if d else None


def _load_request(wd: Path) -> DispatchRequest:
    d = _read_json(wd / REQUEST)
    if d is None:
        raise DispatchError(f"no request.json in {wd}")
    return request_from_json(d)


def _run_id(run_dir: Path) -> str:
    meta = _read_json(run_paths(run_dir).run_json) or {}
    return str(meta.get("run_id") or Path(run_dir).name)


def minute_seconds() -> float:
    if os.environ.get("LEADHAND_TESTING") == "1" and os.environ.get("LEADHAND_MINUTE_SECONDS"):
        return float(os.environ["LEADHAND_MINUTE_SECONDS"])
    return 60.0


def _touch(path: Path) -> None:
    with open(path, "a", encoding="utf-8"):
        pass
    os.utime(path)


def _mtime(path: Path) -> float | None:
    try:
        return path.stat().st_mtime
    except FileNotFoundError:
        return None


# --------------------------------------------------------------------------
# Environment and arguments

def blocking_env(parent: Mapping[str, str], *, gh_config_dir: Path) -> dict[str, str]:
    """The variables that block credential use: BLOCKING_ENV, an empty GH_CONFIG_DIR,
    every GIT_CONFIG_* entry (the parent's, kept in order, then credential.helper reset
    to empty: Git's "clear the helper list"), and GIT_CONFIG_PARAMETERS with the
    parent's entries followed by an empty core.askPass. The whole set is also passed
    through `shell_environment_policy.set`, so the count and its entries always agree."""
    out = dict(BLOCKING_ENV)
    out["GH_CONFIG_DIR"] = str(gh_config_dir)
    try:
        count = max(0, int(parent.get("GIT_CONFIG_COUNT", "0") or "0"))
    except ValueError:
        count = 0
    for i in range(count):
        out[f"GIT_CONFIG_KEY_{i}"] = parent.get(f"GIT_CONFIG_KEY_{i}", "")
        out[f"GIT_CONFIG_VALUE_{i}"] = parent.get(f"GIT_CONFIG_VALUE_{i}", "")
    out[f"GIT_CONFIG_KEY_{count}"], out[f"GIT_CONFIG_VALUE_{count}"] = CREDENTIAL_HELPER_RESET
    out["GIT_CONFIG_COUNT"] = str(count + 1)
    params = (parent.get("GIT_CONFIG_PARAMETERS") or "").strip()
    out["GIT_CONFIG_PARAMETERS"] = f"{params} {ASKPASS_RESET}" if params else ASKPASS_RESET
    return out


def worker_env(parent: Mapping[str, str], *, run_id: str, gh_config_dir: Path) -> dict[str, str]:
    env = {k: v for k, v in parent.items() if k not in CREDENTIAL_VARS}
    env["LEADHAND_RUN_ID"] = run_id
    env.update(blocking_env(parent, gh_config_dir=gh_config_dir))
    return env


def _toml_string(value: str) -> str:
    # A JSON string (without forced ASCII escapes) is a valid TOML basic string.
    return json.dumps(str(value), ensure_ascii=False)


def codex_argv(req: DispatchRequest, last_message: Path, git_common: Path,
               env_overrides: Mapping[str, str]) -> list[str]:
    """The `codex exec` command; the prompt goes on stdin (`-`)."""
    argv = [req.codex_bin, "exec", "-m", req.model,
            "-c", f"model_reasoning_effort={_toml_string(req.effort)}",
            "-c", 'approval_policy="never"',
            "-c", f"sandbox_workspace_write.network_access={'true' if req.network else 'false'}",
            "-C", str(req.cwd),
            "-s", "read-only" if req.role == "scout" else "workspace-write",
            "--add-dir", str(git_common)]
    for d in req.writable_dirs:
        argv += ["--add-dir", str(d)]
    for name, value in env_overrides.items():
        if not _ENV_NAME.match(name):
            raise ValueError(f"not an environment variable name: {name!r}")
        argv += ["-c", f"shell_environment_policy.set.{name}={_toml_string(value)}"]
    argv += ["--json", "-o", str(last_message)]
    if req.resume:
        if req.resume.startswith("-"):
            raise ValueError(f"bad session id {req.resume!r}")
        argv += ["resume", req.resume]
    argv.append("-")
    return argv


# --------------------------------------------------------------------------
# The stream

class _Stream:
    """Reads stream.jsonl incrementally; only complete lines are parsed."""

    def __init__(self, path: Path) -> None:
        self.path, self.offset, self.buf = path, 0, b""
        self.events: list[dict] = []
        self.steps = 0
        self.thread_id: str | None = None
        self.turn_open = False      # a turn.started not (yet) followed by its turn.completed

    def poll(self) -> None:
        try:
            with open(self.path, "rb") as f:
                f.seek(self.offset)
                data = f.read()
        except FileNotFoundError:
            return
        self.offset += len(data)
        self.buf += data
        *lines, self.buf = self.buf.split(b"\n")
        for line in lines:
            try:
                ev = json.loads(line.decode("utf-8", "replace"))
            except ValueError:
                continue
            if isinstance(ev, dict):
                self._add(ev)

    def _add(self, ev: dict) -> None:
        self.events.append(ev)
        kind = ev.get("type")
        if kind == "turn.started":
            self.turn_open = True
        elif kind == "turn.completed":
            self.turn_open = False
        if kind == "thread.started" and isinstance(ev.get("thread_id"), str) and self.thread_id is None:
            self.thread_id = ev["thread_id"]
        elif kind == "item.completed":
            item = ev.get("item")
            if isinstance(item, dict) and item.get("type") in STEP_ITEM_TYPES:
                self.steps += 1

    def last_agent_message(self) -> str:
        for ev in reversed(self.events):
            item = ev.get("item")
            if ev.get("type") == "item.completed" and isinstance(item, dict) \
                    and item.get("type") == "agent_message" and isinstance(item.get("text"), str):
                return item["text"]
        return ""

    def error_texts(self) -> list[str]:
        out = []
        for ev in self.events:
            if ev.get("type") == "error" and isinstance(ev.get("message"), str):
                out.append(ev["message"])
            err = ev.get("error")
            if ev.get("type") == "turn.failed" and isinstance(err, dict) and isinstance(err.get("message"), str):
                out.append(err["message"])
        return out


# --------------------------------------------------------------------------
# Processes: recording, signalling, confirming

class _Tracker:
    """Codex and its recorded descendants, each as {pid, pgid, start}."""

    def __init__(self, wd: Path, codex: dict | None, descendants: Iterable[dict] = (),
                 proc: subprocess.Popen | None = None) -> None:
        self.wd, self.codex, self.proc = wd, codex, proc
        self.live: dict[tuple[int, str], dict] = {}
        self.seen: dict[tuple[int, str], dict] = {}
        for d in descendants:
            if d.get("pid") and d.get("start"):
                self._add(d)
        self._saved: list | None = None

    @classmethod
    def load(cls, wd: Path) -> "_Tracker":
        runner = _read_json(wd / RUNNER) or {}
        saved = _read_json(wd / PROCS) or {}
        return cls(wd, runner.get("codex"), saved.get("descendants") or ())

    def _add(self, rec: dict) -> None:
        key = (int(rec["pid"]), str(rec["start"]))
        self.live.setdefault(key, rec)
        self.seen.setdefault(key, rec)

    def codex_alive(self) -> bool:
        if self.proc is not None:
            # Our own unreaped child: its pid (and group) cannot be reused until we reap
            # it, so it is identified even when its start time could not be read.
            return self.proc.poll() is None
        c = self.codex
        return bool(c) and procs.same_process(c.get("pid"), c.get("start"))

    def _verified(self, t: dict) -> bool:
        if t is self.codex and self.proc is not None:
            return self.proc.poll() is None
        return procs.same_process(t.get("pid"), t.get("start"))

    def snapshot(self) -> None:
        if self.codex_alive():
            for p in procs.descendants(int(self.codex["pid"])):
                self._add(p.record())
        self.live = {k: v for k, v in self.live.items() if procs.same_process(*k)}
        current = sorted(self.live.values(), key=lambda r: r["pid"])
        if current != self._saved:
            with contextlib.suppress(OSError):
                write_json_atomic(self.wd / PROCS, {"descendants": current})
            self._saved = current

    def alive(self) -> list[dict]:
        out = [self.codex] if self.codex_alive() else []
        return out + [r for k, r in self.live.items() if procs.same_process(*k)]

    def signal(self, sig: int, *, codex_only: bool) -> None:
        targets = [self.codex] if self.codex_alive() else []
        if not codex_only:
            targets += list(self.live.values())
        own = {os.getpgid(0), 0, 1}
        groups: set[int] = set()
        for t in targets:   # groups whose leader is a verified recorded process
            pid, pgid = int(t["pid"]), int(t["pgid"])
            if pid == pgid and pgid not in own and pgid not in groups and self._verified(t):
                with contextlib.suppress(ProcessLookupError, PermissionError):
                    os.killpg(pgid, sig)
                groups.add(pgid)
        for t in targets:   # anything else: the process itself, verified
            pid = int(t["pid"])
            if int(t["pgid"]) not in groups and pid != os.getpid() and self._verified(t):
                with contextlib.suppress(ProcessLookupError, PermissionError):
                    os.kill(pid, sig)

    def wait_gone(self, seconds: float, beat: Path | None = None) -> bool:
        deadline = time.monotonic() + seconds
        last_snap = 0.0
        while True:
            now = time.monotonic()
            if now - last_snap >= 0.25:
                self.snapshot()
                last_snap = now
            if not self.alive():
                return True
            if now >= deadline:
                return False
            if beat is not None:
                with contextlib.suppress(OSError):
                    if time.time() - (_mtime(beat) or 0) >= HEARTBEAT_EVERY:
                        _touch(beat)
            time.sleep(0.05)

    def stop(self, *, deadline: float | None = None, beat: Path | None = None) -> list[dict]:
        """The ladder. Returns what is still alive (empty when stopped is confirmed).
        With a deadline, gives up there; without one, re-sends SIGKILL until confirmed."""
        def left() -> float:
            return SIGNAL_STEP if deadline is None else max(0.0, min(SIGNAL_STEP, deadline - time.monotonic()))

        self.snapshot()
        if not self.alive():
            return []
        if self.codex_alive():
            self.signal(signal.SIGINT, codex_only=True)
            if self.wait_gone(left(), beat):
                return []
        self.snapshot()
        if self.alive():
            self.signal(signal.SIGTERM, codex_only=False)
            if self.wait_gone(left(), beat):
                return []
        while True:
            self.snapshot()
            self.signal(signal.SIGKILL, codex_only=False)
            if self.wait_gone(KILL_RETRY if deadline is None else left(), beat):
                return []
            if deadline is not None and time.monotonic() >= deadline:
                return self.alive()


# --------------------------------------------------------------------------
# Finalizing: classify, charge, record the attempt, write result.json

@contextlib.contextmanager
def _finalize_lock(wd: Path):
    fd = os.open(wd / FINALIZE_LOCK, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def has_status_line(reply: str) -> bool:
    if parse_report_head(reply) != ("Status", "BLOCKED", ""):
        return True
    first = next((ln for ln in reply.splitlines() if ln.strip()), "")
    return bool(_STATUS_BLOCKED.match(first))


def _classify(stopped_for: str | None, codex_exit: int | None, reply: str,
              errors: list[str], stderr: str) -> tuple[str, str, str, bool]:
    """(status, blocked_by, exit_reason, keep_raw)."""
    if stopped_for == "error":
        return "BLOCKED", "environment", "error", False    # the runner itself failed
    if stopped_for:
        return "SPENT", "", stopped_for, False
    clean = codex_exit == 0 and bool(reply.strip()) and has_status_line(reply)
    # A usage-limit message counts only when Codex did not finish cleanly with a status:
    # a READY reply after a clean exit stands, whatever warnings came before it.
    if not clean and (any(_USAGE_LIMIT.search(t) for t in errors) or _USAGE_LIMIT.search(stderr)):
        return "BLOCKED", "usage limit", "usage_limit", False
    if codex_exit is not None and codex_exit < 0 and not reply.strip():
        return "SPENT", "", "interrupted", False    # killed by a signal leadhand did not send
    if reply.strip():
        if not has_status_line(reply):
            return "BLOCKED", "", "completed", True
        key, value, blocked_by = parse_report_head(reply)
        # A reviewer's or verifier's reply leads with its Verdict/Result: the work finished.
        return (value if key == "Status" else "READY"), blocked_by, "completed", False
    if codex_exit == 0:
        return "BLOCKED", "", "completed", True
    return "BLOCKED", "", "error", True


def _charge_usage(req: DispatchRequest, wd: Path, *, thread: str | None, launched: bool, generation: int,
                  stopped: bool, stream: _Stream) -> tuple[Usage, list[str], str]:
    """(usage, charge_ids, source). Session file first, then the stream, else unknown."""
    if not launched:
        return Usage(), [], "none"     # Codex never started: nothing was used
    paths = run_paths(req.run_dir)
    suffix = f":g{generation}" if generation > 1 else ""
    if thread:
        cid, key = f"{req.dispatch_id}:codex:{thread}{suffix}", thread
    else:
        cid, key = f"{req.dispatch_id}:codex:unknown{suffix}", f"dispatch:{req.dispatch_id}"
    # charges.jsonl is the source of truth: when the session file is missing, the
    # charge itself is taken from the stream (source "codex-stream"), so it and the
    # attempt record agree. UNKNOWN only when neither has usage.
    c = charge(paths, charge_id=cid, source="codex", key=key, dispatch_id=req.dispatch_id,
               role=req.role, path_or_thread=thread, possibly_incomplete=stopped,
               fallback=codex_usage_from_stream(stream.events))
    if not c.usage.known:
        return UNKNOWN, [cid], "unknown"
    return c.usage, [cid], ("stream" if c.source == CODEX_STREAM_SOURCE else "session")


def _commit_subject(req: DispatchRequest) -> str:
    """The first line of the report's `## Summary`, else the card title from the brief."""
    subject = ""
    try:
        lines = req.report.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        lines = []
    for i, line in enumerate(lines):
        if line.strip().lower() == "## summary":
            subject = next((ln.strip() for ln in lines[i + 1:] if ln.strip()), "")
            if subject.startswith("#"):
                subject = ""
            break
    if not subject:
        try:
            brief = req.brief.read_text(encoding="utf-8", errors="replace")
        except OSError:
            brief = ""
        m = re.search(r"^## Card \d+: (.+)$", brief, re.MULTILINE)
        subject = m.group(1).strip() if m else f"leadhand dispatch {req.dispatch_id}"
    prefix = f"{req.kind} card {req.card}" if req.card is not None else f"{req.kind}"
    text = f"{prefix}: {subject}"
    return text if len(text) <= 100 else text[:99].rstrip() + "…"


def _git(ws: Path, *args: str, timeout: float = COMMIT_TIMEOUT) -> subprocess.CompletedProcess[str]:
    env = {k: v for k, v in os.environ.items() if k not in _REPO_VARS}
    env["GIT_TERMINAL_PROMPT"] = "0"
    return subprocess.run(["git", *args], cwd=ws, env=env, capture_output=True, text=True,
                          stdin=subprocess.DEVNULL, timeout=timeout)


class _CommitProblem(Exception):
    pass


def _commit_work(wd: Path, req: DispatchRequest, *, deadline: float | None = None) -> dict:
    """Commit what the Codex worker left in the workspace, on `req.branch` (ruling R39).

    Returns {"committed": sha or None, "changes": bool} plus "commit_skipped" (why nothing
    was committed) or "commit_error". Two-phase: commit.json records the intent and HEAD
    before; a restart that finds HEAD moved to a commit carrying this dispatch's trailer
    does not commit again. Never commits off the expected branch or during a Git
    operation. `deadline` (time.monotonic) bounds every Git call, for a cancel."""
    if req.kind not in COMMIT_KINDS or not req.branch:
        return {"committed": None, "changes": False,
                "commit_skipped": f"{req.kind or 'this'} dispatch does not commit"}
    ws = Path(req.cwd)

    def run(*args: str, ok: bool = True) -> subprocess.CompletedProcess[str]:
        timeout = COMMIT_TIMEOUT
        if deadline is not None:
            timeout = min(timeout, deadline - time.monotonic())
            if timeout <= 0:
                raise _CommitProblem("no time left before the cancel deadline")
        try:
            r = _git(ws, *args, timeout=timeout)
        except subprocess.TimeoutExpired:
            raise _CommitProblem(f"git {args[0]} timed out"
                                 + (" before the cancel deadline" if deadline is not None else "")) from None
        if ok and r.returncode != 0:
            raise _CommitProblem(f"git {' '.join(args)} failed: {(r.stderr or r.stdout).strip()}")
        return r

    changes = False
    try:
        changes = bool(run("status", "--porcelain=v1", "--untracked-files=all").stdout.strip())
        r = run("symbolic-ref", "--quiet", "--short", "HEAD", ok=False)
        current = r.stdout.strip() if r.returncode == 0 else None
        if current != req.branch:
            return {"committed": None, "changes": changes,
                    "commit_skipped": f"workspace is on {current or 'a detached HEAD'}, not {req.branch}"}
        for name in _GIT_OPERATIONS:
            where = run("rev-parse", "--path-format=absolute", "--git-path", name).stdout.strip()
            if not where:
                raise _CommitProblem(f"git rev-parse --git-path {name} gave no path")
            if Path(where).exists():
                return {"committed": None, "changes": changes,
                        "commit_skipped": f"a Git operation is in progress ({name})"}
        head_now = run("rev-parse", "--verify", "HEAD", ok=False).stdout.strip()
        intent = _read_json(wd / COMMIT_INTENT)
        if intent is not None and head_now and head_now != intent.get("head_before"):
            body = run("log", "-1", "--format=%B", "HEAD").stdout
            if f"{TRAILER}: {req.dispatch_id}" in body.splitlines():
                return {"committed": head_now, "changes": changes}    # committed before a restart
        if not changes:
            return {"committed": None, "changes": False, "commit_skipped": "no changes"}
        if intent is None:
            write_json_atomic(wd / COMMIT_INTENT, {"at": utc_now(), "branch": req.branch, "head_before": head_now})
        run("add", "-A")
        # Unsigned (signing may prompt) and without hooks: this is leadhand's own bookkeeping.
        run("-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "--no-verify",
            "-m", _commit_subject(req), "-m", f"{TRAILER}: {req.dispatch_id}")
        return {"committed": run("rev-parse", "--verify", "HEAD").stdout.strip(), "changes": True}
    except _CommitProblem as e:
        return {"committed": None, "changes": changes, "commit_error": str(e)}
    except OSError as e:
        return {"committed": None, "changes": changes, "commit_error": str(e)}


def commit_failed(result: Mapping[str, Any]) -> str | None:
    """Why a dispatch's changes were left uncommitted, or None. A result whose worker left
    changes that the runner could not commit (an error, the wrong branch, a Git operation
    in progress) must not pass for finished work: the card would go on without it."""
    if result.get("committed") or not result.get("changes"):
        return None
    return result.get("commit_error") or result.get("commit_skipped") or None


def _finalize(wd: Path, req: DispatchRequest, *, stopped_for: str | None, codex_exit: int | None,
              tracker: _Tracker, started_at: str | None, deadline: float | None = None) -> DispatchResult:
    with _finalize_lock(wd):
        existing = load_result(wd)
        if existing is not None:
            return existing
        runner = _read_json(wd / RUNNER) or {}
        generation = int(runner.get("generation") or 1)
        launch = runner.get("launching") or {}
        # Two-phase launch: an intent without a recorded Codex may still have started one.
        launched = bool(runner.get("codex")) or (bool(launch) and not launch.get("aborted"))
        stream = _Stream(wd / STREAM)
        stream.poll()
        try:
            reply = (wd / LAST).read_text(encoding="utf-8", errors="replace")
        except FileNotFoundError:
            reply = ""
        if not reply.strip():
            reply = stream.last_agent_message()
        try:
            stderr = (wd / STDERR).read_text(encoding="utf-8", errors="replace")
        except FileNotFoundError:
            stderr = ""
        status, blocked_by, exit_reason, keep_raw = _classify(stopped_for, codex_exit, reply,
                                                              stream.error_texts(), stderr)
        if not launched and stopped_for is None:
            blocked_by = "environment"     # Codex could not be started at all
        if keep_raw:
            (wd / RAW).write_text(reply, encoding="utf-8")
        # Usage may be missing a response when Codex was stopped (by leadhand or by a signal
        # from anyone else) or ended without the turn.completed of its last turn (R41b).
        incomplete = launched and (stopped_for is not None or (codex_exit is not None and codex_exit < 0)
                                   or stream.turn_open)
        commit = (_commit_work(wd, req, deadline=deadline) if launched
                  else {"committed": None, "changes": False, "commit_skipped": "Codex never ran"})
        failed = commit_failed(commit)
        if failed:
            # The work is still in the tree but not on the branch: an environment problem.
            worker = (f"\n\nWorker reply ({exit_reason}):\n{reply}" if reply.strip() else "")
            reply = (f"Status: BLOCKED\nBlocked by: environment\n"
                     f"leadhand could not commit the worker's changes on {req.branch}: {failed}{worker}")
            commit["worker_status"], commit["worker_exit_reason"] = status, exit_reason
            status, blocked_by, exit_reason = "BLOCKED", "environment", "error"
        usage, charge_ids, source = _charge_usage(req, wd, thread=stream.thread_id, launched=launched,
                                                  generation=generation, stopped=incomplete,
                                                  stream=stream)
        crash_point("after_charge_before_attempt")
        ended_at = utc_now()
        # Codex models are not priced: estimate_cost gives None for them, so an empty table suffices.
        cost = estimate_cost(usage, req.model, {}) if usage.known else None
        attempt = {
            "dispatch": req.dispatch_id, "run": _run_id(req.run_dir), "card": req.card,
            "attempt": req.attempt, "role": req.role, "vendor": "codex", "model": req.model,
            "effort": req.effort, "worker_id": stream.thread_id, "started_at": started_at or ended_at,
            "ended_at": ended_at, "steps": stream.steps, "charge_ids": charge_ids,
            "usage": usage_dict(usage), "usage_known": usage.known, "usage_source": source,
            "cost_estimate": cost, "exit_reason": exit_reason, "status": status, "blocked_by": blocked_by,
            "generation": generation, "possibly_incomplete": incomplete,
        }
        paths = run_paths(req.run_dir)
        with state_lock(paths):
            if not any(a.get("dispatch") == req.dispatch_id and a.get("generation", 1) == generation
                       for a in read_attempts(paths)):
                append_attempt(paths, attempt)
        stopped = sorted(({"pid": int(r["pid"]), "pgid": int(r["pgid"]), "start": r["start"]}
                          for r in ([tracker.codex] if tracker.codex else []) + list(tracker.seen.values())),
                         key=lambda r: r["pid"])
        result = {
            "dispatch_id": req.dispatch_id, "status": status, "blocked_by": blocked_by,
            "session_id": stream.thread_id, "reply": reply, "steps": stream.steps,
            "usage": usage_dict(usage), "usage_known": usage.known, "usage_source": source,
            "possibly_incomplete": incomplete, "exit_reason": exit_reason, "codex_exit": codex_exit, "charge_ids": charge_ids,
            "generation": generation, "stopped": stopped, "confirmed_at": time.time(),
            "cancel_token": ((_read_json(wd / CANCEL) or {}).get("token")
                             if exit_reason == "cancelled" else None),
            "started_at": started_at, "ended_at": ended_at, **commit,
        }
        write_json_atomic(wd / RESULT, result)
        return _result_from(result)


# --------------------------------------------------------------------------
# The runner

def _feed(pipe, data: bytes) -> None:
    try:
        pipe.write(data)
    except (BrokenPipeError, OSError):
        pass
    finally:
        with contextlib.suppress(OSError):
            pipe.close()


def _take_runner_lock(wd: Path) -> int | None:
    """The runner's lifetime lock. A probe by `_runner_locked` holds it for an instant,
    so a starting runner retries briefly before concluding another runner owns it."""
    fd = os.open(wd / RUNNER_LOCK, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    until = time.monotonic() + RUNNER_LOCK_RETRY
    while True:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return fd
        except BlockingIOError:
            if time.monotonic() >= until:
                os.close(fd)
                return None
            time.sleep(0.02)


def _launch(wd: Path, record: dict, argv: list[str], env: Mapping[str, str], cwd: Path,
            cancelled) -> subprocess.Popen | None:
    """Two-phase launch: record the intent, re-check for a cancel, start Codex in a new
    process group, record it. None when a cancel arrived first (nothing started)."""
    record["launching"] = {"at": utc_now()}
    write_json_atomic(wd / RUNNER, record)
    crash_point("before_worker_launch")
    if cancelled():
        record["launching"]["aborted"] = True
        write_json_atomic(wd / RUNNER, record)
        return None
    try:
        with open(wd / STREAM, "wb") as out, open(wd / STDERR, "wb") as err:
            proc = subprocess.Popen(argv, cwd=cwd, env=dict(env), stdin=subprocess.PIPE, stdout=out,
                                    stderr=err, start_new_session=True, close_fds=True)
    except OSError:
        record["launching"]["aborted"] = True
        write_json_atomic(wd / RUNNER, record)
        raise
    info = procs.process_info(proc.pid)
    record["codex"] = {"pid": proc.pid, "pgid": info.pgid if info else proc.pid,
                       "start": info.start if info else None}
    write_json_atomic(wd / RUNNER, record)
    crash_point("after_worker_launch")
    return proc


def _cancel_deadline(wd: Path) -> float | None:
    """The canceller's deadline (time.monotonic), from the cancel request, less a margin."""
    wall = (_read_json(wd / CANCEL) or {}).get("deadline_wall")
    if not isinstance(wall, (int, float)):
        return None
    return time.monotonic() + (wall - time.time()) - CANCEL_FINISH_MARGIN


def run_runner(worker_dir: Path) -> int:
    """The detached runner's body: launch Codex, watch it, stop it at a limit or a
    cancel, then finalize. Exits 0 once result.json is written; 3 when another runner
    already owns the dispatch (it then touches nothing)."""
    global _RUNNER_LOCK_FD
    wd = Path(worker_dir)
    term = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: term.set())
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    fd = _take_runner_lock(wd)
    if fd is None:
        print(f"leadhand: another runner owns {wd}", file=sys.stderr)
        return 3
    _RUNNER_LOCK_FD = fd               # held until this process exits
    if load_result(wd) is not None:
        return 0
    req = _load_request(wd)
    me = procs.process_info(os.getpid())
    started_at = utc_now()
    superseded = wd / SUPERSEDED
    generation = 1 + (sum(1 for _ in superseded.iterdir()) if superseded.is_dir() else 0)
    record = {"runner": me.record() if me else {"pid": os.getpid(), "pgid": os.getpgid(0), "start": None},
              "codex": None, "generation": generation, "started_wall": time.time(), "started_at": started_at}
    write_json_atomic(wd / RUNNER, record)
    _touch(wd / HEARTBEAT)

    def cancelled() -> bool:
        return term.is_set() or (wd / CANCEL).exists()

    tracker = _Tracker(wd, None)
    if cancelled():
        _finalize(wd, req, stopped_for="cancelled", codex_exit=None, tracker=tracker, started_at=started_at)
        return 0
    try:
        gh_dir = wd / GH_DIR
        gh_dir.mkdir(exist_ok=True)
        run_id = _run_id(req.run_dir)
        env = worker_env(os.environ, run_id=run_id, gh_config_dir=gh_dir)
        overrides = {**blocking_env(os.environ, gh_config_dir=gh_dir), "LEADHAND_RUN_ID": run_id}
        argv = codex_argv(req, wd / LAST, git_common_dir(req.cwd), overrides)
        prompt = worker_prompt(req.role, req.brief, req.report).encode("utf-8")
    except (GitError, OSError, ValueError) as e:
        (wd / STDERR).write_text(f"leadhand: cannot prepare the Codex worker: {e}\n", encoding="utf-8")
        _finalize(wd, req, stopped_for=None, codex_exit=None, tracker=tracker, started_at=started_at)
        return 0

    try:
        proc = _launch(wd, record, argv, env, req.cwd, cancelled)
    except OSError as e:
        (wd / STDERR).write_text(f"leadhand: cannot start {req.codex_bin}: {e}\n", encoding="utf-8")
        _finalize(wd, req, stopped_for=None, codex_exit=None, tracker=tracker, started_at=started_at)
        return 0
    if proc is None:
        _finalize(wd, req, stopped_for="cancelled", codex_exit=None, tracker=tracker, started_at=started_at)
        return 0
    threading.Thread(target=_feed, args=(proc.stdin, prompt), daemon=True).start()
    tracker = _Tracker(wd, record["codex"], proc=proc)

    stream = _Stream(wd / STREAM)
    t0 = time.monotonic()               # the time limit counts time awake
    limit = req.minutes * minute_seconds()
    last_snap = t0
    last_beat = time.time()             # the heartbeat is wall-clock, like its mtime
    stopped_for: str | None = None
    try:
        while proc.poll() is None:
            stream.poll()
            now = time.monotonic()
            if cancelled():
                stopped_for = "cancelled"
            elif stream.steps >= req.steps:
                stopped_for = "step_limit"
            elif now - t0 >= limit:
                stopped_for = "time_limit"
            if stopped_for:
                break
            if time.time() - last_beat >= HEARTBEAT_EVERY:
                _touch(wd / HEARTBEAT)
                last_beat = time.time()
            if now - last_snap >= SNAPSHOT_EVERY:
                tracker.snapshot()
                last_snap = now
            time.sleep(POLL)
    except Exception:
        stopped_for = "error"
        raise_later = sys.exc_info()
    else:
        raise_later = None
    write_json_atomic(wd / STOPPING, {"reason": stopped_for or "finished", "at": utc_now()})
    tracker.stop(beat=wd / HEARTBEAT)      # also clears leftovers after a normal exit
    proc.wait()
    _finalize(wd, req, stopped_for=stopped_for, codex_exit=proc.returncode, tracker=tracker,
              started_at=started_at, deadline=_cancel_deadline(wd) if stopped_for == "cancelled" else None)
    if raise_later:
        raise raise_later[1]
    return 0


# --------------------------------------------------------------------------
# Ownership: who owns a dispatch, cancel, dispatch
#
# Every ownership decision (look at the folder -> cancel -> supersede -> spawn) and
# every cancel runs under the worker folder's owner.lock, held only for that step,
# never while waiting for a runner to finish. A runner holds runner.lock for its
# whole life: a held lock means a live runner, whatever its recorded pid says.

@contextlib.contextmanager
def _owner_lock(wd: Path, deadline: float | None = None):
    fd = os.open(wd / OWNER_LOCK, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    try:
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if deadline is not None and time.monotonic() >= deadline:
                    raise DispatchError(f"another leadhand process is handling {wd.name}") from None
                time.sleep(0.05)
        yield
    finally:
        os.close(fd)


def _runner_locked(wd: Path) -> bool:
    try:
        fd = os.open(wd / RUNNER_LOCK, os.O_RDWR | os.O_CLOEXEC)
    except FileNotFoundError:
        return False
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(fd, fcntl.LOCK_UN)
        return False
    finally:
        os.close(fd)


def _live(rec: Mapping[str, Any] | None) -> dict | None:
    if rec and procs.same_process(rec.get("pid"), rec.get("start")):
        return dict(rec)
    return None


def _starting_runner(wd: Path) -> dict | None:
    """A spawned runner that has not written runner.json yet."""
    return _live(_read_json(wd / SPAWN)) if not (wd / RUNNER).exists() else None


def _runner_alive(wd: Path) -> bool:
    return _runner_locked(wd) or _starting_runner(wd) is not None


def _runner_target(wd: Path) -> dict | None:
    """The runner as a signal target: only when its pid and start time match."""
    runner = _read_json(wd / RUNNER)
    if runner is not None:
        return _live(runner.get("runner"))
    return _live(_read_json(wd / SPAWN))


def _beat(wd: Path) -> float:
    return _mtime(wd / HEARTBEAT) or _mtime(wd / RUNNER) or _mtime(wd / SPAWN) or 0.0


def _owner_state(wd: Path) -> str:
    """running | stale | orphaned | dead | none. `stale`: a live runner whose
    heartbeat is old; `dispatch` looks again before calling it hung."""
    now = time.time()
    if _runner_locked(wd):
        return "stale" if now - _beat(wd) > HEARTBEAT_STALE else "running"
    spawn = _starting_runner(wd)
    if spawn is not None:
        return "running" if now - float(spawn.get("at") or 0) < SPAWN_GRACE else "stale"
    if not (wd / RUNNER).exists():
        return "none"
    if _Tracker.load(wd).alive():
        return "orphaned"
    return "dead"


def runner_state(worker_dir: Path, *, recheck: float | None = None) -> str:
    """For recovery after a relaunch: finished (result.json) | running | hung | orphaned |
    dead | none (no runner was ever recorded). A live runner whose heartbeat is stale is
    looked at again for `recheck` seconds (default HUNG_RECHECK) before it counts as hung:
    after a laptop sleep the runner refreshes it as soon as it runs again."""
    wd = Path(worker_dir)
    if load_result(wd) is not None:
        return "finished"
    if not wd.is_dir():
        return "none"
    state = _owner_state(wd)
    if state == "stale":
        beat, until = _beat(wd), time.monotonic() + (HUNG_RECHECK if recheck is None else recheck)
        while time.monotonic() < until:
            if load_result(wd) is not None:
                return "finished"
            if _beat(wd) != beat:
                return "running"
            time.sleep(POLL)
        state = "hung" if _owner_state(wd) == "stale" else _owner_state(wd)
    if load_result(wd) is not None:
        return "finished"
    return state


def stop_strays(worker_dir: Path, *, timeout: float = CANCEL_TIMEOUT) -> list[dict]:
    """Stop the recorded Codex processes of a dispatch whose runner is gone (a SIGKILLed
    runner leaves Codex's tool and helper processes under init, P14). Returns what is still
    alive after `timeout` (empty when everything is stopped). A live runner is left alone."""
    wd = Path(worker_dir)
    if not (wd / RUNNER).exists() or _runner_alive(wd):
        return []
    tracker = _Tracker.load(wd)
    if not tracker.alive():
        return []
    return tracker.stop(deadline=time.monotonic() + timeout)


def _supersede(wd: Path) -> None:
    runner = _read_json(wd / RUNNER) or {}
    superseded = wd / SUPERSEDED
    superseded.mkdir(exist_ok=True)
    n = int(runner.get("generation") or (1 + sum(1 for _ in superseded.iterdir())))
    dest = superseded / str(n)
    while dest.exists():
        n += 1
        dest = superseded / str(n)
    dest.mkdir()
    for name in GENERATION_FILES:
        with contextlib.suppress(FileNotFoundError):
            os.replace(wd / name, dest / name)


def _spawn_runner(wd: Path) -> subprocess.Popen:
    cmd = [sys.executable, "-B", str(CLI_PATH), "dispatch-runner", str(wd)]
    with open(wd / RUNNER_OUT, "ab") as out, open(wd / RUNNER_ERR, "ab") as err:
        p = subprocess.Popen(cmd, cwd=wd, stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                             start_new_session=True, close_fds=True)
    info = procs.process_info(p.pid)
    write_json_atomic(wd / SPAWN, {"pid": p.pid, "start": info.start if info else None, "at": time.time()})
    _SPAWNED.append(p)
    return p


def _reap() -> None:
    for p in list(_SPAWNED):
        if p.poll() is not None:
            _SPAWNED.remove(p)


def _cancel_locked(wd: Path, *, deadline: float, token: str | None) -> DispatchResult:
    """`cancel`'s body; the caller holds owner.lock. A None token keeps a cancel
    request that is already there."""
    done = load_result(wd)
    if done is not None:
        return done
    req = _load_request(wd)
    if token is not None or not (wd / CANCEL).exists():
        write_json_atomic(wd / CANCEL, {"requested_at": utc_now(), "by_pid": os.getpid(), "token": token,
                                        "deadline_wall": time.time() + (deadline - time.monotonic())})
    target = _runner_target(wd)
    if target and _runner_alive(wd):
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.kill(int(target["pid"]), signal.SIGTERM)
    crash_point("after_cancel_request")
    ack_by = time.monotonic() + CANCEL_ACK
    while True:
        _reap()
        done = load_result(wd)
        if done is not None:
            return done
        if not _runner_alive(wd):
            break
        if not (wd / STOPPING).exists() and time.monotonic() >= ack_by:
            # Alive but not acknowledging: hung. Kill it (verified), then stop Codex here.
            target = _runner_target(wd)
            if target:
                with contextlib.suppress(ProcessLookupError, PermissionError):
                    os.kill(int(target["pid"]), signal.SIGKILL)
        if time.monotonic() >= deadline:
            raise DispatchError(f"cancel timed out; the runner of {wd.name} is still alive"
                                + (f" (pid {target['pid']})" if target else ""))
        time.sleep(POLL)
    # The runner is gone: stop what it left, then finalize.
    tracker = _Tracker.load(wd)
    left = tracker.stop(deadline=deadline)
    if left:
        raise DispatchError("cancel timed out; still alive: "
                            + ", ".join(f"pid {r['pid']} (group {r['pgid']})" for r in left))
    started_at = (_read_json(wd / RUNNER) or {}).get("started_at")
    return _finalize(wd, req, stopped_for="cancelled", codex_exit=None, tracker=tracker, started_at=started_at,
                     deadline=deadline - CANCEL_FINISH_MARGIN)


def cancel(worker_dir: Path, *, timeout: float = 30.0) -> DispatchResult:
    """Stop a dispatch, confirm that Codex and its processes are gone, finalize.
    Raises DispatchError after `timeout` naming what is still alive."""
    wd = Path(worker_dir)
    deadline = time.monotonic() + timeout
    done = load_result(wd)
    if done is not None:
        return done
    with _owner_lock(wd, deadline):
        return _cancel_locked(wd, deadline=deadline, token=uuid.uuid4().hex)


def _write_request(wd: Path, req: DispatchRequest) -> None:
    data = request_to_json(req)
    existing = _read_json(wd / REQUEST)
    if existing is None:
        write_json_atomic(wd / REQUEST, data)
    elif existing != data:
        changed = sorted(k for k in set(existing) | set(data) if existing.get(k) != data.get(k))
        raise DispatchError(f"dispatch {req.dispatch_id} was requested with different "
                            f"{', '.join(changed)}; use a new dispatch id")


def write_request(req: DispatchRequest) -> Path:
    """Write the dispatch's request.json unless it is there (DispatchError if it differs).
    A cancel can then finalize a dispatch whose runner was never started. Returns the worker folder."""
    wd = Path(req.run_dir) / "workers" / req.dispatch_id
    wd.mkdir(parents=True, exist_ok=True)
    with _owner_lock(wd):
        _write_request(wd, req)
    return wd


def dispatch(req: DispatchRequest, *, wait: float | None = None) -> DispatchResult:
    """Start or attach to the dispatch's runner and wait for its result.

    Raises DispatchStillRunning after `wait` seconds (default FOREGROUND_WAIT); the
    runner carries on, and calling again attaches to it."""
    wait = FOREGROUND_WAIT if wait is None else wait
    if not req.dispatch_id or "/" in req.dispatch_id or req.dispatch_id in (".", ".."):
        raise DispatchError(f"bad dispatch id {req.dispatch_id!r}")
    wd = Path(req.run_dir) / "workers" / req.dispatch_id
    wd.mkdir(parents=True, exist_ok=True)
    with _owner_lock(wd):
        _write_request(wd, req)
    deadline = time.monotonic() + wait
    mine: subprocess.Popen | None = None
    suspect: tuple[float, float] | None = None     # (heartbeat mtime, when first seen stale)
    while True:
        _reap()
        done = load_result(wd)
        if done is not None:
            return done
        with _owner_lock(wd):
            done = load_result(wd)
            if done is not None:
                return done
            state = _owner_state(wd)
            if state in ("orphaned", "dead", "none") and (wd / RESULT).exists():
                continue                   # the runner finished and exited between the checks
            if state == "stale":
                # Hung only if the heartbeat stays the same across a second look: after a
                # laptop sleep the runner refreshes it as soon as it runs again.
                beat, now = _beat(wd), time.time()
                if suspect is None or suspect[0] != beat:
                    suspect = (beat, now)
                state = "hung" if now - suspect[1] >= HUNG_RECHECK else "running"
            else:
                suspect = None
            if state != "running" and (wd / CANCEL).exists():
                # A stop was requested (stop-dispatch, run cancel): finish it, never replace.
                return _cancel_locked(wd, deadline=time.monotonic() + CANCEL_TIMEOUT, token=None)
            if state in ("hung", "orphaned"):
                token = uuid.uuid4().hex
                result = _cancel_locked(wd, deadline=time.monotonic() + CANCEL_TIMEOUT, token=token)
                if result.exit_reason != "cancelled" or (_read_json(wd / RESULT) or {}).get("cancel_token") != token:
                    return result          # it finished on its own meanwhile: keep that
                _supersede(wd)
                suspect, state, mine = None, "none", None   # the replaced runner was not a failure
            elif state == "dead":
                if mine is not None:
                    raise DispatchError(f"the runner exited without a result; see {wd / RUNNER_ERR}")
                runner = _read_json(wd / RUNNER) or {}
                launch = runner.get("launching") or {}
                if runner.get("codex") or (launch and not launch.get("aborted")):
                    # It died mid-attempt: record what it used before replacing it.
                    _finalize(wd, req, stopped_for="interrupted", codex_exit=None,
                              tracker=_Tracker.load(wd), started_at=runner.get("started_at"))
                _supersede(wd)
                state = "none"
            if state == "none":
                if mine is not None:       # ours died before even writing runner.json
                    raise DispatchError(f"the runner exited without a result; see {wd / RUNNER_ERR}")
                mine = _spawn_runner(wd)
        if time.monotonic() >= deadline:
            raise DispatchStillRunning(f"dispatch {req.dispatch_id} is still running")
        time.sleep(POLL)


# --------------------------------------------------------------------------
# Commands

def limits_for(config, role: str):
    from leadhand.config import ConfigError
    key = LIMITS_ROLE.get(role, role)
    if key not in config.limits:
        raise ConfigError(f"limits.{key}: not configured (role {role})")
    return config.limits[key]


def request_for(run_dir: Path, dispatch_id: str, *, card: int | None, attempt: int, role: str, model: str,
                effort: str, brief: Path, report: Path, cwd: Path, resume: str | None = None,
                kind: str | None = None, branch: str | None = None) -> DispatchRequest:
    """The DispatchRequest for a dispatch, with limits, network and writable folders from the
    run repo's configuration. `leadhand dispatch` and the engine build requests only here, so
    the request.json both write is the same."""
    from leadhand.config import load_config
    run_dir = Path(run_dir).absolute()
    cwd = Path(cwd).absolute()
    meta = _read_json(run_paths(run_dir).run_json) or {}
    config = load_config(Path(meta.get("repo") or cwd))
    limits = limits_for(config, role)
    writable = tuple(p if p.is_absolute() else cwd / p
                     for p in (Path(d).expanduser() for d in config.codex.writable_dirs))
    return DispatchRequest(model=model, effort=effort, role=role, brief=Path(brief).absolute(),
                           report=Path(report).absolute(), cwd=cwd, run_dir=run_dir, card=card, attempt=attempt,
                           dispatch_id=dispatch_id, steps=limits.steps, minutes=limits.minutes,
                           network=config.codex.network, writable_dirs=writable, resume=resume,
                           kind=kind, branch=branch)


def _request_from_args(args) -> DispatchRequest:
    return request_for(Path(args.run), args.dispatch_id, card=args.card, attempt=args.attempt, role=args.role,
                       model=args.model, effort=args.effort, brief=Path(args.brief), report=Path(args.report),
                       cwd=Path(args.cwd), resume=args.resume, kind=args.kind, branch=args.branch)


def reply_lines(result: DispatchResult) -> list[str]:
    lines = [ln.rstrip() for ln in result.reply.strip().splitlines()]
    if not lines:
        lines = [f"Status: {result.status}"]
        if result.blocked_by:
            lines.append(f"Blocked by: {result.blocked_by}")
        lines.append(f"Stopped: {result.exit_reason}")
    return lines[:15]


def _print_result(result: DispatchResult) -> None:
    for line in reply_lines(result):
        print(line)
    print(f"session: {result.session_id or 'none'}")


def _cmd_dispatch(args) -> int:
    from leadhand.config import ConfigError
    try:
        result = dispatch(_request_from_args(args))
    except DispatchStillRunning:
        print(f"still running: run the same command again (dispatch {args.dispatch_id})", flush=True)
        return EXIT_STILL_RUNNING
    except (DispatchError, ConfigError, ValueError, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    _print_result(result)
    return 0


def _cmd_stop(args) -> int:
    try:
        result = cancel(Path(args.run_dir).absolute() / "workers" / args.dispatch)
    except (DispatchError, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    _print_result(result)
    return 0


def _cmd_runner(args) -> int:
    return run_runner(Path(args.worker_dir))


def add_commands(sub) -> None:
    import argparse
    p = sub.add_parser("dispatch", help="run one Codex worker (started detached) and print its reply")
    p.add_argument("--run", required=True, help="the run directory")
    p.add_argument("--dispatch-id", required=True)
    p.add_argument("--card", type=int, default=None)
    p.add_argument("--attempt", type=int, required=True)
    p.add_argument("--role", required=True)
    p.add_argument("--model", required=True)
    p.add_argument("--effort", required=True)
    p.add_argument("--brief", required=True)
    p.add_argument("--report", required=True)
    p.add_argument("--cwd", required=True, help="the workspace the worker runs in")
    p.add_argument("--resume", default=None, help="Codex session (thread) ID to resume")
    p.add_argument("--kind", default=None, help="the dispatch kind (implement, fix, review, ...)")
    p.add_argument("--branch", default=None, help="the branch the worker's changes are committed on")
    p.set_defaults(func=_cmd_dispatch)

    p = sub.add_parser("stop-dispatch", help="stop a Codex dispatch and record its result")
    p.add_argument("run_dir")
    p.add_argument("--dispatch", required=True)
    p.set_defaults(func=_cmd_stop)

    p = sub.add_parser("dispatch-runner", help=argparse.SUPPRESS)
    p.add_argument("worker_dir")
    p.set_defaults(func=_cmd_runner)
