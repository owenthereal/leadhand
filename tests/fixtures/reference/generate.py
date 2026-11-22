"""Records the Python reference's behaviour into the fixtures beside this file.

The Rust tests that once called into the Python reference read these fixtures instead
(Task 9 of the cut-over). Run from the repository root:

    python3 -B tests/fixtures/reference/generate.py

It imports `leadhand` from scripts/lib, writes every fixture under
tests/fixtures/reference/, and prints each file with its size. It is deterministic: every
input is fixed (no timestamps, pids, random values or host paths reach a fixture), so a
rerun reproduces every fixture byte for byte. README.md says what each file is. Some files
are inputs, never written: frozen copies of repository files the reference read, so that a
later edit of those files changes neither side (ruling S3-R30). They are
worker-prompts/agents/ (the agent files of the worker prompts and of the configuration's
maxTurns), config/presets/ (the presets) and
usage/codex-rollout/codex-rollout-interrupted.jsonl (a rollout fixture).

This file is the only one here that may name the Python reference; the fixtures and the
README never do (each write is checked). It is deleted with the Python in Task 10.
"""
from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path, PurePosixPath

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


def sections(parts: dict) -> bytes:
    """A JSON object of arrays, one row per line."""
    body = ",\n".join(dumps(k) + ":" + json_rows(rows).decode().rstrip("\n")
                      for k, rows in parts.items())
    return ("{" + body + "}\n").encode("ascii")


def mapped(fn, inputs: list) -> list:
    """`{"in": x, "out": fn(x)}` for each input, in order."""
    return [{"in": x, "out": fn(x)} for x in inputs]


def float_bits(x: float) -> str:
    """An input float as the fixtures give it: its bits, in hex (exact, whatever reads it)."""
    return struct.pack(">d", x).hex()


def unbits(h: str) -> float:
    return struct.unpack(">d", bytes.fromhex(h))[0]


def rooted(text: str | None, root: Path) -> str | None:
    """A path under a temporary root, with the root written `{root}`."""
    if text is None:
        return None
    r = str(root)
    if text == r or text.startswith(r + "/"):
        return "{root}" + text[len(r):]
    assert r not in text, text
    return text


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


# --------------------------------------------------------------------------
# pytext.json: the Python text and path functions the Rust emulates (`pytext::tests`)

TEXTS = [
    "",
    "plain",
    "it's",
    'say "hi"',
    "both ' and \"",
    "back\\slash",
    "tab\tnew\nline\rret",
    "\x00\x07\x1b\x7f",
    "caf\u00e9 \u2192 \U0001f600",
    "\x85\xa0\u2028\u3000",
    "\xad\u200b\ufeff\ue000",
    "  \t lead and trail \x1c\x1f ",
    "a\r\nb\rc\nd\x0be\x0cf\x1cg\x1dh\x1ei\x85j\u2028k\u2029l",
    "trailing newline\n",
    "\n\nblank lines\n\n",
    "x\x1fy z",
]


def floats(xs: list[float], fn) -> list:
    return [{"in": float_bits(x), "x": x, "out": fn(x)} for x in xs]


def real_paths() -> list:
    """`os.path.realpath` over the links the Rust test lays out under its root."""
    cases = ["dangling", "dangling/deeper", "relative", "relative/../x", "relative/up/sub",
             "loop-a", "loop-a/x", "repo/./sub//", "missing/../repo", "repo/sub/up/sub/up"]
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(os.path.realpath(tmp))
        (root / "repo/sub").mkdir(parents=True)
        os.symlink(root / "repo/notes/docs", root / "dangling")
        os.symlink("repo/sub", root / "relative")
        os.symlink(root / "loop-b", root / "loop-a")
        os.symlink(root / "loop-a", root / "loop-b")
        os.symlink("../repo", root / "repo/sub/up")
        return mapped(lambda c: rooted(os.path.realpath(f"{root}/{c}"), root), cases)


