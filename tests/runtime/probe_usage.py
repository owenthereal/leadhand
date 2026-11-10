#!/usr/bin/env python3
"""Runtime probe for Task 5: interrupted-usage recovery.

Runs every step of Task 5 against the real `claude`, `codex` and `tmux` CLIs and
writes the observed facts to DIR/task5-raw.md, with full artifacts under
DIR/task5-artifacts/. Run it in an ordinary terminal, not inside Claude Code:

    python3 tests/runtime/probe_usage.py --out DIR [--only 1,4] [--dry-run]

Standard library only. Every step runs independently; failures are recorded,
not raised.

- Step 1 runs `codex exec --json` (gpt-6-luna, low effort) in Codex's default
  home ($CODEX_HOME if set, else ~/.codex), kills its process group with
  SIGKILL as soon as the first command completes, reads the thread's rollout
  file under sessions/YYYY/MM/DD/, resumes the thread once and reads it again.
- Steps 2 and 3 drive an interactive `claude --agent lhprobe:super-usage` in
  tmux (haiku, low effort), each in its own fixture repo. Step 2 stops one
  background `lhprobe:usage-worker` with TaskStop and resumes another, finished
  one with SendMessage. Step 3 has the supervisor echo CLAUDE_CODE_SESSION_ID.
- Step 4 writes sanitized fixture candidates from the artifacts that steps 1
  and 2 listed in task5-artifacts/fixture-sources.json (so `--only 4` reuses an
  earlier run in the same DIR).

Records are classified by probe-controlled text (unique tokens in commands),
never by the usage fields being probed. Expect about 10 minutes of wall time.
The probe never writes under the Codex home.
"""

import argparse
import json
import os
import queue
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
PLUGIN_DIR = HERE / "probe-plugin"
CLAUDE_PROJECTS = Path.home() / ".claude" / "projects"
CODEX_HOME = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex")
CODEX_SESSIONS = CODEX_HOME / "sessions"

SUPER_AGENT = "lhprobe:super-usage"
WORKER_AGENT = "lhprobe:usage-worker"
CODEX_MODEL = "gpt-6-luna"
CODEX_PIN = ["-m", CODEX_MODEL, "-c", 'model_reasoning_effort="low"']
CODEX_JSON = ["--json", "--skip-git-repo-check"]
CODEX_WS = ["-s", "workspace-write"]

READY_TIMEOUT = 90        # 60 s for first-run dialogs + startup
DISPATCH_TIMEOUT = 150    # message sent -> worker's command running
TURN_TIMEOUT = 150        # message sent -> supervisor's reply
CODEX_TIMEOUT = 240       # step 1: waiting for the first command
CODEX_RESUME_TIMEOUT = 180
CMD_TIMEOUT = 30          # every short subprocess call
ROLLOUT_SETTLE = 3        # seconds after Codex exits before reading its rollout

STEPS = {
    1: "Codex",
    2: "Claude",
    3: "Supervisor and proxy",
    4: "Save sanitized fixture candidates",
}

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

MENU_MAX_MOVES = 6
MENU_HIGHLIGHT = re.compile(r"^\s*❯\s*(.*)$", re.M)
MENU_WANTED = re.compile(r"\b(Yes|Proceed|Continue)\b")
MENU_FORBIDDEN = re.compile(r"\b(No|exit)\b", re.I)


def select_menu_option(capture, send_key):
    """Highlight the wanted option of a menu dialog before Enter is sent.

    Menu dialogs mark the highlighted option with a leading "❯" (the trust
    dialog defaults to "No, exit"). Returns (ok, highlighted_line, keys_sent);
    Enter is sent only when the highlighted line is a wanted option and not a
    "No"/"exit" one. Non-menu dialogs (no "❯" line, or no "Yes"-type option)
    just get Enter, as before. ok=False means the option could not be highlighted
    and no Enter was sent.
    """
    pane = capture()
    keys: list[str] = []
    for move in range(MENU_MAX_MOVES + 1):
        m = MENU_HIGHLIGHT.search(pane)
        has_yes = any(MENU_WANTED.search(line) for line in pane.splitlines())
        if not m or not has_yes:
            if m and MENU_FORBIDDEN.search(m.group(1)):
                return False, m.group(1).strip(), keys
            send_key("Enter")
            keys.append("Enter")
            return True, m.group(1).strip() if m else None, keys
        line = m.group(1).strip()
        if MENU_WANTED.search(line) and not MENU_FORBIDDEN.search(line):
            send_key("Enter")
            keys.append("Enter")
            return True, line, keys
        if move == MENU_MAX_MOVES:
            return False, line, keys
        send_key("Down")
        keys.append("Down")
        time.sleep(0.5)
        pane = capture()
    return False, None, keys
GIT_SAFE = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
            "-c", "user.name=probe", "-c", "user.email=probe@example.invalid"]
AGENT_ID_RE = re.compile(r"agent[_ ]?id[\"']?\s*[:=]\s*[\"']?([A-Za-z0-9_-]{6,})", re.I)

T0 = time.monotonic()


def progress(msg: str) -> None:
    print(f"[{time.monotonic() - T0:7.1f}s] {msg}", flush=True)


class StepFail(Exception):
    def __init__(self, reason: str, lines: list[str] | None = None):
        super().__init__(reason)
        self.reason = reason
        self.lines = lines or []


# ---------------------------------------------------------------- sanitizer

# String values under these keys are kept (types, ids, model names, roles, stop
# reasons, timestamps); every other string becomes PLACEHOLDER.
SANITIZE_KEEP = frozenset({
    "type", "subtype",
    "id", "uuid", "parentUuid", "logicalParentUuid", "sessionId", "session_id", "agentId",
    "thread_id", "turn_id", "call_id", "tool_use_id", "requestId",
    "model", "role", "stop_reason", "timestamp",
})
PLACEHOLDER = "<placeholder>"


def sanitize_value(value, key: str | None = None, keep=SANITIZE_KEEP, home: str | None = None):
    """Keep structure, numbers, booleans and null; keep strings only under `keep`
    keys (home directory replaced by `~`); replace every other string. Dict keys
    that look like paths are replaced too."""
    home = str(Path.home()) if home is None else home
    if isinstance(value, dict):
        out = {}
        for i, (k, v) in enumerate(value.items()):
            safe = k if ("/" not in k and not k.startswith("~")) else f"<placeholder-key-{i}>"
            out[safe] = sanitize_value(v, k, keep, home)
        return out
    if isinstance(value, list):
        return [sanitize_value(v, key, keep, home) for v in value]
    if isinstance(value, str):
        if key in keep:
            return value.replace(home, "~") if home else value
        return PLACEHOLDER
    return value


def sanitize_lines(lines, keep=SANITIZE_KEEP, home: str | None = None) -> list[str]:
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
        self.art = out / "task5-artifacts"
        self.raw = out / "task5-raw.md"   # main() picks a fresh name if this exists
        self.manifest = self.art / "fixture-sources.json"
        self.dry = dry
        self.uniq = f"{os.getpid() % 10000:04d}"
        self.sock = f"lhprobe5-{os.getpid()}"
        self.tmp: Path | None = None
        self.fixtures: dict[str, Path] = {}
        self.cmds: list[tuple[list[str], object]] = []
        self.facts: dict = {}             # recorded for the current step even when it fails
        self.groups: list[dict] = []      # {"role", "pgid", "leader", "lstart"}
        self.sessions: list = []          # interactive sessions of the current step
        self.needles: list[str] = []      # any process whose command holds one is ours
        self.tmux_started = False

    def token(self, prefix: str) -> str:
        """A unique sleep duration such as 300.2<nnnn>."""
        return f"{prefix}{self.uniq}"

    def fixture(self, label: str) -> Path:
        if self.dry:
            return Path(f"<tmp>/lhprobe5 XXXX/{label} repo")
        if label not in self.fixtures:
            if self.tmp is None:
                self.tmp = Path(tempfile.mkdtemp(prefix="lhprobe5 ")).resolve()
                self.needles.append(str(self.tmp))
            self.fixtures[label] = make_fixture(self.tmp / f"{label} repo")
        return self.fixtures[label]

    def add_source(self, name: str, path: Path, note: str) -> None:
        """Record which artifact Step 4 sanitizes into `name`."""
        data = read_json(self.manifest) or {}
        data[name] = {"artifact": path.name, "note": note}
        atomic_write_json(self.manifest, data)


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
    env.pop("CLAUDE_CODE_SESSION_ID", None)   # Step 3 must see only what Claude sets
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


