#!/usr/bin/env python3
"""Recovery after a relaunch, pause and cancel, and the crash matrix: every crash point at every
occurrence, a restart, and the same final state.

Converted from tests/test_recovery.py and tests/test_engine_recovery.py. FakeSupervisor plays
the run skill. Where a Python test appended `resumed` itself (its `resume`), the run is
relaunched with `run` (tests/fakes/claude as Claude, which exits at once), which records it.
Where it called an engine function, this calls the command that does it (`record`, `unpark`,
`start`, `timer`, `run`); an EngineError is exit code 1 with `error: ...` on stderr. A crash
point is LEADHAND_CRASH_AT on the command, which then exits 97.

Recovery is judged from Git, the filesystem and the fakes' logs, beside the binary's events:
whether a Codex process or its tool child is gone comes from the fake Codex's log (its pid and
the time of the line naming it); a runner is identified by the record in runner.json once
this test has seen that pid alive with that start time. Branches, commits and trees are read
from Git. Waits poll a condition with a bound; a check that something does not happen watches
for it over a stated window.

The branch release (deliberate change 2, ruling S1-R15): a run that ends complete, incomplete
or failed may record more events after `outcome` and detaches its worktree, so these tests
check the events they care about, never a complete list, and branch tips, never which branch
the worktree has checked out.
"""
import collections
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import traceback
import unittest
from pathlib import Path
from unittest import mock

import harness
from contract_cards import codex_world, recorded, relaunch_env, result, run_line, status_lines
from contract_dispatch import holds, logged_alive, wait_for
from gitfixture import commit, run_git, write
from harness import read_json, read_jsonl, same_process, started_by
from rundir import append_event
from supervisor import NO_HOOKS, FakeSupervisor, card, kill_run_leftovers

RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
NOTE = "Use JSON; it is simpler."
CODEX_CARD = "codex gpt-6-luna low"
FIXED_DATE = "2026-01-01T00:00:00+00:00"
SPENT = "Status: SPENT (time limit)"


# --------------------------------------------------------------------------
# Helpers

def relaunch(w: FakeSupervisor) -> harness.Result:
    """`run` on the run directory, with tests/fakes/claude as Claude."""
    return w.cli("run", w.run_dir, env=relaunch_env())


def resume(w: FakeSupervisor, session: str = "supervisor-2") -> None:
    """What a relaunch does: a new supervisor session, then `run`, which records `resumed`."""
    w.session(session)
    relaunch(w).ok()


