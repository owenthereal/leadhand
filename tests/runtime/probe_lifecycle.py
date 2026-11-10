#!/usr/bin/env python3
"""Runtime probe for Task 3: worker lifecycle and process ownership.

Runs every step of Task 3 against the real `claude`, `codex` and `tmux` CLIs
and writes the observed facts to DIR/task3-raw.md, with full artifacts under
DIR/task3-artifacts/. Run it in an ordinary terminal, not inside Claude Code:

    python3 tests/runtime/probe_lifecycle.py --out DIR [--only 1,4] [--dry-run]

Standard library only. Every step runs independently; failures are recorded,
not raised. Steps 1-4 drive an interactive `claude --agent
lhprobe:super-lifecycle` in tmux, each with its own fixture repo; steps 5-6 run
`codex exec`. Processes are matched by unique sleep durations (for example
`sleep 120.1<nnnn>`), never by agent_id or parent fields, which are recorded as
observations only. Expect about 15 minutes of wall time.

Internal modes, used by step 5 only (not for direct use):
    probe_lifecycle.py _codex-launcher RUNDIR
    probe_lifecycle.py _codex-runner RUNDIR
"""

import argparse
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import traceback
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
SELF = Path(__file__).resolve()
PLUGIN_DIR = HERE / "probe-plugin"
FAKE_RUNNER = HERE / "fake-runner.sh"
CLAUDE_PROJECTS = Path.home() / ".claude" / "projects"
CODEX_CONFIG = Path.home() / ".codex" / "config.toml"

SUPER_AGENT = "lhprobe:super-lifecycle"
WORKER_AGENT = "lhprobe:cmd-worker"
CODEX_MODEL = "gpt-6-luna"
CODEX_EFFORT = 'model_reasoning_effort="low"'
CODEX_PIN = ["-m", CODEX_MODEL, "-c", CODEX_EFFORT]
CODEX_JSON = ["--json", "--skip-git-repo-check"]

READY_TIMEOUT = 90        # 60 s for first-run dialogs + startup
DISPATCH_TIMEOUT = 150    # message sent -> worker's command running
TURN_TIMEOUT = 150        # message sent -> supervisor's reply
STEP1_KILL_AFTER = 10
STEP1_WATCH = 130
STEP4_KILL_AFTER = 15
STEP4_WATCH = 130         # measured from the runner's start
CODEX_TIMEOUT = 240
CODEX_RUNNER_MAX = 600    # the runner kills Codex's group after this long
LADDER_WAIT = 3
CMD_TIMEOUT = 30          # every short subprocess call

STEPS = {
    1: "Claude subagent at session exit",
    2: "TaskStop",
    3: "Resume a finished worker",
    4: "The real ownership chain",
    5: "Codex child of the runner",
    6: "Codex resume",
}

# First-run dialogs in an interactive session; each is answered with Enter.
DIALOG_PATTERNS = [
    r"trust the files",
    r"Yes, I trust",
    r"Yes, proceed",
    r"Choose the text style",
    r"Press Enter to continue",
    r"Enter to confirm",
    r"Enable auto mode",
    r"Do you want to proceed\?",
]
READY_PATTERN = re.compile(r"for shortcuts|auto mode on|⏵⏵")
GIT_SAFE = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
            "-c", "user.name=probe", "-c", "user.email=probe@example.invalid"]

T0 = time.monotonic()


def progress(msg: str) -> None:
    print(f"[{time.monotonic() - T0:7.1f}s] {msg}", flush=True)


class StepFail(Exception):
    def __init__(self, reason: str, lines: list[str] | None = None):
        super().__init__(reason)
        self.reason = reason
        self.lines = lines or []


# ---------------------------------------------------------------- context


class Ctx:
    def __init__(self, out: Path, dry: bool):
        self.out = out
        self.art = out / "task3-artifacts"
        self.raw = out / "task3-raw.md"
        self.dry = dry
        self.tag = str(os.getpid())
        self.uniq = f"{os.getpid() % 10000:04d}"
        self.sock = f"lhprobe3-{self.tag}"
        self.tmp: Path | None = None
        self.fixtures: dict[str, Path] = {}
        self.cmds: list[tuple[list[str], object]] = []
        self.pgids: list[int] = []
        self.needles: list[str] = []      # any process whose command holds one is ours
        self.codex_sandbox: list[str] = []
        self.tmux_started = False

    def token(self, prefix: str) -> str:
        """A unique sleep duration such as 120.1<nnnn>."""
        return f"{prefix}{self.uniq}"

    def fixture(self, label: str) -> Path:
        if self.dry:
            return Path(f"<tmp>/lhprobe3 XXXX/{label} repo")
        if label not in self.fixtures:
            if self.tmp is None:
                self.tmp = Path(tempfile.mkdtemp(prefix="lhprobe3 ")).resolve()
                self.needles.append(str(self.tmp))
            self.fixtures[label] = make_fixture(self.tmp / f"{label} repo")
        return self.fixtures[label]


def make_fixture(repo: Path) -> Path:
    repo.mkdir(parents=True)
    (repo / "README.md").write_text("probe fixture\n")
    git = ["git", *GIT_SAFE, "-C", str(repo)]
    for args in (["init", "-q", "-b", "main"], ["add", "README.md"],
                 ["commit", "-q", "--no-gpg-sign", "-m", "init"]):
        subprocess.run(git + args, check=True, capture_output=True, timeout=CMD_TIMEOUT,
                       stdin=subprocess.DEVNULL)
    return repo.resolve()


def child_env(extra: dict | None = None) -> dict:
    env = dict(os.environ)
    env.pop("CLAUDECODE", None)
    env.update(extra or {})
    return env


def atomic_write_json(path: Path, obj) -> None:
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(obj, indent=2, default=str) + "\n")
    os.replace(tmp, path)


def read_json(path: Path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return None


def read_jsonl(path: Path) -> list[dict]:
    out = []
    try:
        for line in path.read_text(errors="replace").splitlines():
            if not line.strip():
                continue
            try:
                v = json.loads(line)
                out.append(v if isinstance(v, dict) else {"value": v})
            except ValueError:
                out.append({"unparsed": line})
    except OSError:
        pass
    return out


def wait_until(pred, timeout: float, sess=None, interval: float = 2.0):
    """Poll pred() (after answering any dialog in sess) until truthy or timeout."""
    end = time.monotonic() + timeout
    while True:
        if sess is not None:
            sess.poll()
        v = pred()
        if v or time.monotonic() >= end:
            return v
        time.sleep(interval)


def idle(seconds: float, sess=None) -> None:
    wait_until(lambda: False, seconds, sess)


# ---------------------------------------------------------------- processes


def ps_rows() -> list[dict]:
    p = subprocess.run(["ps", "-axo", "pid=,pgid=,ppid=,stat=,command="],
                       capture_output=True, text=True, timeout=CMD_TIMEOUT)
    rows = []
    for line in p.stdout.splitlines():
        parts = line.split(None, 4)
        if len(parts) < 4:
            continue
        try:
            rows.append({"pid": int(parts[0]), "pgid": int(parts[1]), "ppid": int(parts[2]),
                         "stat": parts[3], "command": parts[4] if len(parts) > 4 else ""})
        except ValueError:
            continue
    return [r for r in rows if r["pid"] != os.getpid() and "Z" not in r["stat"]]


def rows_with(needle: str, rows: list[dict] | None = None) -> list[dict]:
    return [r for r in (ps_rows() if rows is None else rows) if needle in r["command"]]


def sleep_rows(token: str, rows: list[dict] | None = None) -> list[dict]:
    """Processes whose program is `sleep` and whose arguments hold the token."""
    return [r for r in rows_with(token, rows)
            if os.path.basename(r["command"].split()[0]) == "sleep"]


def row_of(pid: int, rows: list[dict] | None = None) -> dict | None:
    return next((r for r in (ps_rows() if rows is None else rows) if r["pid"] == pid), None)


def alive(pid: int) -> bool:
    return row_of(pid) is not None


def brief(rows: list[dict]) -> list[str]:
    return [f"pid={r['pid']} pgid={r['pgid']} ppid={r['ppid']} cmd={r['command'][:120]}"
            for r in rows]


def descendants(root: int, rows: list[dict]) -> list[dict]:
    kids: dict[int, list[dict]] = {}
    for r in rows:
        kids.setdefault(r["ppid"], []).append(r)
    out, todo = [], [root]
    while todo:
        for r in kids.get(todo.pop(), []):
            out.append(r)
            todo.append(r["pid"])
    return out


def kill_group(pgid: int) -> None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError):
            return
        time.sleep(1)