def read_jsonl(path: Path) -> list[dict]:
    try:
        return parse_lines(path.read_text(errors="replace"))
    except OSError:
        return []


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


def type_counts(values) -> dict:
    out: dict[str, int] = {}
    for v in values:
        out[str(v)] = out.get(str(v), 0) + 1
    return out


def find_key(obj, key: str, path: str = "") -> list[tuple[str, object]]:
    """Every (json path, value) where a dict holds `key`."""
    found = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            p = f"{path}.{k}" if path else k
            if k == key:
                found.append((p, v))
            found += find_key(v, key, p)
    elif isinstance(obj, list):
        for v in obj:
            found += find_key(v, key, path + "[]")
    return found


# ---------------------------------------------------------------- processes


def ps_rows() -> list[dict]:
    p = subprocess.run(["ps", "-axo", "pid=,pgid=,ppid=,stat=,command="],
                       capture_output=True, text=True, timeout=CMD_TIMEOUT,
                       stdin=subprocess.DEVNULL)
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


def py_sleep_cmd(token: str) -> str:
    """Long worker command that Claude Code's Bash tool does not block; token stays in argv."""
    return f"python3 -c 'import time; time.sleep({token})'"


def py_sleep_rows(token: str, rows: list[dict] | None = None) -> list[dict]:
    """Processes whose program is python and whose arguments hold the token (not the shell
    wrapper, whose program is a shell)."""
    return [r for r in (ps_rows() if rows is None else rows)
            if token in r["command"].split("time.sleep(")[-1]
            and os.path.basename(r["command"].split()[0]).lower().startswith("python")]


def brief(rows: list[dict]) -> list[str]:
    return [f"pid={r['pid']} pgid={r['pgid']} ppid={r['ppid']} cmd={r['command'][:120]}"
            for r in rows]


def proc_start(pid: int) -> str | None:
    """The process's start time (`ps -o lstart=`), or None if it is gone or ps fails."""
    try:
        p = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True,
                           text=True, timeout=CMD_TIMEOUT, stdin=subprocess.DEVNULL)
    except Exception:  # noqa: BLE001 - no start time means no SIGKILL at cleanup
        return None
    return p.stdout.strip() or None


def group_evidence(g: dict, rows: list[dict], needles: list[str]) -> str | None:
    """Why the recorded group still belongs to this probe, or None if it can't be shown:
    its recorded leader is still in it with the same start time, or a member's command
    holds a probe needle. A recycled pgid fails both."""
    members = [r for r in rows if r["pgid"] == g["pgid"]]
    if not members:
        return None
    if (g.get("lstart") and any(r["pid"] == g["leader"] for r in members)
            and proc_start(g["leader"]) == g["lstart"]):
        return "recorded leader pid + start time"
    if any(n in r["command"] for r in members for n in needles):
        return "member command holds a probe needle"
    return None


def kill_owned_groups(groups: list[dict], needles: list[str]) -> list[str]:
    """SIGTERM every provably owned group (evidence from one snapshot), then SIGKILL
    those still provably owned."""
    rows = ps_rows()
    owned, log = [], []
    for g in groups:
        ev = group_evidence(g, rows, needles)
        log.append(f"pgid {g['pgid']} ({g.get('role', '?')}): "
                   f"{'owned: ' + ev if ev else 'not signalled: gone or no ownership evidence'}")
        if ev:
            owned.append(g)
    for g in owned:
        try:
            os.killpg(g["pgid"], signal.SIGTERM)
            log.append(f"pgid {g['pgid']}: SIGTERM sent")
        except (ProcessLookupError, PermissionError) as e:
            log.append(f"pgid {g['pgid']}: SIGTERM failed ({type(e).__name__})")
    if owned:
        time.sleep(1)
    rows = ps_rows()
    for g in owned:
        ev = group_evidence(g, rows, needles)
        if not ev:
            continue
        try:
            os.killpg(g["pgid"], signal.SIGKILL)
            log.append(f"pgid {g['pgid']}: SIGKILL sent ({ev})")
        except (ProcessLookupError, PermissionError) as e:
            log.append(f"pgid {g['pgid']}: SIGKILL failed ({type(e).__name__})")
    return log


def kill_pid(pid: int, sig=signal.SIGKILL) -> None:
    try:
        os.kill(pid, sig)
    except (ProcessLookupError, PermissionError):
        pass


def own_group(ctx: Ctx, role: str, pid: int) -> None:
    """Record a process group this probe started (leader = pid, via process_group=0)."""
    ctx.groups.insert(0, {"role": role, "pgid": pid, "leader": pid, "lstart": proc_start(pid)})


# ---------------------------------------------------------------- codex (step 1)


def codex_first_argv() -> list[str]:
    return ["codex", "exec", *CODEX_PIN, *CODEX_JSON, *CODEX_WS, "-"]


def codex_resume_variants(tid: str) -> list[tuple[str, list[str]]]:
    return [
        ("flags-before-resume", ["codex", "exec", *CODEX_PIN, *CODEX_JSON, "resume", tid, "-"]),
        ("flags-after-resume", ["codex", "exec", "resume", *CODEX_PIN, *CODEX_JSON, tid, "-"]),
    ]


def codex_prompt(first: str, sleep_tok: str) -> str:
    return (f"Run these three shell commands one at a time, each as its own separate command, in "
            f"this order: `echo {first}`, `sleep {sleep_tok}`, `echo done`. Then reply with one "
            f"line: done")


RESUME_PROMPT = "Do not run any commands. Reply with one line: resumed"


def thread_id(events: list[dict]) -> str | None:
    for e in events:
        if e.get("thread_id"):
            return str(e["thread_id"])
    return None


def event_kind(e: dict) -> str:
    item = e.get("item") if isinstance(e.get("item"), dict) else None
    t = e.get("type", "unparsed" if "unparsed" in e else "?")
    return f"{t}/{item.get('type')}" if item else str(t)


PROSE_ITEM_TYPES = ("agent_message", "reasoning")
COMMAND_FIELDS = ("command", "aggregated_output")


def is_first_command(e: dict, first: str) -> bool:
    """The kill trigger: an item.completed whose item is not prose and holds the probe
    text in its own command or output field (a preamble quoting the text doesn't count)."""
    item = e.get("item") if isinstance(e.get("item"), dict) else None
    if e.get("type") != "item.completed" or item is None:
        return False
    if item.get("type") in PROSE_ITEM_TYPES:
        return False
    return any(first in json.dumps(item.get(f)) for f in COMMAND_FIELDS if f in item)


