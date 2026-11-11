"""Run engine: the card loop.

The supervisor loops `leadhand next` -> act -> `leadhand record`. The engine owns
every mechanical step (cutting card branches, writing briefs, resolving routes,
choosing what comes next, accepting cards) and returns one action at a time for
the parts only the supervisor can do: launching and stopping workers, answering
questions and reading test-only diffs.

State lives in the run directory's `events.jsonl`; `next_action` re-reads it on
every call and is safe to repeat. Every side effect is idempotent and bracketed
by events, so a crash at any point is reconciled by the next call.

Actions (`next_action`):

    dispatch     {dispatch_id, kind, card, attempt, role, agent, agent_model, agent_effort,
                  route{vendor, model, effort}, message, brief, report, resume, deadline}
                 Launch a worker in the background (or SendMessage to `resume.worker_id`),
                 then `record started`. Repeated until `started` is recorded.
    wait         {dispatch_id, worker_id, deadline, reason}: a worker is running.
    stop_worker  {dispatch_id, worker_id, vendor, record_result}: the deadline passed. Stop
                 the worker (or the Codex proxy); when record_result is true, record its
                 result with `stopped`.
    answer       {card, dispatch_id, worker_id, report, question}: the worker asked a
                 question. Record a ruling, or park the card.
    check_diff   {card, dispatch_id, diff, findings, paths}: a fix touched only tests and
                 docs. Read the diff and record a diff check.
    done         {outcome, qualifier, reasons, report, cards}: the run ended (the report,
                 handoff and runs.md line are written before this is returned).

Dispatch IDs are `c<card>-a<attempt>-<kind>-<n>`, `n` counting that kind within the
card's attempt. Run-level dispatches (card None) are `run-<kind>-<n>` for the end-of-run
gates and `run-w<wave>-<kind>-<n>` for a fix wave; their dispatch actions also carry
`gate` and `wave`.

Integration before acceptance: a card is accepted only by fast-forwarding the run branch
to a tip that contains the run tip. When the card branch does not, the engine rebases it
(`integration_started` -> `rebased` | `conflict`), a conflict goes to an `integrate`
attempt, and the integrated card is re-reviewed (`rereview`) and its deck checks run
(`verify`) before the fast-forward. Once a card has been integrated, every tip it is
accepted at needs a passing verify of that exact tip.

Reviewers and verifiers may build but never change the workspace or move refs. Each of
their dispatches records the card tip and branch it must leave in place and a snapshot of
the run's refs; after the result, any change is a `violation`: preserved under
`refs/leadhand/<run_id>/violations/`, restored, the result voided and the same kind
dispatched again on the same route.

End of run: once every card is accepted, parked, or waits on a parked card, the
commit-bound gates run on the run head (see `_end_of_run`), then the outcome, the
report, a handoff when blocked, and one line in `<docs_dir>/runs.md`.

Recovery: after a relaunch (`resumed`), the first `next` reconciles every step the dead
session left in flight from Git and the run directory before anything new happens, and
records `reconciled` with what it found (see `_reconcile`). The operator's `leadhand
pause` and `leadhand cancel` create flags the engine turns into `paused` and `cancelled`:
a pause lets running workers finish and ends blocked (paused); a cancel stops every
worker, confirmed, then ends incomplete (cancelled), and no relaunch undoes it.
"""
from __future__ import annotations

import hashlib
import inspect
import json
import os
import re
import secrets
import shlex
import shutil
import stat
import sys
import tempfile
import time
from dataclasses import dataclass, replace
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any

from leadhand import dispatch as dsp
from leadhand import usage
from leadhand.briefs import (CODEX_AGENT, ScratchError, agent_for, dispatch_message, parse_report_head,
                             remove_scratch_file, scratch_dir, write_brief)
from leadhand.config import (SNAPSHOT, VENDORS, Config, ConfigError, Route, config_to_dict, docs_dir_for,
                             load_config, load_run_config, unsupported_warnings)
from leadhand.deck import Card, Deck, DeckError, check_deck, dependency_order, parse_deck
from leadhand.gitops import (GitError, card_branch_name, changed_paths, cut_card_branch, default_base,
                             detect_violation, ensure_excluded, fast_forward, git, head, is_ancestor,
                             is_test_or_doc_path, needs_integration, prepare_workspace, preserve_and_restore,
                             rebase_onto, resolves_in_place, stage_work, status_entries, untracked_files)
from leadhand.route import LadderExhausted, PinnedVendorMissing, Resolution, resolve
from leadhand.state import (CardState, RunPaths, utc_now, allowed_next_step, append_attempt, append_event,
                            crash_point, decide_outcome, fold_cards, new_run_dir, read_attempts, read_events,
                            run_paths, state_lock, write_json_atomic)

ACTIONS = ("dispatch", "wait", "check_diff", "answer", "stop_worker", "done")
RECORD_KINDS = ("started", "result", "ruling", "park", "diff-check")
IMPLEMENTER_KINDS = ("implement", "fix", "answer", "continue", "integrate")
REVIEW_KINDS = ("review", "rereview")
# Reviewers and verifiers: they may build, but must leave the workspace and the run's refs as they were.
CHECK_KINDS = (*REVIEW_KINDS, "verify")
VOIDED_NOTE = "## An earlier run of this check was voided"
VIOLATION_CAP = 2             # voided re-runs per card and kind; one more violation parks the card
# Card states whose card has work open (a dispatch, a review, a fix, an integration or an acceptance).
ACTIVE_STATES = ("requested", "started", "reported", "reviewing", "reviewed", "fixing", "integrating", "accepting")
OPERATIONS = (("rebase-merge", "rebase"), ("rebase-apply", "rebase"), ("MERGE_HEAD", "merge"),
              ("CHERRY_PICK_HEAD", "cherry-pick"), ("REVERT_HEAD", "revert"), ("sequencer", "sequencer"))
CODEX_GRACE_MINUTES = 5       # the Codex runner enforces its own limit; the proxy gets this much more
TIMER_INTERVAL = 10.0
REPLY_FILE = "reply.txt"      # in the dispatch's scratch folder (R38)
MAX_STEPS = 200               # mechanical steps in one next_action call
SLUG_MAX = 40
_TURN_LIMIT = re.compile(r"stopped at its \d+-turn limit", re.IGNORECASE)
# A finding line in a reviewer report: "1. [high] ..." (also "- [medium] ...").
_SEVERE_FINDING = re.compile(r"^\s*(?:\d+[.)]|[-*])?\s*\[(?:high|medium)\]", re.IGNORECASE | re.MULTILINE)
_QUESTION = re.compile(r"^#+\s*Question\s*$(.*?)(?=^#+\s|\Z)", re.IGNORECASE | re.MULTILINE | re.DOTALL)


class EngineError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# Small helpers

def _now() -> float:
    return time.time()


