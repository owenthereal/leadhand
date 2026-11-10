#!/usr/bin/env python3
"""Runtime probe for Task 4: limit enforcement and worker environment.

Runs every step of Task 4 against the real `claude` and `codex` CLIs and writes
the observed facts to DIR/task4-raw.md, with full artifacts under
DIR/task4-artifacts/. Run it in an ordinary terminal, not inside Claude Code:

    python3 tests/runtime/probe_limits.py --out DIR [--only 1,4] [--dry-run]

Standard library only. Every step runs independently; failures are recorded,
not raised. Steps 1-3 run headless `claude -p --agent lhprobe:limits-dispatch`,
which dispatches one probe worker. Steps 4-8 run `codex exec --json`, pinned to
gpt-6-luna with low reasoning effort. Expect about 30 minutes of wall time.

Codex home. Codex steps run with CODEX_HOME set to a temporary directory whose
auth.json is a symlink to the operator's auth.json, plus a minimal config.toml
(which turns on `sandbox_workspace_write.network_access` for Step 7 and sets no
shell_environment_policy, so Step 8 sees Codex's built-in default). The
operator's config.toml is never touched, and the probe never copies
credentials. After every Codex run it checks the link: if Codex replaced it
with a regular file (an atomic refresh), the file is synced back only when it
parses as JSON with token or API-key fields, its last_refresh (if any) is newer
than the real file's, and the real file did not change during the check; a
symlinked real auth.json is written through to its target. Then the link is
restored. Which path happened is recorded (never any content). At cleanup the
temp home is removed. Step 6 also runs its acceptance check against the
operator's real home, and Step 8 adds one run with the operator's real config;
both pass `-c notify=[]` and have their output redacted. Without an auth.json
the probe falls back to the real home (see the raw file's header).

Step 8 output is redacted before it is written anywhere: any `NAME=value` line
that is not one of the probe's dummy pairs (or SSH_AUTH_SOCK/SSH_AGENT_PID) has
its value replaced by `<redacted len=N>`, because a login shell may export real
tokens and the model may run a plain `env`.

`sanitize_stream()` is importable (Task 5 reuses it): it keeps keys, numbers,
booleans and a short allow-list of identifier fields, and replaces every other
string with "<placeholder>".
"""

import argparse
import hashlib
import json
import os
import queue
import random
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import traceback
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
PLUGIN_DIR = HERE / "probe-plugin"
CLAUDE_PROJECTS = Path.home() / ".claude" / "projects"
REAL_CODEX_HOME = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser().absolute()
NO_NOTIFY = ["-c", "notify=[]"]   # real-home runs: never start the operator's notify program

DISPATCH_AGENT = "lhprobe:limits-dispatch"
CODEX_MODEL = "gpt-6-luna"
CODEX_PIN = ["-m", CODEX_MODEL, "-c", 'model_reasoning_effort="low"']
CODEX_JSON = ["--json", "--skip-git-repo-check"]
CODEX_WS = ["-s", "workspace-write"]

CMD_TIMEOUT = 30            # every short subprocess call
CLAUDE_TIMEOUT = {1: 300, 2: 300, 3: 1500}
CODEX_TIMEOUT = 300
CODEX_LONG_TIMEOUT = 1200   # step 6 compaction run
STOP_AFTER_ITEMS = 2        # step 5
STOP_GRACE = 5              # step 5: SIGINT -> SIGTERM -> SIGKILL spacing
COUNTED_ITEM_TYPES = ("command_execution", "file_change")   # step 5's guess, recorded

STEPS = {
    1: "maxTurns",
    2: "Per-dispatch override",
    3: "autoCompactWindow",
    4: "Codex event stream",
    5: "Codex step stop",
    6: "Codex compaction",
    7: "Network setting",
    8: "Worker environment inside a Codex shell",
}

GIT_SAFE = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
            "-c", "user.name=probe", "-c", "user.email=probe@example.invalid"]

# Step 8: dummy values only. Process env and shell_environment_policy.set carry
# different GH_TOKEN values so the output shows which path delivered it.
ENV_VARS = {
    "GH_TOKEN": "lhprobe-dummy-env",
    "GH_LHPROBE_ENV_ONLY": "lhprobe-env-only",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_CONFIG_COUNT": "1",
    "GIT_CONFIG_KEY_0": "credential.helper",
    "GIT_CONFIG_VALUE_0": "",
}
SET_VARS = {
    "GH_TOKEN": "lhprobe-dummy-set",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_CONFIG_COUNT": "1",
    "GIT_CONFIG_KEY_0": "credential.helper",
    "GIT_CONFIG_VALUE_0": "",
}
ENV_PREFIX = re.compile(r"^(GIT_|GH_|SSH_)")
ENV_LINE = re.compile(r"^((?:GIT_|GH_|SSH_)[A-Za-z0-9_]*)=(.*)$")
ANY_ENV_LINE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$")
SAFE_SSH = {"SSH_AUTH_SOCK", "SSH_AGENT_PID"}

T0 = time.monotonic()


def progress(msg: str) -> None:
    print(f"[{time.monotonic() - T0:7.1f}s] {msg}", flush=True)


class StepFail(Exception):
    def __init__(self, reason: str, lines: list[str] | None = None):
        super().__init__(reason)
        self.reason = reason
        self.lines = lines or []


# ---------------------------------------------------------------- sanitizer (reused by Task 5)

SANITIZE_KEEP = frozenset({"type", "id", "thread_id", "status", "kind", "subtype", "role"})
PLACEHOLDER = "<placeholder>"


def sanitize_value(value, key: str | None = None, keep=SANITIZE_KEEP, home: str | None = None):
    """Keep keys, numbers, booleans and null; keep strings only under `keep` keys
    (with the home directory replaced by `~`); replace every other string."""
    home = str(Path.home()) if home is None else home
    if isinstance(value, dict):
        return {k: sanitize_value(v, k, keep, home) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitize_value(v, key, keep, home) for v in value]
    if isinstance(value, str):
        if key in keep:
            return value.replace(home, "~") if home else value
        return PLACEHOLDER
    return value


def sanitize_stream(lines, keep=SANITIZE_KEEP, home: str | None = None) -> list[str]:
    """Sanitize JSONL lines; an unparsable line becomes {"unparsed": "<placeholder>"}."""
    out = []
    for line in lines:
        if not line.strip():
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            obj = {"unparsed": PLACEHOLDER}
        out.append(json.dumps(sanitize_value(obj, None, keep, home), separators=(",", ":")))
    return out


# ---------------------------------------------------------------- context


class Ctx:
    def __init__(self, out: Path, dry: bool):
        self.out = out
        self.art = out / "task4-artifacts"
        self.raw = out / "task4-raw.md"
        self.dry = dry
        self.uniq = f"{os.getpid() % 10000:04d}"
        self.tmp: Path | None = None
        self.fixtures: dict[str, Path] = {}
        self.cmds: list[tuple[list[str], object]] = []
        self.pgids: list[tuple[int, str | None]] = []   # (group leader pid, ps lstart)
        self.needles: list[str] = []
        self.codex_home: Path | None = None       # temp CODEX_HOME, once set up
        self.codex_home_mode = "not set up"
        self.auth_log: list[str] = []             # per Codex run: what happened to the auth link
        self.step4_types: list[str] = []          # item types Step 4 saw for commands and the edit
        self.notes: list[str] = []                # cleanup notes for the raw file

    def fixture(self, label: str) -> Path:
        if self.dry:
            return Path(f"<tmp>/lhprobe4 XXXX/{label} repo")
        if label not in self.fixtures:
            if self.tmp is None:
                self.tmp = Path(tempfile.mkdtemp(prefix="lhprobe4 ")).resolve()
                self.needles.append(str(self.tmp))
            self.fixtures[label] = make_fixture(self.tmp / f"{label} repo")
        return self.fixtures[label]


def make_fixture(repo: Path) -> Path:
    repo.mkdir(parents=True)
    (repo / "README.md").write_text("probe fixture\n")
    (repo / "notes.txt").write_text("probe notes\n")
    git = ["git", *GIT_SAFE, "-C", str(repo)]
    for args in (["init", "-q", "-b", "main"], ["add", "README.md", "notes.txt"],
                 ["commit", "-q", "--no-gpg-sign", "-m", "init"]):
        p = subprocess.run(git + args, capture_output=True, text=True, timeout=CMD_TIMEOUT,
                           stdin=subprocess.DEVNULL)
        if p.returncode != 0:
            raise StepFail(f"fixture git {args[0]} failed (rc={p.returncode})",
                           p.stderr.splitlines()[-10:])
    return repo.resolve()


def big_text(path: Path, lines: int, seed: int) -> str:
    """Write `lines` lines of exactly 100 bytes (`L00001 word word ...`); return the last line's tag."""
    rng = random.Random(seed)
    vocab = ["".join(rng.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(rng.randint(3, 9)))
             for _ in range(400)]
    out = []
    for n in range(1, lines + 1):
        s = f"L{n:05d}"
        while len(s) < 99:
            s += " " + rng.choice(vocab)
        out.append(s[:99] + "\n")
    path.write_text("".join(out))
    return f"L{lines:05d}"


