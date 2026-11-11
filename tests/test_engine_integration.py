"""Engine: integration before acceptance, unparking, and reviewer/verifier violations."""
import shlex
import unittest
from unittest import mock
from pathlib import Path

from fakeworld import FakeWorld, card
from gitfixture import commit, current_branch, run_git, write
from leadhand import engine
from leadhand.engine import EngineError
from leadhand.state import append_event

RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
NOTE = "Use JSON; it is simpler."


def parked_world(test, *, card1_files=None, card2_files=None, **kw):
    """Card 1 asks a product question and is parked; independent card 2 is accepted, so the
    run tip moves past card 1's base."""
    w = FakeWorld(test, [card(1), card(2)], **kw)
    w.session("supervisor-1")            # the supervisor session the workers start in
    w.start()
    w.script(1, "implement", {"status": "QUESTION", **({"files": card1_files} if card1_files else {})})
    if card2_files:
        w.script(2, "implement", {"files": card2_files})
    w.parks[1] = "product decision: storage format"
    w.run()
    del w.parks[1]
    return w


def drive_to(w, kind, card_number):
    """Act on every action until the dispatch of `kind` for the card; return it unacted."""
    for _ in range(80):
        a = w.next()
        if a["action"] == "dispatch" and a["kind"] == kind and a["card"] == card_number:
            return a
        w.act(a)
    raise AssertionError(f"no {kind} dispatch for card {card_number}")


def git_ok(cwd, *args):
    try:
        run_git(cwd, *args)
    except AssertionError:
        return False
    return True


def requested(w, dispatch_id):
    return next(e for e in w.events("dispatch_requested") if e["dispatch_id"] == dispatch_id)


def worker_of(w, dispatch_id):
    return next(e["worker_id"] for e in w.events("worker_started") if e["dispatch_id"] == dispatch_id)


def found(w, by="check"):
    return [e for e in w.events("violation") if e.get("stage") == "found" and (e.get("by") or "check") == by]


class Integration(unittest.TestCase):
    def test_resumed_parked_card_is_rebased_rereviewed_verified_and_fast_forwarded(self):
        w = parked_world(self)
        self.assertEqual([e["card"] for e in w.events("accepted")], [2])
        run_tip = w.branch_tip(RUN)
        event = engine.unpark(w.run_dir, card=1, note=NOTE)
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

    def test_integration_conflict_requests_integrate_attempt(self):
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"})
        run_tip = w.branch_tip(RUN)
        engine.unpark(w.run_dir, card=1, note=NOTE)
        integrate = drive_to(w, "integrate", 1)

        (intent,) = w.events("integration_started")
        (conflict,) = w.events("conflict")
        self.assertEqual(conflict["paths"], ["src/shared.txt"])
        self.assertEqual(w.events("rebased"), [])
        # the engine's own rebase was aborted: the card branch is as it was, and checked out
        self.assertEqual(w.branch_tip(CARD1), intent["tip"])
        self.assertEqual(current_branch(w.workspace()), CARD1)
        self.assertFalse(run_git(w.workspace(), "status", "--porcelain", "--untracked-files=no"))
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
        engine.unpark(w.run_dir, card=1, note=NOTE)
        w.act(drive_to(w, "review", 1))
        r = w.cli("next", w.run_dir, crash=point)
        self.assertEqual(r.returncode, 97, r.stderr)
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
        engine.unpark(w.run_dir, card=1, note=NOTE)
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


