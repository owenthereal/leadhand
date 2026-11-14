#!/usr/bin/env python3
"""Integration and violations: how a card's work is rebased onto the run tip and fast-forwarded,
what a conflict does, unparking, and how a worker that changes the workspace or moves a ref it
must leave alone is caught, preserved and re-run, as `start`, `next`, `record` and `unpark` show
them.

Converted from tests/test_engine_integration.py. FakeSupervisor plays the run skill; workers
misbehave through a step's `tamper`, which runs in the harness against the workspace. Where the
Python test called `engine.unpark` or `engine.record`, this calls `unpark` or `record`; an
EngineError is exit code 1 with `error: ...` on stderr.

Integration outcomes and violations are judged from Git itself: branch tips, ancestry, trees,
the refs under refs/leadhand/<run id>/ and their contents, trailers, and the workspace's
checkout. The binary's events are asserted beside that, never as the only evidence.
"""
import shlex
import unittest
from pathlib import Path

import harness
from harness import FAKES
from contract_cards import codex_world
from gitfixture import commit, current_branch, run_git, write
from rundir import append_event
from supervisor import FakeSupervisor, card

RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
NOTE = "Use JSON; it is simpler."


# --------------------------------------------------------------------------
# Helpers

def parked_world(test, *, card1_files=None, card2_files=None, **kw) -> FakeSupervisor:
    """Card 1 asks a product question and is parked; independent card 2 is accepted, so the
    run tip moves past card 1's base."""
    w = FakeSupervisor(test, [card(1), card(2)], **kw)
    if "codex" in kw.get("installed", ()):
        w.env["LEADHAND_TEST_CODEX_BIN"] = str(FAKES / "codex")     # as contract_cards.codex_world
    w.session("supervisor-1")            # the supervisor session the workers start in
    w.start()
    w.script(1, "implement", {"status": "QUESTION", **({"files": card1_files} if card1_files else {})})
    if card2_files:
        w.script(2, "implement", {"files": card2_files})
    w.parks[1] = "product decision: storage format"
    w.run()
    del w.parks[1]
    return w


def unpark(w: FakeSupervisor, n: int, note: str) -> harness.Result:
    """`unpark RUN_DIR --card N --note NOTE`, which must succeed."""
    return w.cli("unpark", w.run_dir, "--card", str(n), "--note", note).ok()


def drive_to(w: FakeSupervisor, kind: str, card_number: int) -> dict:
    """Act on every action until the dispatch of `kind` for the card; return it unacted."""
    for _ in range(80):
        a = w.next()
        if a["action"] == "dispatch" and a["kind"] == kind and a["card"] == card_number:
            return a
        w.act(a)
    raise AssertionError(f"no {kind} dispatch for card {card_number}")


def git_ok(cwd, *args) -> bool:
    try:
        run_git(cwd, *args)
    except AssertionError:
        return False
    return True


def requested(w: FakeSupervisor, dispatch_id: str) -> dict:
    return next(e for e in w.events("dispatch_requested") if e["dispatch_id"] == dispatch_id)


def worker_of(w: FakeSupervisor, dispatch_id: str) -> str:
    return next(e["worker_id"] for e in w.events("worker_started") if e["dispatch_id"] == dispatch_id)


def found(w: FakeSupervisor, by: str = "check") -> list[dict]:
    return [e for e in w.events("violation") if e.get("stage") == "found" and (e.get("by") or "check") == by]


def leadhand_refs(w: FakeSupervisor, kind: str) -> list[str]:
    """The refs under refs/leadhand/<run id>/<kind>/, as Git lists them."""
    prefix = f"refs/leadhand/{w.meta()['run_id']}/{kind}/"
    return run_git(w.repo, "for-each-ref", "--format=%(refname)", prefix).splitlines()


def on_branch(w: FakeSupervisor, branch: str, path: str) -> bool:
    return git_ok(w.repo, "cat-file", "-e", f"refs/heads/{branch}:{path}")


class EngineCase(unittest.TestCase):
    def assert_run_tip_is_accepted(self, w: FakeSupervisor, n: int) -> str:
        """The run branch, read from Git, is at card `n`'s accepted commit; returns it."""
        sha = {e["card"]: e["sha"] for e in w.events("accepted")}[n]
        self.assertEqual(w.branch_tip(RUN), sha)
        return sha


# --------------------------------------------------------------------------

