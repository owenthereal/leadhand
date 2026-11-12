import contextlib
import io
import json
import shlex
import time
import unittest
from pathlib import Path

from fakeworld import CLI, FakeWorld, card
from gitfixture import run_git
from leadhand import engine, procs
from leadhand.engine import EngineError
from leadhand.state import append_event, run_paths


def without_deadline(action):
    return {k: v for k, v in action.items() if k != "deadline"}


def wait_for(cond, timeout=20.0, step=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = cond()
        if value:
            return value
        time.sleep(step)
    raise AssertionError(f"timed out after {timeout}s waiting for {cond}")


class HappyPath(unittest.TestCase):
    def test_happy_path_two_cards_accepted_in_order(self):
        w = FakeWorld(self, [card(1), card(2, depends=[1], risk="medium")])
        w.start()
        actions = w.run()

        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1",
                                            "c2-a1-implement-1", "c2-a1-review-1"])
        first, review = w.started[0], w.started[1]
        self.assertEqual((first["agent"], first["agent_model"], first["agent_effort"]),
                         ("leadhand:implementer", "sonnet", "medium"))
        self.assertEqual(first["route"], {"vendor": "claude", "model": "sonnet", "effort": "medium"})
        self.assertIsNone(first["resume"])
        self.assertIn(first["report"], first["message"])
        self.assertEqual(review["agent"], "leadhand:reviewer")
        self.assertEqual(actions[-1]["action"], "done")

        accepted = w.events("accepted")
        self.assertEqual([e["card"] for e in accepted], [1, 2])
        run_tip = w.branch_tip("leadhand/demo")
        self.assertEqual(run_tip, accepted[1]["sha"])
        run_git(w.repo, "merge-base", "--is-ancestor", accepted[0]["sha"], run_tip)
        cuts = {e["card"]: e for e in w.events("card_cut")}
        self.assertEqual(cuts[1]["base"], w.base)
        self.assertEqual(cuts[2]["base"], accepted[0]["sha"])
        for d in ("c1-a1-implement-1", "c2-a1-implement-1"):
            n = d[1]
            self.assertEqual(run_git(w.repo, "show", f"leadhand/demo:src/card{n}/{d}.txt"), d)

        # every worker charged once under its role, with one attempt record naming the charge
        charges = {c.charge_id: c for c in w.charges()}
        self.assertEqual(len(charges), 4)
        attempts = w.attempts()
        self.assertEqual([a["dispatch"] for a in attempts], w.dispatch_ids())
        for a in attempts:
            self.assertEqual(len(a["charge_ids"]), 1)
            c = charges[a["charge_ids"][0]]
            self.assertEqual(c.charge_id, f"{a['dispatch']}:claude:{a['worker_id']}")
            self.assertEqual(c.role, a["role"])
            self.assertTrue(c.usage.known)
        # the run folder keeps a copy of each dispatch's brief, report and reply
        folder = w.run_dir / "cards" / "1" / "c1-a1-review-1"
        for name in ("brief.md", "report.md", "reply.md"):
            self.assertTrue((folder / name).is_file(), name)

    def test_requested_dispatch_is_repeated_until_started(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a1 = w.next()
        a2 = w.next()
        self.assertEqual(a1["action"], "dispatch")
        self.assertEqual(without_deadline(a1), without_deadline(a2))
        self.assertEqual(len(w.events("dispatch_requested")), 1)
        engine.record(w.run_dir, "started", dispatch_id=a1["dispatch_id"], worker_id="agent-x")
        a3 = w.next()
        self.assertEqual(a3["action"], "wait")
        self.assertEqual((a3["dispatch_id"], a3["worker_id"]), (a1["dispatch_id"], "agent-x"))
        self.assertTrue(a3["reason"])
        self.assertEqual(len(w.events("dispatch_requested")), 1)

    def test_started_dispatch_returns_wait_then_stop_worker_after_deadline(self):
        w = FakeWorld(self, [card(1)])
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
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                              reply="(the worker was stopped)", stopped=True)
        self.assertEqual(event["status"], "SPENT")
        (charge,) = w.charges()
        self.assertTrue(charge.possibly_incomplete)
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"], nxt["resume"]),
                         ("dispatch", "c1-a2-implement-1", None))

    def test_duplicate_result_is_harmless(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        reply = w.perform(a, worker)
        e1 = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker, reply=reply)
        e2 = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker, reply=reply)
        e3 = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                           reply="Status: SPENT\nsomething else")
        self.assertEqual(e1, e2)
        self.assertEqual(e1, e3)
        self.assertEqual(len(w.events("result_recorded")), 1)
        self.assertEqual(len(w.charges()), 1)
        self.assertEqual(len(w.attempts()), 1)
        self.assertEqual(w.next()["dispatch_id"], "c1-a1-review-1")


