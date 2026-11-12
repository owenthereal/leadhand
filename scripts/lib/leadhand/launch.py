"""The launcher: `leadhand run` owns a run and starts its Claude Code supervisor.

Ownership before mutation. `run` takes the run's ownership lock first and
changes nothing in the run directory until it has it. The lock is an exclusive
`flock` on `run.lock` (the long-held run lock, not the short-held `state.lock`).
A launch is also refused while `holder.json` names a supervisor whose pid and
process start time still match, so a replacement can never start while an
earlier supervisor lives, even if that supervisor no longer holds the flock.

Ownership while the supervisor lives. The locked descriptor is passed to the
Claude child (`pass_fds`), so the lock stays held for the session's life even if
the launcher dies. When the child exits, the launcher releases the lock. An
in-place run (its workspace is the repository's checkout) also claims the
checkout itself (`CheckoutClaim`, R53) before its supervisor starts, and passes
that descriptor to the child the same way. Only
once it owns the run does it append `resumed`, and only when the run has events
beyond `run_started` (an earlier supervisor worked it); a fresh run's first
launch appends nothing. A run whose latest outcome is complete, incomplete
(including cancelled) or failed is refused: only a blocked or unfinished run
continues.

The child runs in the run's workspace (from `run.json`), whatever directory the
launch came from, with `LEADHAND_RUN_ID`, `LEADHAND_RUN_DIR` and `LEADHAND_CLI`
set. Its argv is `launch_argv`: the supervisor agent, the supervisor route's
model and effort from the configuration (agent frontmatter `effort` is ignored
for a main-session agent), `--permission-mode auto`, and a `--settings` overlay
whose only entry excludes the leadhand CLI from Claude Code's sandbox (the
supervisor's CLI calls write the run directory and start Codex). Whether that
overlay merges with or replaces the operator's own `sandbox.excludedCommands`
is not established, so the overlay holds nothing else.
"""
from __future__ import annotations

import argparse
import errno
import fcntl
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from . import procs
from .config import ConfigError, load_config, load_run_config
from .gitops import GitError, git
from .state import RunPaths, append_event, read_events, run_paths, write_json_atomic

SUPERVISOR_AGENT = "leadhand:supervisor"
CLI_PATH = Path(__file__).resolve().parents[2] / "leadhand"   # scripts/leadhand
EXIT_REFUSED = 3
LOCK_FD_ENV = "LEADHAND_LOCK_FD"
CHECKOUT_FD_ENV = "LEADHAND_CHECKOUT_FD"
CHECKOUT_RECORD = "leadhand-checkout.json"      # in the checkout's git dir
PLATFORM = sys.platform        # read at launch time; tests inject another platform
KEEP_AWAKE_TIMEOUT = 10.0
FORWARDED_SIGNALS = (signal.SIGINT, signal.SIGTERM)
# `run --print` (hidden; end-to-end drivers only): a headless session whose stream goes
# to the launcher's stdout. The prompt stays the last argument.
PRINT_ARGS = ("-p", "--output-format", "stream-json", "--verbose")

ATTENDED_WARNING = (
    "leadhand: attended use only. Until the Claude-side run guard lands, keep this session "
    "in view and run it only on fixture and trial repositories.\n"
    "leadhand: the supervisor runs with --permission-mode auto. Its --settings overlay holds one "
    "sandbox.excludedCommands entry, the leadhand CLI ({cli} *); whether it merges with or "
    "replaces your own excludedCommands is not established."
)
SPACES_WARNING = (
    "leadhand: warning: the leadhand CLI path contains whitespace ({cli}); the sandbox exclusion "
    "may not match; install leadhand under a path without spaces."
)


class LaunchError(RuntimeError):
    pass


