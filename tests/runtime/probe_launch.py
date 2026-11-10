#!/usr/bin/env python3
"""Runtime probe for Task 2: supervisor launch and hook identity.

Runs every step of Task 2 against the real `claude` and `tmux` CLIs and writes
the observed facts to DIR/task2-raw.md, with full artifacts under
DIR/task2-artifacts/. Run it in an ordinary terminal, not inside Claude Code:

    python3 tests/runtime/probe_launch.py --out DIR [--only 1,4] [--dry-run]

Standard library only. Every step runs independently; failures are recorded,
not raised. Steps 1, 2 and 4 share the foreground `lhprobe:super` session.
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
import threading
import time
import traceback
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
PLUGIN_DIR = HERE / "probe-plugin"
NET_PROBE = HERE / "net-probe.sh"
PROBE_HOME = Path.home() / ".leadhand-probe"
CLAUDE_PROJECTS = Path.home() / ".claude" / "projects"
USER_SETTINGS = Path.home() / ".claude" / "settings.json"

RUN_ID = "probe"
SKILL_TOKEN = "lhprobe-skill-7f3a9c"  # echoed by super when the skill is in context
CONTEXT_MARKER = "lhprobe-ctx-41d2"  # appears only in the skill text, never echoed
AGENT_TOOLS = {"Agent", "Task"}

HEADLESS_TIMEOUT = 300
BACKGROUND_GRACE = 30
INTERACTIVE_TIMEOUT = 240
DIALOG_TIMEOUT = 60

STEPS = {
    1: "Headless launch",
    2: "Skill preload",
    3: "Interactive launch",
    4: "Hook identity",
    5: "Background dispatch in print mode",
    6: "Settings overlay",
}

PERMISSION_ROUTES = {
    "allowed-tools": ["--allowedTools", "Bash,Agent"],
    "auto": ["--permission-mode", "auto"],
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


# ---------------------------------------------------------------- context


class Ctx:
    def __init__(self, out: Path, route: str, dry: bool):
        self.out = out
        self.art = out / "task2-artifacts"
        self.raw = out / "task2-raw.md"
        self.route = route
        self.perm = PERMISSION_ROUTES[route]
        self.dry = dry
        self.tag = f"{os.getpid()}"
        self.tmp: Path | None = None
        self.fixtures: dict[str, Path] = {}
        self.pgids: list[int] = []
        self.tmux_sockets: list[str] = []
        self.markers: list[Path] = []
        self.cache: dict = {}

    def fixture_path(self, name: str = "fixture repo") -> str:
        """A temp Git repo per name (steps 3 and 6 get their own), all under one temp dir."""
        if self.dry:
            return f"<tmp>/lhprobe XXXX/{name}"
        if name not in self.fixtures:
            if self.tmp is None:
                self.tmp = Path(tempfile.mkdtemp(prefix="lhprobe ")).resolve()
            self.fixtures[name] = make_fixture(self.tmp / name)
        return str(self.fixtures[name])


GIT_TIMEOUT = 60


def make_fixture(repo: Path) -> Path:
    repo.mkdir()
    (repo / "README.md").write_text("probe fixture\n")
    # Isolate from the operator's git config: no signing (pinentry), no hooks.
    git = ["git", "-C", str(repo), "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
           "-c", "user.name=probe", "-c", "user.email=probe@example.invalid"]
    for args in (["init", "-q", "-b", "main"], ["add", "README.md"],
                 ["commit", "-q", "--no-gpg-sign", "-m", "init"]):
        subprocess.run(git + args, check=True, timeout=GIT_TIMEOUT,
                       stdin=subprocess.DEVNULL, capture_output=True)
    return repo


def child_env(hook_log: Path | str, extra: dict | None = None) -> dict:
    env = dict(os.environ)
    env.pop("CLAUDECODE", None)
    env.pop("PROBE_DENY_UNSANDBOXED", None)
    env["LEADHAND_RUN_ID"] = RUN_ID
    env["PROBE_LOG"] = str(hook_log)
    env.update(extra or {})
    return env


# ---------------------------------------------------------------- argv builders


def headless_argv(ctx: Ctx, prompt: str, agent: str | None = "lhprobe:super",
                  extra: list[str] | None = None) -> list[str]:
    # The prompt sits right after -p so the variadic --allowedTools can't swallow it.
    argv = ["claude", "-p", prompt, "--plugin-dir", str(PLUGIN_DIR)]
    if agent:
        argv += ["--agent", agent]
    argv += list(extra or [])
    argv += ["--output-format", "stream-json", "--verbose"]
    return argv + ctx.perm


def tmux_argv(sock: str, name: str, cwd: str, hook_log: str) -> list[str]:
    # remain-on-exit keeps the pane (and its last screen) if claude exits early.
    return [
        "tmux", "-L", sock, "start-server", ";",
        "set-option", "-g", "remain-on-exit", "on", ";",
        "new-session", "-d", "-s", name, "-c", cwd,
        "-x", "200", "-y", "50",
        "-e", f"LEADHAND_RUN_ID={RUN_ID}", "-e", f"PROBE_LOG={hook_log}",
        "env", "-u", "CLAUDECODE",
        "claude", "--plugin-dir", str(PLUGIN_DIR), "--agent", "lhprobe:super",
        "--permission-mode", "auto",
    ]


# Step 6 only: log-input.sh denies any Bash call that sets dangerouslyDisableSandbox,
# so a model retry can't make a marker appear for the wrong reason.
STEP6_ENV = {"PROBE_DENY_UNSANDBOXED": "1"}


def overlay_json() -> str:
    return json.dumps({"sandbox": {"excludedCommands": [f"{NET_PROBE} *"]}})


def user_sandbox() -> dict:
    try:
        return json.loads(USER_SETTINGS.read_text()).get("sandbox", {}) or {}
    except (OSError, ValueError):
        return {}


def step6_commands(variant: str, tag: str) -> dict:
    """The shell commands the step-6 session is asked to run, keyed by purpose."""
    cmds = {
        "net_probe": f"{shlex.quote(str(NET_PROBE))} {variant}-{tag}",
        "plain_write": f"echo plain > {shlex.quote(str(PROBE_HOME / f'plain-{variant}-{tag}.txt'))}",
    }
    excl = user_sandbox().get("excludedCommands", []) or []
    if any(str(p).startswith("gh ") for p in excl):
        cmds["user_exclusion_gh"] = (
            f"gh api zen > {shlex.quote(str(PROBE_HOME / f'gh-{variant}-{tag}.txt'))}"
        )
    return cmds


def step6_prompt(cmds: dict) -> str:
    lines = [
        "Run each of these shell commands with the Bash tool, exactly as written, "
        "one Bash call per command, in order. Never set dangerouslyDisableSandbox, "
        "never retry, never change a command. Then reply with each command's output.",
    ]
    lines += [f"{i}. {c}" for i, c in enumerate(cmds.values(), 1)]
    return "\n".join(lines)


def step6_argv(ctx: Ctx, variant: str) -> list[str]:
    extra = ["--model", "haiku", "--effort", "low"]
    if variant == "overlay":
        extra += ["--settings", overlay_json()]
    return headless_argv(ctx, step6_prompt(step6_commands(variant, ctx.tag)),
                         agent=None, extra=extra)


# ---------------------------------------------------------------- running claude


def read_jsonl(path: Path) -> list[dict]:
    out = []
    try:
        for line in path.read_text().splitlines():
            try:
                out.append(json.loads(line))
            except ValueError:
                out.append({"unparsed": line})
    except OSError:
        pass
    return out


def kill_group(pgid: int) -> None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError):
            return
        time.sleep(1)


def copy_transcripts(session_id: str | None, dest: Path) -> list[str]:
    if not session_id:
        return []
    found = []
    pats = [f"*/{session_id}.jsonl", f"*/{session_id}/subagents/*.jsonl"]
    for pat in pats:
        for p in sorted(CLAUDE_PROJECTS.glob(pat)):
            dest.mkdir(parents=True, exist_ok=True)
            shutil.copy2(p, dest / p.name)
            found.append(str(p))
    return found


def run_headless(ctx: Ctx, label: str, argv: list[str], grace: int = 0,
                 fixture: str = "fixture repo", env_extra: dict | None = None) -> dict:
    """Run one `claude -p` session, timestamping each stream-json line."""
    hook_log = ctx.art / f"{label}-hooks.jsonl"
    stream_path = ctx.art / f"{label}-stream.jsonl"
    stderr_path = ctx.art / f"{label}-stderr.txt"
    records: list[tuple[float, dict]] = []
    start = time.monotonic()
    with open(stream_path, "w") as stream_f, open(stderr_path, "w") as err_f:
        proc = subprocess.Popen(
            argv, cwd=ctx.fixture_path(fixture), env=child_env(hook_log, env_extra),
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=err_f,
            text=True, start_new_session=True,
        )
        ctx.pgids.append(proc.pid)

        def reader():
            for line in proc.stdout:
                t = round(time.monotonic() - start, 2)
                stream_f.write(line)
                stream_f.flush()
                try:
                    records.append((t, json.loads(line)))
                except ValueError:
                    records.append((t, {"unparsed": line.rstrip("\n")}))

        th = threading.Thread(target=reader, daemon=True)
        th.start()
        timed_out = False
        try:
            proc.wait(timeout=HEADLESS_TIMEOUT)
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_group(proc.pid)
            proc.wait()
        exit_s = round(time.monotonic() - start, 2)
        th.join(timeout=10)
        hooks_at_exit = len(read_jsonl(hook_log))
        if grace and not timed_out:
            time.sleep(grace)
        kill_group(proc.pid)
    init = next((r for _, r in records
                 if r.get("type") == "system" and r.get("subtype") == "init"), {})
    session_id = init.get("session_id") or next(
        (r.get("session_id") for _, r in records if r.get("session_id")), None)
    transcripts = copy_transcripts(session_id, ctx.art / f"{label}-transcripts")
    return {
        "label": label,
        "argv": argv,
        "env_extra": env_extra or {},
        "exit_code": "timeout" if timed_out else proc.returncode,
        "exit_s": exit_s,
        "records": records,
        "init": init,
        "stderr": stderr_path.read_text(errors="replace"),
        "hooks": read_jsonl(hook_log),
        "hooks_at_exit": hooks_at_exit,
        "session_id": session_id,
        "transcripts": transcripts,
        "transcript_records": [r for p in sorted((ctx.art / f"{label}-transcripts").glob("*.jsonl"))
                               for r in read_jsonl(p)],
    }


def cached_headless(ctx: Ctx, agent: str, prompt: str, grace: int = 0) -> dict:
    key = (agent, prompt)
    if key not in ctx.cache:
        label = f"headless-{agent.split(':')[-1]}-{prompt}"
        ctx.cache[key] = run_headless(ctx, label, headless_argv(ctx, prompt, agent), grace)
    return ctx.cache[key]


# ---------------------------------------------------------------- stream helpers


def content_items(rec: dict) -> list:
    msg = rec.get("message")
    if isinstance(msg, dict) and isinstance(msg.get("content"), list):
        return msg["content"]
    return []


def tool_uses(records) -> list[dict]:
    """tool_use blocks from assistant records, with parent and receive time."""
    out = []
    for t, r in records:
        if r.get("type") != "assistant":
            continue
        for c in content_items(r):
            if isinstance(c, dict) and c.get("type") == "tool_use":
                out.append({"t": t, "parent": r.get("parent_tool_use_id"), "id": c.get("id"),
                            "name": c.get("name"), "input": c.get("input") or {}})
    return out


def text_of(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(text_of(c.get("text", c.get("content", ""))) if isinstance(c, dict)
                         else str(c) for c in content)
    return str(content)


def tool_results(records) -> dict:
    """tool_use_id -> (receive time, result text)."""
    out = {}
    for t, r in records:
        if r.get("type") != "user":
            continue
        for c in content_items(r):
            if isinstance(c, dict) and c.get("type") == "tool_result":
                out[c.get("tool_use_id")] = (t, text_of(c.get("content", "")))
    return out


def assistant_models(records) -> dict:
    """message.model values grouped by the record's parent_tool_use_id (as recorded)."""
    models: dict[str, set] = {}
    for _, r in records:
        if r.get("type") == "assistant" and isinstance(r.get("message"), dict):
            key = f"parent={r.get('parent_tool_use_id')}"
            if r["message"].get("model"):
                models.setdefault(key, set()).add(r["message"]["model"])
    return {k: sorted(v) for k, v in models.items()}


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


