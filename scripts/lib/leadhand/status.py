"""`leadhand status`: what the operator sees while a run is going, built from the
event log (the source of truth) and written to `status.json` at each checkpoint.

Reading never fails on a live or crashed run: a corrupt line in a log is skipped and
reported as an attention line, and a missing file is an empty log.
"""
from __future__ import annotations

import json
import shlex
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from leadhand import gitops
from leadhand import usage as usage_mod
from leadhand.config import ConfigError, docs_dir_for, load_config, load_run_config
from leadhand.dispatch import cli_path
from leadhand.deck import DeckError, parse_deck
from leadhand.state import (CardState, RunPaths, fold_cards, read_attempts, read_events, run_paths,
                            utc_now, write_json_atomic, _read_bytes)

METER_START = "meter-start.json"
METER_SCAN_FILES = 20
IMPLEMENTER_VERBS = {"implement": "Implementing", "continue": "Continuing", "fix": "Fixing",
                     "answer": "Answering a question on", "integrate": "Integrating",
                     "conflict": "Resolving conflicts on"}
CHECK_VERBS = {"review": "Reviewing", "rereview": "Re-reviewing", "verify": "Verifying"}
STATE_VERBS = {"cut": "Starting", "integrating": "Integrating", "accepting": "Accepting",
               "reviewed": "Processing the review of", "fixing": "Fixing", "reviewing": "Reviewing",
               "requested": "Starting", "started": "Working on", "reported": "Processing the result of"}
IDLE_STATES = ("pending", "accepted", "parked")


# --------------------------------------------------------------------------
# Tolerant reading

def _tolerant_lines(file: Path, label: str, problems: list[str]) -> list[dict]:
    """Every parseable complete line; each unreadable one adds a problem naming where it is."""
    try:
        raw = _read_bytes(file)
    except FileNotFoundError:
        return []
    except OSError as e:
        problems.append(f"{label} could not be read: {e}")
        return []
    out: list[dict] = []
    last_seq = 0
    for number, line in enumerate(raw.split(b"\n")[:-1], 1):
        if not line.strip():
            continue
        try:
            value = json.loads(line.decode("utf-8"))
            if not isinstance(value, dict):
                raise ValueError("not an object")
        except (UnicodeDecodeError, ValueError):
            problems.append(f"{label} has an unreadable line at seq {last_seq + 1}/line {number}"
                            if label == "event log" else f"{label} has an unreadable line {number}")
            continue
        if isinstance(value.get("seq"), int):
            last_seq = value["seq"]
        out.append(value)
    return out


def _read(reader: Callable[[RunPaths], list[dict]], paths: RunPaths, file: Path, label: str,
          problems: list[str]) -> list[dict]:
    try:
        return reader(paths)
    except ValueError:
        return _tolerant_lines(file, label, problems)
    except FileNotFoundError:
        return []
    except OSError as e:
        problems.append(f"{label} could not be read: {e}")
        return []


@dataclass
class Snapshot:
    """Everything the run directory holds, read once and tolerantly."""
    root: Path
    paths: RunPaths
    meta: dict
    deck: Any                      # Deck | None
    title: str
    numbers: list[int]
    titles: dict[int, str]
    events: list[dict]
    attempts: list[dict]
    charges: list[usage_mod.Charge]
    cards: dict[int, CardState]
    problems: list[str] = field(default_factory=list)

    @property
    def outcome(self) -> dict | None:
        last = None
        for e in self.events:
            if e.get("kind") == "outcome":
                last = e
            elif e.get("kind") == "resumed":
                last = None
        return last


