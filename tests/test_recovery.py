"""Recovery: reconciling after a relaunch, across every crash window; pause and cancel."""
import collections
import contextlib
import io
import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
import traceback
import unittest
from pathlib import Path
from unittest import mock

from fakeworld import FakeWorld, card
from gitfixture import run_git, write
from leadhand import dispatch as dsp
from leadhand import engine, launch, procs, status
from leadhand.state import CRASH_POINTS, append_event, fold_cards

RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
NOTE = "Use JSON; it is simpler."
CODEX_CARD = "codex gpt-6-luna low"
FIXED_DATE = "2026-01-01T00:00:00+00:00"
SKILL = Path(__file__).resolve().parents[1] / "skills" / "run" / "SKILL.md"


def resume(w, session="supervisor-2"):
    """What `leadhand run` does on a relaunch: a new supervisor session, then `resumed`."""
    w.session(session)
    append_event(w.paths, "resumed", run_id=w.meta()["run_id"])


def wait_for(check, timeout=30.0, what="condition"):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = check()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


def runner_record(w, dispatch_id) -> dict:
    try:
        return json.loads((w.run_dir / "workers" / dispatch_id / "runner.json").read_text())
    except (OSError, ValueError):
        return {}


def alive(rec) -> bool:
    return bool(rec) and procs.same_process(rec.get("pid"), rec.get("start"))


def codex_starts(w) -> collections.Counter:
    """Codex processes started per dispatch, from the fake codex log."""
    log = w.tmp / "codex log.jsonl"
    out = collections.Counter()
    if not log.exists():
        return out
    for line in log.read_text().splitlines():
        rec = json.loads(line)
        if rec.get("event") == "start":
            argv = rec["argv"]
            out[Path(argv[argv.index("-o") + 1]).parent.name] += 1
    return out


def scenario_file(w, name, lines) -> Path:
    path = w.tmp / f"scenario {name}.jsonl"
    path.write_text("".join(json.dumps(x) + "\n" for x in lines), encoding="utf-8")
    return path