def runner_record(w: FakeSupervisor, dispatch_id: str) -> dict:
    try:
        return json.loads((w.run_dir / "workers" / dispatch_id / "runner.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def alive(rec) -> bool:
    return bool(rec) and same_process(rec.get("pid"), rec.get("start"))


def codex_log(w: FakeSupervisor, event: str | None = None) -> list[dict]:
    """The fake Codex's log lines (of one `event`)."""
    log = Path(w.env["FAKE_CODEX_LOG"])
    lines = read_jsonl(log) if log.exists() else []
    return [r for r in lines if event is None or r.get("event") == event]


def codex_starts(w: FakeSupervisor) -> collections.Counter:
    """Codex processes started per dispatch, from the fake Codex's log."""
    out = collections.Counter()
    for rec in codex_log(w, "start"):
        argv = rec["argv"]
        out[Path(argv[argv.index("-o") + 1]).parent.name] += 1
    return out


def codex_alive(rec: dict) -> bool:
    """Whether the Codex a `start` log line names is still that process."""
    return started_by(rec["pid"], rec["time"]) is not None


def scenario_file(w: FakeSupervisor, name: str, lines: list[dict]) -> Path:
    path = w.tmp / f"scenario {name}.jsonl"
    path.write_text("".join(json.dumps(x) + "\n" for x in lines), encoding="utf-8")
    return path


def start_codex(w: FakeSupervisor, a: dict, lines: list[dict]) -> subprocess.Popen:
    """Run dispatch action `a`'s own `dispatch` line in the background, the fake Codex playing
    `lines`: the proxy, which the test kills when the supervisor session dies."""
    argv, env = w.proxy_argv_env(a, {})
    env["FAKE_CODEX_SCENARIO"] = str(scenario_file(w, a["dispatch_id"], lines))
    p = subprocess.Popen(argv, env=env, cwd=w.tmp, stdin=subprocess.DEVNULL,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    w.proxies.append(p)
    return p


def kill_proxy(p: subprocess.Popen) -> None:
    """The supervisor session died, and the proxy with it (the runner is detached)."""
    p.kill()
    p.communicate(timeout=30)


def running(test: unittest.TestCase, w: FakeSupervisor, a: dict, proc=None) -> dict:
    """runner.json once it names Codex; its runner is checked here to be that live process, so
    a later `alive(...)` of it is judged against this test's own sighting."""
    rec = wait_for(lambda: (r := runner_record(w, a["dispatch_id"])).get("codex") and r,
                   "Codex to start", proc=proc)
    test.assertTrue(alive(rec["runner"]), rec)
    return rec


def first_codex(w: FakeSupervisor) -> dict:
    """The fake Codex's first `start` line, once it is there."""
    return wait_for(lambda: next(iter(codex_log(w, "start")), None), "the fake Codex to start")


def signal_if_alive(rec: dict, sig: int) -> None:
    if alive(rec):
        try:
            os.kill(rec["pid"], sig)
        except ProcessLookupError:
            pass


def git_log_body(repo: Path, ref: str) -> str:
    return run_git(repo, "log", "-1", "--format=%B", ref)


def fails(test: unittest.TestCase, r: harness.Result, *needles: str) -> None:
    """A refusal: exit 1, `error: ...` on stderr, naming each of `needles`."""
    test.assertEqual(r.code, 1, r)
    test.assertTrue(r.stderr.startswith("error: "), r)
    for needle in needles:
        test.assertIn(needle, r.stderr, r)


def card_states(w: FakeSupervisor, numbers) -> dict:
    """Each card's state, from `status RUN_DIR`'s card lines (`Card N: <title> · <state> · ...`)."""
    lines = status_lines(w)
    out = {}
    for n in numbers:
        prefix = f"Card {n}: Card number {n} · "
        line = next((ln for ln in lines if ln.startswith(prefix)), None)
        if line is None:
            raise AssertionError(f"no line for card {n} in status:\n" + "\n".join(lines))
        out[n] = line[len(prefix):].split(" · ")[0]
    return out


# --------------------------------------------------------------------------
# test_recovery.py

class Reconcile(unittest.TestCase):
    def test_restart_after_truncated_final_event_then_append(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.start()
        w.act(w.next())
        with open(w.paths.events, "ab") as f:          # a crash in the middle of an append
            f.write(b'{"seq": 99, "ts": "2026-01-01T00:00:00Z", "kind": "accep')
        self.assertTrue(status_lines(w)[0])             # reading never fails
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

    def test_appends_repair_a_torn_last_line_in_any_record_file(self):
        # A crash in the middle of an append leaves a partial last line: in charges.jsonl after one
        # whole record, and as the whole of attempts.jsonl. The next append drops it and stays
        # line-aligned (test_state.py).
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.next()                                        # charges the supervisor's session
        whole = w.paths.charges.read_bytes()
        self.assertTrue(whole.endswith(b"\n") and whole.count(b"\n") == 1, whole)
        self.assertFalse(w.paths.attempts.exists())
        with open(w.paths.charges, "ab") as f:
            f.write(b'{"n": 2')
        w.paths.attempts.write_bytes(b'{"partial": "attem')
        a = w.next()
        w.act(a)
        records = {}
        for path in (w.paths.charges, w.paths.attempts):
            raw = path.read_bytes()
            self.assertTrue(raw.endswith(b"\n"), raw)
            records[path.name] = [json.loads(line) for line in raw.split(b"\n")[:-1]]
        self.assertTrue(w.paths.charges.read_bytes().startswith(whole))
        charges = records["charges.jsonl"]
        self.assertGreater(len(charges), 1)
        self.assertNotIn("n", [k for c in charges for k in c])
        self.assertEqual([x["dispatch"] for x in records["attempts.jsonl"]], [a["dispatch_id"]])

    def test_report_without_recorded_result_is_recorded(self):
        w = FakeSupervisor(self, [card(1)])
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
        self.assertIn(f"c1-a1-implement-1:claude:{worker}", [c["charge_id"] for c in w.charges()])
        (rec,) = w.events("reconciled")
        self.assertLess(rec["seq"], w.events("dispatch_requested")[-1]["seq"])
        self.assertEqual(status_lines(w)[0], "Resumed at card 1; card 1's report was found and recorded")
        self.assertEqual(len([e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "implement"]), 1)

    def test_commits_without_report_continue_not_discard(self):
        w = FakeSupervisor(self, [card(1)])
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
        brief = Path(nxt["brief"]).read_text(encoding="utf-8")
        self.assertIn("continue from the branch as it stands", brief)
        self.assertIn("c1-a1-implement-1", brief)
        (res,) = w.events("result_recorded")
        self.assertEqual((res["dispatch_id"], res["interrupted"], res["tip"]), ("c1-a1-implement-1", True, tip))
        self.assertEqual(status_lines(w)[0], "Resumed at card 1; card 1's commits were found and recorded")
        w.act(nxt)
        w.run()
        (acc,) = w.events("accepted")
        run_git(w.repo, "merge-base", "--is-ancestor", tip, acc["sha"])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-continue-1", "c1-a1-review-1"])

    def test_nothing_on_the_branch_requests_the_same_attempt_again(self):
        """Covers P9: a Claude worker without a recorded result counts as interrupted."""
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)                              # the worker died before doing anything
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["kind"], nxt["dispatch_id"], nxt["attempt"], nxt["resume"]),
                         ("implement", "c1-a1-implement-2", 1, None))
        self.assertIn("card 1's implement worker was interrupted", status_lines(w)[0])
        w.act(nxt)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_fix_round_resumes_with_a_fresh_worker_from_the_findings(self):
        w = FakeSupervisor(self, [card(1)])
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
        self.assertIn(rnd["findings"], Path(nxt["brief"]).read_text(encoding="utf-8"))
        w.act(nxt)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_acceptance_started_completes_after_restart(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        for _ in range(2):
            w.act(w.next())                      # implement, review (clean)
        r = w.cli("next", w.run_dir, crash="before_fast_forward")
        self.assertEqual(r.code, 97, r)
        self.assertEqual(w.branch_tip(RUN), w.base)
        resume(w)
        self.assertEqual(w.next()["action"], "done")
        (acc,) = w.events("accepted")
        self.assertEqual(w.branch_tip(RUN), acc["sha"])
        self.assertIn("card 1's acceptance was completed", w.events("reconciled")[0]["found"])

    def test_unmatched_state_fails_with_reason(self):
        w = FakeSupervisor(self, [card(1), card(2)])
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
        self.assertIn(reason, w.paths.report.read_text(encoding="utf-8"))

    def test_missing_card_branch_fails_with_reason(self):
        w = FakeSupervisor(self, [card(1)])
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
        w = FakeSupervisor(self, [card(1)])
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
        messages = sum(len(p.read_text(encoding="utf-8").splitlines())
                       for p in (w.claude_home / "projects").rglob("*.jsonl"))
        charges = w.charges()
        self.assertTrue(all(c["usage"]["known"] for c in charges), charges)
        self.assertEqual((sum(c["usage"]["input"] for c in charges), sum(c["usage"]["output"] for c in charges)),
                         (100 * messages, 20 * messages))
        # charged once each: a second next charges nothing more
        w.next()
        self.assertEqual(len(w.charges()), len(charges))


class Codex(unittest.TestCase):
    def codex_world(self) -> FakeSupervisor:
        w = codex_world(self, [card(1, route=CODEX_CARD)])
        w.session("supervisor-1")
        w.start()
        return w

    def test_live_codex_runner_is_waited_not_restarted(self):
        w = self.codex_world()
        a = w.next()
        w.launch(a)
        lines = [json.loads(x) for x in w.codex_scenario(a, {}).read_text(encoding="utf-8").splitlines()]
        lines.insert(2, {"action": "sleep", "seconds": 3})
        p = start_codex(w, a, lines)
        running(self, w, a, p)
        kill_proxy(p)
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("wait", a["dispatch_id"]))
        self.assertEqual(len(w.events("dispatch_requested")), 1)
        (rec,) = w.events("reconciled")
        self.assertIn("card 1's Codex worker is still running; waiting for it", rec["found"])
        # no completion notice comes for it: the timer reports the result once the runner is done
        r = w.cli("timer", w.run_dir, "--dispatch", a["dispatch_id"]).ok()
        self.assertEqual(r.stdout.strip(), "result recorded")
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
        wait_for(lambda: (ws / "src/card1/partial.txt").exists(), "the partial file", proc=p)
        rec = running(self, w, a, p)
        codex = first_codex(w)
        kill_proxy(p)
        runner = rec["runner"]
        signal_if_alive(runner, signal.SIGSTOP)            # hung: alive, heartbeat frozen
        self.addCleanup(signal_if_alive, runner, signal.SIGCONT)
        beat = w.run_dir / "workers" / a["dispatch_id"] / "heartbeat"
        old = time.time() - 600
        os.utime(beat, (old, old))
        resume(w)
        nxt = w.next()                                     # the real 5 s acknowledgement and recheck
        self.assertFalse(alive(runner))
        self.assertFalse(codex_alive(codex))
        self.assertFalse(alive(rec["codex"]))
        result_json = read_json(w.run_dir / "workers" / a["dispatch_id"] / "result.json")
        self.assertEqual(result_json["exit_reason"], "cancelled")
        # the runner committed the partial work on the card branch; a worker continues from it
        run_git(w.repo, "cat-file", "-e", f"{CARD1}:src/card1/partial.txt")
        self.assertEqual((nxt["action"], nxt["kind"], nxt["dispatch_id"]), ("dispatch", "continue", "c1-a1-continue-1"))
        self.assertIn(f"--resume {result_json['session_id']}", nxt["message"])
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
        r = harness.run_argv(argv, env=env, cwd=w.tmp, timeout=120)
        self.assertNotEqual(r.code, 0, r)
        self.assertFalse((w.run_dir / "workers" / a["dispatch_id"] / "result.json").exists())
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["kind"], nxt["dispatch_id"]), ("implement", "c1-a1-implement-2"))
        self.assertTrue((w.run_dir / "workers" / a["dispatch_id"] / "result.json").exists())
        self.assertEqual(codex_starts(w), collections.Counter())