def kill_pid(pid: int, sig=signal.SIGKILL) -> None:
    try:
        os.kill(pid, sig)
    except (ProcessLookupError, PermissionError):
        pass


# ---------------------------------------------------------------- tmux + claude


def tmux(ctx: Ctx, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["tmux", "-L", ctx.sock, *args], capture_output=True, text=True,
                          timeout=CMD_TIMEOUT, stdin=subprocess.DEVNULL)


def claude_argv() -> list[str]:
    return ["env", "-u", "CLAUDECODE",
            "claude", "--plugin-dir", str(PLUGIN_DIR), "--agent", SUPER_AGENT,
            "--permission-mode", "auto", "--model", "haiku", "--effort", "low"]


def tmux_new_argv(ctx: Ctx, name: str, cwd: str, hook_log: str) -> list[str]:
    return ["tmux", "-L", ctx.sock, "start-server", ";", "set", "-g", "remain-on-exit", "on", ";",
            "new-session", "-d", "-s", name, "-c", cwd, "-x", "200", "-y", "50",
            "-e", f"PROBE_LOG={hook_log}", "-e", "LEADHAND_RUN_ID=probe3", *claude_argv()]


class Session:
    """One interactive `claude --agent lhprobe:super-lifecycle` in tmux."""

    def __init__(self, ctx: Ctx, label: str):
        self.ctx = ctx
        self.label = label
        self.name = f"lh3-{label}"
        self.cwd = ctx.fixture(label)
        self.hook_log = ctx.art / f"{label}-hooks.jsonl"
        self.answered: list[str] = []
        self.pane = ""
        self.started = 0.0
        self.sid: str | None = None
        self.sid_source = "not found"

    def start(self) -> None:
        argv = tmux_new_argv(self.ctx, self.name, str(self.cwd), str(self.hook_log))
        self.started = time.time()
        p = subprocess.run(argv, capture_output=True, text=True, timeout=CMD_TIMEOUT,
                           stdin=subprocess.DEVNULL)
        self.ctx.tmux_started = True
        self.ctx.cmds.append((argv, p.returncode))
        if p.returncode != 0:
            raise StepFail("tmux new-session failed", p.stderr.splitlines()[:10])
        progress(f"  {self.label}: claude starting in tmux")
        ok = wait_until(lambda: READY_PATTERN.search(self.pane) or self.dead(),
                        READY_TIMEOUT, self)
        self.save("startup")
        if not ok or self.dead():
            raise StepFail("claude never reached the prompt (dead pane or dialog not passed); "
                           f"dialogs answered: {self.answered or 'none'}",
                           self.pane.rstrip().splitlines()[-10:])
        progress(f"  {self.label}: ready (dialogs answered: {self.answered or 'none'})")

    def capture(self) -> str:
        p = tmux(self.ctx, "capture-pane", "-p", "-t", self.name)
        if p.returncode != 0:
            return f"<capture-pane rc={p.returncode} stderr={p.stderr.strip()}>"
        return p.stdout

    def poll(self) -> str:
        self.pane = self.capture()
        for pat in DIALOG_PATTERNS:
            if re.search(pat, self.pane):
                if self.answered.count(pat) >= 4:
                    break  # give up on a dialog Enter does not pass
                self.answered.append(pat)
                tmux(self.ctx, "send-keys", "-t", self.name, "Enter")
                time.sleep(2)
                self.pane = self.capture()
                break
        return self.pane

    def dead(self) -> bool:
        p = tmux(self.ctx, "display", "-p", "-t", self.name, "#{pane_dead}")
        return p.returncode != 0 or p.stdout.strip() == "1"

    def pane_pid(self) -> int:
        p = tmux(self.ctx, "display", "-p", "-t", self.name, "#{pane_pid}")
        return int(p.stdout.strip())

    def send(self, text: str) -> None:
        for args in (["send-keys", "-t", self.name, "-l", text],
                     ["send-keys", "-t", self.name, "Enter"]):
            p = tmux(self.ctx, *args)
            self.ctx.cmds.append((["tmux", "-L", self.ctx.sock, *args], p.returncode))
            time.sleep(0.5)

    def save(self, suffix: str) -> None:
        self.pane = self.capture()
        (self.ctx.art / f"{self.label}-pane-{suffix}.txt").write_text(self.pane)

    def kill(self) -> None:
        tmux(self.ctx, "kill-session", "-t", self.name)

    # ---- transcripts

    def session_id(self) -> str | None:
        if self.sid:
            return self.sid
        for h in read_jsonl(self.hook_log):
            sid = (h.get("hook_input") or {}).get("session_id")
            if sid:
                self.sid, self.sid_source = sid, "PROBE_LOG hook line"
                return sid
        needle = json.dumps(str(self.cwd))[1:-1]
        cands = []
        for p in CLAUDE_PROJECTS.glob("*/*.jsonl"):
            try:
                st = p.stat()
                born = getattr(st, "st_birthtime", st.st_mtime)
                if born < self.started - 2:
                    continue
                with open(p, errors="replace") as f:
                    head = "".join(f.readline() for _ in range(30))
            except OSError:
                continue
            if needle in head:
                cands.append((born, p))
        if cands:
            self.sid, self.sid_source = min(cands)[1].stem, "transcript scan by cwd"
        return self.sid

    def main_path(self) -> Path | None:
        sid = self.session_id()
        hits = sorted(CLAUDE_PROJECTS.glob(f"*/{sid}.jsonl")) if sid else []
        return hits[0] if hits else None

    def main_records(self) -> list[dict]:
        p = self.main_path()
        return read_jsonl(p) if p else []

    def sub_paths(self) -> list[Path]:
        sid = self.session_id()
        return sorted(CLAUDE_PROJECTS.glob(f"*/{sid}/subagents/*.jsonl")) if sid else []

    def copy_transcripts(self) -> list[str]:
        dest = self.ctx.art / f"{self.label}-transcripts"
        found = []
        for p in ([self.main_path()] if self.main_path() else []) + self.sub_paths():
            dest.mkdir(parents=True, exist_ok=True)
            shutil.copy2(p, dest / p.name)
            found.append(str(p))
        return found


def dispatch_bg_msg(cmd: str, desc: str) -> str:
    return (f"Call the Agent tool once with subagent_type \"{WORKER_AGENT}\", description "
            f"\"{desc}\", run_in_background true, and as prompt exactly the text between <<< and "
            f">>>: <<<{cmd}>>> Then reply with one line: dispatched <the agent ID the tool "
            f"returned>. Do not wait for the worker.")


# ---------------------------------------------------------------- transcript helpers