def which_rows() -> list:
    """`shutil.which` over the PATHs the Rust test gives, `{root}` its root, from its `cwd`."""
    cases = [
        ["/nonexistent:{root}/bin", "in-bin"],
        ["{root}/bin", "not-exec"],
        ["{root}/bin", "in-cwd"],
        ["/nonexistent::{root}/bin", "in-cwd"],
        ["{root}/bin:", "in-cwd"],
        [":", "in-cwd"],
        [":{root}/bin", "in-bin"],
        ["", "in-cwd"],
        ["", "in-bin"],
    ]
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(os.path.realpath(tmp))
        bin_, cwd = root / "bin", root / "cwd"
        for d in (bin_, cwd):
            d.mkdir()
        for p in (bin_ / "in-bin", cwd / "in-cwd"):
            p.write_text("#!/bin/sh\n")
            p.chmod(0o755)
        (bin_ / "not-exec").write_text("x")
        with contextlib.chdir(cwd):
            return mapped(lambda c: rooted(
                shutil.which(c[1], path=c[0].replace("{root}", str(root), 1)), root), cases)


def pytext_fixture() -> None:
    pure = ["//a//b/./c/", "///a", "", "./a", "a/..", ".", "a/./", "/", "//", "a//b",
            "/runs/2026-11-10-retry uploads/"]
    fixed = [0.25, 0.125, 2.5, 0.35, 1.005, 0.045, 12.25, 0.0, -0.0, 1e-9, 123456.785, 99.95,
             0.0001234567, 31.0, 1e21]
    reprs = [0.1, 2.5, 1e-5, 1e-4, 0.00012, 5.0, 1e15, 1e16, 1e20, 123456.789, 1.0 / 3.0, -2.5,
             -0.0, 0.0, 9.99e-5, 1.5e300, 5e-324]
    raw = [t.encode() for t in TEXTS] + [b"\xff refs/x \n", b" \xc2\xa0main\xff\xc2\xa0\n",
                                         b"\xe2\xc2\x85x\xe2\x80\x85", b"\xe2\x80\n", b"\xc2"]
    os_lines = [b"worktree /a\nHEAD x\n\nworktree /b c\n", b"a\r\nb\rc\x0bd\x0ce\x1cf\x1dg\x1eh",
                b"one \xc2\x85 two \xe2\x80\xa8 three \xe2\x80\xa9 four",
                b"\xff\n\xfe\xc2\x85\xe2\x80", b"\xe2\x80\xe2\x80\xa8x", b"\xc2\xc2\x85", b"",
                b"\n\n", b"no end"]
    tildes = ["~", "~/runs/{repo}", "/abs/~", "rel", "~other/x"]
    assert os.path.expanduser("~other/x") == "~other/x", "a user named other exists here"
    homes = ["/home/fixture", "/home/fixture//", "/", "//", ""]

    def expanded(c):
        with patched(os, environ={**os.environ, "HOME": c[0]}):
            return os.path.expanduser(c[1])

    paths = ["", ".", "/", "//", "///", "////x", "//x/../y", "a/b/../../..", "../a/./b//c/",
             "/../x", "/a/b/../../../c", "a/..", "./", "x/./y/.", "/runs/r 1/workers/c1-a1",
             "café/../d", "relative/dir"]
    cwds = ["/opt/fixture/work dir", "/"]

    def absolute(c):
        with patched(os, getcwd=lambda: c[0]):
            return os.path.abspath(c[1])

    write("pytext.json", sections({
        "pure_path": mapped(lambda x: str(PurePosixPath(x)), pure),
        "fixed_point": floats(fixed, lambda x: [f"{x:.1f}", f"{x:.2f}", f"{x:.6f}"]),
        "repr": mapped(repr, TEXTS),
        "strip_split_splitlines": mapped(lambda x: [x.strip(), x.split(), x.splitlines()], TEXTS),
        "strip_bytes": mapped(lambda x: list(bytes(x).decode("utf-8", "surrogateescape").strip()
                                             .encode("utf-8", "surrogateescape")),
                              [list(b) for b in raw]),
        "splitlines_bytes": mapped(lambda h: [os.fsencode(line).hex() for line in
                                              os.fsdecode(bytes.fromhex(h)).splitlines()],
                                   [b.hex() for b in os_lines]),
        "float_repr": floats(reprs, repr),
        "real_path": real_paths(),
        "which": which_rows(),
        "expand_user": mapped(expanded, [[h, t] for h in homes for t in tildes]),
        "normpath": mapped(os.path.normpath, paths),
        "abspath": mapped(absolute, [[d, p] for d in cwds for p in paths]),
    }))


# --------------------------------------------------------------------------
# wire.json: how the reference writes and reads JSON text (`wire::tests`)