def run_codex(ctx: Ctx, label: str, argv: list[str], prompt: str, cwd: Path) -> dict:
    """Run one codex exec to completion in its own process group."""
    t0 = time.monotonic()
    p = subprocess.Popen(argv, cwd=cwd, env=child_env(), stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                         process_group=0)
    own_group(ctx, label, p.pid)
    try:
        out, err = p.communicate(prompt, timeout=CODEX_RESUME_TIMEOUT)
        rc = p.returncode
    except subprocess.TimeoutExpired:
        try:
            os.killpg(p.pid, signal.SIGKILL)   # not yet reaped: the pgid is still ours
        except (ProcessLookupError, PermissionError):
            pass
        out, err = p.communicate(timeout=CMD_TIMEOUT)
        rc = "timeout"
    (ctx.art / f"{label}-stream.jsonl").write_text(out or "")
    (ctx.art / f"{label}-stderr.txt").write_text(err or "")
    ctx.cmds.append((argv, rc))
    return {"exit": rc, "stdout": out or "", "stderr": err or "",
            "events": parse_lines(out or ""), "elapsed_s": round(time.monotonic() - t0, 1)}


def rollout_files(tid: str) -> list[Path]:
    """Rollout files under sessions/YYYY/MM/DD/ whose name ends with the thread ID."""
    if not CODEX_SESSIONS.exists():
        return []
    return sorted(CODEX_SESSIONS.glob(f"*/*/*/*{tid}.jsonl"))


def rollout_files_loose(tid: str) -> list[str]:
    if not CODEX_SESSIONS.exists():
        return []
    return [str(p) for p in sorted(CODEX_SESSIONS.rglob(f"*{tid}*"))]


def total_of(usage) -> int | None:
    if not isinstance(usage, dict):
        return None
    if isinstance(usage.get("total_tokens"), int):
        return usage["total_tokens"]
    nums = [v for v in usage.values() if isinstance(v, (int, float)) and not isinstance(v, bool)]
    return int(sum(nums)) if nums else None


def payload_type(r: dict) -> str:
    payload = r.get("payload")
    return str(payload.get("type")) if isinstance(payload, dict) else "-"


def rollout_usage(recs: list[dict]) -> dict:
    """Cumulative token records in a rollout: JSON path and values of the last one."""
    tok = []
    null_info = 0
    for i, r in enumerate(recs):
        payload = r.get("payload") if isinstance(r.get("payload"), dict) else {}
        if payload.get("type") == "token_count" and payload.get("info") is None:
            null_info += 1
        for path, v in find_key(r, "total_token_usage"):
            if isinstance(v, dict):
                tok.append({"line": i, "path": path, "value": v, "record": r})
    out = {
        "records": len(recs),
        "record_types (type/payload.type)": type_counts(
            f"{r.get('type')}/{payload_type(r)}" for r in recs),
        "token_records (holding total_token_usage)": len(tok),
        "token_count_records_with_null_info": null_info,
        "total_tokens_sequence": [total_of(t["value"]) for t in tok],
    }
    if tok:
        last = tok[-1]
        r = last["record"]
        payload = r.get("payload") if isinstance(r.get("payload"), dict) else {}
        info = payload.get("info") if isinstance(payload.get("info"), dict) else {}
        out.update({
            "last_token_record_line": last["line"],
            "last_token_record_type": r.get("type"),
            "last_token_record_payload.type": payload.get("type"),
            "last_cumulative_path": last["path"],
            "last_cumulative_value": last["value"],
            "last_cumulative_total": total_of(last["value"]),
            "last_cumulative_nonzero": any(isinstance(v, (int, float)) and v > 0
                                           for v in last["value"].values()),
            "last_per_turn (last_token_usage) paths": [p for p, _ in find_key(r, "last_token_usage")],
            "last_per_turn value": next((v for _, v in find_key(r, "last_token_usage")), None),
            "token_record_keys": sorted(r),
            "token_record_payload_keys": sorted(payload),
            "token_record_payload.info_keys": sorted(info),
            "token_record_usage_keys": sorted(last["value"]),
        })
    out["turn_context models/efforts"] = [
        {"line": i, **{p: v for p, v in find_key(r, "model") + find_key(r, "effort")
                       if not isinstance(v, (dict, list))}}
        for i, r in enumerate(recs) if r.get("type") == "turn_context"]
    return out


def step1(ctx: Ctx):
    first = f"lhprobe-first-{ctx.uniq}"
    sleep_tok = ctx.token("120.5")
    ctx.needles.append(sleep_tok)
    cwd = ctx.fixture("codex")
    argv = codex_first_argv()
    prompt = codex_prompt(first, sleep_tok)
    obs = {"codex_home": str(CODEX_HOME),
           "codex_home_from": "CODEX_HOME" if os.environ.get("CODEX_HOME") else "default ~/.codex",
           "prompt": prompt,
           "kill_trigger": (f"first item.completed whose item.type is not in {list(PROSE_ITEM_TYPES)} "
                            f"and whose own {list(COMMAND_FIELDS)} field holds `{first}`"),
           "signal": "SIGKILL to codex's process group (worst case: no chance to flush)"}
    ctx.facts = obs   # kept in the raw file even if the step fails part-way
    state: dict = {"tid": None, "copies": []}
    try:
        return step1_body(ctx, obs, state, argv, prompt, cwd, first)
    finally:
        ctx.facts["thread_id"] = state["tid"] or "not seen"
        try:
            if state["tid"]:
                files = rollout_files(state["tid"])
                ctx.facts["rollout_files_at_end"] = [str(f) for f in files] or "none"
                for f in files:
                    dest = ctx.art / f"step1-rollout-final-{f.name}"
                    shutil.copy2(f, dest)
                    state["copies"].append(dest.name)
                if files:
                    ctx.add_source("codex-rollout-interrupted", ctx.art / f"step1-rollout-final-{files[-1].name}",
                                   "rollout after the kill and the resume (if it ran)")
            ctx.facts["rollout_copies"] = state["copies"] or "none"
        except Exception as e:  # noqa: BLE001 - recorded, not fatal
            ctx.facts["rollout_copy_error"] = f"{type(e).__name__}: {e}"


