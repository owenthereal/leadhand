"""Records the Python reference's behaviour into the fixtures beside this file.

The Rust tests that once called into the Python reference read these fixtures instead
(Task 9 of the cut-over). Run from the repository root:

    python3 -B tests/fixtures/reference/generate.py

It imports `leadhand` from scripts/lib, writes every fixture under
tests/fixtures/reference/, and prints each file with its size. It is deterministic: every
input is fixed (no timestamps, pids, random values or host paths reach a fixture), so a
rerun reproduces every fixture byte for byte. README.md says what each file is. One
folder is an input, never written: worker-prompts/agents/, frozen copies of the agent files
the worker prompts are made from.

This file is the only one here that may name the Python reference; the fixtures and the
README never do (each write is checked). It is deleted with the Python in Task 10.
"""
from __future__ import annotations

import contextlib
import io
import json
import os
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
OUT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "scripts" / "lib"))

from leadhand import briefs, cli, procs, state  # noqa: E402
from leadhand import dispatch as d  # noqa: E402
from leadhand import usage as u  # noqa: E402

# What no fixture may hold: the reference's paths and interpreter, the basename of any
# tracked .py file (all of them leave with the Python), and this host's paths.
def _py_basenames() -> list[bytes]:
    names = set()
    for top in ("scripts", "tests"):
        for _dir, _subdirs, files in os.walk(ROOT / top):
            names.update(n.encode() for n in files if n.endswith(".py"))
    return sorted(names)


FORBIDDEN = [b"scripts/leadhand", b"scripts/lib/", b"tests/contract/", b"python3"]
FORBIDDEN += _py_basenames()
FORBIDDEN += [str(ROOT).encode(), str(Path.home()).encode(), tempfile.gettempdir().encode()]

FIXED_TS = "2026-01-01T00:00:00.000Z"
WRITTEN: list[Path] = []


def write(name: str, data: bytes) -> None:
    for word in FORBIDDEN:
        if word in data:
            raise SystemExit(f"{name} would hold {word!r}")
    path = OUT / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    WRITTEN.append(path)


def dumps(value) -> str:
    """As the reference writes a JSON line: ASCII escapes, no spaces."""
    return json.dumps(value, separators=(",", ":"))


def json_rows(rows: list) -> bytes:
    """A JSON array, one row per line."""
    return ("[\n" + ",\n".join(dumps(r) for r in rows) + "\n]\n").encode("ascii")


@contextlib.contextmanager
def patched(obj, **attrs):
    saved = {k: getattr(obj, k) for k in attrs}
    for k, v in attrs.items():
        setattr(obj, k, v)
    try:
        yield
    finally:
        for k, v in saved.items():
            setattr(obj, k, v)


def fs(b: bytes) -> str:
    """OS bytes as the reference holds them (a surrogate escape for each undecodable byte)."""
    return os.fsdecode(b)


# --------------------------------------------------------------------------
# procs-start-times.json

def linux_stat(pid: int, comm: bytes, st: str, ppid: int, pgrp: int, start: int) -> bytes:
    """A `/proc/<pid>/stat` line of a `sleep` process, every field as Linux writes it."""
    rest = [st, ppid, pgrp, 4241, 34816, pgrp, 4194304, 101, 0, 0, 0, 0, 0, 0, 0, 20, 0, 1, 0,
            start, 8343552, 236, 18446744073709551615, 187650000000000, 187650000020000,
            281474000000000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, 3, 0, 0, 0, 0, 0,
            187650000030000, 187650000031000, 187650100000000, 281474000100000,
            281474000100100, 281474000100100, 281474000101000, 0]
    assert len(rest) == 50
    return f"{pid} (".encode() + comm + b") " + " ".join(map(str, rest)).encode() + b"\n"


def kinfo(pid: int, ppid: int, pgid: int, stat: int, sec: int, usec: int) -> bytes:
    """A `KERN_PROC_PID` record of a `sleep` process: the fields the parser reads, the command
    name, and fixed stand-ins for the owner; every other byte zero."""
    buf = bytearray(procs._KINFO_SIZE)
    struct.pack_into("<qi", buf, procs._OFF_START, sec, usec)
    struct.pack_into("<b", buf, procs._OFF_STAT, stat)
    struct.pack_into("<i", buf, procs._OFF_PID, pid)
    buf[243:243 + 5] = b"sleep"  # kp_proc.p_comm
    struct.pack_into("<I", buf, procs._OFF_RUID, 1000)
    struct.pack_into("<ii", buf, procs._OFF_PPID, ppid, pgid)
    return bytes(buf)


