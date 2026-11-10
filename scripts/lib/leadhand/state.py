"""Run state: the run directory, durable events, card folding and run outcomes.

Both the supervisor's CLI calls and the detached Codex runners write the run
directory, so everything here is built for several processes at once and for a
crash at any point.

Run directory layout (names are fixed by `run_paths`):

    deck.md          the deck as it was when the run started
    run.json         run metadata (written atomically)
    events.jsonl     append-only event log; the source of truth for card state
    status.json      rewritten atomically at each checkpoint, read by `leadhand status`
    attempts.jsonl   append-only record of each worker attempt
    charges.jsonl    append-only record of usage charged
    state.lock       short-held lock around every JSONL repair/append and every
                     read-compute-append sequence (this module's `state_lock`)
    report.md        final report
    handoff.md       handoff written on pause or incomplete end
    run.lock         long-held lock that marks supervisor ownership (NOT state.lock)
    holder.json      who holds run.lock, and since when
    pause, cancel    flag files the operator creates
    cards/           per-card files
    workers/         per-dispatch worker files (pid, heartbeat, reply)

Events are written as intent -> completion pairs around every side effect, so a
crash between the two is reconciled by inspecting Git and the run directory.
"""
from __future__ import annotations

import contextlib
import json
import os
import fcntl
import tempfile
import threading
from dataclasses import dataclass
from datetime import date, datetime, timezone
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Iterator

from .config import AfterRun

# --------------------------------------------------------------------------
# Paths

@dataclass(frozen=True)
class RunPaths:
    root: Path
    deck: Path
    run_json: Path
    events: Path
    status: Path
    attempts: Path
    charges: Path
    state_lock: Path
    report: Path
    handoff: Path
    lock: Path
    holder: Path
    pause_flag: Path
    cancel_flag: Path
    cards_dir: Path
    workers_dir: Path


def run_paths(root: Path) -> RunPaths:
    root = Path(root)
    return RunPaths(
        root=root,
        deck=root / "deck.md",
        run_json=root / "run.json",
        events=root / "events.jsonl",
        status=root / "status.json",
        attempts=root / "attempts.jsonl",
        charges=root / "charges.jsonl",
        state_lock=root / "state.lock",
        report=root / "report.md",
        handoff=root / "handoff.md",
        lock=root / "run.lock",
        holder=root / "holder.json",
        pause_flag=root / "pause",
        cancel_flag=root / "cancel",
        cards_dir=root / "cards",
        workers_dir=root / "workers",
    )


def new_run_dir(docs_dir: Path, slug: str, today: date) -> Path:
    """Create and return docs_dir/runs/YYYY-MM-DD-<slug>, or the first free `-2`, `-3`, ...

    The directory is created with a plain `mkdir`, so two launchers racing for
    the same name cannot both get it.
    """
    if not slug or "/" in slug or os.sep in slug or slug.startswith("."):
        raise ValueError(f"invalid run slug: {slug!r}")
    runs = Path(docs_dir) / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    base = f"{today.isoformat()}-{slug}"
    n = 1
    while True:
        candidate = runs / (base if n == 1 else f"{base}-{n}")
        try:
            candidate.mkdir()
            return candidate
        except FileExistsError:
            n += 1


# --------------------------------------------------------------------------
# State lock

class _Held:
    __slots__ = ("rlock", "depth", "fd")

    def __init__(self) -> None:
        self.rlock = threading.RLock()
        self.depth = 0
        self.fd = -1


_HELD_GUARD = threading.Lock()
_HELD: dict[tuple[int, str], _Held] = {}


@contextlib.contextmanager
def state_lock(paths: RunPaths) -> Iterator[None]:
    """Exclusive `flock` on state.lock, re-entrant within a process.

    A per-path depth counter makes nested `with` blocks (in any thread of this
    process) neither re-lock nor unlock early; other threads wait on a
    per-path RLock, other processes wait on the flock. Keyed by pid as well so a
    forked child never trusts the parent's counters.
    """
    key = (os.getpid(), os.path.realpath(paths.state_lock))
    with _HELD_GUARD:
        held = _HELD.setdefault(key, _Held())
    held.rlock.acquire()
    try:
        if held.depth == 0:
            fd = os.open(paths.state_lock, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX)
            except BaseException:
                os.close(fd)
                raise
            held.fd = fd
        held.depth += 1
    except BaseException:
        held.rlock.release()
        raise
    try:
        yield
    finally:
        held.depth -= 1
        try:
            if held.depth == 0:
                fd, held.fd = held.fd, -1
                try:
                    fcntl.flock(fd, fcntl.LOCK_UN)
                finally:
                    os.close(fd)
        finally:
            held.rlock.release()


