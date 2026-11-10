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
    done         {outcome, qualifier, reasons} when the run ended; with outcome None and
                 per-card `cards` when every card is settled (end-of-run steps follow).

Dispatch IDs are `c<card>-a<attempt>-<kind>-<n>`, `n` counting that kind within the
card's attempt.

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
"""
from __future__ import annotations

import hashlib
import inspect
import json
import os
import re
import secrets
import shutil
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any

from leadhand import dispatch as dsp
from leadhand import usage
from leadhand.briefs import CODEX_AGENT, agent_for, dispatch_message, parse_report_head, write_brief
from leadhand.config import VENDORS, Config, ConfigError, Route, docs_dir_for, load_config
from leadhand.deck import Card, Deck, DeckError, check_deck, dependency_order, parse_deck
from leadhand.gitops import (GitError, card_branch_name, changed_paths, cut_card_branch, default_base,
                             detect_violation, fast_forward, git, head, is_test_or_doc_path,
                             needs_integration, prepare_workspace, preserve_and_restore, rebase_onto)
from leadhand.route import LadderExhausted, PinnedVendorMissing, Resolution, resolve
from leadhand.state import (CardState, RunPaths, utc_now, append_attempt, append_event, crash_point,
                            decide_outcome, fold_cards, new_run_dir, read_attempts, read_events, run_paths,
                            state_lock, write_json_atomic)

ACTIONS = ("dispatch", "wait", "check_diff", "answer", "stop_worker", "done")
RECORD_KINDS = ("started", "result", "ruling", "park", "diff-check")
IMPLEMENTER_KINDS = ("implement", "fix", "answer", "continue", "integrate")
REVIEW_KINDS = ("review", "rereview")
# Reviewers and verifiers: they may build, but must leave the workspace and the run's refs as they were.
CHECK_KINDS = (*REVIEW_KINDS, "verify")
VOIDED_NOTE = "## An earlier run of this check was voided"
CODEX_GRACE_MINUTES = 5       # the Codex runner enforces its own limit; the proxy gets this much more
TIMER_INTERVAL = 10.0
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
    root = Path(run_dir).absolute()
    paths = run_paths(root)
    try:
        meta = json.loads(paths.run_json.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise EngineError(f"not a run directory (no run.json): {root}") from None
    except ValueError as e:
        raise EngineError(f"run.json is unreadable: {e}") from None
    if check_digest:
        source = Path(meta["deck_source"])
        try:
            digest = hashlib.sha256(source.read_bytes()).hexdigest()
        except OSError as e:
            raise EngineError(f"cannot read the deck {source}: {e}") from None
        if digest != meta["deck_digest"]:
            raise EngineError(f"deck changed since start (expected {meta['deck_digest']})")
    try:
        deck = parse_deck(paths.deck)
        config = load_config(Path(meta["repo"]))
    except (DeckError, ConfigError, OSError) as e:
        raise EngineError(str(e)) from None
    return _Run(root=root, paths=paths, meta=meta, deck=deck, config=config,
                ws=Path(meta["workspace"]), branch=meta["branch"])


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


def _done_action(outcome: dict) -> dict:
    return {"action": "done", "outcome": outcome["outcome"], "qualifier": outcome.get("qualifier", ""),
            "reasons": outcome.get("reasons", [])}


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


def _find_unfinished(docs: Path, source: str, digest: str) -> Path | None:
    """The newest run of this deck that has no outcome, or whose latest outcome is blocked
    (a blocked run continues, R25)."""
    found = []
    for root, meta in _runs(docs):
        if meta.get("deck_source") != source or meta.get("deck_digest") != digest:
            continue
        ended = _ended(read_events(run_paths(root)))
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
    docs = docs_dir_for(config, repo)
    if not new:
        existing = _find_unfinished(docs, str(deck_path), deck.digest)
        if existing is not None:
            return existing, True, f"existing run: {existing}"

    branch = _free_branch(docs, repo, deck.branch)
    slug = _slug(deck.title)
    root = new_run_dir(docs, slug, date.today())
    paths = run_paths(root)
    _write_bytes_atomic(paths.deck, raw)
    if deck.base_sha:
        base_ref, base_sha = deck.base_ref, head(repo, deck.base_sha)
    else:
        base_ref, base_sha = default_base(repo)
    # The worktree is named after the run directory, which is unique, so no two runs share it.
    ws = prepare_workspace(repo, run_branch=branch, base_sha=base_sha, slug=root.name,
                           use_worktree=config.run.worktree)
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
    """Perform every mechanical step that is due, then return the supervisor's next action."""
    ctx = _load(run_dir, check_digest=True)
    _charge_supervisor(ctx)
    for _ in range(MAX_STEPS):
        events = read_events(ctx.paths)
        ended = _ended(events)
        if ended is not None:
            return _done_action(ended)
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