def effort_fields(result: dict) -> list[str]:
    pat = re.compile(r"effort", re.I)
    seen = set()
    for src, recs in (("stream", [r for _, r in result["records"]]),
                      ("transcript", result["transcript_records"])):
        for r in recs:
            for p, v in find_keys(r, pat):
                seen.add(f"{src}:{r.get('type')}:{p}={v}")
    return sorted(seen)[:20]


def command_role(cmd: str | None) -> str | None:
    """Who ran a Bash command, from the commands the probe agents are told to run.

    Classifying by command, not by agent_id, keeps Step 4's subject (agent_id)
    out of the classification it is compared against.
    """
    cmd = (cmd or "").strip()
    if cmd.startswith("echo probe"):
        return "main"
    if cmd == "pwd" or cmd.startswith("pwd ") or cmd.startswith("pwd;"):
        return "worker"
    return None


def hook_command(h: dict) -> str:
    return str(((h.get("hook_input") or {}).get("tool_input") or {}).get("command", ""))


def split_hooks(hooks: list[dict]) -> tuple[list[dict], list[dict]]:
    """(main, worker) hook lines, classified by command."""
    main = [h for h in hooks if command_role(hook_command(h)) == "main"]
    sub = [h for h in hooks if command_role(hook_command(h)) == "worker"]
    return main, sub