def lone_surrogate_event() -> dict:
    """A ruling appended with a surrogate-escaped path, and the reference reading back the
    action the Rust test prints for it."""
    with tempfile.TemporaryDirectory() as tmp, patched(state, utc_now=lambda: FIXED_TS):
        paths = state.run_paths(Path(tmp))
        state.append_event(paths, "ruling", path=fs(b"a\xffb"))
        events = paths.events.read_text(encoding="ascii")
    raw = events.rstrip("\n")
    printed = '{"action": "dispatch", "path": "a\\udcffb", "event": ' + raw + "}"
    v = json.loads(printed)
    reads = f"{v['path'] == fs(b'a\xffb') == v['event']['path']}\n"
    return {"events": events, "printed": printed, "reads_back": reads}


def wire_fixture() -> None:
    escapes = [b"\xed\xa0\x80", b"\xe2\x82A", b"\xf0\x9f\x98A", b"a\xc0\xafb", b"\xf4\x90\x80\x80",
               b"ok\xf0\x9f\x98\x80\xe9", b"\x80\xbf\xc2"]
    strings = ["\u00e9", "\U0001f600", "a\"b\\c\n\t\r\x08\x0c\x01\x1f\x7f/", "\u4e2d\u6587 \u2028",
               "plain"]

    def dumped(s: str) -> list:
        d = {"s": s, "p": s, "n": [1, 2], "e": [], "o": {}}
        return [json.dumps(s, ensure_ascii=True),
                json.dumps(d, ensure_ascii=True, separators=(",", ":")),
                json.dumps(d, ensure_ascii=True), json.dumps(d, ensure_ascii=True, indent=2)]

    def read_back(text: str) -> str:
        d = json.loads(text)
        return f"{list(d) == ['refs/heads/a' + chr(0xdcff) + 'b', chr(0xe9), chr(0x1f600)]} {d[chr(0xe9)]}\n"

    pymap = '{"refs/heads/a\\udcffb":"w","\\u00e9":"y","\\ud83d\\ude00":"z"}'
    floats_ = [0.0, 1.0, 15.0, 0.1, 5.2e-05, 1e-07, 1e16, 123456.789, -2.5, 1e22]
    reprs = ['"N\\udcff"', "\"it's \\ud800\"", '"\\"q\\" and \'q\' \\udfff\\n"',
             '"caf\u00e9 \U0001f600 \\u0007"', '""']
    encodes = ['"plain caf\u00e9 \U0001f600"', '"ab\\udcff\\udcfecd\\udcffe"', '"x\\udcffy"',
               '"\u00e9\U0001f600\\udcff"', '"lone \\ud800 lead"', '"\\udfff"', '""']

    def encoded(literal: str) -> list:
        t = json.loads(literal)
        try:
            return ["ok", t.encode("utf-8").decode("utf-8")]
        except UnicodeEncodeError as e:
            return ["err", str(e)]

    write("wire.json", sections({
        "surrogateescape": mapped(lambda h: json.dumps(
            bytes.fromhex(h).decode("utf-8", "surrogateescape")), [b.hex() for b in escapes]),
        "lone_surrogate": [lone_surrogate_event()],
        "ascii_escapes": mapped(dumped, strings),
        "pymap_read_back": mapped(read_back, [pymap]),
        "floats": mapped(lambda bits: dumps([unbits(b) for b in bits]) + "\n",
                         [[float_bits(x) for x in floats_]]),
        "repr": mapped(lambda literal: repr(json.loads(literal)), reprs),
        "encode_utf8": mapped(encoded, encodes),
    }))


# --------------------------------------------------------------------------
# deck.json: the deck parser's patterns (`deck::tests`)

