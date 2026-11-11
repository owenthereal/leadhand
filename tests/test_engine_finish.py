"""Engine: commit-bound end-of-run gates, bounded fix waves, the outcome and the report."""
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock

from fakeworld import CLI, FakeWorld, card
from gitfixture import commit, run_git, write
from leadhand import engine
from leadhand.state import append_event, read_events

RUN = "leadhand/demo"
FINAL = "leadhand/demo--card-final"


def gates(w):
    """(gate, examined_sha, passed) for every gate result, in order."""
    return [(e["gate"], e["examined_sha"], e["passed"]) for e in w.events("gate_result")]


def gate_names(w):
    return [e["gate"] for e in w.events("gate_result")]


def outcome(w):
    return w.events("outcome")[-1]


def runs_lines(w):
    path = w.docs / "runs.md"
    if not path.is_file():
        return []
    return [ln for ln in path.read_text(encoding="utf-8").splitlines() if str(w.run_dir) in ln]


class Complete(unittest.TestCase):
    def test_complete_run_writes_outcome_and_report(self):
        w = FakeWorld(self, [card(1), card(2, depends=[1])], gates=True)
        w.start()
        done = w.run()[-1]

        head = w.branch_tip(RUN)
        self.assertEqual(gates(w), [("final_review", head, True), ("evidence", head, True),
                                    ("verify", head, True)])
        self.assertEqual(w.run_dispatch_ids(), ["run-final_review-1", "run-evidence-1", "run-verify-1"])
        roles = [a["role"] for a in w.run_started]
        self.assertEqual(roles, ["final_reviewer", "verifier", "verifier"])
        self.assertTrue(all(a["card"] is None for a in w.run_started))

        # the briefs name the run branch, the base and the accepted cards
        review_brief = Path(w.run_started[0]["brief"]).read_text()
        self.assertIn(f"{w.base}..{head}", review_brief)
        evidence_brief = Path(w.run_started[1]["brief"]).read_text()
        base_wt = w.repo / ".worktrees" / f"{w.run_dir.name}--base"
        self.assertIn(str(base_wt), evidence_brief)
        self.assertEqual(run_git(base_wt, "rev-parse", "HEAD"), w.base)
        for a in w.run_started:
            self.assertIn(f"Branch: {RUN}", Path(a["brief"]).read_text())
        # every gate ran on the run branch at the run head, guarded like a check
        for e in w.events("dispatch_requested"):
            if e.get("gate"):
                self.assertEqual((e["expected_branch"], e["expected_head"]), (RUN, head))
        self.assertEqual([e["gate"] for e in w.events("gate_started")], ["final_review", "evidence", "verify"])

        (out,) = w.events("outcome")
        self.assertEqual((out["outcome"], out["qualifier"], out["next_step"]), ("complete", "", "none"))
        (written,) = w.events("report_written")
        self.assertGreater(written["seq"], out["seq"])
        report = w.paths.report
        self.assertTrue(report.is_file())
        self.assertIn("Outcome: complete", report.read_text())
        self.assertIn(f"final_review: passed on commit {head[:12]}", report.read_text())
        self.assertFalse(w.paths.handoff.exists())
        self.assertEqual((done["action"], done["outcome"], done["report"]), ("done", "complete", str(report)))

        # repeating next changes nothing
        self.assertEqual(w.next()["outcome"], "complete")
        self.assertEqual(len(w.events("outcome")), 1)
        self.assertEqual(len(w.events("report_written")), 1)

    def test_runs_md_line_appended_once(self):
        w = FakeWorld(self, [card(1), card(2)], title="Widgets | phase 1", gates=True)
        w.start()
        w.run()
        w.next()
        w.next()
        (line,) = runs_lines(w)
        cells = [c.strip() for c in re.split(r"(?<!\\)\|", line.strip())[1:-1]]
        self.assertEqual(len(cells), 6, line)                 # the title's pipe is escaped
        self.assertRegex(cells[0], r"^\d{4}-\d{2}-\d{2}$")
        self.assertEqual(cells[1], "Widgets \\| phase 1")
        self.assertEqual(cells[2], "complete")
        self.assertEqual(cells[3], "2/2 cards")
        self.assertRegex(cells[4], r"^(\$\d+\.\d\d|unknown)$")
        self.assertEqual(cells[5], str(w.run_dir))


