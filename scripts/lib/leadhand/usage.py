"""Usage accounting: charge each worker's token usage exactly once, and estimate cost.

`charges.jsonl` is the source of truth. Each charge is one durable record with a
unique `charge_id`; it holds the usage charged and the checkpoint reached (Claude:
the message IDs newly charged; Codex: the cumulative total reached). The whole
`charge` sequence (does the id exist? derive the checkpoint from earlier charges,
read the transcript or session file, compute only what is new, append) runs under
the run's state lock, so a repeated charge is harmless, a crash between a charge
and its attempt record loses nothing, and concurrent writers never double-count.

Source file formats
-------------------
The field paths below are PROVISIONAL: the runtime probes that confirm them have
not run yet. Every path lives in one constant here, so a probe finding is a
one-line change.

Claude transcripts (`~/.claude/projects/<project>/<session>/subagents/agent-<id>.jsonl`
and `~/.claude/projects/<project>/<session>.jsonl`): each content block of one
assistant message is its own line, all with the same `message.id` and the same
input-side usage but growing `output_tokens`. Per message ID the record with the
largest `output_tokens` is kept. A message with no ID cannot be deduplicated or
checkpointed and is skipped. When `cache_creation` is absent,
`cache_creation_input_tokens` counts as 5-minute writes.

Codex rollout (`~/.codex/sessions/YYYY/MM/DD/rollout-*<thread_id>.jsonl`): the last
`token_count` event with a non-null `total_token_usage` holds the cumulative
totals for the session. In those totals `input_tokens` INCLUDES `cached_input_tokens`
(OpenAI convention), so uncached input = input_tokens - cached_input_tokens
(`CODEX_INPUT_INCLUDES_CACHED`); `output_tokens` is kept as reported and
`reasoning_output_tokens` is carried separately for information only (it is a
subset of the output on the same convention, and is never priced on its own). The
`--json` stream's `turn.completed.usage` uses the same field names and mapping and
is summed over turns. A session file with no usable `token_count` is UNKNOWN, not
zero: leadhand cannot tell "used nothing" from "format not understood".
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

CODEX_ROLLOUT_EVENT_TYPE = ("payload", "type")
CODEX_ROLLOUT_EVENT_TOKEN_COUNT = "token_count"
CODEX_ROLLOUT_TOTAL_USAGE = ("payload", "info", "total_token_usage")
CODEX_STREAM_EVENT_TYPE = ("type",)
CODEX_STREAM_EVENT_TURN_COMPLETED = "turn.completed"
CODEX_STREAM_USAGE = ("usage",)
CODEX_INPUT = "input_tokens"
CODEX_CACHED = "cached_input_tokens"
CODEX_OUTPUT = "output_tokens"
CODEX_REASONING = "reasoning_output_tokens"
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

def _json_lines(path: Path, needle: str | None = None) -> Iterable[Any]:
    """Parsed JSON values, one per line; unparseable lines (a partial write at the
    end of a file still being written, or foreign noise) are skipped. A line that
    does not contain `needle` is skipped without parsing."""
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            if needle is not None and needle not in line:
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


def claude_usage(path: Path, *, exclude_ids: frozenset[str]) -> tuple[Usage, frozenset[str]]:
    """Sum the usage of the transcript's assistant messages, once per message ID.

    The record with the largest `output_tokens` represents a message (ties keep
    the first). IDs in `exclude_ids` are skipped. Returns the total and the IDs
    it counted (not the excluded ones), which are the new checkpoint entries.
    """
    best: dict[str, Usage] = {}
    for rec in _json_lines(Path(path), needle=CLAUDE_RECORD_TYPE_ASSISTANT):
        if not isinstance(rec, dict):
            continue
        parsed = _claude_record_usage(rec)
        if parsed is None:
            continue
        u, mid = parsed
        if mid in exclude_ids:
            continue
        if mid not in best or u.output > best[mid].output:
            best[mid] = u
    total = Usage()
    for u in best.values():
        total = add(total, u)
    return total, frozenset(best)


def _codex_usage_from(d: Any) -> Usage | None:
    if not isinstance(d, dict):
        return None
    inp, cached = _int(d.get(CODEX_INPUT)), _int(d.get(CODEX_CACHED))
    if CODEX_INPUT_INCLUDES_CACHED:
        inp = max(0, inp - cached)
    return Usage(input=inp, cached=cached, output=_int(d.get(CODEX_OUTPUT)),
                 reasoning=_int(d.get(CODEX_REASONING)))


def codex_cumulative(thread_id: str, sessions_dir: Path | None = None) -> Usage:
    """Cumulative usage of a Codex session: its last `token_count` event with a
    non-null total. UNKNOWN when the file is missing, unreadable or has none."""
    path = codex_session_file(thread_id, sessions_dir)
    if path is None:
        return UNKNOWN
    last: Usage | None = None
    try:
        for rec in _json_lines(path, needle=CODEX_ROLLOUT_EVENT_TOKEN_COUNT):
            if _dig(rec, CODEX_ROLLOUT_EVENT_TYPE) != CODEX_ROLLOUT_EVENT_TOKEN_COUNT:
                continue
            u = _codex_usage_from(_dig(rec, CODEX_ROLLOUT_TOTAL_USAGE))
            if u is not None:
                last = u
    except OSError:
        return UNKNOWN
    return last if last is not None else UNKNOWN


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


def _charge_to_record(c: Charge) -> dict:
    return {"charge_id": c.charge_id, "source": c.source, "key": c.key,
            "dispatch_id": c.dispatch_id, "role": c.role,
            "usage": _usage_to_dict(c.usage), "checkpoint": c.checkpoint, "ts": c.ts}


def _charge_from_record(r: dict) -> Charge:
    return Charge(charge_id=r["charge_id"], source=r["source"], key=r["key"],
                  dispatch_id=r["dispatch_id"], role=r["role"],
                  usage=_usage_from_dict(r["usage"]), checkpoint=r.get("checkpoint") or {},
                  ts=r["ts"])


def read_charges(paths: RunPaths) -> list[Charge]:
    return [_charge_from_record(r) for r in _read_jsonl(paths, paths.charges)]


def charged_checkpoint(charges: Iterable[Charge], source: str, key: str) -> dict:
    """What earlier charges for (source, key) already cover. Claude:
    `{"message_ids": [sorted union]}`. Codex: `{"cumulative": {field: max}}`
    (empty when nothing was charged)."""
    mine = [c for c in charges if c.source == source and c.key == key]
    if source == "claude":
        ids: set[str] = set()
        for c in mine:
            ids.update(c.checkpoint.get("message_ids") or ())
        return {"message_ids": sorted(ids)}
    if source == "codex":
        cum: dict[str, int] = {}
        for c in mine:
            for f, v in (c.checkpoint.get("cumulative") or {}).items():
                cum[f] = max(cum.get(f, 0), _int(v))
        return {"cumulative": cum}
    raise ValueError(f"unknown usage source: {source!r}")


def charge(paths: RunPaths, *, charge_id: str, source: str, key: str, dispatch_id: str,
           role: str, path_or_thread: Path | str, sessions_dir: Path | None = None) -> Charge:
    """Charge what is new in a transcript (source "claude", `path_or_thread` a
    path) or Codex session (source "codex", `path_or_thread` a thread ID, found
    under `sessions_dir`) exactly once. Runs entirely under the state lock.

    A `charge_id` that already exists is returned unchanged. A missing or
    unreadable file appends an UNKNOWN charge whose checkpoint is the one
    already reached, so nothing is lost when the file appears later.
    """
    if source not in SOURCES:
        raise ValueError(f"unknown usage source: {source!r}")
    if not charge_id:
        raise ValueError("charge_id is required")
    with state_lock(paths):
        earlier = read_charges(paths)
        for c in earlier:
            if c.charge_id == charge_id:
                return c
        prior = charged_checkpoint(earlier, source, key)
        if source == "claude":
            used, checkpoint = _new_claude(Path(path_or_thread), prior)
        else:
            used, checkpoint = _new_codex(str(path_or_thread), sessions_dir, prior)
        record = Charge(charge_id=charge_id, source=source, key=key, dispatch_id=dispatch_id,
                        role=role, usage=used, checkpoint=checkpoint, ts=_utc_now())
        append_jsonl(paths, paths.charges, _charge_to_record(record))
        return record


def _new_claude(path: Path, prior: dict) -> tuple[Usage, dict]:
    charged = frozenset(prior["message_ids"])
    try:
        used, new_ids = claude_usage(path, exclude_ids=charged)
    except OSError:
        return UNKNOWN, {"message_ids": []}
    return used, {"message_ids": sorted(new_ids)}


def _new_codex(thread: str, sessions_dir: Path | None, prior: dict) -> tuple[Usage, dict]:
    reached = prior["cumulative"]
    cumulative = codex_cumulative(thread, sessions_dir)
    if not cumulative.known:
        return UNKNOWN, {"cumulative": dict(reached)}
    delta = subtract(cumulative, Usage(**reached))
    now = _tokens_dict(cumulative)
    return delta, {"cumulative": {f: max(now[f], reached.get(f, 0)) for f in _TOKEN_FIELDS}}


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