class Integration(EngineCase):
    def test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded(self):
        w = parked_world(self)
        self.assertEqual([e["card"] for e in w.events("accepted")], [2])
        run_tip = w.branch_tip(RUN)
        r = unpark(w, 1, NOTE)
        self.assertIn("card 1 unparked", r.stdout)
        (event,) = w.events("unparked")
        self.assertEqual((event["kind"], event["card"], event["note"]), ("unparked", 1, NOTE))
        actions = w.run()
        self.assertEqual(actions[-1]["action"], "done")

        self.assertEqual(w.dispatch_ids()[3:], ["c1-a1-answer-1", "c1-a1-review-1", "c1-a1-rereview-1",
                                                "c1-a1-verify-1"])
        answer, review, rereview, verify = w.started[3:]
        # the parked worker is resumed with the operator's note as the answer
        self.assertEqual(answer["resume"], {"worker_id": worker_of(w, "c1-a1-implement-1")})
        self.assertIn("> " + NOTE, Path(answer["brief"]).read_text())

        (intent,) = w.events("integration_started")
        (rebased,) = w.events("rebased")
        self.assertEqual(w.events("conflict"), [])
        self.assertEqual(intent["onto"], run_tip)
        self.assertLess(requested(w, "c1-a1-review-1")["seq"], intent["seq"])
        self.assertLess(intent["seq"], rebased["seq"])
        self.assertLess(rebased["seq"], requested(w, "c1-a1-rereview-1")["seq"])
        self.assertLess(requested(w, "c1-a1-rereview-1")["seq"], requested(w, "c1-a1-verify-1")["seq"])
        (accept,) = [e for e in w.events("accept_started") if e["card"] == 1]
        self.assertLess(requested(w, "c1-a1-verify-1")["seq"], accept["seq"])

        # the rereview covers the integrated diff; the verifier runs the deck's checks on the card branch
        self.assertIn(f"{run_tip}..{rebased['tip']}", Path(rereview["brief"]).read_text())
        self.assertIsNone(rereview["resume"])
        self.assertEqual((verify["role"], verify["agent"]), ("verifier", "leadhand:verifier"))
        vbrief = Path(verify["brief"]).read_text()
        self.assertIn("- Test: true", vbrief)
        self.assertIn(CARD1, vbrief)

        accepted = {e["card"]: e for e in w.events("accepted")}
        self.assertEqual(accepted[1]["sha"], rebased["tip"])
        self.assertEqual(w.branch_tip(RUN), rebased["tip"])
        self.assertTrue(git_ok(w.repo, "merge-base", "--is-ancestor", run_tip, rebased["tip"]))
        for path in ("src/card1/c1-a1-implement-1.txt", "src/card1/c1-a1-answer-1.txt",
                     "src/card2/c2-a1-implement-1.txt"):
            run_git(w.repo, "cat-file", "-e", f"{RUN}:{path}")
        self.assertEqual(w.events("violation"), [])
        self.assertEqual(leadhand_refs(w, "violations"), [])

    def test_integration_conflict_requests_integrate_attempt(self):
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"})
        run_tip = w.branch_tip(RUN)
        unpark(w, 1, NOTE)
        integrate = drive_to(w, "integrate", 1)

        (intent,) = w.events("integration_started")
        (conflict,) = w.events("conflict")
        self.assertEqual(conflict["paths"], ["src/shared.txt"])
        self.assertEqual(w.events("rebased"), [])
        # the engine's own rebase was aborted: the card branch is as it was, and checked out
        self.assertEqual(w.branch_tip(CARD1), intent["tip"])
        self.assertEqual(current_branch(w.workspace()), CARD1)
        self.assertFalse(run_git(w.workspace(), "status", "--porcelain", "--untracked-files=no"))
        self.assertFalse(git_ok(w.workspace(), "merge-base", "--is-ancestor", run_tip, CARD1))
        # a new attempt on the route of the card's last implementer attempt, not escalated
        answer = requested(w, "c1-a1-answer-1")
        self.assertEqual((integrate["dispatch_id"], integrate["attempt"], integrate["role"]),
                         ("c1-a2-integrate-1", 2, answer["role"]))
        self.assertEqual(integrate["route"], {"vendor": answer["vendor"], "model": answer["model"],
                                              "effort": answer["effort"]})
        self.assertIsNone(integrate["resume"])
        brief = Path(integrate["brief"]).read_text()
        self.assertIn(run_tip, brief)
        self.assertIn("src/shared.txt", brief)

        w.act(integrate)
        w.run()
        self.assertEqual(w.dispatch_ids()[-3:], ["c1-a2-integrate-1", "c1-a2-rereview-1", "c1-a2-verify-1"])
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["dispatch_id"], "c1-a2-integrate-1")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(w.branch_tip(RUN), rebased["tip"])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/shared.txt"), "card one")
        self.assertTrue(git_ok(w.repo, "merge-base", "--is-ancestor", run_tip, rebased["tip"]))

    def crash_at_rebase(self, point):
        w = parked_world(self)
        unpark(w, 1, NOTE)
        w.act(drive_to(w, "review", 1))
        r = w.cli("next", w.run_dir, crash=point)
        self.assertEqual(r.code, 97, r)
        (intent,) = w.events("integration_started")
        self.assertEqual(w.events("rebased"), [])
        return w, intent

    def test_crash_after_rebase_before_event_reconciles(self):
        w, intent = self.crash_at_rebase("after_rebase")
        tip = w.branch_tip(CARD1)
        self.assertNotEqual(tip, intent["tip"])
        self.assertTrue(git_ok(w.repo, "merge-base", "--is-ancestor", intent["onto"], tip))
        w.run()
        # a card branch that already contains the run tip counts as rebased: no second rebase
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["tip"], tip)
        self.assertEqual(len(w.events("integration_started")), 1)
        self.assertEqual(w.events("conflict"), [])
        self.assertEqual({e["card"]: e["sha"] for e in w.events("accepted")}[1], tip)
        self.assertEqual(w.branch_tip(RUN), tip)

    def test_crash_before_rebase_rebases_on_restart(self):
        w, intent = self.crash_at_rebase("before_rebase")
        self.assertEqual(w.branch_tip(CARD1), intent["tip"])
        w.run()
        (rebased,) = w.events("rebased")
        self.assertTrue(git_ok(w.repo, "merge-base", "--is-ancestor", intent["onto"], rebased["tip"]))
        self.assertEqual(w.branch_tip(RUN), rebased["tip"])

    def test_failed_verification_escalates(self):
        w = parked_world(self)
        unpark(w, 1, NOTE)
        w.script(1, "verify", {"result": "fail"})
        w.run()
        ids = w.dispatch_ids()
        self.assertIn("c1-a1-verify-1", ids)
        self.assertIn("c1-a2-implement-1", ids)
        # the escalated attempt's tip is verified again before it is accepted
        self.assertEqual(ids[-1], "c1-a2-verify-1")
        accepted = {e["card"]: e["sha"] for e in w.events("accepted")}
        verify = requested(w, "c1-a2-verify-1")
        self.assertEqual(verify["extra"]["tip"], accepted[1])
        self.assert_run_tip_is_accepted(w, 1)
        self.assertTrue(on_branch(w, RUN, "src/card1/c1-a2-implement-1.txt"))