def start_codex(w, a, lines):
    """Start the Codex proxy for dispatch action `a` in the background with a custom scenario."""
    argv, env = w.proxy_argv_env(a, {})
    env["FAKE_CODEX_SCENARIO"] = str(scenario_file(w, a["dispatch_id"], lines))
    p = subprocess.Popen(argv, env=env, cwd=w.tmp, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    w.proxies.append(p)
    return p


def kill_proxy(p):
    """The supervisor session died, and the proxy with it (the runner is detached)."""
    p.kill()
    p.communicate(timeout=30)


class Reconcile(unittest.TestCase):
    def test_restart_after_truncated_final_event_then_append(self):
        w = FakeWorld(self, [card(1), card(2)])
        w.start()
        w.act(w.next())
        with open(w.paths.events, "ab") as f:          # a crash in the middle of an append
            f.write(b'{"seq": 99, "ts": "2026-01-01T00:00:00Z", "kind": "accep')
        self.assertTrue(status.build_status(w.run_dir)["headline"])   # reading never fails
        resume(w)
        lines = w.paths.events.read_bytes().split(b"\n")
        self.assertEqual(lines[-1], b"")
        parsed = [json.loads(line) for line in lines[:-1]]
        self.assertEqual([e["seq"] for e in parsed], list(range(1, len(parsed) + 1)))
        self.assertEqual(parsed[-1]["kind"], "resumed")
        done = w.run()[-1]
        self.assertEqual(done["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2])
        (rec,) = w.events("reconciled")
        self.assertEqual(rec["resumed_seq"], parsed[-1]["seq"])

    def test_report_without_recorded_result_is_recorded(self):
        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        w.perform(a, worker)                     # report and commits written; the session died
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("dispatch", "c1-a1-review-1"))
        (res,) = w.events("result_recorded")
        self.assertEqual((res["dispatch_id"], res["worker_id"], res["status"], res["source"]),
                         ("c1-a1-implement-1", worker, "READY", "report"))
        self.assertFalse(res.get("interrupted"))
        self.assertIn(f"c1-a1-implement-1:claude:{worker}", [c.charge_id for c in w.charges()])
        (rec,) = w.events("reconciled")
        self.assertLess(rec["seq"], w.events("dispatch_requested")[-1]["seq"])
        self.assertEqual(status.build_status(w.run_dir)["headline"],
                         "Resumed at card 1; card 1's report was found and recorded")
        self.assertEqual(len([e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "implement"]), 1)

    def test_commits_without_report_continue_not_discard(self):
        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"crash": "after_commit"})
        a = w.next()
        worker = w.launch(a)
        self.assertIsNone(w.perform(a, worker))
        tip = w.branch_tip(CARD1)
        self.assertNotEqual(tip, w.base)
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["kind"], nxt["dispatch_id"], nxt["attempt"], nxt["resume"]),
                         ("dispatch", "continue", "c1-a1-continue-1", 1, None))
        self.assertEqual(w.branch_tip(CARD1), tip)             # nothing discarded
        brief = Path(nxt["brief"]).read_text()
        self.assertIn("continue from the branch as it stands", brief)
        self.assertIn("c1-a1-implement-1", brief)
        (res,) = w.events("result_recorded")
        self.assertEqual((res["dispatch_id"], res["interrupted"], res["tip"]), ("c1-a1-implement-1", True, tip))
        self.assertEqual(status.build_status(w.run_dir)["headline"],
                         "Resumed at card 1; card 1's commits were found and recorded")
        w.act(nxt)
        w.run()
        (acc,) = w.events("accepted")
        run_git(w.repo, "merge-base", "--is-ancestor", tip, acc["sha"])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-continue-1", "c1-a1-review-1"])

    def test_nothing_on_the_branch_requests_the_same_attempt_again(self):
        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)                              # the worker died before doing anything
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["kind"], nxt["dispatch_id"], nxt["attempt"], nxt["resume"]),
                         ("implement", "c1-a1-implement-2", 1, None))
        self.assertIn("card 1's implement worker was interrupted",
                      status.build_status(w.run_dir)["headline"])
        w.act(nxt)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_fix_round_resumes_with_a_fresh_worker_from_the_findings(self):
        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "review", {"verdict": "defects"})
        for _ in range(2):
            w.act(w.next())                      # implement, then review with defects
        fix = w.next()
        self.assertEqual((fix["kind"], fix["resume"] is not None), ("fix", True))
        resume(w)                                # the fix was requested, never started
        nxt = w.next()
        self.assertEqual((nxt["kind"], nxt["dispatch_id"], nxt["resume"]), ("fix", "c1-a1-fix-2", None))
        (rnd,) = w.events("fix_round")
        self.assertIn(rnd["findings"], Path(nxt["brief"]).read_text())
        w.act(nxt)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_acceptance_started_completes_after_restart(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        for _ in range(2):
            w.act(w.next())                      # implement, review (clean)
        r = w.cli("next", w.run_dir, crash="before_fast_forward")
        self.assertEqual(r.returncode, 97, r.stderr)
        self.assertEqual(w.branch_tip(RUN), w.base)
        resume(w)
        self.assertEqual(w.next()["action"], "done")
        (acc,) = w.events("accepted")
        self.assertEqual(w.branch_tip(RUN), acc["sha"])
        self.assertIn("card 1's acceptance was completed", w.events("reconciled")[0]["found"])

    def test_unmatched_state_fails_with_reason(self):
        w = FakeWorld(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        for _ in range(2):
            w.act(w.next())                      # card 1 implemented and reviewed
        a = w.next()                             # card 1 accepted; card 2 dispatched
        (acc,) = w.events("accepted")
        self.assertEqual(a["card"], 2)
        w.launch(a)
        # meanwhile someone moved the run branch back behind card 1
        run_git(w.repo, "update-ref", f"refs/heads/{RUN}", w.base)
        resume(w)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"]), ("done", "failed"))
        reason = f"state could not be reconciled: accepted work missing from the run branch: card 1 at {acc['sha'][:12]}"
        self.assertIn(reason, done["reasons"])
        self.assertEqual(len(w.events("dispatch_requested")), 3)      # nothing new was started
        self.assertIn(reason, w.paths.report.read_text())

    def test_missing_card_branch_fails_with_reason(self):
        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)
        ws = w.workspace()
        run_git(ws, "checkout", "--quiet", "--detach")
        run_git(ws, "branch", "-D", CARD1)
        resume(w)
        done = w.next()
        self.assertEqual(done["outcome"], "failed")
        self.assertTrue(any(f"card 1's branch {CARD1} is missing" in r for r in done["reasons"]), done["reasons"])

    def test_usage_after_interrupted_resume_equals_session_totals(self):
        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1", messages=2)
        w.start()
        w.script(1, "implement", {"crash": "before_commit"})
        a = w.next()
        worker = w.launch(a)
        self.assertIsNone(w.perform(a, worker))
        for _ in range(3):                       # the interrupted worker's own turns
            w._append_message(w.subagent_transcript(worker), "claude-sonnet-4")
        w.session("supervisor-1", messages=2)    # supervisor turns after its last charge
        resume(w)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        messages = sum(len(p.read_text().splitlines())
                       for p in (w.claude_home / "projects").rglob("*.jsonl"))
        charges = w.charges()
        self.assertTrue(all(c.usage.known for c in charges), charges)
        self.assertEqual((sum(c.usage.input for c in charges), sum(c.usage.output for c in charges)),
                         (100 * messages, 20 * messages))
        # charged once each: a second next charges nothing more
        w.next()
        self.assertEqual(len(w.charges()), len(charges))


class Codex(unittest.TestCase):
    def codex_world(self):
        w = FakeWorld(self, [card(1, route=CODEX_CARD)], installed=("claude", "codex"))
        w.session("supervisor-1")
        w.start()
        return w

    def test_live_codex_runner_is_waited_not_restarted(self):
        w = self.codex_world()
        a = w.next()
        w.launch(a)
        step = {}
        scenario = w.codex_scenario(a, step)
        lines = [json.loads(x) for x in scenario.read_text().splitlines()]
        lines.insert(2, {"action": "sleep", "seconds": 3})
        p = start_codex(w, a, lines)
        wait_for(lambda: alive(runner_record(w, a["dispatch_id"]).get("codex")), what="codex to start")
        kill_proxy(p)
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("wait", a["dispatch_id"]))
        self.assertEqual(len(w.events("dispatch_requested")), 1)
        (rec,) = w.events("reconciled")
        self.assertIn("card 1's Codex worker is still running; waiting for it", rec["found"])
        # no completion notice comes for it: the timer reports the result once the runner is done
        self.assertEqual(engine.timer(w.run_dir, a["dispatch_id"], interval=0.1), "result")
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("dispatch", "c1-a1-review-1"))
        (res,) = w.events("result_recorded")
        self.assertEqual((res["dispatch_id"], res["status"]), (a["dispatch_id"], "READY"))
        self.assertEqual(codex_starts(w), {a["dispatch_id"]: 1})

    def test_hung_runner_with_stale_heartbeat_is_cancelled_before_redispatch(self):
        w = self.codex_world()
        a = w.next()
        w.launch(a)
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "write_file", "path": "src/card1/partial.txt", "text": "partial\n"},
                 {"action": "sleep", "seconds": 120}]
        p = start_codex(w, a, lines)
        ws = w.workspace()
        wait_for(lambda: (ws / "src/card1/partial.txt").exists(), what="the partial file")
        rec = wait_for(lambda: runner_record(w, a["dispatch_id"]).get("codex") and runner_record(w, a["dispatch_id"]),
                       what="codex to start")
        kill_proxy(p)
        runner = rec["runner"]
        os.kill(runner["pid"], signal.SIGSTOP)             # hung: alive, heartbeat frozen
        beat = w.run_dir / "workers" / a["dispatch_id"] / "heartbeat"
        old = time.time() - 600
        os.utime(beat, (old, old))
        resume(w)
        with mock.patch.object(dsp, "CANCEL_ACK", 0.5), mock.patch.object(dsp, "HUNG_RECHECK", 0.3):
            nxt = w.next()
        self.assertFalse(alive(runner))
        self.assertFalse(alive(rec["codex"]))
        result = json.loads((w.run_dir / "workers" / a["dispatch_id"] / "result.json").read_text())
        self.assertEqual(result["exit_reason"], "cancelled")
        # the runner committed the partial work on the card branch; a worker continues from it
        run_git(w.repo, "cat-file", "-e", f"{CARD1}:src/card1/partial.txt")
        self.assertEqual((nxt["action"], nxt["kind"], nxt["dispatch_id"]), ("dispatch", "continue", "c1-a1-continue-1"))
        self.assertIn(f"--resume {result['session_id']}", nxt["message"])
        old_result = next(e for e in w.events("result_recorded") if e["dispatch_id"] == a["dispatch_id"])
        cont = next(e for e in w.events("dispatch_requested") if e["dispatch_id"] == "c1-a1-continue-1")
        self.assertLess(old_result["seq"], cont["seq"])
        self.assertTrue(old_result["interrupted"])
        self.assertEqual(codex_starts(w), {a["dispatch_id"]: 1})

    def test_dead_runner_without_result_is_finalized_and_dispatched_again(self):
        w = self.codex_world()
        a = w.next()
        w.launch(a)
        argv, env = w.proxy_argv_env(a, {})
        env["LEADHAND_CRASH_AT"] = "before_worker_launch"       # every runner it starts dies there
        r = subprocess.run(argv, env=env, cwd=w.tmp, stdin=subprocess.DEVNULL, capture_output=True,
                           text=True, timeout=120)
        self.assertNotEqual(r.returncode, 0, r.stdout)
        self.assertFalse((w.run_dir / "workers" / a["dispatch_id"] / "result.json").exists())
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["kind"], nxt["dispatch_id"]), ("implement", "c1-a1-implement-2"))
        self.assertTrue((w.run_dir / "workers" / a["dispatch_id"] / "result.json").exists())
        self.assertEqual(codex_starts(w), collections.Counter())