class Loop(unittest.TestCase):
    def test_repeated_answers_and_reviews_get_distinct_ids(self):
        w = FakeWorld(self, [card(1)])
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
        w = FakeWorld(self, [card(1), card(2, risk="medium"), card(3, hard=True)],
                      config='[run]\ncard_review = "by-risk"\n')
        w.start()
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c2-a1-implement-1", "c2-a1-review-1",
                                            "c3-a1-implement-1", "c3-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2, 3])
        hard_impl, hard_review = w.started[3], w.started[4]
        self.assertEqual(hard_impl["route"]["model"], "opus")
        self.assertEqual(hard_review["route"]["model"], "opus")

    def test_defects_get_one_fix_round_then_rereview_on_production_change(self):
        w = FakeWorld(self, [card(1)])
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
        fix_brief = Path(fix["brief"]).read_text()
        self.assertIn(str(w.run_dir / "cards" / "1" / "c1-a1-review-1" / "report.md"), fix_brief)
        self.assertEqual(len(w.events("fix_round")), 1)
        (accepted,) = w.events("accepted")
        self.assertEqual(run_git(w.repo, "show", f"{accepted['sha']}:src/widget.py"), "fixed")

    def test_clean_verdict_with_medium_finding_counts_as_defects(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "clean", "findings": ["[medium] src/a.py:1 — leaks — close it"]})
        w.script(1, "fix", {"files": {"src/a.py": "closed\n"}})
        w.run()
        self.assertEqual(w.dispatch_ids()[:3], ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1"])
        verdicts = w.events("verdict_recorded")
        self.assertEqual(verdicts[0]["verdict"], "defects")

    def test_clean_rereview_noting_fixed_findings_stays_clean(self):
        # Only outstanding findings (numbered lines under ## Findings) overrule the verdict.
        report = ("Verdict: clean\n## Findings\nNone\n## Notes\n"
                  "1. [high] src/widget.py:1 — fixed; regression test passes.\n")
        head = engine._head(report, "", None, False)
        self.assertEqual(head["verdict"], "clean")
        self.assertNotIn("cross_checked", head)

    def test_clean_verdict_cross_check_reads_only_findings_lines(self):
        def verdict(report):
            return engine._head(report, "", None, False)["verdict"]
        self.assertEqual(verdict("Verdict: clean\n## Findings\n2. [High] src/a.py:3 — leaks — close it\n"
                                 "## Notes\nRead the diff.\n"), "defects")
        self.assertEqual(verdict("Verdict: clean\n## Findings\n  1) [medium] src/a.py:3 — x — y\n"), "defects")
        self.assertEqual(verdict("Verdict: clean\n## Findings\n1. [low] src/a.py:3 — nit — rename\n"
                                 "## Notes\n- [high] earlier round: fixed\n1. [medium] fixed\n"), "clean")
        self.assertEqual(verdict("Verdict: clean\n[high] src/a.py:3 outside any section\n"
                                 "## Findings\nNone\n"), "clean")

    def test_clean_verdict_cross_check_counts_bullet_findings_lines(self):
        def verdict(report):
            return engine._head(report, "", None, False)["verdict"]
        self.assertEqual(verdict("Verdict: clean\n## Findings\n- [high] src/a.py:3 — leaks — close it\n"), "defects")
        self.assertEqual(verdict("Verdict: clean\n## Findings\n  * [Medium] src/a.py:3 — x — y\n"
                                 "## Notes\nRead the diff.\n"), "defects")
        self.assertEqual(verdict("Verdict: clean\n## Findings\n- [low] src/a.py:3 — nit — rename\n"
                                 "## Notes\n- [high] earlier round: fixed\n* [medium] fixed\n"), "clean")

    def test_test_only_fix_returns_check_diff(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "defects", "findings": ["[medium] tests/test_widget.py — weak — assert more"]})
        w.script(1, "fix", {"files": {"tests/test_widget.py": "assert more\n", "docs/notes.md": "note\n"}})
        for a in iter(w.next, None):
            if a["action"] == "check_diff":
                break
            w.act(a)
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-fix-1"])
        self.assertEqual((a["card"], a["dispatch_id"]), (1, "c1-a1-fix-1"))
        self.assertEqual(sorted(a["paths"]), ["docs/notes.md", "tests/test_widget.py"])
        diff = Path(a["diff"])
        self.assertTrue(diff.is_relative_to(w.run_dir / "cards" / "1"))
        self.assertIn("+assert more", diff.read_text())
        self.assertEqual(w.next(), a)                       # repeated until recorded
        engine.record(w.run_dir, "diff-check", card=1, ok=True, note="only tests and docs")
        done = w.run()
        self.assertEqual(done[-1]["action"], "done")
        self.assertNotIn("c1-a1-rereview-1", w.dispatch_ids())
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_defects_after_fix_round_escalate_with_fresh_worker(self):
        w = FakeWorld(self, [card(1)])
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
        brief = Path(second["brief"]).read_text()
        self.assertIn(str(w.run_dir / "cards" / "1" / "c1-a1-fix-1" / "report.md"), brief)
        self.assertIn(str(w.run_dir / "cards" / "1" / "c1-a1-rereview-1" / "report.md"), brief)
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_spent_escalates_and_exhausted_ladder_parks(self):
        w = FakeWorld(self, [card(1)])
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
        w = FakeWorld(self, [card(1), card(2)], installed=("claude", "codex"),
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
        w = FakeWorld(self, [card(1)])
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
        w = FakeWorld(self, [card(1)])
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        a = w.next()
        worker = w.launch(a)
        engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                      reply=w.perform(a, worker))
        ask = w.next()
        self.assertEqual((ask["action"], ask["card"], ask["dispatch_id"], ask["worker_id"]),
                         ("answer", 1, "c1-a1-implement-1", worker))
        self.assertIn("Which storage format", ask["question"])
        self.assertTrue(Path(ask["report"]).is_file())
        self.assertEqual(w.next(), ask)
        engine.record(w.run_dir, "ruling", card=1, text="Use JSON; it is simpler.")
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "ruling", card=1, text="again")
        ans = w.next()
        self.assertEqual((ans["action"], ans["kind"], ans["dispatch_id"], ans["resume"]),
                         ("dispatch", "answer", "c1-a1-answer-1", {"worker_id": worker}))
        self.assertIn("> Use JSON; it is simpler.", Path(ans["brief"]).read_text())
        (ruling,) = w.events("ruling")
        self.assertEqual(ruling["text"], "Use JSON; it is simpler.")
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_parked_card_skips_dependents_and_continues_independent(self):
        w = FakeWorld(self, [card(1), card(2, depends=[1]), card(3)])
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision: storage format"
        actions = w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c3-a1-implement-1", "c3-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [3])
        cuts = {e["card"]: e for e in w.events("card_cut")}
        self.assertNotIn(2, cuts)
        self.assertEqual(cuts[3]["base"], w.base)
        done = actions[-1]
        self.assertEqual(done["action"], "done")
        reasons = {c["card"]: c for c in done["cards"]}
        self.assertEqual(reasons[1]["state"], "parked")
        self.assertIn("product decision", reasons[1]["reason"])
        self.assertEqual(reasons[2]["state"], "pending")
        self.assertIn("parked card 1", reasons[2]["reason"])
        self.assertEqual(reasons[3]["state"], "accepted")