def child_env(extra: dict | None = None) -> dict:
    env = dict(os.environ)
    env.pop("CLAUDECODE", None)
    env.update(extra or {})
    return env


def read_jsonl(path: Path) -> list[dict]:
    try:
        return parse_lines(path.read_text(errors="replace"))
    except OSError:
        return []


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


def find_keys(obj, pattern: re.Pattern, path: str = "") -> list[tuple[str, object]]:
    found = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            p = f"{path}.{k}" if path else k
            if pattern.search(str(k)):
                found.append((p, v if not isinstance(v, (dict, list)) else json.dumps(v)[:300]))
            found += find_keys(v, pattern, p)
    elif isinstance(obj, list):
        for v in obj:
            found += find_keys(v, pattern, path + "[]")
    return found


def kill_group(pgid: int) -> None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError):
            return
        time.sleep(1)


def proc_start(pid: int) -> str | None:
    """The process's start time from ps (None when ps fails or it is gone)."""
    try:
        p = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True, text=True,
                           timeout=CMD_TIMEOUT, stdin=subprocess.DEVNULL)
        return p.stdout.strip() or None
    except Exception:  # noqa: BLE001 - ps may be unavailable
        return None


def group_alive(pgid: int) -> bool:
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def track(ctx: Ctx, pid: int) -> None:
    ctx.pgids.append((pid, proc_start(pid)))


def reap(ctx: Ctx, pid: int, label: str) -> None:
    """Right after a run: kill what is left of its group, verify, stop tracking it."""
    if group_alive(pid):
        kill_group(pid)
        ctx.notes.append(f"{label}: killed processes left in group {pid} after the run")
    if group_alive(pid):
        ctx.notes.append(f"{label}: group {pid} still alive after SIGKILL; left for cleanup")
    else:
        ctx.pgids = [x for x in ctx.pgids if x[0] != pid]


def signal_group(pgid: int, sig) -> str:
    try:
        os.killpg(pgid, sig)
        return "sent"
    except ProcessLookupError:
        return "no such group"
    except PermissionError as e:
        return f"denied: {e}"


def ps_rows() -> list[dict]:
    p = subprocess.run(["ps", "-axo", "pid=,pgid=,command="], capture_output=True, text=True,
                       timeout=CMD_TIMEOUT, stdin=subprocess.DEVNULL)
    rows = []
    for line in p.stdout.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            rows.append({"pid": int(parts[0]), "pgid": int(parts[1]), "command": parts[2]})
    return [r for r in rows if r["pid"] != os.getpid()]


# ---------------------------------------------------------------- claude (steps 1-3)


def claude_argv(message: str) -> list[str]:
    # The prompt sits right after -p so the variadic --allowedTools can't swallow it.
    return ["claude", "-p", message, "--plugin-dir", str(PLUGIN_DIR), "--agent", DISPATCH_AGENT,
            "--model", "haiku", "--effort", "low", "--allowedTools", "Bash,Agent,Read",
            "--output-format", "stream-json", "--verbose"]


def dispatch_msg(agent: str, desc: str, prompt: str, opts: str = "") -> str:
    return (f"Call the Agent tool once with subagent_type \"{agent}\", description \"{desc}\""
            f"{opts}, and as prompt exactly the text between <<< and >>>: <<<{prompt}>>> "
            f"Then reply with one line: worker returned: <the first 200 characters of what it "
            f"returned>")


def run_claude(ctx: Ctx, label: str, message: str, cwd: Path, timeout: int) -> dict:
    stream = ctx.art / f"{label}-stream.jsonl"
    hook_log = ctx.art / f"{label}-hooks.jsonl"
    argv = claude_argv(message)
    t0 = time.monotonic()
    with open(stream, "w") as out, open(ctx.art / f"{label}-stderr.txt", "w") as err:
        p = subprocess.Popen(argv, cwd=cwd, stdin=subprocess.DEVNULL, stdout=out, stderr=err,
                             env=child_env({"PROBE_LOG": str(hook_log),
                                            "LEADHAND_RUN_ID": "probe4"}),
                             start_new_session=True)
        track(ctx, p.pid)
        try:
            rc = p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            kill_group(p.pid)
            p.wait(timeout=CMD_TIMEOUT)
            rc = "timeout"
        reap(ctx, p.pid, label)
    ctx.cmds.append((argv, rc))
    recs = read_jsonl(stream)
    init = next((r for r in recs if r.get("type") == "system" and r.get("subtype") == "init"), {})
    sid = init.get("session_id") or next((r.get("session_id") for r in recs if r.get("session_id")),
                                         None)
    found, main, subs = [], [], {}
    dest = ctx.art / f"{label}-transcripts"
    if sid:
        for pat in (f"*/{sid}.jsonl", f"*/{sid}/subagents/*.jsonl"):
            for path in sorted(CLAUDE_PROJECTS.glob(pat)):
                dest.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, dest / path.name)
                found.append(str(path))
                if path.name == f"{sid}.jsonl":
                    main = read_jsonl(path)
                else:
                    subs[path.name] = read_jsonl(path)
    result = next((r for r in reversed(recs) if r.get("type") == "result"), {})
    return {"argv": argv, "exit": rc, "elapsed_s": round(time.monotonic() - t0, 1), "stream": recs,
            "init_keys": sorted(init), "session_id": sid, "transcripts_found": found or "none",
            "main": main, "subs": subs, "result": result, "hooks": read_jsonl(hook_log),
            "stderr": (ctx.art / f"{label}-stderr.txt").read_text(errors="replace")}


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


def tool_uses(recs: list[dict]) -> list[dict]:
    return [{"idx": i, "id": c.get("id"), "name": c.get("name"), "input": c.get("input") or {}}
            for i, r in enumerate(recs) if r.get("type") == "assistant"
            for c in content_items(r) if c.get("type") == "tool_use"]


def tool_result_records(recs: list[dict]) -> dict:
    """tool_use_id -> (result text, the user record's toolUseResult, its keys)."""
    out = {}
    for r in recs:
        if r.get("type") == "user":
            for c in content_items(r):
                if c.get("type") == "tool_result":
                    tur = r.get("toolUseResult")
                    out[c.get("tool_use_id")] = {
                        "text": text_of(c.get("content", "")),
                        "is_error": c.get("is_error"),
                        "toolUseResult": tur,
                        "toolUseResult_keys": sorted(tur) if isinstance(tur, dict) else type(tur).__name__,
                    }
    return out


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
    extra = f" content={str(r.get('content'))[:150]!r}" if r.get("type") == "system" else ""
    return f"{r.get('type')}/{r.get('subtype', '')}:{extra} " + " | ".join(parts)


def assistant_turns(recs: list[dict]) -> list[dict]:
    """One entry per distinct message.id (records repeat per content block)."""
    turns: dict[str, dict] = {}
    for i, r in enumerate(recs):
        if r.get("type") != "assistant" or not isinstance(r.get("message"), dict):
            continue
        m = r["message"]
        mid = m.get("id") or f"<no id {i}>"
        t = turns.setdefault(mid, {"message_id": mid, "first_idx": i, "records": 0,
                                   "model": m.get("model"), "stop_reason": None, "usage": None,
                                   "tools": []})
        t["records"] += 1
        t["stop_reason"] = m.get("stop_reason") or t["stop_reason"]
        t["usage"] = m.get("usage") or t["usage"]
        t["tools"] += [c.get("name") for c in content_items(r) if c.get("type") == "tool_use"]
    return list(turns.values())