def _pick(ctx: _Run, cards: dict[int, CardState]) -> int | None:
    """A card already in flight, else the first pending card whose dependencies are all accepted."""
    order = dependency_order(ctx.deck)
    for n in order:
        if cards[n].state not in ("pending", "accepted", "parked"):
            return n
    for n in order:
        if cards[n].state == "pending" and all(cards[d].state == "accepted"
                                               for d in ctx.card(n).depends_on if d in cards):
            return n
    return None


def _step(ctx: _Run, events: list[dict]) -> dict | None:
    """One mechanical step (returns None after appending an event) or an action."""
    cards = fold_cards(events, ctx.numbers)
    n = _pick(ctx, cards)
    if n is None:
        return _end_of_run(ctx, cards)
    c = cards[n]
    index = _dispatches(events)
    if c.state == "pending":
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


def _end_of_run(ctx: _Run, cards: dict[int, CardState]) -> dict:
    """Every card is accepted, parked, or waits on a parked card. End-of-run steps
    (final review, gates, outcome) come after this; until then the run reports where it stands."""
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
    return {"action": "done", "outcome": None, "qualifier": "", "cards": out,
            "reasons": [f"card {x['card']} {x['state']}: {x['reason']}" for x in out if x["reason"]]}


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
             extra: dict[str, str], resume: _Dispatch | None = None, notes: str = "") -> dict:
    """Write the brief and append `dispatch_requested`. Deterministic: after a crash before the
    event, the next call computes the same dispatch ID and rewrites the same brief."""
    n = 1 + sum(1 for e in events if e["kind"] == "dispatch_requested" and e.get("card") == c.number
                and e.get("attempt") == attempt and e.get("dispatch_kind") == kind)
    did = f"c{c.number}-a{attempt}-{kind}-{n}"
    full = {"base": _card_base(ctx, c, events), "branch": c.branch or "", "test_first": ctx.config.run.test_first,
            **extra}
    guard: dict[str, Any] = {}
    if c.branch:
        _check_out(ctx, c.branch)
        if kind in CHECK_KINDS:
            # What the worker must leave as it found it (R14): the card tip on the card branch,
            # and every ref of the run.
            guard = {"expected_branch": c.branch, "expected_head": head(ctx.ws, f"refs/heads/{c.branch}"),
                     "refs": _ref_snapshot(ctx)}
    brief, report = write_brief(kind, ws=ctx.ws, deck=ctx.deck, card=ctx.card(c.number), attempt=attempt,
                                dispatch_id=did, extra=full)
    if notes:
        with open(brief, "a", encoding="utf-8") as f:
            f.write("\n" + notes.rstrip("\n") + "\n")
    # No worker can have written this dispatch's report yet: one already there is left over
    # from an earlier run that used the same workspace.
    Path(report).unlink(missing_ok=True)
    resume_worker = resume_session = None
    if resume is not None:
        if route.vendor == "claude":
            resume_worker = resume.started["worker_id"] if resume.started else None
        else:
            resume_session = (resume.result or {}).get("session_id")
    try:
        minutes = dsp.limits_for(ctx.config, role).minutes
    except ConfigError as e:
        raise EngineError(str(e)) from None
    return append_event(ctx.paths, "dispatch_requested", dispatch_id=did, card=c.number, attempt=attempt,
                        dispatch_kind=kind, role=role, vendor=route.vendor, model=route.model,
                        effort=route.effort, brief=str(brief), report=str(report), extra=extra, notes=notes,
                        resumes=resume.id if resume is not None else None, resume_worker=resume_worker,
                        resume_session=resume_session, minutes=minutes, **guard)


