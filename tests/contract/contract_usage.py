#!/usr/bin/env python3
"""Usage accounting: what `record result` (Claude workers) and `dispatch` (Codex workers)
charge, and how the run prices it.

Converted from tests/test_usage.py and tests/test_usage_models.py. A Claude charge is read from
charges.jsonl after `record result`, with the worker's transcript written by the test under
$CLAUDE_CONFIG_DIR/projects (a FakeSupervisor step's `transcript`); one worker's later charges
come from answer dispatches that resume it. A Codex charge is read after a `dispatch` line, with
the session file written by the test under $CODEX_HOME/sessions and the `--json` stream played
by tests/fakes/codex. The attempt records (attempts.jsonl), status.json and report.md show the
totals and the cost estimates.

Two invariants hold throughout: every token is charged exactly once (across resumed workers,
repeated charges, a crash between a charge and its attempt record, and concurrent writers), and
usage leadhand cannot read is recorded as unknown, never as zero.
"""
import json
import threading
import unittest
from pathlib import Path

from contract_dispatch import World
from harness import ROOT, read_json, read_jsonl, run_cli
from supervisor import PROJECT, FakeSupervisor, card

FIXTURES = ROOT / "tests" / "fixtures"
OPUS = "claude-opus-4-1"
TOKENS = ("input", "cached", "cache_write_5m", "cache_write_1h", "output")
NOTHING = {"responses": [], "cumulative": {}}       # a Codex charge that reached no checkpoint


# --------------------------------------------------------------------------
# Source lines (field paths as Claude Code and Codex write them)

def claude_line(mid, out, inp=0, cached=0, w5=0, w1=0, model=OPUS):
    u = {"input_tokens": inp, "cache_read_input_tokens": cached, "output_tokens": out,
         "cache_creation_input_tokens": w5 + w1,
         "cache_creation": {"ephemeral_5m_input_tokens": w5, "ephemeral_1h_input_tokens": w1}}
    return json.dumps({"type": "assistant", "message": {"id": mid, "model": model, "role": "assistant", "usage": u}})


def codex_line(inp, cached, out, reasoning=0):
    """A rollout `token_count` event with these cumulative totals."""
    total = {"input_tokens": inp, "cached_input_tokens": cached,
             "output_tokens": out, "reasoning_output_tokens": reasoning}
    return json.dumps({"type": "event_msg",
                       "payload": {"type": "token_count",
                                   "info": {"total_token_usage": total, "last_token_usage": total}}})


def text(*lines) -> str:
    return "".join(line + "\n" for line in lines)


def fixture_lines(name, count=None) -> list[str]:
    lines = (FIXTURES / name).read_text(encoding="utf-8").splitlines()
    return lines if count is None else lines[:count]


def tokens(c: dict, *fields) -> tuple:
    """A charge's or attempt's usage fields (all token fields by default)."""
    return tuple(c["usage"][f] for f in fields or TOKENS)


def summed(charges: list[dict]) -> tuple:
    return tuple(sum(c["usage"][f] for c in charges) for f in TOKENS)


# --------------------------------------------------------------------------
# Claude workers: charged by `record result`

def claude_run(test, *, route=None, config="") -> FakeSupervisor:
    w = FakeSupervisor(test, [card(1, route=route)], config=config)
    w.start()
    return w


def drive(w: FakeSupervisor, dispatches: int) -> None:
    """Act on `next` until `dispatches` more dispatches have run and been recorded."""
    done = 0
    while done < dispatches:
        a = w.next()
        if a["action"] == "done":
            raise AssertionError(f"the run finished after {done} of {dispatches} dispatches")
        w.act(a)
        done += a["action"] == "dispatch"


def charge_worker(w: FakeSupervisor, *texts) -> list[dict]:
    """One Claude worker's dispatches in turn: card 1's implement, then answers that resume the
    same worker (every dispatch but the last asks a question, which a ruling answers). Dispatch i
    appends texts[i] to the worker's transcript (None: it has none yet). Returns the worker's
    charges in dispatch order."""
    last = len(texts) - 1
    w.script(1, "implement", {"status": "QUESTION" if last else "READY", "transcript": texts[0]})
    for i, t in enumerate(texts[1:], 1):
        w.script(1, "answer", {"status": "QUESTION" if i < last else "READY", "transcript": t})
    w.rulings.setdefault(1, []).extend(["A: JSON"] * last)
    drive(w, len(texts))
    workers = {e["worker_id"] for e in w.events("worker_started")}
    if len(workers) != 1:
        raise AssertionError(f"expected one resumed worker, got {sorted(workers)}")
    charges = [c for c in w.charges() if c["key"] in workers]
    if [c["dispatch_id"] for c in charges] != w.dispatch_ids():
        raise AssertionError(f"not one charge per dispatch: {charges}")
    return charges


def record_as(w: FakeSupervisor, worker: str, transcript: Path | None = None, content: str = "") -> dict:
    """The next dispatch, run by a worker the test names: `record started` with `worker`, then
    `content` written to `transcript` (if given), then `record result` with a READY reply.
    Returns the dispatch's charge."""
    a = w.next()
    if a["action"] != "dispatch":
        raise AssertionError(f"expected a dispatch, got {a}")
    did = a["dispatch_id"]
    w.cli("record", w.run_dir, "started", "--dispatch", did, "--worker", worker).ok()
    if transcript is not None:
        transcript.parent.mkdir(parents=True, exist_ok=True)
        transcript.write_text(content, encoding="utf-8")
    reply = Path(a["reply_file"])
    reply.parent.mkdir(parents=True, exist_ok=True)
    reply.write_text(f"Status: READY\nReport: {a['report']}\n", encoding="utf-8")
    w.cli("record", w.run_dir, "result", "--dispatch", did, "--worker", worker, "--reply-file", reply).ok()
    [c] = [c for c in w.charges() if c["dispatch_id"] == did]
    return c