def content_items(rec: dict) -> list:
    msg = rec.get("message")
    if isinstance(msg, dict):
        c = msg.get("content")
        if isinstance(c, list):
            return [x for x in c if isinstance(x, dict)]
        if isinstance(c, str):
            return [{"type": "text", "text": c}]
    return []


def text_of(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(text_of(c.get("text", c.get("content", ""))) if isinstance(c, dict)
                         else str(c) for c in content)
    return str(content)


def tool_uses(recs: list[dict], start: int = 0) -> list[dict]:
    out = []
    for i, r in enumerate(recs):
        if i >= start and r.get("type") == "assistant":
            for c in content_items(r):
                if c.get("type") == "tool_use":
                    out.append({"idx": i, "id": c.get("id"), "name": c.get("name"),
                                "input": c.get("input") or {}})
    return out


def tool_results(recs: list[dict]) -> dict:
    out = {}
    for r in recs:
        if r.get("type") == "user":
            for c in content_items(r):
                if c.get("type") == "tool_result":
                    out[c.get("tool_use_id")] = text_of(c.get("content", ""))
    return out


def assistant_texts(recs: list[dict], start: int = 0) -> list[str]:
    return [c.get("text", "") for i, r in enumerate(recs)
            if i >= start and r.get("type") == "assistant"
            for c in content_items(r) if c.get("type") == "text"]


def user_index(recs: list[dict], needle: str) -> int | None:
    for i, r in enumerate(recs):
        if r.get("type") == "user" and needle in text_of((r.get("message") or {}).get("content", "")):
            return i
    return None


def summarize(r: dict) -> str:
    parts = []
    for c in content_items(r):
        t = c.get("type")
        if t == "text":
            parts.append(f"text:{c.get('text', '')[:150]!r}")
        elif t == "tool_use":
            parts.append(f"tool_use:{c.get('name')} {json.dumps(c.get('input'))[:150]}")
        elif t == "tool_result":
            parts.append(f"tool_result:{text_of(c.get('content', ''))[:150]!r}")
        else:
            parts.append(str(t))
    return f"{r.get('type')}/{r.get('subtype', '')}: " + " | ".join(parts)


def sub_counts(sess: Session) -> dict:
    return {p.name: len(read_jsonl(p)) for p in sess.sub_paths()}


def hook_commands(sess: Session) -> list[str]:
    return [str(((h.get("hook_input") or {}).get("tool_input") or {}).get("command", ""))
            for h in read_jsonl(sess.hook_log)]


def hook_fields(sess: Session, needle: str) -> list[dict]:
    """agent_id / agent_type of hook lines whose command holds needle (observation only)."""
    out = []
    for h in read_jsonl(sess.hook_log):
        hi = h.get("hook_input") or {}
        if needle in str((hi.get("tool_input") or {}).get("command", "")):
            out.append({k: hi.get(k, "absent") for k in ("agent_id", "agent_type", "session_id")})
    return out


# ---------------------------------------------------------------- step 1


def step1(ctx: Ctx):
    sess = Session(ctx, "step1")
    token = ctx.token("120.1")
    ctx.needles.append(token)
    marker = sess.cwd / "late-marker.txt"
    cmd = f"sleep {token}; echo late >> {shlex.quote(str(marker))}"
    obs = {"worker_command": cmd,
           "deviation": "late marker is a file in the fixture repo, not $PROBE_LOG: the hook "
                        "log holds JSON lines, and the fixture is writable under Claude's sandbox"}
    sess.start()
    sess.send(dispatch_bg_msg(cmd, "lifecycle worker"))
    sent = time.monotonic()
    rows = wait_until(lambda: sleep_rows(token), DISPATCH_TIMEOUT, sess)
    if not rows:
        sess.save("no-sleep")
        raise StepFail(f"worker's `sleep {token}` never started", sess.pane.splitlines()[-10:])
    obs["sleep_started_after_s"] = round(time.monotonic() - sent, 1)
    cpid = sess.pane_pid()
    snap = ps_rows()
    crow = row_of(cpid, snap)
    obs["claude_process"] = brief([crow]) if crow else f"pid {cpid} not in ps"
    obs["token_processes_before_kill"] = brief(rows_with(token, snap))
    obs["sleep_pgid_equals_claude_pgid"] = bool(crow) and all(
        r["pgid"] == crow["pgid"] for r in sleep_rows(token, snap))
    obs["sleep_is_claude_descendant"] = [r["pid"] in {d["pid"] for d in descendants(cpid, snap)}
                                         for r in sleep_rows(token, snap)]
    obs["hook_fields_for_sleep_command (observation)"] = hook_fields(sess, token)
    progress(f"  step1: sleep running; SIGTERM claude pid {cpid} in {STEP1_KILL_AFTER}s")
    idle(STEP1_KILL_AFTER, sess)
    sess.save("before-kill")
    os.kill(cpid, signal.SIGTERM)
    ctx.cmds.append((["kill", "-TERM", str(cpid)], 0))
    killed = time.monotonic()
    obs["signal"] = "SIGTERM to the claude pid only"
    seen = {"claude_gone_after_s": None, "sleep_gone_after_s": None, "marker_after_s": None}
    end_reason = f"watched {STEP1_WATCH}s"
    progress(f"  step1: watching up to {STEP1_WATCH}s for the late marker and surviving sleep")
    while time.monotonic() - killed < STEP1_WATCH:
        time.sleep(5)
        t = round(time.monotonic() - killed, 1)
        snap = ps_rows()
        if seen["claude_gone_after_s"] is None and not row_of(cpid, snap):
            seen["claude_gone_after_s"] = t
        if seen["sleep_gone_after_s"] is None and not sleep_rows(token, snap):
            seen["sleep_gone_after_s"] = t
        if seen["marker_after_s"] is None and marker.exists():
            seen["marker_after_s"] = t
        if not rows_with(token, snap) and not row_of(cpid, snap) and t > 10:
            end_reason = f"stopped at {t}s: claude and every process holding the token are gone"
            break
    snap = ps_rows()
    sess.save("after")
    obs.update(seen)
    obs.update({
        "watch": end_reason,
        "late_written": marker.exists(),
        "late_marker_content": marker.read_text().strip() if marker.exists() else None,
        "sleep_survives_at_end": brief(sleep_rows(token, snap)) or False,
        "token_processes_at_end": brief(rows_with(token, snap)) or "none",
        "claude_alive_at_end": bool(row_of(cpid, snap)),
        "pane_dead": sess.dead(),
        "session_id": sess.session_id(),
        "session_id_source": sess.sid_source,
        "transcripts_found": sess.copy_transcripts() or "none",
    })
    subs = sess.sub_paths()
    if subs:
        obs["subagent_transcript_tail"] = [summarize(r) for r in read_jsonl(subs[-1])[-3:]]
    summary = (f"late_written={obs['late_written']} sleep_survives={bool(obs['sleep_survives_at_end'])} "
               f"claude_gone_after={obs['claude_gone_after_s']}s")
    return "OBSERVED", sess.pane.rstrip().splitlines()[-10:], obs, summary


# ---------------------------------------------------------------- step 2


def step2(ctx: Ctx):
    sess = Session(ctx, "step2")
    token = ctx.token("60.2")
    ctx.needles.append(token)
    cmd = f"sleep {token}"
    obs = {"worker_command": cmd}
    sess.start()
    sess.send(dispatch_bg_msg(cmd, "stop worker"))
    rows = wait_until(lambda: sleep_rows(token), DISPATCH_TIMEOUT, sess)
    if not rows:
        sess.save("no-sleep")
        raise StepFail(f"worker's `{cmd}` never started", sess.pane.splitlines()[-10:])
    obs["sleep_before_stop"] = brief(rows)
    obs["hook_fields_for_sleep_command (observation)"] = hook_fields(sess, token)
    recs = sess.main_records()
    disp = [u for u in tool_uses(recs) if u["name"] in ("Agent", "Task")]
    res = tool_results(recs)
    obs["dispatch_input"] = disp[0]["input"] if disp else "no Agent call in transcript yet"
    obs["dispatch_result"] = res.get(disp[0]["id"], "")[:400] if disp else None
    stop_msg = ("Now call the TaskStop tool on the background agent you dispatched, using the "
                "task or agent ID its launch returned. Then reply with one line: stopped: <the "
                "TaskStop result>")
    progress("  step2: worker sleeping; asking the supervisor to call TaskStop")
    sess.send(stop_msg)
    sent = time.monotonic()
    gone_at = None

    def stopped():
        nonlocal gone_at
        if gone_at is None and not sleep_rows(token):
            gone_at = round(time.monotonic() - sent, 1)
        r = sess.main_records()
        i = user_index(r, "TaskStop tool on the background agent")
        return i is not None and any("stopped" in t for t in assistant_texts(r, i))

    wait_until(stopped, TURN_TIMEOUT, sess)
    idle(10, sess)
    stopped()
    recs = sess.main_records()
    i = user_index(recs, "TaskStop tool on the background agent") or 0
    res = tool_results(recs)
    stops = [u for u in tool_uses(recs, i) if u["name"] == "TaskStop"]
    sess.save("after")
    subs = sess.sub_paths()
    obs.update({
        "taskstop_calls": [{"input": u["input"], "result": res.get(u["id"], "<no result>")[:400]}
                           for u in stops] or "none",
        "other_tools_after_stop_msg": [u["name"] for u in tool_uses(recs, i)
                                       if u["name"] != "TaskStop"] or "none",
        "supervisor_reply": assistant_texts(recs, i)[-3:],
        "sleep_gone_after_s": gone_at,
        "sleep_alive_at_end": brief(sleep_rows(token)) or False,
        "task_notification_after_stop": [summarize(r)[:300] for r in recs[i:]
                                         if "task-notification" in json.dumps(r)][:3] or "none",
        "main_transcript_tail": [summarize(r) for r in recs[-3:]],
        "subagent_transcripts": {p.name: len(read_jsonl(p)) for p in subs} or "none",
        "subagent_transcript_tail": ([summarize(r) for r in read_jsonl(subs[-1])[-3:]]
                                     if subs else "none"),
        "session_id": sess.session_id(),
        "session_id_source": sess.sid_source,
        "transcripts_found": sess.copy_transcripts() or "none",
    })
    sess.kill()
    summary = (f"taskstop_called={bool(stops)} sleep_gone_after={gone_at}s "
               f"sleep_alive_at_end={bool(obs['sleep_alive_at_end'])}")
    return "OBSERVED", sess.pane.rstrip().splitlines()[-10:], obs, summary


# ---------------------------------------------------------------- step 3


def step3(ctx: Ctx):
    sess = Session(ctx, "step3")
    token = f"resume-token-{ctx.uniq}"
    cmd = f"echo {token}"
    obs = {"worker_command": cmd}
    sess.start()
    msg1 = (f"Call the Agent tool once with subagent_type \"{WORKER_AGENT}\", description \"resume "
            f"worker\", in the foreground (do not set run_in_background), and as prompt exactly the "
            f"text between <<< and >>>: <<<{cmd}>>> Wait for its reply, then reply with one line: "
            f"finished <the agent ID>")
    sess.send(msg1)

    def replied(needle: str, reply: str):
        r = sess.main_records()
        i = user_index(r, needle)
        return i is not None and any(reply in t for t in assistant_texts(r, i + 1))

    if not wait_until(lambda: replied("resume worker", "finished"), TURN_TIMEOUT, sess):
        sess.save("no-finish")
        raise StepFail("supervisor never replied `finished` (or no transcript found)",
                       sess.pane.splitlines()[-10:])
    recs = sess.main_records()
    disp = [u for u in tool_uses(recs) if u["name"] in ("Agent", "Task")]
    res = tool_results(recs)
    obs["dispatch_input"] = disp[0]["input"] if disp else "none"
    obs["dispatch_result"] = res.get(disp[0]["id"], "")[:400] if disp else None
    obs["worker_ran_command"] = any(token in c for c in hook_commands(sess))
    before_subs = sub_counts(sess)
    before_hooks = len(hook_commands(sess))
    obs["subagent_records_before_followup"] = before_subs or "none"
    progress("  step3: worker finished; asking the supervisor to SendMessage it")
    msg2 = ("Use the SendMessage tool to send the worker you just dispatched (address it by its "
            "agent ID) this message: what did you run before? Then reply with one line: worker "
            "answered: <its answer>")
    sess.send(msg2)
    wait_until(lambda: replied("SendMessage tool to send the worker", "worker answered"),
               TURN_TIMEOUT, sess)
    # A resumed worker may answer asynchronously; wait until the transcripts stop growing.
    last, stable_since, end = None, time.monotonic(), time.monotonic() + 60
    while time.monotonic() < end and time.monotonic() - stable_since < 15:
        sess.poll()
        now = (len(sess.main_records()), tuple(sorted(sub_counts(sess).items())))
        if now != last:
            last, stable_since = now, time.monotonic()
        time.sleep(3)
    recs = sess.main_records()
    i = user_index(recs, "SendMessage tool to send the worker") or 0
    res = tool_results(recs)
    sends = [u for u in tool_uses(recs, i) if u["name"] == "SendMessage"]
    after_subs = sub_counts(sess)
    new_sub_text = []
    for p in sess.sub_paths():
        rs = read_jsonl(p)
        new_sub_text += assistant_texts(rs, before_subs.get(p.name, 0))
    replies = assistant_texts(recs, i)
    sess.save("after")
    obs.update({
        "sendmessage_calls": [{"input": u["input"], "result": res.get(u["id"], "<no result>")[:400]}
                              for u in sends] or "none",
        "other_tools_after_followup": [u["name"] for u in tool_uses(recs, i)
                                       if u["name"] != "SendMessage"] or "none",
        "supervisor_replies_after_followup": replies[-4:],
        "subagent_records_after_followup": after_subs or "none",
        "subagent_new_assistant_text": [t[:200] for t in new_sub_text][-3:] or "none",
        "token_in_answer": any(token in t for t in replies + new_sub_text) or any(
            token in res.get(u["id"], "") for u in sends),
        "worker_new_bash_calls_after_followup": hook_commands(sess)[before_hooks:] or "none",
        "followup_in_subagent_transcript": any(
            user_index(read_jsonl(p), "what did you run before") is not None
            for p in sess.sub_paths()),
        "session_id": sess.session_id(),
        "session_id_source": sess.sid_source,
        "transcripts_found": sess.copy_transcripts() or "none",
    })
    sess.kill()
    summary = (f"sendmessage_called={bool(sends)} answered_from_history={obs['token_in_answer']} "
               f"new_bash={obs['worker_new_bash_calls_after_followup'] != 'none'}")
    return "OBSERVED", sess.pane.rstrip().splitlines()[-10:], obs, summary


# ---------------------------------------------------------------- step 4


def step4_variant(ctx: Ctx, detach: bool) -> tuple[dict, str]:
    variant = "detached" if detach else "attached"
    sess = Session(ctx, f"step4-{variant}")
    rdir = sess.cwd / f"runner-{variant}"
    argv = [str(FAKE_RUNNER)] + (["--detach"] if detach else []) + [str(rdir)]
    cmd = shlex.join(argv)
    obs = {"worker_command": cmd}
    sess.start()
    sess.send(dispatch_bg_msg(cmd, f"runner {variant}"))
    pidf = rdir / "pid"
    if not wait_until(lambda: pidf.exists() and pidf.read_text().strip(), DISPATCH_TIMEOUT, sess):
        sess.save("no-runner")
        raise StepFail(f"{variant} runner never wrote its pid", sess.pane.splitlines()[-10:])
    started = time.monotonic()
    rpid = int(pidf.read_text().strip())
    ctx.needles.append(str(rdir))
    cpid = sess.pane_pid()
    snap = ps_rows()
    crow, rrow = row_of(cpid, snap), row_of(rpid, snap)
    obs.update({
        "runner_ids": (rdir / "ids").read_text().strip() if (rdir / "ids").exists() else None,
        "runner_process": brief([rrow]) if rrow else f"pid {rpid} not in ps",
        "claude_process": brief([crow]) if crow else f"pid {cpid} not in ps",
        "runner_is_claude_descendant": rpid in {d["pid"] for d in descendants(cpid, snap)},
        "hook_fields_for_runner_command (observation)": hook_fields(sess, str(rdir)),
    })
    progress(f"  step4 {variant}: runner pid {rpid} up; SIGKILL claude pid {cpid} in "
             f"{STEP4_KILL_AFTER}s")
    idle(STEP4_KILL_AFTER, sess)
    sess.save("before-kill")
    os.kill(cpid, signal.SIGKILL)
    ctx.cmds.append((["kill", "-KILL", str(cpid)], 0))
    killed_wall = time.time()
    obs["signal"] = "SIGKILL to the claude pid only"
    end_reason = f"watched until {STEP4_WATCH}s after runner start"
    runner_gone = None
    progress(f"  step4 {variant}: watching the runner")
    while time.monotonic() - started < STEP4_WATCH:
        time.sleep(5)
        if (rdir / "result.json").exists():
            end_reason = "result.json written"
            break
        if not alive(rpid):
            runner_gone = round(time.monotonic() - started, 1)
            idle(2)
            end_reason = f"runner gone {runner_gone}s after start, no result.json"
            break
    beats = []
    try:
        beats = [int(x) for x in (rdir / "heartbeat").read_text().split()]
    except (OSError, ValueError):
        pass
    t0 = beats[0] if beats else None
    sess.save("after")
    obs.update({
        "watch": end_reason,
        "runner_alive_at_end": alive(rpid),
        "runner_gone_after_s": runner_gone,
        "heartbeats": len(beats),
        "heartbeat_offsets_s": [b - t0 for b in beats] if beats else [],
        "heartbeats_after_kill": sum(1 for b in beats if b > killed_wall),
        "last_heartbeat_age_s": round(time.time() - beats[-1], 1) if beats else None,
        "result_json": read_json(rdir / "result.json") or "absent",
        "signals_caught": ((rdir / "signals").read_text().strip()
                           if (rdir / "signals").exists() else "none"),
        "claude_alive_at_end": alive(cpid),
        "session_id": sess.session_id(),
        "transcripts_found": sess.copy_transcripts() or "none",
    })
    if rdir.exists():
        shutil.copytree(rdir, ctx.art / f"step4-{variant}-runner", dirs_exist_ok=True)
    line = (f"{variant}: survived={obs['runner_alive_at_end'] or obs['result_json'] != 'absent'} "
            f"beats_after_kill={obs['heartbeats_after_kill']} "
            f"result={'yes' if obs['result_json'] != 'absent' else 'no'}")
    return obs, line


def step4(ctx: Ctx):
    obs, lines, excerpt = {}, [], []
    for detach in (True, False):
        variant = "detached" if detach else "attached"
        try:
            o, line = step4_variant(ctx, detach)
        except StepFail as e:
            o, line = {"failure": e.reason, "pane_last_lines": e.lines}, f"{variant}: FAIL {e.reason}"
        except Exception:  # noqa: BLE001 - recorded, the other variant still runs
            tb = traceback.format_exc()
            o, line = {"failure": tb.strip().splitlines()[-1]}, f"{variant}: FAIL"
            excerpt += tb.strip().splitlines()[-4:]
        obs.update({f"{variant} {k}": v for k, v in o.items()})
        lines.append(line)
        excerpt.append(line)
    return "OBSERVED", excerpt, obs, "; ".join(lines)


# ---------------------------------------------------------------- step 5 (codex)


def codex_exec_argv(ctx: Ctx) -> list[str]:
    return ["codex", "exec", *CODEX_PIN, *CODEX_JSON, *ctx.codex_sandbox, "-"]


def codex_launcher(rdir: Path) -> int:
    """Stand-in dispatcher: start a detached runner, then exit after 3 s."""
    with open(rdir / "runner.log", "ab") as log:
        p = subprocess.Popen([sys.executable, str(SELF), "_codex-runner", str(rdir)],
                             stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                             start_new_session=True)
    atomic_write_json(rdir / "launcher.json", {"launcher_pid": os.getpid(),
                                               "launcher_pgid": os.getpgrp(),
                                               "runner_pid": p.pid})
    time.sleep(3)
    return 0


def codex_runner(rdir: Path) -> int:
    """Stand-in runner: own Codex in its own process group and capture its stream."""
    argv = json.loads((rdir / "argv.json").read_text())
    cwd = (rdir / "cwd.txt").read_text()
    with open(rdir / "prompt.txt", "rb") as stdin, open(rdir / "stream.jsonl", "wb") as out, \
            open(rdir / "codex-stderr.txt", "wb") as err:
        p = subprocess.Popen(argv, cwd=cwd, stdin=stdin, stdout=out, stderr=err,
                             process_group=0, env=child_env())
        try:
            cpgid = os.getpgid(p.pid)
        except ProcessLookupError:
            cpgid = None
        atomic_write_json(rdir / "runner.json", {
            "runner_pid": os.getpid(), "runner_pgid": os.getpgrp(), "runner_sid": os.getsid(0),
            "runner_ppid": os.getppid(), "codex_pid": p.pid, "codex_pgid": cpgid,
            "started": time.time()})
        try:
            rc = p.wait(timeout=CODEX_RUNNER_MAX)
        except subprocess.TimeoutExpired:
            kill_group(p.pid)
            rc = p.wait(timeout=CMD_TIMEOUT)
    atomic_write_json(rdir / "result.json", {
        "codex_exit": rc, "runner_ppid_at_end": os.getppid(), "finished": time.time(),
        "stream_bytes": (rdir / "stream.jsonl").stat().st_size})
    return 0


def find_keys(obj, pattern: re.Pattern, path: str = "") -> list[tuple[str, object]]:
    found = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            p = f"{path}.{k}" if path else k
            if pattern.search(str(k)) and not isinstance(v, (dict, list)):
                found.append((p, v))
            found += find_keys(v, pattern, p)
    elif isinstance(obj, list):
        for v in obj:
            found += find_keys(v, pattern, path + "[]")
    return found


THREAD_KEY = re.compile(r"^(thread_id|session_id|conversation_id)$")


def parse_lines(text: str) -> list[dict]:
    out = []
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            v = json.loads(line)
            out.append(v if isinstance(v, dict) else {"value": v})
        except ValueError:
            out.append({"unparsed": line})
    return out


def analyze_codex(events: list[dict]) -> dict:
    thread = None
    for e in events:
        hits = find_keys(e, THREAD_KEY)
        if hits:
            thread = {"event": e.get("type"), "field": hits[0][0], "value": hits[0][1]}
            break
    cmds, msgs = [], []
    for e in events:
        item = e.get("item") if isinstance(e.get("item"), dict) else {}
        if e.get("type") == "item.completed" and item.get("type") == "command_execution":
            cmds.append({"command": item.get("command"), "exit_code": item.get("exit_code"),
                         "status": item.get("status")})
        if e.get("type") == "item.completed" and item.get("type") == "agent_message":
            msgs.append(item.get("text"))
    types = [e.get("type", "unparsed" if "unparsed" in e else "?") for e in events]
    return {
        "lines": len(events),
        "unparsed_lines": sum(1 for e in events if "unparsed" in e),
        "event_types_in_order": list(dict.fromkeys(types)),
        "last_event": types[-1] if types else None,
        "thread": thread,
        "command_executions": cmds,
        "agent_messages": [str(m)[:200] for m in msgs],
        "usage": next((e.get("usage") for e in events if e.get("type") == "turn.completed"), None),
        "errors": [json.dumps(e)[:300] for e in events if e.get("type") in ("error", "turn.failed")],
    }


def start_codex_runner(ctx: Ctx, label: str, prompt: str) -> dict:
    rdir = ctx.art / label
    rdir.mkdir(parents=True, exist_ok=True)
    argv = codex_exec_argv(ctx)
    atomic_write_json(rdir / "argv.json", argv)
    (rdir / "prompt.txt").write_text(prompt)
    (rdir / "cwd.txt").write_text(str(ctx.fixture("codex")))
    ctx.needles.append(str(rdir))
    largv = [sys.executable, str(SELF), "_codex-launcher", str(rdir)]
    ctx.cmds.append((argv, "run by the runner; see codex_exit"))
    t0 = time.monotonic()
    with open(rdir / "launcher.log", "wb") as log:
        lp = subprocess.Popen(largv, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                              env=child_env())
        lrc = lp.wait(timeout=CMD_TIMEOUT)
    ctx.cmds.append((largv, lrc))
    info = {"rdir": rdir, "argv": argv, "launcher_exit": lrc,
            "launcher_exited_after_s": round(time.monotonic() - t0, 1)}
    runner = wait_until(lambda: read_json(rdir / "runner.json"), 15, interval=0.5)
    if not runner:
        raise StepFail(f"{label}: runner wrote no runner.json",
                       (rdir / "runner.log").read_text(errors="replace").splitlines()[-10:]
                       if (rdir / "runner.log").exists() else [])
    ctx.pgids += [runner["runner_pid"], runner["codex_pid"]]
    rrow = row_of(runner["runner_pid"])
    info.update({"runner": runner,
                 "runner_alive_after_launcher_exit": rrow is not None,
                 "runner_ppid_after_launcher_exit": rrow["ppid"] if rrow else None})
    return info


def codex_obs(info: dict, result) -> dict:
    rdir = info["rdir"]
    a = analyze_codex(read_jsonl(rdir / "stream.jsonl"))
    stderr = (rdir / "codex-stderr.txt").read_text(errors="replace") \
        if (rdir / "codex-stderr.txt").exists() else ""
    return {
        "codex_argv": shlex.join(info["argv"]),
        "launcher_exit": info["launcher_exit"],
        "launcher_exited_after_s": info["launcher_exited_after_s"],
        "runner_pid/pgid/sid": (f"{info['runner']['runner_pid']}/{info['runner']['runner_pgid']}"
                                f"/{info['runner']['runner_sid']}"),
        "codex_pid/pgid": f"{info['runner']['codex_pid']}/{info['runner']['codex_pgid']}",
        "runner_alive_after_launcher_exit": info["runner_alive_after_launcher_exit"],
        "runner_ppid_after_launcher_exit": info["runner_ppid_after_launcher_exit"],
        "runner_result": result or "absent",
        "codex_exit": (result or {}).get("codex_exit", "unknown"),
        "stream_lines": a["lines"],
        "stream_unparsed_lines": a["unparsed_lines"],
        "stream_event_types": a["event_types_in_order"],
        "stream_last_event": a["last_event"],
        "thread_id_location": a["thread"],
        "command_executions": a["command_executions"],
        "agent_messages": a["agent_messages"],
        "usage": a["usage"],
        "stream_errors": a["errors"] or "none",
        "codex_stderr_first_lines": stderr.splitlines()[:5],
    }


def codex_prompt(token: str) -> str:
    return (f"Run the shell command `sleep {token}` and then the shell command `echo done`, as two "
            f"separate commands, in that order. Then reply with one line: done")


def step5_run(ctx: Ctx, label: str, token: str, ladder: bool) -> tuple[dict, list[str]]:
    ctx.needles.append(token)
    info = start_codex_runner(ctx, label, codex_prompt(token))
    rdir, run = info["rdir"], info["runner"]
    cpid, cpgid = run["codex_pid"], run["codex_pgid"] or run["codex_pid"]
    obs, excerpt = {}, []
    tool = wait_until(lambda: sleep_rows(token) or (rdir / "result.json").exists(), 150,
                      interval=1)
    tool = tool if isinstance(tool, list) else []
    snap = ps_rows()
    tree = [row_of(cpid, snap)] + descendants(cpid, snap) if row_of(cpid, snap) else []
    tree = [r for r in tree if r]
    obs["sleep_seen"] = bool(tool)
    obs["tool_processes_while_sleeping"] = brief(sleep_rows(token, snap)) or "none"
    obs["token_processes_while_sleeping"] = brief(rows_with(token, snap)) or "none"
    obs["codex_tree_while_sleeping"] = brief(tree) or "codex not running"
    obs["tool_pgid_vs_codex_pgid"] = [f"tool {r['pid']} pgid={r['pgid']} codex pgid={cpgid}"
                                      for r in sleep_rows(token, snap)] or "no tool seen"
    obs["codex_tree_in_other_pgid"] = brief([r for r in tree if r["pgid"] != cpgid]) or "none"
    if ladder and tool:
        tool_pids = [r["pid"] for r in sleep_rows(token, snap)]
        tree_pids = [r["pid"] for r in tree]
        progress(f"  {label}: tool sleep running; signal ladder on codex pgid {cpgid}")
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGKILL):
            snap = ps_rows()
            left = [p for p in set(tree_pids + tool_pids) if row_of(p, snap)]
            group = [r for r in snap if r["pgid"] == cpgid]
            if not left and not group:
                obs[f"ladder {sig.name}"] = "not sent: nothing left"
                continue
            try:
                os.killpg(cpgid, sig)
                sent = "sent"
            except ProcessLookupError:
                sent = "no process in codex's group"
            ctx.cmds.append((["kill", f"-{sig.name[3:]}", f"-{cpgid}"], sent))
            time.sleep(LADDER_WAIT)
            snap = ps_rows()
            obs[f"ladder {sig.name}"] = {
                "result": sent,
                "codex_alive": bool(row_of(cpid, snap)),
                "tool_pids_alive": [p for p in tool_pids if row_of(p, snap)],
                "codex_group_remaining": brief([r for r in snap if r["pgid"] == cpgid]) or [],
                "earlier_tree_remaining": brief([r for r in snap if r["pid"] in tree_pids]) or [],
            }
            excerpt.append(f"{sig.name}: codex_alive={obs[f'ladder {sig.name}']['codex_alive']} "
                           f"tool_alive={obs[f'ladder {sig.name}']['tool_pids_alive']}")
        leftover = sleep_rows(token)
        obs["tool_left_after_ladder"] = brief(leftover) or "none"
        for r in leftover:
            kill_pid(r["pid"])
    elif ladder:
        obs["ladder"] = "not run: the tool sleep was never seen"
    result = wait_until(lambda: read_json(rdir / "result.json"),
                        20 if ladder else CODEX_TIMEOUT, interval=2)
    obs.update(codex_obs(info, result))
    excerpt += [f"[stream] {l[:160]}" for l in
                (rdir / "stream.jsonl").read_text(errors="replace").splitlines()[-4:]] \
        if (rdir / "stream.jsonl").exists() else []
    return obs, excerpt