def context_tokens(usage) -> int | None:
    if not isinstance(usage, dict):
        return None
    return sum(int(usage.get(k) or 0) for k in
               ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"))


def record_kinds(recs: list[dict]) -> dict:
    kinds: dict[str, int] = {}
    for r in recs:
        k = f"{r.get('type')}/{r.get('subtype', '')}"
        kinds[k] = kinds.get(k, 0) + 1
    return kinds


def claude_common(r: dict) -> dict:
    res = r["result"]
    return {
        "claude_exit": r["exit"],
        "elapsed_s": r["elapsed_s"],
        "session_id": r["session_id"],
        "transcripts_found": r["transcripts_found"],
        "stream_result": {k: res.get(k) for k in ("subtype", "is_error", "num_turns",
                                                  "total_cost_usd", "stop_reason")} if res else "none",
        "stream_result_keys": sorted(res) if res else "none",
        "stderr_first_lines": r["stderr"].strip().splitlines()[:5] or "none",
    }


def dispatch_facts(r: dict) -> tuple[dict, dict | None]:
    disp = [u for u in tool_uses(r["main"]) if u["name"] in ("Agent", "Task")]
    res = tool_result_records(r["main"])
    if not disp:
        return {"dispatch_calls": "no Agent call in the main transcript"}, None
    d = disp[0]
    tr = res.get(d["id"])
    return {
        "dispatch_calls": len(disp),
        "dispatch_input": d["input"],
        "agent_result_text": (tr or {}).get("text", "<no tool_result>")[:800],
        "agent_result_is_error": (tr or {}).get("is_error"),
        "agent_toolUseResult": json.dumps((tr or {}).get("toolUseResult"), default=str)[:1200],
        "agent_toolUseResult_keys": (tr or {}).get("toolUseResult_keys"),
        "dispatcher_final_text": [c.get("text", "")[:300] for rec in r["main"][-5:]
                                  if rec.get("type") == "assistant"
                                  for c in content_items(rec) if c.get("type") == "text"][-2:],
    }, tr


def claude_excerpt(r: dict) -> list[str]:
    lines = [f"claude exit={r['exit']} elapsed={r['elapsed_s']}s session={r['session_id']}"]
    for name, recs in r["subs"].items():
        lines.append(f"{name}: {len(recs)} records; last: {summarize(recs[-1])[:200] if recs else '-'}")
    if r["result"]:
        lines.append(f"result: {r['result'].get('subtype')} {str(r['result'].get('result'))[:200]}")
    return lines[:10]


# ---------------------------------------------------------------- step 1


def step1_prompt(ctx: Ctx) -> str:
    cmds = ", ".join(f"`echo lhprobe-turn-{n}-{ctx.uniq}`" for n in range(1, 6))
    return (f"Run these five shell commands with the Bash tool, in this order, one tool call per "
            f"message: {cmds}. Then reply with one line: all five done")


def step1(ctx: Ctx):
    cwd = ctx.fixture("step1")
    r = run_claude(ctx, "step1", dispatch_msg("lhprobe:turns-worker", "turns worker",
                                              step1_prompt(ctx)), cwd, CLAUDE_TIMEOUT[1])
    obs = {"worker_prompt": step1_prompt(ctx)}
    obs.update(claude_common(r))
    d, _ = dispatch_facts(r)
    obs.update(d)
    marker = re.compile(rf"lhprobe-turn-(\d)-{ctx.uniq}")
    for name, recs in r["subs"].items():
        turns = assistant_turns(recs)
        cmds = [str(u["input"].get("command", "")) for u in tool_uses(recs) if u["name"] == "Bash"]
        results = tool_result_records(recs)
        obs[f"{name} assistant_records"] = sum(1 for x in recs if x.get("type") == "assistant")
        obs[f"{name} assistant_turns (distinct message.id)"] = len(turns)
        obs[f"{name} turns"] = [{k: t[k] for k in ("records", "stop_reason", "tools")} for t in turns]
        obs[f"{name} markers_run (by command text)"] = [m.group(1) for c in cmds
                                                        for m in [marker.search(c)] if m]
        obs[f"{name} tool_results"] = len(results)
        obs[f"{name} record_kinds"] = record_kinds(recs)
        obs[f"{name} last_records"] = [summarize(x)[:300] for x in recs[-3:]]
        obs[f"{name} last_record_keys"] = sorted(recs[-1]) if recs else "none"
        obs[f"{name} turn-related fields"] = sorted({f"{p}={v}" for x in recs
                                                     for p, v in find_keys(x, re.compile(r"turn", re.I))})[:20] or "none"
    if not r["subs"]:
        obs["subagent_transcripts"] = "none found"
    hook_cmds = [str(((h.get("hook_input") or {}).get("tool_input") or {}).get("command", ""))
                 for h in r["hooks"]]
    obs["hook_bash_markers (by command text)"] = [m.group(1) for c in hook_cmds
                                                  for m in [marker.search(c)] if m]
    obs["system_records_main"] = [summarize(x)[:300] for x in r["main"] if x.get("type") == "system"] or "none"
    obs["main turn-related fields"] = sorted({f"{p}={v}" for x in r["main"]
                                              for p, v in find_keys(x, re.compile(r"max.?turn", re.I))})[:20] or "none"
    n_sub_turns = [obs.get(f"{n} assistant_turns (distinct message.id)") for n in r["subs"]]
    summary = (f"subagent_turns={n_sub_turns or 'none'} markers={obs.get('hook_bash_markers (by command text)')} "
               f"is_error={obs.get('agent_result_is_error')}")
    return "OBSERVED", claude_excerpt(r), obs, summary


# ---------------------------------------------------------------- step 2


def step2(ctx: Ctx):
    cwd = ctx.fixture("step2")
    msg = dispatch_msg("lhprobe:worker", "override worker", "Run pwd.",
                       ', model "sonnet", effort "medium"')
    r = run_claude(ctx, "step2", msg, cwd, CLAUDE_TIMEOUT[2])
    obs = {"worker_frontmatter": "model: haiku, effort: low"}
    obs.update(claude_common(r))
    d, _ = dispatch_facts(r)
    obs.update(d)
    eff = re.compile(r"effort|thinking|budget", re.I)
    for name, recs in r["subs"].items():
        turns = assistant_turns(recs)
        first = next((x for x in recs if x.get("type") == "assistant"), {})
        obs[f"{name} message.model values"] = sorted({str(t["model"]) for t in turns}) or "none"
        obs[f"{name} effort-like fields"] = sorted({f"{x.get('type')}:{p}={v}" for x in recs
                                                    for p, v in find_keys(x, eff)})[:20] or "none"
        obs[f"{name} model-like fields"] = sorted({f"{x.get('type')}:{p}={v}" for x in recs
                                                   for p, v in find_keys(x, re.compile(r"model", re.I))})[:20]
        obs[f"{name} first_assistant_record_keys"] = sorted(first) or "none"
        obs[f"{name} first_assistant_message_keys"] = sorted(first.get("message") or {}) or "none"
        obs[f"{name} usage_keys"] = sorted((first.get("message") or {}).get("usage") or {}) or "none"
    if not r["subs"]:
        obs["subagent_transcripts"] = "none found"
    obs["main effort-like fields"] = sorted({f"{x.get('type')}:{p}={v}" for x in r["main"]
                                             for p, v in find_keys(x, eff)})[:20] or "none"
    obs["stream effort-like fields"] = sorted({f"{x.get('type')}:{p}={v}" for x in r["stream"]
                                               for p, v in find_keys(x, eff)})[:20] or "none"
    obs["stream subagent models (parent_tool_use_id set)"] = sorted({
        str((x.get("message") or {}).get("model")) for x in r["stream"]
        if x.get("type") == "assistant" and x.get("parent_tool_use_id")}) or "none"
    models = [obs.get(f"{n} message.model values") for n in r["subs"]]
    summary = f"subagent_models={models or 'none'} dispatch_input={obs.get('dispatch_input')}"
    return "OBSERVED", claude_excerpt(r), obs, summary


# ---------------------------------------------------------------- step 3

STEP3_LINES = 6000      # x 100 bytes = 600 KB
STEP3_CHUNK = 500       # lines = 50 KB


def step3_prompt(path: str) -> str:
    calls = ", ".join(f"offset {o} limit {STEP3_CHUNK}" for o in range(1, STEP3_LINES, STEP3_CHUNK))
    return (f"Use the Read tool on the file {path} {STEP3_LINES // STEP3_CHUNK} times, one call per "
            f"message, in this order: {calls}. Then reply with one line: last tag <the first word "
            f"of the last line you read>")


def step3(ctx: Ctx):
    cwd = ctx.fixture("step3")
    big = cwd / "big.txt"
    last_tag = big_text(big, STEP3_LINES, 3)
    r = run_claude(ctx, "step3", dispatch_msg("lhprobe:compact-worker", "compact worker",
                                              step3_prompt(str(big))), cwd, CLAUDE_TIMEOUT[3])
    obs = {"file_bytes": big.stat().st_size, "file_lines": STEP3_LINES,
           "chunk": f"{STEP3_CHUNK} lines = {STEP3_CHUNK * 100} bytes", "expected_last_tag": last_tag}
    obs.update(claude_common(r))
    d, _ = dispatch_facts(r)
    obs.update(d)
    comp = re.compile(r"compact", re.I)
    boundaries_total = 0
    for name, recs in r["subs"].items():
        series = []
        for t in assistant_turns(recs):
            series.append((t["first_idx"], context_tokens(t["usage"])))
        bounds = [(i, x) for i, x in enumerate(recs)
                  if "compact" in f"{x.get('type')}/{x.get('subtype', '')}".lower()
                  or any(comp.search(k) for k in x)]
        boundaries_total += len(bounds)
        reads = [u["input"] for u in tool_uses(recs) if u["name"] == "Read"]
        obs[f"{name} records"] = len(recs)
        obs[f"{name} record_kinds"] = record_kinds(recs)
        obs[f"{name} read_calls"] = len(reads)
        obs[f"{name} read_offsets"] = [x.get("offset") for x in reads]
        obs[f"{name} context_tokens_by_turn (record idx, input+cache)"] = series
        obs[f"{name} peak_context_tokens"] = max((c for _, c in series if c), default=None)
        obs[f"{name} compact_records"] = [{"idx": i, "kind": f"{x.get('type')}/{x.get('subtype', '')}",
                                           "keys": sorted(x),
                                           "compact_fields": [f"{p}={v}" for p, v in find_keys(x, comp)][:6]}
                                          for i, x in bounds] or "none"
        if bounds:
            i0 = bounds[0][0]
            before = [c for idx, c in series if idx < i0 and c]
            after = [c for idx, c in series if idx > i0 and c]
            obs[f"{name} context_before_first_compact"] = before[-1] if before else None
            obs[f"{name} context_after_first_compact"] = after[0] if after else None
        obs[f"{name} last_records"] = [summarize(x)[:300] for x in recs[-3:]]
    if not r["subs"]:
        obs["subagent_transcripts"] = "none found"
    obs["main compact records"] = [summarize(x)[:300] for x in r["main"]
                                   if "compact" in json.dumps(x)[:2000].lower()][:5] or "none"
    obs["stream compact records"] = [f"{x.get('type')}/{x.get('subtype', '')} keys={sorted(x)}"
                                     for x in r["stream"]
                                     if "compact" in f"{x.get('type')}/{x.get('subtype', '')}".lower()][:5] or "none"
    peaks = [obs.get(f"{n} peak_context_tokens") for n in r["subs"]]
    summary = f"compact_records={boundaries_total} peak_context={peaks or 'none'}"
    return "OBSERVED", claude_excerpt(r), obs, summary


# ---------------------------------------------------------------- codex plumbing


def setup_codex_home(ctx: Ctx) -> None:
    """Temp CODEX_HOME with the operator's auth.json and a minimal config (once)."""
    if ctx.codex_home_mode != "not set up":
        return
    auth = REAL_CODEX_HOME / "auth.json"
    if ctx.dry:
        ctx.codex_home = Path("<tmp>/lhprobe4-codex-home-XXXX")
        ctx.codex_home_mode = "temp"
        return
    if not auth.exists():
        ctx.codex_home_mode = ("real (no auth.json to copy; step 7 uses -c true then -c false; "
                               "step 8 default variants use --ignore-user-config)")
        return
    home = Path(tempfile.mkdtemp(prefix="lhprobe4-codex-home-")).resolve()
    ctx.codex_home = home                 # first, so cleanup removes it whatever happens next
    ctx.needles.append(str(home))
    os.symlink(auth, home / "auth.json")  # never a copy of the credentials
    (home / "config.toml").write_text(
        "# leadhand Task 4 probe: temporary CODEX_HOME, removed at cleanup\n"
        f'model = "{CODEX_MODEL}"\n'
        'model_reasoning_effort = "low"\n'
        "\n[sandbox_workspace_write]\nnetwork_access = true\n")
    ctx.codex_home_mode = "temp"
    progress(f"  codex: temporary CODEX_HOME {home}")


def temp_home(ctx: Ctx) -> bool:
    return ctx.codex_home_mode == "temp"


def codex_env(ctx: Ctx, real_home: bool = False, extra: dict | None = None) -> dict:
    env = child_env(extra)
    if not real_home and temp_home(ctx):
        env["CODEX_HOME"] = str(ctx.codex_home)
    return env


def codex_argv(*extra: str) -> list[str]:
    return ["codex", "exec", *CODEX_PIN, *CODEX_JSON, *extra, "-"]


def run_codex(ctx: Ctx, label: str, argv: list[str], prompt: str | None, cwd: Path, env: dict,
              timeout: int = CODEX_TIMEOUT, redact=None) -> dict:
    """Run one codex exec to completion; prompt None means the argv carries it (stdin closed)."""
    t0 = time.monotonic()
    p = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE,
                         stdin=subprocess.DEVNULL if prompt is None else subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True, start_new_session=True)
    track(ctx, p.pid)
    try:
        out, err = p.communicate(prompt, timeout=timeout)
        rc = p.returncode
    except subprocess.TimeoutExpired:
        kill_group(p.pid)
        out, err = p.communicate(timeout=CMD_TIMEOUT)
        rc = "timeout"
    reap(ctx, p.pid, label)
    check_auth(ctx, label)
    if redact:
        out, err = redact(out), redact(err)
    (ctx.art / f"{label}-stream.jsonl").write_text(out or "")
    (ctx.art / f"{label}-stderr.txt").write_text(err or "")
    ctx.cmds.append((argv, rc))
    return {"exit": rc, "stdout": out or "", "stderr": err or "", "events": parse_lines(out or ""),
            "elapsed_s": round(time.monotonic() - t0, 1)}