def _format_time(ts: float) -> str:
    return datetime.fromtimestamp(ts, timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _parse_time(text: str) -> float:
    return datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()


def installed_vendors() -> frozenset[str]:
    """Vendors whose CLI is on PATH. Tests set LEADHAND_INSTALLED_VENDORS instead."""
    if os.environ.get("LEADHAND_TESTING") == "1" and "LEADHAND_INSTALLED_VENDORS" in os.environ:
        names = os.environ["LEADHAND_INSTALLED_VENDORS"].split(",")
        return frozenset(v.strip() for v in names if v.strip() in VENDORS)
    return frozenset(v for v in VENDORS if shutil.which(v))


def _projects_dir() -> Path | None:
    """Claude Code's transcript folder: $CLAUDE_CONFIG_DIR/projects, else the default."""
    home = os.environ.get("CLAUDE_CONFIG_DIR")
    return Path(home).expanduser() / "projects" if home else None


def _slug(title: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")[:SLUG_MAX].strip("-")
    return slug or "run"


def _write_bytes_atomic(path: Path, data: bytes) -> None:
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def _read_text(path: Path | str | None) -> str | None:
    if not path:
        return None
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace")
    except (FileNotFoundError, IsADirectoryError):
        return None


def _has_status(text: str | None) -> bool:
    return bool(text and text.strip()) and dsp.has_status_line(text)


# --------------------------------------------------------------------------
# The run as loaded for one call

@dataclass
class _Run:
    root: Path
    paths: RunPaths
    meta: dict
    deck: Deck
    config: Config
    ws: Path
    branch: str

    def card(self, number: int) -> Card:
        for c in self.deck.cards:
            if c.number == number:
                return c
        raise EngineError(f"no card {number} in the deck")

    @property
    def numbers(self) -> list[int]:
        return [c.number for c in self.deck.cards]

    def card_dir(self, number: int | None, dispatch_id: str) -> Path:
        return self.root / "cards" / ("run" if number is None else str(number)) / dispatch_id


def _load(run_dir: Path | str, *, check_digest: bool) -> _Run:
    """The run, with its frozen deck (`<run>/deck.md`) and configuration snapshot; with
    `check_digest`, refused when the source deck no longer matches (see `_deck_problem`)."""
    root = Path(run_dir).absolute()
    paths = run_paths(root)
    try:
        meta = json.loads(paths.run_json.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise EngineError(f"not a run directory (no run.json): {root}") from None
    except ValueError as e:
        raise EngineError(f"run.json is unreadable: {e}") from None
    try:
        deck = parse_deck(paths.deck)
        config = load_run_config(root, Path(meta["repo"]))      # the snapshot from start (I5)
    except (DeckError, ConfigError, OSError) as e:
        raise EngineError(str(e)) from None
    ctx = _Run(root=root, paths=paths, meta=meta, deck=deck, config=config,
               ws=Path(meta["workspace"]), branch=meta["branch"])
    problem = _deck_problem(ctx) if check_digest else None
    if problem:
        raise EngineError(problem)
    return ctx


def _deck_problem(ctx: _Run) -> str | None:
    """Why the source deck no longer matches the one the run started from (edited, moved or
    deleted), or None. It blocks every new step; reconciliation, a pause and a cancel run on
    the frozen deck regardless, so the operator can always stop a run."""
    source = Path(ctx.meta["deck_source"])
    try:
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
    except OSError as e:
        return f"cannot read the deck {source}: {e}"
    if digest != ctx.meta["deck_digest"]:
        return f"deck changed since start (expected {ctx.meta['deck_digest']})"
    return None


# --------------------------------------------------------------------------
# Reading the event log

@dataclass
class _Dispatch:
    req: dict
    started: dict | None = None
    result: dict | None = None

    @property
    def id(self) -> str:
        return self.req["dispatch_id"]

    @property
    def kind(self) -> str:
        return self.req["dispatch_kind"]

    @property
    def route(self) -> Route:
        return Route(self.req["vendor"], self.req["model"], self.req["effort"])


def _dispatches(events: list[dict]) -> dict[str, _Dispatch]:
    out: dict[str, _Dispatch] = {}
    for e in events:
        kind, did = e["kind"], e.get("dispatch_id")
        if kind == "dispatch_requested" and did not in out:
            out[did] = _Dispatch(e)
        elif kind == "worker_started" and did in out and out[did].started is None:
            out[did].started = e
        elif kind == "result_recorded" and did in out and out[did].result is None:
            out[did].result = e
    return out


def _ended(events: list[dict]) -> dict | None:
    """The outcome event that ended the run, unless a later `resumed` reopened it."""
    last = None
    for e in events:
        if e["kind"] == "outcome":
            last = e
        elif e["kind"] == "resumed":
            last = None
    return last


def _card_dispatches(index: dict[str, _Dispatch], card: int, *, attempt: int | None = None,
                     kinds: tuple[str, ...] | None = None) -> list[_Dispatch]:
    return [d for d in index.values()
            if d.req.get("card") == card and (attempt is None or d.req.get("attempt") == attempt)
            and (kinds is None or d.kind in kinds)]


def _fix_round(events: list[dict], card: int, attempt: int, before_seq: int | None = None) -> dict | None:
    rounds = [e for e in events if e["kind"] == "fix_round" and e.get("card") == card
              and e.get("attempt") == attempt and (before_seq is None or e["seq"] < before_seq)]
    return rounds[-1] if rounds else None


def _done_action(ctx: "_Run", outcome: dict, events: list[dict]) -> dict:
    report = ctx.paths.report
    return {"action": "done", "outcome": outcome["outcome"], "qualifier": outcome.get("qualifier", ""),
            "reasons": outcome.get("reasons", []), "report": str(report) if report.is_file() else None,
            "cards": _card_summary(ctx, fold_cards(events, ctx.numbers))}


# --------------------------------------------------------------------------
# start

def start(deck_path: Path, *, repo: Path, config: Config | None = None, new: bool = False) -> Path:
    """Start a run for the deck, or return the unfinished run already started from it."""
    root, _, note = _start(Path(deck_path), Path(repo), config, new)
    if note:
        print(note)
    return root


def _runs(docs: Path) -> list[tuple[Path, dict]]:
    """Every run directory under docs_dir/runs with a readable run.json."""
    runs = docs / "runs"
    out = []
    if runs.is_dir():
        for rj in runs.glob("*/run.json"):
            try:
                meta = json.loads(rj.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if isinstance(meta, dict):
                out.append((rj.parent, meta))
    return out


def _find_unfinished(docs: Path, source: str, digest: str, repo: Path) -> Path | None:
    """The newest run of this deck in this repository that has no outcome, or whose latest
    outcome is blocked (a blocked run continues, R25). A run of another repository (a second
    clone sharing the docs_dir) is never returned."""
    found = []
    real_repo = os.path.realpath(repo)
    for root, meta in _runs(docs):
        if meta.get("deck_source") != source or meta.get("deck_digest") != digest:
            continue
        if not isinstance(meta.get("repo"), str) or os.path.realpath(meta["repo"]) != real_repo:
            continue
        try:
            ended = _ended(read_events(run_paths(root)))
        except (OSError, ValueError) as e:
            print(f"warning: skipping the earlier run {root}: its event log is unreadable ({e})", file=sys.stderr)
            continue
        if ended is None or ended.get("outcome") == "blocked":
            found.append((str(meta.get("created_at", "")), str(root), root))
    return max(found)[2] if found else None


def _free_branch(docs: Path, repo: Path, requested: str) -> str:
    """The deck's branch, or its first free numeric suffix (`-2`, `-3`, ...) when another run
    of this repo owns it or the branch already exists (left by a start that crashed before
    run.json, or made by someone else): a new run never reuses another run's branch, card
    branches or worktree (R26')."""
    taken = {meta.get("branch") for _, meta in _runs(docs) if meta.get("repo") == str(repo)}
    taken |= {ref.removeprefix("refs/heads/")
              for ref in git(repo, "for-each-ref", "--format=%(refname)", "refs/heads/").splitlines()}
    branch, n = requested, 1
    while branch in taken:
        n += 1
        branch = f"{requested}-{n}"
    return branch


def _start(deck_path: Path, repo: Path, config: Config | None, new: bool) -> tuple[Path, bool, str]:
    """(run directory, whether it is an existing run, a line to print or "")."""
    deck_path, repo = deck_path.resolve(), repo.resolve()
    try:
        raw = deck_path.read_bytes()
        deck = parse_deck(deck_path)
        config = config or load_config(repo)
    except (DeckError, ConfigError, OSError, UnicodeDecodeError) as e:
        raise EngineError(str(e)) from None
    if hashlib.sha256(raw).hexdigest() != deck.digest:
        raise EngineError("the deck changed while it was being read; start again")
    errors = [p.message for p in check_deck(deck, repo, config) if p.level == "error"]
    if errors:
        raise EngineError("the deck has errors: " + "; ".join(errors))
    for msg in unsupported_warnings(config):
        print(f"warning: {msg}", file=sys.stderr)
    if not config.run.worktree:
        # An in-place run's workspace is the repository itself (L6); `run` checks every launch.
        from leadhand.launch import tools_inside
        problem = tools_inside(repo)
        if problem:
            raise EngineError(problem)
    docs = docs_dir_for(config, repo)
    if not new:
        existing = _find_unfinished(docs, str(deck_path), deck.digest, repo)
        if existing is not None:
            return existing, True, f"existing run: {existing}"

    branch = _free_branch(docs, repo, deck.branch)
    slug = _slug(deck.title)
    root = new_run_dir(docs, slug, date.today())
    paths = run_paths(root)
    _write_bytes_atomic(paths.deck, raw)
    # The effective configuration, read by the run from now on instead of the live files (I5).
    write_json_atomic(root / SNAPSHOT, config_to_dict(config))
    if deck.base_sha:
        base_ref, base_sha = deck.base_ref, head(repo, deck.base_sha)
    else:
        base_ref, base_sha = default_base(repo)
    # The worktree is named after the run directory, which is unique, so no two runs share it.
    ws = prepare_workspace(repo, run_branch=branch, base_sha=base_sha, slug=root.name,
                           use_worktree=config.run.worktree)
    # The operator's own untracked files (an in-place run works in their checkout): never swept,
    # never committed (R48').
    write_json_atomic(root / dsp.UNTRACKED_AT_START, {"paths": untracked_files(Path(ws))})
    meta = {
        "run_id": secrets.token_hex(6), "repo": str(repo), "deck_source": str(deck_path),
        "deck_digest": deck.digest, "base_ref": base_ref, "base_sha": base_sha, "branch": branch,
        "branch_requested": deck.branch,
        "workspace": str(Path(ws).absolute()), "slug": slug, "created_at": utc_now(),
        "supervisor_sessions": [],
    }
    write_json_atomic(paths.run_json, meta)
    append_event(paths, "run_started", run_id=meta["run_id"], base_sha=base_sha, branch=branch)
    note = (f"run branch: {branch} ({deck.branch} belongs to another run)"
            if branch != deck.branch else "")
    return root, False, note


# --------------------------------------------------------------------------
# next_action

def next_action(run_dir: Path) -> dict:
    try:
        return _next_action(run_dir)
    finally:
        _write_status(run_dir)


def _write_status(run_dir: Path) -> None:
    """Rewrite status.json at a checkpoint; a rendering error must never break the engine."""
    try:
        if not run_paths(Path(run_dir)).run_json.is_file():
            return
        from leadhand import status
        status.write_status(Path(run_dir))
    except Exception as e:
        print(f"leadhand: could not write status.json: {e}", file=sys.stderr)


def _next_action(run_dir: Path) -> dict:
    """Perform every mechanical step that is due, then return the supervisor's next action."""
    ctx = _load(run_dir, check_digest=False)
    deck_problem = _deck_problem(ctx)
    _charge_supervisor(ctx)
    for _ in range(MAX_STEPS):
        events = read_events(ctx.paths)
        ended = _ended(events)
        if ended is not None:
            _wrap_up(ctx, ended, events)
            return _done_action(ctx, ended, read_events(ctx.paths))
        if _needs_reconcile(events):
            # After a relaunch, nothing else happens before every in-flight step is reconciled
            # (it only records what really happened, so a pending cancel ends on true state).
            _reconcile(ctx, events)
            action = None
        elif _cancel_requested(ctx, events):
            # A cancel comes before anything new, and a relaunch never undoes it.
            action = _cancel_step(ctx, read_events(ctx.paths))
        elif _pause_requested(ctx, events):
            action = _pause_step(ctx, read_events(ctx.paths))
        elif deck_problem:
            raise EngineError(deck_problem)
        else:
            action = _step(ctx, events)
        if action is not None:
            return action
    raise EngineError(f"no action after {MAX_STEPS} steps; the run state does not settle")


def _charge_supervisor(ctx: _Run) -> None:
    """Record the supervisor's session in run.json and charge its new usage, once."""
    sid = os.environ.get("CLAUDE_CODE_SESSION_ID", "").strip()
    if not sid:
        return
    with state_lock(ctx.paths):
        meta = json.loads(ctx.paths.run_json.read_text(encoding="utf-8"))
        if sid not in meta.setdefault("supervisor_sessions", []):
            meta["supervisor_sessions"].append(sid)
            write_json_atomic(ctx.paths.run_json, meta)
        _charge_session(ctx, sid)


def _charge_earlier_sessions(ctx: _Run) -> None:
    """After a relaunch: charge what earlier supervisor sessions used after their last charge
    (they ended with the crash, so their transcripts are final)."""
    current = os.environ.get("CLAUDE_CODE_SESSION_ID", "").strip()
    with state_lock(ctx.paths):
        try:
            sessions = json.loads(ctx.paths.run_json.read_text(encoding="utf-8")).get("supervisor_sessions") or []
        except (OSError, ValueError):
            sessions = []
        for sid in sessions:
            if sid != current:
                _charge_session(ctx, sid)


def _charge_session(ctx: _Run, sid: str) -> None:
    """Charge a supervisor session's transcript from its checkpoint (caller holds the state lock)."""
    path = usage.claude_session_transcript(sid, _projects_dir())
    if path is None:
        return
    charges = usage.read_charges(ctx.paths)
    _, reached = usage.new_claude_usage(path, usage.charged_checkpoint(charges, "claude", sid))
    if not reached["messages"]:
        return
    prefix = f"supervisor:{sid}:"
    n = 1 + sum(1 for c in charges if c.charge_id.startswith(prefix))
    usage.charge(ctx.paths, charge_id=f"{prefix}{n}", source="claude", key=sid,
                 dispatch_id="supervisor", role="supervisor", path_or_thread=path)


def _pick(ctx: _Run, cards: dict[int, CardState], events: list[dict]) -> int | None:
    """One card in flight at a time: a card with open work always comes first; then a card
    that was cut but not dispatched yet; then an unparked card (it waits until nothing else
    is in flight); else the first pending card whose dependencies are all accepted."""
    order = dependency_order(ctx.deck)
    for n in order:
        if cards[n].state in ACTIVE_STATES:
            return n
    cut = [n for n in order if cards[n].state == "cut"]
    for n in cut:
        if _pending_unpark(events, n) is None:
            return n
    if cut:
        return cut[0]
    for n in order:
        if cards[n].state == "pending" and all(cards[d].state == "accepted"
                                               for d in ctx.card(n).depends_on if d in cards):
            return n
    return None


def _step(ctx: _Run, events: list[dict]) -> dict | None:
    """One mechanical step (returns None after appending an event) or an action."""
    problem = _run_head_problem(ctx, events)
    if problem and not _run_ref_guarded(ctx, events):
        # The run branch moved with no acceptance and no worker guard to explain it (I4).
        _stop_codex_runners(ctx, events)
        _end(ctx, read_events(ctx.paths), unreconciled=problem)
        return None
    if _finish_leftovers(ctx, events):
        return None
    cards = fold_cards(events, ctx.numbers)
    index = _dispatches(events)
    runs = _run_dispatches(index)
    if runs and runs[-1].result is None:
        # A gate or fix wave worker is using the workspace (a card unparked meanwhile waits).
        return _in_flight(ctx, runs[-1])
    if runs:
        # Its result is acted on before any card moves the workspace or the run branch, so its
        # guard is checked against what the worker saw.
        acted, action = _after_run_result(ctx, runs[-1], events, index, _accepted_cards(ctx, cards))
        if acted:
            return action
    n = _pick(ctx, cards, events)
    if n is None:
        return _end_of_run(ctx, cards, events)
    c = cards[n]
    if c.state == "pending":
        _no_open_dispatch(ctx, events, n)
        current = _current_branch(ctx)
        if _ensure_clean(ctx, events, branch=current or ctx.branch, why=f"before cutting card {n}",
                         untracked=True):
            return None
        branch, base = cut_card_branch(ctx.ws, ctx.branch, n)
        append_event(ctx.paths, "card_cut", card=n, branch=branch, base=base)
        return None
    if c.state == "cut":
        unpark = _pending_unpark(events, n)
        if unpark is not None:
            _resume_unparked(ctx, c, events, index, unpark["note"])
        else:
            _start_attempt(ctx, c, events, attempt=max(1, c.attempt), last_vendor=None, notes="")
        return None
    if c.state == "accepting":
        _finish_accept(ctx, n, c.accept_target)
        return None
    if c.state == "integrating":
        _integration_step(ctx, c, events, index)
        return None
    if c.state == "reviewed":
        return _after_verdict(ctx, c, events, index)
    if c.state == "fixing" and c.dispatch_kind != "fix":
        _request_fix(ctx, c, events, index)
        return None
    d = index.get(c.dispatch_id)
    if d is None:
        raise EngineError(f"card {n} is {c.state} but its dispatch {c.dispatch_id} is not recorded")
    if d.result is None:
        return _in_flight(ctx, d)
    return _after_result(ctx, c, d, events, index)


def _run_head_problem(ctx: _Run, events: list[dict]) -> str | None:
    """Why the run head is not the last accepted commit (else the base), or None. An
    acceptance whose fast-forward happened but is not recorded yet is not a problem."""
    run_ref = f"refs/heads/{ctx.branch}"
    now = _ref_sha(ctx, run_ref) if ctx.ws.is_dir() else None
    if now is None:
        return f"the run branch {ctx.branch} is missing"
    accepted = [e for e in events if e["kind"] == "accepted" and e.get("sha")]
    expected = accepted[-1]["sha"] if accepted else ctx.meta["base_sha"]
    if now == expected:
        return None
    after = accepted[-1]["seq"] if accepted else 0
    if any(e["kind"] == "accept_started" and e["seq"] > after and e.get("target") == now for e in events):
        return None
    return (f"the run branch {ctx.branch} is at {now[:12]}, not at the last accepted commit "
            f"{expected[:12]}{'' if accepted else ' (the base)'}")


def _run_ref_guarded(ctx: _Run, events: list[dict]) -> bool:
    """Whether a dispatch that guards the run branch will still check it: one with no result
    yet, a card's current dispatch whose result is not acted on yet (`reported`), the latest
    run-level dispatch whose result is not settled, or a violation not restored yet. Its guard
    then deals with a moved run branch (a violation), not this check."""
    run_ref = f"refs/heads/{ctx.branch}"
    found = {(e.get("dispatch_id"), e.get("n")) for e in events
             if e["kind"] == "violation" and e.get("stage") == "found" and _by(e) != "implementer"}
    restored = {(e.get("dispatch_id"), e.get("n")) for e in events
                if e["kind"] == "violation" and e.get("stage") == "restored" and _by(e) != "implementer"}
    if found - restored:
        return True
    index = _dispatches(events)
    cards = fold_cards(events, ctx.numbers)
    # Card states whose current dispatch `_step` hands to `_after_result` (which checks its guard).
    current = {c.dispatch_id for c in cards.values()
               if c.state in ACTIVE_STATES and c.state not in ("reviewed", "integrating", "accepting")
               and not (c.state == "fixing" and c.dispatch_kind != "fix")}
    runs = _run_dispatches(index)
    for d in index.values():
        if run_ref not in (d.req.get("refs") or {}):
            continue
        if d.req.get("card") is not None:
            if d.id in current:
                return True
        elif runs and d.id == runs[-1].id:
            settled = any(e.get("dispatch_id") == d.id and e["kind"] in ("gate_result", "verdict_recorded")
                          for e in events)
            if d.result is None or not settled:
                return True
    return False


def _card_summary(ctx: _Run, cards: dict[int, CardState]) -> list[dict]:
    """Each card's state, with why a card that is not accepted stopped (parked, or waits on a parked card)."""
    out = []

    def waits_on(n: int, seen: frozenset = frozenset()) -> list[int]:
        parked = []
        for d in ctx.card(n).depends_on:
            if d not in cards or d in seen:
                continue
            if cards[d].state == "parked":
                parked.append(d)
            elif cards[d].state != "accepted":
                parked += waits_on(d, seen | {n})
        return sorted(set(parked))

    for n in dependency_order(ctx.deck):
        c = cards[n]
        if c.state == "accepted":
            out.append({"card": n, "state": "accepted", "reason": ""})
        elif c.state == "parked":
            out.append({"card": n, "state": "parked", "reason": c.park_reason or "no reason recorded"})
        else:
            blockers = waits_on(n)
            reason = (f"waits on parked card{'s' if len(blockers) > 1 else ''} {', '.join(map(str, blockers))}"
                      if blockers else "waits on its dependencies")
            out.append({"card": n, "state": c.state, "reason": reason})
    return out


# -- dispatching -----------------------------------------------------------

def _resolve_or_park(ctx: _Run, card: Card, *, role: str, attempt: int, last_vendor: str | None) -> Resolution | None:
    try:
        return resolve(ctx.config, role=role, card=card, attempt=attempt, last_implementer_vendor=last_vendor,
                       installed=installed_vendors(), profile=ctx.deck.profile)
    except LadderExhausted:
        reason = "ladder exhausted"
    except PinnedVendorMissing:
        reason = "pinned vendor not installed"
    except ConfigError as e:
        reason = str(e)
    append_event(ctx.paths, "parked", card=card.number, reason=reason)
    return None


def _request(ctx: _Run, c: CardState, events: list[dict], *, kind: str, attempt: int, role: str, route: Route,
             extra: dict[str, str], resume: _Dispatch | None = None, notes: str = "",
             continues: str | None = None) -> dict:
    """Write the brief and append `dispatch_requested`. Deterministic: after a crash before the
    event, the next call computes the same dispatch ID and rewrites the same brief. An
    implementer-side dispatch records the card branch's tip as `start_tip`, so a restart can
    tell the commits its worker made; a `continue` names the dispatch it `continues`."""
    n = 1 + sum(1 for e in events if e["kind"] == "dispatch_requested" and e.get("card") == c.number
                and e.get("attempt") == attempt and e.get("dispatch_kind") == kind)
    did = f"c{c.number}-a{attempt}-{kind}-{n}"
    full = {"base": _card_base(ctx, c, events), "branch": c.branch or "", "test_first": ctx.config.run.test_first,
            **extra}
    guard: dict[str, Any] = {}
    if c.branch:
        _check_out(ctx, events, c.number, c.branch)
        if kind in CHECK_KINDS:
            # Implementer leftovers are preserved and cleared first (R30), so they are not
            # blamed on the check.
            _ensure_clean(ctx, events, branch=c.branch, why=f"before {kind} {did}")
            # What the worker must leave as it found it (R14'): the card tip on the card branch,
            # and every ref of the run.
            guard = {"expected_branch": c.branch, "expected_head": head(ctx.ws, f"refs/heads/{c.branch}"),
                     "refs": _ref_snapshot(ctx)}
            if kind == "verify" and extra.get("tip") != guard["expected_head"]:
                raise EngineError(f"card {c.number}: verify of {extra.get('tip')} requested, but the card "
                                  f"branch is at {guard['expected_head']}")
        elif kind in IMPLEMENTER_KINDS:
            # An implementer-side worker commits on (an integrate worker rewrites) the card branch,
            # but must leave the run branch alone: moving it is a violation (I4).
            run_ref = f"refs/heads/{ctx.branch}"
            guard = {"refs": {run_ref: head(ctx.ws, run_ref)}, "ref_scope": [run_ref]}
        if kind in IMPLEMENTER_KINDS:
            guard["start_tip"] = head(ctx.ws, f"refs/heads/{c.branch}")
            # What is untracked already is not this dispatch's: a partial-work commit leaves it (I3).
            guard["untracked"] = untracked_files(ctx.ws)
    if continues is not None:
        guard["continues"] = continues
    brief, report = _write_brief(kind, ws=ctx.ws, deck=ctx.deck, card=ctx.card(c.number), attempt=attempt,
                                 dispatch_id=did, extra=full, notes=notes)
    return _append_request(ctx, did, card=c.number, attempt=attempt, kind=kind, role=role, route=route,
                           brief=brief, report=report, extra=extra, notes=notes, resume=resume, **guard)


def _write_brief(kind: str, **kw: Any) -> tuple[Path, Path]:
    """write_brief, refusing a scratch folder a worker replaced with a symlink or a file (L2):
    the run stops with an error naming the path until the operator removes it."""
    try:
        return write_brief(kind, **kw)
    except ScratchError as e:
        raise EngineError(str(e)) from None


def _append_request(ctx: _Run, did: str, *, card: int | None, attempt: int, kind: str, role: str, route: Route,
                    brief: Path, report: Path, extra: dict, notes: str, resume: _Dispatch | None,
                    **more: Any) -> dict:
    """Append `dispatch_requested` for a brief already written (with the notes appended)."""
    # No worker can have written this dispatch's report yet, nor the supervisor its reply file:
    # one already there is left over from an earlier run that used the same workspace. Never
    # removed through a symlinked scratch folder (L2).
    try:
        for name in (Path(report).name, REPLY_FILE):
            remove_scratch_file(ctx.ws, card, did, name)
    except ScratchError as e:
        raise EngineError(str(e)) from None
    resume_worker = resume_session = None
    if resume is not None:
        if route.vendor == "claude":
            # A Claude worker dies with its supervisor session (P9): one started before the
            # latest relaunch cannot be resumed, so a fresh worker gets the brief instead.
            resume_worker = (resume.started["worker_id"]
                             if resume.started and not _gone(read_events(ctx.paths), resume) else None)
        else:
            resume_session = (resume.result or {}).get("session_id")
    try:
        minutes = dsp.limits_for(ctx.config, role).minutes
    except ConfigError as e:
        raise EngineError(str(e)) from None
    return append_event(ctx.paths, "dispatch_requested", dispatch_id=did, card=card, attempt=attempt,
                        dispatch_kind=kind, role=role, vendor=route.vendor, model=route.model,
                        effort=route.effort, brief=str(brief), report=str(report), extra=extra, notes=notes,
                        resumes=resume.id if resume is not None else None, resume_worker=resume_worker,
                        resume_session=resume_session, minutes=minutes, **more)


def _current_branch(ctx: _Run) -> str | None:
    try:
        return git(ctx.ws, "symbolic-ref", "--quiet", "--short", "HEAD")
    except GitError:
        return None                         # detached


def _check_out(ctx: _Run, events: list[dict], card: int, branch: str) -> None:
    """Put the workspace on the card branch when HEAD is on another one (after another card
    ran, e.g. when a parked card resumes). Never while another card has a dispatch open;
    leftovers in the way are preserved and cleared first (R30). Never creates the branch."""
    current = _current_branch(ctx)
    if current == branch:
        return
    _no_open_dispatch(ctx, events, card)
    _ensure_clean(ctx, events, branch=current or branch, why=f"before switching to {branch}", untracked=True)
    if _current_branch(ctx) != branch:
        git(ctx.ws, "checkout", "--quiet", branch, "--")


def _no_open_dispatch(ctx: _Run, events: list[dict], card: int) -> None:
    """Refuse to touch the workspace for `card` while another card's worker, or (for a card)
    a run-level gate or fix wave worker, may be using it."""
    cards = fold_cards(events, ctx.numbers)
    for d in _dispatches(events).values():
        other = d.req.get("card")
        if (d.result is None and other != card and other in cards
                and cards[other].state not in ("parked", "accepted") and cards[other].dispatch_id == d.id):
            raise EngineError(f"card {other} has dispatch {d.id} open; card {card} must wait for it")
        if d.result is None and other is None and card is not None:
            raise EngineError(f"the run's dispatch {d.id} is open; card {card} must wait for it")


def _card_base(ctx: _Run, c: CardState, events: list[dict]) -> str:
    """The commit the card's work starts from: its cut base, or, once it has been integrated,
    where its branch meets the run branch (the run tip it was rebased onto)."""
    if c.branch and any(e["kind"] == "integration_started" and e.get("card") == c.number for e in events):
        return git(ctx.ws, "merge-base", f"refs/heads/{ctx.branch}", f"refs/heads/{c.branch}")
    return c.base or ""


def _budget_seconds(req: dict) -> float:
    minutes = req["minutes"] + (CODEX_GRACE_MINUTES if req["vendor"] == "codex" else 0)
    return minutes * dsp.minute_seconds()


def _work_branch(ctx: _Run, req: dict) -> str:
    """The branch a dispatch works on: the card branch, the final fix branch, or the run branch.
    The Codex runner commits implementer work there (ruling R39)."""
    if req["card"] is not None:
        return (_card_state(ctx, read_events(ctx.paths), req["card"]).branch
                or card_branch_name(ctx.branch, req["card"]))
    return _final_branch(ctx) if req["dispatch_kind"] == "final_fix" else ctx.branch


def _codex_command(ctx: _Run, req: dict) -> list[str]:
    cmd = [str(dsp.CLI_PATH), "dispatch", "--run", str(ctx.root), "--dispatch-id", req["dispatch_id"],
           *(["--card", str(req["card"])] if req["card"] is not None else []),
           "--attempt", str(req["attempt"]), "--role", req["role"],
           "--model", req["model"], "--effort", req["effort"], "--brief", req["brief"],
           "--report", req["report"], "--cwd", str(ctx.ws),
           "--kind", req["dispatch_kind"], "--branch", _work_branch(ctx, req)]
    if req.get("resume_session"):
        cmd += ["--resume", req["resume_session"]]
    return cmd


def _reply_file(ctx: _Run, card: int | None, dispatch_id: str) -> Path:
    """Where the supervisor writes a worker's reply for `record result --reply-file` (R38): in
    the dispatch's scratch folder inside the workspace, which is git-ignored, so the supervisor
    writes it with its Write tool and records it with a plain command (no heredoc)."""
    return scratch_dir(ctx.ws, card, dispatch_id) / REPLY_FILE


def _check_reply_file(ctx: _Run, d: _Dispatch, path: str) -> None:
    """Only the dispatch's own reply file is read (security review L1): the CLI runs outside the
    sandbox, so any other path would copy a file the worker cannot read into the run."""
    expected = _reply_file(ctx, d.req["card"], d.id)
    if os.path.abspath(path) != os.path.abspath(expected):
        raise EngineError(f"refused: --reply-file must be dispatch {d.id}'s reply file {expected}")
    if os.path.islink(expected) or not resolves_in_place(ctx.ws, expected):
        raise EngineError(f"refused: the reply file {expected} is a symlink or lies behind one")


def _read_reply_file(path: str) -> str:
    """The reply file's text. A missing or unreadable file gives a note instead; the status then
    comes from the report head, as for any reply without a status line. Never follows a symlink
    and reads only a regular file."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as f:
            if not stat.S_ISREG(os.fstat(f.fileno()).st_mode):
                raise OSError(0, "not a regular file")
            return f.read().decode("utf-8", errors="replace")
    except OSError as e:
        return (f"(leadhand: the reply file {path} could not be read: {e.strerror or e}; "
                "the status comes from the report)\n")


def _dispatch_action(ctx: _Run, req: dict) -> dict:
    brief, report = Path(req["brief"]), Path(req["report"])
    if req["vendor"] == "codex":
        agent, agent_model, agent_effort, resume = CODEX_AGENT, None, None, None
        message = dispatch_message(agent, brief, report, codex_command=_codex_command(ctx, req))
    else:
        agent, agent_model, agent_effort = agent_for(req["role"], "claude"), req["model"], req["effort"]
        resume = {"worker_id": req["resume_worker"]} if req.get("resume_worker") else None
        message = dispatch_message(agent, brief, report)
    action = {"action": "dispatch", "dispatch_id": req["dispatch_id"], "kind": req["dispatch_kind"],
              "card": req["card"], "attempt": req["attempt"], "role": req["role"], "agent": agent,
              "agent_model": agent_model, "agent_effort": agent_effort,
              "route": {"vendor": req["vendor"], "model": req["model"], "effort": req["effort"]},
              "message": message, "brief": req["brief"], "report": req["report"], "resume": resume,
              "reply_file": str(_reply_file(ctx, req["card"], req["dispatch_id"])),
              "deadline": _format_time(_now() + _budget_seconds(req))}
    if req["card"] is None:
        action.update(gate=req.get("gate"), wave=req.get("wave"))
    return action


def _in_flight(ctx: _Run, d: _Dispatch) -> dict | None:
    if d.started is None:
        return _dispatch_action(ctx, d.req)       # no worker is known to exist
    worker = d.started["worker_id"]
    deadline = d.started["deadline"]
    orphan = d.req["vendor"] == "codex" and _gone(read_events(ctx.paths), d)
    if d.req["vendor"] == "codex":
        # A Codex dispatch's result is the runner's result.json, recorded as soon as it is
        # there: by the proxy's `record result`, by this step, or (for a runner that
        # outlived its supervisor session, which has no proxy left) only by this step.
        result = dsp.load_result(ctx.root / "workers" / d.id)
        if result is not None:
            _store_result(ctx, d, worker, "\n".join(dsp.reply_lines(result)),
                          stopped=result.exit_reason == "cancelled", charge=not d.started.get("reconciled"))
            if orphan or d.started.get("reconciled") or _now() < _parse_time(deadline):
                return None
            # The runner finished, but this session's proxy is still out past the deadline: stop it.
            return {"action": "stop_worker", "dispatch_id": d.id, "worker_id": worker, "vendor": "codex",
                    "record_result": False}
    if _now() < _parse_time(deadline):
        reason = (f"{d.kind} worker for {_whose(d.req)} is running" if not orphan else
                  f"the Codex worker for {_whose(d.req)} outlived an earlier supervisor session; "
                  "no completion notice comes for it, and the timer reports its result")
        return {"action": "wait", "dispatch_id": d.id, "worker_id": worker, "deadline": deadline,
                "reply_file": str(_reply_file(ctx, d.req["card"], d.id)), "reason": reason}
    synthesized = bool(d.started.get("reconciled"))
    if d.req["vendor"] == "codex":
        result = _cancel_codex(ctx, d)
        # A runner that finished before the cancel keeps its own status (the proxy only hung).
        _store_result(ctx, d, worker, "\n".join(dsp.reply_lines(result)),
                      stopped=result.exit_reason == "cancelled", charge=not synthesized)
        if synthesized:
            return None           # no agent of this session runs it: nothing for the supervisor to stop
        return {"action": "stop_worker", "dispatch_id": d.id, "worker_id": worker, "vendor": "codex",
                "record_result": False}
    if synthesized:
        _store_result(ctx, d, worker, "", interrupted=True, charge=False)
        return None
    return {"action": "stop_worker", "dispatch_id": d.id, "worker_id": worker, "vendor": "claude",
            "record_result": True}


def _whose(req: dict) -> str:
    return "the run" if req.get("card") is None else f"card {req['card']}"


def _cancel_codex(ctx: _Run, d: _Dispatch) -> dsp.DispatchResult:
    """Stop the dispatch's runner (and Codex with its tool processes) and finalize result.json.
    When the proxy never ran the command, the request is written first, so a late run of the
    same command finds the cancelled result instead of starting Codex."""
    req = d.req
    try:
        wd = dsp.write_request(dsp.request_for(
            ctx.root, d.id, card=req["card"], attempt=req["attempt"], role=req["role"], model=req["model"],
            effort=req["effort"], brief=Path(req["brief"]), report=Path(req["report"]), cwd=ctx.ws,
            resume=req.get("resume_session"), kind=req["dispatch_kind"], branch=_work_branch(ctx, req)))
        return dsp.cancel(wd, timeout=dsp.CANCEL_TIMEOUT)
    except (dsp.DispatchError, ConfigError, OSError) as e:
        raise EngineError(f"could not stop Codex dispatch {d.id}: {e}") from None


def _start_attempt(ctx: _Run, c: CardState, events: list[dict], *, attempt: int, last_vendor: str | None,
                   notes: str) -> None:
    card = ctx.card(c.number)
    res = _resolve_or_park(ctx, card, role="implementer", attempt=attempt, last_vendor=last_vendor)
    if res is not None:
        _request(ctx, c, events, kind="implement", attempt=attempt, role=res.role, route=res.route,
                 extra={}, notes=notes)


def _escalate(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch], why: str,
              review: _Dispatch | None = None, label: str = "The review findings") -> None:
    """A fresh worker on the next rung of the ladder, with the previous report in its brief."""
    impl = _card_dispatches(index, c.number, kinds=IMPLEMENTER_KINDS)
    last = max(impl, key=lambda d: d.req["seq"])
    attempt = last.req["attempt"]
    lines = ["## Previous attempt", "",
             f"Attempt {attempt} on this card {why}. Read what it reported before you start; the card "
             "branch holds its commits. Keep what is sound and fix or replace the rest.", ""]
    for name, d in (("Its report", last), (label, review)):
        if d is not None and d.result is not None:
            lines.append(f"- {name}: {_report_copy(d)}")
    _start_attempt(ctx, c, events, attempt=attempt + 1, last_vendor=last.req["vendor"], notes="\n".join(lines))


def _report_copy(d: _Dispatch) -> str:
    """The run-folder copy of the dispatch's report, or of its reply when it wrote no report."""
    files = (d.result or {}).get("files") or {}
    return files.get("report") or files.get("reply") or ""


def _last_implementer(index: dict[str, _Dispatch], card: int, attempt: int, before_seq: int) -> _Dispatch:
    found = [d for d in _card_dispatches(index, card, attempt=attempt, kinds=IMPLEMENTER_KINDS)
             if d.started is not None and d.req["seq"] < before_seq]
    if not found:
        raise EngineError(f"card {card}: no implementer worker in attempt {attempt}")
    return max(found, key=lambda d: d.req["seq"])


def _request_review(ctx: _Run, c: CardState, events: list[dict], impl: _Dispatch) -> None:
    res = _resolve_or_park(ctx, ctx.card(c.number), role="reviewer", attempt=1, last_vendor=impl.req["vendor"])
    if res is None:
        return
    tip = impl.result.get("tip") or c.tip
    _request(ctx, c, events, kind="review", attempt=impl.req["attempt"], role=res.role, route=res.route,
             extra={"diff_range": f"{_card_base(ctx, c, events)}..{tip}", "implementer_report": _report_copy(impl)})


def _request_fix(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch]) -> None:
    rounds = [e for e in events if e["kind"] == "fix_round" and e.get("card") == c.number]
    fix = rounds[-1]
    impl = _last_implementer(index, c.number, fix["attempt"], fix["seq"])
    _request(ctx, c, events, kind="fix", attempt=fix["attempt"], role=impl.req["role"], route=impl.route,
             extra={"findings": fix["findings"], "round": str(fix["n"])}, resume=impl)


# -- deciding after a result ------------------------------------------------

def _after_result(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict],
                  index: dict[str, _Dispatch]) -> dict | None:
    r = d.result
    if "refs" in d.req and not _workspace_kept(ctx, c, d, events):
        return None                         # the result is void; the same kind was requested again (or parked)
    if d.kind in IMPLEMENTER_KINDS and _left_operation(ctx, c, d, events):
        # A rebase, merge or cherry-pick left in progress is the worker's failure: its state
        # is preserved and cleared, and the dispatch counts as BLOCKED (environment).
        events = read_events(ctx.paths)
        return _worker_failed(ctx, c, d, events, _dispatches(events))
    if r.get("interrupted"):
        _after_interrupted(ctx, c, d, events, index)
        return None
    if d.kind == "verify":
        if r.get("result") == "pass":
            _accept(ctx, c.number, d.req["extra"]["tip"])
        elif r.get("result") == "fail":
            _escalate(ctx, c, events, index, "failed the deck's checks after integration", review=d,
                      label="The verifier's report")
        else:
            return _worker_failed(ctx, c, d, events, index)
        return None
    if d.kind in REVIEW_KINDS:
        if r.get("verdict") in ("clean", "defects"):
            append_event(ctx.paths, "verdict_recorded", card=c.number, verdict=r["verdict"],
                         findings=_report_copy(d), dispatch_id=d.id)
            return None
        return _worker_failed(ctx, c, d, events, index)
    status = r["status"]
    if status == "READY" and _integrate_origin(index, d) is not None:
        _after_integrate(ctx, c, d, events, index)
        return None
    if status == "READY":
        fix = _fix_round(events, c.number, d.req["attempt"], before_seq=d.req["seq"])
        if fix is not None:
            return _after_fix(ctx, c, d, fix, events, index)
        card = ctx.card(c.number)
        tip = r.get("tip") or c.tip
        if (ctx.config.run.card_review == "by-risk" and card.risk == "low" and not card.hard
                and tip != _attempt_start_tip(index, c, d.req["attempt"])):
            _accept(ctx, c.number, tip)
        else:
            _request_review(ctx, c, events, d)
        return None
    if status == "QUESTION":
        ruling = next((e for e in events if e["kind"] == "ruling" and e.get("dispatch_id") == d.id), None)
        if ruling is None:
            report = _report_copy(d)
            m = _QUESTION.search(_read_text(report) or "")
            return {"action": "answer", "card": c.number, "dispatch_id": d.id,
                    "worker_id": d.started["worker_id"], "report": report,
                    "question": m.group(1).strip() if m else ""}
        _request(ctx, c, events, kind="answer", attempt=d.req["attempt"], role=d.req["role"], route=d.route,
                 extra={"ruling": ruling["text"]}, resume=d)
        return None
    if status == "SPENT":
        _escalate(ctx, c, events, index, "ended SPENT")
        return None
    return _worker_failed(ctx, c, d, events, index)


def _attempt_start_tip(index: dict[str, _Dispatch], c: CardState, attempt: int) -> str | None:
    """Where the card branch stood when the attempt's first implementer-side dispatch was
    requested (else the card's base). A READY at that tip changed nothing: with
    `card_review = "by-risk"` it still goes to review, never straight to acceptance (I8)."""
    first = sorted(_card_dispatches(index, c.number, attempt=attempt, kinds=IMPLEMENTER_KINDS),
                   key=lambda x: x.req["seq"])
    return (first[0].req.get("start_tip") if first else None) or c.base


def _worker_failed(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict],
                   index: dict[str, _Dispatch]) -> dict | None:
    """BLOCKED from any worker, or a reviewer that ended without a verdict.

    A result that already ended the run (an `outcome` after it) is seen again only after the
    run was resumed: the work is dispatched again as the same attempt, and the retry budget
    counts only results after the latest ending a `resumed` followed (a relaunch without an
    ending keeps the budget)."""
    r = d.result
    broken = _broken_dispatches(events)

    def head_of(x: _Dispatch) -> tuple[str, str]:
        if x.id in broken:
            return "BLOCKED", "environment"
        return x.result["status"], x.result.get("blocked_by") or ""

    status, blocked_by = head_of(d)
    if not any(e["kind"] == "outcome" and e["seq"] > r["seq"] for e in events):
        if status == "BLOCKED" and blocked_by == "usage limit":
            _end_blocked(ctx, events, "provider capacity")
            return None
        # The budget restarts only after an ending the run was resumed from, not on every relaunch.
        since = _reopened_since(events)
        voided = _voided(events)
        same = [x for x in _card_dispatches(index, c.number, attempt=d.req["attempt"], kinds=(d.kind,))
                if x.result is not None and x.result["seq"] > since and x.id not in voided
                and not x.result.get("interrupted")]
        if status == "BLOCKED":
            failures = [x for x in same if head_of(x) != ("BLOCKED", "usage limit")
                        and head_of(x)[0] == "BLOCKED"]
            if len(failures) >= 2:
                _end_blocked(ctx, events, "environment")
                return None
        else:
            failures = [x for x in same if head_of(x)[0] != "BLOCKED" and x.result.get("verdict") is None
                        and x.result.get("result") is None]
            if len(failures) >= 2:
                what = "a result" if d.kind == "verify" else "a verdict"
                append_event(ctx.paths, "parked", card=c.number,
                             reason=f"{d.kind} ended {status} twice without {what}")
                return None
    resume = index.get(d.req.get("resumes") or "")
    _request(ctx, c, events, kind=d.kind, attempt=d.req["attempt"], role=d.req["role"], route=d.route,
             extra=d.req.get("extra") or {}, resume=resume, notes=d.req.get("notes") or "")
    return None


def _after_verdict(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch]) -> dict | None:
    verdict_ev = [e for e in events if e["kind"] == "verdict_recorded" and e.get("card") == c.number][-1]
    review = index[verdict_ev["dispatch_id"]]
    attempt = review.req["attempt"]
    if c.verdict == "clean":
        _accept(ctx, c.number, c.tip)
        return None
    if _fix_round(events, c.number, attempt) is None:
        append_event(ctx.paths, "fix_round", card=c.number, n=c.fix_rounds + 1, attempt=attempt,
                     pre_tip=c.tip, findings=c.findings, review=review.id)
        return None
    _escalate(ctx, c, events, index, "still had review defects after its fix round", review=review)
    return None