def check_kinfo_layout() -> None:
    """On macOS, a live `sleep` child's record has the layout the samples follow."""
    if sys.platform != "darwin":
        return
    child = subprocess.Popen(["sleep", "30"], stdin=subprocess.DEVNULL)
    try:
        raw = procs._sysctl([procs._CTL_KERN, procs._KERN_PROC, procs._KERN_PROC_PID, child.pid],
                            procs._KINFO_SIZE)
        assert raw is not None and len(raw) == procs._KINFO_SIZE, "no record of the child"
        info = procs._kinfo(raw, 0)
        assert info.pid == child.pid and info.ppid == os.getpid(), info
        assert info.pgid == os.getpgrp(), info
        (stat,) = struct.unpack_from("b", raw, procs._OFF_STAT)
        assert stat in (2, 3), stat  # SRUN, SSLEEP
        assert b"sleep\0" in raw[243:243 + 17], raw[243:243 + 17]
    finally:
        child.kill()
        child.wait()


def derived(info) -> dict | None:
    if info is None:
        return None
    return {"pid": info.pid, "ppid": info.ppid, "pgid": info.pgid, "start": info.start,
            "zombie": info.zombie}


def procs_samples() -> None:
    class Stat:
        raw = b""

        def __init__(self, _path):
            pass

        def read_bytes(self):
            return Stat.raw

    linux = [
        ("a sleep child", 4242, linux_stat(4242, b"sleep", "S", 4241, 4242, 1234567)),
        ("a zombie", 4243, linux_stat(4243, b"sleep", "Z", 4241, 4243, 1234890)),
        ("a dead process", 4244, linux_stat(4244, b"sleep", "X", 4241, 4244, 1234891)),
        ("running, in its parent's group", 4245,
         linux_stat(4245, b"sleep", "R", 4241, 4241, 98765432101)),
        ("a name with spaces and parentheses", 4246,
         linux_stat(4246, b"sl) (eep x", "S", 4241, 4246, 1235000)),
        ("a name that is not UTF-8", 4247,
         linux_stat(4247, b"sl\xffeep", "S", 4241, 4247, 1235001)),
        ("a line cut short", 4248, b"4248 (sleep) S 4241 4248 4241 34816\n"),
    ]
    rows_linux = []
    with patched(procs, Path=Stat, process_info=procs._proc_stat):
        for name, pid, raw in linux:
            Stat.raw = raw
            rows_linux.append({"name": name, "pid": pid, "stat": fs(raw),
                               "info": derived(procs._proc_stat(pid)),
                               "start_time": procs.process_start_time(pid)})

    check_kinfo_layout()
    macos = [
        ("a sleep child", 4242, kinfo(4242, 4241, 4242, 2, 1763700000, 42)),
        ("asleep, at a whole second", 4243, kinfo(4243, 4241, 4243, 3, 1763700001, 0)),
        ("in its parent's group, the last microsecond", 4244,
         kinfo(4244, 4241, 4241, 2, 1763700002, 999999)),
        ("a zombie", 4245, kinfo(4245, 4241, 4245, procs._SZOMB, 1763700003, 500000)),
        ("another pid's record", 4246, kinfo(4247, 4241, 4247, 2, 1763700004, 1)),
        ("no such pid", 4248, b""),
        ("a record cut short", 4249, kinfo(4249, 4241, 4249, 2, 1763700005, 2)[:100]),
    ]
    rows_macos = []
    raw_now = {"raw": b""}
    with patched(procs, _sysctl=lambda _mib, _size: raw_now["raw"],
                 process_info=procs._darwin_pid):
        for name, pid, raw in macos:
            raw_now["raw"] = raw
            rows_macos.append({"name": name, "pid": pid, "kinfo": raw.hex(),
                               "info": derived(procs._darwin_pid(pid)),
                               "start_time": procs.process_start_time(pid)})

    text = ('{"linux_stat":' + json_rows(rows_linux).decode().rstrip("\n")
            + ',\n"macos_kinfo":' + json_rows(rows_macos).decode().rstrip("\n") + "}\n")
    write("procs-start-times.json", text.encode("ascii"))