def classification_mismatches(hooks: list[dict]) -> list[str]:
    """Lines where command-based role disagrees with agent_id presence."""
    out = []
    for h in hooks:
        role = command_role(hook_command(h))
        has_id = bool((h.get("hook_input") or {}).get("agent_id"))
        if role and (role == "worker") != has_id:
            out.append(f"{role} command {hook_command(h)!r} has agent_id={has_id}")
    return out


def bash_by_role(records, role: str) -> list[dict]:
    """Bash tool_use blocks from the stream whose command matches `role`."""
    return [u for u in tool_uses(records)
            if u["name"] == "Bash" and command_role(u["input"].get("command")) == role]


def result_excerpt(result: dict) -> list[str]:
    lines = []
    for _, r in result["records"]:
        if r.get("type") == "result":
            lines.append(f"[result] subtype={r.get('subtype')} is_error={r.get('is_error')} "
                         f"num_turns={r.get('num_turns')} cost_usd={r.get('total_cost_usd')}")
            lines += str(r.get("result", "")).splitlines()[:4]
    lines += [f"[stderr] {l}" for l in result["stderr"].splitlines()[:4]]
    return lines[:10]


def common_observed(result: dict) -> dict:
    return {
        "exit_code": result["exit_code"],
        "exit_after_s": result["exit_s"],
        "session_id": result["session_id"],
        "transcripts_found": result["transcripts"] or "none",
        "stderr_first_line": (result["stderr"].splitlines() or [""])[0],
    }


# ---------------------------------------------------------------- steps
# Each step returns (status, commands[(argv, exit)], excerpt lines, observed dict, summary).


FALLBACK_AGENT = "leadhand-supervisor"


def fallback_agents_json() -> str:
    """The brief's fallback: super's definition passed inline with --agents."""
    text = (PLUGIN_DIR / "agents" / "super.md").read_text()
    body = text.split("\n---\n", 1)[1].strip() if text.startswith("---\n") else text
    return json.dumps({FALLBACK_AGENT: {
        "description": "Probe supervisor (inline --agents fallback for leadhand runtime checks).",
        "prompt": body,
        "model": "haiku",
    }})


def fallback_argv(ctx: Ctx) -> list[str]:
    return headless_argv(ctx, "go", agent=FALLBACK_AGENT,
                         extra=["--agents", fallback_agents_json()])


