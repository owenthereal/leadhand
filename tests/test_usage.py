import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from leadhand import state, usage
from leadhand.usage import UNKNOWN, Usage

ROOT = Path(__file__).resolve().parent.parent
LIB = str(ROOT / "scripts" / "lib")
TESTS = str(ROOT / "tests")
FIXTURE = ROOT / "tests" / "fixtures" / "claude-subagent-duplicates.jsonl"

OPUS = "claude-opus-4-1"


# Synthetic line builders. Field paths mirror the constants at the top of usage.py.

def claude_line(mid, out, inp=0, cached=0, w5=0, w1=0, model=OPUS, legacy=False):
    u = {"input_tokens": inp, "cache_read_input_tokens": cached, "output_tokens": out}
    if legacy:
        u["cache_creation_input_tokens"] = w5
    else:
        u["cache_creation_input_tokens"] = w5 + w1
        u["cache_creation"] = {"ephemeral_5m_input_tokens": w5,
                               "ephemeral_1h_input_tokens": w1}
    return json.dumps({"type": "assistant",
                       "message": {"id": mid, "model": model, "role": "assistant", "usage": u}})


def codex_line(inp, cached, out, reasoning=0):
    total = {"input_tokens": inp, "cached_input_tokens": cached,
             "output_tokens": out, "reasoning_output_tokens": reasoning}
    return json.dumps({"type": "event_msg",
                       "payload": {"type": "token_count",
                                   "info": {"total_token_usage": total,
                                            "last_token_usage": total}}})


def append_lines(path, *lines):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a", encoding="utf-8") as f:
        for line in lines:
            f.write(line + "\n")


def rollout_path(sessions_dir, thread_id):
    return Path(sessions_dir) / "2026" / "11" / "10" / f"rollout-2026-11-10T00-00-00-{thread_id}.jsonl"


def child_env(**extra):
    env = {k: v for k, v in os.environ.items()
           if k not in ("LEADHAND_TESTING", "LEADHAND_CRASH_AT")}
    env["PYTHONPATH"] = os.pathsep.join([LIB, TESTS])
    env.update(extra)
    return env


class UsageCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = Path(self._tmp.name)
        self.root = self.tmp / "run with spaces"
        self.root.mkdir()
        self.paths = state.run_paths(self.root)
        self.projects = self.tmp / "projects"
        self.sessions = self.tmp / "sessions"

    def claude_charge(self, n, path, charge_id=None, key="agent1"):
        return usage.charge(self.paths, charge_id=charge_id or f"d{n}:claude:{key}",
                            source="claude", key=key, dispatch_id=f"d{n}",
                            role="implementer", path_or_thread=Path(path))

    def codex_charge(self, n, thread="thr1"):
        return usage.charge(self.paths, charge_id=f"d{n}:codex:{thread}", source="codex",
                            key=thread, dispatch_id=f"d{n}", role="implementer",
                            path_or_thread=thread, sessions_dir=self.sessions)


class ArithmeticTests(unittest.TestCase):
    def test_add_sums_fields_and_known_is_conjunction(self):
        a = Usage(input=1, cached=2, cache_write_5m=3, cache_write_1h=4, output=5, reasoning=6)
        b = Usage(input=10, cached=20, cache_write_5m=30, cache_write_1h=40, output=50, reasoning=60)
        s = usage.add(a, b)
        self.assertEqual((s.input, s.cached, s.cache_write_5m, s.cache_write_1h, s.output, s.reasoning),
                         (11, 22, 33, 44, 55, 66))
        self.assertTrue(s.known)
        self.assertFalse(usage.add(a, UNKNOWN).known)
        self.assertEqual(usage.add(a, UNKNOWN).input, 1)

    def test_subtract_is_field_wise_and_floored_at_zero(self):
        a = Usage(input=5, cached=1, output=100)
        b = Usage(input=3, cached=9, output=40)
        d = usage.subtract(a, b)
        self.assertEqual((d.input, d.cached, d.output), (2, 0, 60))

    def test_estimate_cost_opus_matches_recorded_session(self):
        prices = {"opus": {"input": 4, "output": 20, "cache_read": 0.20,
                           "cache_write_5m": 5, "cache_write_1h": 8}}
        u = Usage(input=304_700, output=1_301_069, cached=366_728_180,
                  cache_write_5m=3_007_432, cache_write_1h=4_080_700)
        cost = usage.estimate_cost(u, OPUS, prices)
        self.assertAlmostEqual(cost, 148.27, delta=0.01)

    def test_estimate_cost_unpriced_model_is_none(self):
        prices = {"opus": {"input": 4, "output": 20, "cache_read": 0.2,
                           "cache_write_5m": 5, "cache_write_1h": 8}}
        u = Usage(input=1000, output=1000)
        self.assertIsNone(usage.estimate_cost(u, "gpt-6-luna", prices))
        self.assertIsNone(usage.estimate_cost(u, "claude-sonnet-4", prices))  # no sonnet price
        self.assertIsNone(usage.estimate_cost(UNKNOWN, OPUS, prices))
        self.assertIsNotNone(usage.estimate_cost(u, OPUS, prices))