def run_usage(w) -> dict:
    """status.json's usage, as the engine's next checkpoint (a `next`) writes it."""
    run_cli("next", w.run_dir, env=w.env, cwd=w.tmp).ok()
    return read_json(w.run_dir / "status.json")["usage"]


def attempt_of(w, did: str) -> dict:
    [a] = [a for a in w.attempts() if a["dispatch"] == did]
    return a


# --------------------------------------------------------------------------
# Codex workers: charged by the `dispatch` runner

def session_file(w: World, thread: str) -> Path:
    path = Path(w.env["CODEX_HOME"]) / "sessions" / "2026" / "11" / "10" / f"rollout-2026-11-10T00-00-00-{thread}.jsonl"
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def append(path: Path, *lines) -> None:
    with open(path, "a", encoding="utf-8") as f:
        f.write(text(*lines))


def turn_completed(inp, cached, out, reasoning=0, cache_write=None) -> dict:
    usage = {"input_tokens": inp, "cached_input_tokens": cached, "output_tokens": out,
             "reasoning_output_tokens": reasoning}
    if cache_write is not None:
        usage["cache_write_input_tokens"] = cache_write
    return {"type": "turn.completed", "usage": usage}


def codex_steps(*turns: dict, named=True, open_turn=False) -> list[dict]:
    """A Codex run that replies READY: the thread starts (unless `named` is False), then the
    `turns` (turn.completed events) are streamed. `open_turn`: a turn starts and never completes."""
    steps = [{"type": "thread.started", "thread_id": "{thread}"}] if named else []
    if open_turn:
        steps.append({"type": "turn.started"})
    return [*steps, {"action": "last_message", "text": "Status: READY\n"}, *turns]


def codex_charge(w: World, did: str, thread: str, *turns: dict, **steps) -> dict:
    """Run the `dispatch` line for `did` with the fake Codex on `thread`; returns its charge."""
    w.scenario(codex_steps(*turns, **steps), thread=thread)
    w.dispatch(did).ok()
    [c] = [c for c in w.charges() if c["dispatch_id"] == did]
    return c


def place_rollout(w: World, thread: str, lines: list[str]) -> Path:
    path = session_file(w, thread)
    path.write_text(text(*lines), encoding="utf-8")
    return path


THREAD = "11111111-1111-4111-8111-111111111111"
STREAM_TURN = turn_completed(1, 0, 1)        # a completed turn whose usage the session file overrides
ROLLOUT = "codex-rollout-interrupted.jsonl"


# --------------------------------------------------------------------------
# Claude

class ClaudeTranscripts(unittest.TestCase):
    """How a worker's transcript is read: once per message ID, the record with the most output."""

    def test_claude_usage_dedupes_by_message_id(self):
        w = claude_run(self)
        [c] = charge_worker(w, (FIXTURES / "claude-subagent-duplicates.jsonl").read_text(encoding="utf-8"))
        self.assertEqual(tokens(c), (130, 1500, 250, 300, 130))
        self.assertTrue(c["usage"]["known"])
        self.assertEqual(c["usage"]["model"], OPUS)
        self.assertEqual(c["checkpoint"], {"messages": {"msg_A": 40, "msg_B": 60, "msg_C": 30}})

    def test_claude_usage_skips_messages_already_charged(self):
        w = claude_run(self)
        lines = fixture_lines("claude-subagent-duplicates.jsonl")
        first, second = charge_worker(w, text(*lines[:4]), text(*lines[4:]))
        self.assertEqual(first["checkpoint"], {"messages": {"msg_A": 40}})
        self.assertEqual(tokens(second), (30, 500, 50, 0, 90))
        self.assertEqual(second["checkpoint"], {"messages": {"msg_B": 60, "msg_C": 30}})

    def test_claude_usage_ignores_partial_and_garbage_lines(self):
        w = claude_run(self)
        content = text(claude_line("m1", 4, inp=1), "not json", "[1,2]") + claude_line("m2", 9)[:30]
        [c] = charge_worker(w, content)            # the partial final line has no newline
        self.assertEqual(c["checkpoint"], {"messages": {"m1": 4}})
        self.assertEqual(tokens(c, "input", "output"), (1, 4))
        self.assertTrue(c["usage"]["known"])

    def test_stopped_transcript_charges_its_one_message_once(self):
        w = claude_run(self)
        c, again = charge_worker(w, (FIXTURES / "claude-subagent-stopped.jsonl").read_text(encoding="utf-8"), "")
        self.assertEqual(tokens(c), (2, 2077, 5811, 0, 117))
        self.assertEqual(c["checkpoint"], {"messages": {"msg_fixture_1": 117}})
        self.assertEqual(tokens(again, "input", "output"), (0, 0))

    def test_resumed_transcript_charges_each_message_once(self):
        w = claude_run(self)
        [c] = charge_worker(w, (FIXTURES / "claude-subagent-resumed.jsonl").read_text(encoding="utf-8"))
        self.assertEqual(tokens(c, "input", "cached", "cache_write_5m", "output"),
                         (12, 27653, 6384, 734))  # 734 per message ID, not 1468

    def test_resumed_file_growth_is_charged_once(self):
        w = claude_run(self)
        lines = fixture_lines("claude-subagent-resumed.jsonl")
        c1, c2, c3 = charge_worker(w, text(*lines[:30]), text(*lines[30:]), "")   # the resume appends
        self.assertEqual(tokens(c1, "input", "output"), (4, 99 + 411))
        self.assertEqual(tokens(c2, "input", "output"), (8, 111 + 113))
        self.assertEqual(tokens(c3, "input", "output"), (0, 0))
        # The whole transcript, charged at once (test_resumed_transcript_charges_each_message_once).
        self.assertEqual(summed([c1, c2, c3]), (12, 27653, 6384, 0, 734))
        implementer = run_usage(w)["by_role"]["implementer"]
        self.assertEqual((implementer["input"], implementer["cached"], implementer["output"]), (12, 27653, 734))