class PauseCancel(unittest.TestCase):
    def test_pause_ends_blocked_paused_with_handoff(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        r = w.cli("pause", w.run_dir).ok()
        self.assertIn("pause requested", r.stdout)
        self.assertTrue(w.paths.pause_flag.exists())
        wait = w.next()
        self.assertEqual((wait["action"], wait["dispatch_id"]), ("wait", a["dispatch_id"]))
        result(w, a, worker, w.perform(a, worker))
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "blocked", "paused"))
        self.assertEqual(len(w.events("dispatch_requested")), 1)              # nothing new started
        self.assertEqual(len(w.events("paused")), 1)
        self.assertFalse(w.paths.pause_flag.exists())
        handoff = w.paths.handoff.read_text(encoding="utf-8")
        self.assertIn("Outcome: blocked (paused)", handoff)
        self.assertIn("leadhand run", handoff)
        # a relaunch continues where it stopped
        resume(w)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2])

    def test_cancel_ends_incomplete_cancelled_and_keeps_card_branches(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        ws = w.workspace()
        write(ws, {"src/card1/partial.txt": "partial\n"})       # the worker's partial work so far
        run_git(ws, "add", "--", "src/card1/partial.txt")
        run_git(ws, *NO_HOOKS, "commit", "--quiet", "-m", "partial")
        partial = w.branch_tip(CARD1)
        r = w.cli("cancel", w.run_dir).ok()
        self.assertIn("cancel requested", r.stdout)
        self.assertTrue(w.paths.cancel_flag.exists())
        stop = w.next()
        self.assertEqual((stop["action"], stop["dispatch_id"], stop["worker_id"], stop["record_result"]),
                         ("stop_worker", a["dispatch_id"], worker, True))
        self.assertEqual(w.events("outcome"), [])               # not before the worker is stopped
        result(w, a, worker, SPENT, "--stopped")
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
        fails(self, relaunch(w), "this run has ended: incomplete (cancelled)")
        self.assertEqual(w.events("resumed"), [])

    def test_cancel_works_after_the_source_deck_was_edited(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        with open(w.deck_path, "a", encoding="utf-8") as f:
            f.write("\nContext: edited after the start.\n")
        w.cli("cancel", w.run_dir).ok()
        stop = w.next()
        self.assertEqual((stop["action"], stop["dispatch_id"], stop["worker_id"]),
                         ("stop_worker", a["dispatch_id"], worker))
        result(w, a, worker, SPENT, "--stopped")
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "incomplete", "cancelled"))
        self.assertEqual(len(w.events("dispatch_requested")), 1)

    def test_pause_works_after_the_source_deck_was_deleted_but_nothing_new_starts(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        reply = w.perform(a, worker)
        w.deck_path.unlink()
        # without a pause, the changed deck still blocks the next step: the result is recorded,
        # then `next` refuses
        result(w, a, worker, reply)
        fails(self, w.cli("next", w.run_dir), "cannot read the deck")
        self.assertEqual(len(w.events("dispatch_requested")), 1)
        w.cli("pause", w.run_dir).ok()
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "blocked", "paused"))
        self.assertEqual(len(w.events("dispatch_requested")), 1)              # nothing new started

    def test_cancel_survives_a_relaunch_before_it_finished(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)
        w.cli("cancel", w.run_dir).ok()
        self.assertEqual(w.next()["action"], "stop_worker")      # the session died before stopping it
        resume(w)
        done = w.next()                          # the dead session's worker needs no stop
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        self.assertEqual(len(w.events("dispatch_requested")), 1)

    def test_cancel_confirms_detached_codex_stopped_before_outcome(self):
        w = codex_world(self, [card(1, route=CODEX_CARD)])
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
        loop = w.workspace() / "src/card1/loop.txt"
        wait_for(lambda: loop.exists() and loop.stat().st_size > 0, "the worker writing", proc=p)
        wait_for(lambda: codex_log(w, "child"), "the tool child", proc=p)
        rec = running(self, w, a, p)
        codex = first_codex(w)
        w.cli("cancel", w.run_dir).ok()
        done = w.next()
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        children = codex_log(w, "child")
        self.assertTrue(children)
        self.assertFalse(alive(rec["runner"]), rec)
        self.assertFalse(codex_alive(codex), codex)
        self.assertFalse(alive(rec["codex"]), rec)
        for child in children:
            self.assertFalse(logged_alive(child), child)
        size = loop.stat().st_size
        holds(lambda: loop.stat().st_size == size, 0.5, "no workspace write after the outcome")
        (out,) = w.events("outcome")
        (res,) = w.events("result_recorded")
        self.assertLess(res["seq"], out["seq"])
        self.assertTrue(res["stopped"])
        p.communicate(timeout=60)

    def test_crash_after_cancel_request_finishes_the_cancel_on_restart(self):
        w = codex_world(self, [card(1, route=CODEX_CARD)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "sleep", "seconds": 120}]
        p = start_codex(w, a, lines)
        rec = running(self, w, a, p)
        codex = first_codex(w)
        w.cli("cancel", w.run_dir).ok()
        r = w.cli("next", w.run_dir, crash="after_cancel_request")
        self.assertEqual(r.code, 97, r)
        self.assertEqual(w.events("outcome"), [])
        kill_proxy(p)
        resume(w)
        done = w.next()
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        self.assertFalse(alive(rec["runner"]), rec)
        self.assertFalse(codex_alive(codex), codex)
        self.assertFalse(alive(rec["codex"]), rec)
        (res,) = w.events("result_recorded")
        self.assertLess(res["seq"], w.events("outcome")[0]["seq"])

    def test_relaunch_refuses_a_finished_run(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        fails(self, relaunch(w), "this run has ended: complete")
        self.assertEqual(w.events("resumed"), [])
        fails(self, w.cli("cancel", w.run_dir), "this run has ended: complete")
        self.assertFalse(w.paths.cancel_flag.exists())


def orphaned_codex(test: unittest.TestCase, lines: list[dict]):
    """A Codex card dispatch whose runner was SIGKILLed while Codex (and a tool child) live on;
    the proxy died with the session. Returns the world, the action, runner.json, the fake's
    `start` line for Codex and its `child` lines."""
    w = codex_world(test, [card(1, route=CODEX_CARD)])
    w.session("supervisor-1")
    w.start()
    a = w.next()
    w.launch(a)
    p = start_codex(w, a, lines)
    wait_for(lambda: codex_log(w, "child"), "the tool child", proc=p)
    rec = running(test, w, a, p)
    codex = first_codex(w)
    signal_if_alive(rec["runner"], signal.SIGKILL)
    kill_proxy(p)
    wait_for(lambda: not alive(rec["runner"]), "the runner to die")
    return w, a, rec, codex, codex_log(w, "child")


ORPHAN_LINES = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                {"action": "write_file", "path": "src/card1/p.txt", "text": "p\n"},
                {"action": "tool_child", "seconds": 120}, {"action": "sleep", "seconds": 120}]