# --------------------------------------------------------------------------
# python-events.jsonl

def events_with_raw_bytes() -> None:
    paths = [
        fs(b"a\xffb"),                               # a surrogate escape
        "café ☕",                          # non-ASCII text
        "\U0001f600/x",                              # an astral character
        fs(b"/ws/caf\xc3\xa9/\xfe\xff/\xf0\x9f\x98\x80"),  # all of them in one path
        "x\ud800y",                                  # a lone surrogate, no escape
    ]
    with tempfile.TemporaryDirectory() as tmp:
        run = Path(tmp)
        for p in paths:
            state.append_jsonl(state.run_paths(run), run / "records.jsonl", {"path": p})
        write("python-events.jsonl", (run / "records.jsonl").read_bytes())


# --------------------------------------------------------------------------
# charge-checkpoint/

def claude_line(mid: str, out: int, inp: int = 0, cached: int = 0, w5: int = 0,
                w1: int = 0) -> str:
    """A transcript line, as the Rust tests' `claude_line` writes it."""
    return json.dumps({"type": "assistant", "message": {
        "id": mid, "model": "claude-opus-4-1", "role": "assistant", "usage": {
            "input_tokens": inp, "cache_read_input_tokens": cached, "output_tokens": out,
            "cache_creation_input_tokens": w5 + w1,
            "cache_creation": {"ephemeral_5m_input_tokens": w5,
                               "ephemeral_1h_input_tokens": w1}}}})


def charge_checkpoint() -> None:
    transcript = [claude_line("m1", 5, 100, 1000, 50, 0), claude_line("m2", 7, 3, 0, 0, 4),
                  claude_line("m1", 40, 100, 1000, 50, 0), claude_line("m3", 9, 1, 2, 0, 0)]
    rollout = (ROOT / "tests" / "fixtures" / "codex-rollout-interrupted.jsonl").read_text(
        encoding="utf-8").splitlines()
    with tempfile.TemporaryDirectory() as tmp, patched(u, utc_now=lambda: FIXED_TS):
        tmp = Path(tmp)
        root = tmp / "run with spaces"
        root.mkdir()
        paths = state.run_paths(root)
        sessions = tmp / "sessions"
        f = tmp / "projects/p/s/subagents/agent-agent1.jsonl"
        f.parent.mkdir(parents=True)
        thread = sessions / "2026/11/10/rollout-2026-11-10T00-00-00-thr1.jsonl"
        thread.parent.mkdir(parents=True)

        def charge(cid: str, dispatch_id: str) -> None:
            u.charge(paths, charge_id=cid + ":claude", source="claude", key="agent1",
                     dispatch_id=dispatch_id, role="implementer", path_or_thread=f)
            u.charge(paths, charge_id=cid + ":codex", source="codex", key="thr1",
                     dispatch_id=dispatch_id, role="implementer", path_or_thread="thr1",
                     sessions_dir=sessions)

        # The first half: m1 mid-stream, m2 whole; and the killed Codex turn.
        f.write_text("\n".join(transcript[:2]) + "\n", encoding="utf-8")
        thread.write_text("\n".join(rollout[:18]) + "\n", encoding="utf-8")
        charge("py1", "d1")
        write("charge-checkpoint/charges.jsonl", paths.charges.read_bytes())
        # The rest, then a charge that finds nothing new, all by the reference.
        f.write_text("\n".join(transcript) + "\n", encoding="utf-8")
        thread.write_text("\n".join(rollout) + "\n", encoding="utf-8")
        charge("rs1", "d2")
        charge("rs2", "d3")
        charges = u.read_charges(paths)
        assert len(charges) == 6, charges
        write("charge-checkpoint/charges-all.jsonl", paths.charges.read_bytes())
        t = u.totals(charges, by="source")
        totals = dumps({k: u.usage_to_dict(v) for k, v in t.items()}) + "\n"
    write("charge-checkpoint/transcript.jsonl", ("\n".join(transcript) + "\n").encode("utf-8"))
    write("charge-checkpoint/totals-by-source.json", totals.encode("ascii"))


# --------------------------------------------------------------------------
# usage-mixed.json

