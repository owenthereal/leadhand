#!/usr/bin/env python3
"""The card loop: cards, risk, decisions, reply files, the Codex result path, run branches and
untracked files, as `start`, `next`, `record`, `timer`, `unpark`, `cancel`, `status`,
`report`, `launch-line` and `dispatch` show them.

Converted from tests/test_engine_cards.py, test_engine_by_risk.py, test_engine_decisions.py,
test_engine_reply_file.py, test_engine_codex_result.py, test_engine_codex_e2e.py,
test_engine_run_branch.py and test_engine_untracked.py, with the clock offset of
test_test_hooks.py. The happy path is
contract_smoke.Smoke.test_two_cards_accepted_in_order.

FakeSupervisor plays the run skill. Where a Python test called `engine.record` or
`engine.start`, this calls `record` or `start`; an EngineError is exit code 1 with
`error: ...` on stderr. Codex routes run the dispatch action's own `dispatch` line with
tests/fakes/codex as Codex (LEADHAND_TEST_CODEX_BIN). Whether a Codex process is alive or gone
is judged from the fake's own log (its pid and the time of its `start` line), never from what
the binary recorded alone; branches, commits and trees are read from Git.
"""
import os
import shlex
import time
import unittest
from datetime import date, datetime
from pathlib import Path

import harness
from contract_dispatch import CARD_BRANCH as DISPATCH_CARD_BRANCH
from contract_dispatch import CARD_BRIEF, World as DispatchWorld, wait_for
from contract_launch import path_without_self
from contract_status import is_unpark_line
from gitfixture import run_git, write
from harness import FAKES, read_json, read_jsonl, run_cli, started_by, temp_root
from rundir import append_event
from supervisor import NO_HOOKS, FakeSupervisor, card

RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
CODEX_CARD = "codex gpt-6-luna low"
BY_RISK = '[run]\ncard_review = "by-risk"\n'


# --------------------------------------------------------------------------
# Helpers

def codex_world(test, cards, **kw) -> FakeSupervisor:
    """A world with Claude and Codex installed, and tests/fakes/codex as Codex."""
    w = FakeSupervisor(test, cards, installed=("claude", "codex"), **kw)
    w.env["LEADHAND_TEST_CODEX_BIN"] = str(FAKES / "codex")
    return w


def record(w: FakeSupervisor, kind: str, *flags, stdin=None) -> harness.Result:
    return w.cli("record", w.run_dir, kind, *flags, stdin=stdin)


def recorded(w: FakeSupervisor, kind: str, *flags, stdin=None) -> dict:
    """`record KIND FLAGS`, which must succeed; the event it prints."""
    return record(w, kind, *flags, stdin=stdin).ok().json()


def result(w: FakeSupervisor, a: dict, worker: str, reply: str, *flags) -> dict:
    return recorded(w, "result", "--dispatch", a["dispatch_id"], "--worker", worker, "--reply", reply, *flags)


def run_line(r: harness.Result) -> Path:
    """The run directory `start` names: its `run: <dir>` or `existing run: <dir>` line."""
    for line in r.stdout.splitlines():
        for prefix in ("run: ", "existing run: "):
            if line.startswith(prefix):
                return Path(line[len(prefix):])
    raise AssertionError(f"start printed no run line\n{r}")


def without_deadline(action: dict) -> dict:
    return {k: v for k, v in action.items() if k != "deadline"}


def drive_to(w: FakeSupervisor, kind: str) -> dict:
    """Act until the next action is a dispatch of `kind`; return it unacted."""
    for _ in range(30):
        a = w.next()
        if a["action"] == "dispatch" and a["kind"] == kind:
            return a
        w.act(a)
    raise AssertionError(f"no {kind} dispatch")


def codex_start(w: FakeSupervisor) -> dict:
    """The fake Codex's `start` log line (its pid and the time it was written), once it is there."""
    log = Path(w.env["FAKE_CODEX_LOG"])
    return wait_for(lambda: next((r for r in (read_jsonl(log) if log.exists() else [])
                                  if r.get("event") == "start"), None), "the fake Codex to start")


def codex_alive(rec: dict) -> bool:
    """Whether the Codex a `start` log line names is still that process."""
    return started_by(rec["pid"], rec["time"]) is not None


def wait_runner_codex(w: FakeSupervisor, did: str, proxy) -> dict:
    """runner.json's Codex record, once the runner has written it."""
    runner = w.run_dir / "workers" / did / "runner.json"
    return wait_for(lambda: (read_json(runner) if runner.exists() else {}).get("codex"),
                    f"runner.json to name Codex for {did}", proc=proxy)


def relaunch_env() -> dict:
    """`run` with tests/fakes/claude as the `claude` found on PATH (it exits at once)."""
    return {"PATH": f"{FAKES}{os.pathsep}{os.environ['PATH']}"}


def status_lines(w: FakeSupervisor) -> list[str]:
    return w.cli("status", w.run_dir).ok().stdout.splitlines()


def stray(world, ws, a):
    """The worker also commits on the run branch, then returns to its card branch."""
    current = run_git(ws, "symbolic-ref", "--short", "HEAD")
    run_git(ws, "checkout", "--quiet", RUN)
    write(ws, {"src/stray.txt": "unreviewed\n"})
    run_git(ws, "add", "--", "src/stray.txt")
    run_git(ws, *NO_HOOKS, "commit", "--quiet", "-m", "stray commit on the run branch")
    run_git(ws, "checkout", "--quiet", current)


def run_log(w: FakeSupervisor) -> list[str]:
    return run_git(w.repo, "log", "--format=%s", f"{w.base}..refs/heads/{RUN}").splitlines()


def commit_on(repo: Path, parent: str) -> str:
    """A commit with `parent` as its parent, made without touching any branch or worktree."""
    tree = run_git(repo, "rev-parse", f"{parent}^{{tree}}")
    return run_git(repo, "commit-tree", tree, "-p", parent, "-m", "outside commit")


class EngineCase(unittest.TestCase):
    def fails(self, r: harness.Result, *, code: int = 1) -> harness.Result:
        """A refusal: exit 1 with `error: ...` on stderr (or the usage error's 2)."""
        self.assertEqual(r.code, code, r)
        if code == 1:
            self.assertTrue(r.stderr.startswith("error: "), r)
        return r


# --------------------------------------------------------------------------
# test_engine_cards.py

