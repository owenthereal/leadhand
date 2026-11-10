import fcntl
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from datetime import date
from pathlib import Path
from unittest import mock

from leadhand import state
from leadhand.config import AfterRun
from leadhand.state import CardState, Outcome

ROOT = Path(__file__).resolve().parent.parent
LIB = str(ROOT / "scripts" / "lib")


def env_with_lib(**extra):
    env = dict(os.environ)
    env["PYTHONPATH"] = LIB
    env.update(extra)
    return env


class RunDirCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name) / "run with spaces"
        self.root.mkdir()
        self.paths = state.run_paths(self.root)


class RunPathsTests(RunDirCase):
    def test_run_paths_names(self):
        p = self.paths
        self.assertEqual(p.deck, self.root / "deck.md")
        self.assertEqual(p.run_json, self.root / "run.json")
        self.assertEqual(p.events, self.root / "events.jsonl")
        self.assertEqual(p.status, self.root / "status.json")
        self.assertEqual(p.attempts, self.root / "attempts.jsonl")
        self.assertEqual(p.charges, self.root / "charges.jsonl")
        self.assertEqual(p.state_lock, self.root / "state.lock")
        self.assertEqual(p.report, self.root / "report.md")
        self.assertEqual(p.handoff, self.root / "handoff.md")
        self.assertEqual(p.lock, self.root / "run.lock")
        self.assertEqual(p.holder, self.root / "holder.json")
        self.assertEqual(p.pause_flag, self.root / "pause")
        self.assertEqual(p.cancel_flag, self.root / "cancel")
        self.assertEqual(p.cards_dir, self.root / "cards")
        self.assertEqual(p.workers_dir, self.root / "workers")

    def test_new_run_dir_suffixes_existing(self):
        docs = Path(self._tmp.name) / "docs dir"
        today = date(2026, 11, 10)
        first = state.new_run_dir(docs, "retry", today)
        second = state.new_run_dir(docs, "retry", today)
        third = state.new_run_dir(docs, "retry", today)
        self.assertEqual(first, docs / "runs" / "2026-11-10-retry")
        self.assertEqual(second, docs / "runs" / "2026-11-10-retry-2")
        self.assertEqual(third, docs / "runs" / "2026-11-10-retry-3")
        for d in (first, second, third):
            self.assertTrue(d.is_dir())

    def test_new_run_dir_rejects_path_in_slug(self):
        for bad in ("", "a/b", "..", ".hidden"):
            with self.assertRaises(ValueError):
                state.new_run_dir(Path(self._tmp.name), bad, date(2026, 11, 10))


class LockTests(RunDirCase):
    def _try_flock(self):
        fd = os.open(self.paths.state_lock, os.O_RDWR | os.O_CREAT)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(fd, fcntl.LOCK_UN)
            return True
        except BlockingIOError:
            return False
        finally:
            os.close(fd)

    def test_state_lock_is_reentrant_within_a_process(self):
        with state.state_lock(self.paths):
            with state.state_lock(self.paths):
                self.assertFalse(self._try_flock())
            # leaving the inner block must not release the lock
            self.assertFalse(self._try_flock())
        self.assertTrue(self._try_flock())

    def test_state_lock_released_after_exception(self):
        with self.assertRaises(RuntimeError):
            with state.state_lock(self.paths):
                raise RuntimeError("boom")
        self.assertTrue(self._try_flock())

    def test_state_lock_is_not_the_run_lock(self):
        with state.state_lock(self.paths):
            self.assertNotEqual(self.paths.state_lock, self.paths.lock)
            self.assertFalse(self.paths.lock.exists())