class FixRound1(unittest.TestCase):
    """Review fix round 1 of Task 21."""

    def test_cancel_after_an_interrupted_acceptance_reconciles_first(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        for _ in range(2):
            w.act(w.next())                      # card 1 implemented and reviewed (clean)
        r = w.cli("next", w.run_dir, crash="after_fast_forward")
        self.assertEqual(r.code, 97, r)
        self.assertNotEqual(w.branch_tip(RUN), w.base)
        w.cli("cancel", w.run_dir).ok()
        resume(w)
        done = w.next()
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        (acc,) = w.events("accepted")
        self.assertEqual((acc["card"], acc["sha"]), (1, w.branch_tip(RUN)))
        self.assertFalse(any("card 1" in r for r in done["reasons"]), done["reasons"])
        (rec,) = [e for e in w.events("reconciled") if "found" in e]
        self.assertIn("card 1's acceptance was completed", rec["found"])
        self.assertLess(rec["seq"], w.events("cancelled")[0]["seq"])

    def test_cancel_commits_a_stopped_claude_workers_partial_work(self):
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        ws = w.workspace()
        write(ws, {"src/card1/partial.txt": "partial\n"})       # uncommitted when it is stopped
        w.cli("cancel", w.run_dir).ok()
        self.assertEqual(w.next()["action"], "stop_worker")
        event = result(w, a, worker, SPENT, "--stopped")
        done = w.next()
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        tip = w.branch_tip(CARD1)
        self.assertEqual(event["tip"], tip)
        run_git(w.repo, "cat-file", "-e", f"{CARD1}:src/card1/partial.txt")
        body = git_log_body(w.repo, CARD1)
        self.assertIn(f"Leadhand-Dispatch: {a['dispatch_id']}", body.splitlines())
        self.assertIn("partial work (stopped)", body)
        self.assertEqual(run_git(ws, "status", "--porcelain"), "")
        intent = [e for e in w.events("reconciled") if e.get("stage") == "partial_work"]
        self.assertEqual([e["dispatch_id"] for e in intent], [a["dispatch_id"]])
        self.assertLess(intent[0]["seq"], event["seq"])

    def test_cancel_commits_partial_work_of_a_worker_that_died_with_its_session(self):
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"crash": "before_commit"})
        a = w.next()
        worker = w.launch(a)
        self.assertIsNone(w.perform(a, worker))  # its files are written, not committed
        w.cli("cancel", w.run_dir).ok()
        resume(w)
        done = w.next()                          # no stop needed: it died with the session
        self.assertEqual((done["outcome"], done["qualifier"]), ("incomplete", "cancelled"))
        run_git(w.repo, "cat-file", "-e", f"{CARD1}:src/card1/{a['dispatch_id']}.txt")
        self.assertIn(f"Leadhand-Dispatch: {a['dispatch_id']}", git_log_body(w.repo, CARD1).splitlines())
        (res,) = w.events("result_recorded")
        self.assertEqual((res["tip"], res["partial_commit"]), (w.branch_tip(CARD1), w.branch_tip(CARD1)))

    def test_stopped_check_worker_changes_are_not_committed(self):
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.act(w.next())                          # implement
        a = w.next()
        self.assertEqual(a["kind"], "review")
        worker = w.launch(a)
        tip = w.branch_tip(CARD1)
        write(w.workspace(), {"src/card1/reviewer.txt": "not mine to write\n"})
        w.cli("cancel", w.run_dir).ok()
        self.assertEqual(w.next()["action"], "stop_worker")
        result(w, a, worker, SPENT, "--stopped")
        w.next()
        self.assertEqual(w.branch_tip(CARD1), tip)

    def test_failed_reconciliation_stops_live_codex_before_the_outcome(self):
        w = codex_world(self, [card(1, route=CODEX_CARD)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.launch(a)
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "write_file", "path": "src/card1/started.txt", "text": "started\n"},
                 {"action": "append_loop", "path": "src/card1/loop.txt", "interval": 0.05},
                 {"action": "sleep", "seconds": 120}]
        p = start_codex(w, a, lines)
        loop = w.workspace() / "src/card1/loop.txt"
        wait_for(lambda: loop.exists() and loop.stat().st_size > 0, "the worker writing", proc=p)
        rec = running(self, w, a, p)
        codex = first_codex(w)
        kill_proxy(p)
        run_git(w.repo, "update-ref", "-d", f"refs/heads/{RUN}")     # unmatched: the run branch is gone
        resume(w)
        done = w.next()
        self.assertEqual(done["outcome"], "failed")
        self.assertIn(f"state could not be reconciled: the run branch {RUN} is missing", done["reasons"])
        self.assertFalse(alive(rec["runner"]), rec)
        self.assertFalse(codex_alive(codex), codex)
        self.assertFalse(alive(rec["codex"]), rec)
        self.assertTrue((w.run_dir / "workers" / a["dispatch_id"] / "result.json").exists())
        size = loop.stat().st_size
        holds(lambda: loop.stat().st_size == size, 0.5, "no workspace write after the outcome")

    def test_orphaned_codex_is_cancelled_before_anything_else(self):
        w, a, rec, codex, children = orphaned_codex(self, ORPHAN_LINES)
        self.assertTrue(codex_alive(codex))
        self.assertTrue(alive(rec["codex"]))
        self.assertTrue(children and logged_alive(children[0]), children)
        resume(w)
        nxt = w.next()
        self.assertFalse(codex_alive(codex))
        self.assertFalse(alive(rec["codex"]))
        self.assertFalse(any(logged_alive(c) for c in children), children)
        (intent,) = [e for e in w.events("reconciled") if e.get("stage") == "cancel"]
        self.assertEqual((intent["dispatch_id"], intent["runner"]), (a["dispatch_id"], "orphaned"))
        self.assertEqual((nxt["kind"], nxt["dispatch_id"]), ("continue", "c1-a1-continue-1"))
        run_git(w.repo, "cat-file", "-e", f"{CARD1}:src/card1/p.txt")      # the runner's cancel commit
        self.assertEqual(codex_starts(w), {a["dispatch_id"]: 1})

    def test_crash_in_reconciliations_cancel_window_is_repeat_safe(self):
        w, a, rec, codex, children = orphaned_codex(self, ORPHAN_LINES)
        resume(w)
        r = w.cli("next", w.run_dir, crash="after_cancel_request")
        self.assertEqual(r.code, 97, r)
        intents = [e for e in w.events("reconciled") if e.get("stage") == "cancel"]
        self.assertEqual(len(intents), 1)
        self.assertEqual([e for e in w.events("reconciled") if "found" in e], [])
        self.assertEqual(w.events("result_recorded"), [])
        resume(w, "supervisor-3")
        nxt = w.next()
        self.assertEqual((nxt["kind"], nxt["dispatch_id"]), ("continue", "c1-a1-continue-1"))
        self.assertFalse(codex_alive(codex))
        self.assertFalse(alive(rec["codex"]))
        self.assertFalse(any(logged_alive(c) for c in children), children)
        (res,) = w.events("result_recorded")
        self.assertTrue(res["interrupted"])          # our own cancel, not a stop at the deadline

    def test_unrecorded_codex_start_past_its_deadline_is_not_charged_or_stopped(self):
        lines = [{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                 {"action": "tool_child", "seconds": 120}, {"action": "sleep", "seconds": 120}]
        w = codex_world(self, [card(1, route=CODEX_CARD)])
        w.session("supervisor-1")
        w.start()
        a = w.next()                             # the proxy ran, but `record started` never happened
        p = start_codex(w, a, lines)
        running(self, w, a, p)
        kill_proxy(p)
        resume(w)
        wait = w.next()
        self.assertEqual((wait["action"], wait["worker_id"]), ("wait", f"{a['dispatch_id']}:unrecorded"))
        w.advance(70 * 60)
        nxt = w.next()
        self.assertEqual(nxt["action"], "dispatch")          # no stop_worker for the made-up worker ID
        self.assertEqual(nxt["dispatch_id"], "c1-a2-implement-1")   # stopped at its limit: SPENT, escalated
        (res,) = w.events("result_recorded")
        self.assertEqual((res["status"], res["stopped"], res["charge_ids"]), ("SPENT", True, []))
        self.assertFalse(any(":unrecorded" in c["charge_id"] for c in w.charges()))

    def interrupted_check(self, kind):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.session("supervisor-1")
        w.start()
        for _ in range(50):
            a = w.next()
            if a["action"] == "dispatch" and (a.get("gate") == kind or a["kind"] == kind):
                break
            w.act(a)
        w.launch(a)                              # the worker dies without a report
        resume(w)
        nxt = w.next()
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        return a, nxt, w

    def test_interrupted_review_without_report_runs_again(self):
        a, nxt, w = self.interrupted_check("review")
        self.assertEqual((nxt["kind"], nxt["dispatch_id"], nxt["resume"]), ("review", "c1-a1-review-2", None))
        res = next(e for e in w.events("result_recorded") if e["dispatch_id"] == a["dispatch_id"])
        self.assertTrue(res["interrupted"])

    def test_interrupted_gate_without_report_runs_again(self):
        a, nxt, w = self.interrupted_check("final_review")
        self.assertEqual((nxt["kind"], nxt["dispatch_id"], nxt["gate"]),
                         ("final_review", "run-final_review-2", "final_review"))
        self.assertEqual([e["dispatch_id"] for e in w.events("gate_started") if e["gate"] == "final_review"],
                         ["run-final_review-1", "run-final_review-2"])
        self.assertEqual([e["dispatch_id"] for e in w.events("gate_result") if e["gate"] == "final_review"],
                         ["run-final_review-2"])

    def interrupted_final_fix(self, crash):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.script(None, "final_review", {"verdict": "defects"})
        w.script(None, "final_fix", {"crash": crash} if crash else {})
        w.session("supervisor-1")
        w.start()
        for _ in range(50):
            a = w.next()
            if a["action"] == "dispatch" and a["kind"] == "final_fix":
                break
            w.act(a)
        worker = w.launch(a)
        if crash:
            self.assertIsNone(w.perform(a, worker))
        resume(w)
        nxt = w.next()
        self.assertEqual((nxt["kind"], nxt["dispatch_id"], nxt["wave"]), ("final_fix", "run-w1-final_fix-2", 1))
        self.assertIn("## The interrupted step", Path(nxt["brief"]).read_text(encoding="utf-8"))
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        return w, a

    def test_interrupted_final_fix_before_its_commit_runs_again(self):
        w, a = self.interrupted_final_fix(None)
        self.assertEqual(len([e for e in w.events("accepted") if e["card"] == "final"]), 1)

    def test_interrupted_final_fix_after_its_commit_keeps_it(self):
        w, a = self.interrupted_final_fix("after_commit")
        found = [e["found"] for e in w.events("reconciled") if "found" in e]
        self.assertIn("the run's commits were found and recorded", found[0])
        run_git(w.repo, "cat-file", "-e", f"refs/heads/{RUN}:src/final/{a['dispatch_id']}.txt")   # on the run branch


class FixRound2(unittest.TestCase):
    """Review fix round 2 of Task 21: a reconciliation mismatch outranks a pending cancel."""

    def rewritten(self, cancel):
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"crash": "after_commit"})
        a = w.next()
        worker = w.launch(a)
        self.assertIsNone(w.perform(a, worker))
        ws = w.workspace()
        orphan = run_git(ws, "commit-tree", "-m", "rewritten", w.base + "^{tree}")
        run_git(ws, "reset", "--hard", orphan)            # the card branch no longer holds where it started
        if cancel:
            w.cli("cancel", w.run_dir).ok()
        resume(w)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "failed", ""))
        self.assertTrue(any(r.startswith("state could not be reconciled: card 1's branch") and "was rewritten" in r
                            for r in done["reasons"]), done["reasons"])
        (rec,) = [e for e in w.events("reconciled") if "found" in e]
        self.assertTrue(any("was rewritten" in f for f in rec["found"]), rec["found"])
        self.assertEqual(len(w.events("outcome")), 1)
        self.assertEqual(len(w.events("dispatch_requested")), 1)       # nothing new was started
        return w

    def test_rewritten_branch_fails_the_run(self):
        self.rewritten(cancel=False)

    def test_rewritten_branch_fails_the_run_even_with_a_cancel_pending(self):
        self.rewritten(cancel=True)

    def test_cancel_with_the_run_branch_gone_fails_with_every_accepted_card_missing(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        for _ in range(2):
            w.act(w.next())
        a = w.next()                             # card 1 accepted; card 2 dispatched
        (acc,) = w.events("accepted")
        worker = w.launch(a)
        run_git(w.repo, "update-ref", "-d", f"refs/heads/{RUN}")
        w.cli("cancel", w.run_dir).ok()
        self.assertEqual(w.next()["action"], "stop_worker")
        result(w, a, worker, SPENT, "--stopped")
        done = w.next()
        self.assertEqual((done["outcome"], done["qualifier"]), ("failed", ""))
        self.assertIn("state could not be reconciled: accepted work missing from the run branch: "
                      f"card 1 at {acc['sha'][:12]}", done["reasons"])
        self.assertIsNone(w.events("outcome")[0]["head"])

    def test_run_branch_deleted_after_acceptance_ends_failed_on_relaunch(self):
        # With the real end of run (gates on), `done` always carries an outcome.
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.session("supervisor-1")
        w.start()
        for _ in range(20):
            if w.events("accepted"):
                break
            w.act(w.next())
        run_git(w.repo, "update-ref", "-d", f"refs/heads/{RUN}")
        fails(self, w.cli("next", w.run_dir))   # the live session cannot go on: an error, not a guess
        resume(w)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"]), ("done", "failed"))
        self.assertIn(f"state could not be reconciled: the run branch {RUN} is missing", done["reasons"])
        self.assertEqual(w.next()["outcome"], "failed")


class RunnerHelpers(unittest.TestCase):
    """What reconciliation does with the processes a finished Codex dispatch's dead runner left
    (`dispatch.stop_strays`), against real processes: the worker folder names a test sleeper as
    Codex (runner.json) and another as its descendant (procs.json), with a runner that is gone."""

    def sleeper(self) -> tuple[subprocess.Popen, dict]:
        # stderr discarded: a stop's SIGINT makes the sleeper print a KeyboardInterrupt traceback.
        p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"], start_new_session=True,
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(self.reap, p)
        start = wait_for(lambda: harness._start_time(p.pid), "the sleeper's process record")
        return p, {"pid": p.pid, "pgid": p.pid, "start": start}

    @staticmethod
    def reap(p: subprocess.Popen) -> None:
        if p.poll() is None:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        p.wait(timeout=10)

    def holding_runner_lock(self, wd: Path) -> subprocess.Popen:
        lock = wd / "runner.lock"
        p = subprocess.Popen([sys.executable, "-c",
                              "import fcntl,os,sys,time\n"
                              f"fd=os.open({str(lock)!r},os.O_RDWR|os.O_CREAT)\nfcntl.flock(fd,fcntl.LOCK_EX)\n"
                              "print('locked',flush=True)\ntime.sleep(120)"],
                             start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True)
        self.addCleanup(lambda: (self.reap(p), p.stdout.close()))
        self.assertEqual(p.stdout.readline().strip(), "locked")
        return p

    def test_stop_strays_stops_what_a_dead_runner_left_and_leaves_a_live_runner_alone(self):
        """Covers P14."""
        w = codex_world(self, [card(1, route=CODEX_CARD)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        w.act(a)                                 # Codex ran to its end; its result is recorded
        (res,) = w.events("result_recorded")
        self.assertEqual((res["dispatch_id"], res["status"]), (a["dispatch_id"], "READY"))
        wd = w.run_dir / "workers" / a["dispatch_id"]
        tool, tool_rec = self.sleeper()
        codex, codex_rec = self.sleeper()
        (wd / "runner.json").write_text(json.dumps({"runner": {"pid": 1, "pgid": 1, "start": "gone"},
                                                    "codex": codex_rec}), encoding="utf-8")
        (wd / "procs.json").write_text(json.dumps({"descendants": [tool_rec]}), encoding="utf-8")
        runner = self.holding_runner_lock(wd)
        resume(w)
        w.next()                                 # a live runner stops its own
        self.assertIsNone(codex.poll())
        self.assertIsNone(tool.poll())
        self.reap(runner)
        resume(w, "supervisor-3")
        w.next()                                 # the runner is gone: what it left is stopped
        self.assertIsNotNone(codex.wait(timeout=10))
        self.assertIsNotNone(tool.wait(timeout=10))
        resume(w, "supervisor-4")
        w.next()                                 # nothing left: a no-op


# --------------------------------------------------------------------------
# Every crash window

class StableSupervisor(FakeSupervisor):
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
        self.launch_log = self.tmp / "launches.txt"
        self.launch_log.touch()

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
        if label == "c2-a1-fix":
            return {"files": {"src/card2/fix.py": "FIXED = 'c2-a1-fix'\n"}}   # production: a re-review follows
        return {}

    def operator(self):
        """Unpark card 1 once card 2 is accepted (once): `unpark` in the drive's environment, so
        a crash point it passes is armed too."""
        events = self.events()
        parked = any(e["kind"] == "parked" and e["card"] == 1 for e in events)
        accepted = any(e["kind"] == "accepted" and e["card"] == 2 for e in events)
        if parked and accepted and not any(e["kind"] == "unparked" for e in events):
            self._cli("unpark", self.run_dir, "--card", 1, "--note", NOTE).ok()

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
        """A runner killed by the crash may leave Codex running; a real restart comes later. Waits
        (at most `timeout`) for every Codex the fake's log names to be gone."""
        end = time.monotonic() + timeout
        while any(codex_alive(r) for r in codex_log(self, "start")) and time.monotonic() < end:
            time.sleep(0.05)

    def final_state(self) -> dict:
        out = self.events("outcome")[-1]
        return {"outcome": (out["outcome"], out["qualifier"]),
                "cards": card_states(self, self.CARDS),
                "accepted": {e["card"]: e["sha"] for e in self.events("accepted")},
                "head": self.branch_tip(RUN),
                "tree": run_git(self.repo, "rev-parse", f"refs/heads/{RUN}^{{tree}}")}

    def check_invariants(self, test):
        started = collections.Counter(e["dispatch_id"] for e in self.events("worker_started"))
        test.assertEqual([d for d, k in started.items() if k > 1], [], "a dispatch recorded two workers")
        launched = collections.Counter(self.launch_log.read_text(encoding="utf-8").split())
        test.assertEqual([d for d, k in launched.items() if k > 1], [], "a dispatch was launched twice")
        test.assertEqual([d for d, k in codex_starts(self).items() if k > 1], [], "Codex started twice")
        fsck = run_git(self.repo, "fsck", "--unreachable", "--no-reflogs", "--no-progress")
        test.assertEqual([ln for ln in fsck.splitlines() if ln.startswith("unreachable commit")], [],
                         "a commit was lost")


def crash_child(w: StableSupervisor, point: str, skip: int, counter: Path, info: Path) -> int:
    """Drive the world in a forked child that is its own process group, as the supervisor
    session is. Its CLI calls and dispatch lines stay in that group (LEADHAND_CRASH_KILL_PGID),
    and the crash point, wherever it is passed (the harness, a CLI call, a detached runner),
    kills the whole group at its (skip+1)-th pass. Returns the wait status. `info` gets the
    child's group, for the matrix's cleanup if this worker is killed at its deadline."""
    errors = w.tmp / "child error.txt"
    pid = os.fork()
    if pid == 0:
        code = 1
        try:
            os.setpgid(0, 0)
            w.env.update({"LEADHAND_CRASH_AT": point, "LEADHAND_CRASH_SKIP": str(skip),
                          "LEADHAND_CRASH_COUNTER": str(counter),
                          "LEADHAND_CRASH_KILL_PGID": str(os.getpid())})
            w.drive()
            code = 0
        except BaseException:
            errors.write_text(traceback.format_exc(), encoding="utf-8")
        finally:
            os._exit(code)
    try:
        os.setpgid(pid, pid)
    except OSError:
        pass                                     # the child already did (or is gone)
    with open(info, "a", encoding="utf-8") as f:
        f.write(json.dumps({"drive_pgid": pid, "drive_start": harness._start_time(pid)}) + "\n")
    end = time.monotonic() + 180
    while True:
        done, status_ = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        if time.monotonic() > end:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
            raise AssertionError(f"{point} #{skip + 1}: the crashed run hung")
        time.sleep(0.02)
    if errors.exists():
        raise AssertionError(f"{point} #{skip + 1}: {errors.read_text(encoding='utf-8')}")
    return status_


def kill_world(info: Path) -> None:
    """Clean up after a crash-matrix worker killed at its deadline: the drive's group, every
    process its run records, the fake Codex's tool children, then its temporary root. The
    group is killed only while its leader is still the drive recorded: it was usually reaped
    long before, and its pid may since name an unrelated group."""
    records = read_jsonl(info) if info.exists() else []
    merged = {k: v for rec in records for k, v in rec.items()}
    if same_process(merged.get("drive_pgid"), merged.get("drive_start")):
        try:
            os.killpg(merged["drive_pgid"], signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    if merged.get("run_dir"):
        kill_run_leftovers(Path(merged["run_dir"]), Path(merged["codex_log"]))
    if merged.get("tmp"):
        shutil.rmtree(merged["tmp"], ignore_errors=True)


class EveryCrashPoint(unittest.TestCase):
    """Crash at every occurrence of every crash point, restart, and reach the uninterrupted
    run's final state. "The end" is the run's outcome with its report (the world runs the
    end-of-run gates, so `before_gate_record` and the gate recovery are covered too). The
    crash runs are independent, so they run in forked workers, a few at a time.

    The contract's crash points are the core plan's CRASH_POINTS and the harness's own
    `before_commit` and `after_commit`. The reference run must pass every one of them except
    `after_cancel_request` (a cancel's: covered by the cancel tests). The matrix crashes at
    every point the reference run's LEADHAND_CRASH_LOG names, as often as it names it, so a
    point a binary adds is crash-tested too. "The same final state" is the reference run's, by
    the same binary: the outcome, every card's state, the accepted SHAs, and the run branch's
    tip and tree."""

    POINTS = ("before_worker_launch", "after_worker_launch", "before_rebase", "after_rebase",
              "before_fast_forward", "after_fast_forward", "before_result_record", "after_result_files",
              "after_charge_before_attempt", "after_cancel_request", "before_gate_record",
              "before_commit", "after_commit")
    NOT_ON_THE_PATH = ("after_cancel_request",)
    PARALLEL = max(1, min(4, os.cpu_count() or 1))
    RUN_TIMEOUT = 300.0

    def stable_world(self) -> StableSupervisor:
        dates = mock.patch.dict(os.environ, {"GIT_AUTHOR_DATE": FIXED_DATE, "GIT_COMMITTER_DATE": FIXED_DATE})
        dates.start()
        self.addCleanup(dates.stop)
        w = StableSupervisor(self)               # its environment, and every Git call here, get the dates
        w.session("supervisor-1")
        w.start()
        return w

    def crash_run(self, point: str, k: int, expected: dict, info: Path) -> None:
        """One crash run (in a forked worker): crash at the (k+1)-th pass of `point`, leave the
        mismatch, relaunch without the crash, run to the end, compare."""
        w = self.stable_world()
        try:
            with open(info, "a", encoding="utf-8") as f:
                f.write(json.dumps({"tmp": str(w.tmp), "run_dir": str(w.run_dir),
                                    "codex_log": w.env["FAKE_CODEX_LOG"]}) + "\n")
            counter = w.tmp / "crash counter"
            status_ = crash_child(w, point, k, counter, info)
            self.assertTrue(os.WIFSIGNALED(status_) or os.WEXITSTATUS(status_) == 97,
                            f"{point} #{k + 1} did not crash (status {status_})")
            self.assertEqual(counter.read_text(encoding="utf-8").strip(), str(k + 1))
            w.wait_for_orphans()
            resume(w)
            w.drive()
            self.assertEqual(w.final_state(), expected)
            w.check_invariants(self)
            self.assertTrue(w.events("reconciled"))
            self.assertTrue(w.paths.report.is_file())
        finally:
            w.kill_leftovers()
            shutil.rmtree(w.tmp, ignore_errors=True)

    def test_restart_from_every_crash_point_reaches_same_final_state(self):
        """Covers bug class 7 (crash windows)."""
        ref = self.stable_world()
        log = ref.tmp / "crash points.txt"
        ref.env["LEADHAND_CRASH_LOG"] = str(log)
        ref.drive()
        del ref.env["LEADHAND_CRASH_LOG"]
        expected = ref.final_state()
        self.assertEqual(expected["outcome"], ("complete", ""))
        self.assertEqual(set(expected["cards"].values()), {"accepted"})
        self.assertEqual(sorted(expected["accepted"]), [1, 2, 3])
        self.assertTrue(ref.events("integration_started") and ref.events("fix_round"))
        self.assertIn("c2-a1-rereview-1", ref.dispatch_ids())
        self.assertTrue(any(d["attempt"] == 2 for d in ref.started if d["card"] == 3))
        self.assertTrue(ref.paths.report.is_file())
        ref.check_invariants(self)
        seen = collections.Counter(log.read_text(encoding="utf-8").split())
        # Every crash point is on this run's path except a cancel's (covered by the cancel tests).
        self.assertEqual([p for p in self.POINTS if p not in self.NOT_ON_THE_PATH and not seen[p]], [], seen)

        order = [p for p in self.POINTS if seen[p]] + sorted(p for p in seen if p not in self.POINTS)
        todo = [(point, k) for point in order for k in range(seen[point])]
        results_dir = Path(tempfile.mkdtemp(prefix="lh crash results "))
        self.addCleanup(shutil.rmtree, results_dir, True)
        running_: dict[int, tuple[str, int, float]] = {}
        results: dict[tuple[str, int], str] = {}
        try:
            while todo or running_:
                while todo and len(running_) < self.PARALLEL:
                    point, k = todo.pop(0)
                    out, info = results_dir / f"{point}-{k}.txt", results_dir / f"{point}-{k}.info"
                    pid = os.fork()
                    if pid == 0:
                        code = 1
                        try:
                            os.setpgid(0, 0)
                            self.crash_run(point, k, expected, info)
                            out.write_text("ok", encoding="utf-8")
                            code = 0
                        except BaseException:
                            out.write_text(traceback.format_exc(), encoding="utf-8")
                        finally:
                            os._exit(code)
                    try:
                        os.setpgid(pid, pid)
                    except OSError:
                        pass
                    running_[pid] = (point, k, time.monotonic())
                for pid, (point, k, began) in list(running_.items()):
                    done, _ = os.waitpid(pid, os.WNOHANG)
                    if not done and time.monotonic() - began > self.RUN_TIMEOUT:
                        self.stop_worker(pid, results_dir / f"{point}-{k}.info")
                        results[(point, k)] = f"timed out after {self.RUN_TIMEOUT:.0f} s"
                        del running_[pid]
                    elif done:
                        out = results_dir / f"{point}-{k}.txt"
                        results[(point, k)] = (out.read_text(encoding="utf-8") if out.exists()
                                               else "the worker died without a result")
                        del running_[pid]
                time.sleep(0.05)
        finally:
            for pid, (point, k, _) in running_.items():     # an error above: stop what still runs
                self.stop_worker(pid, results_dir / f"{point}-{k}.info")
        for (point, k), text in sorted(results.items(), key=lambda x: (order.index(x[0][0]), x[0][1])):
            with self.subTest(point=point, occurrence=k + 1):
                self.assertEqual(text, "ok", f"{point} #{k + 1}:\n{text}")
        self.assertEqual(len(results), sum(seen.values()))

    @staticmethod
    def stop_worker(pid: int, info: Path) -> None:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
        kill_world(info)


# --------------------------------------------------------------------------
# test_engine_recovery.py

def card_list(n=1):
    return [card(i) for i in range(1, n + 1)]


class ResumeAfterBlocked(unittest.TestCase):
    def test_blocked_provider_capacity_resumes_with_a_new_dispatch(self):
        w = FakeSupervisor(self, card_list())
        w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "provider capacity"))
        relaunch(w).ok()                          # capacity is back
        again = w.next()
        self.assertEqual((again["action"], again["dispatch_id"], again["attempt"]),
                         ("dispatch", "c1-a1-implement-2", 1))
        self.assertEqual(again["route"], w.started[0]["route"])
        self.assertEqual(len(w.events("outcome")), 1)
        w.act(again)
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_blocked_environment_resumes_with_a_fresh_retry_budget(self):
        w = FakeSupervisor(self, card_list())
        w.start()
        blocked = {"status": "BLOCKED", "blocked_by": "environment"}
        w.script(1, "implement", blocked, blocked)
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "environment"))
        relaunch(w).ok()                          # the tool was reinstalled
        w.script(1, "implement", blocked, {"status": "READY"})
        w.run()
        self.assertEqual(w.dispatch_ids()[:4], ["c1-a1-implement-1", "c1-a1-implement-2",
                                                "c1-a1-implement-3", "c1-a1-implement-4"])
        self.assertEqual(len(w.events("outcome")), 1)
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_relaunch_without_an_outcome_keeps_the_retry_budget(self):
        w = FakeSupervisor(self, card_list())
        w.start()
        blocked = {"status": "BLOCKED", "blocked_by": "environment"}
        w.script(1, "implement", blocked, blocked)
        w.act(w.next())
        retry = w.next()
        self.assertEqual(retry["dispatch_id"], "c1-a1-implement-2")
        relaunch(w).ok()                          # the launcher relaunched the supervisor
        w.act(retry)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "blocked", "environment"))

    def test_park_after_the_run_ended_raises(self):
        w = FakeSupervisor(self, card_list())
        w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        w.run()
        fails(self, w.cli("record", w.run_dir, "park", "--card", 1, "--reason", "too late"))
        self.assertEqual(w.events("parked"), [])