def _check_out(ctx: _Run, branch: str) -> None:
    """Put the workspace on the card branch (it is elsewhere after another card ran, e.g. when a
    parked card resumes). Never creates the branch."""
    try:
        current = git(ctx.ws, "symbolic-ref", "--quiet", "--short", "HEAD")
    except GitError:
        current = None                      # detached
    if current != branch:
        git(ctx.ws, "checkout", "--quiet", branch, "--")


def _card_base(ctx: _Run, c: CardState, events: list[dict]) -> str:
    """The commit the card's work starts from: its cut base, or, once it has been integrated,
    where its branch meets the run branch (the run tip it was rebased onto)."""
    if c.branch and any(e["kind"] == "integration_started" and e.get("card") == c.number for e in events):
        return git(ctx.ws, "merge-base", f"refs/heads/{ctx.branch}", f"refs/heads/{c.branch}")
    return c.base or ""


def _budget_seconds(req: dict) -> float:
    minutes = req["minutes"] + (CODEX_GRACE_MINUTES if req["vendor"] == "codex" else 0)
    return minutes * dsp.minute_seconds()


def _codex_command(ctx: _Run, req: dict) -> list[str]:
    cmd = [str(dsp.CLI_PATH), "dispatch", "--run", str(ctx.root), "--dispatch-id", req["dispatch_id"],
           "--card", str(req["card"]), "--attempt", str(req["attempt"]), "--role", req["role"],
           "--model", req["model"], "--effort", req["effort"], "--brief", req["brief"],
           "--report", req["report"], "--cwd", str(ctx.ws)]
    if req.get("resume_session"):
        cmd += ["--resume", req["resume_session"]]
    return cmd


def _dispatch_action(ctx: _Run, req: dict) -> dict:
    brief, report = Path(req["brief"]), Path(req["report"])
    if req["vendor"] == "codex":
        agent, agent_model, agent_effort, resume = CODEX_AGENT, None, None, None
        message = dispatch_message(agent, brief, report, codex_command=_codex_command(ctx, req))
    else:
        agent, agent_model, agent_effort = agent_for(req["role"], "claude"), req["model"], req["effort"]
        resume = {"worker_id": req["resume_worker"]} if req.get("resume_worker") else None
        message = dispatch_message(agent, brief, report)
    return {"action": "dispatch", "dispatch_id": req["dispatch_id"], "kind": req["dispatch_kind"],
            "card": req["card"], "attempt": req["attempt"], "role": req["role"], "agent": agent,
            "agent_model": agent_model, "agent_effort": agent_effort,
            "route": {"vendor": req["vendor"], "model": req["model"], "effort": req["effort"]},
            "message": message, "brief": req["brief"], "report": req["report"], "resume": resume,
            "deadline": _format_time(_now() + _budget_seconds(req))}


def _in_flight(ctx: _Run, d: _Dispatch) -> dict:
    if d.started is None:
        return _dispatch_action(ctx, d.req)       # no worker is known to exist
    worker = d.started["worker_id"]
    deadline = d.started["deadline"]
    if _now() < _parse_time(deadline):
        return {"action": "wait", "dispatch_id": d.id, "worker_id": worker, "deadline": deadline,
                "reason": f"{d.kind} worker for card {d.req['card']} is running"}
    if d.req["vendor"] == "codex":
        result = _cancel_codex(ctx, d)
        # A runner that finished before the cancel keeps its own status (the proxy only hung).
        _record_result(ctx, d.id, worker, "\n".join(dsp.reply_lines(result)),
                       stopped=result.exit_reason == "cancelled")
        return {"action": "stop_worker", "dispatch_id": d.id, "worker_id": worker, "vendor": "codex",
                "record_result": False}
    return {"action": "stop_worker", "dispatch_id": d.id, "worker_id": worker, "vendor": "claude",
            "record_result": True}