class HappyPath(EngineCase):
    def test_requested_dispatch_is_repeated_until_started(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a1 = w.next()
        a2 = w.next()
        self.assertEqual(a1["action"], "dispatch")
        self.assertEqual(without_deadline(a1), without_deadline(a2))
        self.assertEqual(len(w.events("dispatch_requested")), 1)
        recorded(w, "started", "--dispatch", a1["dispatch_id"], "--worker", "agent-x")
        a3 = w.next()
        self.assertEqual(a3["action"], "wait")
        self.assertEqual((a3["dispatch_id"], a3["worker_id"]), (a1["dispatch_id"], "agent-x"))
        self.assertTrue(a3["reason"])
        self.assertEqual(len(w.events("dispatch_requested")), 1)

    def test_started_dispatch_returns_wait_then_stop_worker_after_deadline(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        w.advance(59 * 60)
        wait = w.next()
        self.assertEqual(wait["action"], "wait")
        w.advance(2 * 60)
        stop = w.next()
        self.assertEqual(stop, {"action": "stop_worker", "dispatch_id": a["dispatch_id"], "worker_id": worker,
                                "vendor": "claude", "record_result": True})
        self.assertEqual(w.next(), stop)
        w._append_message(w.subagent_transcript(worker), "claude-sonnet-4")
        event = result(w, a, worker, "(the worker was stopped)", "--stopped")
        self.assertEqual(event["status"], "SPENT")
        (charge,) = w.charges()
        self.assertTrue(charge["possibly_incomplete"])
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"], nxt["resume"]),
                         ("dispatch", "c1-a2-implement-1", None))

    def test_duplicate_result_is_harmless(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        reply = w.perform(a, worker)
        e1 = result(w, a, worker, reply)
        e2 = result(w, a, worker, reply)
        e3 = result(w, a, worker, "Status: SPENT\nsomething else")
        self.assertEqual(e1, e2)
        self.assertEqual(e1, e3)
        self.assertEqual(len(w.events("result_recorded")), 1)
        self.assertEqual(len(w.charges()), 1)
        self.assertEqual(len(w.attempts()), 1)
        self.assertEqual(w.next()["dispatch_id"], "c1-a1-review-1")


def custom_report(text: str) -> dict:
    """A review step whose report file is exactly `text` (the worker writes it itself)."""
    def write_report(world, ws, a):
        Path(a["report"]).parent.mkdir(parents=True, exist_ok=True)
        Path(a["report"]).write_text(text, encoding="utf-8")
    return {"report": False, "tamper": write_report}


class Loop(EngineCase):
    def test_repeated_answers_and_reviews_get_distinct_ids(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.script(1, "answer", {"status": "QUESTION"}, {"status": "READY"})
        w.script(1, "review", {"status": "BLOCKED", "blocked_by": "environment"}, {"verdict": "clean"})
        w.rulings[1] = ["Use JSON.", "Keep the old name."]
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-answer-1", "c1-a1-answer-2",
                                            "c1-a1-review-1", "c1-a1-review-2"])
        self.assertEqual(len(set(w.dispatch_ids())), 5)
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_by_risk_review_skips_low_risk_card(self):
        w = FakeSupervisor(self, [card(1), card(2, risk="medium"), card(3, hard=True)], config=BY_RISK)
        w.start()
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c2-a1-implement-1", "c2-a1-review-1",
                                            "c3-a1-implement-1", "c3-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2, 3])
        hard_impl, hard_review = w.started[3], w.started[4]
        self.assertEqual(hard_impl["route"]["model"], "opus")
        self.assertEqual(hard_review["route"]["model"], "opus")

    def test_defects_get_one_fix_round_then_rereview_on_production_change(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "defects", "findings": ["[high] src/widget.py:3 — off by one — fix it"]})
        w.script(1, "fix", {"files": {"src/widget.py": "fixed\n"}})
        w.script(1, "rereview", {"verdict": "clean"})
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1",
                                            "c1-a1-rereview-1"])
        impl, review, fix, rereview = w.started
        impl_worker = w.events("worker_started")[0]["worker_id"]
        review_worker = w.events("worker_started")[1]["worker_id"]
        self.assertEqual(fix["resume"], {"worker_id": impl_worker})
        self.assertEqual(rereview["resume"], {"worker_id": review_worker})
        fix_brief = Path(fix["brief"]).read_text(encoding="utf-8")
        self.assertIn(str(w.run_dir / "cards" / "1" / "c1-a1-review-1" / "report.md"), fix_brief)
        self.assertEqual(len(w.events("fix_round")), 1)
        (accepted,) = w.events("accepted")
        self.assertEqual(run_git(w.repo, "show", f"{accepted['sha']}:src/widget.py"), "fixed")

    def test_clean_verdict_with_medium_finding_counts_as_defects(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "clean", "findings": ["[medium] src/a.py:1 — leaks — close it"]})
        w.script(1, "fix", {"files": {"src/a.py": "closed\n"}})
        w.run()
        self.assertEqual(w.dispatch_ids()[:3], ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1"])
        verdicts = w.events("verdict_recorded")
        self.assertEqual(verdicts[0]["verdict"], "defects")

    def review_verdicts(self, reports: list[str]) -> dict[int, dict]:
        """Run one independent card per report, its reviewer writing that report as is; each
        card's first review result (result_recorded), by card."""
        w = FakeSupervisor(self, [card(n) for n in range(1, len(reports) + 1)])
        w.start()
        for n, text in enumerate(reports, 1):
            w.script(n, "review", custom_report(text))
        w.run()
        first = {}
        for e in w.events("result_recorded"):
            if e["dispatch_kind"] == "review":
                first.setdefault(e["card"], e)
        self.assertEqual(sorted(first), list(range(1, len(reports) + 1)), first)
        return first

    def test_clean_rereview_noting_fixed_findings_stays_clean(self):
        # Only outstanding findings (numbered or bulleted lines under ## Findings) overrule the verdict.
        (head,) = self.review_verdicts(["Verdict: clean\n## Findings\nNone\n## Notes\n"
                                        "1. [high] src/widget.py:1 — fixed; regression test passes.\n"]).values()
        self.assertEqual(head["verdict"], "clean")
        self.assertNotIn("cross_checked", head)

    def test_clean_verdict_cross_check_reads_only_findings_lines(self):
        heads = self.review_verdicts([
            "Verdict: clean\n## Findings\n2. [High] src/a.py:3 — leaks — close it\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\n  1) [medium] src/a.py:3 — x — y\n",
            "Verdict: clean\n## Findings\n1. [low] src/a.py:3 — nit — rename\n"
            "## Notes\n- [high] earlier round: fixed\n1. [medium] fixed\n",
            "Verdict: clean\n[high] src/a.py:3 outside any section\n## Findings\nNone\n"])
        self.assertEqual([heads[n]["verdict"] for n in (1, 2, 3, 4)], ["defects", "defects", "clean", "clean"])

    def test_clean_verdict_cross_check_counts_bullet_findings_lines(self):
        heads = self.review_verdicts([
            "Verdict: clean\n## Findings\n- [high] src/a.py:3 — leaks — close it\n",
            "Verdict: clean\n## Findings\n  * [Medium] src/a.py:3 — x — y\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\n- [low] src/a.py:3 — nit — rename\n"
            "## Notes\n- [high] earlier round: fixed\n* [medium] fixed\n"])
        self.assertEqual([heads[n]["verdict"] for n in (1, 2, 3)], ["defects", "defects", "clean"])

    def test_which_paths_make_a_fix_test_only(self):
        # Ruling R13 (test_gitops.py's is_test_or_doc_path): test and doc folders count at the top
        # level only; test files and docs by name anywhere; dependency and build inputs are
        # production. A fix touching only the first kind is a `check_diff`, else a rereview.
        yes = ["tests/test_x.py", "test/a.c", "spec/a_spec.rb", "docs/guide.html", "tests/deep/helper.py",
               "test_foo.py", "src/foo_test.go", "src/foo.test.ts", "src/foo.spec.js",
               "README.md", "notes/usage.rst", "notes/todo.txt"]
        no = ["src/main.py", "scripts/build.sh", "src/contest.py", "src/latest.py", "docs", "tests", "src/tester.py",
              "pkg/tests/helper.py", "pkg/spec/x.py", "app/docs/views.py",
              "requirements.txt", "requirements-dev.txt", "tests/requirements.txt", "constraints.txt",
              "CMakeLists.txt", "src/CMakeLists.txt"]

        def after_the_fix(paths: list[str]) -> dict:
            w = FakeSupervisor(self, [card(1)])
            w.start()
            w.script(1, "review", {"verdict": "defects"})
            w.script(1, "fix", {"files": {p: "fixed\n" for p in paths}})
            for _ in range(30):
                a = w.next()
                if a["action"] == "check_diff" or a.get("kind") == "rereview":
                    self.assertIn("c1-a1-fix-1", w.dispatch_ids())
                    return a
                w.act(a)
            raise AssertionError("no action after the fix")

        a = after_the_fix(yes)
        self.assertEqual(a["action"], "check_diff", a)
        self.assertEqual(sorted(a["paths"]), sorted(yes))
        for path in no:
            with self.subTest(path=path):
                a = after_the_fix([path])
                self.assertEqual((a["action"], a["kind"]), ("dispatch", "rereview"), a)

    def test_test_only_fix_returns_check_diff(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "defects", "findings": ["[medium] tests/test_widget.py — weak — assert more"]})
        w.script(1, "fix", {"files": {"tests/test_widget.py": "assert more\n", "docs/notes.md": "note\n"}})
        for _ in range(30):
            a = w.next()
            if a["action"] == "check_diff":
                break
            w.act(a)
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1"])
        self.assertEqual((a["card"], a["dispatch_id"]), (1, "c1-a1-fix-1"))
        self.assertEqual(sorted(a["paths"]), ["docs/notes.md", "tests/test_widget.py"])
        diff = Path(a["diff"])
        self.assertTrue(diff.is_relative_to(w.run_dir / "cards" / "1"))
        self.assertIn("+assert more", diff.read_text(encoding="utf-8"))
        self.assertEqual(w.next(), a)                       # repeated until recorded
        recorded(w, "diff-check", "--card", "1", "--ok", "yes", "--note", "only tests and docs")
        done = w.run()
        self.assertEqual(done[-1]["action"], "done")
        self.assertNotIn("c1-a1-rereview-1", w.dispatch_ids())
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_defects_after_fix_round_escalate_with_fresh_worker(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "defects"}, {"verdict": "clean"})
        w.script(1, "fix", {"files": {"src/widget.py": "still wrong\n"}})
        w.script(1, "rereview", {"verdict": "defects"})
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1",
                                            "c1-a1-rereview-1", "c1-a2-implement-1", "c1-a2-review-1"])
        second = w.started[4]
        self.assertIsNone(second["resume"])
        self.assertEqual(second["attempt"], 2)
        self.assertEqual(second["route"], {"vendor": "claude", "model": "sonnet", "effort": "high"})
        workers = [e["worker_id"] for e in w.events("worker_started")]
        self.assertNotIn(workers[4], workers[:4])
        brief = Path(second["brief"]).read_text(encoding="utf-8")
        self.assertIn(str(w.run_dir / "cards" / "1" / "c1-a1-fix-1" / "report.md"), brief)
        self.assertIn(str(w.run_dir / "cards" / "1" / "c1-a1-rereview-1" / "report.md"), brief)
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_spent_escalates_and_exhausted_ladder_parks(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"status": "SPENT"}, {"status": "SPENT"}, {"status": "SPENT"})
        actions = w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a2-implement-1", "c1-a3-implement-1"])
        self.assertEqual([a["route"]["effort"] for a in w.started], ["medium", "high", "high"])
        self.assertEqual(w.started[2]["route"]["model"], "opus")
        (parked,) = w.events("parked")
        self.assertEqual((parked["card"], parked["reason"]), (1, "ladder exhausted"))
        self.assertEqual(actions[-1]["action"], "done")
        self.assertEqual(w.events("accepted"), [])

    def test_usage_limit_ends_blocked_without_switching_vendor(self):
        w = FakeSupervisor(self, [card(1), card(2)], installed=("claude", "codex"),
                           config='[roles.implementer]\nprefer = "claude"\n')
        w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        actions = w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1"])
        done = actions[-1]
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]),
                         ("done", "blocked", "provider capacity"))
        (outcome,) = w.events("outcome")
        self.assertEqual((outcome["outcome"], outcome["qualifier"]), ("blocked", "provider capacity"))
        self.assertEqual(w.next(), done)
        self.assertEqual(len(w.events("dispatch_requested")), 1)

    def test_other_blocked_retries_once_then_ends_blocked_environment(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "environment"},
                 {"status": "BLOCKED", "blocked_by": "environment"})
        actions = w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-implement-2"])
        self.assertEqual(w.started[0]["route"], w.started[1]["route"])
        self.assertEqual(w.started[1]["attempt"], 1)
        done = actions[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "environment"))

    def test_question_ruling_resumes_worker(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        a = w.next()
        worker = w.launch(a)
        result(w, a, worker, w.perform(a, worker))
        ask = w.next()
        self.assertEqual((ask["action"], ask["card"], ask["dispatch_id"], ask["worker_id"]),
                         ("answer", 1, "c1-a1-implement-1", worker))
        self.assertIn("Which storage format", ask["question"])
        self.assertTrue(Path(ask["report"]).is_file())
        self.assertEqual(w.next(), ask)
        recorded(w, "ruling", "--card", "1", "--text", "Use JSON; it is simpler.")
        self.fails(record(w, "ruling", "--card", "1", "--text", "again"))
        ans = w.next()
        self.assertEqual((ans["action"], ans["kind"], ans["dispatch_id"], ans["resume"]),
                         ("dispatch", "answer", "c1-a1-answer-1", {"worker_id": worker}))
        self.assertIn("> Use JSON; it is simpler.", Path(ans["brief"]).read_text(encoding="utf-8").splitlines())
        (ruling,) = w.events("ruling")
        self.assertEqual(ruling["text"], "Use JSON; it is simpler.")
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_parked_card_skips_dependents_and_continues_independent(self):
        w = FakeSupervisor(self, [card(1), card(2, depends=[1]), card(3)])
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision: storage format"
        actions = w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c3-a1-implement-1", "c3-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [3])
        cuts = {e["card"]: e for e in w.events("card_cut")}
        self.assertNotIn(2, cuts)
        self.assertEqual(cuts[3]["base"], w.base)
        self.assertEqual(run_git(w.repo, "rev-parse", "--verify", "--quiet", "refs/heads/leadhand/demo--card-2",
                                 check=False), "")
        done = actions[-1]
        self.assertEqual(done["action"], "done")
        reasons = {c["card"]: c for c in done["cards"]}
        self.assertEqual(reasons[1]["state"], "parked")
        self.assertIn("product decision", reasons[1]["reason"])
        self.assertEqual(reasons[2]["state"], "pending")
        self.assertIn("parked card 1", reasons[2]["reason"])
        self.assertEqual(reasons[3]["state"], "accepted")