class OwnershipError(RuntimeError):
    """Someone else owns the run. `holder` is holder.json's content (maybe empty)."""

    def __init__(self, holder: dict, run_id: str, *, supervisor_alive: bool = True):
        self.holder = holder
        run = holder.get("run_id") or run_id
        pid = holder.get("supervisor_pid")
        if supervisor_alive:
            pid = pid or holder.get("launcher_pid") or "unknown"
            since = holder.get("started_at") or "unknown"
            msg = f"Run already in progress: supervisor pid {pid} since {since} (run {run})"
        elif pid:
            msg = (f"Run already in progress: lock held by another process "
                   f"(supervisor pid {pid} recorded, not running) (run {run})")
        else:
            msg = (f"Run already in progress: lock held by another process "
                   f"(no supervisor recorded) (run {run})")
        super().__init__(msg)


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def process_start_time(pid: int) -> str | None:
    """The process's start time as an opaque string; None if it is gone.

    Delegates to `procs.process_start_time` (/proc on Linux, sysctl on macOS),
    which falls back to `ps -o lstart= -p <pid>` elsewhere. `ps` itself is
    refused inside Claude Code's sandbox.
    """
    return procs.process_start_time(pid)


class RunLock:
    def __init__(self, paths: RunPaths, run_id: str):
        self.paths = paths
        self.run_id = run_id
        self.fd: int | None = None

    def _holder(self) -> dict:
        try:
            data = json.loads(self.paths.holder.read_text(encoding="utf-8"))
        except (FileNotFoundError, ValueError, UnicodeDecodeError):
            return {}
        return data if isinstance(data, dict) else {}

    @staticmethod
    def _supervisor_alive(holder: dict) -> bool:
        pid = holder.get("supervisor_pid")
        start = holder.get("supervisor_start")
        if not isinstance(pid, int) or isinstance(pid, bool) or not isinstance(start, str):
            return False
        return procs.same_process(pid, start)

    def acquire(self) -> int:
        """Lock run.lock and return its descriptor, or raise OwnershipError.

        holder.json is checked before run.lock is opened (so a refusal creates
        nothing) and again once the flock is held (the authoritative check).
        """
        holder = self._holder()
        if self._supervisor_alive(holder):
            raise OwnershipError(holder, self.run_id)
        fd = os.open(self.paths.lock, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as e:
            os.close(fd)
            if e.errno in (errno.EWOULDBLOCK, errno.EAGAIN, errno.EACCES):
                holder = self._holder()
                raise OwnershipError(holder, self.run_id,
                                     supervisor_alive=self._supervisor_alive(holder)) from None
            raise
        holder = self._holder()
        if self._supervisor_alive(holder):
            fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)
            raise OwnershipError(holder, self.run_id)
        self.fd = fd
        return fd

    def record_supervisor(self, pid: int, *, plugin_dir: Path | None = None) -> None:
        """Write holder.json: launcher pid and start, supervisor pid and start, run_id,
        started_at, and plugin_dir when the session loads the plugin from a directory."""
        me = os.getpid()
        holder = {
            "run_id": self.run_id,
            "launcher_pid": me,
            "launcher_start": process_start_time(me),
            "supervisor_pid": pid,
            "supervisor_start": process_start_time(pid),
            "started_at": _utc_now(),
        }
        if plugin_dir is not None:
            holder["plugin_dir"] = str(plugin_dir)
        write_json_atomic(self.paths.holder, holder)

    def detach(self) -> None:
        """Close this process's descriptor without unlocking: the supervisor's
        inherited copy keeps the lock held until it exits."""
        if self.fd is not None:
            fd, self.fd = self.fd, None
            os.close(fd)

    def release(self) -> None:
        """Unlock and close. Unlocking drops the lock for every holder of the open
        file description, so call this only once the supervisor has exited."""
        if self.fd is None:
            return
        fd, self.fd = self.fd, None
        try:
            fcntl.flock(fd, fcntl.LOCK_UN)
        finally:
            os.close(fd)


class CheckoutBusy(RuntimeError):
    """Another in-place run holds the checkout. `holder` is the run directory it recorded,
    or None when no readable record names it."""

    def __init__(self, repo: Path, holder: str | None):
        self.holder = holder
        super().__init__(f"another in-place run of {repo} holds the checkout: {holder or 'unknown'}")