class LocatorTests(UsageCase):
    def test_claude_transcript_locators(self):
        sub = self.projects / "-some-project" / "sess-1" / "subagents" / "agent-abc123.jsonl"
        sess = self.projects / "-some-project" / "sess-1.jsonl"
        append_lines(sub, "{}")
        append_lines(sess, "{}")
        self.assertEqual(usage.claude_subagent_transcript("abc123", self.projects), sub)
        self.assertEqual(usage.claude_session_transcript("sess-1", self.projects), sess)
        self.assertIsNone(usage.claude_subagent_transcript("nope", self.projects))
        self.assertIsNone(usage.claude_session_transcript("nope", self.projects))
        self.assertIsNone(usage.claude_subagent_transcript("*", self.projects))

    def test_missing_projects_dir_is_none(self):
        self.assertIsNone(usage.claude_subagent_transcript("abc", self.tmp / "absent"))


class ClaudeUsageTests(UsageCase):
    def test_claude_usage_dedupes_by_message_id(self):
        u, ids = usage.claude_usage(FIXTURE, exclude_ids=frozenset())
        self.assertEqual(ids, frozenset({"msg_A", "msg_B", "msg_C"}))
        self.assertEqual((u.input, u.cached, u.cache_write_5m, u.cache_write_1h, u.output),
                         (130, 1500, 250, 300, 130))
        self.assertTrue(u.known)
        self.assertEqual(u.model, OPUS)

    def test_claude_usage_skips_excluded_ids(self):
        u, ids = usage.claude_usage(FIXTURE, exclude_ids=frozenset({"msg_A"}))
        self.assertEqual(ids, frozenset({"msg_B", "msg_C"}))
        self.assertEqual((u.input, u.cached, u.cache_write_5m, u.cache_write_1h, u.output),
                         (30, 500, 50, 0, 90))

    def test_claude_usage_ignores_partial_and_garbage_lines(self):
        f = self.tmp / "t.jsonl"
        append_lines(f, claude_line("m1", 4, inp=1), "not json", "[1,2]")
        with open(f, "a") as fh:
            fh.write(claude_line("m2", 9)[:30])  # partial final line, no newline
        u, ids = usage.claude_usage(f, exclude_ids=frozenset())
        self.assertEqual(ids, frozenset({"m1"}))
        self.assertEqual(u.output, 4)