class RunCollisions(unittest.TestCase):
    def test_start_returns_a_blocked_run_as_existing(self):
        """Covers R25."""
        w = FakeSupervisor(self, card_list())
        first = w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        w.run()
        r = w.cli("start", w.deck_path, "--repo", w.repo).ok()
        self.assertEqual(run_line(r), first)
        self.assertIn(f"existing run: {first}", r.stdout)

    def test_new_run_suffixes_a_branch_owned_by_another_run(self):
        """Covers R26 (as refined later: another run's branch is suffixed, never reused)."""
        w = FakeSupervisor(self, card_list())
        first = w.start()
        first_meta = read_json(first / "run.json")
        self.assertEqual((first_meta["branch"], first_meta["branch_requested"]), ("leadhand/demo", "leadhand/demo"))
        r = w.cli("start", w.deck_path, "--repo", w.repo, "--new").ok()
        second = run_line(r)
        self.assertNotEqual(second, first)
        meta = read_json(second / "run.json")
        self.assertEqual((meta["branch"], meta["branch_requested"]), ("leadhand/demo-2", "leadhand/demo"))
        self.assertIn("run branch: leadhand/demo-2", r.stdout)
        self.assertNotEqual(meta["workspace"], first_meta["workspace"])
        third = run_line(w.cli("start", w.deck_path, "--repo", w.repo, "--new").ok())
        self.assertEqual(read_json(third / "run.json")["branch"], "leadhand/demo-3")

        # the suffixed run works on its own branches and leaves the first run's alone
        w.run_dir = second
        before = w.branch_tip("leadhand/demo")
        w.run()
        self.assertEqual(w.branch_tip("leadhand/demo"), before)
        (accepted,) = w.events("accepted")
        self.assertEqual(w.branch_tip("leadhand/demo-2"), accepted["sha"])
        self.assertEqual(w.events("card_cut")[0]["branch"], "leadhand/demo-2--card-1")

    def test_start_after_a_finished_run_suffixes_its_branch(self):
        w = FakeSupervisor(self, card_list())
        first = w.start()
        w.run()
        append_event(w.run_dir, "outcome", outcome="complete", qualifier="", reasons=[])
        finished_tip = w.branch_tip("leadhand/demo")
        r = w.cli("start", w.deck_path, "--repo", w.repo).ok()
        self.assertIn("run branch: leadhand/demo-2", r.stdout)
        self.assertNotIn("existing run", r.stdout)
        self.assertIn(f"run: {first}-2\n", r.stdout)
        self.assertEqual(w.branch_tip("leadhand/demo"), finished_tip)
        self.assertEqual(w.branch_tip("leadhand/demo-2"), w.base)

    def test_interrupted_start_retry_uses_a_free_branch(self):
        # A start that crashed just before run.json: what a real start leaves at that point (its
        # run branch, worktree, deck and settings), without run.json and the event log.
        w = FakeSupervisor(self, card_list())
        crashed = w.start()
        for name in ("run.json", "events.jsonl", "state.lock"):
            (crashed / name).unlink(missing_ok=True)
        w.run_dir = None
        self.assertEqual(run_git(w.repo, "rev-parse", "refs/heads/leadhand/demo"), w.base)
        run_dir = w.start()
        self.assertNotEqual(run_dir, crashed)
        meta = w.meta()
        self.assertEqual((meta["branch"], meta["branch_requested"]), ("leadhand/demo-2", "leadhand/demo"))
        self.assertTrue(Path(meta["workspace"]).is_dir())
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_unrecorded_existing_branch_is_not_reused(self):
        w = FakeSupervisor(self, card_list())
        run_git(w.repo, "checkout", "--quiet", "-b", "leadhand/demo")
        theirs = commit(w.repo, {"theirs.txt": "not a run\n"}, "someone else's work")
        run_git(w.repo, "checkout", "--quiet", "main")
        w.start()
        self.assertEqual(w.meta()["branch"], "leadhand/demo-2")
        self.assertEqual(w.branch_tip("leadhand/demo-2"), w.base)
        w.run()
        self.assertEqual(w.branch_tip("leadhand/demo"), theirs)

    def _unrelated_branch(self, w, name):
        run_git(w.repo, "checkout", "--quiet", "-b", name)
        theirs = commit(w.repo, {"src/unrelated.txt": "not this run's\n"}, "someone else's card")
        run_git(w.repo, "checkout", "--quiet", "main")
        return theirs

    def test_existing_card_branch_makes_its_run_branch_name_taken(self):
        # R54: leadhand/demo itself is free, but leadhand/demo--card-1 is not this run's.
        w = FakeSupervisor(self, card_list(), gates=True)
        theirs = self._unrelated_branch(w, "leadhand/demo--card-1")
        w.start()
        self.assertEqual(w.meta()["branch"], "leadhand/demo-2")
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual(w.events("card_cut")[0]["branch"], "leadhand/demo-2--card-1")
        landed = run_git(w.repo, "diff", "--name-only", w.base, "refs/heads/leadhand/demo-2").splitlines()
        self.assertNotIn("src/unrelated.txt", landed)
        self.assertEqual(w.branch_tip("leadhand/demo--card-1"), theirs)
        self.assertFalse(run_git(w.repo, "branch", "--list", "leadhand/demo"))

    def test_existing_final_branch_makes_its_run_branch_name_taken(self):
        w = FakeSupervisor(self, card_list())
        self._unrelated_branch(w, "leadhand/demo--card-final")
        w.start()
        self.assertEqual(w.meta()["branch"], "leadhand/demo-2")

    def test_card_branch_the_run_did_not_cut_is_refused(self):
        w = FakeSupervisor(self, card_list())
        w.start()
        self.assertEqual(w.meta()["branch"], "leadhand/demo")
        # Someone makes the card branch after the run started, before the run cuts it.
        theirs = self._unrelated_branch(w, "leadhand/demo--card-1")
        fails(self, w.cli("next", w.run_dir), "leadhand/demo--card-1")
        self.assertEqual(w.events("card_cut"), [])
        self.assertEqual(w.branch_tip("leadhand/demo--card-1"), theirs)
        self.assertEqual(w.branch_tip("leadhand/demo"), w.base)