class Unpark(EngineCase):
    def test_unpark_without_a_known_worker_starts_a_fresh_attempt(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        first = w.next()
        self.assertEqual(first["dispatch_id"], "c1-a1-implement-1")
        w.cli("record", w.run_dir, "park", "--card", "1", "--reason", "operator wants to look first").ok()
        unpark(w, 1, NOTE)
        a = w.next()
        self.assertEqual((a["action"], a["kind"], a["dispatch_id"], a["resume"]),
                         ("dispatch", "implement", "c1-a1-implement-2", None))
        self.assertIn(NOTE, Path(a["brief"]).read_text())
        w.act(a)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assert_run_tip_is_accepted(w, 1)
        self.assertTrue(on_branch(w, RUN, "src/card1/c1-a1-implement-2.txt"))

    def test_unpark_cli_appends_unparked_and_refuses_a_card_not_parked(self):
        """Covers limit M6 (run forgery): `unpark` has no caller check; any process that can run the CLI can
        unpark a card of the run."""
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.next()
        r = w.cli("unpark", w.run_dir, "--card", "1", "--note", NOTE)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        self.assertIn("not parked", r.stderr)
        self.assertEqual(w.events("unparked"), [])
        w.cli("record", w.run_dir, "park", "--card", "1", "--reason", "hold").ok()
        r = w.cli("unpark", w.run_dir, "--card", "1", "--note", NOTE)
        self.assertEqual(r.code, 0, r)
        (event,) = w.events("unparked")
        self.assertEqual((event["card"], event["note"]), (1, NOTE))
        for note in (NOTE, "  "):           # no longer parked; an empty note
            r = w.cli("unpark", w.run_dir, "--card", "1", "--note", note)
            self.assertEqual(r.code, 1, r)
            self.assertTrue(r.stderr.startswith("error: "), r)
        self.assertEqual(len(w.events("unparked")), 1)


class Violations(EngineCase):
    def world(self, kind, tamper, **first) -> FakeSupervisor:
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, kind, {"tamper": tamper, **first}, {})
        return w

    def check_rerun(self, w, kind, voided, rerun):
        self.assertIn(voided, w.dispatch_ids())
        self.assertIn(rerun, w.dispatch_ids())
        old, new = requested(w, voided), requested(w, rerun)
        self.assertEqual((new["dispatch_kind"], new["vendor"], new["model"], new["effort"], new["role"]),
                         (kind, old["vendor"], old["model"], old["effort"], old["role"]))
        self.assertIsNone(new["resume_worker"])
        ws = w.workspace()
        self.assertEqual(current_branch(ws), CARD1)
        self.assertEqual(run_git(ws, "status", "--porcelain", "--untracked-files=no"), "")

    def test_reviewer_tracked_change_voids_review_and_reruns(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the reviewer\n"})

        w = self.world("review", tamper, verdict="defects")
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"])
        (v,) = found(w)
        run_id = w.meta()["run_id"]
        self.assertEqual((v["dispatch_id"], v["paths"], v["ref"]),
                         ("c1-a1-review-1", ["README.md"], f"refs/leadhand/{run_id}/violations/1"))
        self.assertEqual(leadhand_refs(w, "violations"), [f"refs/leadhand/{run_id}/violations/1"])
        # the voided defects verdict never counted: no fix round, the re-run's verdict decided
        self.assertEqual(w.events("fix_round"), [])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-2"])
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "changed by the reviewer")
        self.assertEqual((w.workspace() / "README.md").read_text(), "hello\n")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assert_run_tip_is_accepted(w, 1)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")

    def test_reviewer_moving_head_is_violation(self):
        def tamper(world, ws, a):
            commit(ws, {"src/reviewer.txt": "by the reviewer\n"}, "reviewer commit")

        w = self.world("review", tamper)
        w.run()
        (v,) = found(w)
        self.assertTrue(v["head_moved"])
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")
        impl_tip = next(e["tip"] for e in w.events("result_recorded") if e["dispatch_id"] == "c1-a1-implement-1")
        self.assertEqual(requested(w, "c1-a1-review-1")["expected_head"], impl_tip)
        self.assertEqual(w.events("accepted")[0]["sha"], impl_tip)
        self.assertEqual(w.branch_tip(RUN), impl_tip)
        self.assertFalse(on_branch(w, RUN, "src/reviewer.txt"))
        # the reviewer's commit is preserved, not lost
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:src/reviewer.txt"), "by the reviewer")
        moved = v["refs"][f"refs/heads/{CARD1}"]
        self.assertEqual(moved["old"], impl_tip)
        keep = v["preserved"][f"refs/heads/{CARD1}"]
        self.assertEqual(run_git(w.repo, "rev-parse", keep), moved["new"])
        self.assertEqual(run_git(w.repo, "show", f"{keep}:src/reviewer.txt"), "by the reviewer")
        self.assertEqual(run_git(w.repo, "rev-parse", f"{keep}^"), impl_tip)

    def test_reviewer_hiding_an_edit_behind_an_index_flag_is_violation(self):
        # Codex 4554880076: assume-unchanged or skip-worktree hides an edit from status and diff.
        for flag in ("--assume-unchanged", "--skip-worktree"):
            with self.subTest(flag=flag):
                def tamper(world, ws, a, flag=flag):
                    run_git(ws, "update-index", flag, "README.md")
                    write(ws, {"README.md": "edited by the reviewer, hidden\n"})

                w = self.world("review", tamper)
                w.run()
                (v,) = found(w)
                self.assertEqual(v["dispatch_id"], "c1-a1-review-1")
                self.assertIn("README.md", v["paths"])
                self.assertEqual(v["hidden"], ["README.md"])
                self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")
                ws = w.workspace()
                self.assertEqual(run_git(ws, "ls-files", "-v", "--", "README.md"), "H README.md")
                self.assertEqual((ws / "README.md").read_text(), "hello\n")
                self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "edited by the reviewer, hidden")

    def test_a_partial_commit_takes_an_edit_hidden_behind_an_index_flag(self):
        # The Python test called engine._commit_partial; its outside effect is the partial
        # commit a cancel makes of a stopped Claude implementer's work (R42).
        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        a = w.next()
        worker = w.launch(a)
        ws = w.workspace()
        run_git(ws, "update-index", "--assume-unchanged", "README.md")
        write(ws, {"README.md": "a stopped worker's hidden edit\n"})
        w.cli("cancel", w.run_dir).ok()
        self.assertEqual(w.next()["action"], "stop_worker")
        event = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                      "--reply", "Status: SPENT (time limit)", "--stopped").ok().json()
        self.assertIn("partial_commit", event, event)
        self.assertEqual(run_git(ws, "show", "HEAD:README.md"), "a stopped worker's hidden edit")
        self.assertEqual(run_git(w.repo, "rev-parse", f"refs/heads/{CARD1}"), event["partial_commit"])
        self.assertEqual(run_git(w.repo, "show", f"{CARD1}:README.md"), "a stopped worker's hidden edit")
        trailer = run_git(w.repo, "log", "-1", "--format=%(trailers:key=Leadhand-Dispatch,valueonly)", CARD1)
        self.assertEqual(trailer, a["dispatch_id"])

    def test_verifier_staged_change_is_violation(self):
        w = parked_world(self)
        unpark(w, 1, NOTE)

        def tamper(world, ws, a):
            write(ws, {"src/card1/c1-a1-implement-1.txt": "staged by the verifier\n"})
            run_git(ws, "add", "--", "src/card1/c1-a1-implement-1.txt")

        w.script(1, "verify", {"tamper": tamper}, {})
        w.run()
        (v,) = found(w)
        self.assertEqual(v["dispatch_id"], "c1-a1-verify-1")
        self.assertTrue(v["index_dirty"])
        self.assertFalse(v["head_moved"])
        self.check_rerun(w, "verify", "c1-a1-verify-1", "c1-a1-verify-2")
        self.assertEqual(requested(w, "c1-a1-verify-2")["extra"], requested(w, "c1-a1-verify-1")["extra"])
        self.assertEqual(w.dispatch_ids()[-1], "c1-a1-verify-2")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/card1/c1-a1-implement-1.txt"),
                         "c1-a1-implement-1")
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}^2:src/card1/c1-a1-implement-1.txt"),
                         "staged by the verifier")

    def test_violation_preserves_index_and_worktree_on_ref(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "staged\n"})
            run_git(ws, "add", "--", "README.md")
            write(ws, {"README.md": "unstaged\n", "build/out.txt": "untracked output\n"})

        w = self.world("review", tamper)
        w.run()
        (v,) = found(w)
        self.assertTrue(v["index_dirty"])
        self.assertEqual(v["paths"], ["README.md"])
        (done,) = [e for e in w.events("violation") if e.get("stage") == "restored"]
        self.assertEqual(done["ref"], v["ref"])
        self.assertEqual(run_git(w.repo, "rev-parse", v["ref"]), done["saved"])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "unstaged")
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}^2:README.md"), "staged")
        ws = w.workspace()
        self.assertEqual((ws / "README.md").read_text(), "hello\n")
        self.assertEqual((ws / "build" / "out.txt").read_text(), "untracked output\n")
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")

    def test_untracked_build_output_is_not_a_violation(self):
        def tamper(world, ws, a):
            write(ws, {"build/out.txt": "output\n"})

        w = self.world("review", tamper)
        w.run()
        self.assertEqual(w.events("violation"), [])
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"])
        self.assertEqual(leadhand_refs(w, "violations"), [])
        self.assertEqual((w.workspace() / "build" / "out.txt").read_text(), "output\n")
        self.assertFalse(on_branch(w, RUN, "build/out.txt"))

    def test_reviewer_committing_on_run_branch_and_returning_to_card_tip_is_violation(self):
        """Covers R14."""
        def tamper(world, ws, a):
            back = current_branch(ws)
            run_git(ws, "checkout", "--quiet", RUN)
            commit(ws, {"src/sneaky.txt": "on the run branch\n"}, "reviewer commit on the run branch")
            run_git(ws, "checkout", "--quiet", back)

        w = self.world("review", tamper)
        base = w.base
        w.run()
        req = requested(w, "c1-a1-review-1")
        self.assertEqual(req["refs"][f"refs/heads/{RUN}"], base)
        (v,) = found(w)
        self.assertFalse(v["head_moved"])
        self.assertIsNone(v["ref"])
        moved = v["refs"][f"refs/heads/{RUN}"]
        self.assertEqual(moved["old"], base)
        run_id = w.meta()["run_id"]
        keep = v["preserved"][f"refs/heads/{RUN}"]
        self.assertEqual(keep, f"refs/leadhand/{run_id}/violations/1-heads-leadhand-demo")
        self.assertEqual(run_git(w.repo, "show", f"{keep}:src/sneaky.txt"), "on the run branch")
        self.assertEqual(leadhand_refs(w, "violations"), [keep])
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")
        # the run branch was put back before acceptance, so it holds only the card's work
        accepted = w.events("accepted")[0]["sha"]
        self.assertEqual(w.branch_tip(RUN), accepted)
        self.assertFalse(git_ok(w.repo, "cat-file", "-e", f"{RUN}:src/sneaky.txt"))
        self.assertEqual(run_git(w.repo, "rev-parse", f"{RUN}^"), base)

    def test_reviewer_creating_and_deleting_refs_is_violation(self):
        # R14': under refs/leadhand/ only this run's namespace is watched.
        def tamper(world, ws, a):
            run_id = world.meta()["run_id"]
            run_git(ws, "branch", "leadhand/demo--card-7")
            run_git(ws, "update-ref", f"refs/leadhand/{run_id}/elsewhere/note", "HEAD")
            run_git(ws, "update-ref", "refs/leadhand/other-run/note", "HEAD")
            run_git(ws, "update-ref", "-d", f"refs/heads/{RUN}")

        w = self.world("review", tamper)
        w.run()
        run_id = w.meta()["run_id"]
        note = f"refs/leadhand/{run_id}/elsewhere/note"
        (v,) = found(w)
        self.assertEqual(set(v["refs"]), {"refs/heads/leadhand/demo--card-7", note, f"refs/heads/{RUN}"})
        self.assertIsNone(v["refs"][f"refs/heads/{RUN}"]["new"])
        self.assertIsNone(v["refs"][note]["old"])
        for ref in ("refs/heads/leadhand/demo--card-7", note):
            self.assertFalse(git_ok(w.repo, "show-ref", "--verify", "--quiet", ref), ref)
        # another run's ref is not this run's business: left alone
        self.assertTrue(git_ok(w.repo, "show-ref", "--verify", "--quiet", "refs/leadhand/other-run/note"))
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        # the deleted run branch was put back, and the card fast-forwarded onto it
        self.assert_run_tip_is_accepted(w, 1)
        self.assertEqual(run_git(w.repo, "rev-parse", f"{RUN}^"), w.base)

    def test_violation_interrupted_before_restore_still_voids_on_restart(self):
        # The Python test made engine.preserve_and_restore raise. Here a ref under the violation
        # ref's name (made before the run dispatches anything, so no worker moved it) blocks
        # Git from creating that ref: the restore fails part way, after `found` is recorded.
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the reviewer\n"})

        w = self.world("review", tamper, verdict="defects")
        run_id = w.meta()["run_id"]
        ref = f"refs/leadhand/{run_id}/violations/1"
        blocker = f"{ref}/blocker"
        run_git(w.repo, "update-ref", blocker, w.base)
        w.act(drive_to(w, "review", 1))
        r = w.cli("next", w.run_dir)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        (v,) = found(w)
        self.assertEqual(v["ref"], ref)
        self.assertEqual([e for e in w.events("violation") if e.get("stage") == "restored"], [])
        # nothing was restored: the reviewer's edit is still in the workspace, and no ref saved it
        self.assertEqual((w.workspace() / "README.md").read_text(), "changed by the reviewer\n")
        self.assertFalse(git_ok(w.repo, "show-ref", "--verify", "--quiet", ref))
        run_git(w.repo, "update-ref", "-d", blocker)
        w.run()
        self.assertEqual(len(found(w)), 1)
        (done,) = [e for e in w.events("violation") if e.get("stage") == "restored"]
        self.assertEqual(done["ref"], ref)
        self.assertEqual(run_git(w.repo, "show", f"{done['ref']}:README.md"), "changed by the reviewer")
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-2"])
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")
        self.assertEqual((w.workspace() / "README.md").read_text(), "hello\n")

    def test_voided_reviews_do_not_spend_the_retry_budget(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed\n"})

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"tamper": tamper, "status": "SPENT"}, {"tamper": tamper, "status": "SPENT"},
                 {"status": "SPENT"}, {})
        w.run()
        self.assertEqual(len(found(w)), 2)
        self.assertEqual(w.events("parked"), [])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        run_id = w.meta()["run_id"]
        self.assertEqual(leadhand_refs(w, "violations"),
                         [f"refs/leadhand/{run_id}/violations/1", f"refs/leadhand/{run_id}/violations/2"])
        for ref in leadhand_refs(w, "violations"):
            self.assertEqual(run_git(w.repo, "show", f"{ref}:README.md"), "changed")
        self.assert_run_tip_is_accepted(w, 1)
        self.assertTrue(on_branch(w, RUN, "src/card1/c1-a1-implement-1.txt"))