def load(run_dir: Path) -> Snapshot:
    root = Path(run_dir)
    paths = run_paths(root)
    problems: list[str] = []
    try:
        meta = json.loads(paths.run_json.read_text(encoding="utf-8"))
        if not isinstance(meta, dict):
            raise ValueError("not an object")
    except (OSError, ValueError):
        meta = {}
        problems.append("run.json is missing or unreadable")
    events = _read(read_events, paths, paths.events, "event log", problems)
    attempts = _read(read_attempts, paths, paths.attempts, "attempt log", problems)
    try:
        charges = usage_mod.read_charges(paths)
    except (ValueError, KeyError, TypeError, OSError):
        raw = _tolerant_lines(paths.charges, "charge log", problems)
        charges = []
        for r in raw:
            try:
                charges.append(usage_mod._charge_from_record(r))
            except (KeyError, TypeError, ValueError):
                problems.append("charge log has an unreadable record")
    try:
        deck = parse_deck(paths.deck)
        title, numbers = deck.title, [c.number for c in deck.cards]
        titles = {c.number: c.title for c in deck.cards}
    except (DeckError, OSError, ValueError) as e:
        deck, title, titles = None, root.name, {}
        numbers = sorted({int(e_["card"]) for e_ in events
                          if isinstance(e_.get("card"), int) and not isinstance(e_.get("card"), bool)})
        problems.append(f"deck is missing or unreadable: {e}")
    cards = fold_cards(events, numbers)
    return Snapshot(root, paths, meta, deck, title, numbers, titles, events, attempts, charges, cards,
                    problems)


# --------------------------------------------------------------------------
# The Codex meter

def codex_meter(sessions_dir: Path | None = None) -> tuple[float | None, str | None]:
    """(used percent, observation time) from the latest rate-limit record in the Codex rollout
    files under `sessions_dir`, preferring the weekly window; `(None, None)` when there is none."""
    sessions_dir = Path(sessions_dir) if sessions_dir is not None else usage_mod.default_sessions_dir()
    try:
        files = sorted(sessions_dir.glob("**/rollout-*.jsonl"), key=lambda p: p.stat().st_mtime,
                       reverse=True)
    except OSError:
        return None, None
    best: tuple[str, float] | None = None
    for f in files[:METER_SCAN_FILES]:
        try:
            text = f.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for line in text.splitlines():
            if "rate_limits" not in line:
                continue
            try:
                rec = json.loads(line)
                limits = rec["payload"]["rate_limits"]
                windows = [w for w in (limits.get("primary"), limits.get("secondary"))
                           if isinstance(w, dict) and isinstance(w.get("used_percent"), (int, float))]
                weekly = [w for w in windows if w.get("window_minutes") == 10080]
                window = (weekly or windows or [None])[0]
                if window is None:
                    continue
                stamp = str(rec.get("timestamp") or "")
            except (ValueError, KeyError, TypeError, AttributeError):
                continue
            if best is None or stamp >= best[0]:
                best = (stamp, float(window["used_percent"]))
        if best is not None:
            break
    return (best[1], best[0] or None) if best else (None, None)


def read_meter_start(root: Path) -> tuple[float | None, str | None]:
    try:
        d = json.loads((Path(root) / METER_START).read_text(encoding="utf-8"))
        return d.get("percent"), d.get("observed_at")
    except (OSError, ValueError, AttributeError):
        return None, None


# --------------------------------------------------------------------------
# Usage

def vendor_of(source: str) -> str:
    return "claude" if source == "claude" else "codex"


def _prices(snap: Snapshot) -> dict:
    try:
        return dict(load_run_config(snap.root, Path(snap.meta["repo"])).prices)
    except (ConfigError, KeyError, OSError, ValueError, TypeError):
        return {}