def _after_fix(ctx: _Run, c: CardState, d: _Dispatch, fix: dict, events: list[dict],
               index: dict[str, _Dispatch]) -> dict | None:
    """A READY in the fix round: production changes get a narrow re-review; a fix that only
    touched tests and docs gets the supervisor's diff check."""
    pre, tip = fix["pre_tip"], d.result.get("tip") or c.tip
    changed = changed_paths(ctx.ws, pre, tip) if pre and tip and pre != tip else []
    if any(not is_test_or_doc_path(p) for p in changed):
        review = index[fix["review"]]
        _request(ctx, c, events, kind="rereview", attempt=d.req["attempt"], role=review.req["role"],
                 route=review.route, resume=review,
                 extra={"diff_range": f"{_card_base(ctx, c, events)}..{tip}", "findings": fix["findings"],
                        "implementer_report": _report_copy(d)})
        return None
    check = next((e for e in events if e["kind"] == "diff_checked" and e.get("dispatch_id") == d.id), None)
    if check is not None:
        if check.get("ok"):
            _accept(ctx, c.number, tip)
        else:
            _escalate(ctx, c, events, index, f"had its fix rejected by the diff check: {check.get('note') or ''}")
        return None
    diff = _fix_diff_path(ctx, d)
    diff.parent.mkdir(parents=True, exist_ok=True)
    text = git(ctx.ws, "diff", "--no-ext-diff", "--no-color", pre, tip, "--") if changed else ""
    _write_bytes_atomic(diff, (text + "\n" if text else "").encode("utf-8", "surrogateescape"))
    return {"action": "check_diff", "card": c.number, "dispatch_id": d.id, "diff": str(diff),
            "findings": fix["findings"], "paths": changed}


def _fix_diff_path(ctx: _Run, d: _Dispatch) -> Path:
    return ctx.card_dir(d.req["card"], d.id) / "fix.diff"


def _accept(ctx: _Run, card: int, target: str | None) -> None:
    """Accept a card: fast-forward the run branch to its reviewed tip (intent, move, completion).

    A card branch that does not contain the run tip is integrated first; a card that was
    integrated is accepted only at a tip a verify passed on."""
    if not target:
        raise EngineError(f"card {card}: no tip to accept")
    events = read_events(ctx.paths)
    c = fold_cards(events, ctx.numbers)[card]
    branch = c.branch or card_branch_name(ctx.branch, card)
    if needs_integration(ctx.ws, ctx.branch, branch):
        k = 1 + sum(1 for e in events if e["kind"] == "integration_started" and e.get("card") == card)
        integ = append_event(ctx.paths, "integration_started", card=card,
                             onto=head(ctx.ws, f"refs/heads/{ctx.branch}"), tip=head(ctx.ws, f"refs/heads/{branch}"),
                             keep=f"refs/leadhand/{ctx.meta['run_id']}/integration/{card}-{k}")
        _rebase(ctx, card, branch, integ)
        return
    integrated = any(e["kind"] == "integration_started" and e.get("card") == card for e in events)
    if integrated and not _verified(events, card, target):
        _request_verify(ctx, c, events, target)
        return
    append_event(ctx.paths, "accept_started", card=card, target=target)
    crash_point("before_fast_forward")
    _finish_accept(ctx, card, target)


def _finish_accept(ctx: _Run, card: int, target: str) -> None:
    """Idempotent: a run branch that already contains the target is left where it is."""
    fast_forward(ctx.ws, ctx.branch, target)
    crash_point("after_fast_forward")
    append_event(ctx.paths, "accepted", card=card, sha=target)


# -- integration ------------------------------------------------------------

def _contains(ctx: _Run, branch: str, sha: str) -> bool:
    """Whether the branch's tip has `sha` in its history."""
    return is_ancestor(ctx.ws, sha, f"refs/heads/{branch}")


def _verified(events: list[dict], card: int, target: str) -> bool:
    """A verify of exactly `target` passed (and was not voided)."""
    voided = _voided(events)
    return any(d.kind == "verify" and d.req.get("card") == card and (d.req.get("extra") or {}).get("tip") == target
               and d.result is not None and d.result.get("result") == "pass" and d.id not in voided
               for d in _dispatches(events).values())


def _request_verify(ctx: _Run, c: CardState, events: list[dict], target: str) -> None:
    res = _resolve_or_park(ctx, ctx.card(c.number), role="verifier", attempt=1, last_vendor=None)
    if res is not None:
        _request(ctx, c, events, kind="verify", attempt=max(1, c.attempt), role=res.role, route=res.route,
                 extra={"tip": target})


def _rebase(ctx: _Run, card: int, branch: str, integ: dict) -> None:
    """Rebase the card branch onto the run tip recorded by `integration_started`, then record
    `rebased` or `conflict`. Repeat-safe: a card branch that already contains the run tip
    counts as rebased; on conflict rebase_onto has aborted the rebase. The pre-integration
    tip is kept on the integration's `keep` ref first; anything left in the workspace (an
    implementer's leftovers, or a rebase a dead process left in progress) is preserved and
    cleared, which puts the card branch back where it was."""
    if integ.get("keep"):
        git(ctx.ws, "update-ref", "-m", "leadhand: card tip before integration", integ["keep"], integ["tip"])
    crash_point("before_rebase")
    _ensure_clean(ctx, read_events(ctx.paths), branch=branch, why=f"before rebasing card {card}", untracked=True)
    onto = integ["onto"]
    conflicts = [] if _contains(ctx, branch, onto) else rebase_onto(ctx.ws, branch, onto)
    crash_point("after_rebase")
    if conflicts:
        append_event(ctx.paths, "conflict", card=card, onto=onto, paths=conflicts)
    else:
        append_event(ctx.paths, "rebased", card=card, onto=onto, tip=head(ctx.ws, f"refs/heads/{branch}"))


def _integration_step(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch]) -> None:
    """The card is integrating and nothing is dispatched for the step: rebase (again, after a
    crash), dispatch an integrate attempt for a conflict, or re-review a rebased card."""
    steps = [e for e in events if e.get("card") == c.number
             and e["kind"] in ("integration_started", "rebased", "conflict")]
    integ = [e for e in steps if e["kind"] == "integration_started"][-1]
    last = steps[-1]
    branch = c.branch or card_branch_name(ctx.branch, c.number)
    if last["kind"] == "integration_started":
        _rebase(ctx, c.number, branch, integ)
    elif last["kind"] == "conflict":
        _request_integrate(ctx, c, events, index, last)
    else:
        _request_integration_review(ctx, c, events, index, last)


def _request_integrate(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch],
                       conflict: dict) -> None:
    """A new attempt on the route of the card's last implementer attempt (not escalated) that
    may rebase the card branch, and only it, onto the run tip."""
    card = ctx.card(c.number)
    last = max(_card_dispatches(index, c.number, kinds=IMPLEMENTER_KINDS), key=lambda d: d.req["seq"])
    attempt = max(d.req["attempt"] for d in _card_dispatches(index, c.number)) + 1
    cut = max(e["seq"] for e in events if e["kind"] == "card_cut" and e.get("card") == c.number)
    landed = [ctx.card(e["card"]) for e in events if e["kind"] == "accepted" and e["seq"] > cut
              and e.get("card") != c.number and e.get("card") in ctx.numbers]
    run_intent = "; ".join(f"card {x.number} ({x.title}): {x.done_when}" for x in landed)
    route = last.route
    if route.vendor != "claude":
        # Rebasing writes the Git directory, which Codex's sandbox refuses (ruling R39): the
        # integrate attempt runs on the role's Claude route, not escalated.
        route = None
        if "claude" in installed_vendors():
            try:
                # A card pinned to Codex still integrates on Claude: the pin is set aside.
                route = resolve(ctx.config, role=last.req["role"], card=replace(card, route_pin=None), attempt=1,
                                installed=frozenset({"claude"}), profile=ctx.deck.profile).route
            except (LadderExhausted, PinnedVendorMissing, ConfigError):
                route = None
        if route is None or route.vendor != "claude":
            append_event(ctx.paths, "parked", card=c.number, reason="integration needs a Claude worker")
            return
    _request(ctx, c, events, kind="integrate", attempt=attempt, role=last.req["role"], route=route,
             extra={"run_tip": conflict["onto"], "conflicts": ", ".join(conflict["paths"]),
                    "card_intent": f"card {card.number} ({card.title}): {card.done_when}",
                    "run_intent": run_intent or "(no card was accepted since this card was cut)"})


