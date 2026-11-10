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
                             fast_forward, git, head, is_test_or_doc_path, prepare_workspace)
from leadhand.route import LadderExhausted, PinnedVendorMissing, Resolution, resolve
from leadhand.state import (CardState, RunPaths, utc_now, append_attempt, append_event, crash_point,
                            decide_outcome, fold_cards, new_run_dir, read_attempts, read_events, run_paths,
                            state_lock, write_json_atomic)

ACTIONS = ("dispatch", "wait", "check_diff", "answer", "stop_worker", "done")
RECORD_KINDS = ("started", "result", "ruling", "park", "diff-check")
IMPLEMENTER_KINDS = ("implement", "fix", "answer", "continue")
REVIEW_KINDS = ("review", "rereview")
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
    root, existing = _start(Path(deck_path), Path(repo), config, new)
    if existing:
        print(f"existing run: {root}")
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


def _check_branch_free(docs: Path, repo: Path, branch: str) -> None:
    """A new run never takes over another run's branch (R26): its card branches, worktree and
    scratch files would be mistaken for the new run's."""
    for root, meta in _runs(docs):
        if meta.get("branch") == branch and meta.get("repo") == str(repo):
            raise EngineError(f"run branch {branch} already belongs to the run in {root}; "
                              "give the deck a different Branch: to start a new run")


def _check_worktree_free(docs: Path, root: Path, ws: Path) -> None:
    owner = next((r for r, meta in _runs(docs) if meta.get("workspace") == str(ws) and r != root), None)
    if owner is not None or ws.exists():
        root.rmdir()
        raise EngineError(f"worktree {ws} already exists"
                          + (f" and belongs to the run in {owner}" if owner else "")
                          + "; remove it or start the run again")


def _start(deck_path: Path, repo: Path, config: Config | None, new: bool) -> tuple[Path, bool]:
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
            return existing, True

    _check_branch_free(docs, repo, deck.branch)
    slug = _slug(deck.title)
    root = new_run_dir(docs, slug, date.today())
    # The worktree is named after the run directory, which is unique, so no two runs share it.
    if config.run.worktree:
        _check_worktree_free(docs, root, repo / ".worktrees" / root.name)
    paths = run_paths(root)
    _write_bytes_atomic(paths.deck, raw)
    if deck.base_sha:
        base_ref, base_sha = deck.base_ref, head(repo, deck.base_sha)
    else:
        base_ref, base_sha = default_base(repo)
    ws = prepare_workspace(repo, run_branch=deck.branch, base_sha=base_sha, slug=root.name,
                           use_worktree=config.run.worktree)
    meta = {
        "run_id": secrets.token_hex(6), "repo": str(repo), "deck_source": str(deck_path),
        "deck_digest": deck.digest, "base_ref": base_ref, "base_sha": base_sha, "branch": deck.branch,
        "workspace": str(Path(ws).absolute()), "slug": slug, "created_at": utc_now(),
        "supervisor_sessions": [],
    }
    write_json_atomic(paths.run_json, meta)
    append_event(paths, "run_started", run_id=meta["run_id"], base_sha=base_sha, branch=deck.branch)
    return root, False


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
        _start_attempt(ctx, c, events, attempt=max(1, c.attempt), last_vendor=None, notes="")
        return None
    if c.state == "accepting":
        _finish_accept(ctx, n, c.accept_target)
        return None
    if c.state == "integrating":
        raise EngineError(f"card {n}: integration steps are not supported in this version")
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
    full = {"base": c.base or "", "branch": c.branch or "", "test_first": ctx.config.run.test_first, **extra}
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
                        resume_session=resume_session, minutes=minutes)


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
              review: _Dispatch | None = None) -> None:
    """A fresh worker on the next rung of the ladder, with the previous report in its brief."""
    impl = _card_dispatches(index, c.number, kinds=IMPLEMENTER_KINDS)
    last = max(impl, key=lambda d: d.req["seq"])
    attempt = last.req["attempt"]
    lines = ["## Previous attempt", "",
             f"Attempt {attempt} on this card {why}. Read what it reported before you start; the card "
             "branch holds its commits. Keep what is sound and fix or replace the rest.", ""]
    for name, d in (("Its report", last), ("The review findings", review)):
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
             extra={"diff_range": f"{c.base}..{tip}", "implementer_report": _report_copy(impl)})


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
    if d.kind in REVIEW_KINDS:
        if r.get("verdict") in ("clean", "defects"):
            append_event(ctx.paths, "verdict_recorded", card=c.number, verdict=r["verdict"],
                         findings=_report_copy(d), dispatch_id=d.id)
            return None
        return _worker_failed(ctx, c, d, events, index)
    status = r["status"]
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
    counts only results since the latest `resumed`."""
    r = d.result
    status = r["status"]
    if not any(e["kind"] == "outcome" and e["seq"] > r["seq"] for e in events):
        if status == "BLOCKED" and r.get("blocked_by") == "usage limit":
            _end_blocked(ctx, events, "provider capacity")
            return None
        since = max((e["seq"] for e in events if e["kind"] == "resumed"), default=0)
        same = [x for x in _card_dispatches(index, c.number, attempt=d.req["attempt"], kinds=(d.kind,))
                if x.result is not None and x.result["seq"] > since]
        if status == "BLOCKED":
            failures = [x for x in same if x.result["status"] == "BLOCKED"
                        and x.result.get("blocked_by") != "usage limit"]
            if len(failures) >= 2:
                _end_blocked(ctx, events, "environment")
                return None
        else:
            failures = [x for x in same if x.result["status"] != "BLOCKED" and x.result.get("verdict") is None]
            if len(failures) >= 2:
                append_event(ctx.paths, "parked", card=c.number,
                             reason=f"{d.kind} ended {status} twice without a verdict")
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
                 extra={"diff_range": f"{c.base}..{tip}", "findings": fix["findings"],
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
    """Accept a card: fast-forward the run branch to its reviewed tip (intent, move, completion)."""
    if not target:
        raise EngineError(f"card {card}: no tip to accept")
    append_event(ctx.paths, "accept_started", card=card, target=target)
    crash_point("before_fast_forward")
    _finish_accept(ctx, card, target)


def _finish_accept(ctx: _Run, card: int, target: str) -> None:
    """Idempotent: a run branch that already contains the target is left where it is."""
    fast_forward(ctx.ws, ctx.branch, target)
    crash_point("after_fast_forward")
    append_event(ctx.paths, "accepted", card=card, sha=target)


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
    root, existing = _start(Path(args.deck), Path(args.repo), None, args.new)
    print(f"existing run: {root}" if existing else f"run: {root}")
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