def usage_mixed() -> None:
    a = u.Usage(input=1000, output=1000, model="claude-opus-4-1")
    b = u.Usage(input=2000, output=2000, model="claude-sonnet-4-5")

    def p(i: float, o: float) -> dict:
        return {"input": i, "output": o, "cache_read": i / 10, "cache_write_5m": i * 1.25,
                "cache_write_1h": i * 2.0}

    s = u.add(a, b)
    cost = u.estimate_cost(s, None, {"opus": p(15.0, 75.0), "sonnet": p(3.0, 15.0)})
    text = '{"usage":' + dumps(u.usage_to_dict(s)) + ',"cost":' + dumps(repr(cost)) + "}\n"
    write("usage-mixed.json", text.encode("ascii"))


# --------------------------------------------------------------------------
# worker-prompts/<role>.txt

WORKER_PATHS = [
    (b"/ws/.leadhand/1/c1-a1-implement-1/brief.md", b"/ws/.leadhand/1/c1-a1-implement-1/report.md"),
    (b"/ws/caf\xff/.leadhand/run/d/brief.md", b"/ws/caf\xff/.leadhand/run/d/report.md"),
    (b"/ws/caf\xc3\xa9 \xe2\x98\x95/brief.md", b"/ws/caf\xc3\xa9 \xe2\x98\x95/report.md"),
    (b"rel//./ws/brief.md", b"./rel/report.md/"),
]


FROZEN_AGENTS = OUT / "worker-prompts" / "agents"


def worker_prompts() -> None:
    """The prompts, made from the frozen copies of the agent files in worker-prompts/agents/
    (an input here, never written), so that the fixture does not follow later agent edits."""
    for name in sorted(p.name for p in FROZEN_AGENTS.glob("*.md")):
        same = (FROZEN_AGENTS / name).read_bytes() == (ROOT / "agents" / name).read_bytes()
        print(f"frozen agents/{name}: {'identical to' if same else 'differs from'} agents/{name}")
    with patched(briefs, AGENTS_DIR=FROZEN_AGENTS):
        for role in briefs.ROLE_AGENT:
            prompts = [os.fsencode(briefs.worker_prompt(role, Path(fs(b)), Path(fs(r))))
                       for b, r in WORKER_PATHS]
            write(f"worker-prompts/{role}.txt", b"\0".join(prompts))


# --------------------------------------------------------------------------
# codex-argv.json