def _after_integrate(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict],
                     index: dict[str, _Dispatch]) -> None:
    """An integrate attempt reported READY: its branch must now contain the run tip."""
    integ = [e for e in events if e["kind"] == "integration_started" and e.get("card") == c.number][-1]
    branch = c.branch or card_branch_name(ctx.branch, c.number)
    onto = integ["onto"]
    kept = (f" The card's commits before integration are kept on {integ['keep']}."
            if integ.get("keep") else "")
    if not _contains(ctx, branch, onto):
        _escalate(ctx, c, events, index,
                  "reported READY without rebasing the card branch onto the run tip." + kept)
        return
    tip = d.result.get("tip") or head(ctx.ws, f"refs/heads/{branch}")
    old_base = git(ctx.ws, "merge-base", onto, integ["tip"])
    if changed_paths(ctx.ws, old_base, integ["tip"]) and not changed_paths(ctx.ws, onto, tip):
        # The card's work vanished (`rebase --skip`, a reset to the run tip): SPENT, not integrated.
        _escalate(ctx, c, events, index,
                  "ended SPENT: its integration left none of the card's changes on top of the run tip." + kept)
        return
    append_event(ctx.paths, "rebased", card=c.number, onto=onto, tip=tip, dispatch_id=d.id)


def _integrate_origin(index: dict[str, _Dispatch], d: _Dispatch | None) -> _Dispatch | None:
    """The integrate dispatch `d` is, or that `d` answers or continues (through any chain)."""
    seen = set()
    while d is not None and d.id not in seen:
        if d.kind == "integrate":
            return d
        if d.kind not in ("answer", "continue"):
            return None
        seen.add(d.id)
        d = index.get(d.req.get("continues") or d.req.get("resumes") or "")
    return None


def _request_integration_review(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch],
                                rebased: dict) -> None:
    """Re-review the integrated diff with a fresh reviewer. Every integrated card gets it,
    including a low-risk card that `card_review = "by-risk"` let through without a review."""
    impl = [d for d in _card_dispatches(index, c.number, kinds=IMPLEMENTER_KINDS) if d.result is not None]
    last = max(impl, key=lambda d: d.req["seq"]) if impl else None
    res = _resolve_or_park(ctx, ctx.card(c.number), role="reviewer", attempt=1,
                           last_vendor=last.req["vendor"] if last else None)
    if res is None:
        return
    verdicts = [e for e in events if e["kind"] == "verdict_recorded" and e.get("card") == c.number]
    findings = (verdicts[-1].get("findings") if verdicts else "") or "(none: no review findings before integration)"
    onto, tip = rebased["onto"], rebased["tip"]
    notes = ("## Integration\n\n"
             f"Other cards were accepted while this card waited, so its branch was rebased onto the run "
             f"tip {onto}. Review the integrated diff {onto}..{tip} as a whole. The findings above are "
             "from the card's review before integration, if any; check that the card still does what it "
             "should on top of the run tip.")
    _request(ctx, c, events, kind="rereview", attempt=max(1, c.attempt), role=res.role, route=res.route,
             extra={"diff_range": f"{onto}..{tip}", "findings": findings,
                    "implementer_report": (_report_copy(last) if last else "") or "(none recorded)"},
             notes=notes)


# -- unparking ----------------------------------------------------------------

def _pending_unpark(events: list[dict], card: int) -> dict | None:
    """The `unparked` event the card has not acted on yet (a card parked before it was cut
    keeps its note through the cut)."""
    last = None
    for e in events:
        if e.get("card") != card:
            continue
        if e["kind"] == "unparked":
            last = e
        elif e["kind"] in ("dispatch_requested", "parked"):
            last = None
    return last


def _current_session(ctx: _Run) -> str | None:
    """The supervisor session making this call: CLAUDE_CODE_SESSION_ID, else the last one run.json
    recorded."""
    sid = os.environ.get("CLAUDE_CODE_SESSION_ID", "").strip()
    if sid:
        return sid
    try:
        sessions = json.loads(ctx.paths.run_json.read_text(encoding="utf-8")).get("supervisor_sessions") or []
    except (OSError, ValueError):
        sessions = []
    return sessions[-1] if sessions else None


def _resume_unparked(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch], note: str) -> None:
    """Resume the card's last implementer worker with the operator's note as the answer when
    that worker can still be reached: a Claude worker started in the current supervisor
    session (Claude workers die with their session), or a Codex dispatch with a recorded
    thread. Otherwise start a fresh worker on the card's attempt with the note.

    R32: a card the violation cap parked re-runs the kind it was parked on instead, on the
    same route: a fresh reviewer or verifier; an integrate step keeps its resume chain."""
    parks = [e for e in events if e["kind"] == "parked" and e.get("card") == c.number]
    capped = index.get(parks[-1].get("cap_dispatch") or "") if parks else None
    if capped is not None:
        notes = (capped.req.get("notes") or "").rstrip("\n")
        notes = (notes + "\n\n" if notes else "") + (
            f"## The operator's note\n\nThis card was parked because {parks[-1]['reason']}. "
            f"The operator resumed it with:\n\n{note}")
        resume = None if capped.kind not in IMPLEMENTER_KINDS else index.get(capped.req.get("resumes") or "")
        _request(ctx, c, events, kind=capped.kind, attempt=capped.req["attempt"], role=capped.req["role"],
                 route=capped.route, extra=capped.req.get("extra") or {}, notes=notes, resume=resume)
        return
    started = [d for d in _card_dispatches(index, c.number, kinds=IMPLEMENTER_KINDS) if d.started is not None]
    last = max(started, key=lambda d: d.req["seq"]) if started else None
    if last is not None:
        if last.req["vendor"] == "claude":
            session = last.started.get("session")
            reachable = bool(session) and session == _current_session(ctx)
        else:
            reachable = bool((last.result or {}).get("session_id"))
        if reachable:
            _request(ctx, c, events, kind="answer", attempt=last.req["attempt"], role=last.req["role"],
                     route=last.route, extra={"ruling": note}, resume=last)
            return
    _start_attempt(ctx, c, events, attempt=max(1, c.attempt), last_vendor=None,
                   notes=f"## The operator's note\n\nThis card was parked; the operator resumed it with:\n\n{note}")


# -- violations: checks that change things, and implementer leftovers ----------

def _ref_snapshot(ctx: _Run) -> dict[str, str]:
    """ref -> object for the run branch, its card branches and this run's refs/leadhand/<run_id>/ (R14')."""
    out = git(ctx.ws, "for-each-ref", "--format=%(objectname) %(refname)", f"refs/heads/{ctx.branch}",
              f"refs/heads/{ctx.branch}--card-*", f"refs/leadhand/{ctx.meta['run_id']}/")
    snap = {}
    for line in out.splitlines():
        sha, _, name = line.partition(" ")
        if name:
            snap[name] = sha
    return snap


def _by(e: dict) -> str:
    return e.get("by") or "check"


def _voided(events: list[dict]) -> set[str]:
    """Dispatches whose result a check violation voided."""
    return {e["dispatch_id"] for e in events
            if e["kind"] == "violation" and _by(e) != "implementer" and e.get("dispatch_id")}


def _broken_dispatches(events: list[dict]) -> set[str]:
    """Implementer dispatches that left a Git operation in progress: they count as BLOCKED (environment)."""
    return {e["after_dispatch"] for e in events
            if e["kind"] == "violation" and _by(e) == "implementer" and e.get("stage") == "found"
            and e.get("operation") and e.get("after_dispatch")}


def _workspace_kept(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict]) -> bool:
    """True when the worker left what its dispatch guards as it found it: for a reviewer or
    verifier the card tip, a clean workspace and the run's refs; for an integrate worker the
    run branch. Otherwise (now or recorded earlier) the change is preserved and undone, the
    result is void, and the same kind is requested again on the same route; after
    VIOLATION_CAP voided runs of a kind the card is parked instead. Returns False then.

    Two phases: `violation{stage: found}` is written before anything is moved, so a crash
    mid-way still voids the result; `violation{stage: restored}` follows the restore."""
    if not _void(ctx, d, events):
        return True
    events = read_events(ctx.paths)
    since = max((e["seq"] for e in events if e["kind"] == "unparked" and e.get("card") == c.number), default=0)
    count = [e for e in events if e["kind"] == "violation" and _by(e) != "implementer"
             and e.get("stage") == "found" and e.get("card") == c.number
             and e.get("dispatch_kind") == d.kind and e["seq"] > since]
    if len(count) > VIOLATION_CAP:
        paths = sorted({p for e in count for p in e.get("paths") or []})
        refs = sorted({r for e in count for r in e.get("refs") or {}})
        reason = (f"checks change tracked files: {', '.join(paths)}" if paths
                  else f"{d.kind} workers move refs: {', '.join(refs)}")
        # R32: unparking re-runs this kind (the card's work was not the problem).
        append_event(ctx.paths, "parked", card=c.number, reason=reason, cap_dispatch=d.id, cap_kind=d.kind)
        return False
    notes = _voided_notes(d.req.get("notes") or "")
    # R33: a check (review, rereview, verify, and any later check kind) runs again with a fresh
    # worker, never the one whose result was voided. An implementer-side step keeps the voided
    # dispatch's resume chain, so its guard and routing carry over (an answer to an integrate
    # worker stays one).
    resume = None if d.kind not in IMPLEMENTER_KINDS else _dispatches(events).get(d.req.get("resumes") or "")
    _request(ctx, c, events, kind=d.kind, attempt=d.req["attempt"], role=d.req["role"], route=d.route,
             extra=d.req.get("extra") or {}, notes=notes, resume=resume)
    return False


def _void(ctx: _Run, d: _Dispatch, events: list[dict]) -> bool:
    """Whether the check dispatch `d` changed what its dispatch guards (now or as recorded):
    then the change is recorded (`found`), preserved and undone (`restored`), and its result
    is void. Repeat-safe after a crash between the two phases."""
    mine = [e for e in events if e["kind"] == "violation" and _by(e) != "implementer"
            and e.get("dispatch_id") == d.id]
    if not mine:
        found = _find_violation(ctx, d, events)
        if found is None:
            return False
        mine = [found]
    found = next(e for e in mine if e.get("stage") == "found")
    if not any(e.get("stage") == "restored" for e in mine) and "base_worktree" in found:
        _restore_base(ctx, found)               # an evidence worker that changed the base worktree
    elif not any(e.get("stage") == "restored" for e in mine):
        saved, siblings = _restore_violation(ctx, found)
        append_event(ctx.paths, "violation", stage="restored", by="check", dispatch_id=d.id,
                     card=d.req.get("card"), n=found["n"], ref=found["ref"], saved=saved, siblings=siblings,
                     preserved=found["preserved"])
    return True


def _voided_notes(notes: str) -> str:
    """The brief notes for the re-run of a voided check (the warning is added once)."""
    if VOIDED_NOTE in notes:
        return notes
    return (notes.rstrip("\n") + "\n\n" if notes else "") + VOIDED_NOTE + (
        "\n\nAn earlier worker on this step changed the workspace or moved a branch it must "
        "leave alone, so its result was voided. Never stage or commit what this brief does not "
        "ask for, check out another branch, or create, move or delete any other ref.")


def _engine_writes(ctx: _Run, events: list[dict], after: int) -> tuple[dict[str, str | None], set[str]]:
    """What the engine itself recorded moving after event `after`: branch -> the value it last
    set, and the leadhand refs it wrote (owned outright)."""
    latest: dict[str, str | None] = {}
    owned: set[str] = set()
    run_ref = f"refs/heads/{ctx.branch}"
    for e in events:
        if e["seq"] <= after:
            continue
        kind = e["kind"]
        if kind == "accepted":
            latest[run_ref] = e.get("sha")
        elif kind == "card_cut" and e.get("branch"):
            latest[f"refs/heads/{e['branch']}"] = e.get("base")
        elif kind == "rebased" and e.get("card") is not None:
            latest[f"refs/heads/{card_branch_name(ctx.branch, e['card'])}"] = e.get("tip")
        elif kind == "integration_started" and e.get("keep"):
            owned.add(e["keep"])
        elif kind == "violation":
            if e.get("ref"):
                owned.update({e["ref"], f"{e['ref']}-branch", f"{e['ref']}-autostash"})
            owned.update((e.get("preserved") or {}).values())
            if e.get("stage") == "found" and _by(e) == "implementer":
                latest[f"refs/heads/{e['expected_branch']}"] = e.get("expected_head")
            elif e.get("stage") == "found":
                for name, m in (e.get("refs") or {}).items():
                    latest[name] = m.get("old")
    return latest, owned


def _find_violation(ctx: _Run, d: _Dispatch, events: list[dict]) -> dict | None:
    """Compare the workspace and refs with what the dispatch recorded; record `found` if they
    differ. Moves the engine recorded after the dispatch are its own, not the worker's."""
    req = d.req
    v = detect_violation(ctx.ws, req["expected_head"], req["expected_branch"]) if req.get("expected_head") else None
    before, now = req.get("refs") or {}, _ref_snapshot(ctx)
    latest, owned = _engine_writes(ctx, events, req["seq"])
    # An acceptance whose fast-forward happened but whose `accepted` is not recorded yet (a
    # crash in between) moved the run branch to its target: that is the engine's move too.
    run_ref = f"refs/heads/{ctx.branch}"
    targets = {e.get("target") for e in events if e["kind"] == "accept_started" and e["seq"] > req["seq"]}
    names = set(req["ref_scope"]) if req.get("ref_scope") else set(before) | set(now)
    moved = {}
    for name in sorted(names - owned):
        expected = latest[name] if name in latest else before.get(name)
        if name == run_ref and now.get(name) in targets:
            continue
        if now.get(name) != expected:
            moved[name] = {"old": expected, "new": now.get(name)}
    if v is None and not moved:
        return None
    n = 1 + sum(1 for e in events if e["kind"] == "violation" and _by(e) != "implementer"
                and e.get("stage") == "found")
    ref = f"refs/leadhand/{ctx.meta['run_id']}/violations/{n}"
    preserved: dict[str, str] = {}
    for name, m in moved.items():
        if m["new"] is None:
            continue                        # deleted: restoring it brings it back
        slug = re.sub(r"[^A-Za-z0-9_]+", "-", name.removeprefix("refs/")).strip("-") or "ref"
        keep, k = f"{ref}-{slug}", 1
        while keep in preserved.values():
            k += 1
            keep = f"{ref}-{slug}-{k}"
        preserved[name] = keep
    return append_event(ctx.paths, "violation", stage="found", by="check", dispatch_id=d.id, card=req["card"],
                        dispatch_kind=d.kind, n=n, ref=ref if v is not None else None, paths=_violation_paths(ctx, v),
                        head_moved=bool(v and v.head_moved), index_dirty=bool(v and v.index_dirty),
                        expected_head=req.get("expected_head"), expected_branch=req.get("expected_branch"),
                        refs=moved, preserved=preserved)


def _violation_paths(ctx: _Run, v) -> list[str]:
    if v is None:
        return []
    paths = set(v.tracked_changes)
    if v.index_dirty:
        staged = git(ctx.ws, "diff", "--cached", "--name-only", "-z", "--no-renames", "--no-ext-diff", "HEAD", "--")
        paths.update(p for p in staged.split("\0") if p)
    return sorted(paths)


def _restore_violation(ctx: _Run, found: dict) -> tuple[str | None, list[str]]:
    """Keep every moved ref's new tip and the workspace on side refs, then put the workspace
    and the refs back. Repeat-safe after a crash at any point. Returns (saved commit, siblings)."""
    ws = ctx.ws
    for name, keep in found["preserved"].items():
        git(ws, "update-ref", "-m", "leadhand: preserve a ref a worker moved", keep, found["refs"][name]["new"])
    saved, siblings = None, []
    ref = found["ref"]
    if ref:
        if detect_violation(ws, found["expected_head"], found["expected_branch"]) is not None:
            saved = preserve_and_restore(ws, ref=ref, expected_head=found["expected_head"],
                                         expected_branch=found["expected_branch"])
        now = _ref_snapshot(ctx)
        saved = saved or now.get(ref)
        siblings = [s for s in (f"{ref}-branch", f"{ref}-autostash") if s in now]
    now = _ref_snapshot(ctx)
    for name, m in found["refs"].items():
        cur = now.get(name)
        if cur == m["old"]:
            continue
        if cur != m["new"]:
            raise EngineError(f"{name} changed again after the violation by {found['dispatch_id']} "
                              f"(recorded {m['new']}, now {cur}); restore it by hand")
        if m["old"] is None:
            git(ws, "update-ref", "-m", "leadhand: remove a ref a worker created", "-d", name, cur)
        else:
            # The moved value is the expected current value: a ref changed meanwhile is not overwritten.
            git(ws, "update-ref", "-m", "leadhand: restore a ref a worker moved", name, m["old"], cur or "")
    return saved, siblings


def _operation(ctx: _Run) -> str | None:
    """The Git operation left in progress in the workspace (rebase, merge, ...), if any."""
    args = [a for name, _ in OPERATIONS for a in ("--git-path", name)]
    found = git(ctx.ws, "rev-parse", "--path-format=absolute", *args).splitlines()
    for (_, op), path in zip(OPERATIONS, found):
        if Path(path).exists():
            return op
    return None


def _ensure_clean(ctx: _Run, events: list[dict], *, branch: str, why: str, after_dispatch: str | None = None,
                  operations_only: bool = False, untracked: bool = False) -> dict | None:
    """R30: before a check is dispatched and before the engine rebases or switches branches,
    anything an implementer left in the workspace (a dirty tree or index, HEAD off `branch`'s
    tip, an operation in progress) is preserved under refs/leadhand/<run_id>/leftovers/<n>
    (with preserve_and_restore's siblings) and cleared, putting the workspace on `branch` at
    its tip. Recorded as `violation{by: implementer}`; it voids nothing. With
    `operations_only`, only an operation in progress counts. With `untracked` (before the
    engine switches cards or rebases), untracked files Git does not ignore count too: they
    are kept in the leftovers commit and removed, so no later card's commit sweeps them in
    (I3); nested repositories are left alone. Returns the `found` event."""
    op = _operation(ctx)
    if operations_only and op is None:
        return None
    tip = head(ctx.ws, f"refs/heads/{branch}")
    v = detect_violation(ctx.ws, tip, branch)
    loose = _foreign_untracked(ctx, untracked_files(ctx.ws)) if untracked else []
    if op is None and v is None and not loose:
        return None
    n = 1 + sum(1 for e in read_events(ctx.paths) if e["kind"] == "violation" and _by(e) == "implementer"
                and e.get("stage") == "found")
    found = append_event(ctx.paths, "violation", stage="found", by="implementer", n=n,
                         ref=f"refs/leadhand/{ctx.meta['run_id']}/leftovers/{n}", paths=_violation_paths(ctx, v),
                         operation=op, head_moved=bool(v and v.head_moved), index_dirty=bool(v and v.index_dirty),
                         expected_head=tip, expected_branch=branch, after_dispatch=after_dispatch, why=why,
                         **({"untracked": loose} if loose else {}))
    _finish_leftovers(ctx, [found])
    return found


def _foreign_untracked(ctx: _Run, paths: list[str]) -> list[str]:
    """The untracked paths a worker may have left: not a nested repository, not one the
    operator had at start, nothing under the run directory or docs_dir (R48')."""
    return dsp.untracked_to_take(ctx.root, ctx.ws, paths)


def _finish_leftovers(ctx: _Run, events: list[dict]) -> bool:
    """Preserve and clear every recorded leftovers violation not yet restored (repeat-safe:
    a workspace already clean is not preserved again, so the saved state is never replaced)."""
    restored = {e.get("n") for e in events if e["kind"] == "violation" and _by(e) == "implementer"
                and e.get("stage") == "restored"}
    pending = [e for e in events if e["kind"] == "violation" and _by(e) == "implementer"
               and e.get("stage") == "found" and e.get("n") not in restored]
    for f in pending:
        saved = None
        loose = [p for p in f.get("untracked") or () if os.path.lexists(ctx.ws / p)]
        if (_operation(ctx) or detect_violation(ctx.ws, f["expected_head"], f["expected_branch"]) is not None
                or loose):
            saved = preserve_and_restore(ctx.ws, ref=f["ref"], expected_head=f["expected_head"],
                                         expected_branch=f["expected_branch"], untracked=loose)
        now = _ref_snapshot(ctx)
        append_event(ctx.paths, "violation", stage="restored", by="implementer", n=f["n"], ref=f["ref"],
                     saved=saved or now.get(f["ref"]), after_dispatch=f.get("after_dispatch"),
                     siblings=[s for s in (f"{f['ref']}-branch", f"{f['ref']}-autostash") if s in now])
    return bool(pending)