# --------------------------------------------------------------------------
# JSONL append / read

EVENT_KINDS: tuple[str, ...] = (
    "run_started",
    "card_cut",
    "brief_written",
    "dispatch_requested",   # intent: a worker is about to be launched
    "worker_started",       # completion of the launch
    "result_recorded",      # completion of the dispatch (result files are on disk)
    "verdict_recorded",
    "fix_round",
    "diff_checked",
    "integration_started",  # intent: rebase onto the run tip
    "rebased",              # completion
    "conflict",             # completion (rebase stopped on conflicts)
    "accept_started",       # intent: fast-forward the run branch to `target`
    "accepted",             # completion
    "parked",
    "unparked",
    "ruling",
    "violation",
    "gate_started",         # intent: a gate is about to run
    "gate_result",          # completion: gate, examined_sha, passed
    "outcome",
    "report_written",
    "paused",
    "cancelled",
    "resumed",
    "reconciled",
)

CRASH_POINTS: tuple[str, ...] = (
    "before_worker_launch",
    "after_worker_launch",
    "before_rebase",
    "after_rebase",
    "before_fast_forward",
    "after_fast_forward",
    "before_result_record",
    "after_result_files",             # files copied, event not yet written
    "after_charge_before_attempt",    # usage charged, attempt record not yet written
    "after_cancel_request",           # a runner was asked to stop, its stop not yet confirmed
    "before_gate_record",
)

_CHUNK = 4096


def _rfind_newline(fd: int, before: int) -> int:
    """Offset of the last b"\\n" at an offset < `before`, or -1."""
    end = before
    while end > 0:
        start = max(0, end - _CHUNK)
        block = os.pread(fd, end - start, start)
        idx = block.rfind(b"\n")
        if idx != -1:
            return start + idx
        end = start
    return -1


def _fsync_dir(directory: Path) -> None:
    dfd = os.open(directory, os.O_RDONLY)
    try:
        os.fsync(dfd)
    finally:
        os.close(dfd)


def _append(paths: RunPaths, file: Path,
            build: Callable[[dict | None], dict]) -> dict:
    """Under the state lock: drop a partial final line, build a record from the
    last complete record, append it as one line, flush and fsync."""
    file = Path(file)
    with state_lock(paths):
        created = not file.exists()
        fd = os.open(file, os.O_RDWR | os.O_CREAT | os.O_APPEND | os.O_CLOEXEC, 0o644)
        try:
            size = os.fstat(fd).st_size
            if size and os.pread(fd, 1, size - 1) != b"\n":
                good = _rfind_newline(fd, size) + 1  # 0 when there is no newline at all
                os.ftruncate(fd, good)
                size = good
            last = None
            if size:
                start = _rfind_newline(fd, size - 1) + 1
                last = _parse_line(os.pread(fd, size - 1 - start, start), file)
            record = build(last)
            data = (json.dumps(record, ensure_ascii=False, separators=(",", ":"))
                    + "\n").encode("utf-8")
            view = memoryview(data)
            while view:  # O_APPEND: the kernel writes at the end of the file
                view = view[os.write(fd, view):]
            os.fsync(fd)
        finally:
            os.close(fd)
        if created:
            _fsync_dir(file.parent)
    return record


def _parse_line(raw: bytes, file: Path) -> dict:
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise ValueError(f"corrupt record in {file.name}: {exc}") from None
    if not isinstance(value, dict):
        raise ValueError(f"corrupt record in {file.name}: not a JSON object")
    return value


def _read_bytes(file: Path) -> bytes:
    """Whole-file read (a seam for tests; may take several read() calls)."""
    with open(file, "rb") as f:
        return f.read()


def _read_jsonl(paths: RunPaths, file: Path) -> list[dict]:
    """Every complete line, parsed. A final line with no newline is a write in
    progress or a crash remnant and is ignored; a complete line that does not
    parse is real corruption and raises ValueError.

    The read holds the (short-held) state lock: a writer's tail repair
    (truncate a remnant, then append) between two read() calls would otherwise
    splice the remnant onto the new record and look like corruption.
    """
    file = Path(file)
    if not paths.root.is_dir():
        return []
    with state_lock(paths):
        try:
            raw = _read_bytes(file)
        except FileNotFoundError:
            return []
    lines = raw.split(b"\n")[:-1]
    return [_parse_line(line, file) for line in lines if line.strip()]