def deck_fixture() -> None:
    from leadhand.deck import _CARD, _FIELD, _HARD, _HEADER_SPLIT, _REQ

    reqs = ["- R1 Thing → cards 1", "- R1 Thing -> card 1",
            "- R12 a → b → cards 1, 2", "- R1 a -> b → cards 3 -> card 4",
            "- R1 a → cards 1 → nothing", "- R1 a → cards none",
            "- R1 a →cards 1", "- R1 → cards 1", "- R1\t\tx→  card\t7",
            "- R1 a cards 1", "- R1→ cards 1", "- R1 a → cardsx 1", "- R1 a → cards",
            "- R1 a --> cards 2", "- Rx a → cards 1", "- R a → cards 1",
            "-R1 a → cards 1", "- R1 café → cards 1,2 ,3"]
    headers = ["Base: main @ abc  Branch: b", "Base: main @ abc      Branch: leadhand/x",
               "Base: main @ abc Branch: b", "Base: main @ abc\t\tBranch: b",
               "Build: make build                 Test: make test",
               "Profile: standard                 Finish: none", "Source: a  b  c",
               "Source: a  Nope: b", "Source: a  Base:x  Branch: y", "Source: a \xa0Branch: y",
               "Source: a   Branch : y"]
    fields = ["- Role: implementer", "- Done when:   tests pass  ", "- Done when : x",
              "- Depends on:1", "- lower: x", "- Files: src/a.py", "- X1: y", "-  Role: x",
              "- Role x", "- A:b:c", "- Réle: x", "- : x"]
    cards = ["## Card 3: Retry uploads", "## Card 3:   spaced title   ", "## Card 3:",
             "## Card 12:x", "## Card 3 : x", "## Card x: y", "##  Card 3: y", "## Card 007: y"]
    hards = ["yes", "yes — no test can check it", "yes – x", "yes - x", "yes: x", "yes:x",
             "yes -", "yes because", "yess", "yes —   spaced  ", "Yes - x"]

    def groups(pattern, n: int):
        def match(x):
            m = pattern.match(x)
            return [m.group(i) for i in range(1, n + 1)] if m else None
        return match

    def req(x):
        m = _REQ.match(x)
        return [m.group(1), m.group(2).strip(), m.group(3)] if m else None

    def hard(x):
        m = _HARD.match(x)
        return (m.group(1) or "").strip() if m else None

    write("deck.json", sections({
        "requirement": mapped(req, reqs),
        "header_split": mapped(_HEADER_SPLIT.split, headers),
        "field": mapped(groups(_FIELD, 2), fields),
        "card": mapped(groups(_CARD, 2), cards),
        "hard": mapped(hard, hards),
    }))


# --------------------------------------------------------------------------
# config/config.json: configuration loading (`config::tests`), from frozen presets

FROZEN_PRESETS = OUT / "config" / "presets"


def report_frozen(frozen: Path, live: Path, glob: str) -> None:
    for name in sorted(p.name for p in frozen.glob(glob)):
        same = (frozen / name).read_bytes() == (live / name).read_bytes()
        print(f"frozen {live.name}/{name}: {'identical to' if same else 'differs from'} "
              f"{live.name}/{name}")


def config_fixture() -> None:
    from leadhand import config as cfg

    report_frozen(FROZEN_PRESETS, ROOT / "presets", "*.toml")

    def load(user: str, repo: str = ""):
        """The reference loading these files as the Rust test's `Files` lays them out."""
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            (d / "my repo").mkdir()
            if repo:
                (d / "my repo" / ".leadhand.toml").write_text(repo, encoding="utf-8")
            (d / "user.toml").write_text(user, encoding="utf-8")
            return cfg.load_config(d / "my repo", user_file=d / "user.toml",
                                   presets_dir=FROZEN_PRESETS)

    def snapshot(files):
        text = json.dumps(cfg.config_to_dict(load(*files)), indent=2, ensure_ascii=True)
        return {"snapshot": text, "back": str(cfg.config_from_dict(json.loads(text)).prices["x"])}

    def messages(user: str):
        try:
            c = load(user)
        except cfg.ConfigError as e:
            return ["error", str(e)]
        return ["ok", cfg.unsupported_settings(c) + cfg.unsupported_warnings(c)]

    snapshot_files = [
        "docs_dir = \"/tmp/d\"\n[prices.x]\ninput = inf\noutput = -inf\ncache_read = nan\n"
        "cache_write_5m = 1.5\ncache_write_1h = 0\n",
        "[notify]\ncommand = \"say NaN Infinity\"\n",
    ]
    cases = [
        "[run]\ncost_limit_usd = 2.5\n",
        "[run]\ncost_limit_usd = 1e-05\n",
        "[run]\ncost_limit_usd = 0.1\n",
        "[run]\ncost_limit_usd = 5\n",
        "[run]\ncost_limit_usd = 2.0\n",
        "[run]\ncost_limit_usd = 1e20\n",
        "[run]\ncost_limit_usd = 123456.789\n",
        "[run]\ncost_limit_usd = 'x'\n",
        "[run]\nworktree = \"it's\"\n",
        "[run]\nworktree = 'say \"hi\"'\n",
        "[run]\nworktree = \"both ' and \\\"\"\n",
        "[run]\nworktree = \"tab\\tbell\\u0007caf\\u00e9\\u00a0\"\n",
        "[run]\nworktree = [1, 2.5, true, 'x', {a = 1}]\n",
        "[run]\nworktree = 1.5\n",
        "[run]\nzz = 1\naa = 2\n",
        "profile = 'Lean'\n",
        "zeta = 1\nalpha = 2\n",
        "[capacity]\ncodex_pause_percent = 101\n",
        "[capacity]\ncodex_pause_percent = 9.5\n",
        "[codex]\nwritable_dirs = ['a', 1]\n",
        "[limits]\nx = { steps = 1, minutes = 2, z = 3 }\n",
        "[limits]\nverifier = { steps = 1, minutes = 2 }\nimplementer = { steps = 3, minutes = 2 }\n",
        "[prices.x]\ninput = 1\n",
        "[roles.zeta]\nclaude = 'x'\n[roles.alpha]\nclaude = 'y'\n",
        "[roles.implementer]\nprefer = 3\n",
        "[roles.implementer]\nclaude = ['a']\n",
        "[roles.implementer]\nclaude = 'sonnet extreme'\n",
        "[roles.dealer]\nprefer = 'codex'\n",
        "[after_run]\nstep = 'merge'\n",
        "[after_run]\nstep = 'pr'\non_incomplete = 'draft-pr'\n",
        "[notify]\ncommand = 'say x'\n[capacity]\ncodex_pause_percent = 3\n",
        "preset = '.x'\n",
        "preset = 'nope'\n",
        "preset = 3\n",
    ]
    with patched(briefs, AGENTS_DIR=FROZEN_AGENTS):
        write("config/config.json", sections({
            "snapshot": mapped(snapshot, [snapshot_files]),
            "messages": mapped(messages, cases),
        }))