def step5(ctx: Ctx):
    obs, excerpt = {"codex_cwd": "<tmp>/lhprobe3 XXXX/codex repo (git repo, one commit)"}, []
    progress("  step5: run 1, codex to completion under a detached runner")
    o, ex = step5_run(ctx, "step5-run1", ctx.token("30.5"), ladder=False)
    obs.update({f"run1 {k}": v for k, v in o.items()})
    excerpt += ex[-4:]
    ok_cmd = any(ctx.token("30.5") in str(c.get("command")) and c.get("exit_code") == 0
                 for c in o.get("command_executions", []))
    if not ok_cmd:
        progress("  step5: sleep did not succeed under the default sandbox; retrying with "
                 "-s workspace-write")
        ctx.codex_sandbox = ["-s", "workspace-write"]
        o, ex = step5_run(ctx, "step5-run1-ws", ctx.token("30.7"), ladder=False)
        obs.update({f"run1-ws {k}": v for k, v in o.items()})
        excerpt += ex[-3:]
    obs["codex_sandbox_flag_used"] = shlex.join(ctx.codex_sandbox) or "none (default)"
    progress("  step5: run 2, signal ladder on codex's process group")
    o2, ex2 = step5_run(ctx, "step5-run2-ladder", ctx.token("30.6"), ladder=True)
    obs.update({f"run2 {k}": v for k, v in o2.items()})
    excerpt += ex2[:4]
    summary = (f"run1 codex_exit={o.get('codex_exit')} last_event={o.get('stream_last_event')} "
               f"runner_survived_launcher={o.get('runner_alive_after_launcher_exit')}; "
               f"run2 other_pgid={o2.get('codex_tree_in_other_pgid')} "
               f"tool_left={o2.get('tool_left_after_ladder', 'n/a')}")
    return "OBSERVED", excerpt[:10], obs, summary