def items(events: list[dict], etype: str = "item.completed") -> list[dict]:
    return [e["item"] for e in events if e.get("type") == etype and isinstance(e.get("item"), dict)]


def thread_id(events: list[dict]) -> str | None:
    for e in events:
        if e.get("thread_id"):
            return str(e["thread_id"])
    return None


def type_counts(values) -> dict:
    out: dict[str, int] = {}
    for v in values:
        out[str(v)] = out.get(str(v), 0) + 1
    return out


def keys_by(events: list[dict], kind) -> dict:
    out: dict[str, set] = {}
    for e in events:
        k = kind(e)
        if k is not None:
            out.setdefault(str(k), set()).update(e)
    return {k: sorted(v) for k, v in out.items()}


def agent_messages(events: list[dict]) -> list[str]:
    return [str(i.get("text"))[:200] for i in items(events) if i.get("type") == "agent_message"]


def codex_common(r: dict) -> dict:
    ev = r["events"]
    tc = [e for e in ev if e.get("type") == "turn.completed"]
    return {
        "codex_exit": r["exit"],
        "elapsed_s": r["elapsed_s"],
        "thread_id": thread_id(ev),
        "event_type_counts": type_counts(e.get("type", "unparsed" if "unparsed" in e else "?")
                                         for e in ev),
        "last_event": ev[-1].get("type") if ev else None,
        "turn_completed_usage": tc[-1].get("usage") if tc else "no turn.completed",
        "errors": [json.dumps(e)[:300] for e in ev if e.get("type") in ("error", "turn.failed")] or "none",
        "agent_messages": agent_messages(ev),
        "stderr_first_lines": r["stderr"].strip().splitlines()[:5] or "none",
    }


def codex_excerpt(r: dict) -> list[str]:
    lines = [f"codex exit={r['exit']} elapsed={r['elapsed_s']}s"]
    lines += [f"[stream] {l[:160]}" for l in r["stdout"].splitlines()[-4:]]
    lines += [f"[stderr] {l[:160]}" for l in r["stderr"].strip().splitlines()[:3]]
    return lines


def rollout_file(ctx: Ctx, tid: str | None, real_home: bool = False) -> Path | None:
    if not tid:
        return None
    home = REAL_CODEX_HOME if real_home or not temp_home(ctx) else ctx.codex_home
    hits = sorted((home / "sessions").glob(f"*/*/*/*{tid}*.jsonl"))
    return hits[-1] if hits else None


# ---------------------------------------------------------------- step 4


def step4_prompt(ctx: Ctx) -> str:
    cmds = ", ".join(f"`echo lhprobe-cmd-{n}-{ctx.uniq}`" for n in range(1, 6))
    return (f"Run these five shell commands one at a time, each as its own separate command (do not "
            f"combine them), in this order: {cmds}. Then edit the file notes.txt in the current "
            f"directory with your file editing tool (apply_patch), not with a shell command: append "
            f"the line `lhprobe edit {ctx.uniq}`. Then reply with one line: done")


def step4(ctx: Ctx):
    setup_codex_home(ctx)
    cwd = ctx.fixture("codex4")
    r = run_codex(ctx, "step4", codex_argv(*CODEX_WS), step4_prompt(ctx), cwd, codex_env(ctx))
    ev = r["events"]
    obs = {"prompt": step4_prompt(ctx), "codex_home": ctx.codex_home_mode}
    obs.update(codex_common(r))
    marker = re.compile(rf"lhprobe-cmd-(\d)-{ctx.uniq}")
    done = items(ev)
    classified = []
    for it in done:
        m = marker.search(json.dumps(it.get("command", "")))
        role = f"cmd{m.group(1)}" if m else None
        if role is None and "notes.txt" in json.dumps(it.get("changes", "")):
            role = "edit notes.txt"
        classified.append({"item.type": it.get("type"), "role (by probe text)": role or "other",
                           "keys": sorted(it)})
    obs["item.completed (in order)"] = classified
    obs["item.completed item.type counts"] = type_counts(i.get("type") for i in done)
    obs["item.type carrying the 5 commands"] = sorted({c["item.type"] for c in classified
                                                       if str(c["role (by probe text)"]).startswith("cmd")}) or "none"
    obs["commands_seen"] = sorted({c["role (by probe text)"] for c in classified
                                   if str(c["role (by probe text)"]).startswith("cmd")})
    obs["item.type carrying the edit"] = sorted({c["item.type"] for c in classified
                                                 if c["role (by probe text)"] == "edit notes.txt"}) or "none"
    ctx.step4_types = sorted({c["item.type"] for c in classified
                              if c["role (by probe text)"] != "other" and c["item.type"]})
    obs["item.started item.type counts"] = type_counts(i.get("type") for i in items(ev, "item.started"))
    obs["item.updated item.type counts"] = type_counts(i.get("type") for i in items(ev, "item.updated"))
    obs["keys by event type"] = keys_by(ev, lambda e: e.get("type"))
    obs["keys by item.type"] = {k: sorted(v) for k, v in
                                keys_by(done, lambda i: i.get("type")).items()}
    tc = [e for e in ev if e.get("type") == "turn.completed"]
    obs["turn.completed usage field names"] = sorted((tc[-1].get("usage") or {})) if tc else "none"
    obs["file_change items"] = [json.dumps(sanitize_value(i), default=str)[:300]
                                for i in done if i.get("type") == "file_change"] or "none"
    obs["notes.txt now"] = (cwd / "notes.txt").read_text()[-200:]
    raw = r["stdout"].splitlines()
    cand = ctx.art / "codex-exec-stream.sanitized.jsonl"
    cand.write_text("\n".join(sanitize_stream(raw)) + ("\n" if raw else ""))
    obs["sanitized_candidate"] = f"{cand.name} ({len(raw)} lines)"
    summary = (f"cmd_types={obs['item.type carrying the 5 commands']} "
               f"edit_types={obs['item.type carrying the edit']} "
               f"usage_fields={obs['turn.completed usage field names']}")
    return "OBSERVED", codex_excerpt(r), obs, summary


