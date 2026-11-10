#!/usr/bin/env python3
"""Task 20: the first complete leadhand run, end to end, with the real `claude` and `codex`.

    python3 tests/e2e/run_e2e.py --out DIR [--mode headless|tmux] [--skip-push-probe]
                                 [--budget-minutes 45] [--keep] [--dry-run]

Run it once, in an ordinary terminal outside Claude Code (it refuses when CLAUDECODE is
set). One command runs everything unattended:

1. Step 1 (unless --skip-push-probe): codex_push_probe.py into the same DIR.
2. The fixture (make_fixture.py) in a temp folder whose paths contain spaces; the
   operator's leadhand user config is replaced by an empty file (LEADHAND_CONFIG).
3. `leadhand check`, `leadhand route` for both cards, `leadhand start`, all run from an
   unrelated directory.
4. The supervisor is started through `leadhand run <deck> --plugin-dir <this repo>` from
   the unrelated directory, so the run lock, holder.json and the launch environment are
   the real ones.
   - headless (default): with the hidden `--print` option (`claude -p --output-format
     stream-json --verbose`); the stream is saved with the time each line arrived.
   - tmux: interactive in a private tmux server; first-run dialogs are answered (the trust
     dialog by moving to "Yes, I trust this folder" and checking the highlighted line
     before Enter, never a bare Enter on "No, exit").
   If the session exits while the run has no outcome, the driver records what was still
   running at that moment and relaunches with the exact `leadhand launch-line` command,
   in a loop, until the run has an outcome or the budget (45 minutes) ends.
5. While polling (every 5 s) it samples status.json's headline.
6. After the run it collects the run directory, the stream(s), the supervisor and worker
   transcripts, the Codex rollouts and Git state, and performs every Task 20 Step 3 check
   plus the open checks (print-mode waiting, sandbox exclusion of the CLI, the heredoc
   record command, Agent model names, the run skill loading through the Skill tool).

Writes DIR/e2e-raw.md (PASS/FAIL/OBSERVED per check, with `key: value` facts) and
DIR/e2e-artifacts/. Exits 0 even when checks fail (failures are findings); 2 on a usage
error or under CLAUDECODE. Cleans up every process it started (checked by pid and start
time) and removes the temp folder unless --keep.

Extension point (Task 22, `--interrupt`): `Driver.poll_hooks` are called on every poll
with the driver and the current snapshot; a hook can signal the Codex process named in a
runner record or the current session (`Session.signal`) and the relaunch loop takes over.
"""
from __future__ import annotations

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
import threading
import time
import traceback
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
CLI = ROOT / "scripts" / "leadhand"
SKILL_FILE = ROOT / "skills" / "run" / "SKILL.md"
sys.path.insert(0, str(ROOT / "scripts" / "lib"))
sys.path.insert(0, str(HERE))

from leadhand import procs as lh_procs  # noqa: E402
from leadhand import usage as lh_usage  # noqa: E402

import codex_push_probe  # noqa: E402
import make_fixture  # noqa: E402

BUDGET_MINUTES = 45
POLL_S = 5.0
STATUS_KEEP_EVERY_S = 60.0       # a status sample is kept when the headline changes, or this often
AFTER_OUTCOME_GRACE_S = 180.0    # the session may still write the report after the outcome event
STOP_GRACE_S = 30.0
NO_PROGRESS_LIMIT = 3            # consecutive sessions that added no event: stop relaunching
MAX_SESSIONS = 15
CMD_TIMEOUT_S = 120
PRINT_WAIT_NOTE_S = 20.0         # a quiet gap after a turn's result this long counts as "waited"
PANE_SAVE_EVERY_S = 60.0
DIALOG_PATTERNS = [r"trust the files", r"Yes, I trust", r"Yes, proceed", r"Choose the text style",
                   r"Press Enter to continue", r"Enter to confirm", r"Enable auto mode",
                   r"Do you want to proceed\?"]
MENU_HIGHLIGHT = re.compile(r"^\s*❯\s*(.*)$", re.M)
MENU_WANTED = re.compile(r"\b(Yes|Proceed|Continue)\b")
MENU_FORBIDDEN = re.compile(r"\b(No|exit)\b", re.I)
MENU_MAX_MOVES = 6
DIALOG_REPEAT_LIMIT = 3
# Publishing commands. Built from parts so the two-word command never appears as a literal.
PUBLISH_RE = re.compile(r"\bgit\b[^\n;&|]*\b" + "pu" + r"sh\b|\bgh\s+pr\b|\bgh\s+repo\s+create\b")
CARD_PINS = {1: ("claude", "sonnet", "low"), 2: ("codex", "gpt-6-luna", "medium")}


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def parse_ts(ts: str | None) -> float | None:
    if not isinstance(ts, str):
        return None
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def read_jsonl(path: Path) -> list[dict]:
    out: list[dict] = []
    try:
        data = Path(path).read_text(encoding="utf-8", errors="replace")
    except (FileNotFoundError, IsADirectoryError):
        return out
    for line in data.splitlines():
        try:
            v = json.loads(line)
        except ValueError:
            continue
        if isinstance(v, dict):
            out.append(v)
    return out


def read_json(path: Path) -> dict:
    try:
        v = json.loads(Path(path).read_text(encoding="utf-8"))
    except (FileNotFoundError, ValueError, IsADirectoryError):
        return {}
    return v if isinstance(v, dict) else {}


def real(p) -> str:
    return os.path.realpath(str(p))


def short(v, n: int = 300) -> str:
    s = v if isinstance(v, str) else json.dumps(v, default=str)
    s = s.replace("\n", " | ")
    return s if len(s) <= n else s[: n - 3] + "..."


# ---------------------------------------------------------------- transcripts

@dataclass
class ToolUse:
    id: str
    name: str
    input: dict
    file: str
    ts: str | None


@dataclass
class ToolResult:
    text: str
    is_error: bool
    sandboxed: object       # the user record's `sandboxed` flag (True/False/None when absent)
    record: dict


def strings(v) -> str:
    """Every string inside a JSON value, joined by spaces."""
    if isinstance(v, str):
        return v
    if isinstance(v, dict):
        return " ".join(strings(x) for x in v.values())
    if isinstance(v, list):
        return " ".join(strings(x) for x in v)
    return ""


def tool_uses(records: list[dict], file: str) -> list[ToolUse]:
    out = []
    for r in records:
        if r.get("type") != "assistant":
            continue
        for c in (r.get("message") or {}).get("content") or []:
            if isinstance(c, dict) and c.get("type") == "tool_use":
                out.append(ToolUse(c.get("id", ""), c.get("name", ""), c.get("input") or {}, file,
                                   r.get("timestamp")))
    return out


def tool_results(records: list[dict]) -> dict[str, ToolResult]:
    out = {}
    for r in records:
        if r.get("type") != "user":
            continue
        content = (r.get("message") or {}).get("content")
        if not isinstance(content, list):
            continue
        for c in content:
            if isinstance(c, dict) and c.get("type") == "tool_result":
                body = c.get("content")
                if isinstance(body, list):
                    body = " ".join(str(x.get("text", "")) for x in body if isinstance(x, dict))
                out[c.get("tool_use_id", "")] = ToolResult(str(body or ""), bool(c.get("is_error")),
                                                           r.get("sandboxed"), r)
    return out


# ---------------------------------------------------------------- sessions