def launch_facts(ctx: Ctx, r: dict) -> dict:
    """Step 1's observations for one headless foreground session."""
    uses = tool_uses(r["records"])
    results = tool_results(r["records"])
    main_bash = bash_by_role(r["records"], "main")
    worker_bash = bash_by_role(r["records"], "worker")
    dispatch = [u for u in uses if u["name"] in AGENT_TOOLS]
    main_hooks, worker_hooks = split_hooks(r["hooks"])
    launch_cwd = os.path.realpath(ctx.fixture_path())
    worker_hook_cwd = worker_hooks[0]["hook_input"].get("cwd") if worker_hooks else None
    worker_pwd_out = [results.get(u["id"], (None, None))[1] for u in worker_bash]
    cwd_candidates = [c for c in [worker_hook_cwd] + [(o or "").strip() for o in worker_pwd_out] if c]
    init = r["init"]
    obs = common_observed(r)
    obs.update({
        "init_keys": sorted(init.keys()),
        "init_agent": init.get("agent", "absent"),
        "init_model": init.get("model"),
        "init_permissionMode": init.get("permissionMode"),
        "init_agents_lhprobe": [a for a in init.get("agents", []) or []
                                if "lhprobe" in str(a) or FALLBACK_AGENT in str(a)],
        "all_bash_commands": [u["input"].get("command") for u in uses if u["name"] == "Bash"],
        "main_ran_probe_echo": bool(main_bash) or bool(main_hooks),
        "main_bash_parent_ids": [u["parent"] for u in main_bash],
        "agent_dispatch": [{"id": u["id"], "parent": u["parent"], "input": u["input"]}
                           for u in dispatch],
        "assistant_models_by_parent": assistant_models(r["records"]),
        "effort_fields": effort_fields(r) or "none found",
        "worker_bash_parent_ids": [u["parent"] for u in worker_bash],
        "worker_bash_parent_is_dispatch": bool(worker_bash) and all(
            u["parent"] in {d["id"] for d in dispatch} for u in worker_bash),
        "worker_pwd_output": worker_pwd_out,
        "worker_hook_cwd": worker_hook_cwd,
        "worker_hook_pwd": worker_hooks[0].get("pwd") if worker_hooks else None,
        "launch_cwd": launch_cwd,
        "worker_cwd_matches_launch": bool(cwd_candidates) and all(
            os.path.realpath(c) == launch_cwd for c in cwd_candidates),
        "hook_classification_mismatches": classification_mismatches(r["hooks"]) or "none",
        "permission_route": shlex.join(ctx.perm),
    })
    return obs


def step1(ctx: Ctx):
    r = cached_headless(ctx, "lhprobe:super", "go")
    obs = launch_facts(ctx, r)
    commands = [(r["argv"], r["exit_code"])]
    excerpt = result_excerpt(r)
    summary = (f"exit={r['exit_code']} probe_echo={obs['main_ran_probe_echo']} "
               f"models={obs['assistant_models_by_parent']} "
               f"worker_cwd_ok={obs['worker_cwd_matches_launch']}")
    obs["fallback_run"] = "no (plugin-scoped --agent ran the probe echo)"
    if not obs["main_ran_probe_echo"]:
        fb = run_headless(ctx, "headless-fallback-agents", fallback_argv(ctx))
        commands.append((fb["argv"], fb["exit_code"]))
        excerpt = (["[plugin-scoped] " + l for l in excerpt[:5]]
                   + ["[--agents fallback] " + l for l in result_excerpt(fb)[:5]])
        obs["fallback_run"] = f"yes: --agents JSON defining {FALLBACK_AGENT}"
        for k, v in launch_facts(ctx, fb).items():
            obs[f"fallback {k}"] = v
        summary += (f"; fallback {FALLBACK_AGENT}: exit={fb['exit_code']} "
                    f"probe_echo={obs['fallback main_ran_probe_echo']}")
    ok = r["exit_code"] == 0 and obs["main_ran_probe_echo"] and obs["worker_cwd_matches_launch"]
    return ("PASS" if ok else "OBSERVED", commands, excerpt, obs, summary)


def skill_facts(r: dict) -> dict:
    uses = tool_uses(r["records"])
    main_bash = [u["input"].get("command") or "" for u in bash_by_role(r["records"], "main")]
    main_bash += [hook_command(h) for h in split_hooks(r["hooks"])[0] if not main_bash]
    non_assistant = [json.dumps(x) for _, x in r["records"] if x.get("type") != "assistant"]
    non_assistant += [json.dumps(x) for x in r["transcript_records"] if x.get("type") != "assistant"]
    init_skills = r["init"].get("skills", "absent")
    return {
        "exit_code": r["exit_code"],
        "main_bash_commands": main_bash,
        "skill_token_echoed": any(SKILL_TOKEN in c for c in main_bash),
        "no_skill_echoed": any("no-skill" in c for c in main_bash),
        "skill_text_in_context": any(CONTEXT_MARKER in s for s in non_assistant),
        "skill_tool_used": [u["input"] for u in uses if u["name"] == "Skill"] or "no",
        "init_skills_probe": ([s for s in init_skills if "probe" in str(s)]
                              if isinstance(init_skills, list) else init_skills),
        "session_id": r["session_id"],
    }