def usage_summary(snap: Snapshot, sessions_dir: Path | None = None, meter: tuple | None = None) -> dict:
    """Totals from the charge log (the source of truth), with cost per charge so models mix correctly."""
    prices = _prices(snap)
    by_role: dict[str, dict] = {}
    by_vendor: dict[str, dict] = {}
    total = 0.0
    priced_any = False
    incomplete: set[str] = set()
    for c in snap.charges:
        cost = (usage_mod.estimate_cost(c.usage, c.usage.model, prices)
                if c.usage.known and (c.usage.model or c.usage.by_model) else None)
        for table, key in ((by_role, c.role), (by_vendor, vendor_of(c.source))):
            row = table.setdefault(key, {"input": 0, "output": 0, "cached": 0, "estimated_usd": None,
                                         "unknown_charges": 0})
            row["input"] += c.usage.input
            row["output"] += c.usage.output
            row["cached"] += c.usage.cached
            if not c.usage.known:
                row["unknown_charges"] += 1
            if cost is not None:
                row["estimated_usd"] = (row["estimated_usd"] or 0.0) + cost
        if cost is not None:
            total += cost
            priced_any = True
        if c.possibly_incomplete:
            incomplete.add(c.dispatch_id)
    unknown = {(a.get("dispatch"), a.get("role")) for a in snap.attempts if a.get("usage_known") is False}
    unknown |= {(c.dispatch_id, c.role) for c in snap.charges if not c.usage.known}
    percent, observed = codex_meter(sessions_dir) if meter is None else meter
    return {"estimated_usd": round(total, 6) if priced_any else None,
            "unknown_attempts": len(unknown), "by_role": by_role, "by_vendor": by_vendor,
            "undercounted_attempts": len(incomplete),
            "codex_meter": {"percent": percent, "observed_at": observed}}


# --------------------------------------------------------------------------
# Status

def violation_lines(events: list[dict]) -> list[str]:
    """One attention line per recorded violation, with the ref that holds the preserved work."""
    out = []
    for e in events:
        if e.get("kind") != "violation" or e.get("stage") != "found":
            continue
        who = e.get("dispatch_kind") or e.get("by") or "worker"
        card = f" on card {e['card']}" if e.get("card") is not None else ""
        refs = [r for r in [e.get("ref"), *(e.get("preserved") or {}).values()] if r]
        where = refs[0] if refs else "no ref recorded"
        out.append(f"Violation by a {who} worker{card} (dispatch {e.get('dispatch_id') or e.get('after_dispatch') or 'unknown'}): "
                   f"its changes were preserved at {where} — next: inspect {where}")
    return out


def _requests(events: list[dict]) -> dict[str, dict]:
    out: dict[str, dict] = {}
    for e in events:
        if e.get("kind") == "dispatch_requested" and e.get("dispatch_id") not in out:
            out[e.get("dispatch_id")] = e
    return out


def route_text(req: dict | None) -> str:
    if not req:
        return ""
    return " ".join(str(req[k]) for k in ("vendor", "model", "effort") if req.get(k))


def _activity(c: CardState, req: dict | None) -> str:
    kind = c.dispatch_kind if c.state in ("requested", "started", "reported", "reviewing", "fixing") else None
    if kind in CHECK_VERBS:
        return f"{CHECK_VERBS[kind]} card {c.number}"
    if kind in IMPLEMENTER_VERBS:
        route = route_text(req)
        return (f"{IMPLEMENTER_VERBS[kind]} card {c.number} (attempt {c.attempt}"
                + (f", {route})" if route else ")"))
    return f"{STATE_VERBS.get(c.state, 'Working on')} card {c.number}"


def _steering_headline(events: list[dict], progress_text: str) -> str | None:
    """While the run has no outcome: a cancel or pause in progress, or, after a relaunch,
    what reconciliation found (`Resumed at card 2; card 2's commits were found and recorded`)
    until the next worker starts."""
    if any(e.get("kind") == "cancelled" for e in events):
        return f"Cancelling · stopping every worker · {progress_text}"
    since = max((e.get("seq", 0) for e in events if e.get("kind") == "resumed"), default=0)
    if any(e.get("kind") == "paused" and e.get("seq", 0) > since for e in events):
        return f"Pausing · running workers finish, nothing new starts · {progress_text}"
    summaries = [e for e in events if e.get("kind") == "reconciled" and "found" in e
                 and e.get("resumed_seq") == since and since]
    if not summaries:
        return None
    rec = summaries[-1]
    if any(e.get("kind") == "worker_started" and e.get("seq", 0) > rec.get("seq", 0) and not e.get("reconciled")
           for e in events):
        return None
    return str(rec.get("headline") or "") or None


def _number_list(numbers: list[int]) -> str:
    if len(numbers) == 1:
        return f"card {numbers[0]}"
    return "cards " + ", ".join(map(str, numbers[:-1])) + f" and {numbers[-1]}"