class ClaudeCharges(unittest.TestCase):
    """charges.jsonl after `record result`: each message charged once, unknown never zero."""

    def test_charge_claude_resumed_worker_counts_each_message_once(self):
        w = claude_run(self)
        implementation = text(claude_line("m1", 10, inp=100, cached=1000, w5=50),
                              claude_line("m1", 20, inp=100, cached=1000, w5=50),   # repeated block
                              claude_line("m2", 30, inp=5, w1=7))
        resumed_fix = text(claude_line("m3", 40, inp=11, cached=2000), claude_line("m4", 50, inp=12, cached=3000, w5=9))
        interrupted = text(claude_line("m5", 60, inp=13))
        c1, c2, c3, c4 = charge_worker(w, implementation, resumed_fix, interrupted, "")
        self.assertEqual([c1["usage"]["output"], c2["usage"]["output"], c3["usage"]["output"]], [50, 90, 60])
        total = (100 + 5 + 11 + 12 + 13, 1000 + 2000 + 3000, 50 + 9, 7, 20 + 30 + 40 + 50 + 60)
        self.assertEqual(summed([c1, c2, c3]), total)
        self.assertEqual(set(c1["checkpoint"]["messages"]) | set(c2["checkpoint"]["messages"])
                         | set(c3["checkpoint"]["messages"]), {"m1", "m2", "m3", "m4", "m5"})
        # a further charge with nothing new moves nothing
        self.assertEqual(tokens(c4, "input", "output"), (0, 0))
        self.assertTrue(c4["usage"]["known"])
        implementer = run_usage(w)["by_role"]["implementer"]
        self.assertEqual((implementer["input"], implementer["cached"], implementer["output"]),
                         (total[0], total[1], total[4]))

    def test_charge_record_is_durable_and_round_trips(self):
        w = claude_run(self)
        [c] = charge_worker(w, text(claude_line("m1", 10, inp=3)))
        raw = [json.loads(line) for line in w.paths.charges.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(raw, [c])
        [did], worker = w.dispatch_ids(), w.events("worker_started")[0]["worker_id"]
        self.assertEqual(c["charge_id"], f"{did}:claude:{worker}")
        self.assertEqual((c["source"], c["key"], c["dispatch_id"], c["role"]), ("claude", worker, did, "implementer"))
        self.assertEqual(c["usage"]["known"], True)
        self.assertEqual(c["usage"]["output"], 10)
        self.assertEqual(c["checkpoint"], {"messages": {"m1": 10}})
        attempt = attempt_of(w, did)
        self.assertEqual(attempt["charge_ids"], [c["charge_id"]])
        self.assertEqual(attempt["usage"], c["usage"])

    def test_charge_tops_up_message_charged_mid_stream(self):
        w = claude_run(self)
        c1, c2, c3 = charge_worker(w, text(claude_line("m1", 5, inp=100, cached=1000, w5=50)),
                                   text(claude_line("m1", 40, inp=100, cached=1000, w5=50),
                                        claude_line("m2", 7, inp=3)),
                                   "")
        self.assertEqual(tokens(c1, "input", "output"), (100, 5))
        # m1: only the 35 extra output (its input side was charged with its first record); m2 in full
        self.assertEqual(tokens(c2, "input", "cached", "cache_write_5m", "output"), (3, 0, 0, 35 + 7))
        self.assertEqual(c2["checkpoint"], {"messages": {"m1": 40, "m2": 7}})
        self.assertEqual(c3["usage"]["output"], 0)       # nothing grew
        self.assertEqual(c3["checkpoint"], {"messages": {}})
        self.assertEqual(summed([c1, c2, c3]), (103, 1000, 50, 0, 47))
        implementer = run_usage(w)["by_role"]["implementer"]
        self.assertEqual((implementer["input"], implementer["cached"], implementer["output"]), (103, 1000, 47))

    def test_charge_missing_file_is_unknown_and_moves_no_checkpoint(self):
        w = claude_run(self)
        c1, c2 = charge_worker(w, None, text(claude_line("m1", 10, inp=2)))
        self.assertFalse(c1["usage"]["known"])
        self.assertEqual(c1["checkpoint"], {"messages": {}})
        self.assertFalse(attempt_of(w, c1["dispatch_id"])["usage_known"])
        # the file appears later: everything is still chargeable
        self.assertTrue(c2["usage"]["known"])
        self.assertEqual(tokens(c2, "input", "output"), (2, 10))
        # the totals say the run has an unknown charge
        usage = run_usage(w)
        self.assertEqual(usage["unknown_attempts"], 1)
        self.assertEqual(usage["by_role"]["implementer"]["unknown_charges"], 1)

    def test_totals_group_by_role_dispatch_and_source(self):
        w = claude_run(self)
        w.script(1, "implement", {"transcript": text(claude_line("m1", 10))})
        w.script(1, "review", {"transcript": text(claude_line("m9", 5))})
        drive(w, 2)
        impl, review = w.dispatch_ids()
        self.assertEqual(attempt_of(w, impl)["usage"]["output"], 10)
        self.assertEqual(attempt_of(w, review)["usage"]["output"], 5)
        by_role = run_usage(w)["by_role"]
        self.assertEqual({k: v["output"] for k, v in by_role.items()}, {"implementer": 10, "reviewer": 5})
        run_cli("report", w.run_dir, env=w.env, cwd=w.tmp).ok()
        report = (w.run_dir / "report.md").read_text(encoding="utf-8")
        by_vendor = report.split("By vendor:\n")[1].split("\n\n")[0].splitlines()
        self.assertEqual(len(by_vendor), 1, by_vendor)
        self.assertTrue(by_vendor[0].startswith("- claude: ") and by_vendor[0].endswith(" (0 input, 15 output tokens)"),
                        by_vendor)

    def test_crash_after_charge_before_attempt_keeps_exact_total(self):
        w = claude_run(self)
        w.script(1, "implement", {"transcript": text(claude_line("m1", 10, inp=3), claude_line("m2", 20, cached=5))})
        a = w.next()
        worker = w.launch(a)
        reply = w.claude_worker(a, worker)
        reply_file = Path(a["reply_file"])
        reply_file.parent.mkdir(parents=True, exist_ok=True)
        reply_file.write_text(reply, encoding="utf-8")
        args = ("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                "--reply-file", reply_file)
        crashed = w.cli(*args, crash="after_charge_before_attempt")
        self.assertEqual(crashed.code, 97, crashed)
        self.assertEqual(len(w.charges()), 1)
        self.assertEqual(w.attempts(), [])
        w.cli(*args).ok()
        charges, attempts = w.charges(), w.attempts()
        self.assertEqual(len(charges), 1)
        self.assertEqual(len(attempts), 1)
        self.assertEqual(attempts[0]["charge_ids"], [charges[0]["charge_id"]])
        self.assertEqual(tokens(charges[0]), (3, 5, 0, 0, 30))

    def test_repeated_charge_id_is_harmless(self):
        # The charge of a `record result` that crashed before its attempt record is the one the
        # retry finds, although the transcript has grown since; the growth is charged later.
        w = claude_run(self)
        w.script(1, "implement", {"status": "QUESTION", "transcript": text(claude_line("m1", 10))})
        a = w.next()
        worker = w.launch(a)
        reply_file = Path(a["reply_file"])
        reply_file.parent.mkdir(parents=True, exist_ok=True)
        reply_file.write_text(w.claude_worker(a, worker), encoding="utf-8")
        args = ("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                "--reply-file", reply_file)
        self.assertEqual(w.cli(*args, crash="after_charge_before_attempt").code, 97)
        [first] = w.charges()
        with open(w.subagent_transcript(worker), "a", encoding="utf-8") as f:
            f.write(text(claude_line("m2", 99)))
        w.cli(*args).ok()
        self.assertEqual(w.charges(), [first])
        self.assertEqual(first["usage"]["output"], 10)
        w.rulings[1] = ["A: JSON"]
        w.script(1, "answer", {"transcript": ""})
        drive(w, 1)
        _, later = w.charges()
        self.assertEqual(later["checkpoint"], {"messages": {"m2": 99}})
        self.assertEqual(later["usage"]["output"], 99)


class Locators(unittest.TestCase):
    """Where the transcripts are found: $CLAUDE_CONFIG_DIR/projects/<project>/<session>/subagents/
    agent-<worker id>.jsonl for a worker, $CLAUDE_CONFIG_DIR/projects/<project>/<session id>.jsonl
    for the supervisor."""

    def test_claude_transcript_locators(self):
        w = claude_run(self)
        projects = w.claude_home / "projects"
        sub = projects / "-some-project" / "sess-1" / "subagents" / "agent-abc123.jsonl"
        c = record_as(w, "abc123", sub, text(claude_line("w1", 7, inp=1)))
        self.assertTrue(c["usage"]["known"])
        self.assertEqual(tokens(c, "input", "output"), (1, 7))
        # The supervisor's session transcript, charged by `next`.
        (projects / "-some-project" / "sess-1.jsonl").write_text(text(claude_line("s1", 3, inp=2)), encoding="utf-8")
        w.cli("next", w.run_dir, env={"CLAUDE_CODE_SESSION_ID": "sess-1"}).ok()
        [s] = [c for c in w.charges() if c["key"] == "sess-1"]
        self.assertEqual((s["charge_id"], s["role"], s["dispatch_id"]),
                         ("supervisor:sess-1:1", "supervisor", "supervisor"))
        self.assertEqual(tokens(s, "input", "output"), (2, 3))
        # A session with no transcript is not charged.
        w.cli("next", w.run_dir, env={"CLAUDE_CODE_SESSION_ID": "nope"}).ok()
        self.assertEqual([c for c in w.charges() if c["key"] == "nope"], [])

    def test_wildcard_worker_id_finds_no_transcript(self):
        w = claude_run(self)
        other = w.claude_home / "projects" / "-some-project" / "sess-1" / "subagents" / "agent-abc123.jsonl"
        other.parent.mkdir(parents=True)
        other.write_text(text(claude_line("w1", 7)), encoding="utf-8")
        c = record_as(w, "*")
        self.assertFalse(c["usage"]["known"])
        self.assertEqual(c["checkpoint"], {"messages": {}})

    def test_relative_transcript_path_is_rejected(self):
        # A relative CLAUDE_CONFIG_DIR locates the transcript by a relative path, which the
        # charge refuses (ruling S1-R13: pinned as Python behaves).
        w = claude_run(self)
        a = w.next()
        did = a["dispatch_id"]
        w.cli("record", w.run_dir, "started", "--dispatch", did, "--worker", "abc").ok()
        transcript = w.claude_home / "projects" / "p" / "s" / "subagents" / "agent-abc.jsonl"
        transcript.parent.mkdir(parents=True)
        transcript.write_text(text(claude_line("m1", 5)), encoding="utf-8")
        reply = Path(a["reply_file"])
        reply.parent.mkdir(parents=True, exist_ok=True)
        reply.write_text(f"Status: READY\nReport: {a['report']}\n", encoding="utf-8")
        relative = w.claude_home.relative_to(w.tmp)          # "claude home": the cli runs in w.tmp
        self.assertIn(" ", str(relative))
        r = w.cli("record", w.run_dir, "result", "--dispatch", did, "--worker", "abc", "--reply-file", reply,
                  env={"CLAUDE_CONFIG_DIR": str(relative)})
        self.assertEqual(r.code, 1, r)
        self.assertIn(f"error: transcript path must be absolute: {relative}/projects/p/s/subagents/agent-abc.jsonl",
                      r.stderr.splitlines(), r)
        self.assertFalse(w.paths.charges.exists() and w.paths.charges.read_text(encoding="utf-8").strip(),
                         "a charge was appended")

    def test_missing_projects_dir_is_none(self):
        w = claude_run(self)
        self.assertFalse((w.claude_home / "projects").exists())
        c = record_as(w, "abc")
        self.assertFalse(c["usage"]["known"])
        self.assertIsNone(attempt_of(w, c["dispatch_id"])["cost_estimate"])


# --------------------------------------------------------------------------
# Prices

PRICES = {
    "opus": {"input": 15.0, "output": 75.0, "cache_read": 1.5, "cache_write_5m": 18.75, "cache_write_1h": 30.0},
    "haiku": {"input": 1.0, "output": 5.0, "cache_read": 0.1, "cache_write_5m": 1.25, "cache_write_1h": 2.0},
}
PRICES_TOML = "".join(f"[prices.{fam}]\n" + "".join(f"{k} = {v}\n" for k, v in p.items()) for fam, p in PRICES.items())
ONE_OPUS = (1000 * 15.0 + 1000 * 75.0) / 1e6           # 1000 input and 1000 output tokens of each
ONE_HAIKU = (1000 * 1.0 + 1000 * 5.0) / 1e6


def model_line(mid, model, n=1000):
    return claude_line(mid, n, inp=n, model=model)


class Pricing(unittest.TestCase):
    """The cost estimate of a Claude attempt (attempts.jsonl) and of the run (status.json), from
    the prices in the run's configuration: per model, and unknown rather than low."""

    def test_estimate_cost_opus_matches_recorded_session(self):
        w = claude_run(self, route="claude opus high")      # the presets' opus prices
        [c] = charge_worker(w, text(claude_line("m1", 1_301_069, inp=304_700, cached=366_728_180,
                                                w5=3_007_432, w1=4_080_700)))
        attempt = attempt_of(w, c["dispatch_id"])
        self.assertEqual(attempt["model"], "opus")
        self.assertAlmostEqual(attempt["cost_estimate"], 148.27, delta=0.01)
        self.assertAlmostEqual(run_usage(w)["estimated_usd"], 148.27, delta=0.01)

    def test_estimate_cost_unpriced_model_is_none(self):
        w = claude_run(self)                                # claude sonnet: no sonnet price below
        config = read_json(w.run_dir / "config.json")
        del config["prices"]["sonnet"]
        (w.run_dir / "config.json").write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
        w.script(1, "implement", {"transcript": text(claude_line("m1", 1000, inp=1000, model="claude-sonnet-4"))})
        w.script(1, "review", {"transcript": None})
        drive(w, 2)
        impl, review = (attempt_of(w, did) for did in w.dispatch_ids())
        self.assertEqual(impl["model"], "sonnet")
        self.assertTrue(impl["usage_known"])
        self.assertIsNone(impl["cost_estimate"])
        self.assertFalse(review["usage_known"])              # unknown usage: no estimate either
        self.assertIsNone(review["cost_estimate"])
        usage = run_usage(w)
        self.assertIsNone(usage["estimated_usd"])
        self.assertIsNone(usage["by_role"]["implementer"]["estimated_usd"])

    def test_mixed_batch_is_priced_per_model(self):
        w = claude_run(self, route="claude opus high", config=PRICES_TOML)
        w.script(1, "implement", {"transcript": text(model_line("m0", "claude-opus-4"),
                                                     model_line("m1", "claude-haiku-4"),
                                                     model_line("m2", "claude-haiku-4"))})
        # The order does not matter.
        w.script(1, "review", {"transcript": text(model_line("r0", "claude-haiku-4"), model_line("r1", "claude-opus-4"),
                                                  model_line("r2", "claude-haiku-4"))})
        drive(w, 2)
        expected = ONE_OPUS + 2 * ONE_HAIKU
        for did in w.dispatch_ids():
            attempt = attempt_of(w, did)
            self.assertEqual(tokens(attempt, "input", "output"), (3000, 3000))
            # The route's model (opus for the implementer) does not price the haiku parts.
            self.assertAlmostEqual(attempt["cost_estimate"], expected)
        self.assertEqual(attempt_of(w, w.dispatch_ids()[0])["model"], "opus")

    def test_single_model_and_empty_synthetic_records_stay_single(self):
        w = claude_run(self, route="claude opus high", config=PRICES_TOML)
        [c] = charge_worker(w, text(model_line("m0", "claude-opus-4"), claude_line("m1", 0, model="<synthetic>"),
                                    model_line("m2", "claude-opus-4")))
        self.assertEqual(c["usage"]["model"], "claude-opus-4")
        self.assertNotIn("by_model", c["usage"])
        self.assertAlmostEqual(attempt_of(w, c["dispatch_id"])["cost_estimate"], 2 * ONE_OPUS)
        self.assertAlmostEqual(run_usage(w)["estimated_usd"], 2 * ONE_OPUS)

    def test_unpriced_part_makes_the_estimate_unknown(self):
        w = claude_run(self, route="claude opus high", config=PRICES_TOML)
        [c] = charge_worker(w, text(model_line("m0", "claude-opus-4"), model_line("m1", "some-other-model", n=10)))
        attempt = attempt_of(w, c["dispatch_id"])
        self.assertTrue(attempt["usage_known"])
        self.assertIsNone(attempt["cost_estimate"])
        self.assertIsNone(run_usage(w)["estimated_usd"])

    def test_charge_keeps_the_per_model_parts(self):
        w = claude_run(self, route="claude opus high", config=PRICES_TOML)
        [c] = charge_worker(w, text(*(model_line(f"m{i}", model) for i, model in
                                      enumerate(("claude-opus-4", "claude-haiku-4", "claude-haiku-4")))))
        self.assertIsNone(c["usage"]["model"])
        parts = {m: (t["input"], t["output"]) for m, t in c["usage"]["by_model"].items()}
        self.assertEqual(parts, {"claude-opus-4": (1000, 1000), "claude-haiku-4": (2000, 2000)})
        self.assertAlmostEqual(run_usage(w)["estimated_usd"], ONE_OPUS + 2 * ONE_HAIKU)


# --------------------------------------------------------------------------
# Codex

class CodexCharges(unittest.TestCase):
    """charges.jsonl after `dispatch`: the session file first, then the `--json` stream, else
    unknown."""

    def test_codex_cumulative_takes_last_token_count_and_maps_fields(self):
        w = World(self)
        append(session_file(w, THREAD), json.dumps({"type": "session_meta"}),
               codex_line(100, 40, 10, 3),
               json.dumps({"type": "event_msg", "payload": {"type": "token_count", "info": None}}),
               codex_line(300, 100, 25, 8),
               json.dumps({"type": "event_msg", "payload": {"type": "agent_message"}}))
        c = codex_charge(w, "d1", THREAD, STREAM_TURN)
        # input_tokens includes the cached tokens, so uncached input = 300 - 100
        self.assertEqual(tokens(c, "input", "cached", "output", "reasoning"), (200, 100, 25, 8))
        self.assertTrue(c["usage"]["known"])
        self.assertEqual((c["charge_id"], c["source"], c["key"]), (f"d1:codex:{THREAD}", "codex", THREAD))
        attempt = attempt_of(w, "d1")
        self.assertEqual(attempt["usage"], c["usage"])
        self.assertTrue(attempt["usage_known"])
        self.assertIsNone(attempt["cost_estimate"])          # Codex models are unpriced

    def test_codex_cumulative_unknown_when_missing_or_without_token_count(self):
        w = World(self)
        missing = codex_charge(w, "d1", "22222222-2222-4222-8222-222222222222")
        self.assertFalse(missing["usage"]["known"])
        self.assertEqual(missing["source"], "codex")
        self.assertEqual(missing["checkpoint"], NOTHING)
        attempt = attempt_of(w, "d1")
        self.assertEqual((attempt["usage"], attempt["usage_known"], attempt["cost_estimate"]), (None, False, None))
        append(session_file(w, THREAD), json.dumps({"type": "session_meta"}))
        self.assertFalse(codex_charge(w, "d2", THREAD)["usage"]["known"])
        self.assertEqual(run_usage(w)["unknown_attempts"], 2)

    def test_charge_none_thread_is_unknown(self):
        w = World(self)
        c = codex_charge(w, "d1", THREAD, named=False)      # the stream never names its thread
        self.assertEqual((c["charge_id"], c["key"]), ("d1:codex:unknown", "dispatch:d1"))
        self.assertFalse(c["usage"]["known"])

    def test_charge_codex_cumulative_deltas(self):
        w = World(self)
        f = session_file(w, THREAD)
        append(f, codex_line(100, 0, 0))
        c1 = codex_charge(w, "d1", THREAD, STREAM_TURN)
        append(f, codex_line(250, 0, 0))
        c2 = codex_charge(w, "d2", THREAD, STREAM_TURN)
        self.assertEqual((c1["usage"]["input"], c2["usage"]["input"]), (100, 150))
        self.assertEqual(c2["checkpoint"]["cumulative"]["input"], 250)
        # interrupted: no new tokens -> zero delta
        c3 = codex_charge(w, "d3", THREAD, STREAM_TURN)
        self.assertEqual(c3["usage"]["input"], 0)
        self.assertTrue(c3["usage"]["known"])
        self.assertEqual(summed([c1, c2, c3])[0], 250)

    def test_codex_stream_fallback_charged_when_session_missing(self):
        w = World(self)
        c = codex_charge(w, "d1", "99999999-9999-4999-8999-999999999999", turn_completed(100, 30, 25, 8))
        self.assertEqual(c["source"], "codex-stream")
        self.assertEqual(tokens(c, "input", "cached", "output", "reasoning"), (70, 30, 25, 8))
        self.assertTrue(c["usage"]["known"])
        self.assertEqual(attempt_of(w, "d1")["usage"], c["usage"])
        self.assertEqual(run_usage(w)["unknown_attempts"], 0)
        # A stream charge moves no Codex checkpoint: a later session file is charged from its records.
        self.assertEqual(c["checkpoint"], NOTHING)
        # No completed turn either: an unknown Codex charge.
        u = codex_charge(w, "d2", "88888888-8888-4888-8888-888888888888")
        self.assertEqual((u["source"], u["usage"]["known"]), ("codex", False))
        # With a session file the stream is ignored.
        append(session_file(w, THREAD), codex_line(100, 0, 0))
        s = codex_charge(w, "d3", THREAD, turn_completed(100, 30, 25, 8))
        self.assertEqual((s["source"], s["usage"]["input"]), ("codex", 100))

    def test_codex_stream_usage_sums_turn_completed(self):
        w = World(self)
        c = codex_charge(w, "d1", THREAD, turn_completed(100, 30, 10, 4), {"type": "item.completed"},
                         turn_completed(50, 10, 5, 1))
        self.assertEqual(c["source"], "codex-stream")
        self.assertEqual(tokens(c, "input", "cached", "output", "reasoning"), (110, 40, 15, 5))
        self.assertTrue(c["usage"]["known"])

    def test_runner_crash_after_charge_keeps_the_charge(self):
        # The runner dies between its charge and its attempt record; the session file grows
        # before `dispatch` runs again. The finished generation's charge is the one already
        # made, and the next generation charges only what is new.
        w = World(self)
        lines = fixture_lines(ROLLOUT)
        f = place_rollout(w, THREAD, lines[:18])
        w.scenario(codex_steps(STREAM_TURN), thread=THREAD)
        crashed = run_cli(*w.args("d1"), env=dict(w.env, LEADHAND_CRASH_AT="after_charge_before_attempt"),
                          cwd=w.tmp)
        self.assertEqual(crashed.code, 1, crashed)
        [first] = w.charges()
        self.assertEqual(w.attempts(), [])
        f.write_text(text(*lines), encoding="utf-8")
        w.dispatch("d1").ok()
        again, resumed = w.charges()
        self.assertEqual(again, first)
        self.assertEqual(tokens(first, "input", "cached", "output"), (19062 - 7168, 7168, 58))
        self.assertEqual(resumed["charge_id"], f"d1:codex:{THREAD}:g2")
        self.assertEqual(tokens(resumed, "input", "cached", "output"), (20812 - 18944, 18944, 6))
        self.assertEqual([a["charge_ids"] for a in w.attempts()], [[first["charge_id"]], [resumed["charge_id"]]])
        self.assertEqual(summed([first, resumed]), (39874 - 26112, 26112, 0, 0, 64))


class CodexRollout(unittest.TestCase):
    """The real (sanitized) post-resume rollout: a killed turn (response 1, 19120 tokens) then a
    resumed turn (response 2, 20818). Its token_count total is 20818 and omits the killed turn;
    the token_usage_records sum to 39938."""

    def test_rollout_charges_every_response_not_the_token_count_total(self):
        w = World(self)
        place_rollout(w, THREAD, fixture_lines(ROLLOUT))
        c = codex_charge(w, "d1", THREAD, STREAM_TURN)
        self.assertTrue(c["usage"]["known"])
        # 39874 input of which 26112 cached; 64 output: 39938 tokens, not 20818
        self.assertEqual(tokens(c, "input", "cached", "output", "reasoning"), (39874 - 26112, 26112, 64, 0))
        self.assertEqual(c["usage"]["input"] + c["usage"]["cached"] + c["usage"]["output"], 39938)
        self.assertEqual(c["checkpoint"]["responses"], ["resp_fixture_1", "resp_fixture_2"])
        self.assertNotIn("mismatch", c)
        self.assertNotIn("possibly_incomplete", c)

    def test_rollout_charged_twice_charges_nothing_new(self):
        w = World(self)
        place_rollout(w, THREAD, fixture_lines(ROLLOUT))
        first = codex_charge(w, "d1", THREAD, STREAM_TURN)
        second = codex_charge(w, "d2", THREAD, STREAM_TURN)
        self.assertTrue(second["usage"]["known"])
        self.assertEqual(tokens(second, "input", "cached", "output"), (0, 0, 0))
        self.assertEqual(summed([first, second]), summed([first]))

    def test_rollout_truncated_before_resume_charges_only_the_killed_turn(self):
        w = World(self)
        place_rollout(w, THREAD, fixture_lines(ROLLOUT, 18))   # the killed turn: no token_count yet
        killed = codex_charge(w, "d1", THREAD, STREAM_TURN)
        self.assertTrue(killed["usage"]["known"])
        self.assertEqual(tokens(killed, "input", "cached", "output"), (19062 - 7168, 7168, 58))
        self.assertEqual(killed["checkpoint"]["responses"], ["resp_fixture_1"])
        # the resume appends to the same file: only the resumed turn is new
        place_rollout(w, THREAD, fixture_lines(ROLLOUT))
        resumed = codex_charge(w, "d2", THREAD, STREAM_TURN)
        self.assertEqual(tokens(resumed, "input", "cached", "output"), (20812 - 18944, 18944, 6))
        total = summed([killed, resumed])
        self.assertEqual((total[0], total[1], total[4]), (39874 - 26112, 26112, 64))

    def test_rollout_repeated_response_id_counts_once(self):
        w = World(self)
        lines = fixture_lines(ROLLOUT)
        place_rollout(w, THREAD, [*lines, lines[16]])           # the first token_usage_record, repeated
        c = codex_charge(w, "d1", THREAD, STREAM_TURN)
        self.assertEqual(tokens(c, "cached", "output"), (26112, 64))

    def test_rollout_thread_total_mismatch_is_recorded_not_fatal(self):
        w = World(self)
        lines = fixture_lines(ROLLOUT)
        rec = json.loads(lines[27])
        rec["payload"]["thread_token_usage"]["output_tokens"] = 1000
        lines[27] = json.dumps(rec)
        place_rollout(w, THREAD, lines)
        c = codex_charge(w, "d1", THREAD, STREAM_TURN)
        self.assertEqual(c["usage"]["output"], 64)               # still charged from the records
        self.assertEqual(c["mismatch"]["thread_token_usage"]["output"], 1000)
        self.assertEqual(c["mismatch"]["records"]["output"], 64)
        self.assertEqual(attempt_of(w, "d1")["status"], "READY")

    def test_possibly_incomplete_flag_is_recorded_on_the_charge(self):
        w = World(self)
        place_rollout(w, THREAD, fixture_lines(ROLLOUT, 18))
        c = codex_charge(w, "d1", THREAD, open_turn=True)       # it ends inside a turn
        self.assertIs(c["possibly_incomplete"], True)
        self.assertTrue(c["usage"]["known"])
        self.assertIs(attempt_of(w, "d1")["possibly_incomplete"], True)

    def test_rollout_without_any_usable_record_is_unknown(self):
        w = World(self)
        place_rollout(w, THREAD, fixture_lines(ROLLOUT, 3))
        self.assertFalse(codex_charge(w, "d1", THREAD)["usage"]["known"])

    def test_token_count_is_only_a_fallback_for_a_rollout_without_records(self):
        w = World(self)
        place_rollout(w, THREAD, [line for line in fixture_lines(ROLLOUT) if '"token_usage_record"' not in line])
        c = codex_charge(w, "d1", THREAD, STREAM_TURN)
        self.assertEqual(tokens(c, "input", "cached", "output"), (20812 - 18944, 18944, 6))


class CodexStream(unittest.TestCase):
    """Usage from the `--json` stream (a session file is missing): turn.completed summed."""

    def test_stream_usage_sums_turn_completed(self):
        w = World(self)
        w.scenario([json.loads(line) for line in fixture_lines("codex-exec-stream.jsonl")])
        w.dispatch("d1").ok()
        [c] = w.charges()
        self.assertEqual(c["source"], "codex-stream")
        self.assertTrue(c["usage"]["known"])
        self.assertEqual(tokens(c, "input", "cached", "output", "reasoning", "cache_write_5m"),
                         (137911 - 128640, 128640, 368, 0, 0))

    def test_stream_cache_write_tokens_are_kept_as_five_minute_writes(self):
        w = World(self)
        c = codex_charge(w, "d1", THREAD, turn_completed(100, 10, 3, 1, cache_write=5))
        self.assertEqual(tokens(c), (90, 10, 5, 0, 3))


# --------------------------------------------------------------------------
# Concurrency

class Concurrency(unittest.TestCase):
    def test_concurrent_supervisor_and_runner_charges_keep_exact_totals(self):
        """Two writers at once, each charging the supervisor's session (`next`) and a Codex
        thread (`dispatch`): A adds the session's messages, B the thread's usage."""
        n = 12
        w = World(self)
        sid = "concurrent-session"
        session = w.sup.claude_home / "projects" / PROJECT / f"{sid}.jsonl"
        session.parent.mkdir(parents=True)
        append(session, claude_line("seed", 3, inp=1))
        rollout = session_file(w, THREAD)
        append(rollout, codex_line(100, 20, 5))
        w.scenario(codex_steps(), thread=THREAD)
        env = dict(w.env, CLAUDE_CODE_SESSION_ID=sid)

        def charge_session():
            run_cli("next", w.run_dir, env=env, cwd=w.tmp).ok()

        def charge_thread(did):
            run_cli(*w.args(did), env=env, cwd=w.tmp).ok()

        def writer(who):
            for i in range(n):
                if who == "A":
                    append(session, claude_line(f"A-{i}", i + 1, inp=2, cached=7))
                    charge_session()
                    charge_thread(f"A{i}")
                else:
                    append(rollout, codex_line(100 + 10 * (i + 1), 20, 5 * (i + 2)))
                    charge_thread(f"B{i}")
                    charge_session()

        failures = []

        def run(who):
            try:
                writer(who)
            except BaseException as e:      # reported by the test thread
                failures.append(e)

        threads = [threading.Thread(target=run, args=(who,)) for who in ("A", "B")]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=300)
        self.assertFalse(any(t.is_alive() for t in threads), "a writer did not finish")
        if failures:
            raise failures[0]
        # the final charges pick up whatever the last writer added after the last charge
        charge_session()
        charge_thread("final")

        charges = w.charges()
        ids = [c["charge_id"] for c in charges]
        self.assertEqual(len(ids), len(set(ids)))
        codex = [c for c in charges if c["source"] == "codex"]
        claude = [c for c in charges if c["source"] == "claude"]
        self.assertEqual(len(codex), 2 * n + 1)
        self.assertEqual(len(claude) + len(codex), len(charges))
        self.assertTrue(all(c["key"] == sid and c["role"] == "supervisor" for c in claude))
        claude_total, codex_total = summed(claude), summed(codex)
        # Claude: seed + A's n messages (output i+1, input 2, cached 7)
        self.assertEqual(claude_total[4], 3 + sum(i + 1 for i in range(n)))
        self.assertEqual(claude_total[0], 1 + 2 * n)
        self.assertEqual(claude_total[1], 7 * n)
        # Codex: B's last cumulative is (100 + 10n, 20 cached, 5(n+1) output)
        self.assertEqual(codex_total[0], 100 + 10 * n - 20)
        self.assertEqual(codex_total[1], 20)
        self.assertEqual(codex_total[4], 5 * (n + 1))
        # the lock-protected file still parses cleanly line by line
        raw = w.run_dir.joinpath("charges.jsonl").read_text(encoding="utf-8")
        self.assertTrue(raw.endswith("\n"))
        self.assertEqual(len(raw.splitlines()), len(read_jsonl(w.run_dir / "charges.jsonl")))
        # and so does events.jsonl, its seqs 1..N in file order: one counter, under the lock
        raw = w.run_dir.joinpath("events.jsonl").read_text(encoding="utf-8")
        self.assertTrue(raw.endswith("\n"))
        seqs = [e["seq"] for e in read_jsonl(w.run_dir / "events.jsonl")]
        self.assertEqual(len(raw.splitlines()), len(seqs))
        self.assertEqual(seqs, list(range(1, len(seqs) + 1)))


if __name__ == "__main__":
    unittest.main()