def append_jsonl(paths: RunPaths, file: Path, record: dict) -> None:
    """The shared append primitive behind events, attempts and charges."""
    _append(paths, file, lambda _last: record)


def _utc_now() -> str:
    return (datetime.now(timezone.utc).isoformat(timespec="milliseconds")
            .replace("+00:00", "Z"))


def append_event(paths: RunPaths, kind: str, /, **fields: Any) -> dict:
    """Append one event with `seq` (last seq + 1, read from the file under the
    lock) and `ts` (UTC, ISO 8601, `Z`). Returns the record written."""
    if kind not in EVENT_KINDS:
        raise ValueError(f"unknown event kind: {kind!r}")
    for reserved in ("seq", "ts", "kind"):
        if reserved in fields:
            raise ValueError(f"event field {reserved!r} is reserved")

    def build(last: dict | None) -> dict:
        if last is None:
            seq = 1
        else:
            prev = last.get("seq")
            if not isinstance(prev, int) or isinstance(prev, bool):
                raise ValueError("last event in events.jsonl has no integer seq")
            seq = prev + 1
        return {"seq": seq, "ts": _utc_now(), "kind": kind, **fields}

    return _append(paths, paths.events, build)


def read_events(paths: RunPaths) -> list[dict]:
    return _read_jsonl(paths, paths.events)


def append_attempt(paths: RunPaths, record: dict) -> None:
    append_jsonl(paths, paths.attempts, record)


def read_attempts(paths: RunPaths) -> list[dict]:
    return _read_jsonl(paths, paths.attempts)


def write_json_atomic(path: Path, data: dict) -> None:
    """Write JSON to a temp file in the same directory, fsync, `os.replace`.
    No temp file survives, on success or on error."""
    path = Path(path)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2, ensure_ascii=False)
            f.write("\n")
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(tmp)
        raise
    _fsync_dir(path.parent)


def crash_point(name: str) -> None:
    """Fault injection: exit at once with code 97, skipping all cleanup, when
    LEADHAND_TESTING=1 and LEADHAND_CRASH_AT names this point."""
    if os.environ.get("LEADHAND_TESTING") == "1" and os.environ.get("LEADHAND_CRASH_AT") == name:
        os._exit(97)


# --------------------------------------------------------------------------
# Folding events into card state

CARD_STATES = ("pending", "cut", "requested", "started", "reported", "reviewing", "reviewed",
               "fixing", "integrating", "accepting", "accepted", "parked")


@dataclass
class CardState:
    number: int
    state: str
    attempt: int
    branch: str | None
    base: str | None
    tip: str | None
    dispatch_id: str | None
    dispatch_kind: str | None
    worker_id: str | None
    vendor: str | None
    status: str | None
    verdict: str | None
    findings: str | None
    fix_rounds: int
    kind_counts: dict[str, int]
    accept_target: str | None
    accepted_sha: str | None
    park_reason: str | None


def _blank_card(number: int) -> CardState:
    return CardState(number=number, state="pending", attempt=0, branch=None, base=None,
                     tip=None, dispatch_id=None, dispatch_kind=None, worker_id=None, vendor=None, status=None,
                     verdict=None, findings=None, fix_rounds=0, kind_counts={},
                     accept_target=None, accepted_sha=None, park_reason=None)


