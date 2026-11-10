import contextlib
import io
import json
import os
import unittest
from unittest import mock

from leadhand import cli, state, status
from leadhand.state import run_paths
from runworld import RunWorld

KEYS = {"run_id", "deck", "state", "headline", "activity", "progress", "attention", "cards", "usage",
        "updated_at"}


class StatusTests(unittest.TestCase):
    def test_status_headline_while_implementing(self):
        w = RunWorld(self, cards=5)
        w.accept(1)
        w.accept(2)
        w.dispatch(3, effort="high")
        s = status.build_status(w.root, sessions_dir=w.sessions)
        self.assertEqual(set(s), KEYS)
        self.assertEqual(s["state"], "running")
        self.assertEqual(s["headline"], "Running · Implementing card 3 (attempt 1, codex gpt-6.1-terra high)"
                                        " · 2 of 5 accepted")
        self.assertEqual(s["progress"], {"accepted": 2, "total": 5})
        self.assertEqual(s["deck"], "Demo deck")
        self.assertEqual(set(s["usage"]), {"estimated_usd", "unknown_attempts", "by_role", "codex_meter"})
        self.assertEqual(set(s["cards"][2]), {"number", "title", "state", "attempt", "route", "note"})

    def test_status_headline_while_reviewing(self):
        w = RunWorld(self, cards=5)
        w.accept(1)
        w.accept(2)
        w.dispatch(3, kind="review", role="reviewer", vendor="claude", model="opus", effort="high")
        self.assertEqual(status.build_status(w.root, sessions_dir=w.sessions)["headline"],
                         "Running · Reviewing card 3 · 2 of 5 accepted")

    def test_status_attention_lists_parked_cards_and_decisions(self):
        w = RunWorld(self, cards=5)
        w.accept(1)
        w.ev("card_cut", card=5, branch="b5", base="b")
        w.ev("parked", card=5, reason="needs a design decision")
        w.dispatch(2)
        s = status.build_status(w.root, sessions_dir=w.sessions)
        self.assertEqual(s["headline"], "Needs your decision on card 5; independent work continues")
        self.assertIn("Card 5 parked: needs a design decision", s["attention"])
        text = status.render_status(s)
        lines = text.splitlines()
        self.assertEqual(lines[0], s["headline"])
        self.assertEqual(lines[1], "Card 5 parked: needs a design decision")
        self.assertTrue(any(l.startswith("Card 5") or l.startswith("5 ") or " 5 " in l for l in lines[2:]))
        self.assertIn("Usage", lines[-1])

    def test_ended_run_states(self):
        w = RunWorld(self, cards=1)
        w.accept(1)
        w.ev("outcome", outcome="complete", qualifier="", reasons=[])
        s = status.build_status(w.root, sessions_dir=w.sessions)
        self.assertEqual((s["state"], s["progress"]), ("done", {"accepted": 1, "total": 1}))
        w2 = RunWorld(self, cards=2)
        w2.ev("outcome", outcome="blocked", qualifier="paused", reasons=["blocked: paused"])
        self.assertEqual(status.build_status(w2.root, sessions_dir=w2.sessions)["state"], "blocked")

    def test_status_cli_without_status_json(self):
        w = RunWorld(self, cards=3)
        w.dispatch(1)
        self.assertFalse(w.paths.status.exists())
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            rc = cli.main(["status", str(w.root)])
        self.assertEqual(rc, 0)
        self.assertTrue(out.getvalue().startswith("Running · Implementing card 1"))

    def test_status_prefers_status_json_when_present(self):
        w = RunWorld(self, cards=1)
        status.write_status(w.root, sessions_dir=w.sessions)
        self.assertTrue(w.paths.status.is_file())
        self.assertEqual(json.loads(w.paths.status.read_text())["run_id"], "abc123")

    def test_corrupt_event_line_is_an_attention_line_not_a_failure(self):
        w = RunWorld(self, cards=2)
        w.accept(1)
        w.ev("card_cut", card=2, branch="b2", base="b")
        lines = w.paths.events.read_bytes().split(b"\n")
        lines.insert(3, b"{not json}")
        w.paths.events.write_bytes(b"\n".join(lines))
        s = status.build_status(w.root, sessions_dir=w.sessions)
        self.assertEqual(s["progress"]["accepted"], 1)
        self.assertIn("event log has an unreadable line at seq 4/line 4", s["attention"])
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(cli.main(["status", str(w.root)]), 0)

    def test_partial_last_line_is_ignored(self):
        w = RunWorld(self, cards=1)
        with open(w.paths.events, "ab") as f:
            f.write(b'{"seq": 9, "kind": "acc')
        s = status.build_status(w.root, sessions_dir=w.sessions)
        self.assertEqual(s["attention"], [])

    def test_usage_comes_from_charges_and_excludes_nothing_by_role(self):
        w = RunWorld(self, cards=1)
        w.charge("c1", "implementer", tokens=(1000, 100))
        w.charge("c2", "proxy", tokens=(10, 1))
        w.charge("c3", "supervisor", known=False)
        d = w.dispatch(1)
        w.attempt(d, 1, usage_known=False)
        s = status.build_status(w.root, sessions_dir=w.sessions)
        self.assertEqual(set(s["usage"]["by_role"]), {"implementer", "proxy", "supervisor"})
        self.assertGreaterEqual(s["usage"]["unknown_attempts"], 1)

    def test_codex_meter_reads_latest_record(self):
        w = RunWorld(self, cards=1)
        self.assertEqual(status.codex_meter(w.sessions), (None, None))
        w.rate_limit(12.5, ts="2026-01-02T03:04:05.000Z")
        w.rate_limit(15.0, ts="2026-01-02T05:00:00.000Z")
        self.assertEqual(status.codex_meter(w.sessions), (15.0, "2026-01-02T05:00:00.000Z"))
        self.assertEqual(status.codex_meter(w.sessions / "missing"), (None, None))

    def test_no_run_dir_picks_latest_unfinished_run(self):
        w = RunWorld(self, cards=1)
        docs = w.root.parent / "docs"
        runs = docs / "runs"
        runs.mkdir(parents=True)
        done = RunWorld(self, cards=1)
        done.ev("outcome", outcome="complete", qualifier="", reasons=[])
        import shutil
        shutil.copytree(done.root, runs / "2026-01-01-done")
        shutil.copytree(w.root, runs / "2026-01-02-live")
        cfg = w.root.parent / "cfg.toml"
        cfg.write_text(f'docs_dir = "{docs}"\n', encoding="utf-8")
        with mock.patch.dict(os.environ, {"LEADHAND_CONFIG": str(cfg)}):
            self.assertEqual(status._latest_unfinished(docs), runs / "2026-01-02-live")
            self.assertIsNone(status._latest_unfinished(docs / "nothing"))


class EngineStatusTests(unittest.TestCase):
    def test_engine_checkpoints_write_status_json_and_survive_status_errors(self):
        from fakeworld import FakeWorld, card
        from leadhand import engine
        w = FakeWorld(self, [card(1)])
        w.start()
        engine.next_action(w.run_dir)
        data = json.loads(run_paths(w.run_dir).status.read_text())
        self.assertEqual(data["progress"], {"accepted": 0, "total": 1})
        err = io.StringIO()
        with mock.patch.object(status, "write_status", side_effect=RuntimeError("boom")), \
                contextlib.redirect_stderr(err):
            action = engine.next_action(w.run_dir)
        self.assertIn("action", action)
        self.assertIn("could not write status.json: boom", err.getvalue())


if __name__ == "__main__":
    unittest.main()
