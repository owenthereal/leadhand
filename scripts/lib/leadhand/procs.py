"""The process table, read without `ps` where possible.

Identity of a process is (pid, start time): a pid alone can be reused, so code
that signals a recorded process first checks that the pid still has the start
time it was recorded with (`same_process`).

Sources, in order:
- Linux: `/proc/<pid>/stat` (start time in clock ticks since boot).
- macOS: `sysctl` `KERN_PROC_PID` / `KERN_PROC_ALL` through ctypes (start time
  as seconds.microseconds). Claude Code's sandbox refuses `ps` but allows this.
- Anything else, or if those fail: `ps` (`lstart`, second resolution).

A start time is an opaque string, only ever compared for equality with a value
the same function produced on the same machine. Zombies (exited, not yet
reaped) count as gone: they cannot run or write anything.
"""
from __future__ import annotations

import ctypes
import os
import struct
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

PS_TIMEOUT = 10.0


@dataclass(frozen=True)
class ProcInfo:
    pid: int
    ppid: int
    pgid: int
    start: str
    zombie: bool = False

    def record(self) -> dict:
        return {"pid": self.pid, "pgid": self.pgid, "start": self.start}


# --------------------------------------------------------------------------
# Linux: /proc

def _proc_stat(pid: int) -> ProcInfo | None:
    try:
        raw = Path(f"/proc/{pid}/stat").read_bytes().decode("utf-8", "replace")
    except (FileNotFoundError, ProcessLookupError, PermissionError, NotADirectoryError):
        return None
    # The command name (field 2) may contain spaces and parentheses.
    rest = raw[raw.rfind(")") + 2:].split()
    if len(rest) < 20:
        return None
    state, ppid, pgid, start = rest[0], int(rest[1]), int(rest[2]), rest[19]
    return ProcInfo(pid, ppid, pgid, f"ticks:{start}", zombie=state in ("Z", "X"))


def _proc_all() -> list[ProcInfo]:
    out = []
    for name in os.listdir("/proc"):
        if name.isdigit():
            info = _proc_stat(int(name))
            if info is not None:
                out.append(info)
    return out


# --------------------------------------------------------------------------
# macOS: sysctl kern.proc (struct kinfo_proc, 648 bytes on 64-bit Darwin)

_CTL_KERN, _KERN_PROC, _KERN_PROC_ALL, _KERN_PROC_PID = 1, 14, 0, 1
_KINFO_SIZE = 648
_OFF_START = 0      # kp_proc.p_un.__p_starttime: struct timeval {int64 sec; int32 usec}
_OFF_STAT = 36      # kp_proc.p_stat (char)
_OFF_PID = 40       # kp_proc.p_pid
_OFF_PPID = 560     # kp_eproc.e_ppid
_OFF_PGID = 564     # kp_eproc.e_pgid
_SZOMB = 5

_libc = None


def _sysctl(mib: list[int], size: int | None) -> bytes | None:
    """sysctl(mib); with `size` None, ask for the size first. None on failure."""
    global _libc
    if _libc is None:
        _libc = ctypes.CDLL(None, use_errno=True)
    arr = (ctypes.c_int * len(mib))(*mib)
    for _ in range(5):
        if size is None:
            n = ctypes.c_size_t(0)
            if _libc.sysctl(arr, len(mib), None, ctypes.byref(n), None, ctypes.c_size_t(0)) != 0:
                return None
            want = n.value + n.value // 8 + _KINFO_SIZE * 16
        else:
            want = size
        buf = ctypes.create_string_buffer(want)
        n = ctypes.c_size_t(want)
        if _libc.sysctl(arr, len(mib), buf, ctypes.byref(n), None, ctypes.c_size_t(0)) == 0:
            return buf.raw[:n.value]
        if ctypes.get_errno() != 12 or size is not None:  # ENOMEM: the table grew; retry
            return None
    return None


