"""A synthetic run directory for status and report tests: run.json, deck.md, and
whatever events, attempts and charges a test appends. No engine, git or workers."""
from __future__ import annotations

import shutil
import tempfile
import os
import unittest
from unittest import mock
from pathlib import Path

from leadhand import state, usage

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


class RunWorld:
    def __init__(self, test: unittest.TestCase, cards: int = 5):
        self.root = Path(tempfile.mkdtemp(prefix="lh status ")) / "run dir"
        self.root.mkdir()
        test.addCleanup(shutil.rmtree, self.root.parent, True)
        cfg = self.root.parent / "user config.toml"
        cfg.write_text("", encoding="utf-8")
        patcher = mock.patch.dict(os.environ, {"LEADHAND_CONFIG": str(cfg)})
        patcher.start()
        test.addCleanup(patcher.stop)
        self.paths = state.run_paths(self.root)
        self.paths.deck.write_text(DECK + "\n".join(deck_card(n) for n in range(1, cards + 1)),
                                   encoding="utf-8")
        state.write_json_atomic(self.paths.run_json, {
            "run_id": "abc123", "repo": str(self.root.parent / "repo"), "deck_source": "deck.md",
            "deck_digest": "x", "base_ref": "main", "base_sha": "0123456789abcdef0123456789abcdef01234567",
            "branch": "leadhand/demo-2", "branch_requested": "leadhand/demo",
            "workspace": str(self.root.parent / "work space"), "slug": "demo-deck",
            "created_at": "2026-01-01T00:00:00.000Z", "supervisor_sessions": []})
        self.sessions = self.root.parent / "codex sessions"
        self.sessions.mkdir()
        self.ev("run_started", run_id="abc123", base_sha="0123456789abcdef", branch="leadhand/demo-2")

    def ev(self, kind, **f):
        return state.append_event(self.paths, kind, **f)

    def accept(self, n, sha="a" * 40):
        self.ev("card_cut", card=n, branch=f"leadhand/demo-2/c{n}", base="b")
        self.ev("accepted", card=n, sha=sha)

    def dispatch(self, n, kind="implement", attempt=1, vendor="codex", model="gpt-6.1-terra",
                 effort="high", role="implementer", did=None, start=True):
        did = did or f"c{n}-a{attempt}-{kind}-1"
        if not any(e["kind"] == "card_cut" and e["card"] == n for e in state.read_events(self.paths)):
            self.ev("card_cut", card=n, branch=f"leadhand/demo-2/c{n}", base="b")
        self.ev("dispatch_requested", dispatch_id=did, card=n, attempt=attempt, dispatch_kind=kind,
                role=role, vendor=vendor, model=model, effort=effort)
        if start:
            self.ev("worker_started", dispatch_id=did, worker_id="w-" + did)
        return did

    def attempt(self, did, n, role="implementer", vendor="codex", model="gpt-6.1-terra", effort="high",
                attempt=1, status="READY", usage_known=True, **extra):
        state.append_attempt(self.paths, {
            "dispatch": did, "run": "abc123", "card": n, "attempt": attempt, "role": role,
            "vendor": vendor, "model": model, "effort": effort, "status": status,
            "usage_known": usage_known, "cost_estimate": None, **extra})

    def charge(self, cid, role, tokens=(100, 10), known=True, source="claude", model="claude-sonnet-4-5",
               possibly_incomplete=False, dispatch="d1"):
        u = usage.Usage(input=tokens[0], output=tokens[1], known=known, model=model)
        rec = {"charge_id": cid, "source": source, "key": cid, "dispatch_id": dispatch, "role": role,
               "usage": usage._usage_to_dict(u), "checkpoint": {}, "ts": "2026-01-01T00:00:00.000Z"}
        if possibly_incomplete:
            rec["possibly_incomplete"] = True
        state.append_jsonl(self.paths, self.paths.charges, rec)

    def rate_limit(self, percent, ts="2026-01-02T03:04:05.000Z", name="rollout-2026-01-02-abc.jsonl"):
        import json
        d = self.sessions / "2026" / "01" / "02"
        d.mkdir(parents=True, exist_ok=True)
        line = {"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "info": None,
                "rate_limits": {"primary": {"used_percent": percent, "window_minutes": 10080,
                                            "resets_at": 1767830400}, "secondary": None}}}
        with open(d / name, "a", encoding="utf-8") as f:
            f.write(json.dumps(line) + "\n")