def _cancel_codex(ctx: _Run, d: _Dispatch) -> dsp.DispatchResult:
    """Stop the dispatch's runner (and Codex with its tool processes) and finalize result.json.
    When the proxy never ran the command, the request is written first, so a late run of the
    same command finds the cancelled result instead of starting Codex."""
    req = d.req
    try:
        wd = dsp.write_request(dsp.request_for(
            ctx.root, d.id, card=req["card"], attempt=req["attempt"], role=req["role"], model=req["model"],
            effort=req["effort"], brief=Path(req["brief"]), report=Path(req["report"]), cwd=ctx.ws,
            resume=req.get("resume_session")))
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
    if d.kind in CHECK_KINDS and not _workspace_kept(ctx, c, d, events):
        return None                         # the result is void; the same check was requested again
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
    if status == "READY" and d.kind == "integrate":
        _after_integrate(ctx, c, d, events, index)
        return None
    if status == "READY":
        fix = _fix_round(events, c.number, d.req["attempt"], before_seq=d.req["seq"])
        if fix is not None:
            return _after_fix(ctx, c, d, fix, events, index)
        card = ctx.card(c.number)
        if ctx.config.run.card_review == "by-risk" and card.risk == "low" and not card.hard:
            _accept(ctx, c.number, r.get("tip") or c.tip)
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


def _worker_failed(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict],
                   index: dict[str, _Dispatch]) -> dict | None:
    """BLOCKED from any worker, or a reviewer that ended without a verdict.

    A result that already ended the run (an `outcome` after it) is seen again only after the
    run was resumed: the work is dispatched again as the same attempt, and the retry budget
    counts only results after the latest ending a `resumed` followed (a relaunch without an
    ending keeps the budget)."""
    r = d.result
    status = r["status"]
    if not any(e["kind"] == "outcome" and e["seq"] > r["seq"] for e in events):
        if status == "BLOCKED" and r.get("blocked_by") == "usage limit":
            _end_blocked(ctx, events, "provider capacity")
            return None
        # The budget restarts only after an ending the run was resumed from, not on every relaunch.
        since, ending = 0, None
        for e in events:
            if e["kind"] == "outcome":
                ending = e["seq"]
            elif e["kind"] == "resumed" and ending is not None:
                since = ending
        voided = {e.get("dispatch_id") for e in events if e["kind"] == "violation"}
        same = [x for x in _card_dispatches(index, c.number, attempt=d.req["attempt"], kinds=(d.kind,))
                if x.result is not None and x.result["seq"] > since and x.id not in voided]
        if status == "BLOCKED":
            failures = [x for x in same if x.result["status"] == "BLOCKED"
                        and x.result.get("blocked_by") != "usage limit"]
            if len(failures) >= 2:
                _end_blocked(ctx, events, "environment")
                return None
        else:
            failures = [x for x in same if x.result["status"] != "BLOCKED" and x.result.get("verdict") is None
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
        integ = append_event(ctx.paths, "integration_started", card=card,
                             onto=head(ctx.ws, f"refs/heads/{ctx.branch}"), tip=head(ctx.ws, f"refs/heads/{branch}"))
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
    return git(ctx.ws, "merge-base", sha, f"refs/heads/{branch}") == head(ctx.ws, sha)


def _verified(events: list[dict], card: int, target: str) -> bool:
    """A verify of exactly `target` passed (and was not voided)."""
    voided = {e.get("dispatch_id") for e in events if e["kind"] == "violation"}
    return any(d.kind == "verify" and d.req.get("card") == card and (d.req.get("extra") or {}).get("tip") == target
               and d.result is not None and d.result.get("result") == "pass" and d.id not in voided
               for d in _dispatches(events).values())


def _request_verify(ctx: _Run, c: CardState, events: list[dict], target: str) -> None:
    res = _resolve_or_park(ctx, ctx.card(c.number), role="verifier", attempt=1, last_vendor=None)
    if res is not None:
        _request(ctx, c, events, kind="verify", attempt=max(1, c.attempt), role=res.role, route=res.route,
                 extra={"tip": target})


def _abort_stale_rebase(ctx: _Run) -> None:
    """A rebase left in progress by a process that died mid-way: abort it, which puts the
    card branch back where it was (the rebase never completed, so nothing is lost)."""
    for name in ("rebase-merge", "rebase-apply"):
        if Path(git(ctx.ws, "rev-parse", "--path-format=absolute", "--git-path", name)).exists():
            git(ctx.ws, "rebase", "--abort")
            return


def _rebase(ctx: _Run, card: int, branch: str, integ: dict) -> None:
    """Rebase the card branch onto the run tip recorded by `integration_started`, then record
    `rebased` or `conflict`. Repeat-safe: a card branch that already contains the run tip
    counts as rebased; on conflict rebase_onto has aborted the rebase."""
    crash_point("before_rebase")
    _abort_stale_rebase(ctx)
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
    _request(ctx, c, events, kind="integrate", attempt=attempt, role=last.req["role"], route=last.route,
             extra={"run_tip": conflict["onto"], "conflicts": ", ".join(conflict["paths"]),
                    "card_intent": f"card {card.number} ({card.title}): {card.done_when}",
                    "run_intent": run_intent or "(no card was accepted since this card was cut)"})


def _after_integrate(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict],
                     index: dict[str, _Dispatch]) -> None:
    """An integrate attempt reported READY: its branch must now contain the run tip."""
    integ = [e for e in events if e["kind"] == "integration_started" and e.get("card") == c.number][-1]
    branch = c.branch or card_branch_name(ctx.branch, c.number)
    if _contains(ctx, branch, integ["onto"]):
        append_event(ctx.paths, "rebased", card=c.number, onto=integ["onto"],
                     tip=d.result.get("tip") or head(ctx.ws, f"refs/heads/{branch}"), dispatch_id=d.id)
    else:
        _escalate(ctx, c, events, index, "reported READY without rebasing the card branch onto the run tip")