class RelativeDocsDir(unittest.TestCase):
    def test_relative_docs_dir_resolves_against_the_repository_from_any_cwd(self):
        # `start --repo` from another directory and the wrap-up run from the workspace must
        # use one docs dir: the repository-relative one.
        w = FakeWorld(self, [card(1)], gates=True)
        w.config_file.write_text('docs_dir = "../notes/{repo}"\n', encoding="utf-8")
        w.docs = (w.repo.parent / "notes" / w.repo.name).resolve()
        elsewhere = w.tmp / "a" / "b"
        elsewhere.mkdir(parents=True)
        r = subprocess.run([sys.executable, "-B", str(CLI), "start", str(w.deck_path), "--repo", str(w.repo)],
                           cwd=elsewhere, capture_output=True, text=True, timeout=120)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        run_line = [ln for ln in r.stdout.splitlines() if ln.startswith("run: ")]
        self.assertEqual(len(run_line), 1, r.stdout)
        w.run_dir = Path(run_line[0][len("run: "):])
        self.assertEqual(w.run_dir.parent.parent, w.docs)
        cwd = os.getcwd()
        self.addCleanup(os.chdir, cwd)
        os.chdir(w.workspace())
        w.run()
        self.assertEqual(len(runs_lines(w)), 1, sorted(str(p) for p in w.tmp.rglob("runs.md")))