class ChargeClaudeTests(UsageCase):
    def test_charge_claude_resumed_worker_counts_each_message_once(self):
        f = self.projects / "p" / "s" / "subagents" / "agent-agent1.jsonl"
        # implementation
        append_lines(f, claude_line("m1", 10, inp=100, cached=1000, w5=50),
                     claude_line("m1", 20, inp=100, cached=1000, w5=50),  # repeated block
                     claude_line("m2", 30, inp=5, w1=7))
        c1 = self.claude_charge(1, f)
        # resumed fix appended to the same transcript
        append_lines(f, claude_line("m3", 40, inp=11, cached=2000),
                     claude_line("m4", 50, inp=12, cached=3000, w5=9))
        c2 = self.claude_charge(2, f)
        # interrupted resume: one more message
        append_lines(f, claude_line("m5", 60, inp=13))
        c3 = self.claude_charge(3, f)

        self.assertEqual([c1.usage.output, c2.usage.output, c3.usage.output], [50, 90, 60])
        total, ids = usage.claude_usage(f, exclude_ids=frozenset())
        self.assertEqual(ids, frozenset({"m1", "m2", "m3", "m4", "m5"}))
        summed = usage.add(usage.add(c1.usage, c2.usage), c3.usage)
        self.assertEqual(summed, total)
        self.assertEqual(usage.totals(usage.read_charges(self.paths))["implementer"], total)
        # a further charge with nothing new moves nothing
        c4 = self.claude_charge(4, f)
        self.assertEqual((c4.usage.input, c4.usage.output), (0, 0))
        self.assertTrue(c4.usage.known)

    def test_charge_record_is_durable_and_round_trips(self):
        f = self.tmp / "t.jsonl"
        append_lines(f, claude_line("m1", 10, inp=3))
        c = self.claude_charge(1, f)
        raw = [json.loads(line) for line in self.paths.charges.read_text().splitlines()]
        self.assertEqual(len(raw), 1)
        self.assertEqual(raw[0]["charge_id"], "d1:claude:agent1")
        self.assertEqual(raw[0]["usage"]["known"], True)
        self.assertEqual(raw[0]["usage"]["output"], 10)
        self.assertEqual(usage.read_charges(self.paths), [c])
        self.assertEqual(usage.charged_checkpoint([c], "claude", "agent1"), {"messages": {"m1": 10}})
        self.assertEqual(usage.charged_checkpoint([c], "claude", "other"), {"messages": {}})

    def test_charge_tops_up_message_charged_mid_stream(self):
        f = self.tmp / "live.jsonl"
        append_lines(f, claude_line("m1", 5, inp=100, cached=1000, w5=50))
        c1 = self.claude_charge(1, f)
        self.assertEqual((c1.usage.input, c1.usage.output), (100, 5))
        append_lines(f, claude_line("m1", 40, inp=100, cached=1000, w5=50),
                     claude_line("m2", 7, inp=3))
        c2 = self.claude_charge(2, f)
        # m1: only the 35 extra output (input side was charged with its first record); m2 in full
        self.assertEqual((c2.usage.input, c2.usage.cached, c2.usage.cache_write_5m, c2.usage.output),
                         (3, 0, 0, 35 + 7))
        self.assertEqual(c2.checkpoint, {"messages": {"m1": 40, "m2": 7}})
        c3 = self.claude_charge(3, f)  # nothing grew
        self.assertEqual(c3.usage.output, 0)
        self.assertEqual(c3.checkpoint, {"messages": {}})
        total, _ = usage.claude_usage(f, exclude_ids=frozenset())
        self.assertEqual(usage.totals(usage.read_charges(self.paths))["implementer"], total)
        self.assertEqual(usage.charged_checkpoint(usage.read_charges(self.paths), "claude", "agent1"),
                         {"messages": {"m1": 40, "m2": 7}})

    def test_charge_none_path_is_unknown_and_relative_path_is_rejected(self):
        c = usage.charge(self.paths, charge_id="d1:claude:a", source="claude", key="a",
                         dispatch_id="d1", role="implementer", path_or_thread=None)
        self.assertFalse(c.usage.known)
        c2 = usage.charge(self.paths, charge_id="d1:codex:t", source="codex", key="t",
                          dispatch_id="d1", role="implementer", path_or_thread=None)
        self.assertFalse(c2.usage.known)
        with self.assertRaises(ValueError):
            usage.charge(self.paths, charge_id="d2:claude:a", source="claude", key="a",
                         dispatch_id="d2", role="implementer",
                         path_or_thread=Path("relative/agent.jsonl"))
        self.assertEqual(len(usage.read_charges(self.paths)), 2)

    def test_repeated_charge_id_is_harmless(self):
        f = self.tmp / "t.jsonl"
        append_lines(f, claude_line("m1", 10))
        first = self.claude_charge(1, f)
        append_lines(f, claude_line("m2", 99))
        again = self.claude_charge(1, f)  # same charge_id, file has grown
        self.assertEqual(again, first)
        self.assertEqual(len(usage.read_charges(self.paths)), 1)
        # codex too
        append_lines(rollout_path(self.sessions, "thr1"), codex_line(100, 0, 10))
        x = self.codex_charge(1)
        append_lines(rollout_path(self.sessions, "thr1"), codex_line(500, 0, 50))
        self.assertEqual(self.codex_charge(1), x)
        self.assertEqual(len(usage.read_charges(self.paths)), 2)

    def test_charge_missing_file_is_unknown_and_moves_no_checkpoint(self):
        f = self.tmp / "later.jsonl"
        c = self.claude_charge(1, f)
        self.assertFalse(c.usage.known)
        raw = json.loads(self.paths.charges.read_text().splitlines()[0])
        self.assertEqual(raw["usage"]["known"], False)
        self.assertEqual(usage.charged_checkpoint([c], "claude", "agent1"), {"messages": {}})
        # the file appears later: everything is still chargeable
        append_lines(f, claude_line("m1", 10, inp=2))
        c2 = self.claude_charge(2, f)
        self.assertTrue(c2.usage.known)
        self.assertEqual((c2.usage.input, c2.usage.output), (2, 10))
        # codex: missing session file
        u = self.codex_charge(3, "ghost")
        self.assertFalse(u.usage.known)
        self.assertEqual(usage.charged_checkpoint([u], "codex", "ghost"), {"cumulative": {}})
        # totals say the run has an unknown charge
        all_charges = usage.read_charges(self.paths)
        self.assertEqual(usage.unknown_count(all_charges), 2)
        self.assertFalse(usage.totals(all_charges)["implementer"].known)

    def test_totals_group_by_role_dispatch_and_source(self):
        f = self.tmp / "t.jsonl"
        append_lines(f, claude_line("m1", 10))
        usage.charge(self.paths, charge_id="d1:claude:a", source="claude", key="a",
                     dispatch_id="d1", role="implementer", path_or_thread=f)
        g = self.tmp / "u.jsonl"
        append_lines(g, claude_line("m9", 5))
        usage.charge(self.paths, charge_id="d2:claude:b", source="claude", key="b",
                     dispatch_id="d2", role="reviewer", path_or_thread=g)
        cs = usage.read_charges(self.paths)
        self.assertEqual({k: v.output for k, v in usage.totals(cs, by="role").items()},
                         {"implementer": 10, "reviewer": 5})
        self.assertEqual({k: v.output for k, v in usage.totals(cs, by="dispatch_id").items()},
                         {"d1": 10, "d2": 5})
        self.assertEqual(usage.totals(cs, by="source")["claude"].output, 15)
        with self.assertRaises(ValueError):
            usage.totals(cs, by="color")