def step1_body(ctx: Ctx, obs: dict, state: dict, argv, prompt, cwd, first):
    stream_path = ctx.art / "step1-stream.jsonl"
    t0 = time.monotonic()
    err_f = open(ctx.art / "step1-stderr.txt", "w")
    p = subprocess.Popen(argv, cwd=cwd, env=child_env(), stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=err_f, text=True, process_group=0)
    own_group(ctx, "codex step1", p.pid)
    p.stdin.write(prompt)
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
    events: list[dict] = []
    trigger = None
    eof = False
    while time.monotonic() - t0 < CODEX_TIMEOUT and not eof and trigger is None:
        try:
            got = q.get(timeout=1)
        except queue.Empty:
            continue
        if got is None:
            eof = True
            break
        t, line = got
        e = (parse_lines(line) or [{"unparsed": line}])[0]
        events.append(e)
        if not state["tid"] and e.get("thread_id"):
            state["tid"] = str(e["thread_id"])
        if first in line and not is_first_command(e, first) and len(ctx.facts.setdefault(
                "lines_with_probe_text_not_triggering", [])) < 5:
            ctx.facts["lines_with_probe_text_not_triggering"].append(f"{t}s {event_kind(e)}")
        if is_first_command(e, first):
            trigger = {"t": t, "event": event_kind(e),
                       "item_keys": sorted(e["item"]) if isinstance(e.get("item"), dict) else None}
            try:
                os.killpg(p.pid, signal.SIGKILL)   # not yet reaped: the pgid is still ours
                sent = "sent"
            except (ProcessLookupError, PermissionError) as ex:
                sent = f"failed ({type(ex).__name__})"
            ctx.cmds.append((["kill", "-KILL", f"-{p.pid}"], sent))
            progress(f"  step1: first command completed at {t}s; SIGKILL to codex group {p.pid}")
    if trigger is None and p.poll() is None:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        ctx.cmds.append((["kill", "-KILL", f"-{p.pid}"], "timeout/no trigger"))
    try:
        rc = p.wait(timeout=CMD_TIMEOUT)
    except subprocess.TimeoutExpired:
        rc = "did not exit"
    th.join(timeout=10)
    err_f.close()
    ctx.cmds.insert(0, (argv, rc))
    # Drain lines that arrived after the trigger.
    while True:
        try:
            got = q.get_nowait()
        except queue.Empty:
            break
        if got:
            events.append((parse_lines(got[1]) or [{"unparsed": got[1]}])[0])
    stderr = (ctx.art / "step1-stderr.txt").read_text(errors="replace")
    obs.update({
        "first_exec_exit": rc,
        "exit_signal": signal.Signals(-rc).name if isinstance(rc, int) and rc < 0 else "none",
        "trigger": trigger or "never seen",
        "item_types_seen (event/item.type, in order)": list(dict.fromkeys(event_kind(e) for e in events)),
        "event_kind_counts": type_counts(event_kind(e) for e in events),
        "commands_seen (probe text)": [str(e["item"].get("command", ""))[:120] for e in events
                                       if isinstance(e.get("item"), dict) and e["item"].get("command")
                                       and e.get("type") == "item.completed"],
        "thread_id": state["tid"],
        "stderr_last_lines": stderr.strip().splitlines()[-5:] or "none",
    })
    excerpt = [f"first exec exit={rc} trigger={trigger and trigger['t']}s tid={state['tid']}"]
    excerpt += [f"[stream] {json.dumps(e)[:150]}" for e in events[-3:]]
    if trigger is None:
        raise StepFail("first command never completed before the timeout", excerpt)
    if not state["tid"]:
        raise StepFail("no thread_id in the --json stream", excerpt)
    tid = state["tid"]
    idle(ROLLOUT_SETTLE)
    files = rollout_files(tid)
    obs["rollout_files_matching (sessions/*/*/*/*<tid>.jsonl)"] = [str(f) for f in files] or "none"
    if not files:
        obs["rollout_files_loose (any name holding tid)"] = rollout_files_loose(tid) or "none"
        raise StepFail("no rollout file ending with the thread ID", excerpt)
    rf = files[-1]
    rel = rf.relative_to(CODEX_SESSIONS)
    obs.update({
        "rollout_file": str(rf),
        "rollout_dir_is_YYYY/MM/DD": bool(re.fullmatch(r"\d{4}/\d{2}/\d{2}", str(rel.parent))),
        "rollout_name_ends_with_thread_id": rf.stem.endswith(tid),
    })
    before_copy = ctx.art / f"step1-rollout-before-resume-{rf.name}"
    shutil.copy2(rf, before_copy)
    before_text = before_copy.read_text(errors="replace")
    before = rollout_usage(parse_lines(before_text))
    obs.update({f"before_resume {k}": v for k, v in before.items()})
    excerpt.append(f"rollout before resume: records={before['records']} "
                   f"last_total={before.get('last_cumulative_total')} path={before.get('last_cumulative_path')}")

    # Resume once. The alternative order runs only if the first exits non-zero AND
    # shows no sign of having resumed (no thread/turn events), so the thread gets at
    # most one resumed turn.
    used = None
    for n_try, (name, rargv) in enumerate(codex_resume_variants(tid)):
        progress(f"  step1: resume ({name})")
        rr = run_codex(ctx, f"step1-resume-{name}", rargv, RESUME_PROMPT, cwd)
        rev = rr["events"]
        tc = [e for e in rev if e.get("type") == "turn.completed"]
        obs[f"resume {name}"] = {
            "argv": shlex.join(rargv), "exit": rr["exit"], "elapsed_s": rr["elapsed_s"],
            "thread_id": thread_id(rev), "same_thread": thread_id(rev) == tid,
            "event_kinds": list(dict.fromkeys(event_kind(e) for e in rev)),
            "turn_completed_usage": tc[-1].get("usage") if tc else "no turn.completed",
            "stderr_first_lines": rr["stderr"].strip().splitlines()[:5] or "none",
        }
        excerpt.append(f"resume {name}: exit={rr['exit']}")
        started = [event_kind(e) for e in rev
                   if e.get("thread_id") or str(e.get("type", "")).startswith("turn.")]
        obs[f"resume {name}"]["thread_or_turn_events"] = started or "none"
        if rr["exit"] == 0:
            used = name
            break
        if started:
            obs["resume_retry"] = (f"not run: {name} exited {rr['exit']} but emitted {started}, so "
                                   "it may have resumed; a second order would add a second turn")
            used = f"{name} (non-zero exit)"
            break
        if n_try == 0:
            obs["resume_retry"] = (f"{name} exited {rr['exit']} with no thread/turn events; "
                                   "trying the alternative order")
    obs.setdefault("resume_retry", "not needed")
    obs["resume_argv_used"] = (shlex.join(dict(codex_resume_variants(tid))[used.split(" (")[0]])
                               + ("" if used in dict(codex_resume_variants(tid)) else
                                  "  (exited non-zero after starting)")
                               if used else "none resumed")
    idle(ROLLOUT_SETTLE)
    files_after = rollout_files(tid)
    obs["rollout_files_after_resume"] = [str(f) for f in files_after] or "none"
    after_text = rf.read_text(errors="replace") if rf.exists() else ""
    after = rollout_usage(parse_lines(after_text))
    obs.update({f"after_resume {k}": v for k, v in after.items()})
    b_total, a_total = before.get("last_cumulative_total"), after.get("last_cumulative_total")
    obs.update({
        "same_file_after_resume": files_after == files,
        "records before -> after": f"{before['records']} -> {after['records']}",
        "token_records before -> after": (f"{before['token_records (holding total_token_usage)']} -> "
                                          f"{after['token_records (holding total_token_usage)']}"),
        "last cumulative total before -> after": f"{b_total} -> {a_total}",
        "cumulative_total_grew": (isinstance(a_total, int) and isinstance(b_total, int)
                                  and a_total > b_total),
        "before_lines_unchanged_after_resume (append-only)": after_text.startswith(before_text),
    })
    others = [f for f in files_after if f != rf]
    if others:
        obs["other_rollout_files_after_resume (usage)"] = {
            str(f): rollout_usage(read_jsonl(f)) for f in others}
    excerpt.append(f"rollout after resume: records={after['records']} last_total={a_total}")
    summary = (f"path={before.get('last_cumulative_path')} nonzero={before.get('last_cumulative_nonzero')} "
               f"resume={used or 'failed'} total {b_total}->{a_total} same_file={obs['same_file_after_resume']}")
    return "OBSERVED", excerpt[:10], obs, summary


# ---------------------------------------------------------------- tmux + claude


def tmux(ctx: Ctx, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["tmux", "-L", ctx.sock, *args], capture_output=True, text=True,
                          timeout=CMD_TIMEOUT, stdin=subprocess.DEVNULL)


def claude_argv() -> list[str]:
    return ["env", "-u", "CLAUDECODE", "-u", "CLAUDE_CODE_SESSION_ID",
            "claude", "--plugin-dir", str(PLUGIN_DIR), "--agent", SUPER_AGENT,
            "--permission-mode", "auto", "--model", "haiku", "--effort", "low"]