def _kinfo(raw: bytes, offset: int) -> ProcInfo:
    sec, usec = struct.unpack_from("qi", raw, offset + _OFF_START)
    (stat,) = struct.unpack_from("b", raw, offset + _OFF_STAT)
    (pid,) = struct.unpack_from("i", raw, offset + _OFF_PID)
    ppid, pgid = struct.unpack_from("ii", raw, offset + _OFF_PPID)
    return ProcInfo(pid, ppid, pgid, f"{sec}.{usec:06d}", zombie=stat == _SZOMB)


def _darwin_pid(pid: int) -> ProcInfo | None | bool:
    """ProcInfo, None when the pid does not exist, False when sysctl failed."""
    raw = _sysctl([_CTL_KERN, _KERN_PROC, _KERN_PROC_PID, pid], _KINFO_SIZE)
    if raw is None:
        return False
    if len(raw) < _KINFO_SIZE:
        return None
    info = _kinfo(raw, 0)
    return info if info.pid == pid else None


def _darwin_all() -> list[ProcInfo] | None:
    raw = _sysctl([_CTL_KERN, _KERN_PROC, _KERN_PROC_ALL], None)
    if raw is None:
        return None
    return [_kinfo(raw, off) for off in range(0, len(raw) - _KINFO_SIZE + 1, _KINFO_SIZE)]


# --------------------------------------------------------------------------
# Fallback: ps

def _ps(args: list[str]) -> str | None:
    try:
        r = subprocess.run(["ps", *args], capture_output=True, text=True, timeout=PS_TIMEOUT,
                           stdin=subprocess.DEVNULL)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return r.stdout if r.returncode == 0 else None


def _ps_all() -> list[ProcInfo]:
    out = _ps(["-axo", "pid=,ppid=,pgid=,stat=,lstart="]) or ""
    procs = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) < 5 or not all(p.isdigit() for p in parts[:3]):
            continue
        pid, ppid, pgid = (int(p) for p in parts[:3])
        procs.append(ProcInfo(pid, ppid, pgid, "lstart:" + " ".join(parts[4:]),
                              zombie=parts[3].startswith("Z")))
    return procs


def _ps_one(pid: int) -> ProcInfo | None:
    out = _ps(["-o", "pid=,ppid=,pgid=,stat=,lstart=", "-p", str(pid)])
    parts = (out or "").split()
    if len(parts) < 5 or not all(p.isdigit() for p in parts[:3]):
        return None
    return ProcInfo(int(parts[0]), int(parts[1]), int(parts[2]), "lstart:" + " ".join(parts[4:]),
                    zombie=parts[3].startswith("Z"))


# --------------------------------------------------------------------------
# Public

def process_info(pid: int) -> ProcInfo | None:
    """The process's (pid, ppid, pgid, start, zombie), or None if there is none."""
    if not isinstance(pid, int) or pid <= 0:
        return None
    if sys.platform.startswith("linux") and os.path.isdir("/proc/self"):
        return _proc_stat(pid)
    if sys.platform == "darwin":
        got = _darwin_pid(pid)
        if got is not False:
            return got
    return _ps_one(pid)


def process_start_time(pid: int) -> str | None:
    """The process's start time as an opaque string; None if it is gone or a zombie."""
    info = process_info(pid)
    return None if info is None or info.zombie else info.start


def list_processes() -> list[ProcInfo]:
    if sys.platform.startswith("linux") and os.path.isdir("/proc/self"):
        return _proc_all()
    if sys.platform == "darwin":
        got = _darwin_all()
        if got is not None:
            return got
    return _ps_all()


def descendants(pid: int) -> list[ProcInfo]:
    """Every live process below `pid` in the ppid tree (not `pid` itself)."""
    children: dict[int, list[ProcInfo]] = {}
    for p in list_processes():
        if p.pid != p.ppid:
            children.setdefault(p.ppid, []).append(p)
    out, todo, seen = [], [pid], {pid}
    while todo:
        for child in children.get(todo.pop(), []):
            if child.pid not in seen:
                seen.add(child.pid)
                todo.append(child.pid)
                if not child.zombie:
                    out.append(child)
    return out


def same_process(pid: int | None, start: str | None) -> bool:
    """True when `pid` is alive (not a zombie) and still has start time `start`."""
    if not pid or not start:
        return False
    return process_start_time(int(pid)) == start