class Start(unittest.TestCase):
    def test_start_returns_existing_unfinished_run_unless_new(self):
        w = FakeWorld(self, [card(1)])
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

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            again = engine.start(w.deck_path, repo=w.repo)
        self.assertEqual(again, first)
        self.assertIn(f"existing run: {first}", out.getvalue())

        with contextlib.redirect_stdout(io.StringIO()):
            fresh = engine.start(w.deck_path, repo=w.repo, new=True)
        self.assertNotEqual(fresh, first)
        self.assertTrue((fresh / "run.json").is_file())

        for run in (first, fresh):
            append_event(run_paths(run), "outcome", outcome="incomplete", qualifier="", reasons=[])
        with contextlib.redirect_stdout(io.StringIO()):
            third = engine.start(w.deck_path, repo=w.repo)
        self.assertNotIn(third, (first, fresh))

        # the CLI, from an unrelated directory
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(f"existing run: {third}", r.stdout)

    def test_start_never_reuses_an_unfinished_run_of_another_clone(self):
        # Two clones share the docs_dir and the deck file: start against the second clone
        # must create its own run, not return the first clone's.
        w = FakeWorld(self, [card(1)])
        first = w.start()
        clone = w.tmp / "second clone"
        run_git(w.tmp, "clone", "--quiet", str(w.repo), str(clone))
        with contextlib.redirect_stdout(io.StringIO()) as out:
            second = engine.start(w.deck_path, repo=clone)
        self.assertNotEqual(second, first, out.getvalue())
        self.assertNotIn("existing run", out.getvalue())
        meta = json.loads((second / "run.json").read_text(encoding="utf-8"))
        self.assertEqual(meta["repo"], str(clone.resolve()))
        self.assertTrue(Path(meta["workspace"]).resolve().is_relative_to(clone.resolve()))
        # each clone still finds its own unfinished run
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(engine.start(w.deck_path, repo=w.repo), first)
            self.assertEqual(engine.start(w.deck_path, repo=clone), second)

    def test_start_never_reuses_another_runs_worktree(self):
        # docs_dir changed between two same-day runs of the same deck: the second run's
        # directory has the same name, so its default worktree path is the first run's.
        w = FakeWorld(self, [card(1)], gates=True)
        first = w.start()
        a = w.next()
        self.assertEqual(a["action"], "dispatch")
        w.launch(a)                                     # the first run's worker is active
        ws1 = w.workspace()
        branch1 = run_git(ws1, "symbolic-ref", "--short", "HEAD")
        head1 = run_git(ws1, "rev-parse", "HEAD")
        self.assertEqual(branch1, "leadhand/demo--card-1")

        w.config_file.write_text(f'docs_dir = "{w.tmp / "other docs"}"\n', encoding="utf-8")
        with contextlib.redirect_stdout(io.StringIO()):
            second = engine.start(w.deck_path, repo=w.repo)
        self.assertEqual(second.name, first.name)
        meta = json.loads((second / "run.json").read_text(encoding="utf-8"))
        ws2 = Path(meta["workspace"])
        self.assertNotEqual(ws2.resolve(), ws1.resolve())
        self.assertEqual(ws2.name, f"{first.name}-2")
        self.assertEqual(run_git(ws2, "symbolic-ref", "--short", "HEAD"), meta["branch"])
        engine.next_action(second)
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
        w = FakeWorld(self, [card(1)], config='[after_run]\nstep = "pr"\n')
        with self.assertRaises(EngineError) as cm:
            w.start()
        self.assertIn("not supported in this version: after_run.step = pr", str(cm.exception))
        self.assertFalse((w.docs / "runs").exists())

        s = FakeWorld(self, [card(1, role="scout")])
        with self.assertRaises(EngineError) as cm:
            s.start()
        self.assertIn("not supported in this version: scout cards", str(cm.exception))
        r = s.cli("start", s.deck_path, "--repo", s.repo)
        self.assertEqual(r.returncode, 1)
        self.assertIn("scout cards", r.stderr)

    def test_check_and_start_refuse_step_limits_claude_workers_cannot_honour(self):
        w = FakeWorld(self, [card(1)], config='[limits]\nimplementer = { steps = 90, minutes = 60 }\n')
        msg = ("not supported in this version: limits.implementer.steps = 90 "
               "(Claude workers use the agent file's maxTurns = 150)")
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn(msg, r.stdout + r.stderr)
        with self.assertRaises(EngineError) as cm:
            w.start()
        self.assertIn(msg, str(cm.exception))
        self.assertFalse((w.docs / "runs").exists())

    def test_deck_changed_since_start_refuses(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        digest = w.meta()["deck_digest"]
        with open(w.deck_path, "a", encoding="utf-8") as f:
            f.write("\nContext: edited after the start.\n")
        with self.assertRaises(EngineError) as cm:
            w.next()
        self.assertEqual(str(cm.exception), f"deck changed since start (expected {digest})")
        self.assertEqual(w.events("dispatch_requested"), [])
        r = w.cli("next", w.run_dir)
        self.assertEqual(r.returncode, 1)
        self.assertIn("deck changed since start", r.stderr)


class Records(unittest.TestCase):
    def test_reply_without_status_blocks_and_keeps_raw(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        raw = "I changed some things.\nIt went fine, mostly.\n"
        w.script(1, "implement", {"report": False, "reply": raw})
        a = w.next()
        worker = w.launch(a)
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                              reply=w.perform(a, worker))
        self.assertEqual((event["status"], event["blocked_by"]), ("BLOCKED", ""))
        self.assertTrue(event["raw_kept"])
        kept = w.run_dir / "cards" / "1" / "c1-a1-implement-1" / "reply.md"
        self.assertEqual(kept.read_text(), raw)
        retry = w.next()
        self.assertEqual((retry["action"], retry["dispatch_id"]), ("dispatch", "c1-a1-implement-2"))

    def test_turn_limit_reply_is_spent(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        w.script(1, "implement", {"report": False,
                                  "reply": "NOTE: this agent stopped at its 150-turn limit before finishing.\n"})
        a = w.next()
        worker = w.launch(a)
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                              reply=w.perform(a, worker))
        self.assertEqual(event["status"], "SPENT")

    def test_invalid_record_transition_raises(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "result", dispatch_id="c9-a1-implement-1", worker_id="x", reply="Status: READY")
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "started", dispatch_id="c1-a1-implement-1", worker_id="x")
        a = w.next()
        with self.assertRaises(EngineError):            # no worker started yet
            engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id="x", reply="Status: READY")
        with self.assertRaises(EngineError):            # no question pending
            engine.record(w.run_dir, "ruling", card=1, text="do it")
        with self.assertRaises(EngineError):            # no diff check pending
            engine.record(w.run_dir, "diff-check", card=1, ok=True, note="")
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "park", card=7, reason="no such card")
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "nonsense")
        engine.record(w.run_dir, "started", dispatch_id=a["dispatch_id"], worker_id="w1")
        engine.record(w.run_dir, "started", dispatch_id=a["dispatch_id"], worker_id="w1")   # repeat: harmless
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "started", dispatch_id=a["dispatch_id"], worker_id="w2")
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id="w2", reply="Status: READY")
        self.assertEqual(len(w.events("worker_started")), 1)
        self.assertEqual(w.events("result_recorded"), [])
        engine.record(w.run_dir, "park", card=1, reason="operator")
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "park", card=1, reason="twice")

    def test_timer_returns_at_deadline(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        w.launch(a)
        w.advance(61 * 60)
        self.assertEqual(engine.timer(w.run_dir, a["dispatch_id"], interval=0.01), "deadline")

    def test_cli_records_reply_from_stdin(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        r = w.cli("record", w.run_dir, "started", "--dispatch", a["dispatch_id"], "--worker", "agent-cli")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)["worker_id"], "agent-cli")
        reply = w.perform(a, "agent-cli")
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", "agent-cli",
                  "--reply", "-", stdin=reply)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)["status"], "READY")
        r = w.cli("next", w.run_dir)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)["dispatch_id"], "c1-a1-review-1")
        r = w.cli("timer", w.run_dir, "--dispatch", a["dispatch_id"])
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("result recorded", r.stdout)