class FixWaves(unittest.TestCase):
    def test_final_review_defects_get_a_fix_wave_then_all_gates_rerun(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "final_review", {"verdict": "defects"})
        w.run()

        self.assertEqual(w.run_dispatch_ids(), [
            "run-final_review-1", "run-w1-final_fix-1", "run-w1-final_review-1",
            "run-final_review-2", "run-evidence-1", "run-verify-1"])
        fix = w.run_started[1]
        self.assertEqual((fix["kind"], fix["role"], fix["wave"]), ("final_fix", "implementer", 1))
        fix_brief = Path(fix["brief"]).read_text()
        self.assertIn('"final_review" failed', fix_brief)
        self.assertIn(FINAL, fix_brief)
        (accepted_fix,) = [e for e in w.events("accepted") if e["card"] == "final"]
        head = w.branch_tip(RUN)
        self.assertEqual(accepted_fix["sha"], head)
        old = w.events("gate_result")[0]["examined_sha"]
        self.assertNotEqual(old, head)
        self.assertEqual(gates(w), [("final_review", old, False), ("final_review", head, True),
                                    ("evidence", head, True), ("verify", head, True)])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/final/run-w1-final_fix-1.txt"),
                         "run-w1-final_fix-1")
        self.assertEqual(outcome(w)["outcome"], "complete")

    def test_verify_fix_after_security_pass_reruns_security_and_evidence(self):
        w = FakeWorld(self, [card(1, risk="medium")], gates=True)
        w.start()
        w.script(None, "verify", {"result": "fail"})
        w.run()

        first, head = w.events("gate_result")[0]["examined_sha"], w.branch_tip(RUN)
        self.assertNotEqual(first, head)
        self.assertEqual(gates(w), [
            ("final_review", first, True), ("evidence", first, True), ("security", first, True),
            ("verify", first, False),
            ("final_review", head, True), ("evidence", head, True), ("security", head, True),
            ("verify", head, True)])
        changed = run_git(w.repo, "diff", "--name-only", first, head).splitlines()
        self.assertEqual(changed, ["src/final/run-w1-final_fix-1.txt"])
        self.assertEqual(outcome(w)["outcome"], "complete")

    def test_fix_waves_are_capped(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        defects = {"verdict": "defects"}
        w.script(None, "final_review", defects, defects, defects, defects)
        w.run()

        fixes = [i for i in w.run_dispatch_ids() if "final_fix" in i]
        self.assertEqual(fixes, ["run-w1-final_fix-1", "run-w2-final_fix-1"])
        self.assertEqual(engine.MAX_FINAL_FIX_WAVES, 2)
        head = w.branch_tip(RUN)
        self.assertEqual(gate_names(w), ["final_review", "final_review", "final_review", "evidence", "verify"])
        self.assertEqual(gates(w)[-3:], [("final_review", head, False), ("evidence", head, True),
                                         ("verify", head, True)])
        out = outcome(w)
        self.assertEqual(out["outcome"], "incomplete")
        self.assertIn("unresolved review findings", out["reasons"])

    def test_rejected_fix_is_not_accepted_and_spends_its_wave(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "final_review", {"verdict": "defects"})
        w.script(None, "fix_review", {"verdict": "defects"}, {"verdict": "clean"})
        w.run()
        self.assertEqual([i for i in w.run_dispatch_ids() if "final_fix" in i],
                         ["run-w1-final_fix-1", "run-w2-final_fix-1"])
        (accepted_fix,) = [e for e in w.events("accepted") if e["card"] == "final"]
        self.assertEqual(accepted_fix["wave"], 2)
        # wave 1's commits never reached the run branch; they are kept on a ref
        run_files = run_git(w.repo, "ls-tree", "-r", "--name-only", RUN).splitlines()
        self.assertNotIn("src/final/run-w1-final_fix-1.txt", run_files)
        self.assertIn("src/final/run-w2-final_fix-1.txt", run_files)
        kept = run_git(w.repo, "for-each-ref", "--format=%(refname)", "refs/leadhand/").splitlines()
        self.assertTrue(any(r.endswith("/final/1") for r in kept), kept)
        self.assertEqual(outcome(w)["outcome"], "complete")


class Evidence(unittest.TestCase):
    def evidence_fails(self, config=""):
        w = FakeWorld(self, [card(1)], config=config, gates=True)
        w.start()
        gap = {"result": "fail"}
        w.script(None, "evidence", gap, gap, gap)
        done = w.run()[-1]
        return w, done

    def test_evidence_block_gap_ends_blocked(self):
        w, done = self.evidence_fails('[run]\nevidence = "block"\n')
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"))
        self.assertEqual(gate_names(w).count("evidence"), 3)
        self.assertNotIn("verify", gate_names(w))
        handoff = w.paths.handoff.read_text()
        self.assertIn("decision needed", handoff)
        self.assertIn("evidence", handoff)
        self.assertIn("leadhand run", handoff)
        self.assertEqual(len(w.events("report_written")), 1)
        self.assertEqual(runs_lines(w)[0].split("|")[3].strip(), "blocked (decision needed)")

    def test_evidence_record_gap_makes_incomplete_with_hold(self):
        w, done = self.evidence_fails()
        self.assertEqual(done["outcome"], "incomplete")
        self.assertTrue(any(r.startswith("hold: evidence") for r in done["reasons"]), done["reasons"])
        head = w.branch_tip(RUN)
        self.assertEqual(gates(w)[-2:], [("evidence", head, False), ("verify", head, True)])
        self.assertFalse(w.paths.handoff.exists())

    def test_resumed_blocked_run_keeps_gates_that_passed_on_the_same_head(self):
        w, done = self.evidence_fails('[run]\nevidence = "block"\n')
        before = len(w.events("gate_result"))
        append_event(w.paths, "resumed", note="the operator decided the gap is fine to re-check")
        w.script(None, "evidence", {"result": "pass"})
        done = w.run()[-1]
        later = [e["gate"] for e in w.events("gate_result")[before:]]
        self.assertEqual(later, ["evidence", "verify"])          # final_review stayed current
        self.assertEqual(done["outcome"], "complete")
        self.assertFalse(w.paths.handoff.exists())          # the blocked run's handoff is gone
        self.assertEqual(len(runs_lines(w)), 1)
        self.assertIn("| complete |", runs_lines(w)[0])


class Security(unittest.TestCase):
    def security_ran(self, deck_card, files=None):
        w = FakeWorld(self, [deck_card], gates=True)
        w.start()
        if files:
            w.script(1, "implement", {"files": files})
        w.run()
        return w, [a for a in w.run_started if a["kind"] == "security"]

    def test_security_gate_only_for_medium_or_high_or_sensitive_paths(self):
        w, ran = self.security_ran(card(1))
        self.assertEqual(ran, [])
        self.assertEqual(outcome(w)["outcome"], "complete")

        for risk in ("medium", "high"):
            with self.subTest(risk=risk):
                w, ran = self.security_ran(card(1, risk=risk))
                (sec,) = ran
                self.assertEqual(sec["role"], "reviewer_hard")
                self.assertEqual(gate_names(w), ["final_review", "evidence", "security", "verify"])

        for path in (".github/workflows/ci.yml", "scripts/install.sh", "db/migrations/0001_init.sql",
                     "src/auth/session.py", "terraform/main.tf"):
            with self.subTest(path=path):
                w, ran = self.security_ran(card(1), files={path: "x\n"})
                self.assertEqual(len(ran), 1, path)

        self.assertTrue(engine.is_sensitive_path(".github/workflows/ci.yml"))
        self.assertFalse(engine.is_sensitive_path("src/widget.py"))
        self.assertFalse(engine.is_sensitive_path("docs/authoring.md"))


class Verification(unittest.TestCase):
    def test_verification_failure_after_last_wave_is_failed(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        fail = {"result": "fail"}
        w.script(None, "verify", fail, fail, fail)
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "failed")
        self.assertEqual([i for i in w.run_dispatch_ids() if "final_fix" in i],
                         ["run-w1-final_fix-1", "run-w2-final_fix-1"])
        self.assertEqual(gate_names(w).count("verify"), 3)
        self.assertIn("verification of the final commit failed", done["reasons"])
        self.assertTrue(w.paths.report.is_file())
        self.assertFalse(w.paths.handoff.exists())


class Unpark(unittest.TestCase):
    def test_unparked_card_waits_for_an_open_gate_then_gates_rerun(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision: storage format"
        gate = None
        for _ in range(40):
            a = w.next()
            if a["action"] == "dispatch" and a["card"] is None:
                gate = a
                break
            w.act(a)
        self.assertEqual(gate["dispatch_id"], "run-final_review-1")
        worker = w.launch(gate)
        engine.unpark(w.run_dir, 1, "Use JSON.")
        waiting = w.next()
        self.assertEqual((waiting["action"], waiting["dispatch_id"]), ("wait", "run-final_review-1"))
        engine.record(w.run_dir, "result", dispatch_id=gate["dispatch_id"], worker_id=worker,
                      reply=w.perform(gate, worker))
        del w.parks[1]
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", (done["reasons"], w.events("accepted")))
        head = w.branch_tip(RUN)
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(gates(w)[-3:], [("final_review", head, True), ("evidence", head, True),
                                         ("verify", head, True)])


def drive_to_run(w, pred):
    """Act on every action until a run-level dispatch matching `pred`; return it unacted."""
    for _ in range(80):
        a = w.next()
        if a["action"] == "dispatch" and a["card"] is None and pred(a):
            return a
        w.act(a)
    raise AssertionError("no matching run-level dispatch")


def finish_worker(w, a):
    worker = w.launch(a)
    engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker, reply=w.perform(a, worker))


def found_violations(w):
    return [e for e in w.events("violation") if e.get("stage") == "found" and (e.get("by") or "check") != "implementer"]


class Guards(unittest.TestCase):
    def test_final_fix_moving_the_run_branch_is_voided_and_restored(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(None, "final_review", {"verdict": "defects"})

        def rewind(world, ws, a):
            run_git(ws, "branch", "-f", RUN, world.base)
        w.script(None, "final_fix", {"tamper": rewind}, {})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        (v,) = found_violations(w)
        self.assertEqual((v["dispatch_id"], v["dispatch_kind"]), ("run-w1-final_fix-1", "final_fix"))
        self.assertIn("run-w1-final_fix-2", w.run_dispatch_ids())
        head = w.branch_tip(RUN)
        for e in w.events("accepted"):
            run_git(w.repo, "merge-base", "--is-ancestor", e["sha"], head)
        self.assertEqual(sorted(e["card"] for e in w.events("accepted") if e["card"] != "final"), [1, 2])

    def test_accepted_work_missing_from_the_run_branch_fails_the_run(self):
        w = FakeWorld(self, [card(1)])                       # card loop only: no outcome yet
        w.start()
        w.run()
        run_git(w.repo, "update-ref", f"refs/heads/{RUN}", w.base)
        ctx = engine._load(w.run_dir, check_digest=False)
        out = engine._end(ctx, read_events(w.paths), gates_current=True)
        self.assertEqual(out["outcome"], "failed")
        self.assertTrue(any("accepted work missing from the run branch" in r for r in out["reasons"]), out["reasons"])

    def test_evidence_base_worktree_is_reset_and_guarded(self):
        def tamper(world, ws, a):
            bw = world.repo / ".worktrees" / f"{world.run_dir.name}--base"
            write(bw, {"tests/leftover.txt": "x\n"})
            commit(bw, {"src/moved.txt": "moved\n"}, "verifier moved the base")
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": tamper}, {})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        (v,) = found_violations(w)
        self.assertEqual((v["dispatch_id"], v["dispatch_kind"]), ("run-evidence-1", "evidence"))
        rerun = next(a for a in w.run_started if a["dispatch_id"] == "run-evidence-2")
        self.assertIsNone(rerun["resume"])
        bw = w.repo / ".worktrees" / f"{w.run_dir.name}--base"
        self.assertEqual(run_git(bw, "rev-parse", "HEAD"), w.base)
        self.assertEqual(run_git(bw, "status", "--porcelain", "--ignored"), "")
        self.assertEqual([e["dispatch_id"] for e in w.events("gate_result") if e["gate"] == "evidence"],
                         ["run-evidence-2"])

    def test_gate_violation_reruns_with_a_fresh_worker(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the final reviewer\n"})
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "final_review", {"tamper": tamper}, {})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        (v,) = found_violations(w)
        self.assertEqual((v["dispatch_id"], v["card"]), ("run-final_review-1", None))
        self.assertEqual(w.run_dispatch_ids()[:2], ["run-final_review-1", "run-final_review-2"])
        self.assertIsNone(w.run_started[1]["resume"])
        self.assertEqual([e["dispatch_id"] for e in w.events("gate_result") if e["gate"] == "final_review"],
                         ["run-final_review-2"])

    def test_gate_violation_cap_ends_blocked(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the verifier\n"})
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        t = {"tamper": tamper}
        w.script(None, "verify", t, t, t, t)
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"))
        self.assertEqual(len(found_violations(w)), engine.VIOLATION_CAP + 1)
        self.assertTrue(any("verify workers change tracked files: README.md" in r for r in done["reasons"]))
        self.assertTrue(w.paths.handoff.is_file())


class RunWorkerFailures(unittest.TestCase):
    def test_gate_without_a_result_twice_ends_blocked_environment(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "verify", {"status": "READY"}, {"status": "READY"})
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "environment"))
        self.assertEqual([i for i in w.run_dispatch_ids() if "verify" in i], ["run-verify-1", "run-verify-2"])

    def test_gate_usage_limit_ends_blocked_provider_capacity(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"status": "BLOCKED", "blocked_by": "usage limit"})
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "provider capacity"))
        append_event(w.paths, "resumed", note="capacity is back")
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        self.assertEqual([i for i in w.run_dispatch_ids() if "evidence" in i], ["run-evidence-1", "run-evidence-2"])