def _left_operation(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict]) -> bool:
    """Whether implementer dispatch `d` left a Git operation in progress (now or as recorded)."""
    if d.id in _broken_dispatches(events):
        return True
    if not c.branch:
        return False
    return _ensure_clean(ctx, events, branch=c.branch, why=f"{d.id} left an operation in progress",
                         after_dispatch=d.id, operations_only=True) is not None


def _end_blocked(ctx: _Run, events: list[dict], reason: str, why: tuple[str, ...] = ()) -> None:
    _end(ctx, events, blocked_reason=reason, why=why)


# --------------------------------------------------------------------------
# End of run: commit-bound gates, fix waves, the outcome and the wrap-up

GATES = ("final_review", "evidence", "security", "verify")
# gate -> (role to resolve, security review); security resolves to reviewer_hard.
GATE_ROLES = {"final_review": ("final_reviewer", False), "evidence": ("verifier", False),
              "security": ("reviewer", True), "verify": ("verifier", False)}
REVIEW_GATES = ("final_review", "security")
# Run-level kinds that may build but must leave the workspace and the run's refs alone.
RUN_CHECK_KINDS = ("final_review", "evidence", "security", "verify")
MAX_FINAL_FIX_WAVES = 2
FINAL = "final"               # the fix waves' card branch suffix, and the `card` of their accept events
RISKS = ("low", "medium", "high")

SENSITIVE_PATHS: tuple[re.Pattern[str], ...] = tuple(re.compile(p, re.IGNORECASE) for p in (
    # CI
    r"^\.github/(workflows|actions)/", r"^\.gitlab-ci\.ya?ml$", r"^\.circleci/", r"^\.buildkite/",
    r"(^|/)Jenkinsfile$", r"^azure-pipelines\.ya?ml$", r"^\.travis\.ya?ml$",
    # release and install scripts
    r"(^|/)(install|installer|release|publish|deploy|bootstrap)([._-][^/]*)?$",
    r"(^|/)(release|releases|packaging)/",
    # infrastructure
    r"(^|/)(infra|infrastructure|terraform|k8s|kubernetes|helm|ansible|deploy|deployment|deployments)/",
    r"\.(tf|tfvars)$", r"(^|/)Dockerfile[^/]*$", r"(^|/)docker-compose[^/]*\.ya?ml$",
    # auth and secrets
    r"(^|/)(auth|authn|authz|oauth|login|session|sessions|permission|permissions|secret|secrets|"
    r"credential|credentials|crypto|security)([/._-]|$)",
    r"(^|/)\.env(\.[^/]*)?$", r"\.(pem|key|p12|pfx|jks|keystore)$",
    # migrations
    r"(^|/)migrations?/", r"(^|/)(db|database)/migrate/", r"(^|/)alembic/",
))
"""Changed paths that make the security gate required whatever the cards' risk.

The same families as the deck's risk rule (Risk: high for auth, secrets, CI, release
or install scripts and data migrations), plus infrastructure: CI workflows, release
and install scripts, infrastructure as code and container files, auth, sessions,
permissions, secrets and key files, and database migrations. Matched against
repo-relative POSIX paths, case-insensitively; a false match only costs one review."""


def is_sensitive_path(path: str) -> bool:
    return any(p.search(path) for p in SENSITIVE_PATHS)


def _final_branch(ctx: _Run) -> str:
    return card_branch_name(ctx.branch, FINAL)


def _run_head(ctx: _Run) -> str:
    return head(ctx.ws, f"refs/heads/{ctx.branch}")


def _reopened_since(events: list[dict]) -> int:
    """The seq of the latest ending a `resumed` followed (0 when none): failures recorded
    before it are retried; a relaunch without an ending keeps them."""
    since, ending = 0, None
    for e in events:
        if e["kind"] == "outcome":
            ending = e["seq"]
        elif e["kind"] == "resumed" and ending is not None:
            since = ending
    return since


def _run_dispatches(index: dict[str, _Dispatch]) -> list[_Dispatch]:
    return sorted((d for d in index.values() if d.req.get("card") is None), key=lambda d: d.req["seq"])


def _accepted_cards(ctx: _Run, cards: dict[int, CardState]) -> list[Card]:
    return [ctx.card(n) for n in dependency_order(ctx.deck) if cards[n].state == "accepted"]


def _risk_card(accepted: list[Card]) -> Card:
    """The accepted card with the highest risk, stripped of Hard and route pins: run-level
    routes follow the run's highest risk only."""
    top = max(accepted, key=lambda c: RISKS.index(c.risk) if c.risk in RISKS else 1)
    return replace(top, hard=False, hard_reason="", route_pin=None)


def _required_gates(ctx: _Run, accepted: list[Card], head_sha: str) -> list[str]:
    """Every gate, with security only when the highest accepted risk is medium or high (an
    unknown risk counts as medium, as in `_risk_card`) or a path changed since the base is
    sensitive."""
    risky = any(c.risk != "low" for c in accepted)
    security = risky or any(is_sensitive_path(p) for p in changed_paths(ctx.ws, ctx.meta["base_sha"], head_sha))
    return [g for g in GATES if g != "security" or security]


def _gate_results(events: list[dict], head_sha: str, since: int) -> dict[str, dict]:
    """Each gate's last result on `head_sha`. A failure recorded before the ending a resume
    followed is not counted (the gate runs again); a pass stays current while the head stays."""
    out: dict[str, dict] = {}
    for e in events:
        if (e["kind"] == "gate_result" and e.get("examined_sha") == head_sha
                and (e.get("passed") or e["seq"] > since)):
            out[e["gate"]] = e
    return out


def _resolve_run(ctx: _Run, role: str, accepted: list[Card], *, security: bool = False) -> Resolution:
    try:
        return resolve(ctx.config, role=role, card=_risk_card(accepted), attempt=1, installed=installed_vendors(),
                       security=security, profile=ctx.deck.profile)
    except (LadderExhausted, ConfigError) as e:
        raise EngineError(f"no route for the run's {role}: {e}") from None


def _end_of_run(ctx: _Run, cards: dict[int, CardState], events: list[dict]) -> dict | None:
    """Every card is accepted, parked, or waits on a parked card: run the gates, then end.

    A gate passes only for the commit it examined: each `gate_result` records `gate`,
    `examined_sha` and `passed`, and a gate is current only when its last result on the
    run head passed. The loop runs the first required gate that is not current. A failing
    gate gets a fix wave while fewer than MAX_FINAL_FIX_WAVES have run: an implementer
    (`final_fix`) on the final branch cut from the run head, a review of its diff, and, when
    clean, a fast-forward of the run branch to the fix, which makes every gate stale. Once
    the waves are spent, a failing gate is settled: a review gate leaves open findings,
    verify fails the run, and an evidence gap is a hold (`evidence = "record"`) or ends the
    run blocked for a decision (`evidence = "block"`). When every required gate has a
    result on the head, the outcome is decided. With no card accepted, no gate runs."""
    if os.environ.get("LEADHAND_TESTING") == "1" and os.environ.get("LEADHAND_TEST_CARD_LOOP_ONLY") == "1":
        # Card-loop tests: stop where the card loop ends (no gates, no outcome event).
        summary = _card_summary(ctx, cards)
        return {"action": "done", "outcome": None, "qualifier": "", "cards": summary, "report": None,
                "reasons": [f"card {x['card']} {x['state']}: {x['reason']}" for x in summary if x["reason"]]}
    accepted = _accepted_cards(ctx, cards)
    if not accepted:
        _end(ctx, events, gates_current=False)
        return None
    index = _dispatches(events)
    runs = _run_dispatches(index)
    if runs:
        last = runs[-1]
        if last.result is None:
            return _in_flight(ctx, last)
        acted, action = _after_run_result(ctx, last, events, index, accepted)
        if acted:
            return action
    head_sha = _run_head(ctx)
    results = _gate_results(events, head_sha, _reopened_since(events))
    waves = len({d.req.get("wave") for d in runs if d.kind == "final_fix"})
    settled: dict[str, dict] = {}
    for gate in _required_gates(ctx, accepted, head_sha):
        r = results.get(gate)
        if r is None:
            _request_gate(ctx, events, gate, head_sha, accepted)
            return None
        if r["passed"]:
            continue
        if waves < MAX_FINAL_FIX_WAVES:
            _start_wave(ctx, events, waves + 1, r, head_sha, accepted)
            return None
        settled[gate] = r
        if gate == "evidence" and ctx.config.run.evidence == "block":
            _end(ctx, events, blocked_reason="decision needed",
                 why=(f"evidence gaps on {head_sha[:12]} after {waves} fix waves: {r.get('findings') or ''}",))
            return None
    holds = ((f"evidence gaps on {head_sha[:12]}: {settled['evidence'].get('findings') or ''}",)
             if "evidence" in settled else ())
    _end(ctx, events, gates_current=not settled, open_findings=any(g in settled for g in REVIEW_GATES),
         holds=holds, verification_failed="verify" in settled)
    return None


def _request_gate(ctx: _Run, events: list[dict], gate: str, head_sha: str, accepted: list[Card]) -> None:
    role, security = GATE_ROLES[gate]
    res = _resolve_run(ctx, role, accepted, security=security)
    extra: dict[str, str] = {}
    if gate in REVIEW_GATES:
        extra["diff_range"] = f"{ctx.meta['base_sha']}..{head_sha}"
    if gate == "evidence":
        extra["base_worktree"] = str(_base_worktree_path(ctx))      # reset in _request_run
    _request_run(ctx, events, kind=gate, gate=gate, attempt=1, role=res.role, route=res.route,
                 extra=extra, accepted=accepted)


def _base_worktree_path(ctx: _Run) -> Path:
    return Path(dsp.base_worktree_path(ctx.meta, ctx.root.name))


def _base_worktree(ctx: _Run) -> Path:
    """`<repo>/.worktrees/<workspace name>--base` (`<run dir name>--base` for an in-place
    run), detached at the run's base commit, for the
    evidence gate. Before every evidence request it is put back exactly: stale worktree
    records pruned, created when missing, then detached at the base, hard-reset and cleaned
    of every untracked and ignored file. Idempotent. It is kept after the run (beside the
    run's own worktree) so the evidence can be re-checked; `git worktree remove` deletes it."""
    repo = Path(ctx.meta["repo"])
    path = _base_worktree_path(ctx)
    base = ctx.meta["base_sha"]

    def in_place() -> None:
        # In an in-place run <repo>/.worktrees/ is worker-writable: a symlink there would aim
        # the reset below at another worktree (security review M3).
        if not resolves_in_place(repo, path):
            raise EngineError(f"{path} is a symlink or lies behind one; refusing to reset it. Move it aside")

    in_place()
    git(repo, "worktree", "prune")
    listed = git(repo, "worktree", "list", "--porcelain").splitlines()
    registered = {os.path.realpath(ln[len("worktree "):]) for ln in listed if ln.startswith("worktree ")}
    if os.path.lexists(path) and os.path.realpath(path) not in registered:
        raise EngineError(f"{path} exists but is not a worktree of {repo}; move it aside")
    if not os.path.lexists(path):
        ensure_excluded(repo, ".worktrees/")
        git(repo, "worktree", "add", "--quiet", "--detach", str(path), base)
    in_place()
    git(path, "checkout", "--quiet", "--force", "--detach", base, "--")
    git(path, "reset", "--quiet", "--hard", base)
    git(path, "clean", "-q", "-f", "-d", "-x")
    return path


def _base_violation(ctx: _Run, d: _Dispatch, events: list[dict]) -> bool:
    """After an evidence result: the base worktree must still be at the base commit with no
    tracked change. Otherwise record a check violation (found, then restored: a moved HEAD is
    kept on refs/leadhand/<run_id>/violations/<n>-base and the worktree is reset), so the
    result is void like any other check violation."""
    path = Path((d.req.get("extra") or {}).get("base_worktree") or _base_worktree_path(ctx))
    base = ctx.meta["base_sha"]
    try:
        now = git(path, "rev-parse", "--verify", "HEAD")
        dirty = git(path, "status", "--porcelain", "--untracked-files=no")
    except (GitError, OSError):
        now, dirty = None, ""
    if now == base and not dirty:
        return False
    n = 1 + sum(1 for e in events if e["kind"] == "violation" and _by(e) != "implementer"
                and e.get("stage") == "found")
    keep = f"refs/leadhand/{ctx.meta['run_id']}/violations/{n}-base"
    found = append_event(ctx.paths, "violation", stage="found", by="check", dispatch_id=d.id, card=None,
                         dispatch_kind=d.kind, n=n, ref=None, base_worktree=str(path), base_head=now,
                         paths=sorted({ln[3:] for ln in dirty.splitlines() if len(ln) > 3}),
                         head_moved=now != base, index_dirty=False, expected_head=d.req.get("expected_head"),
                         expected_branch=d.req.get("expected_branch"), refs={},
                         preserved={"base": keep} if now and now != base else {})
    _restore_base(ctx, found)
    return True


def _restore_base(ctx: _Run, found: dict) -> None:
    """Keep the moved base HEAD on its ref, reset the base worktree, record `restored`.
    Repeat-safe: after a crash in between, `_void` calls it again."""
    keep = (found.get("preserved") or {}).get("base")
    if keep and found.get("base_head"):
        git(ctx.ws, "update-ref", "-m", "leadhand: keep a base worktree a verifier moved", keep, found["base_head"])
    _base_worktree(ctx)
    append_event(ctx.paths, "violation", stage="restored", by="check", dispatch_id=found["dispatch_id"], card=None,
                 n=found["n"], ref=None, saved=found.get("base_head"), siblings=[], preserved=found["preserved"])


def _start_wave(ctx: _Run, events: list[dict], wave: int, failing: dict, head_sha: str,
                accepted: list[Card]) -> None:
    """Cut the final branch at the run head (keeping an earlier wave's unaccepted commits on
    refs/leadhand/<run_id>/final/<wave>), then request the wave's implementer."""
    branch = _final_branch(ctx)
    cut = [e for e in events if e["kind"] == "card_cut" and e.get("card") == FINAL and e.get("wave") == wave]
    if not cut:
        _check_out(ctx, events, None, ctx.branch)          # off the final branch before it moves
        ref = f"refs/heads/{branch}"
        old = _ref_value(ctx, ref)
        if old != head_sha:
            if old is not None:
                git(ctx.ws, "update-ref", "-m", "leadhand: keep an unaccepted fix wave",
                    f"refs/leadhand/{ctx.meta['run_id']}/final/{wave - 1}", old)
            git(ctx.ws, "update-ref", "-m", "leadhand: cut the final fix branch", ref, head_sha, old or "")
        append_event(ctx.paths, "card_cut", card=FINAL, branch=branch, base=head_sha, wave=wave)
        return
    res = _resolve_run(ctx, "implementer", accepted)
    _request_run(ctx, events, kind="final_fix", wave=wave, attempt=wave, role=res.role, route=res.route,
                 extra={"gate": failing["gate"], "findings": failing.get("findings") or "(no report kept)",
                        "branch": branch, "base": head_sha},
                 accepted=accepted)


def _ref_value(ctx: _Run, ref: str) -> str | None:
    out = git(ctx.ws, "for-each-ref", "--format=%(objectname)", ref)
    return out.strip() or None


def _request_run(ctx: _Run, events: list[dict], *, kind: str, attempt: int, role: str, route: Route,
                 extra: dict[str, str], accepted: list[Card], gate: str | None = None, wave: int | None = None,
                 notes: str = "") -> dict:
    """Write a run-level brief and append its `dispatch_requested` (card None). Gates run on
    the run branch; a wave's implementer and review on the final branch. Checks are guarded
    like card checks (R14'); a gate also records `gate_started` with the commit it examines.
    Deterministic, like `_request`."""
    n = 1 + sum(1 for e in events if e["kind"] == "dispatch_requested" and e.get("card") is None
                and e.get("dispatch_kind") == kind and e.get("wave") == wave)
    did = f"run-{kind}-{n}" if wave is None else f"run-w{wave}-{kind}-{n}"
    branch = ctx.branch if wave is None else _final_branch(ctx)
    _check_out(ctx, events, None, branch)
    guard: dict[str, Any] = {}
    if kind in RUN_CHECK_KINDS:
        _ensure_clean(ctx, events, branch=branch, why=f"before {kind} {did}")
        guard = {"expected_branch": branch, "expected_head": head(ctx.ws, f"refs/heads/{branch}"),
                 "refs": _ref_snapshot(ctx)}
    elif kind == "final_fix":
        # Like an integrate worker: it commits on the final branch but must leave the run branch alone.
        run_ref = f"refs/heads/{ctx.branch}"
        guard = {"refs": {run_ref: head(ctx.ws, run_ref)}, "ref_scope": [run_ref],
                 "start_tip": head(ctx.ws, f"refs/heads/{branch}"), "untracked": untracked_files(ctx.ws)}
    if kind == "evidence":
        # Every evidence dispatch (first run, voided or failed re-run) starts from a clean base.
        extra = {**extra, "base_worktree": str(_base_worktree(ctx))}
    full = {"base": ctx.meta["base_sha"], "branch": ctx.branch, "test_first": ctx.config.run.test_first,
            "cards": ",".join(str(c.number) for c in accepted), **extra}
    if gate is not None and not any(e["kind"] == "gate_started" and e.get("dispatch_id") == did for e in events):
        append_event(ctx.paths, "gate_started", gate=gate, examined_sha=guard["expected_head"], dispatch_id=did)
    brief, report = _write_brief(kind, ws=ctx.ws, deck=ctx.deck, card=None, attempt=attempt, dispatch_id=did,
                                 extra=full, notes=notes)
    return _append_request(ctx, did, card=None, attempt=attempt, kind=kind, role=role, route=route, brief=brief,
                           report=report, extra=extra, notes=notes, resume=None, gate=gate, wave=wave, **guard)


def _usable(d: _Dispatch) -> bool:
    """Whether a run-level result can be acted on: a verdict, a pass/fail, or an implementer status."""
    r = d.result or {}
    if d.kind in ("final_review", "security"):
        return r.get("verdict") in ("clean", "defects")
    if d.kind in ("evidence", "verify"):
        return r.get("result") in ("pass", "fail")
    return r.get("status") in ("READY", "QUESTION", "SPENT")


