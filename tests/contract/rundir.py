"""A run directory's layout, and a synthetic one for status and report tests.

`RunDir` is tests/runworld.py's RunWorld written from the file formats instead of the leadhand
package: run.json, deck.md, and whatever events, attempts and charges a test appends. No
engine, Git or workers. `env` is the environment to run `status` and `report` in; Codex rate
limits written by `rate_limit` land in `sessions`, which is $CODEX_HOME/sessions there.
"""
from __future__ import annotations

import fcntl
import json
import os
import unittest
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from harness import hermetic_env, read_jsonl, temp_root


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
    """The files of the run directory `root`, named as the CLI names them."""
    root = Path(root)
    return RunPaths(root=root, deck=root / "deck.md", run_json=root / "run.json", events=root / "events.jsonl",
                    status=root / "status.json", attempts=root / "attempts.jsonl",
                    charges=root / "charges.jsonl", state_lock=root / "state.lock", report=root / "report.md",
                    handoff=root / "handoff.md", lock=root / "run.lock", holder=root / "holder.json",
                    pause_flag=root / "pause", cancel_flag=root / "cancel", cards_dir=root / "cards",
                    workers_dir=root / "workers")


def read_records(path: Path) -> list[dict]:
    """A JSON-lines file's complete records; none when the file does not exist yet."""
    return read_jsonl(path) if path.exists() else []


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def append_record(state_lock: Path, file: Path, build) -> dict:
    """Append one record as one line under `state_lock`, as the CLI does; `build` gets the
    file's last record (or None)."""
    fd = os.open(state_lock, os.O_RDWR | os.O_CREAT, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        records = read_records(file)
        record = build(records[-1] if records else None)
        with open(file, "a", encoding="utf-8") as f:
            f.write(json.dumps(record, ensure_ascii=True, separators=(",", ":")) + "\n")
    finally:
        os.close(fd)
    return record


def append_event(run_dir: Path, kind: str, **f) -> dict:
    """Append event `kind` to run directory `run_dir`'s events.jsonl, with the next seq."""
    paths = run_paths(run_dir)
    return append_record(paths.state_lock, paths.events, lambda last: {
        "seq": 1 if last is None else last["seq"] + 1, "ts": utc_now(), "kind": kind, **f})


DECK = """# Deck: Demo deck
Source: docs/plan.md
Base: main @ 0123456789abcdef0123456789abcdef01234567
Branch: leadhand/demo
Build: true
Test: true

"""


def deck_card(n, depends="none"):
    return (f"## Card {n}: Card number {n}\n- Role: implementer\n- Hard: no\n- Risk: low\n"
            f"- Depends on: {depends}\n- Allowed paths: src/\n- Done when: it works\n- Stop if: never\n")


class RunDir:
    def __init__(self, test: unittest.TestCase, cards: int = 5):
        tmp = temp_root(test, "lh status ")
        self.root = tmp / "run dir"
        self.root.mkdir()
        self.env = hermetic_env(test, tmp)
        self.paths = run_paths(self.root)
        self.paths.deck.write_text(DECK + "\n".join(deck_card(n) for n in range(1, cards + 1)),
                                   encoding="utf-8")
        meta = {
            "run_id": "abc123", "repo": str(tmp / "repo"), "deck_source": "deck.md",
            "deck_digest": "x", "base_ref": "main", "base_sha": "0123456789abcdef0123456789abcdef01234567",
            "branch": "leadhand/demo-2", "branch_requested": "leadhand/demo",
            "workspace": str(tmp / "work space"), "slug": "demo-deck",
            "created_at": "2026-01-01T00:00:00.000Z", "supervisor_sessions": []}
        self.paths.run_json.write_text(json.dumps(meta, indent=2, ensure_ascii=True) + "\n", encoding="utf-8")
        self.sessions = Path(self.env["CODEX_HOME"]) / "sessions"
        self.sessions.mkdir(parents=True)
        self.ev("run_started", run_id="abc123", base_sha="0123456789abcdef", branch="leadhand/demo-2")

    def _append(self, file: Path, build) -> dict:
        return append_record(self.paths.state_lock, file, build)

    def ev(self, kind, **f):
        return append_event(self.root, kind, **f)

    def accept(self, n, sha="a" * 40):
        self.ev("card_cut", card=n, branch=f"leadhand/demo-2/c{n}", base="b")
        self.ev("accepted", card=n, sha=sha)

    def dispatch(self, n, kind="implement", attempt=1, vendor="codex", model="gpt-6.1-terra",
                 effort="high", role="implementer", did=None, start=True):
        did = did or f"c{n}-a{attempt}-{kind}-1"
        if not any(e["kind"] == "card_cut" and e["card"] == n for e in read_records(self.paths.events)):
            self.ev("card_cut", card=n, branch=f"leadhand/demo-2/c{n}", base="b")
        self.ev("dispatch_requested", dispatch_id=did, card=n, attempt=attempt, dispatch_kind=kind,
                role=role, vendor=vendor, model=model, effort=effort)
        if start:
            self.ev("worker_started", dispatch_id=did, worker_id="w-" + did)
        return did

    def attempt(self, did, n, role="implementer", vendor="codex", model="gpt-6.1-terra", effort="high",
                attempt=1, status="READY", usage_known=True, **extra):
        self._append(self.paths.attempts, lambda _last: {
            "dispatch": did, "run": "abc123", "card": n, "attempt": attempt, "role": role,
            "vendor": vendor, "model": model, "effort": effort, "status": status,
            "usage_known": usage_known, "cost_estimate": None, **extra})

    def charge(self, cid, role, tokens=(100, 10), known=True, source="claude", model="claude-sonnet-4-5",
               possibly_incomplete=False, dispatch="d1"):
        usage = {"input": tokens[0], "cached": 0, "cache_write_5m": 0, "cache_write_1h": 0,
                 "output": tokens[1], "reasoning": 0, "known": known, "model": model}
        rec = {"charge_id": cid, "source": source, "key": cid, "dispatch_id": dispatch, "role": role,
               "usage": usage, "checkpoint": {}, "ts": "2026-01-01T00:00:00.000Z"}
        if possibly_incomplete:
            rec["possibly_incomplete"] = True
        self._append(self.paths.charges, lambda _last: rec)

    def rate_limit(self, percent, ts="2026-01-02T03:04:05.000Z", name="rollout-2026-01-02-abc.jsonl"):
        write_rate_limit(self.sessions, percent, ts=ts, name=name)


def write_rate_limit(sessions: Path, percent, ts="2026-01-02T03:04:05.000Z",
                     name="rollout-2026-01-02-abc.jsonl") -> Path:
    """Append a Codex rate-limit record (a weekly window at `percent`) to the rollout file `name`
    under `sessions`/2026/01/02; returns the file."""
    d = sessions / "2026" / "01" / "02"
    d.mkdir(parents=True, exist_ok=True)
    line = {"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "info": None,
            "rate_limits": {"primary": {"used_percent": percent, "window_minutes": 10080,
                                        "resets_at": 1767830400}, "secondary": None}}}
    with open(d / name, "a", encoding="utf-8") as f:
        f.write(json.dumps(line) + "\n")
    return d / name
