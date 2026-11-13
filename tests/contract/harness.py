"""The contract suite's harness: it reaches leadhand only as a subprocess at LEADHAND_BIN.

Run the suite from the repository root:

    LEADHAND_BIN=scripts/leadhand PYTHONPATH=tests python3 -B -m unittest discover -s tests/contract -p 'contract_*.py'

- LEADHAND_BIN: the binary under test (default <repo>/scripts/leadhand). A relative value
  is resolved against the starting directory, once, at import.
- LEADHAND_IMPL: "python" (default), "rust" or "mixed"; with LEADHAND_RUST_COMMANDS (a
  comma-separated list of commands, or "all") it decides which tagged tests run.

Nothing here imports the leadhand package.
"""
from __future__ import annotations

import ctypes
import json
import os
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import unittest
from dataclasses import dataclass
from pathlib import Path

import gitfixture

ROOT = Path(__file__).resolve().parents[2]
BIN = Path(os.path.abspath(os.environ.get("LEADHAND_BIN") or ROOT / "scripts" / "leadhand"))
IMPL = os.environ.get("LEADHAND_IMPL") or "python"
if IMPL not in ("python", "rust", "mixed"):
    raise ValueError(f"LEADHAND_IMPL must be python, rust or mixed, not {IMPL!r}")
RUST_COMMANDS = frozenset(n.strip() for n in os.environ.get("LEADHAND_RUST_COMMANDS", "").split(",") if n.strip())
FAKES = ROOT / "tests" / "fakes"
COMMANDS = ("cancel", "check", "dispatch", "dispatch-runner", "launch-line", "next", "pause", "record", "report",
            "route", "run", "start", "status", "stop-dispatch", "timer", "unpark", "version")

# The suite's own settings, passed through to the binary (the stage-2 router reads them).
_SUITE_VARS = ("LEADHAND_BIN", "LEADHAND_IMPL", "LEADHAND_RUST_COMMANDS")
# Inherited variables that would steer the binary or the fakes: other LEADHAND_* names are
# test hooks (LEADHAND_CRASH_*, LEADHAND_TEST_*, LEADHAND_INSTALLED_VENDORS,
# LEADHAND_MINUTE_SECONDS) or set by leadhand itself (the run anchor, lock fds, dispatch tag).
_INHERITED_PREFIXES = ("LEADHAND_", "FAKE_CODEX_", "FAKE_CLAUDE_")
_INHERITED_VARS = ("CLAUDE_CODE_SESSION_ID",)


# --------------------------------------------------------------------------
# Tags

def reference_only(obj):
    """Python behaviour that Rust deliberately changes: runs only against the all-Python CLI."""
    return unittest.skipUnless(IMPL == "python" and not RUST_COMMANDS,
                               "reference-only: needs LEADHAND_IMPL=python and no LEADHAND_RUST_COMMANDS")(obj)


def rust_only(*, uses: tuple[str, ...]):
    """New Rust behaviour: runs once every command in `uses` is in LEADHAND_RUST_COMMANDS."""
    return unittest.skipUnless("all" in RUST_COMMANDS or set(uses) <= RUST_COMMANDS,
                               f"rust-only: needs {', '.join(uses)} in LEADHAND_RUST_COMMANDS")


# --------------------------------------------------------------------------
# Running the binary

@dataclass
class Result:
    argv: list[str]
    code: int
    stdout: str
    stderr: str

    def __str__(self) -> str:
        return (f"argv: {shlex.join(self.argv)}\n"
                f"exit code: {self.code}\n"
                f"stdout:\n{self.stdout or '(empty)'}\n"
                f"stderr:\n{self.stderr or '(empty)'}")

    def ok(self) -> Result:
        if self.code != 0:
            raise AssertionError(f"leadhand failed: exit code {self.code}, expected 0\n{self}")
        return self

    def json(self):
        try:
            return json.loads(self.stdout)
        except json.JSONDecodeError as e:
            raise AssertionError(f"leadhand's stdout is not JSON ({e})\n{self}") from None