class CodexStops(unittest.TestCase):
    def test_hung_proxy_over_a_finished_runner_keeps_its_status(self):
        w = codex_world(self, card_list())
        w.start()
        a = w.next()
        w.launch(a)
        w.run_proxy(a, {"report": False})        # the runner finishes; the proxy never reports
        w.advance(66 * 60)
        stop = w.next()
        self.assertEqual((stop["action"], stop["vendor"], stop["record_result"]), ("stop_worker", "codex", False))
        (res,) = w.events("result_recorded")
        self.assertEqual((res["status"], res["stopped"]), ("READY", False))

    def test_codex_stop_before_the_proxy_ran_prewrites_the_request(self):
        w = codex_world(self, card_list())
        w.start()
        a = w.next()
        w.launch(a)
        w.advance(66 * 60)
        stop = w.next()
        self.assertEqual(stop["action"], "stop_worker")
        wd = w.run_dir / "workers" / a["dispatch_id"]
        self.assertTrue((wd / "request.json").is_file())
        saved = read_json(wd / "result.json")
        self.assertEqual(saved["exit_reason"], "cancelled")
        (event,) = w.events("result_recorded")
        self.assertEqual(event["status"], "SPENT")
        out = w.run_proxy(a, {})                 # a late proxy finds the cancelled result
        self.assertIn("session:", out)
        self.assertEqual(codex_log(w, "start"), [])
        self.assertEqual(read_json(wd / "result.json"), saved)


