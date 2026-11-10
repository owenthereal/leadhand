"""Usage accounting: charge each worker's token usage exactly once, and estimate cost.

`charges.jsonl` is the source of truth. Each charge is one durable record with a
unique `charge_id`; it holds the usage charged and the checkpoint reached (Claude:
the output charged so far for each message ID touched; Codex: the response IDs
newly charged). The whole `charge` sequence (does the id exist? derive the
checkpoint from earlier charges, read the transcript or session file, compute
only what is new, append) runs under the run's state lock, so a repeated charge
is harmless, a crash between a charge and its attempt record loses nothing, and
concurrent writers never double-count.

Source file formats (confirmed by the runtime probes; each path is one constant
below, so a change in a later CLI release is a one-line fix)
------------------------------------------------------------------------------
Claude transcripts (`~/.claude/projects/<project>/<session>/subagents/agent-<id>.jsonl`
and `~/.claude/projects/<project>/<session>.jsonl`): each content block of one
assistant message is its own line, all with the same `message.id`. In Claude Code
2.1.322 the usage on those repeats is identical; the per-ID checkpoint also tops
up a message that grew after it was charged, as a guard that is not expected to
fire. Per message ID the record with the largest `output_tokens` is kept. A
message with no ID cannot be deduplicated or checkpointed and is skipped. When
`cache_creation` is absent, `cache_creation_input_tokens` counts as 5-minute
writes. The main transcript holds no worker records, so charging a session and
its workers does not double-count.

Codex rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<thread_id>.jsonl`,
append-only across resumes): the source is the top-level `token_usage_record`
events written after every model response. `payload.usage` is that response's
usage; the sum over distinct `payload.response_id` (a record repeated after a
crash counts once) is the thread's usage, and each response is charged once. The
last `payload.thread_token_usage` is a cross-check: a mismatch with the sum is
recorded on the charge (`mismatch`) and never fails it. The `token_count` event's
`total_token_usage` omits a turn killed before its `token_count` was written, so
it is a fallback only, used when the file holds no `token_usage_record`; its
checkpoint is the largest cumulative total reached and the charge is the floored
difference. A file with neither is UNKNOWN, not zero: leadhand cannot tell "used
nothing" from "format not understood". A response in flight when a worker is
killed leaves no record anywhere, so a killed attempt can be undercounted: the
caller passes `possibly_incomplete=True` for a stopped attempt and the charge
carries it.

Codex field mapping (rollout records, `token_count` totals and the `--json`
stream's `turn.completed.usage` share it). `input_tokens` INCLUDES
`cached_input_tokens` (evidence: the record's `total_tokens` equals
`input_tokens + output_tokens`), so input = input_tokens - cached_input_tokens
(`CODEX_INPUT_INCLUDES_CACHED`). `output_tokens` is kept as reported;
`reasoning_output_tokens` is carried separately for information (it is counted
inside the output, never priced on its own). `cache_write_input_tokens` has no
TTL, so it is kept in `cache_write_5m`; it was 0 in every observed record, so
whether it is also inside `input_tokens` is not established and it is not
subtracted from it. Codex models are unpriced, so none of this affects cost.
"""
from __future__ import annotations

import glob
import json
import os
from dataclasses import asdict, dataclass, fields
from pathlib import Path
from typing import Any, Iterable, Mapping

from .state import RunPaths, _read_jsonl, _utc_now, append_jsonl, state_lock

# --------------------------------------------------------------------------
# Provisional field paths (one constant each; see the module docstring)

CLAUDE_RECORD_TYPE = ("type",)
CLAUDE_RECORD_TYPE_ASSISTANT = "assistant"
CLAUDE_MESSAGE_ID = ("message", "id")
CLAUDE_MESSAGE_MODEL = ("message", "model")
CLAUDE_MESSAGE_USAGE = ("message", "usage")
CLAUDE_INPUT = "input_tokens"
CLAUDE_CACHE_READ = "cache_read_input_tokens"
CLAUDE_OUTPUT = "output_tokens"
CLAUDE_CACHE_CREATION = "cache_creation"                  # dict, per-TTL breakdown
CLAUDE_CACHE_WRITE_5M = "ephemeral_5m_input_tokens"       # inside CLAUDE_CACHE_CREATION
CLAUDE_CACHE_WRITE_1H = "ephemeral_1h_input_tokens"       # inside CLAUDE_CACHE_CREATION
CLAUDE_CACHE_CREATION_TOTAL = "cache_creation_input_tokens"  # fallback: all 5-minute writes