def tmux_new_argv(ctx: Ctx, name: str, cwd: str, hook_log: str) -> list[str]:
    return ["tmux", "-L", ctx.sock, "start-server", ";", "set", "-g", "remain-on-exit", "on", ";",
            "new-session", "-d", "-s", name, "-c", cwd, "-x", "200", "-y", "50",
            "-e", f"PROBE_LOG={hook_log}", "-e", "LEADHAND_RUN_ID=probe5", *claude_argv()]


class Session:
    """One interactive `claude --agent lhprobe:super-usage` in tmux."""

    def __init__(self, ctx: Ctx, label: str):
        self.ctx = ctx
        self.label = label
        self.name = f"lh5-{label}"
        self.cwd = ctx.fixture(label)
        self.hook_log = ctx.art / f"{label}-hooks.jsonl"
        self.answered: list[str] = []
        self.selections: list[dict] = []
        self.pane = ""
        self.started = 0.0
        self.sid: str | None = None
        self.sid_source = "not found"
        ctx.sessions.append(self)

    def start(self) -> None:
        argv = tmux_new_argv(self.ctx, self.name, str(self.cwd), str(self.hook_log))
        self.started = time.time()
        p = subprocess.run(argv, capture_output=True, text=True, timeout=CMD_TIMEOUT,
                           stdin=subprocess.DEVNULL, env=child_env())
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
                ok, hl, keys = select_menu_option(
                    self.capture, lambda k: tmux(self.ctx, "send-keys", "-t", self.name, k))
                self.selections.append({"dialog": pat, "highlighted_at_enter": hl if ok else None,
                                        "last_highlighted": hl, "keys": keys, "ok": ok})
                if not ok:  # could not highlight the wanted option: stop retrying this dialog
                    self.answered.extend([pat] * 4)
                    break
                time.sleep(2)
                self.pane = self.capture()
                break
        return self.pane

    def dead(self) -> bool:
        p = tmux(self.ctx, "display", "-p", "-t", self.name, "#{pane_dead}")
        return p.returncode != 0 or p.stdout.strip() == "1"

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

    def hook_inputs(self) -> list[dict]:
        return [h.get("hook_input") or {} for h in read_jsonl(self.hook_log)]

    def session_id(self) -> str | None:
        if self.sid:
            return self.sid
        for hi in self.hook_inputs():
            if hi.get("session_id"):
                self.sid, self.sid_source = hi["session_id"], "PROBE_LOG hook line"
                return self.sid
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

    def facts(self) -> dict:
        """Recorded for every session, also when its step fails."""
        out = {}
        for key, fn in (("session_id", self.session_id),
                        ("session_id_source", lambda: self.sid_source),
                        ("transcripts_found", lambda: self.copy_transcripts() or "none"),
                        ("dialogs_answered", lambda: self.answered or "none"),
                        ("dialog_selections", lambda: self.selections or "none")):
            try:
                out[key] = fn()
            except Exception as e:  # noqa: BLE001 - recorded, not fatal
                out[key] = f"error: {type(e).__name__}: {e}"
        return out

    def copy_transcripts(self) -> list[str]:
        dest = self.ctx.art / f"{self.label}-transcripts"
        found = []
        main = self.main_path()
        for p in ([main] if main else []) + self.sub_paths():
            dest.mkdir(parents=True, exist_ok=True)
            shutil.copy2(p, dest / p.name)
            found.append(str(p))
        return found


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
    """tool_use_id -> {"text", "structured" (the record's toolUseResult)}."""
    out = {}
    for r in recs:
        if r.get("type") == "user":
            for c in content_items(r):
                if c.get("type") == "tool_result":
                    out[c.get("tool_use_id")] = {"text": text_of(c.get("content", "")),
                                                 "structured": r.get("toolUseResult")}
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


def replied(sess: Session, needle: str, reply: str) -> bool:
    r = sess.main_records()
    i = user_index(r, needle)
    return i is not None and any(reply in t for t in assistant_texts(r, i + 1))


def summarize(r: dict) -> str:
    parts = []
    for c in content_items(r):
        t = c.get("type")
        if t == "text":
            parts.append(f"text:{c.get('text', '')[:120]!r}")
        elif t == "tool_use":
            parts.append(f"tool_use:{c.get('name')} {json.dumps(c.get('input'))[:120]}")
        elif t == "tool_result":
            parts.append(f"tool_result:{text_of(c.get('content', ''))[:120]!r}")
        else:
            parts.append(str(t))
    return f"{r.get('type')}/{r.get('subtype', '')}: " + " | ".join(parts)


def usage_of(r: dict):
    msg = r.get("message") if isinstance(r.get("message"), dict) else {}
    return msg.get("usage") if isinstance(msg.get("usage"), dict) else None


def usage_analysis(recs: list[dict], start: int = 0) -> dict:
    """Assistant usage records from index `start`: counts, repeats per message.id, keys."""
    part = recs[start:]
    asst = [r for r in part if r.get("type") == "assistant"]
    with_usage = [r for r in asst if usage_of(r) is not None]
    by_id: dict[str, list[dict]] = {}
    for r in with_usage:
        by_id.setdefault(str((r.get("message") or {}).get("id")), []).append(r)
    repeats, patterns = {}, {"identical": 0, "grows": 0, "other": 0}
    for mid, rs in by_id.items():
        if len(rs) < 2:
            continue
        us = [usage_of(r) for r in rs]
        same = all(json.dumps(u, sort_keys=True) == json.dumps(us[0], sort_keys=True) for u in us)
        outs = [u.get("output_tokens") for u in us]
        grows = (not same and all(isinstance(o, int) for o in outs)
                 and all(a <= b for a, b in zip(outs, outs[1:])))
        kind = "identical" if same else "grows" if grows else "other"
        patterns[kind] += 1
        if len(repeats) < 6:
            repeats[mid] = {"records": len(rs), "usage": kind,
                            "content_types": [[c.get("type") for c in content_items(r)] for r in rs],
                            "output_tokens": outs,
                            "input_tokens": [u.get("input_tokens") for u in us]}
    last_per_id = [rs[-1] for rs in by_id.values()]
    out_sum = lambda rs: sum(int(usage_of(r).get("output_tokens") or 0) for r in rs)  # noqa: E731
    keys_by_type: dict[str, set] = {}
    for r in part:
        keys_by_type.setdefault(str(r.get("type")), set()).update(r)
    first = with_usage[0] if with_usage else None
    return {
        "records": len(part),
        "record_types": type_counts(r.get("type") for r in part),
        "assistant_records": len(asst),
        "assistant_records_with_message.usage": len(with_usage),
        "distinct_message_ids": len(by_id),
        "message_ids_with_more_than_one_record": sum(1 for rs in by_id.values() if len(rs) > 1),
        "repeat_usage_patterns": patterns,
        "repeat_examples": repeats or "none",
        "output_tokens_sum_all_records": out_sum(with_usage),
        "output_tokens_sum_last_record_per_id": out_sum(last_per_id),
        "models": sorted({str((r.get("message") or {}).get("model")) for r in asst}),
        "record_keys_by_type": {k: sorted(v) for k, v in keys_by_type.items()},
        "assistant_message_keys": sorted(first["message"]) if first else None,
        "assistant_usage_keys": sorted(usage_of(first)) if first else None,
        "first_usage_value": usage_of(first) if first else None,
    }