class OneCardInFlight(EngineCase):
    def test_unpark_while_another_card_is_in_flight_waits(self):
        """Covers R31 (one card in flight)."""
        # Review repro: unparking card 1 while card 2's review is requested must not interleave.
        w = FakeSupervisor(self, [card(1), card(2)])
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision"
        review = drive_to(w, "review", 2)
        del w.parks[1]
        unpark(w, 1, "go")
        self.assertEqual(w.next()["dispatch_id"], review["dispatch_id"])
        w.run()
        ids = w.dispatch_ids()
        accepted = w.events("accepted")
        self.assertEqual([e["card"] for e in accepted], [2, 1])
        # nothing for card 1 was dispatched until card 2 was accepted
        accept2 = accepted[0]["seq"]
        later = [e for e in w.events("dispatch_requested")
                 if e["card"] == 1 and e["dispatch_id"] != "c1-a1-implement-1"]
        self.assertTrue(later)
        self.assertTrue(all(e["seq"] > accept2 for e in later), [e["dispatch_id"] for e in later])
        self.assertEqual([e for e in w.events("violation") if e.get("by") != "implementer"], [])
        self.assertEqual(leadhand_refs(w, "violations"), [])
        tip = w.branch_tip(RUN)
        self.assertEqual(tip, accepted[1]["sha"])
        for path in ("src/card1/c1-a1-implement-1.txt", "src/card2/c2-a1-implement-1.txt"):
            run_git(w.repo, "cat-file", "-e", f"{RUN}:{path}")
        self.assertIn("c1-a1-verify-1", ids)

    def test_engine_ref_moves_after_dispatch_are_not_violations(self):
        def engine_moves(world, ws, a):
            # what the engine records while the review runs: card 2 accepted on the run branch
            other = run_git(ws, "commit-tree", "-p", world.base, "-m", "card 2",
                            run_git(ws, "rev-parse", f"{world.base}^{{tree}}"))
            run_git(ws, "update-ref", f"refs/heads/{RUN}", other, world.base)
            append_event(world.run_dir, "accepted", card=2, sha=other)

        w = FakeSupervisor(self, [card(1), card(2)])
        w.start()
        w.script(1, "review", {"tamper": engine_moves})
        w.run()
        self.assertEqual(w.events("violation"), [])
        self.assertEqual(w.dispatch_ids()[:2], ["c1-a1-implement-1", "c1-a1-review-1"])
        self.assertNotIn("c1-a1-review-2", w.dispatch_ids())
        self.assertEqual({e["card"] for e in w.events("accepted")}, {1, 2})
        self.assertEqual(leadhand_refs(w, "violations"), [])