# ---------------------------------------------------------------- step 5


def step5_prompt(ctx: Ctx) -> str:
    cmds = ", ".join(f"`sleep 4; echo lhprobe-stop-{n}-{ctx.uniq}`" for n in range(1, 6))
    return (f"Run these five shell commands one at a time, each as its own separate command, in "
            f"this order: {cmds}. Then reply with one line: done")


def step5_resume_variants(tid: str) -> list[tuple[str, list[str]]]:
    return [
        ("flags-before-resume", ["codex", "exec", *CODEX_PIN, *CODEX_JSON, "resume", tid, "-"]),
        ("flags-after-resume", ["codex", "exec", "resume", *CODEX_PIN, *CODEX_JSON, tid, "-"]),
    ]


def step5(ctx: Ctx):
    setup_codex_home(ctx)
    cwd = ctx.fixture("codex5")
    argv = codex_argv(*CODEX_WS)
    stream_path = ctx.art / "step5-stream.jsonl"
    counted_types = tuple(ctx.step4_types) or COUNTED_ITEM_TYPES
    obs = {"prompt": step5_prompt(ctx), "codex_home": ctx.codex_home_mode,
           "counted": f"item.completed with item.type in {list(counted_types)}",
           "counted_types_source": ("Step 4's observed command/edit item types" if ctx.step4_types
                                    else "guess (Step 4 did not run or saw none)"),
           "sigint_target": ("the whole process group: os.killpg(<codex pid>) with Codex started "
                             "with process_group=0, so its pgid is its pid"),
           "stop_after_counted_items": STOP_AFTER_ITEMS}
    t0 = time.monotonic()
    err_f = open(ctx.art / "step5-stderr.txt", "w")
    p = subprocess.Popen(argv, cwd=cwd, env=codex_env(ctx), stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=err_f, text=True, process_group=0)
    track(ctx, p.pid)
    p.stdin.write(step5_prompt(ctx))
    p.stdin.close()
    q: queue.Queue = queue.Queue()

    def reader():
        with open(stream_path, "w") as f:
            for line in p.stdout:
                f.write(line)
                f.flush()
                q.put((round(time.monotonic() - t0, 2), line))
        q.put(None)

    th = threading.Thread(target=reader, daemon=True)
    th.start()
    events: list[tuple[float, dict]] = []
    counted, sig_log, sigint_at, exit_seen = [], [], None, None
    eof = False
    deadline = t0 + CODEX_TIMEOUT
    while time.monotonic() < deadline:
        if eof and (sigint_at is None or p.poll() is not None):
            break
        try:
            got = q.get(timeout=1) if not eof else False
        except queue.Empty:
            got = False
        if eof and not got:
            time.sleep(0.5)
        if got is None:
            eof = True
        elif got:
            t, line = got
            e = parse_lines(line)
            e = e[0] if e else {"unparsed": line}
            events.append((t, e))
            it = e.get("item") if isinstance(e.get("item"), dict) else {}
            if e.get("type") == "item.completed" and it.get("type") in counted_types:
                counted.append({"t": t, "item.type": it.get("type"),
                                "command (probe text)": str(it.get("command", ""))[:120]})
                if len(counted) == STOP_AFTER_ITEMS and sigint_at is None:
                    sent = signal_group(p.pid, signal.SIGINT)
                    sigint_at = time.monotonic()
                    sig_log.append(f"SIGINT to pgid {p.pid} at {round(sigint_at - t0, 2)}s ({sent})")
                    progress(f"  step5: {STOP_AFTER_ITEMS} items counted; SIGINT sent")
        if sigint_at is not None and exit_seen is None and p.poll() is not None:
            exit_seen = time.monotonic()
        if sigint_at is not None and p.poll() is None:
            since = time.monotonic() - sigint_at
            if since >= STOP_GRACE and not any(s.startswith("SIGTERM") for s in sig_log):
                sent = signal_group(p.pid, signal.SIGTERM)
                sig_log.append(f"SIGTERM to pgid {p.pid} at {round(time.monotonic() - t0, 2)}s ({sent})")
            elif since >= 2 * STOP_GRACE and not any(s.startswith("SIGKILL") for s in sig_log):
                signal_group(p.pid, signal.SIGKILL)
                sig_log.append(f"SIGKILL to pgid {p.pid} at {round(time.monotonic() - t0, 2)}s "
                               "(safety net, not part of the harness)")
    if p.poll() is None and sigint_at is None:
        kill_group(p.pid)
        sig_log.append("timeout: never counted enough items; group killed")
    try:
        rc = p.wait(timeout=CMD_TIMEOUT)
    except subprocess.TimeoutExpired:
        kill_group(p.pid)
        rc = p.wait(timeout=CMD_TIMEOUT)
    exited_at = exit_seen or time.monotonic()
    th.join(timeout=10)
    err_f.close()
    ctx.cmds.append((argv, rc))
    check_auth(ctx, "step5")
    try:
        group_left = [f"{r['pid']} {r['command'][:100]}" for r in ps_rows() if r["pgid"] == p.pid]
    except Exception as e:  # noqa: BLE001 - recorded
        group_left = [f"0 ps failed: {e!r}"]
    after = [e for t, e in events if sigint_at is not None and t >= sigint_at - t0]
    evs = [e for _, e in events]
    tid = thread_id(evs)
    stderr = (ctx.art / "step5-stderr.txt").read_text(errors="replace")
    obs.update({
        "counted_items": counted,
        "all item.completed types": [i.get("type") for i in items(evs)],
        "signals": sig_log or "none",
        "exit_code": rc,
        "exit_signal": signal.Signals(-rc).name if isinstance(rc, int) and rc < 0 else "none",
        "exit_after_sigint_s (+-1 s poll)": round(exited_at - sigint_at, 2) if sigint_at else None,
        "events_after_sigint": [e.get("type") for e in after],
        "last_event": evs[-1].get("type") if evs else None,
        "turn_completed_or_failed_after_sigint": [json.dumps(e)[:300] for e in after
                                                  if e.get("type") in ("turn.completed", "turn.failed",
                                                                       "error")] or "none",
        "codex_group_left": group_left or "none",
        "stderr_last_lines": stderr.strip().splitlines()[-5:] or "none",
        "thread_id": tid,
    })
    reap(ctx, p.pid, "step5")
    rf = rollout_file(ctx, tid)
    obs["rollout_file_found"] = bool(rf)
    if rf:
        shutil.copy2(rf, ctx.art / f"step5-rollout-{rf.name}")
    excerpt = [f"counted={len(counted)} signals={sig_log} exit={rc}"]
    excerpt += [f"[stream] {json.dumps(e)[:150]}" for e in evs[-3:]]
    if not tid:
        obs["resume"] = "not attempted: no thread id in the stream"
        return "OBSERVED", excerpt, obs, f"exit={rc} resume=not attempted"
    worked = []
    for name, rargv in step5_resume_variants(tid):
        progress(f"  step5: resume ({name})")
        rr = run_codex(ctx, f"step5-resume-{name}", rargv,
                       "Do not run any commands. Reply with one line: resumed", cwd, codex_env(ctx))
        rev = rr["events"]
        obs[f"resume {name}"] = {
            "exit": rr["exit"], "thread_id": thread_id(rev), "same_thread": thread_id(rev) == tid,
            "event_types": list(dict.fromkeys(e.get("type") for e in rev)),
            "agent_messages": agent_messages(rev),
            "usage": next((e.get("usage") for e in rev if e.get("type") == "turn.completed"), None),
            "stderr_first_lines": rr["stderr"].strip().splitlines()[:5],
        }
        excerpt.append(f"resume {name}: exit={rr['exit']} msgs={agent_messages(rev)[:1]}")
        if rr["exit"] == 0:
            worked.append(name)
            break   # one working form is enough
    obs["resume_variants_that_worked"] = worked or "none"
    summary = (f"exit={rc} signal={obs['exit_signal']} after_sigint={obs['exit_after_sigint_s (+-1 s poll)']}s "
               f"sigterm_needed={any(s.startswith('SIGTERM') for s in sig_log)} resume={worked or 'none'}")
    return "OBSERVED", excerpt[:10], obs, summary


# ---------------------------------------------------------------- step 6

STEP6_LINES = 3200      # x 100 bytes = 320 KB, about 80K tokens
STEP6_CHUNK = 80        # lines per sed call: 8 KB, under Codex's output truncation


def step6_check_argv(key_value: str, real: bool = False) -> list[str]:
    return ["codex", "exec", "--strict-config", "-c", key_value, *CODEX_PIN, *CODEX_JSON,
            *(NO_NOTIFY if real else []), "reply ok"]