class CodexTests(UsageCase):
    def test_codex_cumulative_takes_last_token_count_and_maps_fields(self):
        f = rollout_path(self.sessions, "thr1")
        append_lines(f, json.dumps({"type": "session_meta"}),
                     codex_line(100, 40, 10, 3),
                     json.dumps({"type": "event_msg",
                                 "payload": {"type": "token_count", "info": None}}),
                     codex_line(300, 100, 25, 8),
                     json.dumps({"type": "event_msg", "payload": {"type": "agent_message"}}))
        u = usage.codex_cumulative("thr1", self.sessions)
        # input_tokens includes the cached tokens, so uncached input = 300 - 100
        self.assertEqual((u.input, u.cached, u.output, u.reasoning), (200, 100, 25, 8))
        self.assertTrue(u.known)

    def test_codex_cumulative_unknown_when_missing_or_without_token_count(self):
        self.assertFalse(usage.codex_cumulative("thr1", self.sessions).known)
        append_lines(rollout_path(self.sessions, "thr2"), json.dumps({"type": "session_meta"}))
        self.assertFalse(usage.codex_cumulative("thr2", self.sessions).known)

    def test_charge_codex_cumulative_deltas(self):
        f = rollout_path(self.sessions, "thr1")
        append_lines(f, codex_line(100, 0, 0))
        c1 = self.codex_charge(1)
        append_lines(f, codex_line(250, 0, 0))
        c2 = self.codex_charge(2)
        self.assertEqual((c1.usage.input, c2.usage.input), (100, 150))
        self.assertEqual(usage.charged_checkpoint([c1, c2], "codex", "thr1")["cumulative"]["input"], 250)
        # interrupted: no new tokens -> zero delta, checkpoint unchanged
        c3 = self.codex_charge(3)
        self.assertEqual(c3.usage.input, 0)
        total = usage.totals(usage.read_charges(self.paths))["implementer"]
        self.assertEqual(total.input, 250)

    def test_charge_codex_counter_reset_charges_new_epoch_in_full(self):
        f = rollout_path(self.sessions, "thr1")
        append_lines(f, codex_line(100, 0, 40))
        c1 = self.codex_charge(1)
        append_lines(f, codex_line(250, 0, 90))
        c2 = self.codex_charge(2)
        self.assertEqual((c2.usage.input, c2.usage.output, c2.epoch_reset), (150, 50, False))
        # the thread was resumed and its counter restarted below the checkpoint
        append_lines(f, codex_line(30, 0, 5))
        c3 = self.codex_charge(3)
        self.assertEqual((c3.usage.input, c3.usage.output, c3.epoch_reset), (30, 5, True))
        raw = [json.loads(line) for line in self.paths.charges.read_text().splitlines()]
        self.assertTrue(raw[2]["epoch_reset"])
        self.assertNotIn("epoch_reset", raw[0])
        self.assertEqual(usage.read_charges(self.paths)[2], c3)
        # the checkpoint is the cumulative reached (not a max): growth continues from it
        self.assertEqual(usage.charged_checkpoint([c1, c2, c3], "codex", "thr1")["cumulative"]["input"], 30)
        append_lines(f, codex_line(80, 0, 12))
        c4 = self.codex_charge(4)
        self.assertEqual((c4.usage.input, c4.usage.output, c4.epoch_reset), (50, 7, False))
        # a single lower field is enough to count as a reset
        append_lines(f, codex_line(500, 0, 3))
        c5 = self.codex_charge(5)
        self.assertEqual((c5.usage.input, c5.usage.output, c5.epoch_reset), (500, 3, True))

    def test_codex_stream_usage_sums_turn_completed(self):
        events = [
            {"type": "thread.started", "thread_id": "thr1"},
            {"type": "turn.completed", "usage": {"input_tokens": 100, "cached_input_tokens": 30,
                                                 "output_tokens": 10, "reasoning_output_tokens": 4}},
            {"type": "item.completed"},
            {"type": "turn.completed", "usage": {"input_tokens": 50, "cached_input_tokens": 10,
                                                 "output_tokens": 5, "reasoning_output_tokens": 1}},
        ]
        u = usage.codex_usage_from_stream(iter(events))
        self.assertEqual((u.input, u.cached, u.output, u.reasoning), (110, 40, 15, 5))
        self.assertTrue(u.known)
        self.assertFalse(usage.codex_usage_from_stream([{"type": "thread.started"}]).known)