# --------------------------------------------------------------------------
# cli-check-json.json: `check --json` of a deck that does not load (`cli::tests`)

def cli_check_json() -> None:
    """Python's `json.dumps` of the report the Rust test prints for a deck it cannot open (the
    failure's lossless text, a surrogate escape for the byte), and the message's bytes as
    Python reads them back from that JSON."""
    message = fs(b"[Errno 2] No such file or directory: '/decks/d\xff.md'")
    text = json.dumps({"ok": False, "cards": None,
                       "problems": [{"level": "error", "card": None, "message": message}],
                       "fix": None})
    read_back = list(os.fsencode(json.loads(text)["problems"][0]["message"]))
    write("cli-check-json.json", (dumps({"json": text, "read_back": read_back}) + "\n").encode())


# --------------------------------------------------------------------------
# state/: the run state the reference writes (`state::tests`)

def appended(*events) -> bytes:
    """The events file after the reference's `append_event` of each (kind, fields)."""
    with tempfile.TemporaryDirectory() as tmp, patched(state, utc_now=lambda: FIXED_TS):
        paths = state.run_paths(Path(tmp))
        for kind, fields in events:
            state.append_event(paths, kind, **fields)
        return paths.events.read_bytes()


def state_fixture() -> None:
    from dataclasses import fields as dc_fields

    write("state/event-kinds.txt", ("\n".join(state.EVENT_KINDS) + "\n").encode())

    odd = fs(b"src/a\xffb.txt")
    wide = "src/café \U0001f600.txt"
    write("state/round-trip.jsonl", appended(
        ("card_cut", dict(card=1, branch="leadhand/ré", base="b" * 40)),
        ("conflict", dict(card=1, onto="c" * 40, paths=[odd, wide])),
        ("violation", dict(stage="found", by="check", dispatch_id="c1-a1-review-1", card=1,
                           dispatch_kind="review", n=1, ref="refs/leadhand/abc/violations/1",
                           paths=[odd], head_moved=False, index_dirty=False, expected_head=None,
                           expected_branch="leadhand/r",
                           refs={"refs/heads/" + odd: {"old": None, "new": "d" * 40}},
                           preserved={"refs/heads/" + odd:
                                      "refs/leadhand/abc/violations/1-" + wide})),
        ("parked", dict(card="final", reason=wide)),
    ))

    v = "refs/leadhand/abc/violations/1"
    left = "refs/leadhand/abc/leftovers/1"
    write("state/violations.jsonl", appended(
        ("violation", dict(stage="found", by="check", dispatch_id="c1-a1-review-1", card=1,
                           dispatch_kind="review", n=1, ref=v, paths=[odd], head_moved=True,
                           index_dirty=False, expected_head="e" * 40,
                           expected_branch="leadhand/r--card-1",
                           refs={"refs/heads/x": {"old": None, "new": "d" * 40}},
                           preserved={"refs/heads/x": v + "-heads-x"}, hidden=[odd])),
        ("violation", dict(stage="restored", by="check", dispatch_id="c1-a1-review-1", card=1, n=1,
                           ref=v, saved="f" * 40, siblings=[v + "-branch"],
                           preserved={"refs/heads/x": v + "-heads-x"})),
        ("violation", dict(stage="found", by="implementer", n=1, ref=left, paths=[odd],
                           operation="merge", head_moved=False, index_dirty=True,
                           expected_head="e" * 40, expected_branch="leadhand/r--card-1",
                           after_dispatch="c1-a1-implement-1", why="before review c1-a1-review-1",
                           untracked=[odd], hidden=[odd])),
        ("violation", dict(stage="restored", by="implementer", n=1, ref=left, saved=None,
                           after_dispatch=None, siblings=[])),
        ("violation", dict(stage="found", by="check", dispatch_id="run-a1-evidence-1", card=None,
                           dispatch_kind="evidence", n=2, ref=None, base_worktree="/w/b--base",
                           base_head="e" * 40, paths=["x.txt"], head_moved=False,
                           index_dirty=False, expected_head=None, expected_branch=None, refs={},
                           preserved={})),
        ("violation", dict(stage="restored", by="check", dispatch_id="run-a1-evidence-1",
                           card=None, n=2, ref=None, saved="e" * 40, siblings=[], preserved={})),
    ))

    h = "e" * 40
    write("state/end-of-run.jsonl", appended(
        ("gate_started", dict(gate="evidence", examined_sha=h, dispatch_id="run-evidence-1")),
        ("gate_result", dict(gate="evidence", examined_sha=h, passed=False,
                             dispatch_id="run-evidence-1",
                             findings="/r/cards/run/run-evidence-1/report.md")),
        ("card_cut", dict(card="final", branch="leadhand/r--card-final", base=h, wave=1)),
        ("card_cut", dict(card="final", branch="leadhand/r--card-final", base=h, wave=1,
                          recut_after="run-w1-final_fix-1")),
        ("verdict_recorded", dict(card="final", wave=1, verdict="spent", reason="changed nothing",
                                  findings="/r/x.md", dispatch_id="run-w1-final_fix-1")),
        ("verdict_recorded", dict(card="final", wave=1, verdict="clean", findings="/r/y.md",
                                  dispatch_id="run-w1-final_review-1")),
        ("accept_started", dict(card="final", wave=1, target=h,
                                dispatch_id="run-w1-final_review-1")),
        ("accepted", dict(card="final", wave=1, sha=h, dispatch_id="run-w1-final_review-1")),
        ("outcome", dict(outcome="complete", qualifier="", reasons=[], next_step="none", head=h)),
        ("report_written", dict(report="/r/report.md", outcome="complete", outcome_seq=10)),
        ("base_worktree_added", dict(path="/w/b--base")),
        ("base_worktree_ready", dict(path="/w/b--base")),
    ))

    # The Rust test's two rulings (as it writes them, `ts` fixed), then the reference's event.
    with tempfile.TemporaryDirectory() as tmp, patched(state, utc_now=lambda: FIXED_TS):
        paths = state.run_paths(Path(tmp))
        paths.events.write_text("".join(
            dumps({"seq": i, "ts": FIXED_TS, "kind": "ruling", "card": None, "dispatch_id": None,
                   "text": text}) + "\n" for i, text in ((1, "one"), (2, "two"))))
        state.append_event(paths, "paused", by="operator")
        write("state/seq-after-two.jsonl", paths.events.read_bytes())
        write("state/seq-after-two-read.txt", f"{len(state.read_events(paths))}\n".encode())

    with tempfile.TemporaryDirectory() as tmp:
        paths = state.run_paths(Path(tmp))
        state.append_attempt(paths, {"card": 1, "attempt": 3})
        write("state/attempt-3.jsonl", paths.attempts.read_bytes())

    claude = {"dispatch": "c1-a1-implement-1", "run": "abc123", "card": 1, "attempt": 1,
              "role": "implementer", "vendor": "claude", "model": "sonnet", "effort": "high",
              "agent": "leadhand:implementer", "worker_id": "wé",
              "started_at": "2026-11-14T00:00:00.000Z", "ended_at": "2026-11-14T00:01:00.000Z",
              "steps": None, "charge_ids": ["c1-a1-implement-1:claude:wé"],
              "usage": {"input": 3, "cached": 0, "cache_write_5m": 0, "cache_write_1h": 0,
                        "output": 10, "reasoning": 0, "known": True, "model": "claude-opus-4-1"},
              "usage_known": True, "cost_estimate": 1e-05, "exit_reason": "completed",
              "status": "READY", "blocked_by": "", "recorded_by": "engine"}
    codex = {"dispatch": "c1-a2-implement-1", "run": None, "card": None, "attempt": 2,
             "role": "implementer", "vendor": "codex", "model": "gpt-6.1-terra", "effort": "high",
             "worker_id": None, "started_at": "2026-11-14T00:00:00.000Z",
             "ended_at": "2026-11-14T00:01:00.000Z", "steps": 4,
             "charge_ids": ["c1-a2-implement-1:codex:unknown"], "usage": None,
             "usage_known": False, "usage_source": "unknown", "cost_estimate": None,
             "exit_reason": "interrupted", "status": "SPENT", "blocked_by": "", "generation": 1,
             "possibly_incomplete": True}
    with tempfile.TemporaryDirectory() as tmp:
        paths = state.run_paths(Path(tmp))
        state.append_attempt(paths, claude)
        state.append_attempt(paths, codex)
        write("state/attempts.jsonl", paths.attempts.read_bytes())
        got = state.read_attempts(paths)
        same = f"{got == [claude, codex]} {[list(a) for a in got] == [list(claude), list(codex)]}\n"
        write("state/attempts-read.txt", same.encode())

    roots = [".", "", "./run dir/", "a//b/./c", "/runs/r 1", "//r"]
    names = [f.name for f in dc_fields(state.RunPaths)]
    write("state/run-paths.json", sections({
        "run_paths": mapped(lambda r: [str(getattr(state.run_paths(r), n)) for n in names], roots),
    }))