# ---------------------------------------------------------------- step 6 (codex resume)


def codex_config_model() -> str:
    try:
        return str(tomllib.loads(CODEX_CONFIG.read_text()).get("model", "unset"))
    except (OSError, tomllib.TOMLDecodeError) as e:
        return f"unreadable ({type(e).__name__})"


def resume_variants(tid: str) -> list[tuple[str, list[str], bool]]:
    """(name, argv, pinned). Unpinned variants use Codex's configured default model."""
    return [
        ("flags-before-resume", ["codex", "exec", *CODEX_PIN, *CODEX_JSON, "resume", tid, "-"], True),
        ("flags-after-resume", ["codex", "exec", "resume", *CODEX_PIN, *CODEX_JSON, tid, "-"], True),
        ("json-before-mc-after-resume",
         ["codex", "exec", *CODEX_JSON, "resume", *CODEX_PIN, tid, "-"], True),
        ("mc-after-thread-id", ["codex", "exec", *CODEX_JSON, "resume", tid, *CODEX_PIN, "-"], True),
        ("bare-no-mc", ["codex", "exec", "resume", tid, "-"], False),
        ("json-before-no-mc", ["codex", "exec", *CODEX_JSON, "resume", tid, "-"], False),
    ]


def run_codex(ctx: Ctx, label: str, argv: list[str], prompt: str) -> dict:
    cwd = ctx.fixture("codex")
    try:
        p = subprocess.run(argv, cwd=cwd, input=prompt, capture_output=True, text=True,
                           timeout=180, env=child_env(), start_new_session=True)
        rc, out, err = p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired as e:
        rc = "timeout"
        out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        err = e.stderr.decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
    (ctx.art / f"{label}-stdout.txt").write_text(out)
    (ctx.art / f"{label}-stderr.txt").write_text(err)
    ctx.cmds.append((argv, rc))
    return {"exit": rc, "stdout": out, "stderr": err}