class Untested(unittest.TestCase):
    def test_reviewer_spent_twice_parks_the_card(self):
        w = FakeSupervisor(self, card_list())
        w.start()
        w.script(1, "review", {"status": "SPENT"}, {"status": "SPENT"})
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"])
        (parked,) = w.events("parked")
        self.assertEqual(parked["reason"], "review ended SPENT twice without a verdict")

    def test_pinned_vendor_not_installed_parks_the_card(self):
        w = FakeSupervisor(self, [card(1, route=CODEX_CARD), card(2)])
        w.start()
        w.run()
        (parked,) = w.events("parked")
        self.assertEqual((parked["card"], parked["reason"]), (1, "pinned vendor not installed"))
        self.assertEqual(w.dispatch_ids(), ["c2-a1-implement-1", "c2-a1-review-1"])

    def worker_crash(self, point):
        w = FakeSupervisor(self, card_list())
        w.start()
        w.script(1, "implement", {"crash": point})
        a = w.next()
        worker = w.launch(a)
        self.assertIsNone(w.perform(a, worker))
        self.assertEqual(w.next()["action"], "wait")
        w.advance(61 * 60)
        stop = w.next()
        self.assertEqual((stop["action"], stop["record_result"]), ("stop_worker", True))
        event = result(w, a, worker, "(stopped)", "--stopped")
        self.assertEqual(event["status"], "SPENT")
        nxt = w.next()
        self.assertEqual((nxt["dispatch_id"], nxt["resume"]), ("c1-a2-implement-1", None))
        return w, event

    def test_worker_crash_before_commit_escalates(self):
        w, event = self.worker_crash("before_commit")
        self.assertEqual(event["tip"], w.base)
        self.assertEqual(w.branch_tip(CARD1), w.base)

    def test_worker_crash_after_commit_keeps_the_commit(self):
        w, event = self.worker_crash("after_commit")
        self.assertNotEqual(event["tip"], w.base)
        self.assertEqual(event["tip"], w.branch_tip(CARD1))

    def test_crash_before_result_record_writes_nothing(self):
        w = FakeSupervisor(self, card_list())
        w.start()
        a = w.next()
        worker = w.launch(a)
        reply = w.perform(a, worker)
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                  "--reply", "-", crash="before_result_record", stdin=reply)
        self.assertEqual(r.code, 97, r)
        self.assertFalse((w.run_dir / "cards" / "1" / a["dispatch_id"]).exists())
        self.assertEqual(w.charges(), [])
        self.assertEqual(w.events("result_recorded"), [])
        event = recorded(w, "result", "--dispatch", a["dispatch_id"], "--worker", worker, "--reply", "-",
                         stdin=reply)
        self.assertEqual(event["status"], "READY")
        self.assertEqual(len(w.charges()), 1)