# --------------------------------------------------------------------------
# usage/: charges and the vendors' usage records (`usage::tests`)

FROZEN_ROLLOUT = OUT / "usage" / "codex-rollout" / "codex-rollout-interrupted.jsonl"


def line_of(value) -> str:
    return json.dumps(value, separators=(",", ":"))


def usage_fixture() -> None:
    report_frozen(FROZEN_ROLLOUT.parent, ROOT / "tests" / "fixtures", "*.jsonl")

    # A charge of a one-message transcript, as `Case::claude(1, ..)` makes it.
    with tempfile.TemporaryDirectory() as tmp, patched(u, utc_now=lambda: FIXED_TS):
        tmp = Path(tmp)
        (tmp / "run with spaces").mkdir()
        paths = state.run_paths(tmp / "run with spaces")
        f = tmp / "t.jsonl"
        f.write_text(claude_line("m1", 10, 3, 0, 0, 0) + "\n", encoding="utf-8")
        u.charge(paths, charge_id="d1:claude:agent1", source="claude", key="agent1",
                 dispatch_id="d1", role="implementer", path_or_thread=f)
        (c,) = u.read_charges(paths)
        record = paths.charges.read_text(encoding="ascii")
        assert record == line_of(u._charge_to_record(c)) + "\n", record
        checkpoints = (json.dumps(u.charged_checkpoint([c], "claude", "agent1")) + " "
                       + json.dumps(u.charged_checkpoint([c], "claude", "other")) + "\n")
    write("usage/charge-record.txt", (record + checkpoints).encode("ascii"))

    s = u.Usage()
    for x in (u.Usage(input=1, output=10, model="claude-opus-4-1"),
              u.Usage(input=2, output=20, model="claude-opus-4-1"), u.Usage(model="<synthetic>")):
        s = u.add(s, x)
    write("usage/single-model.json", (line_of(u.usage_to_dict(s)) + "\n").encode("ascii"))

    a = u.Usage(input=1000, output=100, model="claude-opus-4")
    b = u.Usage(input=2000, output=200, model="claude-sonnet-4-5")
    write("usage/serialized.jsonl", "".join(
        line_of(u.usage_to_dict(x)) + "\n" for x in (a, u.add(a, b), u.UNKNOWN)).encode("ascii"))

    p = {"opus": {"input": 15.0, "output": 75.0, "cache_read": 1.5, "cache_write_5m": 18.75,
                  "cache_write_1h": 30.0}}
    x = u.Usage(input=1234, cached=56789, cache_write_5m=1011, cache_write_1h=1213, output=1415,
                reasoning=99)
    write("usage/estimate-cost.txt",
          f"{u.estimate_cost(x, 'claude-Opus-4', p)!r} {u.estimate_cost(x, 'gpt-5', p)}\n"
          .encode("ascii"))

    # The Claude records of `claude::tests::records_read_as_the_python_reads_them`.
    lines = [
        r'{"type":"assistant","message":{"id":"legacy","usage":{"input_tokens":1,"cache_creation_input_tokens":7,"output_tokens":2}}}',
        r'{"type":"assistant","message":{"id":"types","model":7,"usage":{"input_tokens":true,"cache_read_input_tokens":"9","output_tokens":3.9,"cache_creation":{"ephemeral_5m_input_tokens":-4,"ephemeral_1h_input_tokens":2}}}}',
        r'{"type":"user","type":"assistant","message":{"id":"a\udcffb","usage":{"output_tokens":1,"output_tokens":5}}}',
        r'{"type":"assistant","message":{"id":"","usage":{"output_tokens":1}}}',
        r'{"type":"assistant","message":{"id":"nousage","usage":[1]}}',
        r'{"type":"system","message":{"id":"sys","usage":{"output_tokens":1}}}',
        r'{"type":["assistant"],"message":{"id":"list","usage":{"output_tokens":1}}}',
        '\x0c{"type":"assistant","message":{"id":"cr","usage":{"output_tokens":6}}}\r'
        '{"type":"assistant","message":{"id":"crlf","usage":{"output_tokens":8}}}\r',
        '{"type":"assistant","x":"\xff","message":{"id":"bad","usage":{"output_tokens":2}}}',
    ]
    transcript = "\n".join(lines).encode("latin-1")
    with tempfile.TemporaryDirectory() as tmp:
        f = Path(tmp) / "odd.jsonl"
        f.write_bytes(transcript)
        messages = "".join(f"{json.dumps(mid)} {line_of(u.usage_to_dict(x))}\n"
                           for mid, x in u._claude_messages(str(f)).items())
    write("usage/claude-records/transcript.jsonl", transcript)
    write("usage/claude-records/messages.txt", messages.encode("ascii"))

    # The rollout of `codex::tests::rollout_reads_as_the_python_reads_it`: the frozen fixture's
    # lines (as Rust's `str::lines` gives them), and four more.
    base = FROZEN_ROLLOUT.read_text(encoding="utf-8").split("\n")
    if base and base[-1] == "":
        base.pop()
    base = [line.removesuffix("\r") for line in base]
    rollout = base + [
        base[16],
        '{"type":"token_usage_record","payload":{"response_id":7,"usage":{"input_tokens":5}}}',
        '{"type":"token_usage_record","payload":[1]}',
        r'{"type":"token_usage_record","payload":{"response_id":"r\udcff","usage":{"input_tokens":5,"cached_input_tokens":9}}}',
    ]
    with tempfile.TemporaryDirectory() as tmp:
        f = Path(tmp) / "rollout-2026-11-10T00-00-00-thr1.jsonl"
        f.write_text("".join(line + "\n" for line in rollout), encoding="utf-8")
        r = u._codex_rollout(str(f))

        def d(v):
            return None if v is None else u.usage_to_dict(v)

        read = "".join(f"{line_of(rid)} {line_of(d(v))}\n" for rid, v in r.responses.items())
        read += line_of(d(r.thread_total)) + "\n" + line_of(d(r.token_count_total)) + "\n"
    write("usage/codex-rollout/read.txt", read.encode("ascii"))


def main() -> None:
    procs_samples()
    events_with_raw_bytes()
    charge_checkpoint()
    usage_mixed()
    worker_prompts()
    codex_argv_table()
    cli_version()
    pytext_fixture()
    wire_fixture()
    deck_fixture()
    config_fixture()
    cli_check_json()
    state_fixture()
    usage_fixture()
    for path in sorted(WRITTEN):
        print(f"{path.stat().st_size:>8}  {path.relative_to(OUT)}")


if __name__ == "__main__":
    main()