def step6(ctx: Ctx):
    word = f"lhprobe-word-{ctx.uniq}"
    first = ["codex", "exec", *CODEX_PIN, *CODEX_JSON, "-"]
    r = run_codex(ctx, "step6-first", first,
                  f"Do not run any commands. Remember this word: {word}. Reply with only: OK")
    a = analyze_codex(parse_lines(r["stdout"]))
    obs = {"first_exit": r["exit"], "first_thread_id_location": a["thread"],
           "first_agent_messages": a["agent_messages"], "codex_config_model": codex_config_model()}
    if not a["thread"]:
        raise StepFail("no thread id in the first exec's --json stream",
                       (r["stderr"] or r["stdout"]).splitlines()[:10])
    tid = str(a["thread"]["value"])
    ask = "What word did I ask you to remember? Reply with only that word."
    excerpt, works = [], []
    for name, argv, pinned in resume_variants(tid):
        if not pinned and obs["codex_config_model"] != CODEX_MODEL:
            obs[f"{name}"] = (f"skipped: no -m/-c would use the configured model "
                              f"{obs['codex_config_model']!r}, not {CODEX_MODEL}")
            excerpt.append(f"{name}: skipped (unpinned)")
            continue
        progress(f"  step6: {name}")
        rr = run_codex(ctx, f"step6-{name}", argv, ask)
        ra = analyze_codex(parse_lines(rr["stdout"]))
        recalled = word in rr["stdout"]
        obs[name] = {
            "argv": shlex.join(argv),
            "exit": rr["exit"],
            "stderr_first_lines": rr["stderr"].strip().splitlines()[:5],
            "stdout_first_lines": [l[:200] for l in rr["stdout"].strip().splitlines()[:3]],
            "recalled_word": recalled,
            "thread_id_in_output": (ra["thread"] or {}).get("value"),
            "same_thread": (ra["thread"] or {}).get("value") == tid if ra["thread"] else None,
        }
        if rr["exit"] == 0 and recalled:
            works.append(name)
        excerpt.append(f"{name}: exit={rr['exit']} recalled={recalled} "
                       f"{(rr['stderr'].strip().splitlines() or [''])[0][:100]}")
    obs["variants_that_worked"] = works or "none"
    where = f"{a['thread']['event']}.{a['thread']['field']}"
    return "OBSERVED", excerpt[:10], obs, f"thread_id_in={where} worked={works or 'none'}"