CODEX_RECORD_TYPE = ("type",)
CODEX_RECORD_TYPE_USAGE = "token_usage_record"
CODEX_RECORD_RESPONSE_ID = ("payload", "response_id")
CODEX_RECORD_USAGE = ("payload", "usage")                     # this response only
CODEX_RECORD_THREAD_USAGE = ("payload", "thread_token_usage")  # whole thread, cross-check
CODEX_ROLLOUT_EVENT_TYPE = ("payload", "type")                 # fallback: token_count events
CODEX_ROLLOUT_EVENT_TOKEN_COUNT = "token_count"
CODEX_ROLLOUT_TOTAL_USAGE = ("payload", "info", "total_token_usage")
CODEX_STREAM_EVENT_TYPE = ("type",)
CODEX_STREAM_EVENT_TURN_COMPLETED = "turn.completed"
CODEX_STREAM_USAGE = ("usage",)
CODEX_INPUT = "input_tokens"
CODEX_CACHED = "cached_input_tokens"
CODEX_OUTPUT = "output_tokens"
CODEX_REASONING = "reasoning_output_tokens"
CODEX_CACHE_WRITE = "cache_write_input_tokens"
CODEX_INPUT_INCLUDES_CACHED = True

PRICE_FAMILIES = ("opus", "sonnet", "haiku", "fable")
SOURCES = ("claude", "codex")


def _dig(obj: Any, path: tuple[str, ...]) -> Any:
    for part in path:
        if not isinstance(obj, dict):
            return None
        obj = obj.get(part)
    return obj