class Codex(unittest.TestCase):
    def test_codex_route_dispatches_proxy_with_null_agent_model(self):
        w = FakeWorld(self, [card(1)], installed=("claude", "codex"))
        w.start()
        before = time.time()
        a = w.next()
        self.assertEqual(a["agent"], "leadhand:codex")
        self.assertIsNone(a["agent_model"])
        self.assertIsNone(a["agent_effort"])
        self.assertEqual(a["route"], {"vendor": "codex", "model": "gpt-6.1-terra", "effort": "high"})
        argv = shlex.split(a["message"])
        self.assertEqual(argv[:2], [str(CLI), "dispatch"])
        self.assertEqual(argv[argv.index("--dispatch-id") + 1], "c1-a1-implement-1")
        self.assertEqual(argv[argv.index("--cwd") + 1], str(w.workspace()))
        self.assertEqual(argv[argv.index("--run") + 1], str(w.run_dir))
        self.assertNotIn("--resume", argv)
        deadline = engine._parse_time(a["deadline"])
        self.assertAlmostEqual(deadline - before, 65 * 60, delta=30)

        worker = w.launch(a)
        reply = w.perform(a, worker)
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker, reply=reply)
        self.assertEqual(event["status"], "READY")
        self.assertEqual(event["tip"], w.branch_tip("leadhand/demo--card-1"))
        self.assertTrue(event["session_id"])
        roles = sorted(c.role for c in w.charges())
        self.assertEqual(roles, ["implementer", "proxy"])
        proxy = next(c for c in w.charges() if c.role == "proxy")
        self.assertEqual(proxy.charge_id, f"{a['dispatch_id']}:claude:{worker}")
        review = w.next()
        self.assertEqual((review["kind"], review["agent"], review["route"]["vendor"]),
                         ("review", "leadhand:reviewer", "claude"))
        w.act(review)
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_stop_worker_for_codex_cancels_runner_and_records_before_returning(self):
        w = FakeWorld(self, [card(1)], installed=("claude", "codex"))
        w.start()
        a = w.next()
        worker = w.launch(a)
        proxy = w.start_proxy(a, {"sleep": 120})
        wd = w.run_dir / "workers" / a["dispatch_id"]
        runner = wait_for(lambda: (json.loads((wd / "runner.json").read_text())
                                   if (wd / "runner.json").exists() else {}).get("codex"))
        self.assertEqual(w.next()["action"], "wait")
        w.advance(66 * 60)
        stop = w.next()
        self.assertEqual(stop, {"action": "stop_worker", "dispatch_id": a["dispatch_id"], "worker_id": worker,
                                "vendor": "codex", "record_result": False})
        (result,) = w.events("result_recorded")
        self.assertEqual((result["status"], result["stopped"]), ("SPENT", True))
        self.assertEqual(json.loads((wd / "result.json").read_text())["exit_reason"], "cancelled")
        self.assertFalse(procs.same_process(runner["pid"], runner["start"]))
        proxy.communicate(timeout=60)
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("dispatch", "c1-a2-implement-1"))