STEP_FUNCS = {1: step1, 2: step2, 3: step3, 4: step4, 5: step5, 6: step6}


# ---------------------------------------------------------------- dry run


def dry_commands(ctx: Ctx, n: int) -> list[str]:
    tm = lambda label: shlex.join(tmux_new_argv(ctx, f"lh3-{label}", str(ctx.fixture(label)),
                                                f"<out>/task3-artifacts/{label}-hooks.jsonl"))
    send = f"tmux -L {ctx.sock} send-keys -t lh3-<label> -l <message> ; ... Enter"
    if n == 1:
        cmd = f"sleep {ctx.token('120.1')}; echo late >> '<fixture>/late-marker.txt'"
        return [tm("step1"), send, f"  message: {dispatch_bg_msg(cmd, 'lifecycle worker')}",
                f"wait for `sleep {ctx.token('120.1')}` in ps; {STEP1_KILL_AFTER}s later "
                f"kill -TERM <pane_pid>; watch {STEP1_WATCH}s"]
    if n == 2:
        return [tm("step2"), send,
                "  message 1: " + dispatch_bg_msg(f"sleep {ctx.token('60.2')}", "stop worker"),
                "  message 2 (once the sleep is in ps): call TaskStop on that agent",
                "read the main and subagent transcripts under ~/.claude/projects/*/<session_id>*"]
    if n == 3:
        return [tm("step3"), send,
                f"  message 1: foreground dispatch of `echo resume-token-{ctx.uniq}`",
                "  message 2: SendMessage the worker 'what did you run before?'"]
    if n == 4:
        out = []
        for detach in (True, False):
            v = "detached" if detach else "attached"
            argv = [str(FAKE_RUNNER)] + (["--detach"] if detach else []) + [f"<fixture>/runner-{v}"]
            out += [tm(f"step4-{v}"), f"  message: {dispatch_bg_msg(shlex.join(argv), f'runner {v}')}",
                    f"  wait for runner pid file; {STEP4_KILL_AFTER}s later kill -KILL <pane_pid>; "
                    f"watch until {STEP4_WATCH}s after runner start"]
        return out
    if n == 5:
        return [f"{sys.executable} {SELF} _codex-launcher <out>/task3-artifacts/step5-run1  "
                "(launcher exits after 3 s; its runner, start_new_session=True, runs:)",
                f"  {shlex.join(codex_exec_argv(ctx))}  < prompt: {codex_prompt(ctx.token('30.5'))!r}  "
                "(cwd=<tmp>/lhprobe3 XXXX/codex repo, process_group=0)",
                "  if the sleep command failed: rerun once with -s workspace-write",
                f"run 2 the same with `sleep {ctx.token('30.6')}`; while it runs: "
                "kill -INT/-TERM/-KILL -<codex pgid>, 3 s apart"]
    if n == 6:
        return ([shlex.join(["codex", "exec", *CODEX_PIN, *CODEX_JSON, "-"]) + "  < remember-word prompt"]
                + [shlex.join(a) + ("" if pinned else "  (only if ~/.codex/config.toml model is "
                                    f"{CODEX_MODEL})")
                   for _, a, pinned in resume_variants("<thread_id>")])
    return []