class Crashes(unittest.TestCase):
    def test_crash_before_gate_record_records_once(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        finish_worker(w, drive_to_run(w, lambda a: a["gate"] == "final_review"))
        r = w.cli("next", w.run_dir, crash="before_gate_record")
        self.assertEqual(r.returncode, 97, r.stderr)
        self.assertEqual(w.events("gate_result"), [])
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        self.assertEqual([e["dispatch_id"] for e in w.events("gate_result")],
                         ["run-final_review-1", "run-evidence-1", "run-verify-1"])

    def crash_final_accept(self, point):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "final_review", {"verdict": "defects"})
        finish_worker(w, drive_to_run(w, lambda a: a["kind"] == "final_review" and a["wave"]))
        before = w.branch_tip(RUN)
        r = w.cli("next", w.run_dir, crash=point)
        self.assertEqual(r.returncode, 97, r.stderr)
        (intent,) = [e for e in w.events("accept_started") if e["card"] == "final"]
        self.assertEqual([e for e in w.events("accepted") if e["card"] == "final"], [])
        return w, before, intent

    def test_crash_before_final_fast_forward_reconciles(self):
        w, before, intent = self.crash_final_accept("before_fast_forward")
        self.assertEqual(w.branch_tip(RUN), before)
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        (acc,) = [e for e in w.events("accepted") if e["card"] == "final"]
        self.assertEqual(acc["sha"], intent["target"])
        self.assertEqual(len([e for e in w.events("accept_started") if e["card"] == "final"]), 1)
        self.assertEqual(w.branch_tip(RUN), intent["target"])

    def test_crash_after_final_fast_forward_reconciles(self):
        w, before, intent = self.crash_final_accept("after_fast_forward")
        self.assertEqual(w.branch_tip(RUN), intent["target"])
        reflog = run_git(w.repo, "reflog", "show", "--format=%H", f"refs/heads/{RUN}")
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        (acc,) = [e for e in w.events("accepted") if e["card"] == "final"]
        self.assertEqual(acc["sha"], intent["target"])
        self.assertEqual(run_git(w.repo, "reflog", "show", "--format=%H", f"refs/heads/{RUN}"), reflog)