class FixRound1(EngineCase):
    def test_repeated_check_violations_park_the_card(self):
        """Covers R31 (the violation cap)."""
        def tamper(world, ws, a):
            write(ws, {"README.md": f"rewritten by {a['dispatch_id']}\n"})

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", *({"tamper": tamper} for _ in range(5)))
        w.run()
        self.assertEqual(len(found(w)), 3)
        self.assertEqual([i for i in w.dispatch_ids() if "review" in i],
                         ["c1-a1-review-1", "c1-a1-review-2", "c1-a1-review-3"])
        (parked,) = w.events("parked")
        self.assertEqual(parked["reason"], "checks change tracked files: README.md")
        self.assertEqual(w.events("accepted"), [])
        self.assertEqual((w.workspace() / "README.md").read_text(), "hello\n")
        run_id = w.meta()["run_id"]
        refs = leadhand_refs(w, "violations")
        self.assertEqual(refs, [f"refs/leadhand/{run_id}/violations/{n}" for n in (1, 2, 3)])
        for n, ref in enumerate(refs, 1):
            self.assertEqual(run_git(w.repo, "show", f"{ref}:README.md"), f"rewritten by c1-a1-review-{n}")
        self.assertEqual(w.branch_tip(RUN), w.base)

    def test_unpark_after_relaunch_starts_fresh_attempt_with_note(self):
        w = parked_world(self)
        w.session("supervisor-2")           # relaunched: the parked worker died with the old session
        unpark(w, 1, NOTE)
        a = w.next()
        self.assertEqual((a["kind"], a["dispatch_id"], a["resume"]), ("implement", "c1-a1-implement-2", None))
        self.assertIn(NOTE, Path(a["brief"]).read_text())
        started = next(e for e in w.events("worker_started") if e["dispatch_id"] == "c1-a1-implement-1")
        self.assertEqual(started["session"], "supervisor-1")
        w.act(a)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assert_run_tip_is_accepted(w, 1)
        for path in ("src/card1/c1-a1-implement-2.txt", "src/card2/c2-a1-implement-1.txt"):
            self.assertTrue(on_branch(w, RUN, path), path)

    def test_operation_left_in_progress_is_blocked_and_preserved(self):
        def leave_merge(world, ws, a):
            back = current_branch(ws)
            run_git(ws, "checkout", "--quiet", "-b", "side")
            commit(ws, {"README.md": "side\n"}, "side change")
            run_git(ws, "checkout", "--quiet", back)
            run_git(ws, "merge", "--quiet", "--no-ff", "--no-commit", "side")

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"tamper": leave_merge}, {})
        w.run()
        self.assertEqual(w.dispatch_ids()[:2], ["c1-a1-implement-1", "c1-a1-implement-2"])
        (v,) = found(w, by="implementer")
        run_id = w.meta()["run_id"]
        self.assertEqual((v["operation"], v["after_dispatch"], v["ref"]),
                         ("merge", "c1-a1-implement-1", f"refs/leadhand/{run_id}/leftovers/1"))
        self.assertEqual(leadhand_refs(w, "leftovers"), [v["ref"]])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "side")
        ws = w.workspace()
        self.assertFalse(git_ok(ws, "rev-parse", "--verify", "--quiet", "MERGE_HEAD"))
        self.assertEqual(current_branch(ws), CARD1)
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assert_run_tip_is_accepted(w, 1)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")

    def test_implementer_leftovers_are_preserved_before_review_without_voiding(self):
        def leave_dirty(world, ws, a):
            write(ws, {"README.md": "uncommitted by the implementer\n"})

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "implement", {"tamper": leave_dirty})
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"])
        (v,) = found(w, by="implementer")
        self.assertEqual(v["paths"], ["README.md"])
        self.assertIsNone(v["operation"])
        self.assertEqual(found(w), [])
        self.assertEqual(leadhand_refs(w, "violations"), [])
        self.assertEqual(leadhand_refs(w, "leftovers"), [v["ref"]])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "uncommitted by the implementer")
        self.assertLess(v["seq"], requested(w, "c1-a1-review-1")["seq"])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assert_run_tip_is_accepted(w, 1)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")

    def conflicted(self) -> FakeSupervisor:
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"})
        unpark(w, 1, NOTE)
        return w

    def test_integration_keeps_the_pre_integration_tip(self):
        w = self.conflicted()
        w.run()
        (intent,) = w.events("integration_started")
        run_id = w.meta()["run_id"]
        self.assertEqual(intent["keep"], f"refs/leadhand/{run_id}/integration/1-1")
        self.assertEqual(run_git(w.repo, "rev-parse", intent["keep"]), intent["tip"])
        self.assertEqual(leadhand_refs(w, "integration"), [intent["keep"]])
        self.assertEqual(run_git(w.repo, "show", f"{intent['keep']}:src/shared.txt"), "card one")
        self.assertFalse(git_ok(w.repo, "merge-base", "--is-ancestor", intent["onto"], intent["keep"]))

    def test_integrate_moving_run_branch_is_violation(self):
        def push_run(world, ws, a):
            run_git(ws, "update-ref", f"refs/heads/{RUN}", "HEAD")

        w = self.conflicted()
        onto = w.branch_tip(RUN)
        w.script(1, "integrate", {"tamper": push_run}, {})
        w.run()
        (v,) = found(w)
        self.assertEqual(v["dispatch_id"], "c1-a2-integrate-1")
        self.assertEqual(v["refs"][f"refs/heads/{RUN}"]["old"], onto)
        keep = v["preserved"][f"refs/heads/{RUN}"]
        self.assertEqual(run_git(w.repo, "rev-parse", keep), v["refs"][f"refs/heads/{RUN}"]["new"])
        self.assertIn("c1-a2-integrate-2", w.dispatch_ids())
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["dispatch_id"], "c1-a2-integrate-2")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(self.assert_run_tip_is_accepted(w, 1), rebased["tip"])
        self.assertTrue(git_ok(w.repo, "merge-base", "--is-ancestor", onto, RUN))
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/shared.txt"), "card one")

    def test_integrate_dropping_card_work_escalates(self):
        def reset_to_onto(world, ws, a):
            run_git(ws, "reset", "--quiet", "--hard", f"refs/heads/{RUN}")

        w = self.conflicted()
        w.script(1, "integrate", {"rebase": False, "tamper": reset_to_onto})
        w.run()
        self.assertEqual(w.events("rebased"), [])
        ids = w.dispatch_ids()
        self.assertIn("c1-a3-implement-1", ids)
        brief = Path(requested(w, "c1-a3-implement-1")["brief"]).read_text()
        (intent,) = w.events("integration_started")
        self.assertIn(intent["keep"], brief)
        self.assertEqual(run_git(w.repo, "rev-parse", intent["keep"]), intent["tip"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assert_run_tip_is_accepted(w, 1)
        self.assertTrue(on_branch(w, RUN, "src/card1/c1-a3-implement-1.txt"))

    def test_integrate_question_answer_goes_through_integration(self):
        w = self.conflicted()
        w.script(1, "integrate", {"rebase": False, "status": "QUESTION"})
        w.script(1, "answer", {}, {"rebase": True})     # the unpark answer, then the integrate answer
        w.rulings[1] = ["Keep card one's version."]
        w.run()
        ids = w.dispatch_ids()
        self.assertIn("c1-a2-answer-1", ids)
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["dispatch_id"], "c1-a2-answer-1")
        self.assertEqual(ids[ids.index("c1-a2-answer-1") + 1], "c1-a2-rereview-1")
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/shared.txt"), "card one")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(self.assert_run_tip_is_accepted(w, 1), rebased["tip"])


class FixRound2(EngineCase):
    def test_voided_answer_to_integrate_keeps_its_resume_chain(self):
        # Review scratch test: an answer resuming an integrate worker moves the run branch.
        def push_run(world, ws, a):
            run_git(ws, "update-ref", f"refs/heads/{RUN}", "HEAD")

        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"})
        unpark(w, 1, NOTE)
        onto = w.branch_tip(RUN)
        w.script(1, "integrate", {"rebase": False, "status": "QUESTION"})
        w.script(1, "answer", {}, {"rebase": True, "tamper": push_run}, {"rebase": True})
        w.rulings[1] = ["Keep card one's version."]
        w.run()
        (v,) = found(w)
        self.assertEqual(v["dispatch_id"], "c1-a2-answer-1")
        self.assertEqual(v["refs"][f"refs/heads/{RUN}"]["old"], onto)
        self.assertEqual(run_git(w.repo, "rev-parse", v["preserved"][f"refs/heads/{RUN}"]),
                         v["refs"][f"refs/heads/{RUN}"]["new"])
        rerun = requested(w, "c1-a2-answer-2")
        self.assertEqual(rerun["resumes"], "c1-a2-integrate-1")
        self.assertEqual(rerun["resume_worker"], worker_of(w, "c1-a2-integrate-1"))
        self.assertEqual(rerun["refs"], {f"refs/heads/{RUN}": onto})
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["dispatch_id"], "c1-a2-answer-2")
        ids = w.dispatch_ids()
        self.assertEqual(ids[ids.index("c1-a2-answer-2") + 1], "c1-a2-rereview-1")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/shared.txt"), "card one")
        self.assertEqual(self.assert_run_tip_is_accepted(w, 1), rebased["tip"])

    def test_unpark_after_violation_cap_reruns_the_check(self):
        """Covers R32."""
        def tamper(world, ws, a):
            write(ws, {"README.md": f"rewritten by {a['dispatch_id']}\n"})

        w = FakeSupervisor(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "review", *({"tamper": tamper} for _ in range(3)), {})
        w.run()
        (parked,) = w.events("parked")
        self.assertTrue(parked["reason"].startswith("checks change tracked files"))
        unpark(w, 1, "The test command no longer rewrites README.md.")
        a = w.next()
        self.assertEqual((a["action"], a["kind"], a["dispatch_id"], a["resume"]),
                         ("dispatch", "review", "c1-a1-review-4", None))
        old = requested(w, "c1-a1-review-3")
        self.assertEqual(a["route"], {"vendor": old["vendor"], "model": old["model"], "effort": old["effort"]})
        self.assertEqual(a["role"], old["role"])
        self.assertIn("no longer rewrites README.md", Path(a["brief"]).read_text())
        w.act(a)
        w.run()
        self.assertEqual([i for i in w.dispatch_ids() if "implement" in i or "answer" in i], ["c1-a1-implement-1"])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-4"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        impl_tip = next(e["tip"] for e in w.events("result_recorded") if e["dispatch_id"] == "c1-a1-implement-1")
        self.assertEqual(self.assert_run_tip_is_accepted(w, 1), impl_tip)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")

    def test_voided_rereview_is_rerun_by_a_fresh_worker(self):
        # R33: a voided check never resumes the reviewer whose result was voided.
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the reviewer\n"})

        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"verdict": "defects"})
        w.script(1, "fix", {"files": {"src/widget.py": "fixed = True\n"}})     # production: a rereview
        w.script(1, "rereview", {"tamper": tamper}, {})
        w.run()
        first = requested(w, "c1-a1-rereview-1")
        self.assertEqual(first["resumes"], "c1-a1-review-1")
        self.assertEqual(first["resume_worker"], worker_of(w, "c1-a1-review-1"))
        (v,) = found(w)
        self.assertEqual(v["dispatch_id"], "c1-a1-rereview-1")
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "changed by the reviewer")
        rerun = requested(w, "c1-a1-rereview-2")
        self.assertIsNone(rerun["resumes"])
        self.assertIsNone(rerun["resume_worker"])
        started = w.started[w.dispatch_ids().index("c1-a1-rereview-2")]
        self.assertIsNone(started["resume"])
        self.assertEqual((rerun["vendor"], rerun["model"], rerun["effort"], rerun["role"]),
                         (first["vendor"], first["model"], first["effort"], first["role"]))
        self.assertEqual(rerun["extra"], first["extra"])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")],
                         ["c1-a1-review-1", "c1-a1-rereview-2"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assert_run_tip_is_accepted(w, 1)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/widget.py"), "fixed = True")
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")


class CodexIntegrate(EngineCase):
    """Ruling R39: integrating rewrites the card branch, and Codex's sandbox refuses Git writes."""

    def test_integrate_after_codex_implementer_routes_to_claude(self):
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"}, installed=("claude", "codex"))
        implement = requested(w, "c1-a1-implement-1")
        self.assertEqual(implement["vendor"], "codex")
        argv = shlex.split(w.started[0]["message"])
        self.assertEqual(argv[argv.index("--kind") + 1], "implement")
        self.assertEqual(argv[argv.index("--branch") + 1], CARD1)
        unpark(w, 1, NOTE)
        integrate = drive_to(w, "integrate", 1)
        self.assertEqual(integrate["route"]["vendor"], "claude")
        self.assertEqual(integrate["agent"], "leadhand:implementer")
        self.assertEqual(integrate["role"], implement["role"])
        w.act(integrate)
        w.run()
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["dispatch_id"], "c1-a2-integrate-1")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        self.assertEqual(self.assert_run_tip_is_accepted(w, 1), rebased["tip"])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/shared.txt"), "card one")

    def test_failed_runner_commit_is_recorded_blocked_environment(self):
        w = codex_world(self, [card(1)])
        for name in ("EMAIL", "GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"):
            w.env.pop(name, None)
        run_git(w.repo, "config", "--unset", "user.name")
        run_git(w.repo, "config", "--unset", "user.email")
        run_git(w.repo, "config", "user.useConfigOnly", "true")
        w.start()
        w.script(1, "implement", {"commit": False})
        a = w.next()
        self.assertEqual(a["agent"], "leadhand:codex")
        w.act(a)
        (recorded,) = w.events("result_recorded")
        self.assertEqual((recorded["status"], recorded["blocked_by"]), ("BLOCKED", "environment"))
        self.assertIsNone(recorded["committed"])
        self.assertTrue(recorded["commit_error"])
        self.assertIn("src/card1/", run_git(w.workspace(), "status", "--porcelain", "--untracked-files=all"))
        # nothing was committed: the card branch is still at the run's base
        self.assertEqual(run_git(w.repo, "rev-parse", f"refs/heads/{CARD1}"), w.base)

    def test_integrate_without_claude_parks_the_card(self):
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"}, installed=("codex",))
        unpark(w, 1, NOTE)
        w.run()
        parked = [e for e in w.events("parked") if e["card"] == 1][-1]
        self.assertEqual(parked["reason"], "integration needs a Claude worker")
        self.assertNotIn("c1-a2-integrate-1", w.dispatch_ids())
        # only card 2 reached the run branch
        self.assertEqual([e["card"] for e in w.events("accepted")], [2])
        self.assert_run_tip_is_accepted(w, 2)
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/shared.txt"), "card two")


if __name__ == "__main__":
    unittest.main()