def _after_run_result(ctx: _Run, d: _Dispatch, events: list[dict], index: dict[str, _Dispatch],
                      accepted: list[Card]) -> tuple[bool, dict | None]:
    """Act on the latest run-level dispatch's result. (True, action or None) when something
    was done; (False, None) when the result is settled and the gate loop decides what's next."""
    r, req = d.result, d.req
    if req.get("gate"):
        if any(e["kind"] == "gate_result" and e.get("dispatch_id") == d.id for e in events):
            return False, None
        if "refs" in req and _run_check_voided(ctx, d, events, accepted):
            return True, None
        if r.get("interrupted"):
            _rerun(ctx, events, accepted, notes=req.get("notes") or "", d=d)     # the gate runs again
            return True, None
        if not _usable(d):
            _run_worker_failed(ctx, d, events, index, accepted)
            return True, None
        passed = r.get("verdict") == "clean" or r.get("result") == "pass"
        crash_point("before_gate_record")
        append_event(ctx.paths, "gate_result", gate=req["gate"], examined_sha=req["expected_head"],
                     passed=passed, dispatch_id=d.id, findings=_report_copy(d))
        return True, None
    wave = req.get("wave")
    if d.kind == "final_fix":
        if any(e["kind"] == "verdict_recorded" and e.get("dispatch_id") == d.id for e in events):
            return False, None                      # settled as spent: never checked again
        if "refs" in req and _run_check_voided(ctx, d, events, accepted):
            return True, None
        if r.get("interrupted"):
            # Its worker died with an earlier session: the wave's implementer runs again on the
            # final branch as it stands (the fix review covers every commit since the wave's base).
            _rerun(ctx, events, accepted, d=d, notes=_continue_notes(req.get("notes") or "", d))
            return True, None
        if r["status"] == "READY":
            base, tip = req["extra"]["base"], r.get("tip")
            if not tip or tip == base or base != _run_head(ctx):
                why = "changed nothing" if not tip or tip == base else "the run head moved"
                _settle_spent(ctx, d, why)
                return True, None
            res = _resolve_run(ctx, "final_reviewer", accepted)
            notes = ("## Fix wave\n\n"
                     f"This reviews fix wave {wave} for the failed \"{req['extra']['gate']}\" gate, whose "
                     f"findings are in {req['extra']['findings']}. The fix's report: {_report_copy(d)}. "
                     f"Review only the fix's diff {base}..{tip}: it must fix the findings without undoing "
                     "any card's \"Done when\" or reaching outside the cards' allowed paths. `Verdict: clean` "
                     "lets it onto the run branch.")
            _request_run(ctx, events, kind="final_review", wave=wave, attempt=wave, role=res.role,
                         route=res.route, extra={"diff_range": f"{base}..{tip}", "base": base,
                                                 "branch": _final_branch(ctx)},
                         accepted=accepted, notes=notes)
            return True, None
        if r["status"] == "BLOCKED":
            _run_worker_failed(ctx, d, events, index, accepted)
            return True, None
        _settle_spent(ctx, d, f"ended {r['status']}")
        return True, None
    # The review of a fix wave.
    verdict = next((e for e in events if e["kind"] == "verdict_recorded" and e.get("dispatch_id") == d.id), None)
    if verdict is None:
        if "refs" in req and _run_check_voided(ctx, d, events, accepted):
            return True, None
        if r.get("interrupted"):
            _rerun(ctx, events, accepted, notes=req.get("notes") or "", d=d)
            return True, None
        if not _usable(d):
            _run_worker_failed(ctx, d, events, index, accepted)
            return True, None
        append_event(ctx.paths, "verdict_recorded", card=FINAL, wave=wave, verdict=r["verdict"],
                     findings=_report_copy(d), dispatch_id=d.id)
        return True, None
    if verdict["verdict"] != "clean":
        return False, None                          # rejected: the wave is spent, the run head stays
    if any(e["kind"] == "accepted" and e.get("card") == FINAL and e.get("dispatch_id") == d.id for e in events):
        return False, None
    target = req["expected_head"]
    reviewed = (req.get("extra") or {}).get("diff_range", "").partition("..")[2]
    if target != reviewed:
        raise EngineError(f"fix review {d.id} examined {target}, not the fix tip {reviewed}")
    started = any(e["kind"] == "accept_started" and e.get("dispatch_id") == d.id for e in events)
    if not started and not is_ancestor(ctx.ws, f"refs/heads/{ctx.branch}", target):
        return False, None                          # a card was accepted meanwhile: the wave is stale
    if not started:
        append_event(ctx.paths, "accept_started", card=FINAL, wave=wave, target=target, dispatch_id=d.id)
        crash_point("before_fast_forward")
    fast_forward(ctx.ws, ctx.branch, target)
    crash_point("after_fast_forward")
    append_event(ctx.paths, "accepted", card=FINAL, wave=wave, sha=target, dispatch_id=d.id)
    return True, None


def _settle_spent(ctx: _Run, d: _Dispatch, why: str) -> None:
    """Record that a fix wave's implementer result spent the wave (its guard was checked
    first), as `verdict_recorded{card: "final", verdict: "spent"}`, so the result is acted on
    once: later steps never check its guard again, whatever moves afterwards."""
    append_event(ctx.paths, "verdict_recorded", card=FINAL, wave=d.req.get("wave"), verdict="spent",
                 reason=why, findings=_report_copy(d), dispatch_id=d.id)


def _run_check_voided(ctx: _Run, d: _Dispatch, events: list[dict], accepted: list[Card]) -> bool:
    """A run-level check that changed what it guards: preserved, undone and run again with a
    fresh worker (R33). Past VIOLATION_CAP voided runs of the kind since the run was last
    resumed, the run ends blocked for a decision instead (there is no card to park)."""
    if not _void(ctx, d, events) and not (d.kind == "evidence" and _base_violation(ctx, d, events)):
        return False
    events = read_events(ctx.paths)
    since = _reopened_since(events)
    count = [e for e in events if e["kind"] == "violation" and _by(e) != "implementer"
             and e.get("stage") == "found" and e.get("card") is None
             and e.get("dispatch_kind") == d.kind and e["seq"] > since]
    if len(count) > VIOLATION_CAP:
        paths = sorted({p for e in count for p in e.get("paths") or []})
        refs = sorted({r for e in count for r in e.get("refs") or {}})
        what = (f"change tracked files: {', '.join(paths)}" if paths else f"move refs: {', '.join(refs)}")
        _end(ctx, events, blocked_reason="decision needed", why=(f"{d.kind} workers {what}",))
        return True
    if d.kind == "final_fix":
        _recut_final(ctx, d, events)
    _rerun(ctx, read_events(ctx.paths), accepted, notes=_voided_notes(d.req.get("notes") or ""), d=d)
    return True


def _recut_final(ctx: _Run, d: _Dispatch, events: list[dict]) -> None:
    """R36: the fresh re-run of a voided final fix starts from the run head. The final branch
    is moved back there (compare-and-swap), and the voided worker's commits are kept only on
    refs/leadhand/<run_id>/violations/<n>-final, so the fix review never covers them.
    Recorded as `card_cut{card: "final", recut_after}`; repeat-safe."""
    if any(e["kind"] == "card_cut" and e.get("recut_after") == d.id for e in events):
        return
    found = next(e for e in events if e["kind"] == "violation" and e.get("dispatch_id") == d.id
                 and e.get("stage") == "found")
    branch, head_sha = _final_branch(ctx), _run_head(ctx)
    ref = f"refs/heads/{branch}"
    old = _ref_value(ctx, ref)
    if old != head_sha:
        _check_out(ctx, events, None, ctx.branch)          # off the final branch before it moves
        if old is not None:
            git(ctx.ws, "update-ref", "-m", "leadhand: keep a voided final fix",
                f"refs/leadhand/{ctx.meta['run_id']}/violations/{found['n']}-final", old)
        git(ctx.ws, "update-ref", "-m", "leadhand: re-cut the final fix branch", ref, head_sha, old or "")
    append_event(ctx.paths, "card_cut", card=FINAL, branch=branch, base=head_sha, wave=d.req.get("wave"),
                 recut_after=d.id)


def _rerun(ctx: _Run, events: list[dict], accepted: list[Card], *, notes: str, d: _Dispatch) -> None:
    """The same run-level dispatch again, on the same route with a fresh worker."""
    req = d.req
    _request_run(ctx, events, kind=d.kind, gate=req.get("gate"), wave=req.get("wave"), attempt=req["attempt"],
                 role=req["role"], route=d.route, extra=req.get("extra") or {}, accepted=accepted, notes=notes)


def _run_worker_failed(ctx: _Run, d: _Dispatch, events: list[dict], index: dict[str, _Dispatch],
                       accepted: list[Card]) -> None:
    """A run-level worker ended BLOCKED or without a verdict or result: provider capacity
    ends the run blocked at once; otherwise it is dispatched again, and a second such ending
    for the same step (since the run was last resumed) ends the run blocked (environment)."""
    r, req = d.result, d.req
    if not any(e["kind"] == "outcome" and e["seq"] > r["seq"] for e in events):
        if r["status"] == "BLOCKED" and r.get("blocked_by") == "usage limit":
            _end_blocked(ctx, events, "provider capacity")
            return
        since, voided = _reopened_since(events), _voided(events)
        same = [x for x in _run_dispatches(index)
                if x.kind == d.kind and x.req.get("wave") == req.get("wave") and x.req.get("gate") == req.get("gate")
                and x.req.get("expected_head") == req.get("expected_head") and x.result is not None
                and x.result["seq"] > since and x.id not in voided and not _usable(x)
                and not x.result.get("interrupted")]
        if len(same) >= 2:
            _end_blocked(ctx, events, "environment",
                         why=(f"{d.kind} ended {r['status']} twice without a usable report",))
            return
    _rerun(ctx, events, accepted, notes=req.get("notes") or "", d=d)


def _end(ctx: _Run, events: list[dict], *, blocked_reason: str | None = None, open_findings: bool = False,
         holds: tuple[str, ...] = (), gates_current: bool = False, verification_failed: bool = False,
         why: tuple[str, ...] = (), cancelled: bool = False, unreconciled: str | None = None) -> dict:
    """Decide the outcome and append it, with the allowed next step and the run head. The
    wrap-up (handoff, report, runs.md) follows in `_wrap_up`.

    `gates_current` means every required gate passed on the run head; a gate that failed
    there for good is also reported through `open_findings`, `holds` or
    `verification_failed`. Every accepted card and fix must still be on the run branch;
    otherwise the state cannot be reconciled and the run fails (`unreconciled` names another
    mismatch a restart found). `cancelled`: the operator's cancel, after every worker stopped."""
    cards = fold_cards(events, ctx.numbers)
    run_ref = f"refs/heads/{ctx.branch}"
    run_tip = _ref_sha(ctx, run_ref) if ctx.ws.is_dir() else None
    # With no run branch at all, every accepted card's work is missing from it.
    missing = [f"card {e.get('card')} at {str(e.get('sha'))[:12]}" for e in events
               if e["kind"] == "accepted" and e.get("sha")
               and (run_tip is None or not is_ancestor(ctx.ws, e["sha"], run_ref))]
    if missing and not unreconciled:
        unreconciled = f"accepted work missing from the run branch: {', '.join(missing)}"
    if run_tip is not None and not unreconciled:
        # Only acceptance moves the run branch: anything else on it is unaccepted work (I4).
        unreconciled = _run_head_problem(ctx, events)
    if not (blocked_reason or cancelled or unreconciled or verification_failed):
        waiting = _waiting_on_decisions(ctx, cards, events)
        if waiting:
            # R44: everything left needs the operator's decision: the run can continue once
            # they unpark the cards, so it ends blocked (decision needed), with the handoff.
            blocked_reason = "decision needed"
            why = (*waiting, *why, *(("unresolved review findings",) if open_findings else ()),
                   *(f"hold: {h}" for h in holds))
    decision = decide_outcome(cards, removed=frozenset(), open_findings=open_findings, holds=holds,
                              gates_current=gates_current, verification_failed=verification_failed,
                              cancelled=cancelled, blocked_reason=blocked_reason, unreconciled=unreconciled)
    return append_event(ctx.paths, "outcome", outcome=decision.outcome.value, qualifier=decision.qualifier,
                        reasons=[*decision.reasons, *why],
                        next_step=allowed_next_step(decision, ctx.config.after_run), head=run_tip)


def _decision_park(events: list[dict], card: int) -> bool:
    """Whether the card's latest park waits on the operator's decision: the supervisor parked
    it on a worker's question (`decision`), or the violation cap parked it (R32)."""
    parks = [e for e in events if e["kind"] == "parked" and e.get("card") == card]
    return bool(parks) and bool(parks[-1].get("decision") or parks[-1].get("cap_dispatch"))


def _waiting_on_decisions(ctx: _Run, cards: dict[int, CardState], events: list[dict]) -> list[str]:
    """When every card not accepted is parked on a decision, or waits only on such cards,
    one reason per parked card; else [] (also when every card is accepted)."""
    left = [n for n in sorted(cards) if cards[n].state != "accepted"]
    if not left:
        return []
    parked = {n for n in left if cards[n].state == "parked"}

    def blockers(n: int, seen: frozenset = frozenset()) -> set[int]:
        out: set[int] = set()
        for d in ctx.card(n).depends_on:
            if d not in cards or d in seen or cards[d].state == "accepted":
                continue
            out |= {d} if cards[d].state == "parked" else blockers(d, seen | {n})
        return out

    for n in left:
        if n in parked:
            if not _decision_park(events, n):
                return []
        else:
            waits = blockers(n)
            if not waits or not all(_decision_park(events, b) for b in waits):
                return []
    return [f"card {n} parked: {cards[n].park_reason or 'no reason recorded'}" for n in sorted(parked)]


def _wrap_up(ctx: _Run, ended: dict, events: list[dict]) -> None:
    """After an outcome: the handoff (blocked runs), report.md, the runs.md line, then
    `report_written`. Repeat-safe; done once per outcome."""
    if any(e["kind"] == "report_written" and e["seq"] > ended["seq"] for e in events):
        return
    if ended.get("outcome") == "blocked":
        _write_handoff(ctx, ended, events)
    else:
        ctx.paths.handoff.unlink(missing_ok=True)      # a resumed blocked run's handoff no longer applies
    from leadhand import report as report_mod
    path = report_mod.write_report(ctx.root)
    _record_run_line(ctx, ended, events)
    append_event(ctx.paths, "report_written", report=str(path), outcome=ended.get("outcome"),
                 outcome_seq=ended["seq"])


def _write_handoff(ctx: _Run, ended: dict, events: list[dict]) -> None:
    cards = fold_cards(events, ctx.numbers)
    accepted = sum(1 for c in cards.values() if c.state == "accepted")
    parked = [n for n in sorted(cards) if cards[n].state == "parked"]
    index = _dispatches(events)
    last = max(index.values(), key=lambda d: d.req["seq"]) if index else None
    qualifier = ended.get("qualifier") or ""
    cli, root = shlex.quote(str(dsp.CLI_PATH)), shlex.quote(str(ctx.root))
    lines = [f"# Handoff: {ctx.deck.title}", "",
             f"Outcome: blocked{f' ({qualifier})' if qualifier else ''}", "",
             "## Where it stopped", "",
             f"- Run branch: {ctx.branch} at {ended.get('head') or _run_head(ctx)}",
             f"- Workspace: {ctx.ws}",
             f"- Cards: {accepted} of {len(cards)} accepted"
             + (f"; parked: {', '.join(map(str, parked))}" if parked else ""),
             f"- Last step: {last.kind} {last.id} ({_whose(last.req)})" if last else "- Last step: none"]
    lines += ["", "## Why", "", *(f"- {r}" for r in ended.get("reasons") or ["no reason recorded"])]
    lines += ["", "## What to do", "",
              f"1. Read the report: {ctx.paths.report}",
              "2. Settle what the reasons above name: a decision (record it in the decisions file or "
              f"`{cli} unpark {root} --card N --note '…'` with a note), capacity coming back, or the "
              "environment fixed.",
              f"3. Resume the run with `{cli} run {root}` (`{cli} launch-line {root}` "
              "prints the exact command). Gates that passed on the same commit are not run again."]
    _write_bytes_atomic(ctx.paths.handoff, ("\n".join(lines) + "\n").encode("utf-8"))


def _md_cell(text: str) -> str:
    return " ".join(str(text).split()).replace("|", "\\|")


def _record_run_line(ctx: _Run, ended: dict, events: list[dict]) -> None:
    """One line per run in `<docs_dir>/runs.md`, replaced in place when the run ends again
    (a resumed blocked run), so a run never has two lines."""
    from leadhand import status as st
    cards = fold_cards(events, ctx.numbers)
    accepted = sum(1 for c in cards.values() if c.state == "accepted")
    usd = st.usage_summary(st.load(ctx.root))["estimated_usd"]
    qualifier = ended.get("qualifier") or ""
    outcome = f"{ended.get('outcome')}" + (f" ({qualifier})" if qualifier else "")
    run_cell = f"| {_md_cell(ctx.root)} |"
    line = (f"| {date.today().isoformat()} | {_md_cell(ctx.deck.title)} | {outcome} | "
            f"{accepted}/{len(cards)} cards | {'unknown' if usd is None else f'${usd:.2f}'} {run_cell}")
    docs = docs_dir_for(ctx.config, Path(ctx.meta["repo"]))
    path = docs / "runs.md"
    try:
        old = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        old = ["# Runs", "", "| Date | Deck | Outcome | Cards | Estimated cost | Run |",
               "| --- | --- | --- | --- | --- | --- |"]
    new, placed = [], False
    for ln in old:
        if ln.rstrip().endswith(run_cell):
            if not placed:
                new.append(line)
                placed = True
            continue
        new.append(ln)
    if not placed:
        new.append(line)
    docs.mkdir(parents=True, exist_ok=True)
    _write_bytes_atomic(path, ("\n".join(new) + "\n").encode("utf-8"))


# --------------------------------------------------------------------------
# Recovery: reconciling after a relaunch, pause and cancel

def _latest_resumed(events: list[dict]) -> int:
    """The seq of the latest `resumed` (0 when the run was never relaunched)."""
    return max((e["seq"] for e in events if e["kind"] == "resumed"), default=0)


def _gone(events: list[dict], d: _Dispatch) -> bool:
    """Whether the dispatch's Claude worker (or, for a Codex route, its proxy) died with an
    earlier supervisor session: it was requested or started before the latest relaunch, or its
    start was recorded by reconciliation (no worker of this session runs it). P9."""
    since = _latest_resumed(events)
    if d.started is None:
        return d.req["seq"] < since
    return bool(d.started.get("reconciled")) or d.started["seq"] < since


def _needs_reconcile(events: list[dict]) -> bool:
    since = _latest_resumed(events)
    return bool(since) and not any(e["kind"] == "reconciled" and "found" in e and e.get("resumed_seq") == since
                                   for e in events)


def _open_dispatches(ctx: _Run, events: list[dict], index: dict[str, _Dispatch]) -> list[_Dispatch]:
    """The dispatches the run waits on: each unsettled card's current dispatch, and the latest
    run-level one, while they have no result."""
    cards = fold_cards(events, ctx.numbers)
    out = [index[c.dispatch_id] for c in cards.values()
           if c.state in ACTIVE_STATES and c.dispatch_id in index and index[c.dispatch_id].result is None]
    runs = _run_dispatches(index)
    if runs and runs[-1].result is None:
        out.append(runs[-1])
    return sorted(out, key=lambda d: d.req["seq"])


def _ref_sha(ctx: _Run, ref: str) -> str | None:
    try:
        return git(ctx.ws, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}") or None
    except GitError:
        return None


def _unmatched(ctx: _Run, events: list[dict]) -> str | None:
    """What a restart finds that matches no recovery rule (the run then fails), or None."""
    if not ctx.ws.is_dir():
        return f"the workspace {ctx.ws} is missing"
    run_ref = f"refs/heads/{ctx.branch}"
    run_tip = _ref_sha(ctx, run_ref)
    if run_tip is None:
        return f"the run branch {ctx.branch} is missing"
    missing = [f"card {e.get('card')} at {str(e['sha'])[:12]}" for e in events
               if e["kind"] == "accepted" and e.get("sha") and not is_ancestor(ctx.ws, e["sha"], run_ref)]
    if missing:
        return f"accepted work missing from the run branch: {', '.join(missing)}"
    cards = fold_cards(events, ctx.numbers)
    for n in sorted(cards):
        c = cards[n]
        if c.state in ("cut", *ACTIVE_STATES) and c.branch and _ref_sha(ctx, f"refs/heads/{c.branch}") is None:
            return f"card {n}'s branch {c.branch} is missing"
        target = c.accept_target
        if c.state == "accepting" and target and not (is_ancestor(ctx.ws, target, run_ref)
                                                      or is_ancestor(ctx.ws, run_ref, target)):
            return (f"card {n}'s acceptance of {target[:12]} cannot complete: the run branch is at "
                    f"{run_tip[:12]}, which neither contains it nor is behind it")
    return None


def _tip_before(ctx: _Run, events: list[dict], d: _Dispatch) -> str | None:
    """The branch tip a dispatch started from: recorded as `start_tip`, else (an older log) the
    card's last known tip or base before the dispatch, or a fix wave's base."""
    if d.req.get("start_tip"):
        return d.req["start_tip"]
    if d.req["card"] is None:
        return (d.req.get("extra") or {}).get("base")
    before = fold_cards([e for e in events if e["seq"] < d.req["seq"]], ctx.numbers).get(d.req["card"])
    return (before.tip or before.base) if before else None


def _branch_progress(ctx: _Run, events: list[dict], d: _Dispatch) -> str:
    """For an interrupted implementer-side dispatch: "commits" when its branch moved on from
    where the dispatch started (an integrate step may also have rewritten it), "nothing"
    when it did not move, else why the branch cannot be matched."""
    n = d.req["card"]
    if n is None:
        branch = _final_branch(ctx)
    else:
        branch = fold_cards(events, ctx.numbers)[n].branch or card_branch_name(ctx.branch, n)
    tip = _ref_sha(ctx, f"refs/heads/{branch}")
    who = "the final fix" if n is None else f"card {n}"
    if tip is None:
        return f"{who}'s branch {branch} is missing"
    start = _tip_before(ctx, events, d)
    if not start or tip == start:
        return "nothing"
    if is_ancestor(ctx.ws, start, tip) or _integrate_origin(_dispatches(events), d) is not None:
        return "commits"
    return (f"{who}'s branch {branch} was rewritten while {d.id} was interrupted "
            f"(from {start[:12]} to {tip[:12]})")