def find_agent(sess: Session, token: str) -> dict:
    """Locate the worker whose dispatch prompt holds `token`, by three routes."""
    recs = sess.main_records()
    res = tool_results(recs)
    disp = [u for u in tool_uses(recs) if u["name"] in ("Agent", "Task")
            and token in json.dumps(u["input"])]
    out: dict = {"dispatch_found_in_main_transcript": bool(disp)}
    ids: dict[str, object] = {}
    if disp:
        u = disp[-1]
        r = res.get(u["id"]) or {}
        m = AGENT_ID_RE.search(r.get("text", ""))
        if m:
            ids["tool result text"] = m.group(1)
        st = r.get("structured")
        if isinstance(st, dict) and st.get("agentId"):
            ids["transcript toolUseResult.agentId"] = str(st["agentId"])
        out["dispatch_input"] = u["input"]
        out["dispatch_result_text"] = r.get("text", "<no result yet>")[:400]
        out["dispatch_toolUseResult_keys"] = sorted(st) if isinstance(st, dict) else None
    listed = []
    for p in sess.sub_paths():
        try:
            if token in p.read_text(errors="replace"):
                listed.append(p.stem.removeprefix("agent-"))
        except OSError:
            pass
    if listed:
        ids["directory listing (subagent file holding the token)"] = listed
    out["agent_id_sources"] = ids or "none"
    # Prefer the first candidate (in route order) whose agent-<id>.jsonl exists.
    subs = {p.name: p for p in sess.sub_paths()}
    cands = [(k, v) for k, vs in ids.items() for v in (vs if isinstance(vs, list) else [vs])]
    aid, src = next(((v, k) for k, v in cands if f"agent-{v}.jsonl" in subs),
                    (cands[0][1], cands[0][0] + " (no file yet)") if cands else (None, None))
    flat = {v for vs in ids.values() for v in (vs if isinstance(vs, list) else [vs])}
    out["agent_id"] = aid
    out["agent_id_found_by"] = src or "not found"
    out["agent_id_sources_agree"] = len(flat) <= 1
    path = subs.get(f"agent-{aid}.jsonl") if aid else None
    out["path"] = path
    out["transcript_path"] = str(path) if path else "not found"
    out["transcript_holds_token"] = bool(path) and token in path.read_text(errors="replace")
    return out


def copy_agent(ctx: Ctx, info: dict, label: str) -> Path | None:
    path = info.get("path")
    if not path or not path.exists():
        return None
    dest = ctx.art / f"{label}-{path.name}"
    shutil.copy2(path, dest)
    return dest


def dispatch_msg(cmd: str, desc: str, background: bool) -> str:
    mode = ("run_in_background true" if background
            else "in the foreground (do not set run_in_background)")
    tail = ("Then reply with one line: dispatched <the agent ID the tool returned>. Do not wait "
            "for the worker." if background
            else "Wait for its reply, then reply with one line: finished <the agent ID>")
    return (f"Call the Agent tool once with subagent_type \"{WORKER_AGENT}\", description "
            f"\"{desc}\", {mode}, and as prompt exactly the text between <<< and >>>: <<<{cmd}>>> "
            f"{tail}")


STOP_MSG = ("Now call the TaskStop tool on the background agent you dispatched, using the task or "
            "agent ID its launch returned. Then reply with one line: stopped: <the TaskStop result>")
STOP_NEEDLE = "TaskStop tool on the background agent"
SEND_NEEDLE = "SendMessage tool to send the worker"


def send_msg(aid: str | None) -> str:
    who = f"agent ID {aid}" if aid else "its agent ID"
    return (f"Use the SendMessage tool to send the worker you just dispatched in the foreground "
            f"(address it by {who}) this message: what did you run before? Then reply with one "
            f"line: worker answered: <its answer>")


# ---------------------------------------------------------------- step 2


def step2_stop(ctx: Ctx, sess: Session) -> tuple[dict, str]:
    token = ctx.token("300.2")
    ctx.needles.append(token)
    cmd = py_sleep_cmd(token)
    obs: dict = {"worker_command": cmd, "note": "Claude Code's Bash tool blocks foreground `sleep N`; the probe uses python3 time.sleep instead."}
    info: dict = {}
    try:
        sess.send(dispatch_msg(cmd, "usage stop worker", background=True))
        rows = wait_until(lambda: py_sleep_rows(token), DISPATCH_TIMEOUT, sess)
        if not rows:
            sess.save("no-sleep")
            raise StepFail(f"worker's `{cmd}` never started", sess.pane.splitlines()[-10:])
        obs["sleep_before_stop"] = brief(rows)
        idle(5, sess)
        info = wait_until(lambda: (lambda i: i if i.get("path") else None)(find_agent(sess, token)),
                          30, sess) or find_agent(sess, token)
        before = len(read_jsonl(info["path"])) if info.get("path") else None
        obs["transcript_records_before_stop"] = before
        sess.send(STOP_MSG)
        sent = time.monotonic()
        gone_at = None

        def stopped():
            nonlocal gone_at
            if gone_at is None and not py_sleep_rows(token):
                gone_at = round(time.monotonic() - sent, 1)
            return replied(sess, STOP_NEEDLE, "stopped")

        wait_until(stopped, TURN_TIMEOUT, sess)
        idle(10, sess)
        stopped()
        recs = sess.main_records()
        i = user_index(recs, STOP_NEEDLE) or 0
        res = tool_results(recs)
        stops = [u for u in tool_uses(recs, i) if u["name"] == "TaskStop"]
        info = find_agent(sess, token)
        path = info.get("path")
        after = len(read_jsonl(path)) if path else None
        obs.update({
            "taskstop_calls": [{"input": u["input"],
                                "result": (res.get(u["id"]) or {}).get("text", "<no result>")[:400]}
                               for u in stops] or "none",
            "sleep_gone_after_stop_s": gone_at,
            "sleep_alive_at_end": brief(py_sleep_rows(token)) or False,
            "transcript_records_after_stop": after,
            "usage": usage_analysis(read_jsonl(path)) if path else "no transcript",
            "transcript_tail": [summarize(r) for r in read_jsonl(path)[-3:]] if path else "none",
        })
        line = (f"stop: taskstop={bool(stops)} usage_records="
                f"{obs['usage']['assistant_records_with_message.usage'] if path else 'n/a'}")
        return obs, line
    finally:
        obs.update({k: v for k, v in info.items() if k != "path"})
        dest = copy_agent(ctx, info, "step2-stopped")
        obs["copied_to"] = dest.name if dest else "nothing to copy"
        if dest:
            ctx.add_source("claude-subagent-stopped", dest, "worker transcript after TaskStop")