def _card_number(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def fold_cards(events: list[dict], card_numbers: list[int]) -> dict[int, CardState]:
    """Derive each card's state from the event stream, in order.

    Every card in `card_numbers` starts `pending`. Events for other cards are
    ignored. Event fields used (everything else is ignored):

        card_cut{card, branch, base}            -> cut. Starts a fresh branch: sets branch and
                                                   base; clears tip, verdict, findings, status,
                                                   dispatch_id, worker_id, accept_target and
                                                   fix_rounds. attempt and kind_counts are kept.
        dispatch_requested{dispatch_id, card,   -> requested (dispatch_kind "review" -> reviewing,
                           attempt, dispatch_kind,  "fix" -> fixing). Sets dispatch_id, attempt,
                           vendor}                  vendor; clears worker_id and status;
                                                   kind_counts[dispatch_kind] += 1.
        worker_started{dispatch_id, worker_id}  -> started (only from requested; review and fix
                                                   dispatches stay reviewing / fixing). Sets
                                                   worker_id.
        result_recorded{dispatch_id, status,    -> reported (a review dispatch stays reviewing
                        blocked_by, tip}           until its verdict). Sets status, and tip when
                                                   given. blocked_by is not folded.
        verdict_recorded{card, verdict,         -> reviewed. Sets verdict and findings.
                         findings}
        fix_round{card, n}                      -> fixing. fix_rounds = n.
        diff_checked{card, ...}                 -> no change.
        integration_started{card}               -> integrating.
        rebased{card, tip}                      -> integrating. Sets tip when given; clears
                                                   verdict and findings (the review no longer
                                                   covers the integrated diff).
        conflict{card}                          -> integrating (a conflict dispatch follows).
        accept_started{card, target}            -> accepting. accept_target = target.
        accepted{card, sha}                     -> accepted. accepted_sha = sha.
        parked{card, reason}                    -> parked. park_reason = reason.
        unparked{card, note}                    -> cut if the card has a branch, else pending.
                                                   park_reason cleared.

    A parked card records the fields of later events (a late result_recorded or
    verdict_recorded for its in-flight dispatch still sets status, tip, verdict, ...) but its
    state stays `parked`; only `unparked` leaves it.

    Phases that share a state are told apart by `dispatch_kind` (the kind of the card's
    current dispatch, set by dispatch_requested, cleared by card_cut):
        reviewing + dispatch_kind "review"  -> a review dispatch is in flight or finished,
                                               verdict not yet recorded
        reviewing + anything else           -> not reachable by folding
        fixing    + dispatch_kind "fix"     -> a fix dispatch is requested/started
        fixing    + dispatch_kind != "fix"  -> fix round recorded, nothing dispatched yet
                                               (dispatch_kind is the previous dispatch's)
        requested / started                 -> dispatch_kind says implement, conflict, ...
        reported                            -> dispatch_kind says which dispatch reported
        integrating                         -> integration_started / rebased / conflict recorded;
                                               dispatch_kind is the previous dispatch's, so it
                                               says nothing about the integration step (a
                                               conflict dispatch moves the card to requested)

    The dispatch's own kind is `dispatch_kind` (implement, fix, review, conflict, ...)
    because `kind` is the event kind in every record. worker_started and result_recorded
    name only a dispatch_id; they apply to the card that dispatch belongs to, and only while
    it is that card's current dispatch (events for an older or unknown dispatch are
    ignored). `accepted` is terminal: later events for an accepted card are ignored. All
    other event kinds do not touch card state.
    """
    cards = {n: _blank_card(n) for n in card_numbers}
    dispatches: dict[Any, tuple[int, str]] = {}  # dispatch_id -> (card, dispatch_kind)

    for e in events:
        kind = e.get("kind")
        dispatch_kind = ""
        if kind in ("worker_started", "result_recorded"):
            owner = dispatches.get(e.get("dispatch_id"))
            if owner is None:
                continue
            number, dispatch_kind = owner
        else:
            number = _card_number(e.get("card"))
            if kind == "dispatch_requested" and number is not None:
                dispatch_kind = str(e.get("dispatch_kind") or "implement")
                dispatches[e.get("dispatch_id")] = (number, dispatch_kind)
        card = cards.get(number) if number is not None else None
        if card is None or card.state == "accepted":
            continue
        if kind in ("worker_started", "result_recorded") and card.dispatch_id != e.get("dispatch_id"):
            continue
        held = card.state == "parked" and kind != "unparked"
        before = card.state

        if kind == "card_cut":
            card.state = "cut"
            card.branch = e.get("branch")
            card.base = e.get("base")
            card.tip = card.verdict = card.findings = card.status = None
            card.dispatch_id = card.dispatch_kind = card.worker_id = card.accept_target = None
            card.fix_rounds = 0
        elif kind == "dispatch_requested":
            card.state = {"review": "reviewing", "fix": "fixing"}.get(dispatch_kind, "requested")
            card.dispatch_id = e.get("dispatch_id")
            card.dispatch_kind = dispatch_kind
            card.attempt = e.get("attempt", card.attempt)
            card.vendor = e.get("vendor")
            card.worker_id = card.status = None
            card.kind_counts[dispatch_kind] = card.kind_counts.get(dispatch_kind, 0) + 1
        elif kind == "worker_started":
            card.worker_id = e.get("worker_id")
            if card.state == "requested":
                card.state = "started"
        elif kind == "result_recorded":
            card.status = e.get("status")
            if e.get("tip") is not None:
                card.tip = e.get("tip")
            if dispatch_kind != "review":
                card.state = "reported"
        elif kind == "verdict_recorded":
            card.state = "reviewed"
            card.verdict = e.get("verdict")
            card.findings = e.get("findings")
        elif kind == "fix_round":
            card.state = "fixing"
            card.fix_rounds = e.get("n", card.fix_rounds + 1)
        elif kind in ("integration_started", "conflict"):
            card.state = "integrating"
        elif kind == "rebased":
            card.state = "integrating"
            if e.get("tip") is not None:
                card.tip = e.get("tip")
            card.verdict = card.findings = None
        elif kind == "accept_started":
            card.state = "accepting"
            card.accept_target = e.get("target")
        elif kind == "accepted":
            card.state = "accepted"
            card.accepted_sha = e.get("sha")
        elif kind == "parked":
            card.state = "parked"
            card.park_reason = e.get("reason")
        elif kind == "unparked":
            card.state = "cut" if card.branch is not None else "pending"
            card.park_reason = None
        if held:
            card.state = before  # only `unparked` leaves parked
    return cards


# --------------------------------------------------------------------------
# Outcomes

class Outcome(str, Enum):
    COMPLETE = "complete"
    INCOMPLETE = "incomplete"
    BLOCKED = "blocked"
    FAILED = "failed"


BLOCKED_QUALIFIERS = ("paused", "provider capacity", "decision needed", "environment")


@dataclass(frozen=True)
class OutcomeDecision:
    outcome: Outcome
    qualifier: str
    reasons: tuple[str, ...]


def decide_outcome(cards: dict[int, CardState], *, removed: frozenset[int], open_findings: bool,
                   holds: tuple[str, ...], gates_current: bool, verification_failed: bool,
                   cancelled: bool, blocked_reason: str | None,
                   unreconciled: str | None) -> OutcomeDecision:
    """First match wins:

    1. unreconciled          -> failed ("state could not be reconciled: ...")
    2. verification_failed   -> failed
    3. blocked_reason        -> blocked, qualifier = the reason (paused, provider capacity,
                                decision needed or environment; anything else is a ValueError)
    4. cancelled             -> incomplete, qualifier "cancelled"
    5. every card accepted or removed, no open findings, no holds, gates current -> complete
    6. otherwise             -> incomplete, one reason per card not done (parked or still
                                in flight), per hold, one for open findings, one for stale gates

    The qualifier is "" when there is none.
    """
    if unreconciled:
        return OutcomeDecision(Outcome.FAILED, "",
                               (f"state could not be reconciled: {unreconciled}",))
    if verification_failed:
        return OutcomeDecision(Outcome.FAILED, "",
                               ("verification of the final commit failed",))
    if blocked_reason:
        if blocked_reason not in BLOCKED_QUALIFIERS:
            raise ValueError(f"unknown blocked reason: {blocked_reason!r}")
        return OutcomeDecision(Outcome.BLOCKED, blocked_reason, (f"blocked: {blocked_reason}",))

    reasons: list[str] = []
    for number in sorted(cards):
        c = cards[number]
        if c.state == "accepted" or number in removed:
            continue
        if c.state == "parked":
            reasons.append(f"card {number} parked: {c.park_reason or 'no reason recorded'}")
        else:
            reasons.append(f"card {number} not accepted (state: {c.state})")
    if open_findings:
        reasons.append("unresolved review findings")
    reasons.extend(f"hold: {h}" for h in holds)
    if not gates_current:
        reasons.append("required gates have not all passed on the run head")

    if cancelled:
        return OutcomeDecision(Outcome.INCOMPLETE, "cancelled",
                               ("run cancelled", *reasons))
    if not reasons:
        return OutcomeDecision(Outcome.COMPLETE, "", ())
    return OutcomeDecision(Outcome.INCOMPLETE, "", tuple(reasons))


def allowed_next_step(decision: OutcomeDecision, after_run: AfterRun) -> str:
    """complete -> after_run.step; incomplete -> "draft-pr" when on_incomplete is
    "draft-pr", else "none"; blocked and failed -> "none"."""
    if decision.outcome is Outcome.COMPLETE:
        return after_run.step
    if decision.outcome is Outcome.INCOMPLETE:
        return "draft-pr" if after_run.on_incomplete == "draft-pr" else "none"
    return "none"