def codex_argv_table() -> None:
    def path(b: bytes) -> str:
        """A request path, as `request.json` spells it (`str(Path(...))`)."""
        return str(Path(fs(b)))

    def base(role: str) -> dict:
        return {"model": "gpt-6-luna", "effort": "medium", "role": role,
                "brief": "/ws/.leadhand/1/c1-a1/brief.md",
                "report": "/ws/.leadhand/1/c1-a1/report.md", "cwd": "/ws", "run_dir": "/runs/r1",
                "card": 1, "attempt": 1, "dispatch_id": "c1-a1", "steps": 40, "minutes": 20,
                "network": False, "writable_dirs": [], "resume": None, "kind": "implement",
                "branch": "lh/r1"}

    def overrides() -> list:
        return [[fs(k), fs(v)] for k, v in [
            (b"GIT_SSH_COMMAND", b"false"),
            (b"GIT_TERMINAL_PROMPT", b"0"),
            (b"GH_CONFIG_DIR", b"/runs/r 1/workers/c1-a1/gh\xff-config"),
            (b"GIT_CONFIG_KEY_0", b"http.extraHeader"),
            (b"GIT_CONFIG_VALUE_0", b"say \"hi\" \\ \n\t\x01\x1f\x7f caf\xc3\xa9 \xfe"),
            (b"GIT_CONFIG_KEY_1", b"credential.helper"),
            (b"GIT_CONFIG_VALUE_1", b""),
            (b"GIT_CONFIG_COUNT", b"2"),
            (b"GIT_CONFIG_PARAMETERS", b"'a.b=c' 'core.askPass='"),
            (b"LEADHAND_RUN_ID", b"r1"),
            (b"LEADHAND_DISPATCH_TAG", b"r1:c1-a1:1"),
        ]]

    def case(req: dict) -> dict:
        return {"req": req, "last": "/runs/r1/workers/c1-a1/last-message.md",
                "git_common": "/repo/.git", "overrides": overrides(), "extra": []}

    # The Rust test's table, case for case (`dispatch::argv::tests::table`).
    cases = []
    for i, role in enumerate(briefs.ROLE_AGENT):
        req = base(role)
        req["network"] = i % 2 == 0
        if i % 3 == 0:
            req["writable_dirs"] = ["/extra/a", "/extra b/c"]
        cases.append(case(req))
    for role in ("implementer", "scout"):
        req = base(role)
        req["resume"] = "0f0f0f0f-0000-4000-8000-00000000c0de"
        cases.append(case(req))
    req = base("reviewer")
    req["resume"] = ""
    cases.append(case(req))
    req = base("reviewer")
    req["resume"] = "-x"
    cases.append(case(req))
    req = base("implementer")
    req["resume"] = fs(b"thread-\xff")
    cases.append(case(req))
    req = base("implementer")
    req["cwd"] = "/w s/ws dir"
    req["writable_dirs"] = ["/w s/extra dir"]
    c = case(req)
    c["last"] = "/w s/runs/r 1/workers/c1-a1/last-message.md"
    c["git_common"] = "/w s/repo/.git"
    cases.append(c)
    req = base("verifier")
    req["cwd"] = "/wé/ws"
    req["model"] = "gpt-é"
    req["effort"] = "hégh \U0001f600"
    req["writable_dirs"] = ["/d→"]
    c = case(req)
    c["last"] = "/ré/last-message.md"
    cases.append(c)
    req = base("final_reviewer")
    req["cwd"] = path(b"/w\xff/ws")
    req["writable_dirs"] = [path(b"/d\xff"), path(b"/d\xc3")]
    c = case(req)
    c["last"] = fs(b"/w\xff/last-message.md")
    c["git_common"] = fs(b"/r\xfe/.git")
    c["extra"] = [fs(b"/r\xfe/.worktrees/ws--base")]
    cases.append(c)
    req = base("implementer_hard")
    req["cwd"] = path(b"/a//b/./c/")
    req["writable_dirs"] = [path(b"./rel/"), path(b"//two/slash/")]
    c = case(req)
    c["last"] = "rel//x/./last-message.md"
    c["git_common"] = "/repo/.git/"
    c["extra"] = ["/repo/.worktrees//x--base/"]
    cases.append(c)
    req = base("reviewer_hard")
    req["model"] = "gpt 6"
    req["effort"] = "a\"b\\c\nd\te\x01f\x7fg\x08\x0c\r"
    cases.append(case(req))
    for name in (b"NAME\n", b"1BAD", b"A-B", b"", b"N\xff"):
        c = case(base("implementer"))
        c["overrides"].append([fs(name), "v"])
        cases.append(c)
    req = base("implementer")
    req["resume"] = "--bad"
    c = case(req)
    c["overrides"].insert(0, ["BAD NAME", "v"])
    cases.append(c)
    req = base("implementer")
    req["effort"] = "x\ud800y"
    cases.append(case(req))
    req = base("implementer")
    req["model"] = "\udfff"
    cases.append(case(req))
    c = case(base("scout"))
    c["overrides"] = []
    cases.append(c)

    rows = []
    with patched(os, environ={k: v for k, v in os.environ.items() if k != "LEADHAND_TESTING"}):
        for c in cases:
            given = json.loads(dumps(c))  # as the reference reads it from JSON
            req = d.request_from_json(given["req"])
            try:
                argv = d.codex_argv(req, Path(given["last"]), Path(given["git_common"]),
                                    dict(given["overrides"]), [Path(e) for e in given["extra"]])
                out = "ok " + " ".join(os.fsencode(a).hex() for a in argv)
            except ValueError as e:
                out = "err " + str(e)
            rows.append({"case": c, "argv": out})
    write("codex-argv.json", json_rows(rows))


# --------------------------------------------------------------------------
# cli-version.txt

def cli_version() -> None:
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = cli.main(["version"])
    assert code == 0, code
    write("cli-version.txt", out.getvalue().encode("utf-8"))


def main() -> None:
    procs_samples()
    events_with_raw_bytes()
    charge_checkpoint()
    usage_mixed()
    worker_prompts()
    codex_argv_table()
    cli_version()
    for path in sorted(WRITTEN):
        print(f"{path.stat().st_size:>8}  {path.relative_to(OUT)}")


if __name__ == "__main__":
    main()