def step2_resume(ctx: Ctx, sess: Session) -> tuple[dict, str]:
    token = f"lhprobe-resume-{ctx.uniq}"
    cmd = f"echo {token}"
    obs: dict = {"worker_command": cmd}
    info: dict = {}
    try:
        sess.send(dispatch_msg(cmd, "usage resume worker", background=False))
        finished = wait_until(lambda: replied(sess, "usage resume worker", "finished"),
                              TURN_TIMEOUT, sess)
        obs["finished_reply_detected"] = bool(finished)
        info = wait_until(lambda: (lambda i: i if i.get("path") else None)(find_agent(sess, token)),
                          30, sess) or find_agent(sess, token)
        path = info.get("path")
        if not path:
            sess.save("no-resume-worker")
            raise StepFail("the finished worker's transcript was not found",
                           sess.pane.splitlines()[-10:])
        before_recs = read_jsonl(path)
        before_ino = path.stat().st_ino
        obs["transcript_records_before_resume"] = len(before_recs)
        shutil.copy2(path, ctx.art / f"step2-resumed-before-{path.name}")
        progress("  step2: worker finished; asking the supervisor to SendMessage it")
        sess.send(send_msg(info.get("agent_id")))
        wait_until(lambda: replied(sess, SEND_NEEDLE, "worker answered"), TURN_TIMEOUT, sess)
        # A resumed worker may answer asynchronously; wait until the transcripts stop growing.
        last, stable_since, end = None, time.monotonic(), time.monotonic() + 60
        while time.monotonic() < end and time.monotonic() - stable_since < 15:
            sess.poll()
            now = (len(sess.main_records()),
                   tuple((p.name, len(read_jsonl(p))) for p in sess.sub_paths()))
            if now != last:
                last, stable_since = now, time.monotonic()
            time.sleep(3)
        recs = sess.main_records()
        i = user_index(recs, SEND_NEEDLE) or 0
        res = tool_results(recs)
        sends = [u for u in tool_uses(recs, i) if u["name"] == "SendMessage"]
        after_recs = read_jsonl(path) if path.exists() else []
        subs_now = [p.name for p in sess.sub_paths()]
        obs.update({
            "sendmessage_calls": [{"input": u["input"],
                                   "result": (res.get(u["id"]) or {}).get("text", "<no result>")[:400]}
                                  for u in sends] or "none",
            "supervisor_replies_after_followup": assistant_texts(recs, i)[-3:],
            "transcript_records_after_resume": len(after_recs),
            "same_file_same_inode": path.exists() and path.stat().st_ino == before_ino,
            "earlier_records_unchanged": after_recs[:len(before_recs)] == before_recs,
            "followup_in_this_transcript": user_index(after_recs[len(before_recs):],
                                                      "what did you run before") is not None,
            "subagent_files_in_session": subs_now,
            "other_files_holding_followup": [p.name for p in sess.sub_paths() if p != path and
                                             "what did you run before" in p.read_text(errors="replace")]
                                            or "none",
            "usage_whole_file": usage_analysis(after_recs),
            "usage_appended_part": usage_analysis(after_recs, len(before_recs)),
            "appended_tail": [summarize(r) for r in after_recs[len(before_recs):][-4:]] or "none",
        })
        line = (f"resume: sendmessage={bool(sends)} lines {len(before_recs)}->{len(after_recs)} "
                f"same_file={obs['same_file_same_inode']}")
        return obs, line
    finally:
        obs.update({k: v for k, v in info.items() if k != "path"})
        dest = copy_agent(ctx, info, "step2-resumed")
        obs["copied_to"] = dest.name if dest else "nothing to copy"
        if dest:
            ctx.add_source("claude-subagent-resumed", dest, "worker transcript after SendMessage")


def step2(ctx: Ctx):
    sess = Session(ctx, "step2")
    sess.start()
    obs, lines, excerpt = {}, [], []
    for name, fn in (("stop", step2_stop), ("resume", step2_resume)):
        progress(f"  step2: {name} part")
        try:
            o, line = fn(ctx, sess)
        except StepFail as e:
            o, line = {"failure": e.reason, "pane_last_lines": e.lines}, f"{name}: FAIL {e.reason}"
        except Exception:  # noqa: BLE001 - recorded, the other part still runs
            tb = traceback.format_exc()
            o, line = {"failure": tb.strip().splitlines()[-1]}, f"{name}: FAIL"
            excerpt += tb.strip().splitlines()[-4:]
        obs.update({f"{name} {k}": v for k, v in o.items()})
        lines.append(line)
        excerpt.append(line)
    sess.save("after")
    sess.kill()
    return "OBSERVED", excerpt[:10], obs, "; ".join(lines)


# ---------------------------------------------------------------- step 3


def step3_command(ctx: Ctx) -> str:
    return (f"echo \"sid=$CLAUDE_CODE_SESSION_ID lhprobe-sid-{ctx.uniq}\"; "
            f"env | cut -d= -f1 | grep '^CLAUDE' | sort | tr '\\n' ' '")


def step3(ctx: Ctx):
    sess = Session(ctx, "step3")
    token = f"lhprobe-sid-{ctx.uniq}"
    cmd = step3_command(ctx)
    obs: dict = {"command": cmd}
    ctx.facts = obs   # kept in the raw file even if the step fails part-way
    sess.start()
    sess.send(f"Run this command once with the Bash tool, exactly as written between <<< and >>>: "
              f"<<<{cmd}>>> Then reply with one line: sid check done")
    wait_until(lambda: replied(sess, token, "sid check done"), TURN_TIMEOUT, sess)
    idle(3, sess)
    sess.save("after")
    sid = sess.session_id()
    recs = sess.main_records()
    res = tool_results(recs)
    calls = [u for u in tool_uses(recs) if u["name"] == "Bash" and token in json.dumps(u["input"])]
    output = (res.get(calls[-1]["id"]) or {}).get("text", "") if calls else ""
    m = re.search(r"sid=(\S*)", output)
    echoed = m.group(1) if m and not m.group(1).startswith("lhprobe-sid-") else ""
    hooks = [hi for hi in sess.hook_inputs() if token in json.dumps(hi.get("tool_input"))]
    main = sess.main_path()
    obs.update({
        "session_id": sid,
        "session_id_source": sess.sid_source,
        "main_transcript_path": str(main) if main else "not found",
        "main_transcript_project_dir": main.parent.name if main else None,
        "hook_transcript_path": hooks[-1].get("transcript_path") if hooks else "no hook line",
        "hook_transcript_path_matches": bool(hooks and main and hooks[-1].get("transcript_path") == str(main)),
        "hook_input_keys": sorted(hooks[-1]) if hooks else None,
        "bash_calls_with_token": len(calls),
        "bash_input": calls[-1]["input"] if calls else "none",
        "bash_output": output[:400] or "none",
        "CLAUDE_CODE_SESSION_ID_visible": bool(echoed),
        "CLAUDE_CODE_SESSION_ID_value": echoed or "empty",
        "CLAUDE_CODE_SESSION_ID_equals_session_id": bool(echoed) and echoed == sid,
        "main_transcript_usage": usage_analysis(recs) if recs else "no transcript",
    })
    if not calls:
        raise StepFail("the supervisor never ran the sid command", sess.pane.splitlines()[-10:])
    sess.kill()
    summary = (f"sid_visible={obs['CLAUDE_CODE_SESSION_ID_visible']} "
               f"equals_session={obs['CLAUDE_CODE_SESSION_ID_equals_session_id']} "
               f"main_transcript={'found' if main else 'missing'}")
    return "OBSERVED", [output[:200]] + sess.pane.rstrip().splitlines()[-5:], obs, summary


# ---------------------------------------------------------------- step 4


FIXTURES = {
    "codex-rollout-interrupted": "codex-rollout-interrupted.sanitized.jsonl",
    "claude-subagent-stopped": "claude-subagent-stopped.sanitized.jsonl",
    "claude-subagent-resumed": "claude-subagent-resumed.sanitized.jsonl",
}


def leak_needles(ctx: Ctx) -> list[str]:
    home = str(Path.home())
    out = [home, json.dumps(home)[1:-1], "lhprobe", "lhprobe5 ", str(CODEX_HOME)]
    if Path.home().name and len(Path.home().name) >= 3:
        out.append(Path.home().name)
    if ctx.tmp:
        out.append(str(ctx.tmp))
    return list(dict.fromkeys(out))