def _request_integration_review(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch],
                                rebased: dict) -> None:
    """Re-review the integrated diff with a fresh reviewer."""
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


def _resume_unparked(ctx: _Run, c: CardState, events: list[dict], index: dict[str, _Dispatch], note: str) -> None:
    """Resume the card's last implementer worker with the operator's note as the answer when
    that worker is known; otherwise start a fresh worker on the card's attempt with the note."""
    started = [d for d in _card_dispatches(index, c.number, kinds=IMPLEMENTER_KINDS) if d.started is not None]
    last = max(started, key=lambda d: d.req["seq"]) if started else None
    if last is not None and (last.req["vendor"] == "claude" or (last.result or {}).get("session_id")):
        _request(ctx, c, events, kind="answer", attempt=last.req["attempt"], role=last.req["role"],
                 route=last.route, extra={"ruling": note}, resume=last)
        return
    _start_attempt(ctx, c, events, attempt=max(1, c.attempt), last_vendor=None,
                   notes=f"## The operator's note\n\nThis card was parked; the operator resumed it with:\n\n{note}")


# -- reviewer and verifier violations ----------------------------------------

def _ref_snapshot(ctx: _Run) -> dict[str, str]:
    """ref -> object for the run branch, its card branches and everything under refs/leadhand/."""
    out = git(ctx.ws, "for-each-ref", "--format=%(objectname) %(refname)", f"refs/heads/{ctx.branch}",
              f"refs/heads/{ctx.branch}--card-*", "refs/leadhand/")
    snap = {}
    for line in out.splitlines():
        sha, _, name = line.partition(" ")
        if name:
            snap[name] = sha
    return snap


def _workspace_kept(ctx: _Run, c: CardState, d: _Dispatch, events: list[dict]) -> bool:
    """True when the reviewer or verifier left the workspace and the run's refs as it found
    them. Otherwise (now or recorded earlier) the change is preserved and undone, the result
    is void, the same kind is requested again on the same route, and False is returned.

    Two phases: `violation{stage: found}` is written before anything is moved, so a crash
    mid-way still voids the result; `violation{stage: restored}` follows the restore."""
    if not d.req.get("expected_head"):
        return True
    mine = [e for e in events if e["kind"] == "violation" and e.get("dispatch_id") == d.id]
    if not mine:
        found = _find_violation(ctx, d, events)
        if found is None:
            return True
        mine = [found]
    found = next(e for e in mine if e.get("stage") == "found")
    if not any(e.get("stage") == "restored" for e in mine):
        saved, siblings = _restore_violation(ctx, found)
        append_event(ctx.paths, "violation", stage="restored", dispatch_id=d.id, card=c.number, n=found["n"],
                     ref=found["ref"], saved=saved, siblings=siblings, preserved=found["preserved"])
    notes = d.req.get("notes") or ""
    if VOIDED_NOTE not in notes:
        notes = (notes.rstrip("\n") + "\n\n" if notes else "") + VOIDED_NOTE + (
            "\n\nAn earlier worker on this check changed the workspace or moved a branch, so its result "
            "was voided. Build and run tests as you need, but never edit or stage tracked files, commit, "
            "check out another branch, or create, move or delete any ref.")
    _request(ctx, c, events, kind=d.kind, attempt=d.req["attempt"], role=d.req["role"], route=d.route,
             extra=d.req.get("extra") or {}, notes=notes)
    return False