CRASH_SCRIPT = """
import sys
from pathlib import Path
from leadhand import state, usage
root, transcript = sys.argv[1:3]
paths = state.run_paths(Path(root))
c = usage.charge(paths, charge_id="d1:claude:agent1", source="claude", key="agent1",
                 dispatch_id="d1", role="implementer", path_or_thread=Path(transcript))
state.crash_point("after_charge_before_attempt")
state.append_attempt(paths, {"dispatch_id": "d1", "charge_ids": [c.charge_id]})
"""

WORKER_SCRIPT = """
import sys
from pathlib import Path
import test_usage as t
from leadhand import state, usage
root, who, claude_file, sessions, n = sys.argv[1:6]
n = int(n)
paths = state.run_paths(Path(root))
thread = "thr1"
for i in range(n):
    if who == "A":
        t.append_lines(claude_file, t.claude_line(f"A-{i}", i + 1, inp=2, cached=7))
    else:
        t.append_lines(t.rollout_path(sessions, thread), t.codex_line(100 + 10 * (i + 1), 20, 5 * (i + 2)))
    usage.charge(paths, charge_id=f"{who}{i}:claude:S", source="claude", key="S",
                 dispatch_id=f"{who}{i}", role="supervisor" if who == "A" else "implementer",
                 path_or_thread=Path(claude_file))
    usage.charge(paths, charge_id=f"{who}{i}:codex:T", source="codex", key=thread,
                 dispatch_id=f"{who}{i}", role="supervisor" if who == "A" else "implementer",
                 path_or_thread=thread, sessions_dir=Path(sessions))
"""