def run_cli(*args, env: dict, cwd: Path, stdin: str | None = None, timeout: float = 120.0) -> Result:
    """Run BIN with `args`. A run past `timeout` seconds kills the binary's whole process group
    (only the binary, in a crash drive: see run_argv) and fails the test with the command named."""
    return run_argv([str(BIN), *(str(a) for a in args)], env=env, cwd=cwd, stdin=stdin, timeout=timeout)


def run_argv(argv: list[str], *, env: dict, cwd: Path, stdin: str | None = None,
             timeout: float = 120.0) -> Result:
    """Run a full command line, such as a dispatch line the binary printed, as run_cli runs BIN.

    The command gets its own process group, so a timeout kills the children that hold the
    pipes too. In a crash drive (`env` names LEADHAND_CRASH_KILL_PGID) it stays in the caller's
    group instead, so a crash point's group kill takes it down with the drive."""
    own_group = "LEADHAND_CRASH_KILL_PGID" not in env
    proc = subprocess.Popen(
        argv, cwd=cwd, env=env, text=True, encoding="utf-8", errors="surrogateescape",
        stdin=subprocess.DEVNULL if stdin is None else subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        process_group=0 if own_group else None,
    )
    try:
        out, err = proc.communicate(stdin, timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            if own_group:
                os.killpg(proc.pid, signal.SIGKILL)
            else:
                proc.kill()
        except ProcessLookupError:
            pass
        try:
            out, err = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:      # a descendant left the group and holds the pipes
            proc.kill()
            proc.wait()
            out, err = "", "(not read: a process outside the binary's group held the pipes)"
        killed = "its process group was" if own_group else "it was"
        raise AssertionError(f"leadhand timed out after {timeout:g} s; {killed} killed\n"
                             f"{Result(argv, proc.returncode, out, err)}") from None
    return Result(argv, proc.returncode, out, err)


# --------------------------------------------------------------------------
# Temporary roots and the sealed environment

def temp_root(test: unittest.TestCase, prefix: str) -> Path:
    """A new directory in the system temp dir, its name containing a space; removed on cleanup."""
    root = Path(tempfile.mkdtemp(prefix=prefix if " " in prefix else prefix + " "))
    test.addCleanup(shutil.rmtree, root, True)
    return root


def _inherited(name: str) -> bool:
    if name in _SUITE_VARS:
        return False
    return name in _INHERITED_VARS or name.startswith(_INHERITED_PREFIXES)


def hermetic_env(test: unittest.TestCase, tmp: Path, *, installed=("claude",), config_text: str = "") -> dict:
    """The environment for the binary: the process's own, with Git sealed off from the operator's
    configuration (gitfixture, which also lets the test's own Git helpers work under `tmp`),
    inherited hooks dropped, and leadhand's configuration, Claude and Codex homes under `tmp`."""
    gitfixture.hermetic(test, tmp)
    env = {k: v for k, v in os.environ.items() if k not in gitfixture.LEAKY_VARS and not _inherited(k)}
    env.update(gitfixture.HERMETIC_ENV)
    config = tmp / "user config.toml"
    config.write_text(f'docs_dir = "{tmp / "docs dir"}"\n{config_text}', encoding="utf-8")
    env.update({
        "LEADHAND_TESTING": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "LEADHAND_CONFIG": str(config),
        "CLAUDE_CONFIG_DIR": str(tmp / "claude home"),
        "CODEX_HOME": str(tmp / "codex home"),
        "LEADHAND_INSTALLED_VENDORS": ",".join(installed),
    })
    return env


# --------------------------------------------------------------------------
# The binary's own path (ruling S1-R11)
#
# Where the binary prints or passes a path for itself (an unpark hint, the launch line, a
# dispatch line's argv[0], LEADHAND_CLI), parity is the file it names, not its spelling.

def names_bin(path_text: str) -> bool:
    """True when `path_text` names the binary under test: the same file as BIN once both are resolved."""
    return os.path.realpath(path_text) == os.path.realpath(BIN)


def is_bin_command(text: str, before: str, after: str) -> bool:
    """True when `text` is exactly `before`, then one shell word naming BIN (names_bin), then
    exactly `after`."""
    if not (text.startswith(before) and text.endswith(after) and len(text) >= len(before) + len(after)):
        return False
    word = text[len(before):len(text) - len(after)]
    if not word or word != word.strip():
        return False
    try:
        words = shlex.split(word)
    except ValueError:
        return False
    return len(words) == 1 and names_bin(words[0])


# --------------------------------------------------------------------------
# Reading what the binary wrote

def read_jsonl(path: Path) -> list[dict]:
    """Every complete line of a JSON-lines file; a trailing partial line (a write in progress)
    is skipped."""
    text = path.read_text(encoding="utf-8")
    lines = text.split("\n")
    return [json.loads(line) for line in lines[:-1] if line.strip()]


def read_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


# --------------------------------------------------------------------------
# Process identity, without `ps` (copied from scripts/lib/leadhand/procs.py)
#
# Identity is (pid, start time); a start time is an opaque string in the same format the
# CLI records. Zombies count as gone.

_CTL_KERN, _KERN_PROC, _KERN_PROC_PID = 1, 14, 1
_KINFO_SIZE = 648   # struct kinfo_proc on 64-bit Darwin
_OFF_START = 0      # kp_proc.p_un.__p_starttime: struct timeval {int64 sec; int32 usec}
_OFF_STAT = 36      # kp_proc.p_stat (char)
_OFF_PID = 40       # kp_proc.p_pid
_SZOMB = 5

_libc = None


def _linux_start(pid: int) -> str | None:
    try:
        raw = Path(f"/proc/{pid}/stat").read_bytes().decode("utf-8", "replace")
    except (FileNotFoundError, ProcessLookupError, PermissionError, NotADirectoryError):
        return None
    # The command name (field 2) may contain spaces and parentheses.
    rest = raw[raw.rfind(")") + 2:].split()
    if len(rest) < 20 or rest[0] in ("Z", "X"):
        return None
    return f"ticks:{rest[19]}"


def _darwin_start(pid: int) -> str | None:
    global _libc
    if _libc is None:
        _libc = ctypes.CDLL(None, use_errno=True)
    mib = (ctypes.c_int * 4)(_CTL_KERN, _KERN_PROC, _KERN_PROC_PID, pid)
    buf = ctypes.create_string_buffer(_KINFO_SIZE)
    n = ctypes.c_size_t(_KINFO_SIZE)
    if _libc.sysctl(mib, 4, buf, ctypes.byref(n), None, ctypes.c_size_t(0)) != 0:
        raise OSError(ctypes.get_errno(), f"sysctl kern.proc.pid.{pid} failed")
    raw = buf.raw[:n.value]
    if len(raw) < _KINFO_SIZE or struct.unpack_from("i", raw, _OFF_PID)[0] != pid:
        return None
    sec, usec = struct.unpack_from("qi", raw, _OFF_START)
    (stat,) = struct.unpack_from("b", raw, _OFF_STAT)
    return None if stat == _SZOMB else f"{sec}.{usec:06d}"


def _start_time(pid: int) -> str | None:
    """The process's start time; None if it is gone or a zombie."""
    if pid <= 0:
        return None
    if sys.platform.startswith("linux"):
        return _linux_start(pid)
    if sys.platform == "darwin":
        return _darwin_start(pid)
    raise OSError(f"no process table reader for {sys.platform}")


def same_process(pid: int, start: str | None) -> bool:
    """True when `pid` is alive (not a zombie) and still has start time `start`."""
    if not pid or not start:
        return False
    return _start_time(int(pid)) == start