def _find_violation(ctx: _Run, d: _Dispatch, events: list[dict]) -> dict | None:
    """Compare the workspace and refs with what the dispatch recorded; record `found` if they differ."""
    req = d.req
    v = detect_violation(ctx.ws, req["expected_head"], req["expected_branch"])
    before, now = req.get("refs") or {}, _ref_snapshot(ctx)
    moved = {name: {"old": before.get(name), "new": now.get(name)}
             for name in sorted(set(before) | set(now)) if before.get(name) != now.get(name)}
    if v is None and not moved:
        return None
    n = 1 + sum(1 for e in events if e["kind"] == "violation" and e.get("stage") == "found")
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
    paths: set[str] = set()
    if v is not None:
        paths.update(v.tracked_changes)
        if v.index_dirty:
            staged = git(ctx.ws, "diff", "--cached", "--name-only", "-z", "--no-renames", "--no-ext-diff", "HEAD", "--")
            paths.update(p for p in staged.split("\0") if p)
    return append_event(ctx.paths, "violation", stage="found", dispatch_id=d.id, card=req["card"],
                        dispatch_kind=d.kind, n=n, ref=ref if v is not None else None, paths=sorted(paths),
                        head_moved=bool(v and v.head_moved), index_dirty=bool(v and v.index_dirty),
                        expected_head=req["expected_head"], expected_branch=req["expected_branch"],
                        refs=moved, preserved=preserved)


def _restore_violation(ctx: _Run, found: dict) -> tuple[str | None, list[str]]:
    """Keep every moved ref's new tip and the workspace on side refs, then put the workspace
    and the refs back. Repeat-safe after a crash at any point. Returns (saved commit, siblings)."""
    ws = ctx.ws
    for name, keep in found["preserved"].items():
        git(ws, "update-ref", "-m", "leadhand: preserve a ref a reviewer moved", keep, found["refs"][name]["new"])
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
            git(ws, "update-ref", "-m", "leadhand: remove a ref a reviewer created", "-d", name, cur)
        else:
            # The moved value is the expected current value: a ref changed meanwhile is not overwritten.
            git(ws, "update-ref", "-m", "leadhand: restore a ref a reviewer moved", name, m["old"], cur or "")
    return saved, siblings


def _end_blocked(ctx: _Run, events: list[dict], reason: str) -> None:
    cards = fold_cards(events, ctx.numbers)
    decision = decide_outcome(cards, removed=frozenset(), open_findings=False, holds=(), gates_current=False,
                              verification_failed=False, cancelled=False, blocked_reason=reason,
                              unreconciled=None)
    append_event(ctx.paths, "outcome", outcome=decision.outcome.value, qualifier=decision.qualifier,
                 reasons=list(decision.reasons))


# --------------------------------------------------------------------------
# record

def record(run_dir: Path, kind: str, **fields: Any) -> dict:
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
    c = _card_state(ctx, events, d.req["card"])
    if c.dispatch_id != dispatch_id:
        raise EngineError(f"dispatch {dispatch_id} is not card {c.number}'s current dispatch")
    deadline = _format_time(_now() + _budget_seconds(d.req))
    return append_event(ctx.paths, "worker_started", dispatch_id=dispatch_id, worker_id=worker_id,
                        card=d.req["card"], deadline=deadline)


def _copy_if_missing(src: Path | None, dest: Path, text: str | None = None) -> bool:
    if dest.exists():
        return True
    if text is None:
        if src is None or not src.is_file():
            return False
        data = src.read_bytes()
    else:
        data = text.encode("utf-8", "surrogateescape")
    _write_bytes_atomic(dest, data)
    return True