class CrashAndConcurrencyTests(UsageCase):
    def test_crash_after_charge_before_attempt_keeps_exact_total(self):
        f = self.tmp / "agent1.jsonl"
        append_lines(f, claude_line("m1", 10, inp=3), claude_line("m2", 20, cached=5))
        crashed = subprocess.run(
            [sys.executable, "-B", "-c", CRASH_SCRIPT, str(self.root), str(f)],
            env=child_env(LEADHAND_TESTING="1", LEADHAND_CRASH_AT="after_charge_before_attempt"),
            capture_output=True, text=True, timeout=60)
        self.assertEqual(crashed.returncode, 97, crashed.stderr)
        self.assertEqual(len(usage.read_charges(self.paths)), 1)
        self.assertEqual(state.read_attempts(self.paths), [])
        retry = subprocess.run(
            [sys.executable, "-B", "-c", CRASH_SCRIPT, str(self.root), str(f)],
            env=child_env(), capture_output=True, text=True, timeout=60)
        self.assertEqual(retry.returncode, 0, retry.stderr)
        charges = usage.read_charges(self.paths)
        attempts = state.read_attempts(self.paths)
        self.assertEqual(len(charges), 1)
        self.assertEqual(len(attempts), 1)
        self.assertEqual(attempts[0]["charge_ids"], [charges[0].charge_id])
        expected, _ = usage.claude_usage(f, exclude_ids=frozenset())
        self.assertEqual(usage.totals(charges)["implementer"], expected)

    def test_concurrent_supervisor_and_runner_charges_keep_exact_totals(self):
        n = 12
        claude_file = self.projects / "p" / "S.jsonl"
        append_lines(claude_file, claude_line("seed", 3, inp=1))
        append_lines(rollout_path(self.sessions, "thr1"), codex_line(100, 20, 5))
        procs = [subprocess.Popen(
            [sys.executable, "-B", "-c", WORKER_SCRIPT, str(self.root), who,
             str(claude_file), str(self.sessions), str(n)],
            env=child_env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            for who in ("A", "B")]
        for p in procs:
            out, err = p.communicate(timeout=120)
            self.assertEqual(p.returncode, 0, err)
        # the final charges pick up whatever the last writer added after the last charge
        self.claude_charge(99, claude_file, charge_id="final:claude:S", key="S")
        self.codex_charge(99)

        charges = usage.read_charges(self.paths)
        ids = [c.charge_id for c in charges]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(len(ids), 2 * 2 * n + 2 + 0)
        by_source = usage.totals(charges, by="source")
        # Claude: seed + A's n messages (output i+1, input 2, cached 7)
        self.assertEqual(by_source["claude"].output, 3 + sum(i + 1 for i in range(n)))
        self.assertEqual(by_source["claude"].input, 1 + 2 * n)
        self.assertEqual(by_source["claude"].cached, 7 * n)
        # Codex: B's last cumulative is (100 + 10n, 20 cached, 5(n+1) output)
        self.assertEqual(by_source["codex"].input, 100 + 10 * n - 20)
        self.assertEqual(by_source["codex"].cached, 20)
        self.assertEqual(by_source["codex"].output, 5 * (n + 1))
        # the lock-protected file still parses cleanly line by line
        self.assertEqual(len(self.paths.charges.read_text().splitlines()), len(ids))


if __name__ == "__main__":
    unittest.main()