class Unpark(unittest.TestCase):
    def test_unpark_without_a_known_worker_starts_a_fresh_attempt(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        first = w.next()
        self.assertEqual(first["dispatch_id"], "c1-a1-implement-1")
        engine.record(w.run_dir, "park", card=1, reason="operator wants to look first")
        engine.unpark(w.run_dir, card=1, note=NOTE)
        a = w.next()
        self.assertEqual((a["action"], a["kind"], a["dispatch_id"], a["resume"]),
                         ("dispatch", "implement", "c1-a1-implement-2", None))
        self.assertIn(NOTE, Path(a["brief"]).read_text())
        w.act(a)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_unpark_cli_appends_unparked_and_refuses_a_card_not_parked(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        w.next()
        r = w.cli("unpark", w.run_dir, "--card", "1", "--note", NOTE)
        self.assertEqual(r.returncode, 1)
        self.assertIn("not parked", r.stderr)
        engine.record(w.run_dir, "park", card=1, reason="hold")
        r = w.cli("unpark", w.run_dir, "--card", "1", "--note", NOTE)
        self.assertEqual(r.returncode, 0, r.stderr)
        (event,) = w.events("unparked")
        self.assertEqual((event["card"], event["note"]), (1, NOTE))
        with self.assertRaises(EngineError):
            engine.unpark(w.run_dir, card=1, note=NOTE)
        with self.assertRaises(EngineError):
            engine.unpark(w.run_dir, card=1, note="  ")


class Violations(unittest.TestCase):
    def world(self, kind, tamper, **first):
        w = FakeWorld(self, [card(1)])
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
        # the voided defects verdict never counted: no fix round, the re-run's verdict decided
        self.assertEqual(w.events("fix_round"), [])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-2"])
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "changed by the reviewer")
        self.assertEqual((w.workspace() / "README.md").read_text(), "hello\n")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

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
        # the reviewer's commit is preserved, not lost
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:src/reviewer.txt"), "by the reviewer")
        moved = v["refs"][f"refs/heads/{CARD1}"]
        self.assertEqual(moved["old"], impl_tip)
        keep = v["preserved"][f"refs/heads/{CARD1}"]
        self.assertEqual(run_git(w.repo, "rev-parse", keep), moved["new"])

    def test_verifier_staged_change_is_violation(self):
        w = parked_world(self)
        engine.unpark(w.run_dir, card=1, note=NOTE)

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

    def test_reviewer_committing_on_run_branch_and_returning_to_card_tip_is_violation(self):
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

    def test_violation_interrupted_before_restore_still_voids_on_restart(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the reviewer\n"})

        w = self.world("review", tamper, verdict="defects")
        w.act(drive_to(w, "review", 1))
        real = engine.preserve_and_restore
        with mock.patch.object(engine, "preserve_and_restore", side_effect=RuntimeError("died mid-restore")):
            with self.assertRaises(RuntimeError):
                w.next()
        (v,) = found(w)
        self.assertEqual([e for e in w.events("violation") if e.get("stage") == "restored"], [])
        self.assertIs(engine.preserve_and_restore, real)
        w.run()
        self.assertEqual(len(found(w)), 1)
        (done,) = [e for e in w.events("violation") if e.get("stage") == "restored"]
        self.assertEqual(run_git(w.repo, "show", f"{done['ref']}:README.md"), "changed by the reviewer")
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-2"])
        self.check_rerun(w, "review", "c1-a1-review-1", "c1-a1-review-2")

    def test_voided_reviews_do_not_spend_the_retry_budget(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed\n"})

        w = FakeWorld(self, [card(1)])
        w.start()
        w.script(1, "review", {"tamper": tamper, "status": "SPENT"}, {"tamper": tamper, "status": "SPENT"},
                 {"status": "SPENT"}, {})
        w.run()
        self.assertEqual(len(found(w)), 2)
        self.assertEqual(w.events("parked"), [])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])


class OneCardInFlight(unittest.TestCase):
    def test_unpark_while_another_card_is_in_flight_waits(self):
        # Review repro: unparking card 1 while card 2's review is requested must not interleave.
        w = FakeWorld(self, [card(1), card(2)])
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision"
        review = drive_to(w, "review", 2)
        del w.parks[1]
        engine.unpark(w.run_dir, card=1, note="go")
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
            append_event(world.paths, "accepted", card=2, sha=other)

        w = FakeWorld(self, [card(1), card(2)])
        w.start()
        w.script(1, "review", {"tamper": engine_moves})
        w.run()
        self.assertEqual(w.events("violation"), [])
        self.assertEqual(w.dispatch_ids()[:2], ["c1-a1-implement-1", "c1-a1-review-1"])
        self.assertNotIn("c1-a1-review-2", w.dispatch_ids())
        self.assertEqual({e["card"] for e in w.events("accepted")}, {1, 2})


class FixRound1(unittest.TestCase):
    def test_repeated_check_violations_park_the_card(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": f"rewritten by {a['dispatch_id']}\n"})

        w = FakeWorld(self, [card(1)])
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

    def test_unpark_after_relaunch_starts_fresh_attempt_with_note(self):
        w = parked_world(self)
        w.session("supervisor-2")           # relaunched: the parked worker died with the old session
        engine.unpark(w.run_dir, card=1, note=NOTE)
        a = w.next()
        self.assertEqual((a["kind"], a["dispatch_id"], a["resume"]), ("implement", "c1-a1-implement-2", None))
        self.assertIn(NOTE, Path(a["brief"]).read_text())
        started = next(e for e in w.events("worker_started") if e["dispatch_id"] == "c1-a1-implement-1")
        self.assertEqual(started["session"], "supervisor-1")
        w.act(a)
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])

    def test_operation_left_in_progress_is_blocked_and_preserved(self):
        def leave_merge(world, ws, a):
            back = current_branch(ws)
            run_git(ws, "checkout", "--quiet", "-b", "side")
            commit(ws, {"README.md": "side\n"}, "side change")
            run_git(ws, "checkout", "--quiet", back)
            run_git(ws, "merge", "--quiet", "--no-ff", "--no-commit", "side")

        w = FakeWorld(self, [card(1)])
        w.start()
        w.script(1, "implement", {"tamper": leave_merge}, {})
        w.run()
        self.assertEqual(w.dispatch_ids()[:2], ["c1-a1-implement-1", "c1-a1-implement-2"])
        (v,) = found(w, by="implementer")
        run_id = w.meta()["run_id"]
        self.assertEqual((v["operation"], v["after_dispatch"], v["ref"]),
                         ("merge", "c1-a1-implement-1", f"refs/leadhand/{run_id}/leftovers/1"))
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "side")
        ws = w.workspace()
        self.assertFalse(git_ok(ws, "rev-parse", "--verify", "--quiet", "MERGE_HEAD"))
        self.assertEqual(current_branch(ws), CARD1)
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_implementer_leftovers_are_preserved_before_review_without_voiding(self):
        def leave_dirty(world, ws, a):
            write(ws, {"README.md": "uncommitted by the implementer\n"})

        w = FakeWorld(self, [card(1)])
        w.start()
        w.script(1, "implement", {"tamper": leave_dirty})
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1"])
        (v,) = found(w, by="implementer")
        self.assertEqual(v["paths"], ["README.md"])
        self.assertIsNone(v["operation"])
        self.assertEqual(found(w), [])
        self.assertEqual(run_git(w.repo, "show", f"{v['ref']}:README.md"), "uncommitted by the implementer")
        self.assertLess(v["seq"], requested(w, "c1-a1-review-1")["seq"])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def conflicted(self):
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"})
        engine.unpark(w.run_dir, card=1, note=NOTE)
        return w

    def test_integration_keeps_the_pre_integration_tip(self):
        w = self.conflicted()
        w.run()
        (intent,) = w.events("integration_started")
        run_id = w.meta()["run_id"]
        self.assertEqual(intent["keep"], f"refs/leadhand/{run_id}/integration/1-1")
        self.assertEqual(run_git(w.repo, "rev-parse", intent["keep"]), intent["tip"])

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
        self.assertIn("c1-a2-integrate-2", w.dispatch_ids())
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["dispatch_id"], "c1-a2-integrate-2")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])

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