def step4(ctx: Ctx):
    sources = read_json(ctx.manifest) or {}
    obs: dict = {"manifest": str(ctx.manifest), "keep_keys": sorted(SANITIZE_KEEP)}
    lines, written = [], 0
    for name, out_name in FIXTURES.items():
        src = sources.get(name)
        path = ctx.art / src["artifact"] if src else None
        if not path or not path.exists():
            obs[name] = "no source artifact (its step failed or was not run in this DIR)"
            lines.append(f"{name}: missing")
            continue
        raw = path.read_text(errors="replace").splitlines()
        clean = sanitize_lines(raw)
        dest = ctx.art / out_name
        dest.write_text("\n".join(clean) + ("\n" if clean else ""))
        written += 1
        text = dest.read_text()
        raw_recs, clean_recs = parse_lines("\n".join(raw)), parse_lines(text)
        if name.startswith("codex"):
            ru, cu = rollout_usage(raw_recs), rollout_usage(clean_recs)
            kept = {"last_cumulative_value": ru.get("last_cumulative_value") == cu.get("last_cumulative_value"),
                    "token_records": ru["token_records (holding total_token_usage)"]
                    == cu["token_records (holding total_token_usage)"]}
        else:
            ru, cu = usage_analysis(raw_recs), usage_analysis(clean_recs)
            kept = {k: ru[k] == cu[k] for k in ("assistant_records_with_message.usage",
                                                 "message_ids_with_more_than_one_record",
                                                 "output_tokens_sum_all_records", "models")}
        obs[name] = {
            "source": src["artifact"], "note": src.get("note"), "written": out_name,
            "lines raw -> sanitized": f"{len(raw)} -> {len(clean)}",
            "usage_facts_preserved": kept,
            "leak_hits": {n: text.count(n) for n in leak_needles(ctx) if n in text} or "none",
        }
        lines.append(f"{name}: {len(clean)} lines, preserved={all(kept.values())}, "
                     f"leaks={'none' if obs[name]['leak_hits'] == 'none' else 'SEE RAW'}")
    status = "OBSERVED" if written else "FAIL"
    return status, lines, obs, f"written={written}/{len(FIXTURES)}"


STEP_FUNCS = {1: step1, 2: step2, 3: step3, 4: step4}


# ---------------------------------------------------------------- dry run


def dry_commands(ctx: Ctx, n: int) -> list[str]:
    tm = lambda label: shlex.join(tmux_new_argv(ctx, f"lh5-{label}", str(ctx.fixture(label)),  # noqa: E731
                                                f"<out>/task5-artifacts/{label}-hooks.jsonl"))
    send = f"tmux -L {ctx.sock} send-keys -t lh5-<label> -l <message> ; ... Enter"
    if n == 1:
        first, tok = f"lhprobe-first-{ctx.uniq}", ctx.token("120.5")
        return ([f"{shlex.join(codex_first_argv())}  < {codex_prompt(first, tok)!r}  "
                 f"(cwd=<tmp>/lhprobe5 XXXX/codex repo, process_group=0)",
                 f"on the first non-prose item.completed whose command/aggregated_output holds `{first}`: "
                 f"kill -KILL -<codex pgid>",
                 f"read {CODEX_SESSIONS}/YYYY/MM/DD/*<thread_id>.jsonl; copy it"]
                + [shlex.join(a) + f"  < {RESUME_PROMPT!r}" + ("" if i == 0 else
                                                               "  (only if the first exits non-zero with no thread/turn events)")
                   for i, (_, a) in enumerate(codex_resume_variants("<thread_id>"))]
                + ["read the same rollout file again; compare record counts and cumulative totals"])
    if n == 2:
        return [tm("step2"), send,
                "  message 1: " + dispatch_msg(py_sleep_cmd(ctx.token('300.2')), "usage stop worker", True),
                f"  message 2 (once the sleep is in ps): {STOP_MSG}",
                "  message 3: " + dispatch_msg(f"echo lhprobe-resume-{ctx.uniq}", "usage resume worker", False),
                f"  message 4: {send_msg('<agent id>')}",
                "read ~/.claude/projects/*/<session_id>/subagents/agent-<id>.jsonl before and after"]
    if n == 3:
        return [tm("step3"), send,
                f"  message: Run this command once with the Bash tool ...: <<<{step3_command(ctx)}>>>",
                "read ~/.claude/projects/*/<session_id>.jsonl and the hook log"]
    if n == 4:
        return [f"sanitize the artifacts listed in <out>/task5-artifacts/fixture-sources.json into "
                f"<out>/task5-artifacts/{{{', '.join(FIXTURES.values())}}}"]
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
    if ctx.groups:
        try:
            glog = kill_owned_groups(ctx.groups, ctx.needles)
        except Exception as e:  # noqa: BLE001 - recorded, cleanup continues
            glog = [f"group cleanup error: {type(e).__name__}: {e}"]
        with open(ctx.raw, "a") as f:
            f.write("\n## Cleanup: process groups\n\n" + "".join(f"- {l}\n" for l in glog))
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
    ap = argparse.ArgumentParser(
        description="Task 5 runtime probe: interrupted-usage recovery. Run in an ordinary "
                    "terminal, not inside Claude Code. Takes about 10 minutes.")
    ap.add_argument("--out", required=True, help="directory for task5-raw.md and task5-artifacts/")
    ap.add_argument("--only", help="comma-separated step numbers to run (1-4)")
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
        print("probe_usage.py: refusing to run inside a Claude Code session (CLAUDECODE is set); "
              "run it from an ordinary terminal.", file=sys.stderr)
        return 2

    ctx.art.mkdir(parents=True, exist_ok=True)
    # Never overwrite earlier findings: a partial run (or a rerun) gets its own file.
    if args.only or ctx.raw.exists():
        stem = "task5-raw" + (f"-only-{'-'.join(map(str, steps))}" if args.only else "")
        cand, i = ctx.out / f"{stem}.md", 2
        while cand.exists():
            cand, i = ctx.out / f"{stem}-{i}.md", i + 1
        ctx.raw = cand
    ctx.raw.write_text(
        f"# Task 5 raw probe results: interrupted-usage recovery\n\n"
        f"- date: {datetime.now(timezone.utc).isoformat(timespec='seconds')}\n"
        f"- claude: {version(['claude', '--version'])}\n"
        f"- codex: {version(['codex', '--version'])}\n"
        f"- tmux: {version(['tmux', '-V'])}\n"
        f"- python: {sys.version.split()[0]}\n"
        f"- platform: {sys.platform}\n"
        f"- plugin_dir: {PLUGIN_DIR}\n"
        f"- supervisor_agent: {SUPER_AGENT}; worker_agent: {WORKER_AGENT}\n"
        f"- codex_home: {CODEX_HOME} ({'CODEX_HOME' if os.environ.get('CODEX_HOME') else 'default'})\n"
        f"- CLAUDE_CODE_SESSION_ID set in the probe's own env (removed for children): "
        f"{bool(os.environ.get('CLAUDE_CODE_SESSION_ID'))}\n"
        f"- unique_suffix: {ctx.uniq}\n"
        f"- steps: {steps}\n"
    )
    summary = []
    try:
        for n in steps:
            progress(f"Step {n}: {STEPS[n]} ...")
            ctx.cmds, ctx.sessions, ctx.facts = [], [], {}
            error = None
            try:
                status, excerpt, obs, line = STEP_FUNCS[n](ctx)
            except StepFail as e:
                status, line, excerpt, obs = "FAIL", e.reason, e.lines, {"failure": e.reason}
            except Exception:  # noqa: BLE001 - a failing step is a finding
                error = traceback.format_exc()
                status, line = "FAIL", error.strip().splitlines()[-1]
                excerpt, obs, error = error.strip().splitlines()[-10:], {}, error[-3000:]
            # Facts recorded on the way out of the step, whatever the outcome.
            obs.update(ctx.facts)
            for sess in ctx.sessions:
                for k, v in sess.facts().items():
                    obs[f"{sess.label} {k}"] = v
                try:
                    sess.kill()
                except Exception:  # noqa: BLE001
                    pass
            write_section(ctx, n, status, excerpt, obs, error=error)
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