# --------------------------------------------------------------------------
# test_state.py: the crash protocol's gate

class CrashProtocol(unittest.TestCase):
    def test_a_crash_point_exits_97_only_when_testing_and_named(self):
        """Covers limit L4 (test hooks need LEADHAND_TESTING=1)."""
        # `record result` passes before_result_record. It exits 97 there, recording nothing, only
        # under LEADHAND_TESTING=1 with LEADHAND_CRASH_AT naming that point.
        cases = (({"LEADHAND_TESTING": "1", "LEADHAND_CRASH_AT": "before_result_record"}, 97),
                 ({"LEADHAND_CRASH_AT": "before_result_record"}, 0),
                 ({"LEADHAND_TESTING": "0", "LEADHAND_CRASH_AT": "before_result_record"}, 0),
                 ({"LEADHAND_TESTING": "1"}, 0),
                 ({"LEADHAND_TESTING": "1", "LEADHAND_CRASH_AT": "after_rebase"}, 0))
        for overrides, code in cases:
            with self.subTest(env=overrides):
                w = FakeSupervisor(self, [card(1)])
                w.start()
                a = w.next()
                worker = w.launch(a)
                reply = w.perform(a, worker)
                env = {k: v for k, v in w.env.items() if k not in ("LEADHAND_TESTING", "LEADHAND_CRASH_AT")}
                r = harness.run_cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker",
                                    worker, "--reply", "-", env=dict(env, **overrides), cwd=w.tmp, stdin=reply)
                self.assertEqual(r.code, code, r)
                recorded_ = [e["dispatch_id"] for e in w.events("result_recorded")]
                self.assertEqual(recorded_, [] if code == 97 else [a["dispatch_id"]])
                if code == 0:
                    self.assertEqual(r.json()["status"], "READY")


if __name__ == "__main__":
    unittest.main()