class CheckoutClaim:
    """An in-place run's exclusive claim on its repository's checkout (R53): an exclusive
    `flock` on the checkout's top directory itself, so it holds whichever runs folder a run
    lives in, and two launches started together cannot both get it. A worker cannot remove or
    replace that directory (it works inside it; its writable roots are the checkout and .git),
    so nothing it can delete lets a second claim in. Who holds the claim is recorded in
    <git dir>/leadhand-checkout.json, only to name the holder in a refusal: a worker can
    delete or rewrite that record, which changes the message and nothing else."""

    def __init__(self, repo: Path):
        self.repo = Path(repo)
        self.fd: int | None = None
        self.recorded: tuple[Path, str] | None = None

    def _record_path(self) -> Path | None:
        """The record's path in the checkout's own git dir, or None when `repo` is not the top
        of a Git checkout (nothing is then written)."""
        try:
            top, git_dir = git(self.repo, "rev-parse", "--show-toplevel", "--absolute-git-dir").splitlines()
        except (GitError, ValueError):
            return None
        return Path(git_dir) / CHECKOUT_RECORD if os.path.realpath(top) == os.path.realpath(self.repo) else None

    def holder(self) -> str | None:
        path = self._record_path()
        try:
            data = json.loads(path.read_text(encoding="utf-8")) if path is not None else None
        except (OSError, ValueError):
            return None
        run_dir = data.get("run_dir") if isinstance(data, dict) else None
        return run_dir if isinstance(run_dir, str) and run_dir.isprintable() else None

    def acquire(self) -> int:
        """Claim the checkout and return the locked descriptor, or raise CheckoutBusy."""
        fd = os.open(self.repo, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as e:
            os.close(fd)
            if e.errno in (errno.EWOULDBLOCK, errno.EAGAIN, errno.EACCES):
                raise CheckoutBusy(self.repo, self.holder()) from None
            raise
        self.fd = fd
        return fd

    def record(self, run_dir: Path) -> None:
        """Name the run holding the claim (best effort; call only while holding it)."""
        path = self._record_path()
        if path is None:
            return
        try:
            write_json_atomic(path, {"run_dir": str(run_dir)})
        except OSError:
            return
        self.recorded = (path, str(run_dir))

    def detach(self) -> None:
        """Close this process's descriptor without unlocking (see RunLock.detach)."""
        if self.fd is not None:
            fd, self.fd = self.fd, None
            os.close(fd)

    def release(self) -> None:
        """Remove the record while it still names this claim, then unlock and close. Call
        only once the supervisor has exited."""
        if self.fd is None:
            return
        fd, self.fd = self.fd, None
        try:
            if self.recorded is not None:
                path, run_dir = self.recorded
                try:
                    if json.loads(path.read_text(encoding="utf-8")).get("run_dir") == run_dir:
                        path.unlink()
                except (OSError, ValueError, AttributeError):
                    pass
            fcntl.flock(fd, fcntl.LOCK_UN)
        finally:
            os.close(fd)


def launch_argv(*, claude_bin: str, agent: str, first_message: str, plugin_dir: Path | None,
                settings: str | None, model: str | None = None, effort: str | None = None,
                print_mode: bool = False) -> list[str]:
    argv = [claude_bin]
    if plugin_dir is not None:
        argv += ["--plugin-dir", str(plugin_dir)]
    if settings is not None:
        argv += ["--settings", settings]
    argv += ["--agent", agent]
    if model:
        argv += ["--model", model]
    if effort:
        argv += ["--effort", effort]
    argv += ["--permission-mode", "auto"]
    if print_mode:
        argv += PRINT_ARGS
    return argv + [first_message]


def keep_awake_prefix(platform: str) -> list[str]:
    """The keep-awake command for Linux with systemd-inhibit; macOS: [] (`caffeinate
    -w` is used instead); anything else: []. It never wraps the supervisor: `run`
    starts it beside the child (`prefix + tail --pid=<child> -f /dev/null`), so
    claude stays the launcher's direct child and keeps the inherited run.lock."""
    if platform.startswith("linux") and shutil.which("systemd-inhibit"):
        return ["systemd-inhibit", "--what=idle", "--why=leadhand run"]
    return []


def cli_settings(cli: Path | None = None) -> str:
    """The --settings overlay: only the leadhand CLI's sandbox exclusion."""
    cli = CLI_PATH if cli is None else cli
    return json.dumps({"sandbox": {"excludedCommands": [f"{cli} *"]}})


def first_message(run_dir: Path, cli: Path | None = None) -> str:
    cli = CLI_PATH if cli is None else cli
    return (f"Run the leadhand deck in {run_dir}. leadhand CLI: {cli}. "
            "First invoke the Skill tool with leadhand:run, then follow it.")


def resolve_run_dir(path: Path) -> Path:
    """A run directory, or a run's deck.md (whose parent has run.json), as an absolute path."""
    p = Path(os.path.abspath(path))
    if p.is_dir() and (p / "run.json").is_file():
        return p
    if p.name == "deck.md" and p.is_file() and (p.parent / "run.json").is_file():
        return p.parent
    raise LaunchError(f"not a run directory or a run's deck.md: {p}")


def _read_meta(paths: RunPaths) -> dict:
    try:
        meta = json.loads(paths.run_json.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        raise LaunchError(f"cannot read {paths.run_json}: {e}") from None
    if not isinstance(meta, dict) or not isinstance(meta.get("run_id"), str) or not meta["run_id"]:
        raise LaunchError(f"{paths.run_json}: no run_id")
    return meta


def _start_command(meta: dict) -> str:
    """The operator's command to start a new run of this run's deck (paths quoted)."""
    cmd = [str(CLI_PATH), "start", str(meta.get("deck_source") or "<deck>")]
    if meta.get("repo"):
        cmd += ["--repo", str(meta["repo"])]
    return " ".join(shlex.quote(c) for c in cmd)


def _supervisor_route(meta: dict, root: Path | None = None) -> tuple[str | None, str | None]:
    repo = meta.get("repo")
    if not isinstance(repo, str) or not repo:
        raise LaunchError("run.json: no repo")
    # The run's configuration snapshot (I5), when the run directory is known.
    config = load_run_config(root, Path(repo)) if root is not None else load_config(Path(repo))
    role = config.roles.get("supervisor")
    route = role.routes.get("claude") if role is not None else None
    return (route.model, route.effort) if route is not None else (None, None)


def _ended(events: list[dict]) -> dict | None:
    """The outcome event that ended the run, unless a later `resumed` reopened it."""
    last = None
    for e in events:
        if e.get("kind") == "outcome":
            last = e
        elif e.get("kind") == "resumed":
            last = None
    return last


def _workspace(meta: dict) -> Path:
    ws = meta.get("workspace")
    if not isinstance(ws, str) or not ws or not os.path.isabs(ws) or not os.path.isdir(ws):
        raise LaunchError(f"run.json: workspace is not an existing absolute directory: {ws!r}")
    return Path(ws)


def _in_place(meta: dict) -> bool:
    ws, repo = meta.get("workspace"), meta.get("repo")
    return (isinstance(ws, str) and isinstance(repo, str) and bool(ws) and bool(repo)
            and os.path.realpath(ws) == os.path.realpath(repo))


def checkout_in_use(root: Path, meta: dict) -> Path | None:
    """For an in-place run (its workspace is the repository's checkout): another in-place run
    of the same repository, under the same runs folder, whose recorded supervisor is alive,
    or None. An in-place run has the checkout to itself (R53); worktree runs never collide.
    `CheckoutClaim` is what keeps out an in-place run from another runs folder, or one whose
    launch has not written holder.json yet."""
    if not _in_place(meta):
        return None
    repo, me = os.path.realpath(meta["repo"]), os.path.realpath(root)
    for rj in sorted(root.parent.glob("*/run.json")):
        other = rj.parent
        if os.path.realpath(other) == me:
            continue
        try:
            theirs = json.loads(rj.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if not isinstance(theirs, dict) or not _in_place(theirs) or os.path.realpath(theirs["repo"]) != repo:
            continue
        if RunLock._supervisor_alive(RunLock(run_paths(other), "")._holder()):
            return other
    return None


def tools_inside(workspace: Path, plugin_dir: Path | None = None) -> str | None:
    """Why a run in `workspace` must not launch, or None: the leadhand CLI (which runs outside
    the sandbox) or the plugin directory the supervisor loads lies inside the workspace, where
    workers can edit it (security review L6)."""
    ws = os.path.realpath(workspace)
    for what, path in (("the leadhand CLI", CLI_PATH), ("--plugin-dir", plugin_dir)):
        if path is None:
            continue
        real = os.path.realpath(path)
        if real == ws or real.startswith(ws.rstrip(os.sep) + os.sep):
            return (f"{what} ({path}) lies inside the run's workspace {workspace}, where workers can "
                    "edit it; install leadhand outside that repository, or run in a worktree")
    return None


def _keep_awake_argv(platform: str, pid: int) -> list[str]:
    """What runs beside the supervisor `pid` and ends with it: `caffeinate -i -w` on
    macOS, `systemd-inhibit ... tail --pid=<pid> -f /dev/null` on Linux."""
    if platform == "darwin":
        exe = shutil.which("caffeinate")
        return [exe, "-i", "-w", str(pid)] if exe else []
    prefix = keep_awake_prefix(platform)
    return prefix + ["tail", f"--pid={pid}", "-f", "/dev/null"] if prefix else []


def _start_keep_awake(platform: str, pid: int) -> subprocess.Popen | None:
    argv = _keep_awake_argv(platform, pid)
    if not argv:
        return None
    try:     # close_fds (the default): it never holds run.lock
        return subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except OSError:
        return None


def _stop_keep_awake(proc: subprocess.Popen | None) -> None:
    if proc is None:
        return
    if proc.poll() is None:
        proc.terminate()
    try:
        proc.wait(timeout=KEEP_AWAKE_TIMEOUT)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=KEEP_AWAKE_TIMEOUT)


def run(run_dir: Path, *, plugin_dir: Path | None = None, claude_bin: str = "claude",
        print_mode: bool = False) -> int:
    """Own the run and start its supervisor; return the supervisor's exit code.

    1 when the run cannot be launched, 3 when someone else owns it. A child
    killed by a signal returns 128 + the signal number.
    """
    try:
        root = resolve_run_dir(run_dir)
        paths = run_paths(root)
        meta = _read_meta(paths)
    except LaunchError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    run_id = meta["run_id"]

    lock = RunLock(paths, run_id)
    try:
        fd = lock.acquire()
    except OwnershipError as e:
        print(str(e), file=sys.stderr)
        return EXIT_REFUSED

    claim = None
    try:
        ended = _ended(read_events(paths))
        if ended is not None and ended.get("outcome") != "blocked":
            # Only a blocked or unfinished run continues: a relaunch must never reopen a run
            # that completed, failed or was cancelled.
            qualifier = ended.get("qualifier") or ""
            print(f"error: this run has ended: {ended.get('outcome')}"
                  + (f" ({qualifier})" if qualifier else "")
                  + "; only a blocked or unfinished run continues. Start a new run from the deck "
                    f"with: {_start_command(meta)}", file=sys.stderr)
            return 1
        try:
            workspace = _workspace(meta)
            problem = tools_inside(workspace, plugin_dir)
            if problem:
                raise LaunchError(problem)
            busy = checkout_in_use(root, meta)
            if busy is not None:
                raise LaunchError(f"another in-place run of {meta['repo']} is being supervised: {busy}; "
                                  "an in-place run has the checkout to itself, so wait for that "
                                  "supervisor to stop")
            if _in_place(meta):
                # Atomic, and wherever the other run's folder lives (R53).
                claim = CheckoutClaim(Path(meta["repo"]))
                try:
                    claim.acquire()
                except CheckoutBusy as e:
                    claim = None
                    raise LaunchError(f"another in-place run of {meta['repo']} is being supervised: "
                                      f"{e.holder or 'unknown'}; an in-place run has the checkout to "
                                      "itself, so wait for that supervisor to stop") from None
                claim.record(root)
            model, effort = _supervisor_route(meta, root)
            if shutil.which(claude_bin) is None:
                raise LaunchError(f"{claude_bin} not found on PATH")
        except (LaunchError, ConfigError) as e:
            print(f"error: {e}", file=sys.stderr)
            return 1

        if any(e.get("kind") != "run_started" for e in read_events(paths)):
            append_event(paths, "resumed", run_id=run_id)

        plugin = Path(os.path.abspath(plugin_dir)) if plugin_dir is not None else None
        argv = launch_argv(
            claude_bin=claude_bin, agent=SUPERVISOR_AGENT, first_message=first_message(root),
            plugin_dir=plugin, settings=cli_settings(), model=model, effort=effort,
            print_mode=print_mode)
        env = {**os.environ, "LEADHAND_RUN_ID": run_id, "LEADHAND_RUN_DIR": str(root),
               "LEADHAND_CLI": str(CLI_PATH), LOCK_FD_ENV: str(fd)}
        if claim is not None:
            env[CHECKOUT_FD_ENV] = str(claim.fd)
        print(ATTENDED_WARNING.format(cli=CLI_PATH), file=sys.stderr, flush=True)
        if any(c.isspace() for c in str(CLI_PATH)):
            print(SPACES_WARNING.format(cli=CLI_PATH), file=sys.stderr, flush=True)
        sys.stdout.flush()
        return _supervise(lock, fd, argv, workspace, env, plugin_dir=plugin, claim=claim)
    finally:
        if claim is not None:
            claim.release()
        lock.release()


def _supervise(lock: RunLock, fd: int, argv: list[str], workspace: Path, env: dict, *,
               plugin_dir: Path | None = None, claim: CheckoutClaim | None = None) -> int:
    """Start the child holding the lock fd (and the checkout claim's), forward SIGINT/SIGTERM,
    wait for it.

    Returns only after the child has exited; the caller then releases the lock.
    If anything raises while the child still runs, the lock is detached, not
    released, so the supervisor keeps it (the claim too).
    """
    child: subprocess.Popen | None = None
    pending: list[int] = []

    def forward(signum, _frame):
        if child is None:
            pending.append(signum)
            return
        if child.returncode is not None:     # reaped: its pid may belong to someone else now
            return
        try:
            os.kill(child.pid, signum)
        except ProcessLookupError:
            pass

    previous = {s: signal.signal(s, forward) for s in FORWARDED_SIGNALS}
    keep_awake = None
    try:
        if pending:
            return 128 + pending[0]
        try:
            child = subprocess.Popen(argv, cwd=str(workspace), env=env,
                                     pass_fds=(fd,) if claim is None else (fd, claim.fd))
        except OSError as e:
            print(f"error: cannot start {argv[0]}: {e}", file=sys.stderr)
            return 1
        for signum in pending:
            forward(signum, None)
        try:
            lock.record_supervisor(child.pid, plugin_dir=plugin_dir)
        except OSError as e:
            print(f"warning: could not write {lock.paths.holder}: {e}", file=sys.stderr)
        keep_awake = _start_keep_awake(PLATFORM, child.pid)
        code = child.wait()
        return 128 - code if code < 0 else code
    finally:
        if child is not None and child.poll() is None:
            lock.detach()       # never unlock under a live supervisor
            if claim is not None:
                claim.detach()
        for s, h in previous.items():
            signal.signal(s, h)
        _stop_keep_awake(keep_awake)


def close_inherited_lock(environ=os.environ) -> None:
    """Drop the run.lock descriptor a supervisor's Bash command inherited.

    Claude Code does not close the descriptor the launcher passes it, so every
    command the supervisor runs inherits run.lock. A long-lived one (a background
    `leadhand timer`) would keep the run locked after the supervisor dies and
    refuse every relaunch. Every `leadhand` command calls this first: it closes
    fd $LEADHAND_LOCK_FD only when it is open and is the same file (device and
    inode) as $LEADHAND_RUN_DIR/run.lock, never any other descriptor, and never
    unlocks (closing one reference leaves the supervisor's lock in place). The
    variable is removed from `environ` either way, so children do not try again.
    An in-place run's checkout claim ($LEADHAND_CHECKOUT_FD) is dropped the same way,
    when it is the same file as the checkout run.json names (`repo`).
    """
    raw = environ.pop(LOCK_FD_ENV, None)
    claim_raw = environ.pop(CHECKOUT_FD_ENV, None)
    run_dir = environ.get("LEADHAND_RUN_DIR")
    if not run_dir:
        return
    paths = run_paths(Path(run_dir))
    _close_if_same(raw, lambda: paths.lock)
    _close_if_same(claim_raw, lambda: json.loads(paths.run_json.read_text(encoding="utf-8"))["repo"])


def _close_if_same(raw: str | None, target) -> None:
    """Close descriptor `raw` only when it is open and is the same file (device and inode)
    as the path `target()` returns."""
    if not raw:
        return
    try:
        fd = int(raw)
    except ValueError:
        return
    if fd <= 2:
        return
    try:
        held = os.fstat(fd)
        want = os.stat(target())
    except (OSError, ValueError, LookupError, TypeError):
        return
    if (held.st_dev, held.st_ino) == (want.st_dev, want.st_ino):
        try:
            os.close(fd)
        except OSError:
            pass


def launch_line(run_dir: Path, *, plugin_dir: Path | None = None) -> str:
    """The exact command that launches (or resumes) the run, every argument quoted.

    Without `plugin_dir`, the plugin directory recorded by the last launch (in
    holder.json) is kept, so a relaunch loads the plugin the same way.
    """
    root = resolve_run_dir(run_dir)
    if plugin_dir is None:
        recorded = RunLock(run_paths(root), "")._holder().get("plugin_dir")
        plugin_dir = Path(recorded) if isinstance(recorded, str) and recorded else None
    argv = [str(CLI_PATH), "run", str(root / "deck.md")]
    if plugin_dir is not None:
        argv += ["--plugin-dir", os.path.abspath(plugin_dir)]
    return shlex.join(argv)


# --------------------------------------------------------------------------
# CLI

def _cmd_run(args) -> int:
    try:
        return run(Path(args.run_dir), plugin_dir=Path(args.plugin_dir) if args.plugin_dir else None,
                   print_mode=args.print_mode)
    except (OSError, ValueError) as e:      # e.g. an unreadable events.jsonl; the lock is released
        print(f"error: {e}", file=sys.stderr)
        return 1


def _cmd_launch_line(args) -> int:
    try:
        print(launch_line(Path(args.run_dir),
                          plugin_dir=Path(args.plugin_dir) if args.plugin_dir else None))
    except LaunchError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


def add_commands(sub) -> None:
    p = sub.add_parser("run", help="own a run and start its supervisor session (attended use only)")
    p.add_argument("run_dir", help="the run directory or its deck.md")
    p.add_argument("--plugin-dir", help="load the leadhand plugin from this directory")
    p.add_argument("--print", dest="print_mode", action="store_true", help=argparse.SUPPRESS)
    p.set_defaults(func=_cmd_run)

    p = sub.add_parser("launch-line", help="print the exact command that launches or resumes a run")
    p.add_argument("run_dir", help="the run directory or its deck.md")
    p.add_argument("--plugin-dir", help="load the leadhand plugin from this directory")
    p.set_defaults(func=_cmd_launch_line)