# ---------------------------------------------------------------- report


def fmt(v) -> str:
    return v if isinstance(v, str) else json.dumps(v, default=str)


def write_section(ctx: Ctx, n: int, status: str, excerpt, obs, error=None) -> None:
    parts = [f"\n## Step {n}: {STEPS[n]}\n", f"status: {status}\n", "\n### Commands\n"]
    for argv, code in ctx.cmds:
        parts.append(f"\n```\n{shlex.join(argv)}\n```\nexit_code: {code}\n")
    parts.append("\n### Excerpt\n\n```\n" + "\n".join(str(l) for l in excerpt[:10]) + "\n```\n")
    parts.append("\n### Observed\n\n")
    parts += [f"- {k}: {fmt(v)}\n" for k, v in obs.items()]
    if error:
        parts.append("\n### Error\n\n```\n" + error + "\n```\n")
    with open(ctx.raw, "a") as f:
        f.write("".join(parts))


def version(argv: list[str]) -> str:
    try:
        p = subprocess.run(argv, capture_output=True, text=True, timeout=CMD_TIMEOUT,
                           stdin=subprocess.DEVNULL)
        return (p.stdout or p.stderr).strip().splitlines()[0]
    except Exception as e:  # noqa: BLE001 - recorded, not fatal
        return f"unavailable ({e})"


def cleanup(ctx: Ctx) -> None:
    if ctx.tmux_started:
        try:
            tmux(ctx, "kill-server")
        except Exception:  # noqa: BLE001
            pass
    for pgid in ctx.pgids:
        kill_group(pgid)
    try:
        leftovers = [r for r in ps_rows() if any(n in r["command"] for n in ctx.needles)]
    except Exception:  # noqa: BLE001
        leftovers = []
    if leftovers:
        print("cleanup: killing leftover probe processes:\n  " + "\n  ".join(brief(leftovers)))
        with open(ctx.raw, "a") as f:
            f.write("\n## Cleanup\n\n" + "".join(f"- killed leftover: {l}\n" for l in brief(leftovers)))
        for r in leftovers:
            kill_pid(r["pid"], signal.SIGTERM)
        time.sleep(1)
        for r in leftovers:
            kill_pid(r["pid"])
    if ctx.tmp:
        shutil.rmtree(ctx.tmp, ignore_errors=True)


def parse_steps(text: str | None) -> list[int]:
    if not text:
        return sorted(STEPS)
    steps = sorted({int(s) for s in text.split(",") if s.strip()})
    bad = [s for s in steps if s not in STEPS]
    if bad:
        raise ValueError(f"unknown step(s): {bad}; valid: {sorted(STEPS)}")
    return steps


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) == 2 and argv[0] == "_codex-launcher":
        return codex_launcher(Path(argv[1]))
    if len(argv) == 2 and argv[0] == "_codex-runner":
        return codex_runner(Path(argv[1]))

    ap = argparse.ArgumentParser(
        description="Task 3 runtime probe: worker lifecycle and process ownership. "
                    "Run in an ordinary terminal, not inside Claude Code. Takes about 15 minutes.")
    ap.add_argument("--out", required=True, help="directory for task3-raw.md and task3-artifacts/")
    ap.add_argument("--only", help="comma-separated step numbers to run (1-6)")
    ap.add_argument("--dry-run", action="store_true", help="print each step's commands; run nothing")
    args = ap.parse_args(argv)
    try:
        steps = parse_steps(args.only)
    except ValueError as e:
        ap.error(str(e))
    ctx = Ctx(Path(args.out).resolve(), args.dry_run)

    if args.dry_run:
        for n in steps:
            print(f"Step {n}: {STEPS[n]}")
            for c in dry_commands(ctx, n):
                print(f"  {c}")
        return 0

    if os.environ.get("CLAUDECODE"):
        print("probe_lifecycle.py: refusing to run inside a Claude Code session (CLAUDECODE is "
              "set); run it from an ordinary terminal.", file=sys.stderr)
        return 2

    ctx.art.mkdir(parents=True, exist_ok=True)
    ctx.raw.write_text(
        f"# Task 3 raw probe results: worker lifecycle and process ownership\n\n"
        f"- date: {datetime.now(timezone.utc).isoformat(timespec='seconds')}\n"
        f"- claude: {version(['claude', '--version'])}\n"
        f"- codex: {version(['codex', '--version'])}\n"
        f"- tmux: {version(['tmux', '-V'])}\n"
        f"- python: {sys.version.split()[0]}\n"
        f"- platform: {sys.platform}\n"
        f"- plugin_dir: {PLUGIN_DIR}\n"
        f"- supervisor_agent: {SUPER_AGENT}; worker_agent: {WORKER_AGENT}\n"
        f"- fake_runner_detach: python3 fork + os.setsid + os.execv (no setsid binary on macOS)\n"
        f"- codex_config_model: {codex_config_model()}\n"
        f"- unique_suffix: {ctx.uniq}\n"
        f"- steps: {steps}\n"
    )
    summary = []
    try:
        for n in steps:
            progress(f"Step {n}: {STEPS[n]} ...")
            ctx.cmds = []
            try:
                status, excerpt, obs, line = STEP_FUNCS[n](ctx)
                write_section(ctx, n, status, excerpt, obs)
            except StepFail as e:
                status, line = "FAIL", e.reason
                write_section(ctx, n, status, e.lines, {"failure": e.reason})
            except Exception:  # noqa: BLE001 - a failing step is a finding
                tb = traceback.format_exc()
                status, line = "FAIL", tb.strip().splitlines()[-1]
                write_section(ctx, n, status, tb.strip().splitlines()[-10:], {}, error=tb[-3000:])
            summary.append(f"Step {n} {STEPS[n]}: {status} {line}")
            progress(summary[-1])
    finally:
        cleanup(ctx)
    with open(ctx.raw, "a") as f:
        f.write("\n## Summary\n\n" + "".join(f"- {s}\n" for s in summary))
    print("\n".join(summary))
    print(f"raw results: {ctx.raw}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