CONTINUE_NOTE = "## The interrupted step"


def _continue_notes(notes: str, d: _Dispatch) -> str:
    """Brief notes for the step that carries on from an interrupted dispatch (added once)."""
    if CONTINUE_NOTE in notes:
        return notes
    copy = ((d.result or {}).get("files") or {}).get("brief") or d.req["brief"]
    text = (f"{CONTINUE_NOTE}\n\nThis carries on from {d.kind} dispatch {d.id}, whose worker stopped "
            f"before it reported (its supervisor session ended). Its brief: {copy}. Keep its commits; "
            "continue from the branch as it stands and write your report.")
    return (notes.rstrip("\n") + "\n\n" if notes else "") + text


def _after_interrupted(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict],
                       index: dict[str, _Dispatch]) -> None:
    """A card dispatch whose worker died with an earlier session and left no report: a check
    runs again with a fresh worker; implementer-side work continues from its commits (resuming
    the Codex session when its ID is known) or, when the branch did not move, is requested
    again as the same attempt under a new dispatch ID. A branch that matches neither fails
    the run: nothing is guessed."""
    req = d.req
    if d.kind not in IMPLEMENTER_KINDS:
        _request(ctx, c, events, kind=d.kind, attempt=req["attempt"], role=req["role"], route=d.route,
                 extra=req.get("extra") or {}, notes=req.get("notes") or "")
        return
    progress = _branch_progress(ctx, events, d)
    if progress == "commits":
        _request(ctx, c, events, kind="continue", attempt=req["attempt"], role=req["role"], route=d.route,
                 extra={}, resume=d, notes=_continue_notes("", d), continues=d.id)
    elif progress == "nothing":
        _request(ctx, c, events, kind=d.kind, attempt=req["attempt"], role=req["role"], route=d.route,
                 extra=req.get("extra") or {}, resume=index.get(req.get("resumes") or ""),
                 notes=req.get("notes") or "", continues=req.get("continues"))
    else:
        _stop_codex_runners(ctx, events)
        _end(ctx, read_events(ctx.paths), unreconciled=progress)


def _stop_codex_runners(ctx: _Run, events: list[dict]) -> None:
    """Before reconciliation ends the run: stop every Codex dispatch that has no result
    (`dispatch.cancel`, termination confirmed), so no detached runner writes, or commits,
    after the outcome. Their results are left unrecorded: the state they would be recorded
    against could not be matched."""
    for d in _dispatches(events).values():
        if d.result is None and d.req["vendor"] == "codex":
            _cancel_codex(ctx, d)


def _synth_started(ctx: _Run, d: _Dispatch) -> dict:
    """Record a start for a dispatch whose start was never recorded, so its result (or its
    still-running Codex runner) can be recorded against it."""
    return append_event(ctx.paths, "worker_started", dispatch_id=d.id, worker_id=f"{d.id}:unrecorded",
                        card=d.req["card"], deadline=_format_time(_now() + _budget_seconds(d.req)),
                        session=_current_session(ctx), reconciled=True)


class _Mismatch(Exception):
    """Reconciliation found a state no rule matches; the message says what."""


def _reconcile_dispatch(ctx: _Run, events: list[dict], d: _Dispatch) -> str:
    """Settle one dispatch the run waits on whose worker (or proxy) died with an earlier
    session; returns what was found."""
    who = "the run" if d.req["card"] is None else f"card {d.req['card']}"
    started = d.started
    if d.req["vendor"] == "codex":
        wd = ctx.root / "workers" / d.id
        ours = any(e["kind"] == "reconciled" and e.get("stage") == "cancel" and e.get("dispatch_id") == d.id
                   for e in events)
        state = dsp.runner_state(wd)
        if state == "running":
            if started is None:
                _synth_started(ctx, d)
            return f"{who}'s Codex worker is still running; waiting for it"
        if state == "finished" and not ours:
            left = dsp.stop_strays(wd)
            if left:
                raise EngineError(f"could not stop what Codex dispatch {d.id} left running: "
                                  + ", ".join(f"pid {r['pid']}" for r in left))
            started = started or _synth_started(ctx, d)
            result = dsp.load_result(wd)
            _store_result(ctx, d, started["worker_id"], "\n".join(dsp.reply_lines(result)),
                          stopped=result.exit_reason == "cancelled", charge=not started.get("reconciled"))
            return f"{who}'s Codex worker had finished; its result was recorded"
        if state != "finished":
            # Hung, orphaned (its processes outlived the runner, P14), dead or never started:
            # stop it (confirmed) before anything else; the intent makes a repeat treat the
            # cancelled result as an interruption.
            append_event(ctx.paths, "reconciled", stage="cancel", dispatch_id=d.id, runner=state)
            _cancel_codex(ctx, d)
    started = started or _synth_started(ctx, d)
    ev = _store_result(ctx, d, started["worker_id"], "", interrupted=True, charge=not started.get("reconciled"))
    if not ev.get("interrupted"):
        return f"{who}'s report was found and recorded"
    if d.kind in IMPLEMENTER_KINDS or d.kind == "final_fix":
        progress = _branch_progress(ctx, read_events(ctx.paths), d)
        if progress == "commits":
            return f"{who}'s commits were found and recorded"
        if progress == "nothing":
            return f"{who}'s {d.kind} worker was interrupted before it changed anything; it runs again"
        raise _Mismatch(progress)
    if d.req.get("gate"):
        return f"the {d.req['gate']} gate was interrupted; it runs again"
    return f"{who}'s {d.kind} worker was interrupted; it runs again"


def _reconcile(ctx: _Run, events: list[dict]) -> None:
    """After a `resumed`, before anything new: settle every step a dead session left in flight,
    then append `reconciled{resumed_seq, found, headline}`. Repeat-safe: a crash part-way
    leaves what was settled recorded, and the next call settles the rest.

    - A dispatch with no result: a live Codex runner with a fresh heartbeat is waited for (its
      result is recorded when it lands); a finished runner's result is recorded; a hung,
      orphaned or dead runner is cancelled first (confirmed). Otherwise the worker counts as
      interrupted: a report in the scratch folder is recorded as its result, else the result
      is marked interrupted and the next step continues from the branch's commits or requests
      the same attempt again (`_after_interrupted`).
    - A Codex dispatch the run no longer waits on (a parked card's) is cancelled; a finished
      one's leftover processes are stopped (P14).
    - An acceptance whose fast-forward may not have happened is completed.
    - Integration, a fix round and the gates resume from the recorded events and Git.
    - Anything that matches none of these ends the run failed with the mismatch, after every
      Codex dispatch without a result is stopped (confirmed), so nothing writes after the outcome.

    It runs before a pending cancel is acted on: it only records what really happened, and
    the cancel then ends the run on that state."""
    since = _latest_resumed(events)
    _charge_earlier_sessions(ctx)
    problem = _unmatched(ctx, events)
    if problem:
        _fail_reconciliation(ctx, since, [], problem)
        return
    try:
        _settle(ctx, events, since)
    except _Mismatch as e:
        # Decided here, not left to the next step: a pending cancel must not mask it.
        _fail_reconciliation(ctx, since, [], str(e))


def _fail_reconciliation(ctx: _Run, since: int, found: list[str], problem: str) -> None:
    """End the run failed with the mismatch, after every Codex dispatch without a result is
    stopped (confirmed). `failed` outranks any cancel (decide_outcome's order)."""
    _stop_codex_runners(ctx, read_events(ctx.paths))
    append_event(ctx.paths, "reconciled", resumed_seq=since, found=[*found, f"state could not be reconciled: {problem}"],
                 headline=f"Resumed; state could not be reconciled: {problem}")
    _end(ctx, read_events(ctx.paths), unreconciled=problem)


def _settle(ctx: _Run, events: list[dict], since: int) -> None:
    """`_reconcile`'s body once the run as a whole matches: settle each step in flight, then
    record the summary. Raises _Mismatch for a step no rule matches."""
    index = _dispatches(events)
    current = {d.id for d in _open_dispatches(ctx, events, index)}
    found: list[tuple[int | None, str]] = []
    for d in sorted(index.values(), key=lambda x: x.req["seq"]):
        if d.req["vendor"] == "codex" and d.req["seq"] < since and d.result is not None:
            left = dsp.stop_strays(ctx.root / "workers" / d.id)
            if left:
                raise EngineError(f"could not stop what Codex dispatch {d.id} left running: "
                                  + ", ".join(f"pid {r['pid']}" for r in left))
        if d.result is not None or not _gone(events, d):
            continue
        if d.id in current:
            found.append((d.req["card"], _reconcile_dispatch(ctx, read_events(ctx.paths), d)))
        elif d.req["vendor"] == "codex" and d.started is not None:
            _cancel_codex(ctx, d)               # nothing waits on it: it must not write any more
    events = read_events(ctx.paths)
    cards = fold_cards(events, ctx.numbers)
    for n in dependency_order(ctx.deck):
        c = cards[n]
        if c.state == "accepting":
            _finish_accept(ctx, n, c.accept_target)
            found.append((n, f"card {n}'s acceptance was completed"))
        elif c.state == "integrating" and not any(n == m for m, _ in found):
            found.append((n, f"card {n}'s integration resumes from Git's state"))
        elif c.state == "fixing" and c.dispatch_kind != "fix":
            found.append((n, f"card {n}'s fix round resumes from its findings"))
    lines = [text for _, text in found]
    if found:
        n, text = found[0]
        headline = f"Resumed at card {n}; {text}" if n is not None else f"Resumed at the end of the run; {text}"
    else:
        headline = "Resumed; nothing was in flight"
    append_event(ctx.paths, "reconciled", resumed_seq=since, found=lines, headline=headline)


def _cancel_requested(ctx: _Run, events: list[dict]) -> bool:
    """The operator's cancel, from the flag (recorded as `cancelled` once). It is never undone:
    a relaunch finishes it."""
    if any(e["kind"] == "cancelled" for e in events):
        ctx.paths.cancel_flag.unlink(missing_ok=True)
        return True
    if ctx.paths.cancel_flag.exists():
        append_event(ctx.paths, "cancelled", by="operator")
        ctx.paths.cancel_flag.unlink(missing_ok=True)
        return True
    return False


def _cancel_step(ctx: _Run, events: list[dict]) -> dict | None:
    """Stop every running worker, then end incomplete (cancelled). Codex runners are cancelled
    through the dispatcher (termination confirmed) and their results recorded; a Claude worker
    of this session gets `stop_worker`; one that died with an earlier session needs no stop
    and is recorded as interrupted. The outcome comes only after every worker is stopped.
    Partial work stays on the card branches: a Codex runner commits it (R39), and a stopped
    or dead Claude worker's uncommitted changes are committed by `_store_result` (R42)."""
    for d in sorted(_dispatches(events).values(), key=lambda x: x.req["seq"]):
        if d.result is not None:
            continue
        if d.req["vendor"] == "codex":
            result = _cancel_codex(ctx, d)     # also when never started: a late proxy then starts nothing
            if d.started is None:
                continue
            _store_result(ctx, d, d.started["worker_id"], "\n".join(dsp.reply_lines(result)),
                          stopped=result.exit_reason == "cancelled", charge=not d.started.get("reconciled"))
            return None
        if d.started is None:
            continue
        if _gone(events, d):
            _store_result(ctx, d, d.started["worker_id"], "", interrupted=True,
                          charge=not d.started.get("reconciled"))
            return None
        return {"action": "stop_worker", "dispatch_id": d.id, "worker_id": d.started["worker_id"],
                "vendor": "claude", "record_result": True, "reason": "the run is cancelled"}
    _charge_earlier_sessions(ctx)
    _end(ctx, events, cancelled=True)
    return None


def _pause_requested(ctx: _Run, events: list[dict]) -> bool:
    """The operator's pause, from the flag (recorded as `paused`); a relaunch ends it."""
    since = _latest_resumed(events)
    if any(e["kind"] == "paused" and e["seq"] > since for e in events):
        ctx.paths.pause_flag.unlink(missing_ok=True)
        return True
    if ctx.paths.pause_flag.exists():
        append_event(ctx.paths, "paused", by="operator")
        ctx.paths.pause_flag.unlink(missing_ok=True)
        return True
    return False


def _pause_step(ctx: _Run, events: list[dict]) -> dict | None:
    """Running workers finish (`wait`, or `stop_worker` at their deadline); nothing new starts.
    Then the run ends blocked (paused), with the handoff."""
    for d in _open_dispatches(ctx, events, _dispatches(events)):
        if d.started is not None:
            return _in_flight(ctx, d)
    _end_blocked(ctx, events, "paused")
    return None


def _outcome_text(ended: dict) -> str:
    q = ended.get("qualifier") or ""
    return f"{ended.get('outcome')}" + (f" ({q})" if q else "")


def _flag(run_dir: Path, what: str) -> RunPaths:
    paths = run_paths(Path(run_dir).absolute())
    if not paths.run_json.is_file():
        raise EngineError(f"not a run directory (no run.json): {paths.root}")
    ended = _ended(read_events(paths))
    if ended is not None and (what == "pause" or ended.get("outcome") != "blocked"):
        raise EngineError(f"this run has ended: {_outcome_text(ended)}; there is nothing to {what}")
    flag = paths.pause_flag if what == "pause" else paths.cancel_flag
    write_json_atomic(flag, {"requested_at": utc_now(), "by_pid": os.getpid()})
    return paths


def _run_command(paths: RunPaths) -> str:
    return f"{shlex.quote(str(dsp.CLI_PATH))} run {shlex.quote(str(paths.root))}"


def request_pause(run_dir: Path) -> str:
    """`leadhand pause`: create the pause flag. Running workers finish, nothing new starts, the
    handoff is written and the run ends blocked (paused); `leadhand run` continues it."""
    paths = _flag(run_dir, "pause")
    return ("pause requested: running workers finish, nothing new starts, and the run ends blocked "
            f"(paused) with a handoff. Continue it later with: {_run_command(paths)}")


def request_cancel(run_dir: Path) -> str:
    """`leadhand cancel`: create the cancel flag. Every running worker is stopped (confirmed),
    then the run ends incomplete (cancelled); partial work stays on the card branches."""
    paths = _flag(run_dir, "cancel")
    return ("cancel requested: every running worker is stopped and the run ends incomplete (cancelled); "
            "partial work stays on the card branches. If no supervisor is running, "
            f"{_run_command(paths)} finishes the cancel.")


# --------------------------------------------------------------------------
# record

def record(run_dir: Path, kind: str, **fields: Any) -> dict:
    try:
        return _record(run_dir, kind, **fields)
    finally:
        _write_status(run_dir)


def _record(run_dir: Path, kind: str, **fields: Any) -> dict:
    """Record what the supervisor saw or decided. Returns the event written (or the one
    already there, for a repeated `started` or `result`)."""
    handlers = {"started": _record_started, "result": _record_result, "ruling": _record_ruling,
                "park": _record_park, "diff-check": _record_diff_check}
    if kind not in handlers:
        raise EngineError(f"unknown record kind {kind!r}; expected one of {', '.join(RECORD_KINDS)}")
    handler = handlers[kind]
    try:
        inspect.signature(handler).bind(None, **fields)
    except TypeError as e:
        raise EngineError(f"record {kind}: {e}") from None
    return handler(_load(run_dir, check_digest=False), **fields)


def _known_dispatch(events: list[dict], dispatch_id: str) -> _Dispatch:
    d = _dispatches(events).get(dispatch_id)
    if d is None:
        raise EngineError(f"no dispatch {dispatch_id!r} was requested")
    return d


def _not_ended(events: list[dict]) -> None:
    ended = _ended(events)
    if ended is not None:
        raise EngineError(f"the run has ended ({ended['outcome']})")


def _card_state(ctx: _Run, events: list[dict], card: Any) -> CardState:
    try:
        n = int(card)
    except (TypeError, ValueError):
        raise EngineError(f"not a card number: {card!r}") from None
    cards = fold_cards(events, ctx.numbers)
    if n not in cards:
        raise EngineError(f"no card {n} in the deck")
    return cards[n]


def _record_started(ctx: _Run, dispatch_id: str, worker_id: str) -> dict:
    events = read_events(ctx.paths)
    d = _known_dispatch(events, dispatch_id)
    if not worker_id or not str(worker_id).strip():
        raise EngineError("record started needs the worker's agent ID")
    if d.started is not None:
        if d.started["worker_id"] == worker_id:
            return d.started
        raise EngineError(f"dispatch {dispatch_id} already started worker {d.started['worker_id']}")
    _not_ended(events)
    if d.req["card"] is None:
        current = max(_run_dispatches(_dispatches(events)), key=lambda x: x.req["seq"])
        if current.id != dispatch_id:
            raise EngineError(f"dispatch {dispatch_id} is not the run's current dispatch")
    else:
        c = _card_state(ctx, events, d.req["card"])
        if c.dispatch_id != dispatch_id:
            raise EngineError(f"dispatch {dispatch_id} is not card {c.number}'s current dispatch")
    deadline = _format_time(_now() + _budget_seconds(d.req))
    # The supervisor session the worker runs under: a Claude worker dies with it.
    return append_event(ctx.paths, "worker_started", dispatch_id=dispatch_id, worker_id=worker_id,
                        card=d.req["card"], deadline=deadline, session=_current_session(ctx))


def _copy_if_missing(src: Path | None, dest: Path, text: str | None = None,
                     notes: list[str] | None = None) -> bool:
    """Copy a worker's file into the run folder once. Only a regular file is copied, never
    through a symlink (a worker could point one anywhere); anything else is noted."""
    if dest.exists():
        return True
    if text is None:
        if src is None:
            return False
        try:
            fd = os.open(src, os.O_RDONLY | os.O_NOFOLLOW | getattr(os, "O_NONBLOCK", 0))
        except FileNotFoundError:
            return False
        except OSError as e:
            if notes is not None:
                notes.append(f"{src} not copied: it is a symlink or cannot be opened ({e.strerror or e})")
            return False
        with os.fdopen(fd, "rb") as f:
            if not stat.S_ISREG(os.fstat(f.fileno()).st_mode):
                if notes is not None:
                    notes.append(f"{src} not copied: not a regular file")
                return False
            data = f.read()
    else:
        data = text.encode("utf-8", "surrogateescape")
    _write_bytes_atomic(dest, data)
    return True


def _head(report: str | None, reply: str, codex: dsp.DispatchResult | None, stopped: bool,
          interrupted: bool = False) -> dict:
    """Status from the report's head line, else (interrupted -> SPENT, marked interrupted: the
    restart decides from the branch what follows), (stopped -> SPENT), the reply's head, the
    Codex runner's result, a Claude turn-limit note (SPENT), else BLOCKED with the raw reply kept."""
    def from_text(text: str, source: str) -> dict:
        key, value, blocked_by = parse_report_head(text)
        out = {"status": value if key == "Status" else "READY", "blocked_by": blocked_by, "head": key,
               "verdict": value if key == "Verdict" else None, "result": value if key == "Result" else None,
               "source": source, "raw_kept": False}
        if out["verdict"] == "clean" and _SEVERE_FINDING.search(text):
            out["verdict"], out["cross_checked"] = "defects", True
        return out

    if _has_status(report):
        return from_text(report, "report")
    if interrupted:
        return {"status": "SPENT", "blocked_by": "", "head": "Status", "verdict": None, "result": None,
                "source": "interrupted", "raw_kept": False, "interrupted": True}
    if stopped:
        return {"status": "SPENT", "blocked_by": "", "head": "Status", "verdict": None, "result": None,
                "source": "stopped", "raw_kept": False}
    if _has_status(reply):
        return from_text(reply, "reply")
    if codex is not None and codex.status:
        return {"status": codex.status, "blocked_by": codex.blocked_by, "head": "Status",
                "verdict": None, "result": None, "source": "runner", "raw_kept": True}
    if _TURN_LIMIT.search(reply or ""):
        return {"status": "SPENT", "blocked_by": "", "head": "Status", "verdict": None, "result": None,
                "source": "turn limit", "raw_kept": True}
    return {"status": "BLOCKED", "blocked_by": "", "head": "Status", "verdict": None, "result": None,
            "source": "unparsed", "raw_kept": True}