class Start(EngineCase):
    def test_run_folders_are_dated_and_suffixed(self):
        # <docs_dir>/runs/<local date>-<slug>, then the first free -2, -3 (test_state.py).
        w = FakeSupervisor(self, [card(1)])
        before = date.today()
        first = w.start()
        second = w.start(new=True)
        third = w.start(new=True)
        after = date.today()
        self.assertEqual(first.parent, w.docs / "runs")
        self.assertIn(first.name, {f"{d.isoformat()}-demo-deck" for d in (before, after)})
        self.assertEqual([second, third], [first.parent / f"{first.name}-2", first.parent / f"{first.name}-3"])
        for run in (first, second, third):
            self.assertTrue((run / "run.json").is_file(), run)

    def test_every_event_line_carries_seq_ts_and_kind(self):
        # events.jsonl: one JSON object per line, numbered from 1 with no gap, each with its UTC
        # time and its kind (test_state.py).
        w = FakeSupervisor(self, [card(1), card(2)])
        w.start()
        w.run()
        self.assertTrue(w.paths.events.read_bytes().endswith(b"\n"))
        events = read_jsonl(w.paths.events)
        self.assertGreater(len(events), 10)
        self.assertEqual([e["seq"] for e in events], list(range(1, len(events) + 1)))
        self.assertEqual(events[0]["kind"], "run_started")
        for e in events:
            self.assertRegex(e["ts"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z$")
            self.assertIsInstance(e["kind"], str)

    def test_a_whole_run_leaves_no_temporary_files(self):
        # Every JSON file and report is written through a temporary file in the same folder
        # (`.<name>.<random>`, `.tmp` for JSON) and renamed: none survives in the run directory,
        # its card folders or a Codex worker folder (test_state.py). Read from the filesystem.
        w = codex_world(self, [card(1, route=CODEX_CARD), card(2)], gates=True)
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"commit": False})
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertTrue(w.paths.report.is_file())
        workers = sorted((w.run_dir / "workers").iterdir())
        self.assertTrue(workers)
        self.assertTrue(all((wd / "result.json").is_file() for wd in workers), workers)
        left = [str(p.relative_to(w.run_dir)) for p in w.run_dir.rglob("*")
                if p.name.endswith(".tmp") or p.name.startswith(".")]
        self.assertEqual(left, [])

    def test_start_returns_existing_unfinished_run_unless_new(self):
        w = FakeSupervisor(self, [card(1)])
        first = w.start()
        meta = w.meta()
        self.assertRegex(meta["run_id"], r"^[0-9a-f]{12}$")
        self.assertEqual(meta["deck_source"], str(w.deck_path.resolve()))
        self.assertEqual(meta["base_sha"], w.base)
        self.assertEqual(meta["branch"], "leadhand/demo")
        self.assertEqual(meta["supervisor_sessions"], [])
        self.assertEqual(meta["slug"], "demo-deck")
        for key in ("repo", "deck_digest", "base_ref", "workspace", "created_at"):
            self.assertIn(key, meta)
        self.assertTrue(Path(meta["workspace"]).is_dir())
        self.assertEqual((first / "deck.md").read_bytes(), w.deck_path.read_bytes())
        self.assertEqual([e["kind"] for e in w.events()], ["run_started"])

        r = w.cli("start", w.deck_path, "--repo", w.repo).ok()
        self.assertEqual(run_line(r), first)
        self.assertIn(f"existing run: {first}", r.stdout.splitlines())

        r = w.cli("start", w.deck_path, "--repo", w.repo, "--new").ok()
        fresh = run_line(r)
        self.assertIn(f"run: {fresh}", r.stdout.splitlines())
        self.assertNotEqual(fresh, first)
        self.assertTrue((fresh / "run.json").is_file())

        for run in (first, fresh):
            append_event(run, "outcome", outcome="incomplete", qualifier="", reasons=[])
        third = run_line(w.cli("start", w.deck_path, "--repo", w.repo).ok())
        self.assertNotIn(third, (first, fresh))

        # the CLI, from an unrelated directory
        elsewhere = temp_root(self, "lh elsewhere ")
        r = run_cli("start", w.deck_path, "--repo", w.repo, env=w.env, cwd=elsewhere).ok()
        self.assertIn(f"existing run: {third}", r.stdout.splitlines())

    def test_start_never_reuses_an_unfinished_run_of_another_clone(self):
        # Two clones share the docs_dir and the deck file: start against the second clone
        # must create its own run, not return the first clone's.
        w = FakeSupervisor(self, [card(1)])
        first = w.start()
        clone = w.tmp / "second clone"
        run_git(w.tmp, "clone", "--quiet", str(w.repo), str(clone))
        r = w.cli("start", w.deck_path, "--repo", clone).ok()
        second = run_line(r)
        self.assertNotEqual(second, first, r)
        self.assertNotIn("existing run", r.stdout)
        meta = read_json(second / "run.json")
        self.assertEqual(meta["repo"], str(clone.resolve()))
        self.assertTrue(Path(meta["workspace"]).resolve().is_relative_to(clone.resolve()))
        # each clone still finds its own unfinished run
        self.assertIn(f"existing run: {first}", w.cli("start", w.deck_path, "--repo", w.repo).ok().stdout)
        self.assertIn(f"existing run: {second}", w.cli("start", w.deck_path, "--repo", clone).ok().stdout)

    def test_start_never_reuses_another_runs_worktree(self):
        """Covers R26 (as refined later: another run's worktree is never reused)."""
        # docs_dir changed between two same-day runs of the same deck: the second run's
        # directory has the same name, so its default worktree path is the first run's.
        w = FakeSupervisor(self, [card(1)], gates=True)
        first = w.start()
        a = w.next()
        self.assertEqual(a["action"], "dispatch")
        w.launch(a)                                     # the first run's worker is active
        ws1 = w.workspace()
        branch1 = run_git(ws1, "symbolic-ref", "--short", "HEAD")
        head1 = run_git(ws1, "rev-parse", "HEAD")
        self.assertEqual(branch1, "leadhand/demo--card-1")

        w.config_file.write_text(f'docs_dir = "{w.tmp / "other docs"}"\n', encoding="utf-8")
        second = run_line(w.cli("start", w.deck_path, "--repo", w.repo).ok())
        self.assertEqual(second.name, first.name)
        meta = read_json(second / "run.json")
        ws2 = Path(meta["workspace"])
        self.assertNotEqual(ws2.resolve(), ws1.resolve())
        self.assertEqual(ws2.name, f"{first.name}-2")
        self.assertEqual(run_git(ws2, "symbolic-ref", "--short", "HEAD"), meta["branch"])
        w.cli("next", second).ok()
        # the first run's worktree is untouched
        self.assertEqual(run_git(ws1, "symbolic-ref", "--short", "HEAD"), branch1)
        self.assertEqual(run_git(ws1, "rev-parse", "HEAD"), head1)
        self.assertEqual(run_git(ws2, "symbolic-ref", "--short", "HEAD"), f"{meta['branch']}--card-1")
        # the second run's evidence base worktree is its own too
        w.run_dir = second
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        self.assertTrue((ws2.parent / f"{ws2.name}--base").is_dir())
        self.assertFalse((ws1.parent / f"{first.name}--base").exists())
        self.assertEqual(run_git(ws1, "symbolic-ref", "--short", "HEAD"), branch1)

    def test_start_refuses_unsupported_settings_and_scout_cards(self):
        w = FakeSupervisor(self, [card(1)], config='[after_run]\nstep = "pr"\n')
        r = self.fails(w.cli("start", w.deck_path, "--repo", w.repo))
        self.assertIn("not supported in this version: after_run.step = pr", r.stderr)
        self.assertFalse((w.docs / "runs").exists())

        s = FakeSupervisor(self, [card(1, role="scout")])
        r = self.fails(s.cli("start", s.deck_path, "--repo", s.repo))
        self.assertIn("not supported in this version: scout cards", r.stderr)
        self.assertFalse((s.docs / "runs").exists())

    def test_check_and_start_refuse_step_limits_claude_workers_cannot_honour(self):
        w = FakeSupervisor(self, [card(1)], config='[limits]\nimplementer = { steps = 90, minutes = 60 }\n')
        msg = ("not supported in this version: limits.implementer.steps = 90 "
               "(Claude workers use the agent file's maxTurns = 150)")
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertIn(msg, r.stdout + r.stderr)
        r = self.fails(w.cli("start", w.deck_path, "--repo", w.repo))
        self.assertIn(msg, r.stderr)
        self.assertFalse((w.docs / "runs").exists())

    def test_deck_changed_since_start_refuses(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        digest = w.meta()["deck_digest"]
        with open(w.deck_path, "a", encoding="utf-8") as f:
            f.write("\nContext: edited after the start.\n")
        r = self.fails(w.cli("next", w.run_dir))
        self.assertEqual(r.stderr.splitlines()[0], f"error: deck changed since start (expected {digest})")
        self.assertEqual(w.events("dispatch_requested"), [])


class Records(EngineCase):
    def test_reply_without_status_blocks_and_keeps_raw(self):
        """Covers P7."""
        w = FakeSupervisor(self, [card(1)])
        w.start()
        raw = "I changed some things.\nIt went fine, mostly.\n"
        w.script(1, "implement", {"report": False, "reply": raw})
        a = w.next()
        worker = w.launch(a)
        event = result(w, a, worker, w.perform(a, worker))
        self.assertEqual((event["status"], event["blocked_by"]), ("BLOCKED", ""))
        self.assertTrue(event["raw_kept"])
        kept = w.run_dir / "cards" / "1" / "c1-a1-implement-1" / "reply.md"
        self.assertEqual(kept.read_text(encoding="utf-8"), raw)
        retry = w.next()
        self.assertEqual((retry["action"], retry["dispatch_id"]), ("dispatch", "c1-a1-implement-2"))

    def test_turn_limit_reply_is_spent(self):
        """Covers P7."""
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"report": False,
                                  "reply": "NOTE: this agent stopped at its 150-turn limit before finishing.\n"})
        a = w.next()
        worker = w.launch(a)
        event = result(w, a, worker, w.perform(a, worker))
        self.assertEqual(event["status"], "SPENT")

    def test_invalid_record_transition_raises(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        self.fails(record(w, "result", "--dispatch", "c9-a1-implement-1", "--worker", "x", "--reply", "Status: READY"))
        self.fails(record(w, "started", "--dispatch", "c1-a1-implement-1", "--worker", "x"))
        a = w.next()
        did = a["dispatch_id"]
        # no worker started yet
        self.fails(record(w, "result", "--dispatch", did, "--worker", "x", "--reply", "Status: READY"))
        self.fails(record(w, "ruling", "--card", "1", "--text", "do it"))        # no question pending
        self.fails(record(w, "diff-check", "--card", "1", "--ok", "yes", "--note", ""))   # no diff check pending
        self.fails(record(w, "park", "--card", "7", "--reason", "no such card"))
        self.fails(record(w, "nonsense"), code=2)                                # not a record kind
        recorded(w, "started", "--dispatch", did, "--worker", "w1")
        recorded(w, "started", "--dispatch", did, "--worker", "w1")             # repeat: harmless
        self.fails(record(w, "started", "--dispatch", did, "--worker", "w2"))
        self.fails(record(w, "result", "--dispatch", did, "--worker", "w2", "--reply", "Status: READY"))
        self.assertEqual(len(w.events("worker_started")), 1)
        self.assertEqual(w.events("result_recorded"), [])
        recorded(w, "park", "--card", "1", "--reason", "operator")
        self.fails(record(w, "park", "--card", "1", "--reason", "twice"))
        self.assertEqual(len(w.events("parked")), 1)

    def test_timer_returns_at_deadline(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        w.launch(a)
        w.advance(61 * 60)
        r = w.cli("timer", w.run_dir, "--dispatch", a["dispatch_id"]).ok()
        self.assertEqual(r.stdout.splitlines(), ["deadline passed"])

    def test_cli_records_reply_from_stdin(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        event = recorded(w, "started", "--dispatch", a["dispatch_id"], "--worker", "agent-cli")
        self.assertEqual(event["worker_id"], "agent-cli")
        reply = w.perform(a, "agent-cli")
        event = recorded(w, "result", "--dispatch", a["dispatch_id"], "--worker", "agent-cli", "--reply", "-",
                         stdin=reply)
        self.assertEqual(event["status"], "READY")
        self.assertEqual(Path(event["files"]["reply"]).read_text(encoding="utf-8"), reply)
        self.assertEqual(w.next()["dispatch_id"], "c1-a1-review-1")
        r = w.cli("timer", w.run_dir, "--dispatch", a["dispatch_id"]).ok()
        self.assertIn("result recorded", r.stdout)


class Codex(EngineCase):
    def test_codex_route_dispatches_proxy_with_null_agent_model(self):
        w = codex_world(self, [card(1)])
        w.start()
        before = time.time()
        a = w.next()
        self.assertEqual(a["agent"], "leadhand:codex")
        self.assertIsNone(a["agent_model"])
        self.assertIsNone(a["agent_effort"])
        self.assertEqual(a["route"], {"vendor": "codex", "model": "gpt-6.1-terra", "effort": "high"})
        argv = shlex.split(a["message"])
        self.assertTrue(harness.names_bin(argv[0]), argv)              # S1-R11: by file
        self.assertEqual(argv[1], "dispatch")
        self.assertEqual(argv[argv.index("--dispatch-id") + 1], "c1-a1-implement-1")
        self.assertEqual(argv[argv.index("--cwd") + 1], str(w.workspace()))
        self.assertEqual(argv[argv.index("--run") + 1], str(w.run_dir))
        self.assertNotIn("--resume", argv)
        deadline = datetime.fromisoformat(a["deadline"]).timestamp()
        self.assertAlmostEqual(deadline - before, 65 * 60, delta=30)

        worker = w.launch(a)
        reply = w.perform(a, worker)
        event = result(w, a, worker, reply)
        self.assertEqual(event["status"], "READY")
        self.assertEqual(event["tip"], w.branch_tip(CARD1))
        self.assertTrue(event["session_id"])
        self.assertEqual(run_git(w.repo, "show", f"{CARD1}:src/card1/c1-a1-implement-1.txt"), "c1-a1-implement-1")
        self.assertEqual(len([r for r in read_jsonl(Path(w.env["FAKE_CODEX_LOG"])) if r.get("event") == "start"]), 1)
        roles = sorted(c["role"] for c in w.charges())
        self.assertEqual(roles, ["implementer", "proxy"])
        proxy = next(c for c in w.charges() if c["role"] == "proxy")
        self.assertEqual(proxy["charge_id"], f"{a['dispatch_id']}:claude:{worker}")
        review = w.next()
        self.assertEqual((review["kind"], review["agent"], review["route"]["vendor"]),
                         ("review", "leadhand:reviewer", "claude"))
        w.act(review)
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_stop_worker_for_codex_cancels_runner_and_records_before_returning(self):
        w = codex_world(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        proxy = w.start_proxy(a, {"sleep": 120})
        wd = w.run_dir / "workers" / a["dispatch_id"]
        wait_runner_codex(w, a["dispatch_id"], proxy)
        codex = codex_start(w)
        self.assertTrue(codex_alive(codex))
        self.assertEqual(w.next()["action"], "wait")
        w.advance(66 * 60)
        stop = w.next()
        self.assertEqual(stop, {"action": "stop_worker", "dispatch_id": a["dispatch_id"], "worker_id": worker,
                                "vendor": "codex", "record_result": False})
        (res,) = w.events("result_recorded")
        self.assertEqual((res["status"], res["stopped"]), ("SPENT", True))
        self.assertEqual(read_json(wd / "result.json")["exit_reason"], "cancelled")
        self.assertFalse(codex_alive(codex))        # stopped before `next` returned
        proxy.communicate(timeout=60)
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("dispatch", "c1-a2-implement-1"))


class Crashes(EngineCase):
    def test_crash_after_fast_forward_before_accepted_reconciles(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.act(drive_to(w, "review"))
        r = w.cli("next", w.run_dir, crash="after_fast_forward")
        self.assertEqual(r.code, 97, r)
        (intent,) = w.events("accept_started")
        self.assertEqual(w.events("accepted"), [])
        self.assertEqual(w.branch_tip(RUN), intent["target"])
        reflog = run_git(w.repo, "reflog", "show", "--format=%H", f"refs/heads/{RUN}")
        done = w.next()
        self.assertEqual(done["action"], "done")
        (accepted,) = w.events("accepted")
        self.assertEqual(accepted["sha"], intent["target"])
        self.assertEqual(len(w.events("accept_started")), 1)
        self.assertEqual(w.branch_tip(RUN), intent["target"])
        self.assertEqual(run_git(w.repo, "reflog", "show", "--format=%H", f"refs/heads/{RUN}"), reflog)

    def crash_record(self, w, point):
        a = w.next()
        worker = w.launch(a)
        reply = w.perform(a, worker)
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                  "--reply", "-", crash=point, stdin=reply)
        self.assertEqual(r.code, 97, r)
        return a, worker, reply

    def test_crash_after_result_files_before_event_reconciles(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a, worker, reply = self.crash_record(w, "after_result_files")
        folder = w.run_dir / "cards" / "1" / a["dispatch_id"]
        for name in ("brief.md", "report.md", "reply.md"):
            self.assertTrue((folder / name).is_file(), name)
        self.assertEqual(w.events("result_recorded"), [])
        self.assertEqual(w.charges(), [])
        self.assertEqual(w.next()["action"], "wait")
        event = result(w, a, worker, reply)
        self.assertEqual(event["status"], "READY")
        self.assertEqual((folder / "reply.md").read_text(encoding="utf-8"), reply)
        self.assertEqual(len(w.events("result_recorded")), 1)
        self.assertEqual(len(w.charges()), 1)
        self.assertEqual(len(w.attempts()), 1)
        self.assertEqual(w.next()["dispatch_id"], "c1-a1-review-1")

    def test_crash_after_charge_before_attempt_retry_keeps_one_charge(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a, worker, reply = self.crash_record(w, "after_charge_before_attempt")
        self.assertEqual(len(w.charges()), 1)
        self.assertEqual(w.attempts(), [])
        self.assertEqual(w.events("result_recorded"), [])
        event = result(w, a, worker, reply)
        (charge,) = w.charges()
        (attempt,) = w.attempts()
        self.assertEqual(attempt["charge_ids"], [charge["charge_id"]])
        self.assertEqual(event["charge_ids"], [charge["charge_id"]])
        self.assertEqual(len(w.events("result_recorded")), 1)


class Supervisor(EngineCase):
    def test_supervisor_usage_charged_once_across_calls(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.session("sess-1", messages=2)
        w.next()
        w.next()
        sup = [c for c in w.charges() if c["role"] == "supervisor"]
        self.assertEqual([c["charge_id"] for c in sup], ["supervisor:sess-1:1"])
        self.assertEqual(sup[0]["usage"]["output"], 40)
        self.assertEqual(w.meta()["supervisor_sessions"], ["sess-1"])
        w.session("sess-1", messages=1)
        w.next()
        w.next()
        sup = [c for c in w.charges() if c["role"] == "supervisor"]
        self.assertEqual([c["charge_id"] for c in sup], ["supervisor:sess-1:1", "supervisor:sess-1:2"])
        self.assertEqual(sup[1]["usage"]["output"], 20)
        self.assertEqual(w.meta()["supervisor_sessions"], ["sess-1"])


# --------------------------------------------------------------------------
# test_engine_by_risk.py: `card_review = "by-risk"` never accepts a READY that changed nothing
# without a review (final review I8).

class ByRisk(EngineCase):
    def test_ready_at_the_attempt_start_tip_goes_to_review(self):
        w = FakeSupervisor(self, [card(1, risk="low")], config=BY_RISK)
        w.start()
        a = w.next()
        worker = w.launch(a)
        # The implementer writes a file but never commits it, and says READY.
        write(w.workspace(), {"src/uncommitted.txt": "x\n"})
        result(w, a, worker, "Status: READY\nReport: none")
        nxt = w.next()
        self.assertEqual(w.events("accepted"), [])
        self.assertEqual(w.branch_tip(RUN), w.base)
        self.assertEqual((nxt["action"], nxt["kind"]), ("dispatch", "review"))

    def test_low_risk_ready_with_commits_is_still_accepted_without_review(self):
        w = FakeSupervisor(self, [card(1, risk="low")], config=BY_RISK)
        w.start()
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/card1/c1-a1-implement-1.txt"), "c1-a1-implement-1")


# --------------------------------------------------------------------------
# test_engine_decisions.py: a run whose every remaining card waits on a decision ends blocked
# (decision needed), and `unpark` continues it (final review I2, ruling R44).

class DecisionNeeded(EngineCase):
    def test_question_parked_on_a_decision_ends_blocked_and_unpark_continues(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        reason = "decision needed: which storage format"
        w.parks[1] = reason
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"), done["reasons"])
        self.assertIn(f"card 1 parked: {reason}", done["reasons"])
        self.assertTrue(w.paths.handoff.is_file())
        self.assertEqual([e["card"] for e in w.events("accepted")], [2])
        lines = status_lines(w)
        self.assertTrue(any(is_unpark_line(line, 1, w.run_dir, reason) for line in lines), lines)
        # The operator answers and relaunches: the run continues and completes.
        w.cli("unpark", w.run_dir, "--card", "1", "--note", "Use JSON.").ok()
        del w.parks[1]
        w.cli("run", w.run_dir, env=relaunch_env()).ok()
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])
        for n in (1, 2):
            run_git(w.repo, "cat-file", "-e", f"{RUN}:src/card{n}/c{n}-a1-implement-1.txt")

    def test_card_waiting_on_a_decision_card_still_ends_blocked(self):
        w = FakeSupervisor(self, [card(1), card(2, depends=[1]), card(3)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "decision needed: which storage format"
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"), done["reasons"])

    def test_violation_cap_park_ends_blocked_for_a_decision(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": f"rewritten by {a['dispatch_id']}\n"})

        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(1, "review", *({"tamper": tamper} for _ in range(5)))
        done = w.run()[-1]
        (parked,) = w.events("parked")
        self.assertTrue(parked.get("cap_dispatch"))
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"), done["reasons"])
        self.assertTrue(w.paths.handoff.is_file())

    def test_park_that_is_not_a_decision_still_ends_incomplete(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
        w.start()
        w.act(w.next())
        recorded(w, "park", "--card", "1", "--reason", "operator: skip")
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "incomplete", done["reasons"])

    def test_unpark_refuses_on_a_run_it_cannot_reopen_and_hides_the_hint(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
        w.start()
        w.act(w.next())
        recorded(w, "park", "--card", "1", "--reason", "operator: skip")
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "incomplete")
        r = self.fails(w.cli("unpark", w.run_dir, "--card", "1", "--note", "go on"))
        self.assertIn("incomplete", r.stderr)
        self.assertEqual(w.events("unparked"), [])
        lines = status_lines(w)
        self.assertTrue(any(line.startswith("Card 1 parked: operator: skip") for line in lines), lines)
        self.assertFalse(any("unpark" in line for line in lines), lines)
        w.cli("report", w.run_dir).ok()
        text = w.paths.report.read_text(encoding="utf-8")
        self.assertIn("Card 1 parked: operator: skip", text)
        self.assertNotIn(" unpark ", text.split("## Needs your attention")[1].split("## Result")[0])


# --------------------------------------------------------------------------
# test_engine_reply_file.py: R38, the supervisor records a worker's reply through a file,
# never a heredoc.

class ReplyFile(EngineCase):
    def test_dispatch_action_carries_a_git_ignored_reply_file_in_the_scratch_dir(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        path = Path(a["reply_file"])
        self.assertTrue(path.is_absolute())
        ws = w.workspace()
        self.assertEqual(path, ws / ".leadhand" / "1" / a["dispatch_id"] / "reply.txt")
        self.assertIn(ws.resolve(), path.resolve().parents)
        path.write_text("Status: READY\n", encoding="utf-8")
        self.assertEqual(run_git(ws, "check-ignore", str(path)), str(path))     # raises unless ignored
        self.assertNotIn("reply.txt", run_git(ws, "status", "--porcelain", "--untracked-files=all"))

    def test_wait_names_the_same_reply_file(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        w.launch(a)
        wait = w.next()
        self.assertEqual(wait["action"], "wait")
        self.assertEqual(wait["reply_file"], a["reply_file"])

    def test_cli_records_reply_from_a_file_with_spaces_in_its_path(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        self.assertIn(" ", a["reply_file"])
        worker = w.launch(a)
        reply = w.perform(a, worker)
        Path(a["reply_file"]).write_text(reply, encoding="utf-8")
        event = recorded(w, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                         "--reply-file", a["reply_file"])
        self.assertEqual(event["status"], "READY")
        self.assertEqual(Path(event["files"]["reply"]).read_text(encoding="utf-8"), reply)

    def test_reply_and_reply_file_are_exclusive(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        Path(a["reply_file"]).write_text(w.perform(a, worker), encoding="utf-8")
        r = record(w, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                   "--reply", "Status: READY", "--reply-file", a["reply_file"])
        self.assertNotEqual(r.code, 0, r)
        self.assertEqual(w.events("result_recorded"), [])
        self.assertEqual(w.next()["action"], "wait")

    def test_missing_reply_file_falls_back_to_the_report_head_with_a_note(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        w.perform(a, worker)                          # writes the report; the reply file is never written
        event = recorded(w, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                         "--reply-file", a["reply_file"])
        self.assertEqual(event["status"], "READY")
        note = Path(event["files"]["reply"]).read_text(encoding="utf-8")
        self.assertIn("reply file", note)
        self.assertIn(a["reply_file"], note)

    def test_new_request_removes_a_stale_reply_file(self):
        # Dispatch IDs are deterministic, so a reply file or report already at a new request's
        # scratch path is stale: before the first request, and planted by an earlier worker.
        w = FakeSupervisor(self, [card(1)])
        w.start()
        ws = w.workspace()

        def plant(did):
            folder = ws / ".leadhand" / "1" / did
            folder.mkdir(parents=True, exist_ok=True)
            for name in ("reply.txt", "report.md"):
                (folder / name).write_text("Status: READY\nstale\n", encoding="utf-8")
            return folder

        implement = plant("c1-a1-implement-1")
        w.script(1, "implement", {"tamper": lambda world, ws, a: plant("c1-a1-review-1")})
        a = w.next()
        self.assertEqual(Path(a["reply_file"]), implement / "reply.txt")
        self.assertEqual(Path(a["report"]), implement / "report.md")
        self.assertFalse((implement / "reply.txt").exists())
        self.assertFalse((implement / "report.md").exists())
        w.act(a)
        self.assertTrue((ws / ".leadhand" / "1" / "c1-a1-review-1" / "reply.txt").exists())
        review = w.next()
        self.assertEqual(review["dispatch_id"], "c1-a1-review-1")
        folder = ws / ".leadhand" / "1" / "c1-a1-review-1"
        self.assertEqual(Path(review["reply_file"]), folder / "reply.txt")
        self.assertFalse((folder / "reply.txt").exists())
        self.assertFalse((folder / "report.md").exists())


# --------------------------------------------------------------------------
# test_engine_codex_result.py: a Codex-route dispatch's result comes only from the runner's
# result.json (final review I1). The proxy only runs `dispatch`; whatever text it returns is
# never taken as the worker's result.

class CodexResultFromRunner(EngineCase):
    def test_early_still_running_reply_waits_and_starts_no_second_implementer(self):
        w = codex_world(self, [card(1)])
        w.start()
        a = w.next()
        self.assertEqual(a["agent"], "leadhand:codex")
        worker = w.launch(a)
        proxy = w.start_proxy(a, {"sleep": 4})
        wait_runner_codex(w, a["dispatch_id"], proxy)
        codex = codex_start(w)
        # The proxy gives up early and replies with the dispatcher's "still running" line.
        out = result(w, a, worker, f"still running: run the same command again (dispatch {a['dispatch_id']})")
        self.assertEqual(out["action"], "wait", out)
        self.assertEqual(out["dispatch_id"], a["dispatch_id"])
        self.assertEqual(w.events("result_recorded"), [])
        self.assertTrue(codex_alive(codex))
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("wait", a["dispatch_id"]))
        # The timer reports the runner's result once result.json is there; `next` records it.
        r = w.cli("timer", w.run_dir, "--dispatch", a["dispatch_id"]).ok()
        self.assertEqual(r.stdout.splitlines(), ["result recorded"])
        proxy.communicate(timeout=60)
        review = w.next()
        (res,) = w.events("result_recorded")
        self.assertEqual((res["dispatch_id"], res["status"]), (a["dispatch_id"], "READY"))
        self.assertEqual((review["action"], review["kind"]), ("dispatch", "review"))
        implements = [e["dispatch_id"] for e in w.events("dispatch_requested")
                      if e["dispatch_kind"] == "implement"]
        self.assertEqual(implements, [a["dispatch_id"]])
        self.assertEqual(len([r for r in read_jsonl(Path(w.env["FAKE_CODEX_LOG"])) if r.get("event") == "start"]), 1)

    def test_invented_verdict_without_a_worker_is_not_accepted(self):
        cfg = '[roles.reviewer]\ncodex = "gpt-6.1-terra medium"\nprefer = "codex"\n'
        w = codex_world(self, [card(1, route="claude sonnet low", risk="medium")], config=cfg)
        w.start()
        w.act(w.next())                                  # the Claude implementer
        r = w.next()
        self.assertEqual((r["action"], r["kind"], r["agent"]), ("dispatch", "review", "leadhand:codex"))
        worker = w.launch(r)
        # The proxy never ran the command and invents a clean verdict.
        ev = result(w, r, worker, "Verdict: clean\nReport: none")
        self.assertNotEqual(ev.get("verdict"), "clean", ev)
        wd = w.run_dir / "workers" / r["dispatch_id"]
        self.assertEqual(read_json(wd / "result.json")["exit_reason"], "cancelled")
        nxt = w.next()
        self.assertEqual(w.events("accepted"), [])
        self.assertEqual(w.branch_tip(RUN), w.base)
        self.assertEqual(w.events("verdict_recorded"), [])
        self.assertEqual((nxt["action"], nxt["kind"]), ("dispatch", "review"))
        self.assertNotEqual(nxt["dispatch_id"], r["dispatch_id"])

    def test_skipping_a_codex_card_stops_its_runner(self):
        w = codex_world(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        proxy = w.start_proxy(a, {"sleep": 120})
        wd = w.run_dir / "workers" / a["dispatch_id"]
        wait_runner_codex(w, a["dispatch_id"], proxy)
        codex = codex_start(w)
        self.assertTrue(codex_alive(codex))
        # The run skill's skip path: stop the proxy, record it stopped, park the card.
        ev = result(w, a, worker, "Status: SPENT (time limit)", "--stopped")
        self.assertEqual(ev["kind"], "result_recorded")
        self.assertTrue(ev["stopped"])
        self.assertFalse(codex_alive(codex))
        self.assertEqual(read_json(wd / "result.json")["exit_reason"], "cancelled")
        recorded(w, "park", "--card", "1", "--reason", "operator: skip")
        proxy.communicate(timeout=60)
        self.assertEqual(w.next()["action"], "done")


# --------------------------------------------------------------------------
# test_engine_codex_e2e.py: end to end with a Codex worker that never commits, as the real one
# (ruling R39, final review I11, ruling R47): the runner commits its work and the run completes.

class NonCommittingCodex(EngineCase):
    def test_two_card_run_with_gates_completes_with_the_runners_commits(self):
        w = codex_world(self, [card(1, route="claude sonnet low"), card(2, depends=[1], route=CODEX_CARD)],
                        gates=True)
        w.start()
        w.script(2, "implement", {"commit": False})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2])
        (codex,) = [e for e in w.events("result_recorded") if e["dispatch_id"] == "c2-a1-implement-1"]
        self.assertTrue(codex.get("committed"), codex)
        head = w.branch_tip(RUN)
        run_git(w.repo, "merge-base", "--is-ancestor", codex["committed"], head)
        body = run_git(w.repo, "log", "-1", "--format=%B", codex["committed"])
        self.assertIn("Leadhand-Dispatch: c2-a1-implement-1", body.splitlines())
        files = run_git(w.repo, "diff", "--name-only", w.base, head).splitlines()
        self.assertEqual(sorted(files), ["src/card1/c1-a1-implement-1.txt", "src/card2/c2-a1-implement-1.txt"])
        gates = {e["gate"]: e for e in w.events("gate_result")}
        self.assertTrue(gates)
        self.assertTrue(all(g["passed"] and g["examined_sha"] == head for g in gates.values()), gates)


# --------------------------------------------------------------------------
# test_engine_run_branch.py: the run branch moves only by acceptance (final review I4).

class ImplementerGuard(EngineCase):
    def test_implementer_commit_on_the_run_branch_is_a_violation(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"tamper": stray, "status": "QUESTION"})
        w.parks[1] = "decision needed: storage format"
        w.run()
        found = [e for e in w.events("violation") if e.get("stage") == "found" and e.get("by") == "check"]
        self.assertEqual([e["dispatch_id"] for e in found], ["c1-a1-implement-1"])
        self.assertIn(f"refs/heads/{RUN}", found[0]["refs"])
        kept = found[0]["preserved"][f"refs/heads/{RUN}"]
        self.assertEqual(run_git(w.repo, "log", "-1", "--format=%s", kept), "stray commit on the run branch")
        self.assertNotIn("stray commit on the run branch", run_log(w))
        self.assertIn("c1-a1-implement-2", w.dispatch_ids())
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])
        (done,) = w.events("outcome")
        self.assertEqual(done["outcome"], "complete", done["reasons"])


class RunHeadCheck(EngineCase):
    def test_run_branch_moved_between_dispatches_fails_the_run(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.act(w.next())
        self.assertEqual(w.next()["action"], "answer")
        recorded(w, "park", "--card", "1", "--reason", "operator: skip")
        # Someone moves the run branch while no worker is out.
        ws = w.workspace()
        outside = commit_on(w.repo, w.base)
        run_git(ws, "update-ref", f"refs/heads/{RUN}", outside)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"]), ("done", "failed"), done)
        self.assertTrue(any("run branch" in r for r in done["reasons"]), done["reasons"])
        self.assertEqual(w.branch_tip(RUN), outside)

    def test_outcome_checks_the_run_head_after_a_cancel(self):
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        stray(w, w.workspace(), a)               # the worker commits on the run branch, then is cancelled
        w.cli("cancel", w.run_dir).ok()
        self.assertEqual(w.next()["action"], "stop_worker")
        result(w, a, worker, "Status: SPENT (time limit)", "--stopped")
        done = w.next()
        self.assertEqual((done["action"], done["outcome"]), ("done", "failed"), done)


# --------------------------------------------------------------------------
# test_engine_untracked.py: untracked files a worker did not create never land in its card's
# commits (final review I3).

def leave_untracked(world, ws, a):
    write(ws, {"src/card1/half-done.py": "# card 1's unfinished, uncommitted new file\n"})


class EngineSide(EngineCase):
    def test_parked_cards_untracked_file_does_not_land_via_the_next_codex_card(self):
        w = codex_world(self, [card(1, route="claude sonnet low"), card(2, route=CODEX_CARD)], gates=True)
        w.start()
        w.script(1, "implement", {"tamper": leave_untracked, "status": "QUESTION"})
        w.parks[1] = "decision needed: storage format"
        w.script(2, "implement", {"commit": False})      # like real Codex: the runner commits
        w.run()
        run = w.meta()["branch"]
        self.assertEqual([e["card"] for e in w.events("accepted")], [2])
        landed = run_git(w.repo, "diff", "--name-only", w.base, f"refs/heads/{run}").splitlines()
        self.assertEqual(landed, ["src/card2/c2-a1-implement-1.txt"])
        # The file is kept on a leftovers ref, and gone from the workspace.
        found = [e for e in w.events("violation") if e.get("by") == "implementer" and e.get("stage") == "found"]
        self.assertTrue(any("src/card1/half-done.py" in (e.get("untracked") or []) for e in found), found)
        restored = [e for e in w.events("violation") if e.get("by") == "implementer" and e.get("stage") == "restored"]
        kept = [e["saved"] for e in restored if e.get("saved")]
        self.assertTrue(any(run_git(w.repo, "cat-file", "-t", f"{sha}:src/card1/half-done.py", check=False) == "blob"
                            for sha in kept), kept)
        run_id = w.meta()["run_id"]
        leftovers = run_git(w.repo, "for-each-ref", "--format=%(objectname)",
                            f"refs/leadhand/{run_id}/leftovers/").splitlines()
        self.assertTrue(any(run_git(w.repo, "cat-file", "-t", f"{sha}:src/card1/half-done.py", check=False) == "blob"
                            for sha in leftovers), leftovers)
        self.assertFalse((w.workspace() / "src" / "card1" / "half-done.py").exists())

    def test_cancel_partial_commit_leaves_untracked_files_that_were_there_before(self):
        def build_output(world, ws, a):
            write(ws, {"build/out.txt": "reviewer's build output\n"})

        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "review", {"tamper": build_output, "verdict": "defects"})
        w.act(w.next())                          # implement
        w.act(w.next())                          # review: defects, leaves untracked build output
        fix = w.next()
        self.assertEqual(fix["kind"], "fix")
        worker = w.launch(fix)
        ws = w.workspace()
        write(ws, {"src/card1/fix-partial.txt": "half a fix\n"})
        w.cli("cancel", w.run_dir).ok()
        self.assertEqual(w.next()["action"], "stop_worker")
        event = result(w, fix, worker, "Status: SPENT (time limit)", "--stopped")
        self.assertTrue(event.get("partial_commit"), event)
        files = run_git(w.repo, "show", "--name-only", "--format=", event["partial_commit"]).splitlines()
        self.assertEqual(files, ["src/card1/fix-partial.txt"])
        self.assertTrue((ws / "build" / "out.txt").exists())


class RunnerSide(EngineCase):
    def test_runner_commits_only_tracked_changes_and_files_created_during_the_dispatch(self):
        w = DispatchWorld(self)
        w.git("checkout", "--quiet", "-b", DISPATCH_CARD_BRANCH)
        write(w.ws, {"stray/left by another card.txt": "not this dispatch's\n"})
        w.scratch("d1", brief_text=CARD_BRIEF)
        w.scenario([{"type": "thread.started", "thread_id": "{thread}"}, {"type": "turn.started"},
                    {"action": "write_file", "path": "README.md", "text": "changed\n"},
                    {"action": "write_file", "path": "src/new.py", "text": "NEW = 1\n"},
                    {"action": "last_message", "text": "Status: READY\n"},
                    {"type": "turn.completed", "usage": {"input_tokens": 1, "cached_input_tokens": 0,
                                                         "output_tokens": 1}}])
        w.dispatch("d1", kind="implement", branch=DISPATCH_CARD_BRANCH).ok()
        saved = w.result()
        self.assertEqual(saved["status"], "READY")
        self.assertEqual(saved["committed"], w.git("rev-parse", "HEAD"))
        files = w.git("show", "--name-only", "--format=", saved["committed"]).splitlines()
        self.assertEqual(sorted(files), ["README.md", "src/new.py"])
        self.assertTrue((w.ws / "stray" / "left by another card.txt").exists())
        self.assertIn("?? stray/", w.git("status", "--porcelain"))


# --------------------------------------------------------------------------
# Review focus 5: spaces in every path, every call from an unrelated directory.

class Paths(EngineCase):
    def test_spaces_everywhere_from_unrelated_cwd(self):
        w = FakeSupervisor(self, [card(1)])
        elsewhere = temp_root(self, "lh unrelated cwd ")
        env = dict(w.env, PATH=path_without_self(w.env["PATH"]))
        for path in (w.repo, w.docs, w.deck_path):
            self.assertIn(" ", str(path))
        self.assertFalse(elsewhere.resolve().is_relative_to(w.tmp.resolve()))

        r = run_cli("start", w.deck_path, "--repo", w.repo, env=env, cwd=elsewhere).ok()
        run_dir = run_line(r)
        self.assertIn(f"run: {run_dir}", r.stdout.splitlines())
        self.assertTrue(run_dir.resolve().is_relative_to(w.docs.resolve()), run_dir)
        meta = read_json(run_dir / "run.json")
        self.assertEqual(meta["repo"], str(w.repo.resolve()))
        self.assertIn(" ", meta["workspace"])

        a = run_cli("next", run_dir, env=env, cwd=elsewhere).ok().json()
        self.assertEqual((a["action"], a["dispatch_id"]), ("dispatch", "c1-a1-implement-1"))
        self.assertTrue(Path(a["brief"]).is_file())
        scratch = Path(meta["workspace"]) / ".leadhand" / "1" / "c1-a1-implement-1"
        self.assertEqual(Path(a["brief"]), scratch / "brief.md")

        deck = str(run_dir / "deck.md")
        plugin = elsewhere / "plugin dir"
        for args, tail in (((run_dir,), ["run", deck]),
                           ((run_dir, "--plugin-dir", plugin), ["run", deck, "--plugin-dir", str(plugin)])):
            r = run_cli("launch-line", *args, env=env, cwd=elsewhere).ok()
            self.assertEqual(r.stdout.count("\n"), 1, r)
            words = shlex.split(r.stdout)
            self.assertTrue(harness.names_bin(words[0]), words)          # S1-R11: by file
            self.assertEqual(words[1:], tail, r)
            self.assertTrue(harness.is_bin_command(r.stdout[:-1], "", " " + shlex.join(tail)), r)

    def test_spaced_cli_path_from_unrelated_cwd(self):
        # Ruling S1-R14: the CLI's own path gets a space through LEADHAND_TEST_CLI, a symlink to
        # the binary in a directory whose name has a space. The dispatch line and the launch line
        # quote it, and the dispatch line runs to completion through it.
        w = codex_world(self, [card(1)])
        elsewhere = temp_root(self, "lh unrelated cwd ")
        link = temp_root(self, "lh cli ") / "lead hand bin" / "leadhand"
        link.parent.mkdir()
        link.symlink_to(harness.BIN)
        self.assertTrue(harness.names_bin(str(link)))
        w.env.update(LEADHAND_TEST_CLI=str(link), PATH=path_without_self(w.env["PATH"]))
        for path in (w.repo, w.docs, w.deck_path, link):
            self.assertIn(" ", str(path))

        run_dir = run_line(run_cli("start", w.deck_path, "--repo", w.repo, env=w.env, cwd=elsewhere).ok())
        w.run_dir = run_dir
        a = run_cli("next", run_dir, env=w.env, cwd=elsewhere).ok().json()
        self.assertEqual((a["action"], a["agent"]), ("dispatch", "leadhand:codex"), a)
        self.assertEqual(len(a["message"].splitlines()), 1)
        argv = shlex.split(a["message"])
        self.assertEqual(argv[0], str(link))                    # the spaced path, quoted as one word
        self.assertTrue(harness.names_bin(argv[0]), argv)      # S1-R11: by file
        self.assertEqual(argv[1], "dispatch")
        self.assertEqual(argv[argv.index("--run") + 1], str(run_dir))

        r = run_cli("launch-line", run_dir, env=w.env, cwd=elsewhere).ok()
        words = shlex.split(r.stdout)
        self.assertEqual(words, [str(link), "run", str(run_dir / "deck.md")], r)
        self.assertTrue(harness.is_bin_command(r.stdout[:-1], "", " " + shlex.join(words[1:])), r)

        # The printed dispatch line runs, from the unrelated directory, through the spaced path.
        recorded(w, "started", "--dispatch", a["dispatch_id"], "--worker", "agent-proxy")
        line, _ = w.proxy_argv_env(a, {})
        self.assertEqual(line, argv)
        out = w.proxy(a, {}, cwd=elsewhere)
        self.assertEqual(out.code, 0, out)
        self.assertEqual(out.stdout.splitlines()[0], "Status: READY", out)
        self.assertEqual(read_json(run_dir / "workers" / a["dispatch_id"] / "result.json")["status"], "READY")
        self.assertEqual(run_git(w.repo, "show", f"{CARD1}:src/card1/c1-a1-implement-1.txt"), "c1-a1-implement-1")


# --------------------------------------------------------------------------
# test_test_hooks.py: the clock offset

class ClockOffsetHook(EngineCase):
    def test_clock_offset_is_honoured_only_under_leadhand_testing(self):
        """Covers limit L4 (test hooks need LEADHAND_TESTING=1)."""
        # Observed through the deadline `record started` gives a worker (the CLI's clock plus the
        # dispatch's budget): one fresh run per case, the same card, so the same budget.
        def deadline_lead(**env) -> float:
            w = FakeSupervisor(self, [card(1)])
            w.start()
            a = w.next()
            self.assertEqual(a["action"], "dispatch", a)
            before = time.time()
            e = run_cli("record", w.run_dir, "started", "--dispatch", a["dispatch_id"], "--worker", "agent0001",
                        env=dict(w.env, **env), cwd=w.tmp).ok().json()
            self.assertEqual(e["kind"], "worker_started", e)
            return datetime.fromisoformat(e["deadline"]).timestamp() - before

        plain = deadline_lead()
        shifted = deadline_lead(LEADHAND_TEST_CLOCK_OFFSET="3600")
        ignored = deadline_lead(LEADHAND_TEST_CLOCK_OFFSET="3600", LEADHAND_TESTING="0")
        self.assertAlmostEqual(shifted - plain, 3600, delta=60)
        self.assertAlmostEqual(ignored, plain, delta=60)


# --------------------------------------------------------------------------
# The card-loop-only test mode (LEADHAND_TEST_CARD_LOOP_ONLY)

class CardLoopOnlyHook(EngineCase):
    def test_card_loop_only_mode_ends_the_run_after_its_cards(self):
        """Covers R34: with LEADHAND_TEST_CARD_LOOP_ONLY=1 (honoured under LEADHAND_TESTING) the
        engine stops once the cards are done: `done` with no outcome and no report, and no
        end-of-run gate. Without it the same deck goes on to the final review."""
        loop = FakeSupervisor(self, [card(1)])
        self.assertEqual(loop.env.get("LEADHAND_TEST_CARD_LOOP_ONLY"), "1")
        loop.start()
        done = loop.run()[-1]
        self.assertEqual((done["action"], done["outcome"], done["report"]), ("done", None, None), done)
        self.assertEqual(done["cards"], [{"card": 1, "state": "accepted", "reason": ""}], done)
        self.assertEqual(loop.events("outcome"), [])
        self.assertEqual(loop.run_dispatch_ids(), [])
        self.assertEqual([e for e in loop.events("dispatch_requested") if e.get("card") is None], [])

        gated = FakeSupervisor(self, [card(1)], gates=True)
        self.assertNotIn("LEADHAND_TEST_CARD_LOOP_ONLY", gated.env)
        gated.start()
        for _ in range(20):
            a = gated.next()
            if a["action"] == "dispatch" and a["card"] is None:
                break
            gated.act(a)
        self.assertEqual((a["action"], a["kind"]), ("dispatch", "final_review"), a)


if __name__ == "__main__":
    unittest.main()