def build_status(run_dir: Path, sessions_dir: Path | None = None, meter: tuple | None = None) -> dict:
    snap = load(run_dir)
    requests = _requests(snap.events)
    cards = snap.cards
    accepted = sum(1 for c in cards.values() if c.state == "accepted")
    total = len(snap.numbers)
    parked = [n for n in sorted(cards) if cards[n].state == "parked"]
    active = [n for n in sorted(cards) if cards[n].state not in IDLE_STATES]
    outcome = snap.outcome

    # `unpark` is a next step only while the run can continue: unfinished, or ended blocked.
    actionable = outcome is None or outcome.get("outcome") == "blocked"
    attention = [f"Card {n} parked: {cards[n].park_reason or 'no reason recorded'}"
                 + (f" — next: {shlex.quote(str(cli_path()))} unpark {shlex.quote(str(snap.root.absolute()))}"
                    f" --card {n} --note '…'" if actionable else "") for n in parked]
    attention.extend(violation_lines(snap.events))
    state_name = "running"
    activity = ""
    progress_text = f"{accepted} of {total} accepted"
    if outcome is not None:
        kind = str(outcome.get("outcome") or "")
        qualifier = outcome.get("qualifier") or ""
        state_name = {"complete": "done", "incomplete": "done", "blocked": "blocked",
                      "failed": "failed"}.get(kind, "done")
        label = kind.capitalize() + (f" ({qualifier})" if qualifier else "")
        headline = f"{label} · {progress_text}"
        for reason in outcome.get("reasons") or []:
            if not (isinstance(reason, str) and reason.startswith("card ") and " parked" in reason):
                attention.append(str(reason))
    elif parked and not active:
        headline = f"Needs your decision on {_number_list(parked)}"
    elif parked:
        headline = f"Needs your decision on {_number_list(parked)}; independent work continues"
        activity = _activity(cards[active[0]], requests.get(cards[active[0]].dispatch_id))
    elif active:
        c = cards[active[0]]
        activity = _activity(c, requests.get(c.dispatch_id))
        headline = f"Running · {activity} · {progress_text}"
    else:
        activity = "Waiting for the supervisor"
        headline = f"Running · {activity} · {progress_text}"
    if outcome is None:
        steering = _steering_headline(snap.events, progress_text)
        if steering:
            headline = steering
    attention.extend(snap.problems)

    card_rows = []
    for n in sorted(cards):
        c = cards[n]
        note = c.park_reason or ""
        if not note and c.state == "fixing":
            note = f"fix round {c.fix_rounds}"
        card_rows.append({"number": n, "title": snap.titles.get(n, ""), "state": c.state,
                          "attempt": c.attempt, "route": route_text(requests.get(c.dispatch_id)),
                          "note": note})
    summary = usage_summary(snap, sessions_dir, meter)
    return {"run_id": snap.meta.get("run_id", ""), "deck": snap.title, "state": state_name,
            "headline": headline, "activity": activity,
            "progress": {"accepted": accepted, "total": total}, "attention": attention,
            "cards": card_rows,
            "usage": {"estimated_usd": summary["estimated_usd"],
                      "unknown_attempts": summary["unknown_attempts"], "by_role": summary["by_role"],
                      "codex_meter": summary["codex_meter"]},
            "updated_at": utc_now()}


def write_status(run_dir: Path, sessions_dir: Path | None = None) -> dict:
    """Rewrite status.json atomically. The Codex meter is read on the first write (kept in
    meter-start.json as the run's 'before', even when there is no reading) and once the run has
    ended; other checkpoints carry the last reading forward instead of scanning rollout files."""
    root = Path(run_dir)
    first = not (root / METER_START).exists()
    ended = load(root).outcome is not None
    meter = None
    if not (first or ended):
        try:
            previous = json.loads((root / "status.json").read_text(encoding="utf-8"))["usage"]["codex_meter"]
            meter = (previous.get("percent"), previous.get("observed_at"))
        except (OSError, ValueError, KeyError, TypeError, AttributeError):
            meter = None
    s = build_status(root, sessions_dir, meter)
    if first:
        write_json_atomic(root / METER_START, s["usage"]["codex_meter"])
    write_json_atomic(root / "status.json", s)
    return s