def step6_prompt() -> str:
    calls = ", ".join(f"`sed -n '{a},{a + STEP6_CHUNK - 1}p' big80.txt`"
                      for a in range(1, STEP6_LINES, STEP6_CHUNK))
    return (f"Run these {STEP6_LINES // STEP6_CHUNK} shell commands one at a time, each as its own "
            f"separate command, in this order, and read each output: {calls}. Do not combine them. "
            f"Then reply with one line: last tag <the first word of the last line printed>")


def step6(ctx: Ctx):
    setup_codex_home(ctx)
    cwd = ctx.fixture("codex6")
    obs = {"codex_home": ctx.codex_home_mode,
           "operator_config (names only)": operator_config_facts()}
    excerpt = []
    checks = [("bogus-key-control", "lhprobe_bogus_key=1", False),
              ("compact-key", "model_auto_compact_token_limit=60000", False),
              ("compact-key-real-home", "model_auto_compact_token_limit=60000", True)]
    accepted = None
    for name, kv, real in checks:
        if real and not temp_home(ctx):
            obs[name] = "same as compact-key (already the real home)"
            continue
        progress(f"  step6: {name}")
        rr = run_codex(ctx, f"step6-{name}", step6_check_argv(kv, real), None, cwd,
                       codex_env(ctx, real_home=real), timeout=180,
                       redact=redact_stream if real else None)
        key = kv.split("=")[0]
        obs[name] = {
            "argv": shlex.join(step6_check_argv(kv, real)),
            "home": "operator's real CODEX_HOME" if real else ctx.codex_home_mode,
            "exit": rr["exit"],
            "stderr_lines_naming_key": [l[:200] for l in rr["stderr"].splitlines() if key in l] or "none",
            "stderr_first_lines": rr["stderr"].strip().splitlines()[:5] or "none",
            "agent_messages": agent_messages(rr["events"]),
            "last_event": rr["events"][-1].get("type") if rr["events"] else None,
        }
        excerpt.append(f"{name}: exit={rr['exit']} {(rr['stderr'].strip().splitlines() or [''])[0][:120]}")
        if name == "compact-key":
            accepted = rr["exit"] == 0 and not any(key in l for l in rr["stderr"].splitlines())
    obs["compact_key_accepted (exit 0, no stderr line naming it)"] = accepted
    if not accepted:
        obs["compaction_run"] = "not run: the key was not accepted"
        return "OBSERVED", excerpt, obs, f"key_accepted={accepted} compaction_run=no"
    last_tag = big_text(cwd / "big80.txt", STEP6_LINES, 6)
    argv = codex_argv(*CODEX_WS, "-c", "model_auto_compact_token_limit=60000")
    progress(f"  step6: compaction run ({STEP6_LINES // STEP6_CHUNK} sed calls; several minutes)")
    r = run_codex(ctx, "step6-compaction", argv, step6_prompt(), cwd, codex_env(ctx),
                  timeout=CODEX_LONG_TIMEOUT)
    ev = r["events"]
    obs["file"] = f"big80.txt {STEP6_LINES} lines x 100 bytes, expected last tag {last_tag}"
    obs.update({f"run {k}": v for k, v in codex_common(r).items()})
    obs["run commands_completed"] = sum(1 for i in items(ev) if i.get("type") == "command_execution")
    obs["run item.completed types"] = type_counts(i.get("type") for i in items(ev))
    obs["run stream types naming compact"] = sorted({
        f"{e.get('type')}/{(e.get('item') or {}).get('type') if isinstance(e.get('item'), dict) else ''}"
        for e in ev if "compact" in json.dumps(e.get("type")).lower()
        or (isinstance(e.get("item"), dict) and "compact" in str(e["item"].get("type")).lower())}) or "none"
    obs["run turn.completed usages"] = [e.get("usage") for e in ev if e.get("type") == "turn.completed"]
    rf = rollout_file(ctx, thread_id(ev))
    obs["rollout_file_found"] = bool(rf)
    if rf:
        shutil.copy2(rf, ctx.art / f"step6-rollout-{rf.name}")
        recs = read_jsonl(rf)
        kinds = type_counts(f"{x.get('type')}/{(x.get('payload') or {}).get('type') if isinstance(x.get('payload'), dict) else ''}"
                            for x in recs)
        obs["rollout record kinds (type/payload.type)"] = kinds
        obs["rollout kinds naming compact"] = {k: v for k, v in kinds.items() if "compact" in k.lower()} or "none"
        series = []
        for x in recs:
            pl = x.get("payload") if isinstance(x.get("payload"), dict) else {}
            info = pl.get("info") if isinstance(pl.get("info"), dict) else {}
            if pl.get("type") == "token_count" and isinstance(info.get("last_token_usage"), dict):
                series.append(info["last_token_usage"].get("input_tokens"))
        obs["rollout last_token_usage.input_tokens series"] = series[:80]
        obs["rollout input_tokens drops (prev -> next)"] = [
            f"{a}->{b}" for a, b in zip(series, series[1:]) if a and b and b < a * 0.7] or "none"
    summary = (f"key_accepted={accepted} stream_compact={obs['run stream types naming compact']} "
               f"rollout_compact={obs.get('rollout kinds naming compact', 'no rollout')}")
    return "OBSERVED", (excerpt + codex_excerpt(r))[:10], obs, summary


# ---------------------------------------------------------------- step 7

CURL = "curl -sS -m 20 -o /dev/null -w '%{http_code}' https://api.github.com/zen"


def step7_prompt() -> str:
    return (f"Run exactly this shell command once, unchanged, and do not retry it or run anything "
            f"else: `{CURL}` Then reply with one line: curl said <its output> exit <its exit code>")


def step7_variants(ctx: Ctx) -> list[tuple[str, list[str]]]:
    t, f = "sandbox_workspace_write.network_access=true", "sandbox_workspace_write.network_access=false"
    if temp_home(ctx):
        return [("config-true (control)", []), ("config-true + -c false", ["-c", f])]
    return [("-c true (control)", ["-c", t]), ("-c true then -c false", ["-c", t, "-c", f]),
            ("-c false then -c true", ["-c", f, "-c", t])]


def step7(ctx: Ctx):
    setup_codex_home(ctx)
    cwd = ctx.fixture("codex7")
    obs = {"codex_home": ctx.codex_home_mode,
           "user_config": ("temp CODEX_HOME config.toml sets [sandbox_workspace_write] "
                           "network_access = true" if temp_home(ctx) else
                           "real home; network_access set with -c (order recorded)"),
           "prompt": step7_prompt()}
    excerpt, line = [], []
    for name, extra in step7_variants(ctx):
        progress(f"  step7: {name}")
        argv = codex_argv(*CODEX_WS, *extra)
        r = run_codex(ctx, f"step7-{re.sub(r'[^a-z0-9]+', '-', name.lower()).strip('-')}", argv,
                      step7_prompt(), cwd, codex_env(ctx))
        curls = [i for i in items(r["events"]) if i.get("type") == "command_execution"
                 and "api.github.com/zen" in str(i.get("command", ""))]
        obs[name] = {
            "argv": shlex.join(argv),
            "codex_exit": r["exit"],
            "curl_items": [{"exit_code": i.get("exit_code"), "status": i.get("status"),
                            "output": str(i.get("aggregated_output", ""))[:200]} for i in curls] or "none",
            "other_commands": [str(i.get("command"))[:120] for i in items(r["events"])
                               if i.get("type") == "command_execution" and i not in curls] or "none",
            "agent_messages": agent_messages(r["events"]),
            "stderr_first_lines": r["stderr"].strip().splitlines()[:3] or "none",
        }
        c = curls[0] if curls else {}
        line.append(f"{name}: curl_exit={c.get('exit_code')} out={str(c.get('aggregated_output', ''))[:20]!r}")
        excerpt.append(line[-1])
    return "OBSERVED", excerpt, obs, "; ".join(line)


# ---------------------------------------------------------------- step 8

STEP8_ENV_CMD = "env | grep -E '^(GIT_|GH_|SSH_)'"
STEP8_GIT_CMD = "git config --get-all credential.helper"


def step8_prompt() -> str:
    return (f"Run exactly these two shell commands, unchanged, one at a time, as two separate "
            f"commands: `{STEP8_ENV_CMD}` and then `{STEP8_GIT_CMD}`. Then reply with one line: done")


def redact_env_text(text: str) -> str:
    """Replace the value of every NAME=value line that is not one of the probe's dummy pairs
    (SSH_AUTH_SOCK and SSH_AGENT_PID are kept), so even a plain `env` leaks nothing."""
    known = set(ENV_VARS.items()) | set(SET_VARS.items())
    out = []
    for line in text.split("\n"):
        m = ANY_ENV_LINE.match(line)
        if m and (m.group(1), m.group(2)) not in known and m.group(1) not in SAFE_SSH:
            line = f"{m.group(1)}=<redacted len={len(m.group(2))}>"
        out.append(line)
    return "\n".join(out)