class AppendTests(RunDirCase):
    def test_append_event_assigns_seq_and_fsyncs(self):
        real = os.fsync
        calls = []

        def counting(fd):
            calls.append(fd)
            return real(fd)

        with mock.patch("os.fsync", counting):
            first = state.append_event(self.paths, "run_started", run_id="r1")
            after_first = len(calls)
            second = state.append_event(self.paths, "card_cut", card=1, branch="b", base="x")
            after_second = len(calls)
        self.assertGreaterEqual(after_first, 1)
        self.assertGreaterEqual(after_second - after_first, 1)
        self.assertEqual((first["seq"], second["seq"]), (1, 2))
        self.assertEqual(first["kind"], "run_started")
        self.assertRegex(first["ts"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z$")
        self.assertEqual(state.read_events(self.paths), [first, second])

    def test_append_attempt_and_read_attempts(self):
        state.append_attempt(self.paths, {"card": 1, "attempt": 1})
        state.append_attempt(self.paths, {"card": 1, "attempt": 2})
        self.assertEqual(state.read_attempts(self.paths),
                         [{"card": 1, "attempt": 1}, {"card": 1, "attempt": 2}])

    def test_read_missing_files_are_empty(self):
        self.assertEqual(state.read_events(self.paths), [])
        self.assertEqual(state.read_attempts(self.paths), [])

    def test_read_events_ignores_partial_trailing_line(self):
        state.append_event(self.paths, "run_started")
        with open(self.paths.events, "ab") as f:
            f.write(b'{"seq": 2, "kind": "card_cu')
        events = state.read_events(self.paths)
        self.assertEqual([e["seq"] for e in events], [1])

    def test_read_events_ignores_complete_json_without_newline(self):
        state.append_event(self.paths, "run_started")
        with open(self.paths.events, "ab") as f:
            f.write(b'{"seq": 2, "kind": "paused"}')
        self.assertEqual([e["seq"] for e in state.read_events(self.paths)], [1])

    def test_read_attempts_ignores_partial_trailing_line(self):
        state.append_attempt(self.paths, {"a": 1})
        with open(self.paths.attempts, "ab") as f:
            f.write(b'{"a": ')
        self.assertEqual(state.read_attempts(self.paths), [{"a": 1}])

    def test_read_events_raises_on_corrupt_complete_line(self):
        with open(self.paths.events, "wb") as f:
            f.write(b'{"seq": 1, "kind": "run_started"}\nnot json\n')
        with self.assertRaises(ValueError):
            state.read_events(self.paths)

    def test_append_after_truncated_final_record_stays_valid(self):
        state.append_event(self.paths, "run_started")
        state.append_event(self.paths, "paused")
        with open(self.paths.events, "ab") as f:
            f.write(b'{"seq": 3, "ts": "2026-11-10T00:00:00Z", "kind": "res')
        record = state.append_event(self.paths, "resumed")
        self.assertEqual(record["seq"], 3)
        raw = self.paths.events.read_bytes()
        self.assertTrue(raw.endswith(b"\n"))
        lines = raw.split(b"\n")[:-1]
        self.assertEqual(len(lines), 3)
        parsed = [json.loads(line) for line in lines]
        self.assertEqual([e["seq"] for e in parsed], [1, 2, 3])
        self.assertEqual(parsed[2]["kind"], "resumed")
        self.assertNotIn(b'"res"', raw)

    def test_append_when_whole_file_is_one_partial_line(self):
        with open(self.paths.events, "wb") as f:
            f.write(b'{"seq": 1, "ki')
        record = state.append_event(self.paths, "run_started")
        self.assertEqual(record["seq"], 1)
        self.assertEqual(len(state.read_events(self.paths)), 1)
        self.assertEqual(self.paths.events.read_bytes().count(b"\n"), 1)

    def test_append_jsonl_repairs_partial_line_in_any_file(self):
        with open(self.paths.charges, "wb") as f:
            f.write(b'{"n": 1}\n{"n": 2')
        state.append_jsonl(self.paths, self.paths.charges, {"n": 3})
        lines = self.paths.charges.read_bytes().split(b"\n")
        self.assertEqual(lines[-1], b"")
        self.assertEqual([json.loads(x)["n"] for x in lines[:-1]], [1, 3])

    def test_failed_serialisation_leaves_file_intact(self):
        state.append_event(self.paths, "run_started")
        before = self.paths.events.read_bytes()
        with self.assertRaises(TypeError):
            state.append_event(self.paths, "ruling", bad=object())
        self.assertEqual(self.paths.events.read_bytes(), before)

    def test_unknown_event_kind_rejected(self):
        with self.assertRaises(ValueError):
            state.append_event(self.paths, "not_a_kind")
        self.assertFalse(self.paths.events.exists())

    def test_reserved_field_names_rejected(self):
        for field in ("seq", "ts", "kind"):
            with self.assertRaises(ValueError):
                state.append_event(self.paths, "ruling", **{field: 1})

    def test_concurrent_appends_from_threads(self):
        def work():
            for _ in range(50):
                state.append_event(self.paths, "ruling", who="t")

        threads = [threading.Thread(target=work) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=60)
        self.assertEqual(sorted(e["seq"] for e in state.read_events(self.paths)),
                         list(range(1, 201)))

    def test_concurrent_appends_from_two_processes_keep_whole_lines_and_unique_seq(self):
        code = (
            "import sys\n"
            "from pathlib import Path\n"
            "from leadhand import state\n"
            "paths = state.run_paths(Path(sys.argv[1]))\n"
            "for i in range(200):\n"
            "    state.append_event(paths, 'ruling', who=sys.argv[2], i=i, pad='x' * 300)\n"
        )
        procs = [
            subprocess.Popen([sys.executable, "-B", "-c", code, str(self.root), name],
                             env=env_with_lib(), stderr=subprocess.PIPE)
            for name in ("a", "b")
        ]
        for proc in procs:
            _, err = proc.communicate(timeout=120)
            self.assertEqual(proc.returncode, 0, err.decode())
        raw = self.paths.events.read_bytes()
        self.assertTrue(raw.endswith(b"\n"))
        lines = raw.split(b"\n")[:-1]
        self.assertEqual(len(lines), 400)
        parsed = [json.loads(line) for line in lines]
        self.assertEqual([e["seq"] for e in parsed], list(range(1, 401)))
        for who in ("a", "b"):
            self.assertEqual([e["i"] for e in parsed if e["who"] == who], list(range(200)))


class WriteJsonAtomicTests(RunDirCase):
    def test_write_json_atomic_leaves_no_temp_files(self):
        target = self.root / "status.json"
        state.write_json_atomic(target, {"a": 1})
        state.write_json_atomic(target, {"a": 2, "b": [1, 2]})
        self.assertEqual(json.loads(target.read_text()), {"a": 2, "b": [1, 2]})
        self.assertEqual(sorted(p.name for p in self.root.iterdir()), ["status.json"])

    def test_write_json_atomic_error_keeps_old_content_and_no_temp(self):
        target = self.root / "status.json"
        state.write_json_atomic(target, {"ok": True})
        with self.assertRaises(TypeError):
            state.write_json_atomic(target, {"bad": object()})
        self.assertEqual(json.loads(target.read_text()), {"ok": True})
        self.assertEqual(sorted(p.name for p in self.root.iterdir()), ["status.json"])

    def test_write_json_atomic_replace_failure_cleans_up(self):
        target = self.root / "status.json"
        with mock.patch("os.replace", side_effect=OSError("nope")):
            with self.assertRaises(OSError):
                state.write_json_atomic(target, {"a": 1})
        self.assertEqual(list(self.root.iterdir()), [])

    def test_write_json_atomic_fsyncs(self):
        real = os.fsync
        calls = []
        with mock.patch("os.fsync", lambda fd: (calls.append(fd), real(fd))[1]):
            state.write_json_atomic(self.root / "x.json", {"a": 1})
        self.assertGreaterEqual(len(calls), 1)


class CrashPointTests(unittest.TestCase):
    def _run(self, **env):
        base = {k: v for k, v in os.environ.items()
                if k not in ("LEADHAND_TESTING", "LEADHAND_CRASH_AT")}
        base["PYTHONPATH"] = LIB
        base.update(env)
        return subprocess.run(
            [sys.executable, "-B", "-c",
             "from leadhand import state; state.crash_point('before_rebase'); print('survived')"],
            env=base, capture_output=True, text=True, timeout=60)

    def test_crash_point_exits_97_only_when_testing(self):
        hit = self._run(LEADHAND_TESTING="1", LEADHAND_CRASH_AT="before_rebase")
        self.assertEqual(hit.returncode, 97)
        self.assertNotIn("survived", hit.stdout)
        for env in (
            {"LEADHAND_CRASH_AT": "before_rebase"},
            {"LEADHAND_TESTING": "0", "LEADHAND_CRASH_AT": "before_rebase"},
            {"LEADHAND_TESTING": "1"},
            {"LEADHAND_TESTING": "1", "LEADHAND_CRASH_AT": "after_rebase"},
        ):
            miss = self._run(**env)
            self.assertEqual(miss.returncode, 0, env)
            self.assertIn("survived", miss.stdout)

    def test_crash_points_listed(self):
        self.assertEqual(state.CRASH_POINTS, (
            "before_worker_launch", "after_worker_launch", "before_rebase", "after_rebase",
            "before_fast_forward", "after_fast_forward", "before_result_record",
            "after_result_files", "after_charge_before_attempt", "after_cancel_request",
            "before_gate_record"))

    def test_event_kinds_listed(self):
        self.assertEqual(set(state.EVENT_KINDS), {
            "run_started", "card_cut", "brief_written", "dispatch_requested", "worker_started",
            "result_recorded", "verdict_recorded", "fix_round", "diff_checked",
            "integration_started", "rebased", "conflict", "accept_started", "accepted",
            "parked", "unparked", "ruling", "violation", "gate_started", "gate_result",
            "outcome", "report_written", "paused", "cancelled", "resumed", "reconciled"})
        self.assertEqual(len(state.EVENT_KINDS), len(set(state.EVENT_KINDS)))


def ev(seq, kind, **fields):
    return {"seq": seq, "ts": "2026-11-10T00:00:00Z", "kind": kind, **fields}


class FoldCardsTests(unittest.TestCase):
    def test_fold_cards_tracks_requested_started_and_accepting(self):
        events = [
            ev(1, "run_started"),
            ev(2, "card_cut", card=1, branch="leadhand/c1", base="aaa"),
            ev(3, "dispatch_requested", dispatch_id="d1", card=1, attempt=1,
               dispatch_kind="implement", vendor="codex"),
        ]
        cards = state.fold_cards(events, [1, 2])
        self.assertEqual(cards[1].state, "requested")
        self.assertEqual(cards[1].attempt, 1)
        self.assertEqual(cards[1].dispatch_id, "d1")
        self.assertEqual(cards[1].vendor, "codex")
        self.assertEqual(cards[1].branch, "leadhand/c1")
        self.assertEqual(cards[1].base, "aaa")
        self.assertEqual(cards[1].kind_counts, {"implement": 1})
        self.assertEqual(cards[2].state, "pending")
        self.assertEqual(cards[2].attempt, 0)

        events.append(ev(4, "worker_started", dispatch_id="d1", worker_id="w1"))
        cards = state.fold_cards(events, [1, 2])
        self.assertEqual((cards[1].state, cards[1].worker_id), ("started", "w1"))

        events.append(ev(5, "result_recorded", dispatch_id="d1", status="READY",
                         blocked_by=None, tip="bbb"))
        cards = state.fold_cards(events, [1, 2])
        self.assertEqual((cards[1].state, cards[1].status, cards[1].tip),
                         ("reported", "READY", "bbb"))

        events.append(ev(6, "dispatch_requested", dispatch_id="d2", card=1, attempt=1,
                         dispatch_kind="review", vendor="claude"))
        cards = state.fold_cards(events, [1, 2])
        self.assertEqual(cards[1].state, "reviewing")
        self.assertEqual(cards[1].kind_counts, {"implement": 1, "review": 1})

        events.append(ev(7, "verdict_recorded", card=1, verdict="approve", findings=None))
        cards = state.fold_cards(events, [1, 2])
        self.assertEqual((cards[1].state, cards[1].verdict), ("reviewed", "approve"))

        events.append(ev(8, "accept_started", card=1, target="bbb"))
        cards = state.fold_cards(events, [1, 2])
        self.assertEqual((cards[1].state, cards[1].accept_target), ("accepting", "bbb"))
        self.assertIsNone(cards[1].accepted_sha)

        events.append(ev(9, "accepted", card=1, sha="bbb"))
        cards = state.fold_cards(events, [1, 2])
        self.assertEqual((cards[1].state, cards[1].accepted_sha), ("accepted", "bbb"))

    def test_fold_cards_fix_round(self):
        events = [
            ev(1, "card_cut", card=1, branch="b", base="a"),
            ev(2, "dispatch_requested", dispatch_id="d1", card=1, attempt=1,
               dispatch_kind="implement", vendor="codex"),
            ev(3, "result_recorded", dispatch_id="d1", status="READY", tip="t1"),
            ev(4, "verdict_recorded", card=1, verdict="changes", findings="F1: fix it"),
            ev(5, "fix_round", card=1, n=1),
        ]
        c = state.fold_cards(events, [1])[1]
        self.assertEqual((c.state, c.fix_rounds, c.findings), ("fixing", 1, "F1: fix it"))
        events.append(ev(6, "dispatch_requested", dispatch_id="d2", card=1, attempt=2,
                         dispatch_kind="fix", vendor="codex"))
        events.append(ev(7, "worker_started", dispatch_id="d2", worker_id="w2"))
        c = state.fold_cards(events, [1])[1]
        self.assertEqual((c.state, c.attempt, c.kind_counts), ("fixing", 2, {"implement": 1, "fix": 1}))
        events.append(ev(8, "result_recorded", dispatch_id="d2", status="READY", tip="t2"))
        c = state.fold_cards(events, [1])[1]
        self.assertEqual((c.state, c.tip), ("reported", "t2"))

    def test_fold_cards_integration_resets_verdict(self):
        events = [
            ev(1, "card_cut", card=1, branch="b", base="a"),
            ev(2, "verdict_recorded", card=1, verdict="approve", findings=None),
            ev(3, "integration_started", card=1),
        ]
        c = state.fold_cards(events, [1])[1]
        self.assertEqual(c.state, "integrating")
        events.append(ev(4, "rebased", card=1, tip="newtip"))
        c = state.fold_cards(events, [1])[1]
        self.assertEqual((c.state, c.tip, c.verdict), ("integrating", "newtip", None))
        events.append(ev(5, "conflict", card=1))
        self.assertEqual(state.fold_cards(events, [1])[1].state, "integrating")

    def test_fold_cards_park_and_unpark(self):
        events = [
            ev(1, "card_cut", card=1, branch="b", base="a"),
            ev(2, "parked", card=1, reason="two approaches failed"),
        ]
        c = state.fold_cards(events, [1])[1]
        self.assertEqual((c.state, c.park_reason), ("parked", "two approaches failed"))
        events.append(ev(3, "unparked", card=1, note="try again"))
        c = state.fold_cards(events, [1])[1]
        self.assertEqual((c.state, c.park_reason, c.branch), ("cut", None, "b"))

    def test_fold_cards_unparked_without_branch_is_pending(self):
        events = [ev(1, "parked", card=1, reason="x"), ev(2, "unparked", card=1, note="n")]
        self.assertEqual(state.fold_cards(events, [1])[1].state, "pending")

    def test_fold_cards_accepted_is_terminal_and_unknown_cards_ignored(self):
        events = [
            ev(1, "card_cut", card=1, branch="b", base="a"),
            ev(2, "accepted", card=1, sha="s"),
            ev(3, "parked", card=1, reason="late"),
            ev(4, "card_cut", card=99, branch="z", base="a"),
        ]
        cards = state.fold_cards(events, [1])
        self.assertEqual(list(cards), [1])
        self.assertEqual(cards[1].state, "accepted")
        self.assertIsNone(cards[1].park_reason)

    def test_fold_cards_ignores_stale_dispatch_events(self):
        events = [
            ev(1, "dispatch_requested", dispatch_id="d1", card=1, attempt=1,
               dispatch_kind="implement", vendor="codex"),
            ev(2, "dispatch_requested", dispatch_id="d2", card=1, attempt=1,
               dispatch_kind="implement", vendor="codex"),
            ev(3, "worker_started", dispatch_id="d1", worker_id="old"),
            ev(4, "worker_started", dispatch_id="nope", worker_id="ghost"),
        ]
        c = state.fold_cards(events, [1])[1]
        self.assertEqual((c.state, c.worker_id, c.dispatch_id), ("requested", None, "d2"))

    def test_card_states_constant(self):
        self.assertEqual(state.CARD_STATES, (
            "pending", "cut", "requested", "started", "reported", "reviewing", "reviewed",
            "fixing", "integrating", "accepting", "accepted", "parked"))


def card(number, st="accepted", **kw):
    base = dict(number=number, state=st, attempt=1, branch=None, base=None, tip=None,
                dispatch_id=None, worker_id=None, vendor=None, status=None, verdict=None,
                findings=None, fix_rounds=0, kind_counts={}, accept_target=None,
                accepted_sha=None, park_reason=None)
    base.update(kw)
    return CardState(**base)


def decide(cards, **kw):
    args = dict(removed=frozenset(), open_findings=False, holds=(), gates_current=True,
                verification_failed=False, cancelled=False, blocked_reason=None,
                unreconciled=None)
    args.update(kw)
    return state.decide_outcome(cards, **args)


class OutcomeTests(unittest.TestCase):
    def test_all_accepted_and_current_is_complete(self):
        d = decide({1: card(1), 2: card(2)})
        self.assertEqual((d.outcome, d.qualifier, d.reasons), (Outcome.COMPLETE, "", ()))

    def test_removed_cards_count_as_done(self):
        d = decide({1: card(1), 2: card(2, "parked", park_reason="x")}, removed=frozenset({2}))
        self.assertEqual(d.outcome, Outcome.COMPLETE)

    def test_complete_requires_current_gates(self):
        d = decide({1: card(1)}, gates_current=False)
        self.assertEqual(d.outcome, Outcome.INCOMPLETE)
        self.assertEqual(len(d.reasons), 1)
        self.assertIn("gate", d.reasons[0])

    def test_parked_card_makes_incomplete(self):
        d = decide({1: card(1), 2: card(2, "parked", park_reason="stuck"),
                    3: card(3, "parked", park_reason="spent")})
        self.assertEqual((d.outcome, d.qualifier), (Outcome.INCOMPLETE, ""))
        self.assertEqual(len(d.reasons), 2)
        self.assertIn("card 2", d.reasons[0])
        self.assertIn("stuck", d.reasons[0])
        self.assertIn("card 3", d.reasons[1])

    def test_unfinished_card_makes_incomplete(self):
        d = decide({1: card(1, "reviewing")})
        self.assertEqual(d.outcome, Outcome.INCOMPLETE)
        self.assertIn("reviewing", d.reasons[0])

    def test_open_findings_make_incomplete(self):
        d = decide({1: card(1)}, open_findings=True)
        self.assertEqual(d.outcome, Outcome.INCOMPLETE)
        self.assertEqual(len(d.reasons), 1)

    def test_holds_make_incomplete(self):
        d = decide({1: card(1)}, holds=("card 1: no evidence", "card 1: unverified claim"))
        self.assertEqual(d.outcome, Outcome.INCOMPLETE)
        self.assertEqual(len(d.reasons), 2)
        self.assertIn("no evidence", d.reasons[0])

    def test_reasons_accumulate(self):
        d = decide({1: card(1, "parked", park_reason="p")}, open_findings=True,
                   holds=("h",), gates_current=False)
        self.assertEqual(d.outcome, Outcome.INCOMPLETE)
        self.assertEqual(len(d.reasons), 4)

    def test_cancel_is_incomplete_cancelled(self):
        d = decide({1: card(1)}, cancelled=True)
        self.assertEqual((d.outcome, d.qualifier), (Outcome.INCOMPLETE, "cancelled"))
        self.assertTrue(d.reasons)

    def test_pause_is_blocked_paused(self):
        d = decide({1: card(1, "pending")}, blocked_reason="paused")
        self.assertEqual((d.outcome, d.qualifier), (Outcome.BLOCKED, "paused"))

    def test_blocked_qualifiers(self):
        for q in ("paused", "provider capacity", "decision needed", "environment"):
            d = decide({}, blocked_reason=q)
            self.assertEqual((d.outcome, d.qualifier), (Outcome.BLOCKED, q))
        with self.assertRaises(ValueError):
            decide({}, blocked_reason="because")

    def test_unreconciled_is_failed(self):
        d = decide({1: card(1)}, unreconciled="card 2 has a worker with no dispatch")
        self.assertEqual(d.outcome, Outcome.FAILED)
        self.assertEqual(d.qualifier, "")
        self.assertEqual(d.reasons,
                         ("state could not be reconciled: card 2 has a worker with no dispatch",))

    def test_verification_failed_is_failed(self):
        d = decide({1: card(1)}, verification_failed=True)
        self.assertEqual(d.outcome, Outcome.FAILED)

    def test_first_match_wins(self):
        everything = dict(unreconciled="x", verification_failed=True,
                          blocked_reason="paused", cancelled=True)
        self.assertEqual(decide({1: card(1)}, **everything).outcome, Outcome.FAILED)
        self.assertIn("reconciled", decide({}, **everything).reasons[0])
        everything["unreconciled"] = None
        d = decide({}, **everything)
        self.assertEqual(d.outcome, Outcome.FAILED)
        self.assertIn("verification", d.reasons[0])
        everything["verification_failed"] = False
        self.assertEqual(decide({}, **everything).outcome, Outcome.BLOCKED)
        everything["blocked_reason"] = None
        d = decide({1: card(1)}, **everything)
        self.assertEqual((d.outcome, d.qualifier), (Outcome.INCOMPLETE, "cancelled"))

    def test_outcome_values(self):
        self.assertEqual([o.value for o in Outcome],
                         ["complete", "incomplete", "blocked", "failed"])
        self.assertEqual(Outcome.COMPLETE, "complete")

    def test_allowed_next_step_matrix(self):
        def decision(outcome):
            return state.OutcomeDecision(outcome, "", ())

        for step in ("none", "pr", "skill:x"):
            for on_incomplete in ("none", "draft-pr"):
                after = AfterRun(step=step, on_incomplete=on_incomplete,
                                 repeat_safe=False, check="")
                self.assertEqual(state.allowed_next_step(decision(Outcome.COMPLETE), after), step)
                self.assertEqual(state.allowed_next_step(decision(Outcome.INCOMPLETE), after),
                                 "draft-pr" if on_incomplete == "draft-pr" else "none")
                self.assertEqual(state.allowed_next_step(decision(Outcome.BLOCKED), after), "none")
                self.assertEqual(state.allowed_next_step(decision(Outcome.FAILED), after), "none")


if __name__ == "__main__":
    unittest.main()