class FixRound2(unittest.TestCase):
    def test_voided_answer_to_integrate_keeps_its_resume_chain(self):
        # Review scratch test: an answer resuming an integrate worker moves the run branch.
        def push_run(world, ws, a):
            run_git(ws, "update-ref", f"refs/heads/{RUN}", "HEAD")

        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"})
        engine.unpark(w.run_dir, card=1, note=NOTE)
        onto = w.branch_tip(RUN)
        w.script(1, "integrate", {"rebase": False, "status": "QUESTION"})
        w.script(1, "answer", {}, {"rebase": True, "tamper": push_run}, {"rebase": True})
        w.rulings[1] = ["Keep card one's version."]
        w.run()
        (v,) = found(w)
        self.assertEqual(v["dispatch_id"], "c1-a2-answer-1")
        self.assertEqual(v["refs"][f"refs/heads/{RUN}"]["old"], onto)
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

    def test_unpark_after_violation_cap_reruns_the_check(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": f"rewritten by {a['dispatch_id']}\n"})

        w = FakeWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "review", *({"tamper": tamper} for _ in range(3)), {})
        w.run()
        (parked,) = w.events("parked")
        self.assertTrue(parked["reason"].startswith("checks change tracked files"))
        engine.unpark(w.run_dir, card=1, note="The test command no longer rewrites README.md.")
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

    def test_voided_rereview_is_rerun_by_a_fresh_worker(self):
        # R33: a voided check never resumes the reviewer whose result was voided.
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the reviewer\n"})

        w = FakeWorld(self, [card(1)])
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


class CodexIntegrate(unittest.TestCase):
    """Ruling R39: integrating rewrites the card branch, and Codex's sandbox refuses Git writes."""

    def test_integrate_after_codex_implementer_routes_to_claude(self):
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"}, installed=("claude", "codex"))
        implement = requested(w, "c1-a1-implement-1")
        self.assertEqual(implement["vendor"], "codex")
        argv = shlex.split(w.started[0]["message"])
        self.assertEqual(argv[argv.index("--kind") + 1], "implement")
        self.assertEqual(argv[argv.index("--branch") + 1], CARD1)
        engine.unpark(w.run_dir, card=1, note=NOTE)
        integrate = drive_to(w, "integrate", 1)
        self.assertEqual(integrate["route"]["vendor"], "claude")
        self.assertEqual(integrate["agent"], "leadhand:implementer")
        self.assertEqual(integrate["role"], implement["role"])
        w.act(integrate)
        w.run()
        (rebased,) = w.events("rebased")
        self.assertEqual(rebased["dispatch_id"], "c1-a2-integrate-1")
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])

    def test_integrate_without_claude_parks_the_card(self):
        w = parked_world(self, card1_files={"src/shared.txt": "card one\n"},
                         card2_files={"src/shared.txt": "card two\n"}, installed=("codex",))
        engine.unpark(w.run_dir, card=1, note=NOTE)
        w.run()
        parked = [e for e in w.events("parked") if e["card"] == 1][-1]
        self.assertEqual(parked["reason"], "integration needs a Claude worker")
        self.assertNotIn("c1-a2-integrate-1", w.dispatch_ids())


if __name__ == "__main__":
    unittest.main()