def _int(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0
    return max(0, int(value))


# --------------------------------------------------------------------------
# Usage

@dataclass(frozen=True)
class Usage:
    input: int = 0
    cached: int = 0
    cache_write_5m: int = 0
    cache_write_1h: int = 0
    output: int = 0
    reasoning: int = 0
    known: bool = True
    model: str | None = None


UNKNOWN = Usage(known=False)

_TOKEN_FIELDS = ("input", "cached", "cache_write_5m", "cache_write_1h", "output", "reasoning")


def add(a: Usage, b: Usage) -> Usage:
    """Field-wise sum. The result is known only if both are: a total that includes
    an unknown part must not pass for a complete one. Differing models give None."""
    if a.model == b.model or b.model is None:
        model = a.model
    elif a.model is None:
        model = b.model
    else:
        model = None
    return Usage(**{f: getattr(a, f) + getattr(b, f) for f in _TOKEN_FIELDS},
                 known=a.known and b.known, model=model)


def subtract(a: Usage, b: Usage) -> Usage:
    """Field-wise a - b, floored at 0. Keeps a's `known` and model."""
    return Usage(**{f: max(0, getattr(a, f) - getattr(b, f)) for f in _TOKEN_FIELDS},
                 known=a.known, model=a.model)


def _usage_to_dict(u: Usage) -> dict:
    return asdict(u)


def _usage_from_dict(d: Mapping[str, Any]) -> Usage:
    names = {f.name for f in fields(Usage)}
    return Usage(**{k: v for k, v in d.items() if k in names})


def _tokens_dict(u: Usage) -> dict:
    return {f: getattr(u, f) for f in _TOKEN_FIELDS}


# --------------------------------------------------------------------------
# Locating source files

def default_projects_dir() -> Path:
    return Path.home() / ".claude" / "projects"


def default_sessions_dir() -> Path:
    home = os.environ.get("CODEX_HOME")
    return (Path(home) if home else Path.home() / ".codex") / "sessions"


def _newest(pattern: str) -> Path | None:
    matches = [Path(p) for p in glob.glob(pattern)]
    if not matches:
        return None

    def mtime(p: Path) -> float:
        try:
            return p.stat().st_mtime
        except OSError:
            return 0.0
    return max(matches, key=lambda p: (mtime(p), str(p)))


def _safe_id(value: str) -> bool:
    return bool(value) and "/" not in value and os.sep not in value and "\0" not in value


def claude_subagent_transcript(agent_id: str, projects_dir: Path | None = None) -> Path | None:
    """`<projects_dir>/*/*/subagents/agent-<id>.jsonl`, or None."""
    if not _safe_id(agent_id):
        return None
    base = glob.escape(str(projects_dir if projects_dir is not None else default_projects_dir()))
    return _newest(os.path.join(base, "*", "*", "subagents", f"agent-{glob.escape(agent_id)}.jsonl"))


def claude_session_transcript(session_id: str, projects_dir: Path | None = None) -> Path | None:
    """`<projects_dir>/*/<session_id>.jsonl`, or None."""
    if not _safe_id(session_id):
        return None
    base = glob.escape(str(projects_dir if projects_dir is not None else default_projects_dir()))
    return _newest(os.path.join(base, "*", f"{glob.escape(session_id)}.jsonl"))


def codex_session_file(thread_id: str, sessions_dir: Path | None = None) -> Path | None:
    """`<sessions_dir>/YYYY/MM/DD/*<thread_id>.jsonl`, or None."""
    if not _safe_id(thread_id):
        return None
    base = glob.escape(str(sessions_dir if sessions_dir is not None else default_sessions_dir()))
    return _newest(os.path.join(base, "*", "*", "*", f"*{glob.escape(thread_id)}.jsonl"))


# --------------------------------------------------------------------------
# Reading usage from a source file

def _json_lines(path: Path, needle: str | tuple[str, ...] | None = None) -> Iterable[Any]:
    """Parsed JSON values, one per line; unparseable lines (a partial write at the
    end of a file still being written, or foreign noise) are skipped. A line that
    contains none of `needle` (a string or tuple of strings) is skipped without parsing."""
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            if needle is not None and not any(n in line for n in
                                              ((needle,) if isinstance(needle, str) else needle)):
                continue
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except ValueError:
                continue


def _claude_record_usage(rec: dict) -> tuple[Usage, str] | None:
    if _dig(rec, CLAUDE_RECORD_TYPE) != CLAUDE_RECORD_TYPE_ASSISTANT:
        return None
    mid = _dig(rec, CLAUDE_MESSAGE_ID)
    u = _dig(rec, CLAUDE_MESSAGE_USAGE)
    if not isinstance(mid, str) or not mid or not isinstance(u, dict):
        return None
    creation = u.get(CLAUDE_CACHE_CREATION)
    if isinstance(creation, dict):
        w5 = _int(creation.get(CLAUDE_CACHE_WRITE_5M))
        w1 = _int(creation.get(CLAUDE_CACHE_WRITE_1H))
    else:
        w5, w1 = _int(u.get(CLAUDE_CACHE_CREATION_TOTAL)), 0
    model = _dig(rec, CLAUDE_MESSAGE_MODEL)
    return Usage(input=_int(u.get(CLAUDE_INPUT)), cached=_int(u.get(CLAUDE_CACHE_READ)),
                 cache_write_5m=w5, cache_write_1h=w1, output=_int(u.get(CLAUDE_OUTPUT)),
                 model=model if isinstance(model, str) else None), mid


def _claude_messages(path: Path) -> dict[str, Usage]:
    """The best record per message ID: the one with the largest `output_tokens`
    (ties keep the first). Unparseable and ID-less lines are skipped."""
    best: dict[str, Usage] = {}
    for rec in _json_lines(Path(path), needle=CLAUDE_RECORD_TYPE_ASSISTANT):
        if not isinstance(rec, dict):
            continue
        parsed = _claude_record_usage(rec)
        if parsed is None:
            continue
        u, mid = parsed
        if mid not in best or u.output > best[mid].output:
            best[mid] = u
    return best


def claude_usage(path: Path, *, exclude_ids: frozenset[str]) -> tuple[Usage, frozenset[str]]:
    """Sum the usage of the transcript's assistant messages, once per message ID.

    The record with the largest `output_tokens` represents a message. IDs in
    `exclude_ids` are skipped. Returns the total and the IDs it counted (not the
    excluded ones).
    """
    total, counted = Usage(), set()
    for mid, u in _claude_messages(path).items():
        if mid not in exclude_ids:
            total = add(total, u)
            counted.add(mid)
    return total, frozenset(counted)


def _codex_usage_from(d: Any) -> Usage | None:
    if not isinstance(d, dict):
        return None
    inp, cached = _int(d.get(CODEX_INPUT)), _int(d.get(CODEX_CACHED))
    if CODEX_INPUT_INCLUDES_CACHED:
        inp = max(0, inp - cached)
    return Usage(input=inp, cached=cached, cache_write_5m=_int(d.get(CODEX_CACHE_WRITE)),
                 output=_int(d.get(CODEX_OUTPUT)), reasoning=_int(d.get(CODEX_REASONING)))


@dataclass(frozen=True)
class _Rollout:
    responses: dict[str, Usage]          # distinct response_id -> that response's usage
    thread_total: Usage | None           # the last record's thread_token_usage
    token_count_total: Usage | None      # the last token_count total (fallback only)


def _codex_rollout(path: Path) -> _Rollout:
    responses: dict[str, Usage] = {}
    thread_total = token_count_total = None
    for rec in _json_lines(Path(path), needle=(CODEX_RECORD_TYPE_USAGE, CODEX_ROLLOUT_EVENT_TOKEN_COUNT)):
        if not isinstance(rec, dict):
            continue
        if _dig(rec, CODEX_RECORD_TYPE) == CODEX_RECORD_TYPE_USAGE:
            rid, u = _dig(rec, CODEX_RECORD_RESPONSE_ID), _codex_usage_from(_dig(rec, CODEX_RECORD_USAGE))
            if isinstance(rid, str) and rid and u is not None and rid not in responses:
                responses[rid] = u
            t = _codex_usage_from(_dig(rec, CODEX_RECORD_THREAD_USAGE))
            if t is not None:
                thread_total = t
        elif _dig(rec, CODEX_ROLLOUT_EVENT_TYPE) == CODEX_ROLLOUT_EVENT_TOKEN_COUNT:
            u = _codex_usage_from(_dig(rec, CODEX_ROLLOUT_TOTAL_USAGE))
            if u is not None:
                token_count_total = u
    return _Rollout(responses, thread_total, token_count_total)


def _sum(usages: Iterable[Usage]) -> Usage:
    total = Usage()
    for u in usages:
        total = add(total, u)
    return total


def codex_cumulative(thread_id: str, sessions_dir: Path | None = None) -> Usage:
    """Total usage of a Codex session: the sum over its distinct `token_usage_record`
    responses, else (no such record) the last `token_count` total. UNKNOWN when the
    file is missing, unreadable or has neither."""
    path = codex_session_file(thread_id, sessions_dir)
    if path is None:
        return UNKNOWN
    try:
        rollout = _codex_rollout(path)
    except OSError:
        return UNKNOWN
    if rollout.responses:
        return _sum(rollout.responses.values())
    return rollout.token_count_total if rollout.token_count_total is not None else UNKNOWN


def codex_usage_from_stream(events: Iterable[dict]) -> Usage:
    """Sum `turn.completed` usage over a `codex exec --json` stream. UNKNOWN when
    no turn completed (the worker was interrupted: recover from the session file)."""
    total, seen = Usage(), False
    for e in events:
        if _dig(e, CODEX_STREAM_EVENT_TYPE) != CODEX_STREAM_EVENT_TURN_COMPLETED:
            continue
        u = _codex_usage_from(_dig(e, CODEX_STREAM_USAGE))
        if u is not None:
            total, seen = add(total, u), True
    return total if seen else UNKNOWN


# --------------------------------------------------------------------------
# Charges

@dataclass(frozen=True)
class Charge:
    charge_id: str
    source: str
    key: str
    dispatch_id: str
    role: str
    usage: Usage
    checkpoint: dict
    ts: str
    possibly_incomplete: bool = False   # the attempt was stopped; an in-flight response is lost
    mismatch: dict | None = None        # Codex: records disagree with thread_token_usage


def _charge_to_record(c: Charge) -> dict:
    return {"charge_id": c.charge_id, "source": c.source, "key": c.key,
            "dispatch_id": c.dispatch_id, "role": c.role,
            "usage": _usage_to_dict(c.usage), "checkpoint": c.checkpoint, "ts": c.ts,
            **({"possibly_incomplete": True} if c.possibly_incomplete else {}),
            **({"mismatch": c.mismatch} if c.mismatch else {})}


def _charge_from_record(r: dict) -> Charge:
    return Charge(charge_id=r["charge_id"], source=r["source"], key=r["key"],
                  dispatch_id=r["dispatch_id"], role=r["role"],
                  usage=_usage_from_dict(r["usage"]), checkpoint=r.get("checkpoint") or {},
                  ts=r["ts"], possibly_incomplete=bool(r.get("possibly_incomplete")),
                  mismatch=r.get("mismatch") or None)


def read_charges(paths: RunPaths) -> list[Charge]:
    return [_charge_from_record(r) for r in _read_jsonl(paths, paths.charges)]


def charged_checkpoint(charges: Iterable[Charge], source: str, key: str) -> dict:
    """What earlier charges for (source, key) already cover.

    Claude: `{"messages": {id: output_tokens_charged}}`, the max per ID over all
    charges (a message charged mid-stream is topped up if it grows).
    Codex: `{"responses": [sorted response IDs], "cumulative": {field: max}}`;
    `cumulative` is the fallback (token_count) checkpoint and is empty for a
    thread charged from token_usage_record events.
    """
    mine = [c for c in charges if c.source == source and c.key == key]
    if source == "claude":
        messages: dict[str, int] = {}
        for c in mine:
            for mid, n in (c.checkpoint.get("messages") or {}).items():
                messages[mid] = max(messages.get(mid, 0), _int(n))
        return {"messages": messages}
    if source == "codex":
        ids: set[str] = set()
        cum: dict[str, int] = {}
        for c in mine:
            ids.update(c.checkpoint.get("responses") or ())
            for f, v in (c.checkpoint.get("cumulative") or {}).items():
                cum[f] = max(cum.get(f, 0), _int(v))
        return {"responses": sorted(ids), "cumulative": cum}
    raise ValueError(f"unknown usage source: {source!r}")


def charge(paths: RunPaths, *, charge_id: str, source: str, key: str, dispatch_id: str,
           role: str, path_or_thread: Path | str | None,
           sessions_dir: Path | None = None, possibly_incomplete: bool = False) -> Charge:
    """Charge what is new in a transcript (source "claude", `path_or_thread` an
    absolute path) or Codex session (source "codex", `path_or_thread` a thread
    ID, found under `sessions_dir`) exactly once. Runs entirely under the state
    lock.

    A `charge_id` that already exists is returned unchanged. A missing or
    unreadable file, or a `path_or_thread` of None (no transcript known), appends
    an UNKNOWN charge whose checkpoint is the one already reached, so nothing is
    lost when the file appears later. A relative Claude path is a ValueError.
    `possibly_incomplete` marks the charge of an attempt that was stopped.
    """
    if source not in SOURCES:
        raise ValueError(f"unknown usage source: {source!r}")
    if not charge_id:
        raise ValueError("charge_id is required")
    if source == "claude" and path_or_thread is not None and not Path(path_or_thread).is_absolute():
        raise ValueError(f"transcript path must be absolute: {path_or_thread!s}")
    with state_lock(paths):
        earlier = read_charges(paths)
        for c in earlier:
            if c.charge_id == charge_id:
                return c
        prior = charged_checkpoint(earlier, source, key)
        mismatch = None
        if source == "claude":
            used, checkpoint = _new_claude(
                None if path_or_thread is None else Path(path_or_thread), prior)
        else:
            used, checkpoint, mismatch = _new_codex(
                None if path_or_thread is None else str(path_or_thread), sessions_dir, prior)
        record = Charge(charge_id=charge_id, source=source, key=key, dispatch_id=dispatch_id,
                        role=role, usage=used, checkpoint=checkpoint, ts=_utc_now(),
                        possibly_incomplete=possibly_incomplete, mismatch=mismatch)
        append_jsonl(paths, paths.charges, _charge_to_record(record))
        return record


def _new_claude(path: Path | None, prior: dict) -> tuple[Usage, dict]:
    """New messages are charged in full; an already-charged message that has grown
    is charged only its extra output (its input side was charged with its first
    record). The checkpoint lists the new or grown IDs at their output so far."""
    if path is None:
        return UNKNOWN, {"messages": {}}
    charged = prior["messages"]
    try:
        messages = _claude_messages(path)
    except OSError:
        return UNKNOWN, {"messages": {}}
    used, reached = Usage(), {}
    for mid, u in messages.items():
        if mid not in charged:
            used, reached[mid] = add(used, u), u.output
        elif u.output > charged[mid]:
            used = add(used, Usage(output=u.output - charged[mid], model=u.model))
            reached[mid] = u.output
    return used, {"messages": reached}


def _new_codex(thread: str | None, sessions_dir: Path | None,
               prior: dict) -> tuple[Usage, dict, dict | None]:
    """Charge the responses not yet charged. A file with no `token_usage_record`
    falls back to the `token_count` total, charged as its floored difference from
    the largest total reached. Returns (usage, checkpoint, mismatch)."""
    nothing = {"responses": [], "cumulative": {}}
    path = None if thread is None else codex_session_file(thread, sessions_dir)
    if path is None:
        return UNKNOWN, nothing, None
    try:
        rollout = _codex_rollout(path)
    except OSError:
        return UNKNOWN, nothing, None
    if rollout.responses:
        charged = set(prior["responses"])
        new = {rid: u for rid, u in rollout.responses.items() if rid not in charged}
        mismatch = None
        if rollout.thread_total is not None:
            records, thread = _tokens_dict(_sum(rollout.responses.values())), _tokens_dict(rollout.thread_total)
            if records != thread:
                mismatch = {"records": records, "thread_token_usage": thread}
        return _sum(new.values()), {"responses": sorted(new), "cumulative": {}}, mismatch
    if rollout.token_count_total is not None:
        now = _tokens_dict(rollout.token_count_total)
        delta = subtract(rollout.token_count_total, Usage(**prior["cumulative"]))
        return delta, {"responses": [], "cumulative": now}, None
    return UNKNOWN, nothing, None


_GROUPS = {"role": "role", "dispatch": "dispatch_id", "dispatch_id": "dispatch_id",
           "source": "source", "key": "key"}


def totals(charges: Iterable[Charge], *, by: str = "role") -> dict[str, Usage]:
    """Sum charged usage per group (`role`, `dispatch_id`, `source` or `key`). A
    group containing any unknown charge is itself not known."""
    attr = _GROUPS.get(by)
    if attr is None:
        raise ValueError(f"cannot group charges by {by!r}")
    out: dict[str, Usage] = {}
    for c in charges:
        g = getattr(c, attr)
        out[g] = add(out.get(g, Usage()), c.usage)
    return out


def unknown_count(charges: Iterable[Charge]) -> int:
    """How many charges have unknown usage, so run totals can say so."""
    return sum(1 for c in charges if not c.usage.known)


# --------------------------------------------------------------------------
# Cost

def price_family(model: str | None) -> str | None:
    """`claude-opus-*` -> "opus", and so on, by substring. Unknown models (all of
    Codex's) -> None."""
    low = (model or "").lower()
    return next((f for f in PRICE_FAMILIES if f in low), None)


def estimate_cost(usage: Usage, model: str, prices: Mapping[str, Mapping[str, float]]) -> float | None:
    """Estimated API-equivalent cost in dollars: sum of tokens x $/MTok. None for
    unknown usage or an unpriced model. Reasoning tokens are not priced separately."""
    if not usage.known:
        return None
    family = price_family(model)
    p = prices.get(family) if family else None
    if p is None:
        return None
    return (usage.input * p["input"] + usage.output * p["output"]
            + usage.cached * p["cache_read"]
            + usage.cache_write_5m * p["cache_write_5m"]
            + usage.cache_write_1h * p["cache_write_1h"]) / 1_000_000