class PauseCancel(unittest.TestCase):
    def test_pause_ends_blocked_paused_with_handoff(self):
        w = FakeWorld(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        r = w.cli("pause", w.run_dir)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("pause requested", r.stdout)
        self.assertTrue(w.paths.pause_flag.exists())
        wait = w.next()
        self.assertEqual((wait["action"], wait["dispatch_id"]), ("wait", a["dispatch_id"]))
        engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                      reply=w.perform(a, worker))
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "blocked", "paused"))
        self.assertEqual(len(w.events("dispatch_requested")), 1)              # nothing new started
        self.assertEqual(len(w.events("paused")), 1)
        self.assertFalse(w.paths.pause_flag.exists())
        handoff = w.paths.handoff.read_text()
        self.assertIn("Outcome: blocked (paused)", handoff)
        self.assertIn("leadhand run", handoff)
        # a relaunch continues where it stopped
        resume(w)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2])

    def test_cancel_ends_incomplete_cancelled_and_keeps_card_branches(self):
        w = FakeWorld(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        ws = w.workspace()
        write(ws, {"src/card1/partial.txt": "partial\n"})       # the worker's partial work so far
        run_git(ws, "add", "--", "src/card1/partial.txt")
        run_git(ws, "commit", "--quiet", "-m", "partial")
        partial = w.branch_tip(CARD1)
        r = w.cli("cancel", w.run_dir)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("cancel requested", r.stdout)
        stop = w.next()
        self.assertEqual((stop["action"], stop["dispatch_id"], stop["worker_id"], stop["record_result"]),
                         ("stop_worker", a["dispatch_id"], worker, True))
        self.assertEqual(w.events("outcome"), [])               # not before the worker is stopped
        engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                      reply="Status: SPENT (time limit)", stopped=True)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "incomplete", "cancelled"))
        (out,) = w.events("outcome")
        (cancelled,) = w.events("cancelled")
        (res,) = w.events("result_recorded")
        self.assertLess(cancelled["seq"], res["seq"])
        self.assertLess(res["seq"], out["seq"])
        self.assertEqual(w.branch_tip(CARD1), partial)          # partial work stays on the card branch
        self.assertEqual(w.branch_tip(RUN), w.base)
        self.assertEqual(len(w.events("dispatch_requested")), 1)
        self.assertFalse(w.paths.cancel_flag.exists())
        # a relaunch cannot undo it
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = launch.run(w.run_dir, claude_bin="no-such-claude-for-leadhand-tests")
        self.assertEqual(code, 1)
        self.assertIn("this run has ended: incomplete (cancelled)", err.getvalue())
        self.assertEqual(w.events("resumed"), [])

    def test_cancel_survives_a_relaunch_before_it_finished(self):
        w = FakeWorld(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)
        w.cli("cancel", w.run_dir)
        self.assertEqual(w.next()["action"], "stop_worker")      # the session died before stopping it
        resume(w)
        done = w.next()                          # the dead session's worker needs no stop
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        self.assertEqual(len(w.events("dispatch_requested")), 1)

    def test_cancel_confirms_detached_codex_stopped_before_outcome(self):
        w = FakeWorld(self, [card(1, route=CODEX_CARD)], installed=("claude", "codex"))
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "write_file", "path": "src/card1/started.txt", "text": "started\n"},
                 {"action": "append_loop", "path": "src/card1/loop.txt", "interval": 0.05},
                 {"action": "tool_child", "seconds": 120},
                 {"action": "sleep", "seconds": 120}]
        p = start_codex(w, a, lines)
        ws = w.workspace()
        loop = ws / "src/card1/loop.txt"
        wait_for(lambda: loop.exists() and loop.stat().st_size > 0, what="the worker writing")
        wait_for(lambda: '"child"' in (w.tmp / "codex log.jsonl").read_text(), what="the tool child")
        rec = wait_for(lambda: runner_record(w, a["dispatch_id"]).get("codex") and runner_record(w, a["dispatch_id"]),
                       what="codex to start")
        r = w.cli("cancel", w.run_dir)
        self.assertEqual(r.returncode, 0, r.stderr)
        done = w.next()
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        children = [json.loads(x)["child_pid"] for x in (w.tmp / "codex log.jsonl").read_text().splitlines()
                    if json.loads(x).get("event") == "child"]
        self.assertTrue(children)
        for who in (rec["runner"], rec["codex"]):
            self.assertFalse(alive(who), who)
        for pid in children:
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
        size = loop.stat().st_size
        time.sleep(0.5)
        self.assertEqual(loop.stat().st_size, size)              # no workspace write after the outcome
        (out,) = w.events("outcome")
        (res,) = w.events("result_recorded")
        self.assertLess(res["seq"], out["seq"])
        self.assertTrue(res["stopped"])
        p.communicate(timeout=60)

    def test_crash_after_cancel_request_finishes_the_cancel_on_restart(self):
        w = FakeWorld(self, [card(1, route=CODEX_CARD)], installed=("claude", "codex"))
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "sleep", "seconds": 120}]
        p = start_codex(w, a, lines)
        rec = wait_for(lambda: runner_record(w, a["dispatch_id"]).get("codex") and runner_record(w, a["dispatch_id"]),
                       what="codex to start")
        w.cli("cancel", w.run_dir)
        r = w.cli("next", w.run_dir, crash="after_cancel_request")
        self.assertEqual(r.returncode, 97, r.stderr)
        self.assertEqual(w.events("outcome"), [])
        kill_proxy(p)
        resume(w)
        done = w.next()
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        for who in (rec["runner"], rec["codex"]):
            self.assertFalse(alive(who), who)
        (res,) = w.events("result_recorded")
        self.assertLess(res["seq"], w.events("outcome")[0]["seq"])

    def test_relaunch_refuses_a_finished_run(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = launch.run(w.run_dir, claude_bin="no-such-claude-for-leadhand-tests")
        self.assertEqual(code, 1)
        self.assertIn("this run has ended: complete", err.getvalue())
        self.assertEqual(w.events("resumed"), [])
        r = w.cli("cancel", w.run_dir)
        self.assertEqual(r.returncode, 1)
        self.assertIn("this run has ended: complete", r.stderr)
        self.assertFalse(w.paths.cancel_flag.exists())

    def test_run_skill_steers_with_plain_commands(self):
        text = SKILL.read_text(encoding="utf-8")
        steering = text[text.index("## Steering"):]
        self.assertIn("to pause, run `<cli> pause <run_dir>`; to cancel, `<cli> cancel <run_dir>`", steering)
        self.assertIn("`<cli> unpark <run_dir> --card N --note '…'`", steering)


# --------------------------------------------------------------------------
# Every crash window

class StableWorld(FakeWorld):
    """Three cards and the end-of-run gates. Card 1 asks a product question and is parked; once
    card 2 is accepted the operator unparks it, and it is integrated onto the moved run tip.
    Card 2 runs on Codex and has one fix round. Card 3 ends SPENT and is escalated.

    A worker's work is keyed to its card, attempt and step (not its dispatch ID), and commit
    dates are fixed, so a step re-run under a new dispatch ID makes the same commit and every
    run of the deck accepts the same SHAs."""

    CARDS = (1, 2, 3)

    def __init__(self, test):
        super().__init__(test, [card(1), card(2, route=CODEX_CARD), card(3)],
                         config='preset = "claude-only"\n', installed=("claude", "codex"), gates=True)
        self.parks[1] = "product decision: storage format"

    def requested(self, dispatch_id) -> dict:
        return next(e for e in self.events("dispatch_requested") if e["dispatch_id"] == dispatch_id)

    def label(self, dispatch_id) -> str:
        e = self.requested(dispatch_id)
        kind, n = e["dispatch_kind"], e["card"]
        if kind == "continue" and e.get("continues"):
            return self.label(e["continues"])
        if n is None:
            return f"run-w{e.get('wave')}-{kind}"
        unparked = any(u["card"] == n and u["seq"] < e["seq"] for u in self.events("unparked"))
        if kind in ("implement", "answer") and unparked:
            return f"c{n}-a{e['attempt']}-unparked"
        return f"c{n}-a{e['attempt']}-{kind}"

    def work_label(self, a):
        return self.label(a["dispatch_id"])

    def _take(self, a):
        label = self.label(a["dispatch_id"])
        if label == "c1-a1-implement":
            return {"status": "QUESTION"}
        if label == "c3-a1-implement":
            return {"status": "SPENT"}
        if label == "c2-a1-review":
            return {"verdict": "defects"}
        return {}

    def operator(self):
        cards = fold_cards(self.events(), list(self.CARDS))
        if cards[1].state == "parked" and cards[2].state == "accepted" and not self.events("unparked"):
            engine.unpark(self.run_dir, card=1, note=NOTE)

    def drive(self, limit=200) -> dict:
        for _ in range(limit):
            self.operator()
            a = self.next()
            if a["action"] == "done":
                return a
            if a["action"] == "wait":
                time.sleep(0.1)
                continue
            self.act(a)
        raise AssertionError("the run did not finish")

    def wait_for_orphans(self, timeout=30.0):
        """A runner killed by the crash may leave Codex running; a real restart comes later."""
        end = time.monotonic() + timeout
        for runner in self.run_dir.glob("workers/*/runner.json"):
            rec = json.loads(runner.read_text()).get("codex")
            while alive(rec) and time.monotonic() < end:
                time.sleep(0.05)

    def outcome(self) -> dict:
        cards = fold_cards(self.events(), list(self.CARDS))
        out = self.events("outcome")[-1]
        return {"outcome": (out["outcome"], out["qualifier"]),
                "cards": {n: c.state for n, c in cards.items()},
                "accepted": {e["card"]: e["sha"] for e in self.events("accepted")},
                "head": self.branch_tip(RUN)}

    def check_invariants(self, test):
        started = collections.Counter(e["dispatch_id"] for e in self.events("worker_started"))
        test.assertEqual([d for d, k in started.items() if k > 1], [], "a dispatch recorded two workers")
        launched = collections.Counter(self.launch_log.read_text().split())
        test.assertEqual([d for d, k in launched.items() if k > 1], [], "a dispatch was launched twice")
        test.assertEqual([d for d, k in codex_starts(self).items() if k > 1], [], "Codex started twice")
        fsck = run_git(self.repo, "fsck", "--unreachable", "--no-reflogs", "--no-progress")
        test.assertEqual([ln for ln in fsck.splitlines() if ln.startswith("unreachable commit")], [],
                         "a commit was lost")


def crash_child(w, point, skip, counter) -> int:
    """Run the world in a child process that is its own process group, as the supervisor
    session is; the crash point kills the whole group (with its Claude workers and Codex
    proxies) at the (skip+1)-th time it is passed. Returns the wait status."""
    errors = w.tmp / "child error.txt"
    pid = os.fork()
    if pid == 0:
        code = 1
        try:
            os.setpgid(0, 0)
            os.environ.update({"LEADHAND_CRASH_AT": point, "LEADHAND_CRASH_SKIP": str(skip),
                               "LEADHAND_CRASH_COUNTER": str(counter),
                               "LEADHAND_CRASH_KILL_PGID": str(os.getpid())})
            w.drive()
            code = 0
        except BaseException:
            errors.write_text(traceback.format_exc())
        finally:
            os._exit(code)
    end = time.monotonic() + 180
    while True:
        done, status_ = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        if time.monotonic() > end:
            os.killpg(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            raise AssertionError(f"{point} #{skip + 1}: the crashed run hung")
        time.sleep(0.02)
    if errors.exists():
        raise AssertionError(f"{point} #{skip + 1}: {errors.read_text()}")
    return status_


class EveryCrashPoint(unittest.TestCase):
    """Crash at every occurrence of every crash point, restart, and reach the uninterrupted
    run's final state. "The end" is the run's outcome with its report (the world runs the
    end-of-run gates, so `before_gate_record` and the gate recovery are covered too). The
    crash runs are independent, so they run in forked workers, a few at a time."""

    POINTS = (*CRASH_POINTS, "before_commit", "after_commit")
    PARALLEL = max(1, min(4, os.cpu_count() or 1))
    RUN_TIMEOUT = 300.0

    def stable_world(self):
        dates = mock.patch.dict(os.environ, {"GIT_AUTHOR_DATE": FIXED_DATE, "GIT_COMMITTER_DATE": FIXED_DATE})
        dates.start()
        self.addCleanup(dates.stop)
        w = StableWorld(self)
        w.launch_log = w.tmp / "launches.txt"
        w.launch_log.touch()
        w.session("supervisor-1")
        w.start()
        return w

    def crash_run(self, point, k, expected):
        """One crash run (in a forked worker): crash at the (k+1)-th pass of `point`, leave the
        mismatch, relaunch without the crash, run to the end, compare."""
        w = self.stable_world()
        try:
            counter = w.tmp / "crash counter"
            status_ = crash_child(w, point, k, counter)
            self.assertTrue(os.WIFSIGNALED(status_) or os.WEXITSTATUS(status_) == 97,
                            f"{point} #{k + 1} did not crash (status {status_})")
            self.assertEqual(counter.read_text().strip(), str(k + 1))
            w.wait_for_orphans()
            resume(w)
            w.drive()
            self.assertEqual(w.outcome(), expected)
            w.check_invariants(self)
            self.assertTrue(w.events("reconciled"))
        finally:
            w.kill_leftovers()
            shutil.rmtree(w.tmp, ignore_errors=True)

    def test_restart_from_every_crash_point_reaches_same_final_state(self):
        t0 = time.monotonic()
        ref = self.stable_world()
        log = ref.tmp / "crash points.txt"
        with mock.patch.dict(os.environ, {"LEADHAND_CRASH_LOG": str(log)}):
            ref.drive()
        expected = ref.outcome()
        self.assertEqual(expected["outcome"], ("complete", ""))
        self.assertEqual(set(expected["cards"].values()), {"accepted"})
        self.assertEqual(sorted(expected["accepted"]), [1, 2, 3])
        self.assertTrue(ref.events("integration_started") and ref.events("fix_round"))
        self.assertTrue(any(d["attempt"] == 2 for d in ref.started if d["card"] == 3))
        ref.check_invariants(self)
        seen = collections.Counter(log.read_text().split())
        # Every crash point is on this run's path except a cancel's (covered by the cancel tests).
        self.assertEqual({p for p in self.POINTS if not seen[p]}, {"after_cancel_request"}, seen)

        todo = [(point, k) for point in self.POINTS for k in range(seen[point])]
        results_dir = Path(tempfile.mkdtemp(prefix="lh crash results "))
        self.addCleanup(shutil.rmtree, results_dir, True)
        running: dict[int, tuple[str, int, float]] = {}
        results: dict[tuple[str, int], str] = {}
        while todo or running:
            while todo and len(running) < self.PARALLEL:
                point, k = todo.pop(0)
                out = results_dir / f"{point}-{k}.txt"
                pid = os.fork()
                if pid == 0:
                    code = 1
                    try:
                        self.crash_run(point, k, expected)
                        out.write_text("ok")
                        code = 0
                    except BaseException:
                        out.write_text(traceback.format_exc())
                    finally:
                        os._exit(code)
                running[pid] = (point, k, time.monotonic())
            for pid, (point, k, began) in list(running.items()):
                done, _ = os.waitpid(pid, os.WNOHANG)
                if not done and time.monotonic() - began > self.RUN_TIMEOUT:
                    os.kill(pid, signal.SIGKILL)
                    os.waitpid(pid, 0)
                    results[(point, k)] = f"timed out after {self.RUN_TIMEOUT:.0f}s"
                    del running[pid]
                elif done:
                    out = results_dir / f"{point}-{k}.txt"
                    results[(point, k)] = out.read_text() if out.exists() else "the worker died without a result"
                    del running[pid]
            time.sleep(0.05)
        for (point, k), text in sorted(results.items(), key=lambda x: (self.POINTS.index(x[0][0]), x[0][1])):
            with self.subTest(point=point, occurrence=k + 1):
                self.assertEqual(text, "ok", f"{point} #{k + 1}:\n{text}")
        self.assertEqual(len(results), sum(seen[p] for p in self.POINTS))
        print(f"\n[crash matrix: {len(results)} crash runs, {time.monotonic() - t0:.0f}s, "
              f"{self.PARALLEL} at a time]", flush=True)


if __name__ == "__main__":
    unittest.main()