def redact_obj(v):
    if isinstance(v, dict):
        return {k: redact_obj(x) for k, x in v.items()}
    if isinstance(v, list):
        return [redact_obj(x) for x in v]
    return redact_env_text(v) if isinstance(v, str) else v


def redact_stream(text: str) -> str:
    """Redact a JSONL stream line by line (stderr and unparsable lines as text)."""
    out = []
    for line in (text or "").splitlines():
        try:
            out.append(json.dumps(redact_obj(json.loads(line))))
        except ValueError:
            out.append(redact_env_text(line))
    return "\n".join(out) + ("\n" if out else "")


def set_args(vars_: dict) -> list[str]:
    out = []
    for k, v in vars_.items():
        out += ["-c", f"shell_environment_policy.set.{k}={json.dumps(v)}"]
    return out


def step8_variants(ctx: Ctx) -> list[tuple[str, list[str], bool]]:
    """(name, extra argv, real_home)."""
    core = ["-c", 'shell_environment_policy.inherit="core"']
    dflt = [] if temp_home(ctx) else ["--ignore-user-config"]
    v = [("default policy, env only", dflt, False),
         ("default policy, env + set", dflt + set_args(SET_VARS), False),
         ("inherit=core, env only", dflt + core, False),
         ("inherit=core, env + set", dflt + core + set_args(SET_VARS), False)]
    if temp_home(ctx):
        v.append(("operator config, env + set", NO_NOTIFY + set_args(SET_VARS), True))
    return v