def _head(report: str | None, reply: str, codex: dsp.DispatchResult | None, stopped: bool) -> dict:
    """Status from the report's head line, else (stopped -> SPENT), the reply's head, the
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


def _record_result(ctx: _Run, dispatch_id: str, worker_id: str, reply: str, stopped: bool = False) -> dict:
    events = read_events(ctx.paths)
    d = _known_dispatch(events, dispatch_id)
    if d.result is not None:
        return d.result
    if d.started is None:
        raise EngineError(f"dispatch {dispatch_id} has no started worker; record started first")
    if d.started["worker_id"] != worker_id:
        raise EngineError(f"dispatch {dispatch_id} started worker {d.started['worker_id']}, not {worker_id}")
    reply = reply or ""
    crash_point("before_result_record")
    req, n = d.req, d.req["card"]
    folder = ctx.card_dir(n, dispatch_id)
    folder.mkdir(parents=True, exist_ok=True)
    files = {}
    for name, src in (("brief", Path(req["brief"])), ("report", Path(req["report"]))):
        if _copy_if_missing(src, folder / f"{name}.md"):
            files[name] = str(folder / f"{name}.md")
    _copy_if_missing(None, folder / "reply.md", text=reply)
    files["reply"] = str(folder / "reply.md")
    codex = None
    if req["vendor"] == "codex":
        codex = dsp.load_result(ctx.root / "workers" / dispatch_id)
    head_info = _head(_read_text(folder / "report.md"), reply, codex, stopped)
    tip = None
    if d.kind in IMPLEMENTER_KINDS:
        card_branch = _card_state(ctx, events, n).branch or card_branch_name(ctx.branch, n)
        tip = head(ctx.ws, f"refs/heads/{card_branch}")
    crash_point("after_result_files")

    role = req["role"] if req["vendor"] == "claude" else "proxy"
    charge_id = f"{dispatch_id}:claude:{worker_id}"
    charged = usage.charge(ctx.paths, charge_id=charge_id, source="claude", key=worker_id,
                           dispatch_id=dispatch_id, role=role,
                           path_or_thread=usage.claude_subagent_transcript(worker_id, _projects_dir()),
                           possibly_incomplete=stopped)
    crash_point("after_charge_before_attempt")

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
        "cost_estimate": cost, "exit_reason": "stopped" if stopped else "completed",
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
                            charge_ids=[charge_id], **head_info)


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
    return append_event(ctx.paths, "parked", card=c.number, reason=reason.strip())


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
        c = _card_state(ctx, read_events(ctx.paths), card)
        if c.state != "parked":
            raise EngineError(f"card {c.number} is not parked (it is {c.state})")
        return append_event(ctx.paths, "unparked", card=c.number, note=note.strip())


# --------------------------------------------------------------------------
# timer

def timer(run_dir: Path, dispatch_id: str, *, interval: float = TIMER_INTERVAL) -> str:
    """Return "result" once the dispatch has a result, or "deadline" once its deadline passes."""
    paths = run_paths(Path(run_dir).absolute())
    if not paths.run_json.is_file():
        raise EngineError(f"not a run directory (no run.json): {paths.root}")
    while True:
        d = _known_dispatch(read_events(paths), dispatch_id)
        if d.result is not None:
            return "result"
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
                       reply=_stdin_or(args.reply) or "", stopped=args.stopped)
    elif args.kind == "ruling":
        event = record(run_dir, "ruling", card=args.card, text=_stdin_or(args.text))
    elif args.kind == "park":
        event = record(run_dir, "park", card=args.card, reason=args.reason)
    else:
        event = record(run_dir, "diff-check", card=args.card, ok=args.ok, note=args.note)
    _print_json(event)
    return 0


def _cmd_timer(args) -> int:
    what = timer(Path(args.run_dir), args.dispatch)
    print("result recorded" if what == "result" else "deadline passed", flush=True)
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
    p.add_argument("--reply", help="the worker's reply (result); - reads stdin")
    p.add_argument("--stopped", action="store_true", help="the worker was stopped at its deadline (result)")
    p.add_argument("--card", type=int, help="card number (ruling, park, diff-check)")
    p.add_argument("--text", help="the ruling (ruling); - reads stdin")
    p.add_argument("--reason", help="why the card is parked (park)")
    p.add_argument("--ok", choices=("yes", "no"), help="whether the diff is acceptable (diff-check)")
    p.add_argument("--note", default="", help="what the diff check found (diff-check)")
    p.set_defaults(func=_guard(_cmd_record))

    p = sub.add_parser("timer", help="wait until a dispatch has a result or its deadline passes")
    p.add_argument("run_dir")
    p.add_argument("--dispatch", required=True)
    p.set_defaults(func=_guard(_cmd_timer))