def _record_result(ctx: _Run, dispatch_id: str, worker_id: str, reply: str = "", stopped: bool = False,
                   reply_file: str | None = None) -> dict:
    if reply_file is not None and reply:
        raise EngineError("record result takes a reply or a reply file, not both")
    events = read_events(ctx.paths)
    d = _known_dispatch(events, dispatch_id)
    if reply_file is not None:
        _check_reply_file(ctx, d, reply_file)
    if d.result is not None:
        return d.result
    if d.started is None:
        raise EngineError(f"dispatch {dispatch_id} has no started worker; record started first")
    if d.started["worker_id"] != worker_id:
        raise EngineError(f"dispatch {dispatch_id} started worker {d.started['worker_id']}, not {worker_id}")
    if d.req["vendor"] == "codex":
        return _record_codex_result(ctx, d, worker_id, stopped=stopped)
    reply = _read_reply_file(reply_file) if reply_file is not None else (reply or "")
    return _store_result(ctx, d, worker_id, reply, stopped=stopped)


def _record_codex_result(ctx: _Run, d: _Dispatch, worker_id: str, *, stopped: bool) -> dict:
    """A Codex-route dispatch's result comes only from the runner's result.json, never from
    the proxy's reply (the proxy may give up early, or invent one). While the runner runs,
    nothing is recorded: the `wait` it returns lets `next` and the timer pick up result.json
    when it lands. With no live runner and no result (the proxy never ran the command, or
    the runner died), and for `--stopped` (the supervisor stopped the proxy: a skipped card),
    the runner is stopped and the dispatch finalized first (`dispatch.cancel`), so no Codex
    process writes after the result is recorded."""
    wd = ctx.root / "workers" / d.id
    result = dsp.load_result(wd)
    if result is None and not stopped and dsp.runner_alive(wd):
        return {"action": "wait", "dispatch_id": d.id, "worker_id": worker_id, "deadline": d.started["deadline"],
                "reply_file": str(_reply_file(ctx, d.req["card"], d.id)),
                "reason": f"the Codex worker for {_whose(d.req)} is still running; its result is recorded "
                          "from the runner when it finishes (the timer reports it)"}
    if result is None:
        result = _cancel_codex(ctx, d)
    # A runner that finished before the cancel keeps its own status.
    return _store_result(ctx, d, worker_id, "\n".join(dsp.reply_lines(result)),
                         stopped=result.exit_reason == "cancelled")


def _store_result(ctx: _Run, d: _Dispatch, worker_id: str, reply: str, *, stopped: bool = False,
                  interrupted: bool = False, charge: bool = True) -> dict:
    """Copy the dispatch's files, work out its head, charge the worker and append the attempt
    and `result_recorded` (repeat-safe). `interrupted`: the worker died with an earlier
    supervisor session (reconciliation); without a report the result is marked interrupted.
    `charge` False: no worker is known (its start was never recorded), so nothing is charged
    and no attempt is written."""
    dispatch_id = d.id
    crash_point("before_result_record")
    req, n = d.req, d.req["card"]
    folder = ctx.card_dir(n, dispatch_id)
    folder.mkdir(parents=True, exist_ok=True)
    files, skipped = {}, []
    for name, src in (("brief", Path(req["brief"])), ("report", Path(req["report"]))):
        if _copy_if_missing(src, folder / f"{name}.md", notes=skipped):
            files[name] = str(folder / f"{name}.md")
    _copy_if_missing(None, folder / "reply.md", text=reply)
    files["reply"] = str(folder / "reply.md")
    codex, commit = None, {}
    if req["vendor"] == "codex":
        codex = dsp.load_result(ctx.root / "workers" / dispatch_id)
        commit = dsp.load_commit(ctx.root / "workers" / dispatch_id)
    head_info = _head(_read_text(folder / "report.md"), reply, codex, stopped, interrupted)
    if dsp.commit_failed(commit):
        # The runner could not commit the worker's changes (ruling R39): whatever the report
        # says, the work is not on the branch, so the card must not go on as if it were.
        head_info = {**head_info, "status": "BLOCKED", "blocked_by": "environment", "head": "Status",
                     "verdict": None, "result": None, "source": "runner commit"}
        head_info.pop("interrupted", None)
    partial: dict[str, Any] = {"files_skipped": skipped} if skipped else {}
    if (req["vendor"] == "claude" and (d.kind in IMPLEMENTER_KINDS or d.kind == "final_fix")
            and (stopped or head_info.get("interrupted")) and _cancelling(ctx, read_events(ctx.paths))):
        # R42 (cancel): a Claude worker the cancel stopped, or found dead, leaves its partial
        # work committed on its branch (a Codex runner does this itself, R39). Not yet for a
        # stop at the deadline or an interruption found on a relaunch: see the fix report.
        partial = {**partial, **_commit_partial(ctx, d)}
    tip = None
    if d.kind in IMPLEMENTER_KINDS:
        card_branch = _card_state(ctx, read_events(ctx.paths), n).branch or card_branch_name(ctx.branch, n)
        tip = head(ctx.ws, f"refs/heads/{card_branch}")
    elif d.kind == "final_fix":
        tip = head(ctx.ws, f"refs/heads/{_final_branch(ctx)}")
    crash_point("after_result_files")

    role = req["role"] if req["vendor"] == "claude" else "proxy"
    charge_id = f"{dispatch_id}:claude:{worker_id}"
    if charge:
        charged = usage.charge(ctx.paths, charge_id=charge_id, source="claude", key=worker_id,
                               dispatch_id=dispatch_id, role=role,
                               path_or_thread=usage.claude_subagent_transcript(worker_id, _projects_dir()),
                               possibly_incomplete=stopped or bool(head_info.get("interrupted")))
    crash_point("after_charge_before_attempt")
    if not charge:
        with state_lock(ctx.paths):
            again = _known_dispatch(read_events(ctx.paths), dispatch_id)
            if again.result is not None:
                return again.result
            return append_event(ctx.paths, "result_recorded", dispatch_id=dispatch_id, card=n,
                                attempt=req["attempt"], dispatch_kind=d.kind, worker_id=worker_id, tip=tip,
                                session_id=codex.session_id if codex else None, files=files,
                                stopped=bool(stopped), charge_ids=[], reconciled=True, **head_info, **partial,
                                **({k: commit.get(k) for k in ("committed", "commit_error", "commit_skipped", "gitlinks_skipped")
                                    if k != "gitlinks_skipped" or commit.get(k)}
                                   if req["vendor"] == "codex" else {}))

    ended_at = utc_now()
    model = req["model"] if req["vendor"] == "claude" else None
    cost = (usage.estimate_cost(charged.usage, model, ctx.config.prices)
            if model and charged.usage.known else None)
    attempt = {
        "dispatch": dispatch_id, "run": ctx.meta.get("run_id"), "card": n, "attempt": req["attempt"],
        "role": role, "vendor": "claude", "model": model, "effort": req["effort"] if model else None,
        "agent": CODEX_AGENT if req["vendor"] == "codex" else agent_for(req["role"], "claude"),
        "worker_id": worker_id, "started_at": d.started["ts"], "ended_at": ended_at, "steps": None,
        "charge_ids": [charge_id], "usage": dsp.usage_dict(charged.usage), "usage_known": charged.usage.known,
        "cost_estimate": cost,
        "exit_reason": "stopped" if stopped else "interrupted" if head_info.get("interrupted") else "completed",
        "status": head_info["status"], "blocked_by": head_info["blocked_by"], "recorded_by": "engine",
    }
    with state_lock(ctx.paths):
        if not any(a.get("dispatch") == dispatch_id and a.get("recorded_by") == "engine"
                   for a in read_attempts(ctx.paths)):
            append_attempt(ctx.paths, attempt)
        again = _known_dispatch(read_events(ctx.paths), dispatch_id)
        if again.result is not None:
            return again.result
        return append_event(ctx.paths, "result_recorded", dispatch_id=dispatch_id, card=n,
                            attempt=req["attempt"], dispatch_kind=d.kind, worker_id=worker_id, tip=tip,
                            session_id=codex.session_id if codex else None, files=files, stopped=bool(stopped),
                            charge_ids=[charge_id], **({"reconciled": True} if interrupted else {}), **head_info,
                            **partial,
                            **({k: commit.get(k) for k in ("committed", "commit_error", "commit_skipped", "gitlinks_skipped")
                                    if k != "gitlinks_skipped" or commit.get(k)}
                               if req["vendor"] == "codex" else {}))


def _cancelling(ctx: _Run, events: list[dict]) -> bool:
    """A cancel is recorded, or requested and not yet seen by `next` (reconciliation comes first)."""
    return any(e["kind"] == "cancelled" for e in events) or ctx.paths.cancel_flag.exists()


def _commit_partial(ctx: _Run, d: _Dispatch) -> dict[str, str]:
    """R42: commit what a stopped or interrupted Claude worker left uncommitted, when the
    workspace is on its branch and no Git operation is in progress: its tracked changes and
    the untracked files Git does not ignore that were not there when it was dispatched (I3),
    unsigned and without hooks, with the `Leadhand-Dispatch` trailer, after a
    `reconciled{stage: partial_work}` intent. Repeat-safe: after a crash past the commit the
    workspace is clean and nothing more is committed. Returns `partial_commit` (the commit)
    or `partial_commit_error` for the result event, or {} when there was nothing to commit."""
    n = d.req["card"]
    if n is None:
        branch = _final_branch(ctx)
    else:
        branch = fold_cards(read_events(ctx.paths), ctx.numbers)[n].branch or card_branch_name(ctx.branch, n)
    try:
        if _current_branch(ctx) != branch or _operation(ctx) is not None:
            return {}
        tracked, loose = status_entries(ctx.ws)
        earlier = set(d.req.get("untracked") or ())
        new = [p for p in _foreign_untracked(ctx, loose) if p not in earlier]
        if not tracked and not new:
            return {}
        before = head(ctx.ws, f"refs/heads/{branch}")
        if not any(e["kind"] == "reconciled" and e.get("stage") == "partial_work" and e.get("dispatch_id") == d.id
                   for e in read_events(ctx.paths)):
            append_event(ctx.paths, "reconciled", stage="partial_work", dispatch_id=d.id, branch=branch,
                         head_before=before)
        # Tracked changes and the files that appeared during the dispatch; never another
        # worker's untracked leftovers (I3).
        staged, gitlinks = stage_work(ctx.ws, new_paths=new)
        if not staged:
            return {"partial_gitlinks_skipped": gitlinks} if gitlinks else {}
        git(ctx.ws, "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "commit", "--quiet",
            "--no-verify", "-m", f"{d.id}: partial work (stopped)", "-m", f"{dsp.TRAILER}: {d.id}")
        return {"partial_commit": head(ctx.ws, f"refs/heads/{branch}"),
                **({"partial_gitlinks_skipped": gitlinks} if gitlinks else {})}
    except GitError as e:
        return {"partial_commit_error": str(e)}


def _record_ruling(ctx: _Run, card: Any, text: str) -> dict:
    events = read_events(ctx.paths)
    _not_ended(events)
    c = _card_state(ctx, events, card)
    d = _dispatches(events).get(c.dispatch_id or "")
    asked = (c.state == "reported" and d is not None and d.result is not None
             and d.kind in IMPLEMENTER_KINDS and d.result["status"] == "QUESTION")
    if not asked:
        raise EngineError(f"card {c.number} has no question waiting for a ruling")
    if any(e["kind"] == "ruling" and e.get("dispatch_id") == d.id for e in events):
        raise EngineError(f"the question from {d.id} already has a ruling")
    if not text or not text.strip():
        raise EngineError("a ruling needs text")
    return append_event(ctx.paths, "ruling", card=c.number, dispatch_id=d.id, text=text.strip())


def _record_park(ctx: _Run, card: Any, reason: str) -> dict:
    events = read_events(ctx.paths)
    _not_ended(events)
    c = _card_state(ctx, events, card)
    if c.state in ("accepted", "parked"):
        raise EngineError(f"card {c.number} is already {c.state}")
    if not reason or not reason.strip():
        raise EngineError("parking a card needs a reason")
    # A card parked on its worker's question waits on the operator's decision (R44).
    d = _dispatches(events).get(c.dispatch_id or "")
    asked = (c.state == "reported" and d is not None and d.result is not None
             and d.kind in IMPLEMENTER_KINDS and d.result["status"] == "QUESTION"
             and not any(e["kind"] == "ruling" and e.get("dispatch_id") == d.id for e in events))
    return append_event(ctx.paths, "parked", card=c.number, reason=reason.strip(),
                        **({"decision": True} if asked else {}))


def _record_diff_check(ctx: _Run, card: Any, ok: Any, note: str = "") -> dict:
    events = read_events(ctx.paths)
    _not_ended(events)
    c = _card_state(ctx, events, card)
    d = _dispatches(events).get(c.dispatch_id or "")
    pending = (c.state == "reported" and d is not None and d.result is not None
               and d.kind in IMPLEMENTER_KINDS and d.result["status"] == "READY"
               and _fix_round(events, c.number, d.req["attempt"], before_seq=d.req["seq"]) is not None
               and _fix_diff_path(ctx, d).exists())
    if not pending:
        raise EngineError(f"card {c.number} has no diff check waiting")
    if any(e["kind"] == "diff_checked" and e.get("dispatch_id") == d.id for e in events):
        raise EngineError(f"the diff of {d.id} was already checked")
    if isinstance(ok, str):
        if ok.lower() not in ("yes", "no", "true", "false"):
            raise EngineError(f"diff-check ok must be yes or no, got {ok!r}")
        ok = ok.lower() in ("yes", "true")
    return append_event(ctx.paths, "diff_checked", card=c.number, dispatch_id=d.id, ok=bool(ok),
                        note=(note or "").strip())


def unpark(run_dir: Path, card: Any, note: str) -> dict:
    """The operator's `leadhand unpark`: a parked card continues, its last worker resumed with
    the note as the answer (or a fresh worker given the note). Returns the `unparked` event."""
    ctx = _load(run_dir, check_digest=False)
    if not note or not note.strip():
        raise EngineError("unparking a card needs a note")
    with state_lock(ctx.paths):
        events = read_events(ctx.paths)
        ended = _ended(events)
        if ended is not None and ended.get("outcome") != "blocked":
            # Only a blocked run continues (a relaunch never reopens any other ending).
            raise EngineError(f"this run has ended: {_outcome_text(ended)}; a parked card can only "
                              "continue in a blocked or unfinished run. Start a new run from the deck.")
        c = _card_state(ctx, events, card)
        if c.state != "parked":
            raise EngineError(f"card {c.number} is not parked (it is {c.state})")
        return append_event(ctx.paths, "unparked", card=c.number, note=note.strip())


# --------------------------------------------------------------------------
# timer

def timer(run_dir: Path, dispatch_id: str, *, interval: float = TIMER_INTERVAL) -> str:
    """Return "result" once the dispatch has a result, "deadline" once its deadline passes,
    or "flag" as soon as the operator asks to pause or cancel the run (`next` acts on it)."""
    paths = run_paths(Path(run_dir).absolute())
    if not paths.run_json.is_file():
        raise EngineError(f"not a run directory (no run.json): {paths.root}")
    while True:
        events = read_events(paths)
        d = _known_dispatch(events, dispatch_id)
        if d.result is not None:
            return "result"
        if paths.cancel_flag.exists() or paths.pause_flag.exists():
            return "flag"
        if (d.req["vendor"] == "codex" and d.started is not None
                and dsp.load_result(paths.root / "workers" / dispatch_id) is not None):
            return "result"           # the runner finished: `next` records its result.json
        wait = interval
        if d.started is not None:
            left = _parse_time(d.started["deadline"]) - _now()
            if left <= 0:
                return "deadline"
            wait = min(wait, left)
        time.sleep(wait)


# --------------------------------------------------------------------------
# Commands

def _print_json(value: dict) -> None:
    print(json.dumps(value, ensure_ascii=False), flush=True)


def _guard(fn):
    def run(args) -> int:
        try:
            return fn(args)
        except (EngineError, DeckError, ConfigError, GitError, OSError, ValueError) as e:
            print(f"error: {e}", file=sys.stderr)
            return 1
    return run


def _cmd_start(args) -> int:
    root, existing, note = _start(Path(args.deck), Path(args.repo), None, args.new)
    if note:
        print(note)
    if not existing:
        print(f"run: {root}")
    return 0


def _cmd_next(args) -> int:
    _print_json(next_action(Path(args.run_dir)))
    return 0


def _stdin_or(value: str | None) -> str | None:
    return sys.stdin.read() if value == "-" else value


def _cmd_record(args) -> int:
    run_dir = Path(args.run_dir)
    if args.kind == "started":
        event = record(run_dir, "started", dispatch_id=args.dispatch, worker_id=args.worker)
    elif args.kind == "result":
        event = record(run_dir, "result", dispatch_id=args.dispatch, worker_id=args.worker,
                       reply=_stdin_or(args.reply) or "", stopped=args.stopped, reply_file=args.reply_file)
    elif args.kind == "ruling":
        event = record(run_dir, "ruling", card=args.card, text=_stdin_or(args.text))
    elif args.kind == "park":
        event = record(run_dir, "park", card=args.card, reason=args.reason)
    else:
        event = record(run_dir, "diff-check", card=args.card, ok=args.ok, note=args.note)
    _print_json(event)
    return 0


def _cmd_pause(args) -> int:
    print(request_pause(Path(args.run_dir)), flush=True)
    return 0


def _cmd_cancel(args) -> int:
    print(request_cancel(Path(args.run_dir)), flush=True)
    return 0


def _cmd_timer(args) -> int:
    what = timer(Path(args.run_dir), args.dispatch)
    print({"result": "result recorded", "deadline": "deadline passed"}.get(what, "pause or cancel requested"),
          flush=True)
    return 0


def add_commands(sub) -> None:
    p = sub.add_parser("start", help="start a run for a deck (or report the unfinished one)")
    p.add_argument("deck")
    p.add_argument("--repo", default=".")
    p.add_argument("--new", action="store_true", help="start a new run even if one is unfinished")
    p.set_defaults(func=_guard(_cmd_start))

    p = sub.add_parser("next", help="perform due steps and print the supervisor's next action")
    p.add_argument("run_dir")
    p.set_defaults(func=_guard(_cmd_next))

    p = sub.add_parser("record", help="record a worker start or result, a ruling, a park or a diff check")
    p.add_argument("run_dir")
    p.add_argument("kind", choices=RECORD_KINDS)
    p.add_argument("--dispatch", help="dispatch ID (started, result)")
    p.add_argument("--worker", help="the worker's agent ID (started, result)")
    replies = p.add_mutually_exclusive_group()
    replies.add_argument("--reply", help="the worker's reply (result); - reads stdin")
    replies.add_argument("--reply-file", help="a file holding the worker's reply (result)")
    p.add_argument("--stopped", action="store_true", help="the worker was stopped at its deadline (result)")
    p.add_argument("--card", type=int, help="card number (ruling, park, diff-check)")
    p.add_argument("--text", help="the ruling (ruling); - reads stdin")
    p.add_argument("--reason", help="why the card is parked (park)")
    p.add_argument("--ok", choices=("yes", "no"), help="whether the diff is acceptable (diff-check)")
    p.add_argument("--note", default="", help="what the diff check found (diff-check)")
    p.set_defaults(func=_guard(_cmd_record))

    p = sub.add_parser("pause", help="pause a run: running workers finish, nothing new starts, it ends blocked")
    p.add_argument("run_dir")
    p.set_defaults(func=_guard(_cmd_pause))

    p = sub.add_parser("cancel", help="cancel a run: stop every worker, end it incomplete (cancelled)")
    p.add_argument("run_dir")
    p.set_defaults(func=_guard(_cmd_cancel))

    p = sub.add_parser("timer", help="wait until a dispatch has a result or its deadline passes")
    p.add_argument("run_dir")
    p.add_argument("--dispatch", required=True)
    p.set_defaults(func=_guard(_cmd_timer))