def step2(ctx: Ctx):
    bare = cached_headless(ctx, "lhprobe:super", "go")
    ns = cached_headless(ctx, "lhprobe:super-ns", "go")
    obs = {}
    for name, r in (("bare `skills: [probe]` (lhprobe:super)", bare),
                    ("qualified `skills: [lhprobe:probe]` (lhprobe:super-ns)", ns)):
        for k, v in skill_facts(r).items():
            obs[f"{name} {k}"] = v
    excerpt = ["[bare] " + l for l in result_excerpt(bare)[:5]] + \
              ["[qualified] " + l for l in result_excerpt(ns)[:5]]
    b, q = skill_facts(bare), skill_facts(ns)
    summary = (f"bare: token={b['skill_token_echoed']} in_context={b['skill_text_in_context']}; "
               f"qualified: token={q['skill_token_echoed']} in_context={q['skill_text_in_context']}")
    return ("OBSERVED", [(bare["argv"], bare["exit_code"]), (ns["argv"], ns["exit_code"])],
            excerpt, obs, summary)


def tmux(sock: str, *args: str, check: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(["tmux", "-L", sock, *args], capture_output=True, text=True,
                          timeout=30, check=check)


DIALOG_REPEAT_LIMIT = 3  # the same dialog text this many times = stale, stop answering it


def step3(ctx: Ctx):
    sock = name = f"lhprobe-{ctx.tag}"
    hook_log = ctx.art / "interactive-hooks.jsonl"
    argv = tmux_argv(sock, name, ctx.fixture_path("interactive repo"), str(hook_log))
    ctx.tmux_sockets.append(sock)
    commands = [(argv, None)]
    proc = subprocess.run(argv, capture_output=True, text=True, timeout=30)
    commands[0] = (argv, proc.returncode)
    if proc.returncode != 0:
        return ("FAIL", commands, proc.stderr.splitlines()[:10], {"tmux_stderr": proc.stderr}, "tmux failed")
    capture_errors: list[str] = []

    def capture() -> str:
        p = tmux(sock, "capture-pane", "-p", "-t", name)
        if p.returncode != 0:
            capture_errors.append(f"rc={p.returncode} {p.stderr.strip()}")
        return p.stdout

    def pane_state() -> str:
        p = tmux(sock, "display-message", "-p", "-t", name, "dead=#{pane_dead} status=#{pane_dead_status}")
        return p.stdout.strip() if p.returncode == 0 else f"rc={p.returncode} {p.stderr.strip()}"

    selections: list[dict] = []

    def answer_dialog(pane: str, answered: list) -> bool:
        """Press Enter on a known dialog; False once the same dialog has gone stale."""
        for pat in DIALOG_PATTERNS:
            if re.search(pat, pane):
                if answered.count(pat) >= DIALOG_REPEAT_LIMIT:
                    return False
                answered.append(pat)
                ok, hl, keys = select_menu_option(
                    capture, lambda k: tmux(sock, "send-keys", "-t", name, k))
                selections.append({"dialog": pat, "highlighted_at_enter": hl if ok else None,
                                   "last_highlighted": hl, "keys": keys, "ok": ok})
                if not ok:  # could not highlight the wanted option: stop retrying this dialog
                    answered.extend([pat] * DIALOG_REPEAT_LIMIT)
                    return False
                time.sleep(2)
                return True
        return False

    start = time.monotonic()
    answered: list[str] = []
    pane = ""
    ready_s = None
    while time.monotonic() - start < DIALOG_TIMEOUT + 30:
        pane = capture()
        if READY_PATTERN.search(pane):
            ready_s = round(time.monotonic() - start, 1)
            break
        if "dead=1" in pane_state():
            break
        if not answer_dialog(pane, answered):
            if any(answered.count(p) >= DIALOG_REPEAT_LIMIT for p in DIALOG_PATTERNS
                   if re.search(p, pane)):
                break  # stale dialog: answering it again won't help
            time.sleep(2)
    (ctx.art / "interactive-pane-startup.txt").write_text(pane)
    obs = {"dialogs_answered": answered or "none", "dialog_selections": selections or "none",
           "pane_state": pane_state(),
           "capture_errors": capture_errors or "none"}
    if ready_s is None:
        obs["pane_last_lines"] = pane.rstrip().splitlines()[-10:]
        return ("FAIL", commands, pane.rstrip().splitlines()[-10:], obs,
                f"never reached the prompt ({obs['pane_state']}; see interactive-pane-startup.txt)")
    obs["ready_after_s"] = ready_s
    send = [["tmux", "-L", sock, "send-keys", "-t", name, "-l", "go"],
            ["tmux", "-L", sock, "send-keys", "-t", name, "Enter"]]
    for a in send:
        p = subprocess.run(a, capture_output=True, timeout=30)
        commands.append((a, p.returncode))
        time.sleep(0.5)
    sent = time.monotonic()
    last_pane, stable_since, worker_seen_s = "", time.monotonic(), None
    while time.monotonic() - sent < INTERACTIVE_TIMEOUT:
        time.sleep(2)
        pane = capture()
        if "dead=1" in pane_state():
            break
        if answer_dialog(pane, answered):
            continue
        if pane != last_pane:
            last_pane, stable_since = pane, time.monotonic()
        _, sub = split_hooks(read_jsonl(hook_log))
        if sub and worker_seen_s is None:
            worker_seen_s = round(time.monotonic() - sent, 1)
        if worker_seen_s is not None and time.monotonic() - stable_since > 6:
            break
    (ctx.art / "interactive-pane-final.txt").write_text(pane)
    hooks = read_jsonl(hook_log)
    main, sub = split_hooks(hooks)
    sid = next(((h.get("hook_input") or {}).get("session_id") for h in hooks), None)
    ctx.cache["interactive"] = {"hooks": hooks}
    obs.update({
        "dialogs_answered": answered or "none",
        "dialog_selections": selections or "none",
        "pane_state": pane_state(),
        "capture_errors": capture_errors or "none",
        "probe_log_lines": len(hooks),
        "hook_classification_mismatches": classification_mismatches(hooks) or "none",
        "main_bash_lines": [hook_command(h) for h in main],
        "worker_bash_lines": [hook_command(h) for h in sub],
        "worker_line_after_s": worker_seen_s,
        "LEADHAND_RUN_ID_values": sorted({str(h.get("LEADHAND_RUN_ID")) for h in hooks}),
        "permission_modes": sorted({str((h.get("hook_input") or {}).get("permission_mode"))
                                    for h in hooks}),
        "session_id": sid,
        "transcripts_found": copy_transcripts(sid, ctx.art / "interactive-transcripts") or "none",
    })
    ok = bool(main) and bool(sub)
    return ("PASS" if ok else "OBSERVED", commands, pane.rstrip().splitlines()[-10:], obs,
            f"log_lines={len(hooks)} worker_line={'yes' if sub else 'no'}")


IDENTITY_KEYS = ["agent_id", "agent_type", "permission_mode", "session_id", "cwd", "hook_event_name"]


def identity(h: dict | None) -> dict:
    if not h:
        return {"present": False}
    hi = h.get("hook_input") or {}
    d = {k: hi.get(k, "absent") for k in IDENTITY_KEYS}
    d["LEADHAND_RUN_ID"] = h.get("LEADHAND_RUN_ID")
    d["pwd"] = h.get("pwd")
    d["command"] = hook_command(h)
    d["hook_input_keys"] = sorted(hi.keys())
    return d


def step4(ctx: Ctx):
    r = cached_headless(ctx, "lhprobe:super", "go")
    obs = {}
    sources = [("headless", r["hooks"])]
    if "interactive" in ctx.cache:
        sources.append(("interactive", ctx.cache["interactive"]["hooks"]))
    for src, hooks in sources:
        main, sub = split_hooks(hooks)
        for who, h in (("main", main[0] if main else None), ("worker", sub[0] if sub else None)):
            for k, v in identity(h).items():
                obs[f"{src} {who} {k}"] = v
        obs[f"{src} line_count"] = f"main={len(main)} worker={len(sub)} total={len(hooks)}"
        obs[f"{src} classification"] = "by command (main=`echo probe…`, worker=`pwd`)"
        obs[f"{src} agent_id_vs_command_mismatches"] = classification_mismatches(hooks) or "none"
    excerpt = [json.dumps(h)[:300] for h in r["hooks"][:10]]
    m, s = split_hooks(r["hooks"])
    mi = identity(m[0] if m else None)
    si = identity(s[0] if s else None)
    summary = (f"main agent_id={mi.get('agent_id')} agent_type={mi.get('agent_type')}; "
               f"worker agent_id={si.get('agent_id', 'n/a')} agent_type={si.get('agent_type')}; "
               f"run_id={mi.get('LEADHAND_RUN_ID')}")
    return ("OBSERVED", [(r["argv"], r["exit_code"])], excerpt, obs, summary)


def step5(ctx: Ctx):
    r = cached_headless(ctx, "lhprobe:super", "go-bg", grace=BACKGROUND_GRACE)
    uses = tool_uses(r["records"])
    results = tool_results(r["records"])
    dispatch = [u for u in uses if u["name"] in AGENT_TOOLS]
    d = dispatch[0] if dispatch else None
    ret = results.get(d["id"]) if d else None
    result_recs = [(t, x) for t, x in r["records"] if x.get("type") == "result"]
    notif = [t for t, x in r["records"] if x.get("type") == "user"
             and "task-notification" in json.dumps(x)]
    _, sub = split_hooks(r["hooks"])
    obs = common_observed(r)
    obs.update({
        "dispatch_input": d["input"] if d else "no Agent call",
        "dispatch_tool_use_at_s": d["t"] if d else None,
        "dispatch_tool_result_at_s": ret[0] if ret else None,
        "dispatch_tool_result_text": (ret[1][:400] if ret else None),
        "agent_id_in_result": bool(ret and re.search(r"agent[ _-]?id|\ba[0-9a-f]{6,}", ret[1], re.I)),
        "result_records": [(t, x.get("subtype"), str(x.get("result", ""))[:120]) for t, x in result_recs],
        "task_notification_seen_at_s": notif or "none",
        "worker_hook_lines_before_exit": len(split_hooks(r["hooks"][:r["hooks_at_exit"]])[1]),
        "worker_hook_lines_after_grace": len(sub),
        "grace_s": BACKGROUND_GRACE,
        "subagent_records_in_stream": sum(1 for _, x in r["records"] if x.get("parent_tool_use_id")),
        "worker_bash_in_stream": len(bash_by_role(r["records"], "worker")),
        "hook_classification_mismatches": classification_mismatches(r["hooks"]) or "none",
    })
    waited = obs["worker_hook_lines_before_exit"] > 0 and bool(notif)
    summary = (f"exit_after={r['exit_s']}s worker_before_exit={obs['worker_hook_lines_before_exit']} "
               f"notification={'yes' if notif else 'no'} agent_id_returned={obs['agent_id_in_result']}")
    return ("OBSERVED", [(r["argv"], r["exit_code"])], result_excerpt(r),
            dict(obs, waited_for_worker=waited), summary)


def step6(ctx: Ctx):
    PROBE_HOME.mkdir(exist_ok=True)
    sandbox = user_sandbox()
    obs = {
        "user_sandbox_enabled": sandbox.get("enabled", "absent"),
        "user_excludedCommands": sandbox.get("excludedCommands", "absent"),
        "overlay": overlay_json(),
    }
    commands, excerpt = [], []
    for variant in ("control", "overlay"):
        cmds = step6_commands(variant, ctx.tag)
        markers = {
            "net_probe": PROBE_HOME / f"net-probe-{variant}-{ctx.tag}",
            "plain_write": PROBE_HOME / f"plain-{variant}-{ctx.tag}.txt",
            "user_exclusion_gh": PROBE_HOME / f"gh-{variant}-{ctx.tag}.txt",
        }
        ctx.markers += markers.values()
        # Each command's marker filename is unique, so retries or edited commands still match.
        needles = {k: m.name if k != "net_probe" else NET_PROBE.name for k, m in markers.items()}
        r = run_headless(ctx, f"settings-{variant}", step6_argv(ctx, variant),
                         fixture="settings repo", env_extra=STEP6_ENV)
        commands.append((r["argv"], r["exit_code"]))
        uses = [u for u in tool_uses(r["records"]) if u["name"] == "Bash"]
        results = tool_results(r["records"])
        obs[f"{variant} exit_code"] = r["exit_code"]
        obs[f"{variant} deny_unsandboxed_hook"] = "on (PROBE_DENY_UNSANDBOXED=1)"
        tainted = []
        for key, cmd in cmds.items():
            mine = [u for u in uses if needles[key] in (u["input"].get("command") or "")]
            hook_mine = [h for h in r["hooks"] if needles[key] in hook_command(h)]
            unsandboxed = [u for u in mine if u["input"].get("dangerouslyDisableSandbox")]
            unsandboxed_hooks = [h for h in hook_mine if ((h.get("hook_input") or {})
                                 .get("tool_input") or {}).get("dangerouslyDisableSandbox")]
            exact = [u for u in mine if (u["input"].get("command") or "").strip() == cmd]
            first = exact[0] if exact else (mine[0] if mine else None)
            out = results.get(first["id"], (None, ""))[1] if first else None
            m = markers[key]
            obs[f"{variant} {key} command"] = cmd
            obs[f"{variant} {key} use_count"] = len(mine)
            obs[f"{variant} {key} hook_line_count"] = len(hook_mine)
            obs[f"{variant} {key} run_exactly"] = bool(exact)
            obs[f"{variant} {key} variants_run"] = sorted({u["input"].get("command") for u in mine
                                                          if u not in exact}) or "none"
            obs[f"{variant} {key} any_disable_sandbox"] = bool(unsandboxed or unsandboxed_hooks)
            obs[f"{variant} {key} denied_by_hook"] = sum(1 for h in hook_mine if h.get("denied"))
            obs[f"{variant} {key} first_output"] = (out or "")[:200].replace("\n", " | ") if first else None
            obs[f"{variant} {key} marker_exists"] = m.exists()
            obs[f"{variant} {key} marker_content"] = m.read_text().strip()[:80] if m.exists() else None
            if len(mine) > 1 or unsandboxed or unsandboxed_hooks or (mine and not exact):
                tainted.append(key)
            if first:
                excerpt.append(f"[{variant}] {key}: {(out or '').strip()[:150]}")
        if "user_exclusion_gh" not in cmds:
            obs[f"{variant} user_exclusion_gh"] = "skipped: no `gh *` entry in user excludedCommands"
        obs[f"{variant} other_bash_commands"] = [
            u["input"].get("command") for u in uses
            if not any(n in (u["input"].get("command") or "") for n in needles.values())] or "none"
        obs[f"{variant} TAINTED"] = (f"yes: {tainted} (retried, edited, or sandbox-disable attempted)"
                                     if tainted else "no")

    def mark(variant: str, key: str) -> str:
        v = obs.get(f"{variant} {key} marker_exists", "n/a")
        return f"{v}{'(TAINTED)' if key in obs.get(f'{variant} TAINTED', '') else ''}"
    summary = (f"net-probe marker control={mark('control', 'net_probe')} "
               f"overlay={mark('overlay', 'net_probe')}; user gh exclusion "
               f"control={mark('control', 'user_exclusion_gh')} "
               f"overlay={mark('overlay', 'user_exclusion_gh')}")
    return ("OBSERVED", commands, excerpt[:10], obs, summary)


STEP_FUNCS = {1: step1, 2: step2, 3: step3, 4: step4, 5: step5, 6: step6}


def dry_commands(ctx: Ctx, n: int) -> list[str]:
    def note(fixture: str = "fixture repo", extra: str = "") -> str:
        return (f"  (cwd={ctx.fixture_path(fixture)!r}; env LEADHAND_RUN_ID={RUN_ID}, "
                f"PROBE_LOG=<out>/task2-artifacts/<label>-hooks.jsonl{extra})")
    if n in (1, 4):
        cmds = [shlex.join(headless_argv(ctx, "go")) + note()]
        if n == 1:
            cmds.append("(only if the probe echo did not run) "
                        + shlex.join(fallback_argv(ctx)) + note())
        return cmds
    if n == 2:
        return [shlex.join(headless_argv(ctx, "go")) + note(),
                shlex.join(headless_argv(ctx, "go", "lhprobe:super-ns")) + note()]
    if n == 3:
        sock = f"lhprobe-{ctx.tag}"
        fx = ctx.fixture_path("interactive repo")
        return [shlex.join(tmux_argv(sock, sock, fx, "<out>/task2-artifacts/interactive-hooks.jsonl")),
                f"tmux -L {sock} send-keys -t {sock} -l go ; tmux -L {sock} send-keys -t {sock} Enter",
                f"(poll: tmux -L {sock} capture-pane -p -t {sock}; answer first-run dialogs with Enter)"]
    if n == 5:
        return [shlex.join(headless_argv(ctx, "go-bg")) + note()
                + f"  (then wait {BACKGROUND_GRACE}s and re-read PROBE_LOG)"]
    if n == 6:
        extra = ", " + ", ".join(f"{k}={v}" for k, v in STEP6_ENV.items())
        return [shlex.join(step6_argv(ctx, v)) + note("settings repo", extra)
                for v in ("control", "overlay")]
    return []


# ---------------------------------------------------------------- report


def fmt(v) -> str:
    return v if isinstance(v, str) else json.dumps(v, default=str)


def write_section(ctx: Ctx, n: int, status: str, commands, excerpt, obs, error=None) -> None:
    parts = [f"\n## Step {n}: {STEPS[n]}\n", f"status: {status}\n", "\n### Commands\n"]
    for argv, code in commands:
        text = argv if isinstance(argv, str) else shlex.join(argv)
        parts.append(f"\n```\n{text}\n```\nexit_code: {code}\n")
    parts.append("\n### Excerpt\n\n```\n" + "\n".join(str(l) for l in excerpt[:10]) + "\n```\n")
    parts.append("\n### Observed\n\n")
    parts += [f"- {k}: {fmt(v)}\n" for k, v in obs.items()]
    if error:
        parts.append("\n### Error\n\n```\n" + error + "\n```\n")
    with open(ctx.raw, "a") as f:
        f.write("".join(parts))


def version(argv: list[str]) -> str:
    try:
        p = subprocess.run(argv, capture_output=True, text=True, timeout=30)
        return (p.stdout or p.stderr).strip().splitlines()[0]
    except Exception as e:  # noqa: BLE001 - recorded, not fatal
        return f"unavailable ({e})"


def cleanup(ctx: Ctx) -> None:
    for sock in ctx.tmux_sockets:
        try:
            tmux(sock, "kill-server")
        except Exception:  # noqa: BLE001
            pass
    for pgid in ctx.pgids:
        kill_group(pgid)
    kept = ctx.art / "markers"
    for m in ctx.markers:
        if m.exists():
            kept.mkdir(parents=True, exist_ok=True)
            shutil.copy2(m, kept / m.name)
            m.unlink()
    try:
        PROBE_HOME.rmdir()
    except OSError:
        pass
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
        description="Task 2 runtime probe: supervisor launch and hook identity. "
                    "Run in an ordinary terminal, not inside Claude Code.")
    ap.add_argument("--out", required=True, help="directory for task2-raw.md and task2-artifacts/")
    ap.add_argument("--only", help="comma-separated step numbers to run (1-6)")
    ap.add_argument("--dry-run", action="store_true", help="print each step's commands; run nothing")
    ap.add_argument("--print-permission", choices=sorted(PERMISSION_ROUTES), default="allowed-tools",
                    help="how -p sessions get Bash/Agent permission (default: allowed-tools)")
    args = ap.parse_args(argv)
    try:
        steps = parse_steps(args.only)
    except ValueError as e:
        ap.error(str(e))
    ctx = Ctx(Path(args.out).resolve(), args.print_permission, args.dry_run)

    if args.dry_run:
        for n in steps:
            print(f"Step {n}: {STEPS[n]}")
            for c in dry_commands(ctx, n):
                print(f"  {c}")
        return 0

    if os.environ.get("CLAUDECODE"):
        print("probe_launch.py: refusing to run inside a Claude Code session (CLAUDECODE is set); "
              "run it from an ordinary terminal.", file=sys.stderr)
        return 2

    ctx.art.mkdir(parents=True, exist_ok=True)
    ctx.raw.write_text(
        f"# Task 2 raw probe results: supervisor launch and hook identity\n\n"
        f"- date: {datetime.now(timezone.utc).isoformat(timespec='seconds')}\n"
        f"- claude: {version(['claude', '--version'])}\n"
        f"- tmux: {version(['tmux', '-V'])}\n"
        f"- python: {sys.version.split()[0]}\n"
        f"- plugin_dir: {PLUGIN_DIR}\n"
        f"- print_permission_route: {shlex.join(ctx.perm)}\n"
        f"- steps: {steps}\n"
    )
    summary = []
    try:
        for n in steps:
            print(f"Step {n}: {STEPS[n]} ...", flush=True)
            try:
                status, commands, excerpt, obs, line = STEP_FUNCS[n](ctx)
                write_section(ctx, n, status, commands, excerpt, obs)
            except Exception:  # noqa: BLE001 - a failing step is a finding
                tb = traceback.format_exc()
                status, line = "FAIL", tb.strip().splitlines()[-1]
                try:  # the planned commands, with placeholder fixture paths
                    planned = [(c, "not recorded: step raised")
                               for c in dry_commands(Ctx(ctx.out, ctx.route, True), n)]
                except Exception:  # noqa: BLE001
                    planned = []
                write_section(ctx, n, status, planned, tb.strip().splitlines()[-10:], {}, error=tb[-3000:])
            summary.append(f"Step {n} {STEPS[n]}: {status} {line}")
    finally:
        cleanup(ctx)
    with open(ctx.raw, "a") as f:
        f.write("\n## Summary\n\n" + "".join(f"- {s}\n" for s in summary))
    print("\n".join(summary))
    print(f"raw results: {ctx.raw}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