class Crashes(unittest.TestCase):
    def drive_to(self, w, kind):
        """Act until the next action is a dispatch of `kind`; return it unacted."""
        for _ in range(30):
            a = w.next()
            if a["action"] == "dispatch" and a["kind"] == kind:
                return a
            w.act(a)
        raise AssertionError(f"no {kind} dispatch")

    def test_crash_after_fast_forward_before_accepted_reconciles(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        w.act(self.drive_to(w, "review"))
        r = w.cli("next", w.run_dir, crash="after_fast_forward")
        self.assertEqual(r.returncode, 97, r.stderr)
        (intent,) = w.events("accept_started")
        self.assertEqual(w.events("accepted"), [])
        self.assertEqual(w.branch_tip("leadhand/demo"), intent["target"])
        reflog = run_git(w.repo, "reflog", "show", "--format=%H", "refs/heads/leadhand/demo")
        done = w.next()
        self.assertEqual(done["action"], "done")
        (accepted,) = w.events("accepted")
        self.assertEqual(accepted["sha"], intent["target"])
        self.assertEqual(len(w.events("accept_started")), 1)
        self.assertEqual(w.branch_tip("leadhand/demo"), intent["target"])
        self.assertEqual(run_git(w.repo, "reflog", "show", "--format=%H", "refs/heads/leadhand/demo"), reflog)

    def crash_record(self, w, point):
        a = w.next()
        worker = w.launch(a)
        reply = w.perform(a, worker)
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                  "--reply", "-", crash=point, stdin=reply)
        self.assertEqual(r.returncode, 97, r.stderr)
        return a, worker, reply

    def test_crash_after_result_files_before_event_reconciles(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a, worker, reply = self.crash_record(w, "after_result_files")
        folder = w.run_dir / "cards" / "1" / a["dispatch_id"]
        for name in ("brief.md", "report.md", "reply.md"):
            self.assertTrue((folder / name).is_file(), name)
        self.assertEqual(w.events("result_recorded"), [])
        self.assertEqual(w.charges(), [])
        self.assertEqual(w.next()["action"], "wait")
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker, reply=reply)
        self.assertEqual(event["status"], "READY")
        self.assertEqual((folder / "reply.md").read_text(), reply)
        self.assertEqual(len(w.events("result_recorded")), 1)
        self.assertEqual(len(w.charges()), 1)
        self.assertEqual(len(w.attempts()), 1)
        self.assertEqual(w.next()["dispatch_id"], "c1-a1-review-1")

    def test_crash_after_charge_before_attempt_retry_keeps_one_charge(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a, worker, reply = self.crash_record(w, "after_charge_before_attempt")
        self.assertEqual(len(w.charges()), 1)
        self.assertEqual(w.attempts(), [])
        self.assertEqual(w.events("result_recorded"), [])
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker, reply=reply)
        (charge,) = w.charges()
        (attempt,) = w.attempts()
        self.assertEqual(attempt["charge_ids"], [charge.charge_id])
        self.assertEqual(event["charge_ids"], [charge.charge_id])
        self.assertEqual(len(w.events("result_recorded")), 1)


class Supervisor(unittest.TestCase):
    def test_supervisor_usage_charged_once_across_calls(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        w.session("sess-1", messages=2)
        w.next()
        w.next()
        sup = [c for c in w.charges() if c.role == "supervisor"]
        self.assertEqual([c.charge_id for c in sup], ["supervisor:sess-1:1"])
        self.assertEqual(sup[0].usage.output, 40)
        self.assertEqual(w.meta()["supervisor_sessions"], ["sess-1"])
        w.session("sess-1", messages=1)
        w.next()
        w.next()
        sup = [c for c in w.charges() if c.role == "supervisor"]
        self.assertEqual([c.charge_id for c in sup], ["supervisor:sess-1:1", "supervisor:sess-1:2"])
        self.assertEqual(sup[1].usage.output, 20)
        self.assertEqual(w.meta()["supervisor_sessions"], ["sess-1"])


if __name__ == "__main__":
    unittest.main()