def base_wt(w):
    return w.repo / ".worktrees" / f"{w.run_dir.name}--base"


class FixRound2(unittest.TestCase):
    def test_spent_final_fix_is_not_rechecked_after_a_card_accept_crash(self):
        w = FakeWorld(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision"
        w.script(None, "final_review", {"verdict": "defects"})
        w.script(None, "final_fix", {"status": "QUESTION"})
        fix = drive_to_run(w, lambda a: a["kind"] == "final_fix")
        worker = w.launch(fix)
        engine.unpark(w.run_dir, 1, "Use JSON.")
        del w.parks[1]
        engine.record(w.run_dir, "result", dispatch_id=fix["dispatch_id"], worker_id=worker,
                      reply=w.perform(fix, worker))
        for _ in range(60):
            a = w.next()
            w.act(a)
            if a["action"] == "dispatch" and a["card"] == 1 and a["kind"] == "verify":
                break
        r = w.cli("next", w.run_dir, crash="after_fast_forward")
        self.assertEqual(r.returncode, 97, r.stderr)
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual(found_violations(w), [])
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])
        head = w.branch_tip(RUN)
        for e in w.events("accepted"):
            run_git(w.repo, "merge-base", "--is-ancestor", e["sha"], head)
        (spent,) = [e for e in w.events("verdict_recorded") if e.get("dispatch_id") == fix["dispatch_id"]]
        self.assertEqual(spent["verdict"], "spent")

    def test_crash_between_base_violation_found_and_restored_recovers(self):
        def move(world, ws, a):
            commit(base_wt(world), {"src/moved.txt": "m\n"}, "moved base")
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": move}, {})
        real = engine.git

        class Crash(Exception):
            pass
        armed = {"on": True}

        def crashing(cwd, *args, **kw):
            if armed["on"] and "update-ref" in args and any(str(x).endswith("-base") for x in args):
                armed["on"] = False
                raise Crash()
            return real(cwd, *args, **kw)
        with mock.patch.object(engine, "git", crashing):
            with self.assertRaises(Crash):
                w.run()
        self.assertEqual([e["stage"] for e in found_violations(w)], ["found"])
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        stages = [e["stage"] for e in w.events("violation")]
        self.assertEqual(stages, ["found", "restored"])
        self.assertEqual(run_git(base_wt(w), "rev-parse", "HEAD"), w.base)
        kept = run_git(w.repo, "for-each-ref", "--format=%(refname)", "refs/leadhand/").splitlines()
        self.assertTrue(any(k.endswith("-base") for k in kept), kept)

    def test_evidence_rerun_after_a_worker_failure_gets_a_reset_base(self):
        seen = {}

        def leave(world, ws, a):
            write(base_wt(world), {"tests/leftover_test.py": "x\n", "build/cache.o": "y\n"})

        def look(world, ws, a):
            seen["files"] = {p.name for p in base_wt(world).rglob("*") if p.is_file() and ".git" not in p.parts}
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": leave, "status": "BLOCKED"}, {"tamper": look})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        self.assertEqual([i for i in w.run_dispatch_ids() if "evidence" in i], ["run-evidence-1", "run-evidence-2"])
        self.assertNotIn("leftover_test.py", seen["files"])
        self.assertNotIn("cache.o", seen["files"])

    def test_voided_final_fix_reruns_from_the_run_head(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(None, "final_review", {"verdict": "defects"})

        def rewind(world, ws, a):
            run_git(ws, "branch", "-f", RUN, world.base)
        w.script(None, "final_fix", {"tamper": rewind}, {})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        (v,) = found_violations(w)
        run_files = run_git(w.repo, "ls-tree", "-r", "--name-only", RUN).splitlines()
        self.assertNotIn("src/final/run-w1-final_fix-1.txt", run_files)     # the voided worker's commit
        self.assertIn("src/final/run-w1-final_fix-2.txt", run_files)
        review = next(a for a in w.run_started if a["kind"] == "final_review" and a["wave"])
        req = next(e for e in w.events("dispatch_requested") if e["dispatch_id"] == review["dispatch_id"])
        base, tip = req["extra"]["diff_range"].split("..")
        self.assertEqual(run_git(w.repo, "diff", "--name-only", base, tip).splitlines(),
                         ["src/final/run-w1-final_fix-2.txt"])
        keep = f"refs/leadhand/{w.meta()['run_id']}/violations/{v['n']}-final"
        self.assertEqual(run_git(w.repo, "show", f"{keep}:src/final/run-w1-final_fix-1.txt"),
                         "run-w1-final_fix-1")


class NothingAccepted(unittest.TestCase):
    def test_run_with_every_card_parked_runs_no_gates(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision"
        done = w.run()[-1]
        self.assertEqual(w.run_started, [])
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"))
        self.assertTrue(w.paths.report.is_file())


if __name__ == "__main__":
    unittest.main()