def operator_env_policy() -> str:
    try:
        cfg = tomllib.loads((REAL_CODEX_HOME / "config.toml").read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        return f"unreadable ({type(e).__name__})"
    pol = cfg.get("shell_environment_policy")
    if not isinstance(pol, dict):
        return "no [shell_environment_policy] (Codex default)"
    return json.dumps({k: (sorted(v) if isinstance(v, dict) else v) for k, v in pol.items()})


def operator_config_facts() -> dict | str:
    """MCP server names and whether notify is set in the operator's config (names only)."""
    try:
        cfg = tomllib.loads((REAL_CODEX_HOME / "config.toml").read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        return f"unreadable ({type(e).__name__})"
    mcp = cfg.get("mcp_servers")
    return {"mcp_servers": sorted(mcp) if isinstance(mcp, dict) else [],
            "notify_set": bool(cfg.get("notify")),
            "real-home runs pass": shlex.join(NO_NOTIFY)}


def step8(ctx: Ctx):
    setup_codex_home(ctx)
    cwd = ctx.fixture("codex8")
    base_env = child_env()
    dropped = sorted(k for k in base_env if ENV_PREFIX.match(k) and k not in SAFE_SSH)
    for k in dropped:
        base_env.pop(k)
    base_env.update(ENV_VARS)
    if temp_home(ctx):
        base_env["CODEX_HOME"] = str(ctx.codex_home)
    obs = {"codex_home": ctx.codex_home_mode,
           "process_env_dummies": ENV_VARS,
           "set_dummies": SET_VARS,
           "inherited_GIT_/GH_ names dropped from the process env": dropped or "none",
           "operator_config shell_environment_policy (set: names only)": operator_env_policy(),
           "operator_config (names only)": operator_config_facts(),
           "prompt": step8_prompt()}
    excerpt, line = [], []
    for name, extra, real in step8_variants(ctx):
        progress(f"  step8: {name}")
        env = dict(base_env)
        if real:
            env.pop("CODEX_HOME", None)
            if os.environ.get("CODEX_HOME"):
                env["CODEX_HOME"] = os.environ["CODEX_HOME"]
        argv = codex_argv(*CODEX_WS, *extra)
        r = run_codex(ctx, f"step8-{re.sub(r'[^a-z0-9]+', '-', name.lower()).strip('-')}", argv,
                      step8_prompt(), cwd, env, redact=redact_stream)
        cmds = [i for i in items(r["events"]) if i.get("type") == "command_execution"]
        envi = [i for i in cmds if "grep" in str(i.get("command", "")) and "GH_" in str(i.get("command", ""))]
        giti = [i for i in cmds if "credential.helper" in str(i.get("command", ""))]
        seen = {}
        for i in envi:
            for l in str(i.get("aggregated_output", "")).splitlines():
                m = ENV_LINE.match(l)
                if m:
                    seen[m.group(1)] = m.group(2)
        reach = {k: seen.get(k, "<absent>") for k in sorted(set(ENV_VARS) | set(SET_VARS))}
        obs[name] = {
            "argv": shlex.join(argv),
            "home": "operator's real CODEX_HOME" if real else ctx.codex_home_mode,
            "codex_exit": r["exit"],
            "probe_vars_in_shell": reach,
            "GH_TOKEN_source": {"lhprobe-dummy-env": "process env", "lhprobe-dummy-set": "set"}.get(
                seen.get("GH_TOKEN", ""), "absent" if "GH_TOKEN" not in seen else "other (redacted)"),
            "other_GIT_/GH_/SSH_ names in shell": sorted(set(seen) - set(reach)) or "none",
            "env_command": [{"command": str(i.get("command"))[:120], "exit_code": i.get("exit_code")}
                            for i in envi] or "not run",
            "credential_helper_command": [{"command": str(i.get("command"))[:120],
                                           "exit_code": i.get("exit_code"),
                                           "output_lines": str(i.get("aggregated_output", "")).splitlines()[:6]}
                                          for i in giti] or "not run",
            "stderr_first_lines": r["stderr"].strip().splitlines()[:3] or "none",
        }
        line.append(f"{name}: GH_TOKEN={obs[name]['GH_TOKEN_source']} "
                    f"KEY_0={'yes' if 'GIT_CONFIG_KEY_0' in seen else 'no'}")
        excerpt.append(line[-1])
    return "OBSERVED", excerpt, obs, "; ".join(line)


STEP_FUNCS = {1: step1, 2: step2, 3: step3, 4: step4, 5: step5, 6: step6, 7: step7, 8: step8}


# ---------------------------------------------------------------- dry run


def dry_commands(ctx: Ctx, n: int) -> list[str]:
    cl = lambda msg: shlex.join(claude_argv(msg)) + "  (cwd=<tmp>/lhprobe4 XXXX/step<N> repo)"
    if n in (1, 2, 3):
        setup = []
        if n == 1:
            msg = dispatch_msg("lhprobe:turns-worker", "turns worker", step1_prompt(ctx))
        elif n == 2:
            msg = dispatch_msg("lhprobe:worker", "override worker", "Run pwd.",
                               ', model "sonnet", effort "medium"')
        else:
            setup = [f"generate big.txt: {STEP3_LINES} lines x 100 bytes in the step3 fixture"]
            msg = dispatch_msg("lhprobe:compact-worker", "compact worker",
                               step3_prompt("<fixture>/big.txt"))
        return setup + [cl(msg)]
    setup_codex_home(ctx)
    home = (f"CODEX_HOME={ctx.codex_home}  (temp; auth.json is a symlink to the operator's, "
            "minimal config.toml)")
    if n == 4:
        return [home, shlex.join(codex_argv(*CODEX_WS)) + f"  < {step4_prompt(ctx)!r}",
                "write task4-artifacts/codex-exec-stream.sanitized.jsonl"]
    if n == 5:
        return [home, shlex.join(codex_argv(*CODEX_WS)) + f"  < {step5_prompt(ctx)!r}",
                f"  after {STOP_AFTER_ITEMS} item.completed of Step 4's observed types "
                f"(else {list(COUNTED_ITEM_TYPES)}): "
                f"kill -INT -<pgid>; {STOP_GRACE}s later kill -TERM -<pgid>; another {STOP_GRACE}s "
                "later kill -KILL -<pgid>"]  + [
                "  then, until one exits 0: " + shlex.join(a) + "  < 'Do not run any commands...'"
                for _, a in step5_resume_variants("<thread_id>")]
    if n == 6:
        return [home] + [shlex.join(step6_check_argv(kv)) + note for kv, note in (
            ("lhprobe_bogus_key=1", "  (control)"), ("model_auto_compact_token_limit=60000", ""),
            ("model_auto_compact_token_limit=60000", "  (again with the operator's real CODEX_HOME, plus -c notify=[])"))] + [
            "if accepted: " + shlex.join(codex_argv(*CODEX_WS, "-c", "model_auto_compact_token_limit=60000"))
            + f"  < {STEP6_LINES // STEP6_CHUNK} sed chunk reads of big80.txt ({STEP6_LINES} x 100 bytes)"]
    if n == 7:
        return [home] + [f"{name}: " + shlex.join(codex_argv(*CODEX_WS, *extra)) + f"  < {step7_prompt()!r}"
                         for name, extra in step7_variants(ctx)]
    if n == 8:
        envs = " ".join(f"{k}={shlex.quote(v)}" for k, v in ENV_VARS.items())
        return [home, f"process env (inherited GIT_*/GH_* dropped): {envs}"] + [
            f"{name}{' (real home)' if real else ''}: " + shlex.join(codex_argv(*CODEX_WS, *extra))
            + f"  < {step8_prompt()!r}" for name, extra, real in step8_variants(ctx)]
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
        return (p.stdout or p.stderr).strip().splitlines()[-1]
    except Exception as e:  # noqa: BLE001 - recorded, not fatal
        return f"unavailable ({e})"


def write_private(path: Path, data: bytes) -> None:
    """Atomically replace path with data, keeping its mode (0600 if new); staging file beside it."""
    try:
        mode = path.stat().st_mode & 0o777
    except OSError:
        mode = 0o600
    staged = path.with_name(f".{path.name}.lhprobe4-{os.getpid()}")
    fd = os.open(staged, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode)
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.replace(staged, path)
    finally:
        if staged.exists():
            staged.unlink()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def iso_time(value) -> datetime | None:
    try:
        t = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except ValueError:
        return None
    return t if t.tzinfo else t.replace(tzinfo=timezone.utc)


def auth_fields_ok(data) -> bool:
    if not isinstance(data, dict):
        return False
    tokens = data.get("tokens")
    has_tokens = isinstance(tokens, dict) and bool(tokens.get("access_token")) \
        and bool(tokens.get("refresh_token"))
    return has_tokens or bool(data.get("OPENAI_API_KEY"))


def guarded_sync(temp_file: Path, real: Path) -> str:
    """Copy a regular-file temp auth.json to the real one only when it is safe. Never logs content."""
    try:
        raw = temp_file.read_bytes()
        new = json.loads(raw)
    except (OSError, ValueError) as e:
        return f"skipped: temp file unreadable or not JSON ({type(e).__name__})"
    if not auth_fields_ok(new):
        return "skipped: temp file lacks token or API-key fields"
    target = real.resolve() if real.is_symlink() else real
    try:
        before = target.read_bytes()
    except OSError as e:
        return f"skipped: real auth.json unreadable ({type(e).__name__})"
    if before == raw:
        return "nothing to sync: identical to the real file"
    try:
        cur = json.loads(before)
    except ValueError:
        cur = {}
    cur_t = iso_time(cur.get("last_refresh")) if isinstance(cur, dict) else None
    new_t = iso_time(new.get("last_refresh")) if new.get("last_refresh") is not None else None
    if new.get("last_refresh") is not None and new_t is None:
        return "skipped: temp last_refresh unparsable"
    if cur_t and not new_t:
        return "skipped: real file has last_refresh, temp file has none"
    if cur_t and new_t and not new_t > cur_t:
        return "skipped: temp last_refresh not newer than the real file's"
    if sha256(target.read_bytes()) != sha256(before):     # re-hash right before the replace
        return "skipped: real auth.json changed during the check"
    write_private(target, raw)
    return ("synced to the real auth.json" +
            (" (written through its symlink to the target)" if target != real else ""))


def relink(link: Path, real: Path) -> None:
    staged = link.with_name(f".auth.json.relink-{os.getpid()}")
    if staged.is_symlink() or staged.exists():
        staged.unlink()
    os.symlink(real, staged)
    os.replace(staged, link)     # also removes any regular-file credential copy


def check_auth(ctx: Ctx, label: str) -> None:
    """After a Codex run: is the temp home's auth.json still a link to the real one?"""
    if not (ctx.codex_home and temp_home(ctx)):
        return
    link, real = ctx.codex_home / "auth.json", REAL_CODEX_HOME / "auth.json"
    try:
        if link.is_symlink():
            ctx.auth_log.append(f"{label}: link kept")
            return
        if link.exists():
            outcome = f"link replaced by a regular file; {guarded_sync(link, real)}"
        else:
            outcome = "link missing"
        relink(link, real)
        ctx.auth_log.append(f"{label}: {outcome}; link restored")
    except Exception as e:  # noqa: BLE001 - recorded, never content
        ctx.auth_log.append(f"{label}: auth check failed ({type(e).__name__})")


def remove_staging(ctx: Ctx) -> None:
    real = REAL_CODEX_HOME / "auth.json"
    dirs = {real.parent}
    try:
        dirs.add(real.resolve().parent)
    except OSError:
        pass
    for d in dirs:
        for f in d.glob(".auth.json.lhprobe4-*"):
            try:
                f.unlink()
                ctx.notes.append(f"removed leftover staging file {f.name}")
            except OSError as e:
                ctx.notes.append(f"could not remove staging file {f.name} ({type(e).__name__})")


def cleanup(ctx: Ctx) -> None:
    """Runs with SIGINT/SIGTERM/SIGHUP ignored so a second Ctrl-C cannot skip removing the temp home."""
    caught = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    old = {s: signal.signal(s, signal.SIG_IGN) for s in caught}
    try:
        try:
            for pgid, start in list(ctx.pgids):
                now = proc_start(pgid) if start else None
                if start and now == start:
                    kill_group(pgid)
                    ctx.notes.append(f"killed group {pgid} (leader start time matched)")
                else:
                    ctx.notes.append(f"group {pgid} not signalled: "
                                     + ("start time unknown (ps failed)" if not start
                                        else "leader gone or pid reused"))
            try:
                leftovers = [r for r in ps_rows() if any(n in r["command"] for n in ctx.needles)]
            except Exception:  # noqa: BLE001
                leftovers = []
            for r in leftovers:
                ctx.notes.append(f"killed leftover: {r['pid']} {r['command'][:120]}")
                try:
                    os.kill(r["pid"], signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
        finally:
            try:
                check_auth(ctx, "cleanup")
            finally:
                if ctx.codex_home:
                    shutil.rmtree(ctx.codex_home, ignore_errors=True)
                    ctx.notes.append("removed the temporary CODEX_HOME" +
                                     (" (still present!)" if ctx.codex_home.exists() else ""))
                remove_staging(ctx)
                if ctx.tmp:
                    shutil.rmtree(ctx.tmp, ignore_errors=True)
        if ctx.raw.exists() and (ctx.notes or ctx.auth_log):
            with open(ctx.raw, "a") as f:
                if ctx.auth_log:
                    f.write("\n## Codex auth link\n\n" + "".join(f"- {n}\n" for n in ctx.auth_log))
                if ctx.notes:
                    f.write("\n## Cleanup\n\n" + "".join(f"- {n}\n" for n in ctx.notes))
        for n in ctx.notes:
            print(f"cleanup: {n}")
    finally:
        for sig, h in old.items():
            signal.signal(sig, h)


def exit_on_signal(signum, _frame):
    """First SIGINT/SIGTERM/SIGHUP ends the run; any further one is ignored, so it cannot
    interrupt the cleanup that removes the temp CODEX_HOME."""
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, signal.SIG_IGN)
    if signum == signal.SIGINT:
        raise KeyboardInterrupt
    raise SystemExit(128 + signum)


def parse_steps(text: str | None) -> list[int]:
    if not text:
        return sorted(STEPS)
    steps = sorted({int(s) for s in text.split(",") if s.strip()})
    bad = [s for s in steps if s not in STEPS]
    if bad:
        raise ValueError(f"unknown step(s): {bad}; valid: {sorted(STEPS)}")
    return steps


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Task 4 runtime probe: limit enforcement and worker environment. "
                    "Run in an ordinary terminal, not inside Claude Code. Takes about 30 minutes.")
    ap.add_argument("--out", required=True, help="directory for task4-raw.md and task4-artifacts/")
    ap.add_argument("--only", help="comma-separated step numbers to run (1-8)")
    ap.add_argument("--dry-run", action="store_true", help="print each step's commands; run nothing")
    args = ap.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        steps = parse_steps(args.only)
    except ValueError as e:
        ap.error(str(e))
    ctx = Ctx(Path(args.out).expanduser().resolve(), args.dry_run)

    if args.dry_run:
        for n in steps:
            print(f"Step {n}: {STEPS[n]}")
            for c in dry_commands(ctx, n):
                print(f"  {c}")
        return 0

    if os.environ.get("CLAUDECODE"):
        print("probe_limits.py: refusing to run inside a Claude Code session (CLAUDECODE is set); "
              "run it from an ordinary terminal.", file=sys.stderr)
        return 2

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        if signal.getsignal(sig) is not signal.SIG_IGN:
            signal.signal(sig, exit_on_signal)
    ctx.art.mkdir(parents=True, exist_ok=True)
    ctx.raw.write_text(
        f"# Task 4 raw probe results: limit enforcement and worker environment\n\n"
        f"- date: {datetime.now(timezone.utc).isoformat(timespec='seconds')}\n"
        f"- claude: {version(['claude', '--version'])}\n"
        f"- codex: {version(['codex', '--version'])}\n"
        f"- python: {sys.version.split()[0]}\n"
        f"- platform: {sys.platform}\n"
        f"- plugin_dir: {PLUGIN_DIR}\n"
        f"- dispatcher: {DISPATCH_AGENT}; workers: lhprobe:turns-worker (maxTurns 2), "
        f"lhprobe:worker (haiku low), lhprobe:compact-worker (autoCompactWindow 100000)\n"
        f"- codex pin: {shlex.join(CODEX_PIN)}\n"
        f"- codex home: temp CODEX_HOME whose auth.json is a symlink to the operator's, plus a "
        f"minimal config.toml (network_access = true), when {REAL_CODEX_HOME / 'auth.json'} exists "
        f"({'yes' if (REAL_CODEX_HOME / 'auth.json').exists() else 'no: real home fallback'}); "
        f"the operator's config.toml is never edited\n"
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