def render_status(s: dict) -> str:
    lines = [s["headline"]]
    lines.extend(s["attention"])
    for c in s["cards"]:
        parts = [f"Card {c['number']}: {c['title']}", c["state"]]
        if c["attempt"]:
            parts.append(f"attempt {c['attempt']}")
        if c["route"]:
            parts.append(c["route"])
        if c["note"]:
            parts.append(c["note"])
        lines.append(" · ".join(parts))
    u = s["usage"]
    cost = "unpriced" if u["estimated_usd"] is None else f"${u['estimated_usd']:.2f}"
    m = u["codex_meter"]
    meter = "unknown" if m["percent"] is None else f"{m['percent']:.1f}% as of {m['observed_at']}"
    lines.append(f"Usage: estimated {cost} API-equivalent (an estimate, not subscription use) · "
                 f"{u['unknown_attempts']} attempts with unknown usage · Codex meter {meter} · "
                 f"Claude meter unknown")
    return "\n".join(lines)


# --------------------------------------------------------------------------
# Command

def _unfinished(root: Path) -> bool:
    try:
        events = read_events(run_paths(root))
    except (ValueError, OSError):
        return True
    last = None
    for e in events:
        if e.get("kind") == "outcome":
            last = e
        elif e.get("kind") == "resumed":
            last = None
    return last is None or last.get("outcome") == "blocked"


def _latest_unfinished(docs: Path) -> Path | None:
    """The unfinished run (no outcome, or blocked) whose event log changed last."""
    best: tuple[float, str, Path] | None = None
    for rj in (Path(docs) / "runs").glob("*/run.json"):
        root = rj.parent
        if not _unfinished(root):
            continue
        try:
            stamp = max(p.stat().st_mtime for p in (run_paths(root).events, rj) if p.exists())
        except (OSError, ValueError):
            continue
        if best is None or (stamp, str(root)) > best[:2]:
            best = (stamp, str(root), root)
    return best[2] if best else None


def _repo_root() -> Path:
    """The operator's main checkout, even from one of its worktrees."""
    try:
        r = gitops._run(Path.cwd(), ("rev-parse", "--path-format=absolute", "--git-common-dir"), timeout=30)
        common = r.stdout.strip() if r.returncode == 0 else ""
        if common:
            return Path(common).parent
    except (OSError, gitops.GitError):
        pass
    return Path.cwd()


def _cmd_status(args) -> int:
    if args.run_dir:
        root = Path(args.run_dir)
    else:
        try:
            docs = docs_dir_for(load_config(_repo_root()), _repo_root())
        except (ConfigError, OSError) as e:
            print(f"error: {e}", file=sys.stderr)
            return 1
        root = _latest_unfinished(docs)
        if root is None:
            print(f"error: no unfinished run under {docs / 'runs'}", file=sys.stderr)
            return 1
    if not run_paths(root).run_json.is_file() and not run_paths(root).events.is_file():
        print(f"error: not a run directory: {root}", file=sys.stderr)
        return 1
    try:
        s = build_status(root)
    except Exception as e:  # reading must not fail on a live or crashed run: fall back to the file
        try:
            s = json.loads(run_paths(root).status.read_text(encoding="utf-8"))
            s["attention"] = [*s.get("attention", []), f"status rebuilt from status.json: {e}"]
        except (OSError, ValueError):
            print(f"error: cannot read the run: {e}", file=sys.stderr)
            return 1
    print(render_status(s))
    return 0


def add_commands(sub) -> None:
    p = sub.add_parser("status", help="show a run's headline, attention items, cards and usage")
    p.add_argument("run_dir", nargs="?",
                   help="the run directory; default: the most recently updated unfinished run "
                        "under this repository's docs_dir")
    p.set_defaults(func=_cmd_status)