@dataclass
class Session:
    n: int
    mode: str
    argv: list[str]
    how: str                              # "first launch" | "relaunch (launch-line)"
    started_at: float = 0.0
    started_utc: str = ""
    exited_at: float | None = None
    exited_utc: str | None = None
    exit_code: object = None
    seq_at_start: int = 0                 # the last event seq when the session started
    launcher_pid: int | None = None
    launcher_start: str | None = None
    stream_meta: list[dict] = field(default_factory=list)   # headless: (t, type, subtype, ...)
    notes: dict = field(default_factory=dict)

    def alive(self) -> bool:
        raise NotImplementedError

    def poll(self) -> None:
        pass

    def signal(self, sig: int) -> bool:
        """Signal the launcher (it forwards SIGINT/SIGTERM to claude). Checked by pid + start."""
        if self.launcher_pid and lh_procs.same_process(self.launcher_pid, self.launcher_start):
            try:
                os.kill(self.launcher_pid, sig)
                return True
            except ProcessLookupError:
                return False
        return False

    def stop(self) -> None:
        raise NotImplementedError


class HeadlessSession(Session):
    def start(self, cwd: Path, env: dict, art: Path) -> None:
        self.dir = art / f"session-{self.n}"
        self.dir.mkdir(parents=True, exist_ok=True)
        self.err = open(self.dir / "launcher.stderr.txt", "wb")
        self.started_at, self.started_utc = time.time(), utc_now()
        self.proc = subprocess.Popen(self.argv, cwd=str(cwd), env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=self.err, start_new_session=True)
        self.launcher_pid = self.proc.pid
        self.launcher_start = lh_procs.process_start_time(self.proc.pid)
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self) -> None:
        with open(self.dir / "stream.jsonl", "wb") as raw, open(self.dir / "stream-times.jsonl", "w") as times:
            for line in self.proc.stdout:
                t = time.time()
                raw.write(line)
                raw.flush()
                meta = {"t": round(t, 3), "type": None}
                try:
                    rec = json.loads(line)
                    meta["type"], meta["subtype"] = rec.get("type"), rec.get("subtype")
                    if rec.get("type") == "result":
                        meta.update({k: rec.get(k) for k in ("result_index", "is_error", "num_turns",
                                                                "total_cost_usd", "duration_ms")})
                        meta["origin"] = (rec.get("origin") or {}).get("kind")
                    if rec.get("type") == "system" and rec.get("subtype") == "init":
                        meta.update({k: rec.get(k) for k in ("session_id", "model", "permissionMode")})
                except ValueError:
                    meta["type"] = "unparsed"
                self.stream_meta.append(meta)
                times.write(json.dumps(meta) + "\n")
                times.flush()

    def alive(self) -> bool:
        if self.proc.poll() is None:
            return True
        if self.exited_at is None:
            self.exited_at, self.exited_utc, self.exit_code = time.time(), utc_now(), self.proc.returncode
            self.reader.join(timeout=10)
            self.err.close()
        return False

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=STOP_GRACE_S)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(self.proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.proc.wait(timeout=10)
            self.notes["stopped_by_driver"] = True
        self.alive()


class TmuxSession(Session):
    def start(self, cwd: Path, env: dict, art: Path, sock: str) -> None:
        self.dir = art / f"session-{self.n}"
        self.dir.mkdir(parents=True, exist_ok=True)
        self.sock, self.name = sock, f"{sock}-{self.n}"
        extra = []
        for key in ("LEADHAND_CONFIG",):
            extra += ["-e", f"{key}={env[key]}"]
        self.tmux_argv = ["tmux", "-L", sock, "start-server", ";", "set-option", "-g", "remain-on-exit", "on",
                          ";", "new-session", "-d", "-s", self.name, "-c", str(cwd), "-x", "200", "-y", "50",
                          *extra, "env", "-u", "CLAUDECODE", *self.argv]
        self.started_at, self.started_utc = time.time(), utc_now()
        r = subprocess.run(self.tmux_argv, capture_output=True, text=True, timeout=30, env=env)
        self.notes["tmux_exit"] = r.returncode
        if r.returncode != 0:
            self.notes["tmux_stderr"] = r.stderr.strip()
        pid = self._fmt("#{pane_pid}")
        self.launcher_pid = int(pid) if pid.isdigit() else None
        self.launcher_start = lh_procs.process_start_time(self.launcher_pid) if self.launcher_pid else None
        self.answered: list[str] = []
        self.selections: list[dict] = []
        self.last_save = 0.0

    def tmux(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(["tmux", "-L", self.sock, *args], capture_output=True, text=True, timeout=30)

    def _fmt(self, fmt: str) -> str:
        r = self.tmux("display-message", "-p", "-t", self.name, fmt)
        return r.stdout.strip() if r.returncode == 0 else ""

    def capture(self) -> str:
        return self.tmux("capture-pane", "-p", "-t", self.name).stdout

    def alive(self) -> bool:
        if self._fmt("#{pane_dead}") == "0":
            return True
        if self.exited_at is None:
            self.exited_at, self.exited_utc = time.time(), utc_now()
            status = self._fmt("#{pane_dead_status}")
            self.exit_code = int(status) if status.lstrip("-").isdigit() else status or "unknown"
            (self.dir / "pane-final.txt").write_text(self.capture(), encoding="utf-8")
        return False

    def _select(self) -> tuple[bool, str | None, list[str]]:
        pane, keys = self.capture(), []
        for move in range(MENU_MAX_MOVES + 1):
            m = MENU_HIGHLIGHT.search(pane)
            has_yes = any(MENU_WANTED.search(line) for line in pane.splitlines())
            if not m or not has_yes:
                if m and MENU_FORBIDDEN.search(m.group(1)):
                    return False, m.group(1).strip(), keys
                self.tmux("send-keys", "-t", self.name, "Enter")
                return True, m.group(1).strip() if m else None, keys + ["Enter"]
            line = m.group(1).strip()
            if MENU_WANTED.search(line) and not MENU_FORBIDDEN.search(line):
                self.tmux("send-keys", "-t", self.name, "Enter")
                return True, line, keys + ["Enter"]
            if move == MENU_MAX_MOVES:
                return False, line, keys
            self.tmux("send-keys", "-t", self.name, "Down")
            keys.append("Down")
            time.sleep(0.5)
            pane = self.capture()
        return False, None, keys

    def poll(self) -> None:
        if not self.alive():
            return
        pane = self.capture()
        now = time.time()
        if now - self.last_save >= PANE_SAVE_EVERY_S:
            (self.dir / f"pane-{int(now - self.started_at):05d}s.txt").write_text(pane, encoding="utf-8")
            self.last_save = now
        for pat in DIALOG_PATTERNS:
            if re.search(pat, pane) and self.answered.count(pat) < DIALOG_REPEAT_LIMIT:
                self.answered.append(pat)
                (self.dir / f"dialog-{len(self.answered)}.txt").write_text(pane, encoding="utf-8")
                ok, hl, keys = self._select()
                self.selections.append({"dialog": pat, "ok": ok, "highlighted": hl, "keys": keys,
                                        "at_s": round(now - self.started_at, 1)})
                if not ok:
                    self.answered += [pat] * DIALOG_REPEAT_LIMIT
                time.sleep(2)
                break
        self.notes["dialogs"] = self.selections

    def stop(self) -> None:
        if self.alive():
            (self.dir / "pane-at-stop.txt").write_text(self.capture(), encoding="utf-8")
            self.signal(signal.SIGTERM)
            deadline = time.time() + STOP_GRACE_S
            while time.time() < deadline and self.alive():
                time.sleep(1)
            if self.alive() and self.launcher_pid and lh_procs.same_process(self.launcher_pid, self.launcher_start):
                os.kill(self.launcher_pid, signal.SIGKILL)
            self.notes["stopped_by_driver"] = True
        self.tmux("kill-session", "-t", self.name)
        self.alive()


# ---------------------------------------------------------------- checks

@dataclass
class Check:
    name: str
    verdict: str
    how: str
    facts: dict
    excerpt: list[str] = field(default_factory=list)


class Driver:
    def __init__(self, args):
        self.out = Path(args.out).expanduser().absolute()
        self.art = self.out / "e2e-artifacts"
        self.raw = self.out / "e2e-raw.md"
        self.mode = args.mode
        self.budget_s = args.budget_minutes * 60
        self.keep = args.keep
        self.skip_push_probe = args.skip_push_probe
        self.tmp: Path | None = None
        self.fx: dict = {}
        self.run_dir: Path | None = None
        self.env: dict = {}
        self.commands: list[tuple[str, object, str]] = []   # (argv, exit, cwd)
        self.sessions: list[Session] = []
        self.status_samples: list[dict] = []
        self.checks: list[Check] = []
        self.facts: dict = {}
        self.errors: list[str] = []
        self.poll_hooks: list[Callable[["Driver", dict], None]] = []
        self.tmux_sock = f"lhe2e-{os.getpid()}"
        self.t0 = time.time()
        self.outcome_seen_at: float | None = None
        self.stop_reason = ""
        self.origin_refs_before = ""

    # -------------------------------------------------------------- helpers

    def log(self, msg: str) -> None:
        line = f"[{utc_now()}] {msg}"
        print(line, flush=True)
        with open(self.art / "driver.log", "a", encoding="utf-8") as f:
            f.write(line + "\n")

    def cmd(self, argv: list[str], *, cwd: Path | None = None, timeout: float = CMD_TIMEOUT_S,
            env: dict | None = None, record: bool = True) -> subprocess.CompletedProcess:
        cwd = cwd or Path(self.fx.get("elsewhere") or self.out)
        try:
            r = subprocess.run(argv, cwd=str(cwd), env=env or self.env or None, capture_output=True, text=True,
                               timeout=timeout, stdin=subprocess.DEVNULL)
        except subprocess.TimeoutExpired as e:
            r = subprocess.CompletedProcess(argv, f"timeout after {timeout}s", e.stdout or "", e.stderr or "")
        except OSError as e:
            r = subprocess.CompletedProcess(argv, f"oserror: {e}", "", str(e))
        if record:
            self.commands.append((shlex.join(argv), r.returncode, str(cwd)))
            with open(self.art / "commands.log", "a", encoding="utf-8") as f:
                f.write(f"$ (cwd={cwd}) {shlex.join(argv)}\n-> exit {r.returncode}\n{r.stdout}{r.stderr}\n")
        return r

    def git(self, *args: str, repo: str | None = None) -> str:
        r = self.cmd(["git", "-C", repo or self.fx["repo"], *args], record=False)
        return (r.stdout or "").strip() if r.returncode == 0 else ""

    def events(self) -> list[dict]:
        return read_jsonl(self.run_dir / "events.jsonl") if self.run_dir else []

    # -------------------------------------------------------------- plan

    def launch_argv_first(self) -> list[str]:
        deck = Path(self.run_dir) / "deck.md" if self.run_dir else Path("<run dir>/deck.md")
        argv = [str(CLI), "run", str(deck), "--plugin-dir", str(ROOT)]
        return argv + (["--print"] if self.mode == "headless" else [])

    def plan(self) -> None:
        t = "<tmp>/lh e2e XXXX"
        print("Plan (dry run; nothing is executed):")
        if not self.skip_push_probe:
            print("0. Step 1, Codex push probe (codex_push_probe.py) into the same --out:")
            codex_push_probe.Probe(self.out, False, True).plan()
        print(f"1. fixture: python3 {HERE / 'make_fixture.py'} '{t}'  (repo '{t}/fixture repo', bare "
              f"'{t}/origin.git', docs_dir '{t}/leadhand docs', deck '{t}/plans/fixture deck.md')")
        print(f"   env: LEADHAND_CONFIG='{t}/user config.toml' (empty); every command runs from '{t}/elsewhere'")
        print("2. versions: claude --version; codex --version" + ("; tmux -V" if self.mode == "tmux" else ""))
        deck, repo = f"{t}/plans/fixture deck.md", f"{t}/fixture repo"
        for argv in ([str(CLI), "check", deck, "--repo", repo],
                     [str(CLI), "route", deck, "1", "implementer", "--repo", repo],
                     [str(CLI), "route", deck, "2", "implementer", "--repo", repo],
                     [str(CLI), "start", deck, "--repo", repo]):
            print(f"   {shlex.join(argv)}")
        first = [str(CLI), "run", "<run dir>/deck.md", "--plugin-dir", str(ROOT)]
        if self.mode == "headless":
            print(f"3. launch (headless): {shlex.join(first + ['--print'])}")
            print("   -> claude ... --permission-mode auto -p --output-format stream-json --verbose '<first message>'")
        else:
            print(f"3. launch (tmux): tmux -L {self.tmux_sock} ... new-session -d -s {self.tmux_sock}-1 -c "
                  f"'{t}/elsewhere' -x 200 -y 50 env -u CLAUDECODE {shlex.join(first)}")
            print("   dialogs: trust dialog answered with Down, then Enter once 'Yes, I trust' is highlighted")
        print(f"4. poll every {POLL_S:.0f}s: status.json headline, events.jsonl; budget {self.budget_s // 60} min")
        print(f"   session exits without an outcome -> record open dispatches, relaunch with "
              f"`{CLI} launch-line <run dir>`" + (" + --print" if self.mode == "headless" else ""))
        print(f"5. after: {CLI} status <run dir>; {CLI} report <run dir> (only if report.md is missing)")
        print("6. checks:")
        for name, how in CHECK_DOCS:
            print(f"   - {name}: {how}")
        print(f"7. write {self.raw} and {self.art}/; stop sessions, runners and tmux; remove the temp dir")

    # -------------------------------------------------------------- phases

    def preflight(self) -> bool:
        missing = [b for b in ("claude", "codex", "git") + (("tmux",) if self.mode == "tmux" else ())
                   if shutil.which(b) is None]
        self.facts["binaries missing"] = missing or "none"
        if any(c.isspace() for c in str(CLI)):
            self.facts["cli path has whitespace"] = str(CLI)
        for name, argv in (("claude", ["claude", "--version"]), ("codex", ["codex", "--version"]),
                           ("tmux", ["tmux", "-V"]), ("git", ["git", "--version"])):
            if shutil.which(argv[0]):
                r = self.cmd(argv, cwd=self.out)
                self.facts[f"version {name}"] = (r.stdout or r.stderr).strip()
        self.facts["version python"] = sys.version.split()[0]
        self.facts["platform"] = sys.platform
        user_cfg = Path.home() / ".config" / "leadhand" / "config.toml"
        self.facts["operator leadhand user config exists (not read)"] = user_cfg.exists()
        return not missing

    def setup(self) -> bool:
        self.tmp = Path(tempfile.mkdtemp(prefix="lh e2e "))
        self.fx = make_fixture.make_fixture(self.tmp)
        self.env = {k: v for k, v in os.environ.items() if k != "CLAUDECODE"}
        self.env["LEADHAND_CONFIG"] = self.fx["user_config"]
        (self.art / "fixture.json").write_text(json.dumps(self.fx, indent=2), encoding="utf-8")
        self.origin_refs_before = self.git("for-each-ref", repo=self.fx["origin"])
        deck, repo = self.fx["deck"], self.fx["repo"]
        r = self.cmd([str(CLI), "check", deck, "--repo", repo])
        self.facts["leadhand check"] = f"exit {r.returncode}: {short(r.stdout.strip())}"
        for card in (1, 2):
            r = self.cmd([str(CLI), "route", deck, str(card), "implementer", "--repo", repo])
            self.facts[f"route card {card} implementer"] = short(r.stdout.strip() or r.stderr.strip())
        r = self.cmd([str(CLI), "start", deck, "--repo", repo])
        m = re.search(r"^run: (.+)$", r.stdout, re.M)
        self.facts["leadhand start"] = f"exit {r.returncode}: {short(r.stdout.strip() or r.stderr.strip())}"
        if r.returncode != 0 or not m:
            self.errors.append("leadhand start failed")
            return False
        self.run_dir = Path(m.group(1).strip())
        meta = read_json(self.run_dir / "run.json")
        self.facts["run dir"] = str(self.run_dir)
        self.facts["run branch"] = meta.get("branch")
        self.facts["workspace"] = meta.get("workspace")
        return True

    def launch(self, argv: list[str], how: str) -> Session:
        n = len(self.sessions) + 1
        cls = HeadlessSession if self.mode == "headless" else TmuxSession
        s = cls(n=n, mode=self.mode, argv=argv, how=how)
        s.seq_at_start = max((e.get("seq") or 0 for e in self.events()), default=0)
        if self.mode == "headless":
            s.start(Path(self.fx["elsewhere"]), self.env, self.art / "sessions")
        else:
            s.start(Path(self.fx["elsewhere"]), self.env, self.art / "sessions", self.tmux_sock)
        self.commands.append((shlex.join(argv), "(session)", self.fx["elsewhere"]))
        self.sessions.append(s)
        self.log(f"session {n} started ({how}): {shlex.join(argv)}")
        return s

    def relaunch_argv(self) -> list[str] | None:
        r = self.cmd([str(CLI), "launch-line", str(self.run_dir)])
        line = r.stdout.strip()
        if r.returncode != 0 or not line:
            self.errors.append(f"launch-line failed: {r.stderr.strip()}")
            return None
        argv = shlex.split(line)
        self.facts.setdefault("launch-line outputs", []).append(line)
        if "--plugin-dir" not in argv:
            # holder.json records the plugin dir only once a launch got that far.
            self.facts.setdefault("launch-line without --plugin-dir (driver added it)", []).append(line)
            argv += ["--plugin-dir", str(ROOT)]
        return argv + (["--print"] if self.mode == "headless" else [])

    def sample_status(self, force: bool = False) -> dict:
        st = read_json(self.run_dir / "status.json")
        headline = st.get("headline")
        last = self.status_samples[-1] if self.status_samples else None
        now = time.time()
        if headline is not None and (force or last is None or last["headline"] != headline
                                     or now - last["t"] >= STATUS_KEEP_EVERY_S):
            sample = {"t": round(now, 3), "utc": utc_now(), "elapsed_s": round(now - self.t0, 1),
                      "headline": headline, "activity": st.get("activity")}
            self.status_samples.append(sample)
            with open(self.art / "status-samples.jsonl", "a", encoding="utf-8") as f:
                f.write(json.dumps(sample) + "\n")
            if last is None or last["headline"] != headline:
                self.log(f"status: {headline}")
        return st

    def analyze_exit(self, s: Session, events: list[dict]) -> None:
        """What was still running when a session exited, and what happened after."""
        exit_t = s.exited_at or time.time()
        started, results = {}, {}
        for e in events:
            if e.get("kind") == "worker_started":
                started[e.get("dispatch_id")] = e
            elif e.get("kind") == "result_recorded":
                results[e.get("dispatch_id")] = e
        open_at_exit = []
        for did, e in started.items():
            t = parse_ts(e.get("ts")) or 0
            r = results.get(did)
            if t <= exit_t and (r is None or (parse_ts(r.get("ts")) or 0) > exit_t):
                open_at_exit.append(did)
        runners_after = []
        wdir = self.run_dir / "workers"
        if wdir.is_dir():
            for d in sorted(wdir.iterdir()):
                res = d / "result.json"
                if res.exists() and res.stat().st_mtime > exit_t:
                    runners_after.append(d.name)
        outcome = any(e.get("kind") == "outcome" for e in events if (parse_ts(e.get("ts")) or 0) <= exit_t)
        # Progress = events the session caused; the launcher's own `resumed` does not count.
        added = [e for e in events if (e.get("seq") or 0) > s.seq_at_start and e.get("kind") != "resumed"
                 and (parse_ts(e.get("ts")) or 0) <= exit_t + 2]
        s.notes.update({"events_added": len(added),
                        "dispatches_open_at_exit": open_at_exit or "none",
                        "codex_results_written_after_exit": runners_after or "none",
                        "run_had_outcome_at_exit": outcome,
                        "exited_while_workers_running": bool(open_at_exit) and not outcome})
        if s.stream_meta:
            results_meta = [m for m in s.stream_meta if m.get("type") == "result"]
            gaps = []
            for i, m in enumerate(s.stream_meta[:-1]):
                if m.get("type") == "result":
                    nxt = s.stream_meta[i + 1]
                    gaps.append({"after_result_index": m.get("result_index"),
                                 "gap_s": round(nxt["t"] - m["t"], 1),
                                 "next": f"{nxt.get('type')}/{nxt.get('subtype')}"})
            s.notes.update({
                "stream_lines": len(s.stream_meta),
                "result_records": len(results_meta),
                "max_result_index": max((m.get("result_index") or 0 for m in results_meta), default=None),
                "task_notifications": sum(1 for m in s.stream_meta if m.get("subtype") == "task_notification"),
                "quiet_gaps_after_a_result": gaps or "none",
                "longest_wait_after_a_result_s": max((g["gap_s"] for g in gaps), default=0),
                "last_result_cost_usd": next((m.get("total_cost_usd") for m in reversed(results_meta)), None),
                "session_ids": sorted({m.get("session_id") for m in s.stream_meta if m.get("session_id")}),
                "last_line_to_exit_s": round(exit_t - s.stream_meta[-1]["t"], 1),
            })

    def drive(self) -> None:
        deadline = self.t0 + self.budget_s
        session = self.launch(self.launch_argv_first(), "first launch")
        no_progress = 0
        while True:
            now = time.time()
            events = self.events()
            status = self.sample_status()
            session.poll()
            snapshot = {"events": events, "status": status, "session": session, "now": now}
            for hook in self.poll_hooks:
                try:
                    hook(self, snapshot)
                except Exception:
                    self.errors.append("poll hook: " + traceback.format_exc().splitlines()[-1])
            outcome = next((e for e in reversed(events) if e.get("kind") == "outcome"), None)
            if outcome and self.outcome_seen_at is None:
                self.outcome_seen_at = now
                self.log(f"outcome: {outcome.get('outcome')} {outcome.get('qualifier') or ''}".rstrip())
            alive = session.alive()
            if outcome:
                reported = any(e.get("kind") == "report_written" for e in events)
                if not alive or now - self.outcome_seen_at > AFTER_OUTCOME_GRACE_S or (
                        self.mode == "tmux" and reported and now - self.outcome_seen_at > 20):
                    self.stop_reason = f"outcome {outcome.get('outcome')}"
                    break
            elif not alive:
                self.analyze_exit(session, events)
                self.log(f"session {session.n} exited (code {session.exit_code}) without an outcome; "
                         f"open dispatches: {session.notes.get('dispatches_open_at_exit')}")
                no_progress = 0 if session.notes.get("events_added") else no_progress + 1
                if now >= deadline:
                    self.stop_reason = "budget ended"
                    break
                if no_progress >= NO_PROGRESS_LIMIT:
                    self.stop_reason = f"{no_progress} sessions in a row added no event; stopped relaunching"
                    break
                if len(self.sessions) >= MAX_SESSIONS:
                    self.stop_reason = f"{MAX_SESSIONS} sessions started; stopped relaunching"
                    break
                time.sleep(5 if no_progress == 0 else 20)
                argv = self.relaunch_argv()
                if argv is None:
                    self.stop_reason = "launch-line failed"
                    break
                session = self.launch(argv, "relaunch (launch-line)")
                continue
            if now >= deadline:
                self.stop_reason = "budget ended"
                break
            time.sleep(POLL_S)
        self.log(f"stopping: {self.stop_reason}")
        self.sample_status(force=True)

    def stop_everything(self) -> None:
        for s in self.sessions:
            try:
                s.stop()
            except Exception:
                self.errors.append(f"stop session {s.n}: " + traceback.format_exc().splitlines()[-1])
        if self.mode == "tmux":
            subprocess.run(["tmux", "-L", self.tmux_sock, "kill-server"], capture_output=True, timeout=30)
        if self.run_dir is None:
            return
        wdir = self.run_dir / "workers"
        stopped = []
        for d in sorted(wdir.iterdir()) if wdir.is_dir() else []:
            if (d / "result.json").exists():
                continue
            r = self.cmd([str(CLI), "stop-dispatch", str(self.run_dir), "--dispatch", d.name], timeout=120)
            stopped.append(f"{d.name}: exit {r.returncode}")
            rj = read_json(d / "runner.json")
            for pk, sk in (("runner_pid", "runner_start"), ("codex_pid", "codex_start"), ("pid", "start")):
                pid, start = rj.get(pk), rj.get(sk)
                if isinstance(pid, int) and isinstance(start, str) and lh_procs.same_process(pid, start):
                    try:
                        os.kill(pid, signal.SIGKILL)
                        stopped.append(f"{d.name}: SIGKILL {pk} {pid}")
                    except ProcessLookupError:
                        pass
        self.facts["runners stopped by the driver"] = stopped or "none"

    def finish_run(self) -> None:
        r = self.cmd([str(CLI), "status", str(self.run_dir)])
        (self.art / "leadhand-status.txt").write_text(r.stdout + r.stderr, encoding="utf-8")
        report = self.run_dir / "report.md"
        self.facts["report.md written by the run"] = report.exists()
        if not report.exists():
            r = self.cmd([str(CLI), "report", str(self.run_dir)])
            self.facts["leadhand report (driver)"] = f"exit {r.returncode}: {short(r.stdout.strip() or r.stderr.strip())}"

    # -------------------------------------------------------------- collection

    def session_ids(self) -> list[str]:
        ids = list(read_json(self.run_dir / "run.json").get("supervisor_sessions") or [])
        for s in self.sessions:
            for sid in s.notes.get("session_ids") or []:
                if sid not in ids:
                    ids.append(sid)
        return ids

    def transcripts(self) -> tuple[list[Path], list[Path]]:
        mains, subs = [], []
        for sid in self.session_ids():
            p = lh_usage.claude_session_transcript(sid)
            if p is None:
                continue
            mains.append(p)
            subs += sorted((p.parent / sid / "subagents").glob("agent-*.jsonl"))
        return mains, subs

    def collect(self) -> None:
        if self.run_dir and self.run_dir.is_dir():
            shutil.copytree(self.run_dir, self.art / "run-dir", dirs_exist_ok=True,
                            ignore=shutil.ignore_patterns("gh-config"))
        mains, subs = self.transcripts()
        tdir = self.art / "transcripts"
        tdir.mkdir(exist_ok=True)
        for p in mains:
            shutil.copy2(p, tdir / p.name)
        for p in subs:
            d = tdir / p.parent.parent.name / "subagents"
            d.mkdir(parents=True, exist_ok=True)
            shutil.copy2(p, d / p.name)
        self.facts["transcripts found"] = [str(p) for p in mains + subs] or "none"
        cdir = self.art / "codex-rollouts"
        found = []
        for a in read_jsonl(self.run_dir / "attempts.jsonl"):
            if a.get("vendor") == "codex" and a.get("worker_id"):
                p = lh_usage.codex_session_file(a["worker_id"])
                if p is not None:
                    cdir.mkdir(exist_ok=True)
                    shutil.copy2(p, cdir / p.name)
                    found.append(str(p))
        self.facts["codex rollouts found"] = found or "none"
        gdir = self.art / "git"
        gdir.mkdir(exist_ok=True)
        branch = read_json(self.run_dir / "run.json").get("branch") or ""
        for name, args, repo in (
                ("log-all.txt", ("log", "--all", "--graph", "--format=%h %d %s"), None),
                ("run-branch.diff", ("diff", f"{self.fx['base_sha']}..refs/heads/{branch}"), None),
                ("refs.txt", ("for-each-ref",), None),
                ("origin-refs-after.txt", ("for-each-ref",), self.fx["origin"])):
            (gdir / name).write_text(self.git(*args, repo=repo) + "\n", encoding="utf-8")
        (gdir / "origin-refs-before.txt").write_text(self.origin_refs_before + "\n", encoding="utf-8")

    # -------------------------------------------------------------- checks

    def add(self, name: str, verdict: str, facts: dict, excerpt: list[str] | None = None) -> None:
        how = dict(CHECK_DOCS).get(name, "")
        self.checks.append(Check(name, verdict, how, facts, excerpt or []))

    def run_checks(self) -> None:
        events = self.events()
        meta = read_json(self.run_dir / "run.json")
        attempts = read_jsonl(self.run_dir / "attempts.jsonl")
        charges = read_jsonl(self.run_dir / "charges.jsonl")
        branch = meta.get("branch") or ""
        head = self.git("rev-parse", f"refs/heads/{branch}")
        mains, subs = self.transcripts()
        main_recs = {str(p): read_jsonl(p) for p in mains}
        sub_recs = {str(p): read_jsonl(p) for p in subs}
        steps = [self.c_outcome, self.c_branch, self.c_report, self.c_gates, self.c_attempts,
                 self.c_usage, self.c_order, self.c_workspace, self.c_headline, self.c_publish,
                 self.c_elapsed, self.c_print_wait, self.c_sandbox, self.c_agent_models, self.c_skill]
        ctx = dict(events=events, meta=meta, attempts=attempts, charges=charges, branch=branch, head=head,
                   main_recs=main_recs, sub_recs=sub_recs)
        for fn in steps:
            try:
                fn(**ctx)
            except Exception:
                self.add(fn.__name__, "FAIL", {"error": traceback.format_exc().splitlines()[-1]},
                         traceback.format_exc().splitlines()[-8:])

    def c_outcome(self, events, **_):
        out = [e for e in events if e.get("kind") == "outcome"]
        last = out[-1] if out else {}
        self.add("outcome complete", "PASS" if last.get("outcome") == "complete" else "FAIL",
                 {"outcome events": [(e.get("outcome"), e.get("qualifier"), e.get("reasons")) for e in out] or "none",
                  "stop reason": self.stop_reason})

    def c_branch(self, events, branch, head, **_):
        acc = {e.get("card"): e.get("sha") for e in events if e.get("kind") == "accepted"}
        facts = {"run branch": branch, "run head": head or "missing", "accepted": acc or "none"}
        ok = bool(head)
        for card in (1, 2):
            sha = acc.get(card)
            on = bool(sha) and self.cmd(["git", "-C", self.fx["repo"], "merge-base", "--is-ancestor", sha,
                                         f"refs/heads/{branch}"], record=False).returncode == 0
            facts[f"card {card} commit on run branch"] = on
            ok = ok and on
        log = self.git("log", "--format=%h %s", f"{self.fx['base_sha']}..refs/heads/{branch}")
        facts["commits since base"] = log.splitlines() or "none"
        src = self.git("show", f"refs/heads/{branch}:calc/__init__.py")
        facts["calc has subtract/multiply"] = ("def subtract" in src, "def multiply" in src)
        ws = read_json(self.run_dir / "run.json").get("workspace")
        if ws and Path(ws).is_dir():
            r = self.cmd(["python3", "-m", "unittest", "discover", "-s", "tests"], cwd=Path(ws), timeout=120)
            facts["fixture tests in the workspace"] = f"exit {r.returncode}: {short((r.stderr or '').strip()[-200:])}"
        self.add("run branch holds both cards' commits", "PASS" if ok else "FAIL", facts)

    def c_report(self, **_):
        report = self.run_dir / "report.md"
        lines = report.read_text(encoding="utf-8").splitlines() if report.exists() else []
        line3 = lines[2] if len(lines) >= 3 else None
        self.add("report.md line 3", "PASS" if line3 == "Outcome: complete" else "FAIL",
                 {"line 3": line3 if line3 is not None else "missing", "report exists": report.exists()},
                 lines[:10])

    def c_gates(self, events, head, **_):
        res = [e for e in events if e.get("kind") == "gate_result"]
        last = {}
        for e in res:
            last[e.get("gate")] = e
        facts = {"run head": head or "missing",
                 "gate results": [(e.get("gate"), (e.get("examined_sha") or "")[:12], e.get("passed")) for e in res] or "none"}
        for g, e in last.items():
            facts[f"{g} last examined_sha == head"] = e.get("examined_sha") == head
            facts[f"{g} passed"] = e.get("passed")
        stale = [g for g, e in last.items() if e.get("examined_sha") != head]
        facts["earlier results on another sha"] = [(e.get("gate"), (e.get("examined_sha") or "")[:12]) for e in res
                                                   if e.get("examined_sha") != head] or "none"
        self.add("every gate examined the run head", "PASS" if last and head and not stale else "FAIL", facts)

    def c_attempts(self, attempts, **_):
        brief = [(a.get("card"), a.get("role"), a.get("vendor"), a.get("model"), a.get("effort"), a.get("status"),
                  a.get("usage_known")) for a in attempts]
        for card, (vendor, model, effort) in CARD_PINS.items():
            mine = [a for a in attempts if a.get("role") == "implementer" and a.get("vendor") == vendor
                    and a.get("model") == model and a.get("effort") == effort and a.get("card") == card]
            self.add(f"{vendor} implementer attempt on {model} {effort}", "PASS" if mine else "FAIL",
                     {"matching attempts": len(mine), "usage_known": [a.get("usage_known") for a in mine],
                      "all attempts (card, role, vendor, model, effort, status, usage_known)": brief})

    def c_usage(self, attempts, charges, **_):
        sup = [c for c in charges if c.get("role") == "supervisor"]
        prox_c = [c for c in charges if c.get("role") == "proxy"]
        prox_a = [a for a in attempts if a.get("role") == "proxy"]
        sup_a = [a for a in attempts if a.get("role") == "supervisor"]
        known = lambda cs: [bool((c.get("usage") or {}).get("known", True)) for c in cs]  # noqa: E731
        costs = [a.get("cost_estimate") for a in attempts if isinstance(a.get("cost_estimate"), (int, float))]
        self.add("supervisor and proxy usage recorded", "PASS" if sup and (prox_a or prox_c) else "FAIL",
                 {"supervisor charges (charges.jsonl)": len(sup), "supervisor charges known": known(sup),
                  "supervisor attempts (attempts.jsonl)": len(sup_a),
                  "proxy attempts (attempts.jsonl)": len(prox_a), "proxy charges": len(prox_c),
                  "sum of attempt cost estimates (USD, priced Claude only)": round(sum(costs), 4),
                  "session cost from stream result (USD)": [s.notes.get("last_result_cost_usd") for s in self.sessions]})

    def c_order(self, events, **_):
        req, started, result = {}, {}, {}
        for e in events:
            did = e.get("dispatch_id")
            if e.get("kind") == "dispatch_requested":
                req.setdefault(did, e["seq"])
            elif e.get("kind") == "worker_started":
                started.setdefault(did, e["seq"])
            elif e.get("kind") == "result_recorded":
                result.setdefault(did, e["seq"])
        bad = [d for d, s in result.items() if d not in started or started[d] >= s]
        never = [d for d in req if d not in result]
        self.add("worker_started before result_recorded", "PASS" if result and not bad else "FAIL",
                 {"dispatches": len(req), "with result": len(result), "out of order or unstarted": bad or "none",
                  "requested but no result": never or "none"})

    def c_workspace(self, meta, sub_recs, **_):
        ws = real(meta.get("workspace") or "")
        facts, ok = {"workspace": meta.get("workspace")}, True
        for path, recs in sub_recs.items():
            cwds = sorted({r.get("cwd") for r in recs if r.get("cwd")})
            mine = all(real(c) == ws for c in cwds) and bool(cwds)
            facts[f"claude {Path(path).name} cwd"] = cwds or "none"
            ok = ok and mine
        wdir = self.run_dir / "workers"
        for d in sorted(wdir.iterdir()) if wdir.is_dir() else []:
            req = read_json(d / "request.json")
            res = read_json(d / "result.json")
            rollout = lh_usage.codex_session_file(res.get("session_id") or "") if res.get("session_id") else None
            rcwd = None
            if rollout:
                first = next(iter(read_jsonl(rollout)), {})
                rcwd = (first.get("payload") or {}).get("cwd")
            facts[f"codex {d.name} request cwd"] = req.get("cwd")
            facts[f"codex {d.name} rollout cwd"] = rcwd or "not found"
            ok = ok and real(req.get("cwd") or "") == ws and (rcwd is None or real(rcwd) == ws)
        if not sub_recs:
            ok = False
            facts["claude worker transcripts"] = "none found"
        self.add("workers ran in the workspace", "PASS" if ok else "FAIL", facts)

    def c_headline(self, events, **_):
        heads = [s["headline"] for s in self.status_samples]
        impl = [h for h in heads if "Implementing" in h]
        rev = [h for h in heads if "Reviewing" in h]
        kinds = [e.get("dispatch_kind") for e in events if e.get("kind") == "dispatch_requested"]
        self.add("headline moved through implementing and reviewing", "PASS" if impl and rev else "FAIL",
                 {"samples": len(heads), "distinct headlines": list(dict.fromkeys(heads))[:30],
                  "dispatch kinds in events (for reference)": kinds})

    def c_publish(self, main_recs, sub_recs, **_):
        hits, n = [], 0
        for path, recs in list(main_recs.items()) + list(sub_recs.items()):
            for u in tool_uses(recs, path):
                if u.name == "Bash":
                    n += 1
                    c = str(u.input.get("command") or "")
                    if PUBLISH_RE.search(c):
                        hits.append(f"{Path(path).name}: {short(c, 200)}")
        codex_n = 0
        wdir = self.run_dir / "workers"
        for d in sorted(wdir.iterdir()) if wdir.is_dir() else []:
            for e in read_jsonl(d / "stream.jsonl"):
                item = e.get("item") or {}
                if e.get("type") == "item.completed" and item.get("type") == "command_execution":
                    codex_n += 1
                    if PUBLISH_RE.search(str(item.get("command") or "")):
                        hits.append(f"codex {d.name}: {short(item.get('command'), 200)}")
        after = self.git("for-each-ref", repo=self.fx["origin"])
        unchanged = after == self.origin_refs_before
        self.add("no publishing command ran", "PASS" if not hits and unchanged and (n or codex_n) else "FAIL",
                 {"claude Bash commands scanned": n, "codex commands scanned": codex_n,
                  "publishing commands": hits or "none", "origin refs unchanged": unchanged,
                  "origin refs after": after.splitlines()})

    def c_elapsed(self, events, **_):
        start = next((e for e in events if e.get("kind") == "run_started"), {})
        end = next((e for e in reversed(events) if e.get("kind") == "outcome"), {})
        t1, t2 = parse_ts(start.get("ts")), parse_ts(end.get("ts"))
        secs = round(t2 - t1) if t1 and t2 else None
        self.add("elapsed time", "OBSERVED" if secs is not None else "FAIL",
                 {"run_started -> outcome (s)": secs, "run_started": start.get("ts"), "outcome": end.get("ts"),
                  "driver wall time (s)": round(time.time() - self.t0),
                  "sessions": len(self.sessions),
                  "session durations (s)": [round((s.exited_at or time.time()) - s.started_at) for s in self.sessions]})

    def c_print_wait(self, **_):
        facts = {"mode": self.mode}
        early = []
        for s in self.sessions:
            keep = ("how", "exit_code")
            facts[f"session {s.n}"] = {**{k: getattr(s, k) for k in keep},
                                       "started": s.started_utc, "exited": s.exited_utc, **s.notes}
            if s.notes.get("exited_while_workers_running"):
                early.append(s.n)
        facts["sessions that exited while workers ran (relaunched)"] = early or "none"
        waited = [s.notes.get("longest_wait_after_a_result_s") or 0 for s in self.sessions]
        facts["longest quiet wait after a result (s), per session"] = waited
        facts[f"print mode waited >= {PRINT_WAIT_NOTE_S:.0f}s for a worker"] = any(w >= PRINT_WAIT_NOTE_S for w in waited)
        self.add("open check: print mode outlives long workers", "OBSERVED", facts)

    def c_sandbox(self, main_recs, sub_recs, **_):
        cli = str(CLI)
        rows, heredoc, plain_violations = [], [], []
        for path, recs in list(main_recs.items()) + list(sub_recs.items()):
            results = tool_results(recs)
            for u in tool_uses(recs, path):
                if u.name != "Bash":
                    continue
                c = str(u.input.get("command") or "")
                if cli not in c:
                    continue
                r = results.get(u.id)
                row = {"file": Path(path).name, "command": short(c, 140),
                       "sandboxed": r.sandboxed if r else "no result",
                       "disable_sandbox": bool(u.input.get("dangerouslyDisableSandbox")),
                       "background": bool(u.input.get("run_in_background"))}
                stripped = c.lstrip()
                if not (stripped.startswith(cli) or stripped.startswith(shlex.quote(cli))):
                    plain_violations.append(row["command"])
                rows.append(row)
                if "<<" in c and "record" in c:
                    heredoc.append(row)
        sandboxed = [r for r in rows if r["sandboxed"] is True]
        unknown = [r for r in rows if not isinstance(r["sandboxed"], bool)]
        flagged = [r for r in rows if isinstance(r["sandboxed"], bool)]
        self.add("open check: CLI calls ran unsandboxed (exclusion matched)",
                 "PASS" if flagged and not sandboxed else ("FAIL" if sandboxed else "OBSERVED"),
                 {"cli calls": len(rows), "with a sandboxed flag": len(flagged),
                  "sandboxed": [r["command"] for r in sandboxed] or "none",
                  "no sandboxed flag (e.g. background calls)": [r["command"] for r in unknown] or "none",
                  "dangerouslyDisableSandbox set": sum(r["disable_sandbox"] for r in rows),
                  "calls not starting with the CLI path": plain_violations or "none",
                  "first calls": [(r["command"], r["sandboxed"]) for r in rows[:8]]})
        self.add("open check: heredoc record command matched the exclusion",
                 "PASS" if heredoc and all(r["sandboxed"] is False for r in heredoc) else ("FAIL" if heredoc else "OBSERVED"),
                 {"heredoc record calls": len(heredoc),
                  "sandboxed flags": [r["sandboxed"] for r in heredoc] or "none",
                  "examples": [r["command"] for r in heredoc[:3]] or "none"})

    def c_agent_models(self, main_recs, sub_recs, **_):
        calls = []
        sub_models = {}
        for path, recs in sub_recs.items():
            aid = Path(path).stem.removeprefix("agent-")
            sub_models[aid] = sorted({(r.get("message") or {}).get("model") for r in recs
                                      if r.get("type") == "assistant" and (r.get("message") or {}).get("model")})
        for path, recs in main_recs.items():
            results = tool_results(recs)
            for u in tool_uses(recs, path):
                if u.name not in ("Agent", "Task"):
                    continue
                r = results.get(u.id)
                m = re.search(r"agentId:\s*([A-Za-z0-9]+)", r.text if r else "")
                aid = m.group(1) if m else None
                calls.append({"subagent_type": u.input.get("subagent_type"), "model": u.input.get("model"),
                              "effort": u.input.get("effort"), "error": bool(r and r.is_error),
                              "result": short(r.text if r else "no result", 120), "agent_id": aid,
                              "actual_models": sub_models.get(aid or "", "transcript not found")})
        errors = [c for c in calls if c["error"]]
        self.add("open check: Agent tool accepted the route model names",
                 "PASS" if calls and not errors else ("FAIL" if calls else "OBSERVED"),
                 {"agent calls": len(calls), "rejected": errors or "none", "calls": calls})

    def c_skill(self, main_recs, **_):
        body = SKILL_FILE.read_text(encoding="utf-8").split("---", 2)[-1]
        para = next((p.strip() for p in body.split("\n\n") if p.strip() and not p.strip().startswith("#")), "")
        marker = " ".join(para.split())[:50]
        calls, loaded = [], False
        for path, recs in main_recs.items():
            results = tool_results(recs)
            for u in tool_uses(recs, path):
                if u.name != "Skill":
                    continue
                r = results.get(u.id)
                calls.append({"input": u.input, "error": bool(r and r.is_error), "result": short(r.text if r else "none", 120)})
            loaded = loaded or any(marker in " ".join(strings(r).split()) for r in recs)
        wanted = [c for c in calls if "leadhand:run" in json.dumps(c["input"])]
        ok = bool(wanted) and not any(c["error"] for c in wanted) and bool(loaded)
        self.add("open check: leadhand:run loaded through the Skill tool", "PASS" if ok else "FAIL",
                 {"skill calls": calls or "none", "body marker": marker, "body marker found in transcript": bool(loaded)})

    # -------------------------------------------------------------- output

    def write_raw(self, started: str, push_summary: list[str] | None) -> None:
        out = ["# Task 20: first complete run, end to end — raw results", "",
               f"- started: {started}", f"- finished: {utc_now()}", f"- mode: {self.mode}",
               f"- budget: {self.budget_s // 60} min", f"- stop reason: {self.stop_reason or 'n/a'}",
               f"- temp dir: {self.tmp} ({'kept' if self.keep else 'removed'})",
               f"- plugin dir: {ROOT}", f"- cli: {CLI}", ""]
        if push_summary is not None:
            out += ["## Step 1: Codex push probe", "", "See push-probe-raw.md.", ""]
            out += [f"- {s}" for s in push_summary] + [""]
        out += ["## Setup facts", ""]
        out += [f"- {k}: {v if isinstance(v, str) else json.dumps(v, default=str)}" for k, v in self.facts.items()]
        out += ["", "## Commands", "", "```"]
        out += [f"(exit {code}) (cwd={cwd}) {argv}" for argv, code, cwd in self.commands]
        out += ["```", ""]
        if self.errors:
            out += ["## Driver errors", ""] + [f"- {e}" for e in self.errors] + [""]
        out += ["## Checks", ""]
        for c in self.checks:
            out += [f"### {c.name} — {c.verdict}", "", f"How: {c.how}", ""]
            if c.excerpt:
                out += ["```"] + c.excerpt[:10] + ["```", ""]
            out += [f"- {k}: {v if isinstance(v, str) else json.dumps(v, default=str)}" for k, v in c.facts.items()]
            out.append("")
        out += ["## Summary", ""] + [f"- {c.verdict:8} {c.name}" for c in self.checks] + [""]
        self.raw.write_text("\n".join(out), encoding="utf-8")

    def run(self) -> int:
        self.out.mkdir(parents=True, exist_ok=True)
        self.art.mkdir(parents=True, exist_ok=True)
        started = utc_now()
        push_summary = None
        try:
            if not self.skip_push_probe:
                self.log("Step 1: Codex push probe")
                try:
                    push_summary = codex_push_probe.run_probe(self.out)
                except Exception:
                    push_summary = ["FAIL     push probe crashed: " + traceback.format_exc().splitlines()[-1]]
            self.log("Step 2: end-to-end run")
            self.t0 = time.time()
            if self.preflight() and self.setup():
                try:
                    self.drive()
                except Exception:
                    self.errors.append("drive: " + traceback.format_exc())
                    self.stop_reason = self.stop_reason or "driver error"
                finally:
                    self.stop_everything()
                for s in self.sessions:     # again, now that every later result is known
                    self.analyze_exit(s, self.events())
                for phase in (self.finish_run, self.collect, self.run_checks):
                    try:
                        phase()
                    except Exception:
                        self.errors.append(f"{phase.__name__}: " + traceback.format_exc())
        except KeyboardInterrupt:
            self.errors.append("interrupted by the operator (Ctrl-C)")
            self.stop_reason = self.stop_reason or "operator interrupt"
            self.stop_everything()
        finally:
            self.write_raw(started, push_summary)
            if self.tmp and not self.keep:
                shutil.rmtree(self.tmp, ignore_errors=True)
        print("\nSummary:")
        for s in push_summary or []:
            print(f"  push probe: {s}")
        for c in self.checks:
            print(f"  {c.verdict:8} {c.name}")
        print(f"raw results: {self.raw}")
        return 0


CHECK_DOCS = [
    ("outcome complete", "the last `outcome` event in events.jsonl has outcome `complete`"),
    ("run branch holds both cards' commits", "each card's `accepted` sha is an ancestor of refs/heads/<run branch> "
     "(git merge-base --is-ancestor); also lists base..branch commits and runs the fixture tests in the workspace"),
    ("report.md line 3", "line 3 of <run>/report.md is exactly `Outcome: complete`"),
    ("every gate examined the run head", "for each gate, its last `gate_result` event's examined_sha equals "
     "`git rev-parse refs/heads/<run branch>`; earlier results on other shas are listed"),
    ("claude implementer attempt on sonnet low", "attempts.jsonl has card 1, role implementer, vendor claude, "
     "model sonnet, effort low"),
    ("codex implementer attempt on gpt-6-luna medium", "attempts.jsonl has card 2, role implementer, vendor codex, "
     "model gpt-6-luna, effort medium"),
    ("supervisor and proxy usage recorded", "charges.jsonl has role `supervisor` charges; attempts.jsonl (or "
     "charges.jsonl) has role `proxy`"),
    ("worker_started before result_recorded", "every dispatch with a `result_recorded` event has a "
     "`worker_started` event with a smaller seq"),
    ("workers ran in the workspace", "every Claude worker transcript record's cwd, every Codex request.json cwd and "
     "rollout session_meta cwd resolve to run.json's workspace"),
    ("headline moved through implementing and reviewing", "status.json sampled every 5 s; some headline contains "
     "`Implementing` and some contains `Reviewing`"),
    ("no publishing command ran", "no Bash command in the supervisor or worker transcripts and no Codex "
     "command_execution matches git+push / gh pr; the fixture's bare origin refs are unchanged"),
    ("elapsed time", "run_started -> outcome event timestamps; driver wall time; per-session durations"),
    ("open check: print mode outlives long workers", "per session: exit time vs worker_started/result_recorded; "
     "dispatches open at exit; Codex results written after exit; quiet gaps after each stream `result`"),
    ("open check: CLI calls ran unsandboxed (exclusion matched)", "every Bash call containing the CLI path: its "
     "tool_result record's `sandboxed` flag is false (supervisor and worker transcripts)"),
    ("open check: heredoc record command matched the exclusion", "the `record ... --reply - <<'LEADHAND_REPLY'` "
     "Bash calls: `sandboxed` is false"),
    ("open check: Agent tool accepted the route model names", "every Agent call (subagent_type, model, effort) got "
     "a non-error result; the worker transcript's message.model is listed"),
    ("open check: leadhand:run loaded through the Skill tool", "a Skill tool call naming leadhand:run without "
     "error, and the first paragraph of skills/run/SKILL.md appears in the session transcript"),
]


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    ap.add_argument("--out", required=True, help="directory for e2e-raw.md, e2e-artifacts/ and the push probe")
    ap.add_argument("--mode", choices=("headless", "tmux"), default="headless")
    ap.add_argument("--budget-minutes", type=int, default=BUDGET_MINUTES)
    ap.add_argument("--skip-push-probe", action="store_true", help="do not run codex_push_probe.py first")
    ap.add_argument("--keep", action="store_true", help="keep the temp fixture folder")
    ap.add_argument("--dry-run", action="store_true", help="print the plan and exit; runs nothing")
    args = ap.parse_args(argv)
    if args.budget_minutes < 1:
        ap.error("--budget-minutes must be at least 1")
    d = Driver(args)
    if args.dry_run:
        d.plan()
        return 0
    codex_push_probe.refuse_inside_claude_code()
    return d.run()


if __name__ == "__main__":
    raise SystemExit(main())
