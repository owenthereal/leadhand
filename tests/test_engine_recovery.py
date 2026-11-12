"""Engine fix round 1: resuming blocked runs, run collisions, Codex stops and crash points."""
import contextlib
import io
import json
import unittest
from pathlib import Path
from unittest import mock

from fakeworld import FakeWorld, card
from gitfixture import commit, run_git
from leadhand import engine
from leadhand.engine import EngineError
from leadhand.state import append_event


def card_list(n=1):
    return [card(i) for i in range(1, n + 1)]


class ResumeAfterBlocked(unittest.TestCase):
    def test_blocked_provider_capacity_resumes_with_a_new_dispatch(self):
        w = FakeWorld(self, card_list())
        w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "provider capacity"))
        append_event(w.paths, "resumed", note="capacity is back")
        again = w.next()
        self.assertEqual((again["action"], again["dispatch_id"], again["attempt"]),
                         ("dispatch", "c1-a1-implement-2", 1))
        self.assertEqual(again["route"], w.started[0]["route"])
        self.assertEqual(len(w.events("outcome")), 1)
        w.act(again)
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_blocked_environment_resumes_with_a_fresh_retry_budget(self):
        w = FakeWorld(self, card_list())
        w.start()
        blocked = {"status": "BLOCKED", "blocked_by": "environment"}
        w.script(1, "implement", blocked, blocked)
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "environment"))
        append_event(w.paths, "resumed", note="tool reinstalled")
        w.script(1, "implement", blocked, {"status": "READY"})
        w.run()
        self.assertEqual(w.dispatch_ids()[:4], ["c1-a1-implement-1", "c1-a1-implement-2",
                                                "c1-a1-implement-3", "c1-a1-implement-4"])
        self.assertEqual(len(w.events("outcome")), 1)
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_relaunch_without_an_outcome_keeps_the_retry_budget(self):
        w = FakeWorld(self, card_list())
        w.start()
        blocked = {"status": "BLOCKED", "blocked_by": "environment"}
        w.script(1, "implement", blocked, blocked)
        w.act(w.next())
        retry = w.next()
        self.assertEqual(retry["dispatch_id"], "c1-a1-implement-2")
        append_event(w.paths, "resumed", note="the launcher relaunched the supervisor")
        w.act(retry)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"], done["qualifier"]), ("done", "blocked", "environment"))

    def test_park_after_the_run_ended_raises(self):
        w = FakeWorld(self, card_list())
        w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        w.run()
        with self.assertRaises(EngineError):
            engine.record(w.run_dir, "park", card=1, reason="too late")
        self.assertEqual(w.events("parked"), [])


class RunCollisions(unittest.TestCase):
    def test_start_returns_a_blocked_run_as_existing(self):
        w = FakeWorld(self, card_list())
        first = w.start()
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        w.run()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            again = engine.start(w.deck_path, repo=w.repo)
        self.assertEqual(again, first)
        self.assertIn(f"existing run: {first}", out.getvalue())

    def test_new_run_suffixes_a_branch_owned_by_another_run(self):
        w = FakeWorld(self, card_list())
        first = w.start()
        first_meta = json.loads((first / "run.json").read_text())
        self.assertEqual((first_meta["branch"], first_meta["branch_requested"]), ("leadhand/demo", "leadhand/demo"))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            second = engine.start(w.deck_path, repo=w.repo, new=True)
        self.assertNotEqual(second, first)
        meta = json.loads((second / "run.json").read_text())
        self.assertEqual((meta["branch"], meta["branch_requested"]), ("leadhand/demo-2", "leadhand/demo"))
        self.assertIn("run branch: leadhand/demo-2", out.getvalue())
        self.assertNotEqual(meta["workspace"], first_meta["workspace"])
        with contextlib.redirect_stdout(io.StringIO()):
            third = engine.start(w.deck_path, repo=w.repo, new=True)
        self.assertEqual(json.loads((third / "run.json").read_text())["branch"], "leadhand/demo-3")

        # the suffixed run works on its own branches and leaves the first run's alone
        w.run_dir = second
        before = w.branch_tip("leadhand/demo")
        w.run()
        self.assertEqual(w.branch_tip("leadhand/demo"), before)
        (accepted,) = w.events("accepted")
        self.assertEqual(w.branch_tip("leadhand/demo-2"), accepted["sha"])
        self.assertEqual(w.events("card_cut")[0]["branch"], "leadhand/demo-2--card-1")

    def test_start_after_a_finished_run_suffixes_its_branch(self):
        w = FakeWorld(self, card_list())
        first = w.start()
        w.run()
        append_event(w.paths, "outcome", outcome="complete", qualifier="", reasons=[])
        finished_tip = w.branch_tip("leadhand/demo")
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("run branch: leadhand/demo-2", r.stdout)
        self.assertNotIn("existing run", r.stdout)
        self.assertIn(f"run: {first}-2\n", r.stdout)
        self.assertEqual(w.branch_tip("leadhand/demo"), finished_tip)
        self.assertEqual(w.branch_tip("leadhand/demo-2"), w.base)

    def test_interrupted_start_retry_uses_a_free_branch(self):
        w = FakeWorld(self, card_list())
        real = engine.write_json_atomic
        calls = []

        def fail_once(path, data):
            if Path(path).name == "run.json" and not calls:
                calls.append(path)
                raise OSError("simulated crash before run.json")
            return real(path, data)

        with mock.patch.object(engine, "write_json_atomic", side_effect=fail_once):
            with self.assertRaises(OSError):
                w.start()
        self.assertEqual(run_git(w.repo, "rev-parse", "refs/heads/leadhand/demo"), w.base)
        run_dir = w.start()
        meta = json.loads((run_dir / "run.json").read_text())
        self.assertEqual((meta["branch"], meta["branch_requested"]), ("leadhand/demo-2", "leadhand/demo"))
        self.assertTrue(Path(meta["workspace"]).is_dir())
        w.run()
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])

    def test_unrecorded_existing_branch_is_not_reused(self):
        w = FakeWorld(self, card_list())
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
        w = FakeWorld(self, card_list(), gates=True)
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
        w = FakeWorld(self, card_list())
        self._unrelated_branch(w, "leadhand/demo--card-final")
        w.start()
        self.assertEqual(w.meta()["branch"], "leadhand/demo-2")

    def test_card_branch_the_run_did_not_cut_is_refused(self):
        w = FakeWorld(self, card_list())
        w.start()
        self.assertEqual(w.meta()["branch"], "leadhand/demo")
        # Someone makes the card branch after the run started, before the run cuts it.
        theirs = self._unrelated_branch(w, "leadhand/demo--card-1")
        with self.assertRaisesRegex(Exception, "leadhand/demo--card-1"):
            w.next()
        self.assertEqual(w.events("card_cut"), [])
        self.assertEqual(w.branch_tip("leadhand/demo--card-1"), theirs)
        self.assertEqual(w.branch_tip("leadhand/demo"), w.base)

    def test_new_run_without_worktree_ignores_a_stale_scratch_report(self):
        w = FakeWorld(self, card_list(), config="[run]\nworktree = false\n")
        w.start()
        w.run()
        append_event(w.paths, "outcome", outcome="complete", qualifier="", reasons=[])
        old_report = Path(w.started[0]["report"])
        self.assertTrue(old_report.is_file())
        w.write_deck(card_list(), branch="leadhand/demo-2")
        w.start()
        a = w.next()
        self.assertEqual(Path(a["report"]), old_report)
        self.assertFalse(old_report.exists())
        w.script(1, "implement", {"report": False, "reply": "no status here"})
        worker = w.launch(a)
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                              reply=w.perform(a, worker))
        self.assertEqual(event["status"], "BLOCKED")


class CodexStops(unittest.TestCase):
    def test_hung_proxy_over_a_finished_runner_keeps_its_status(self):
        w = FakeWorld(self, card_list(), installed=("claude", "codex"))
        w.start()
        a = w.next()
        w.launch(a)
        w.run_proxy(a, {"report": False})        # the runner finishes; the proxy never reports
        w.advance(66 * 60)
        stop = w.next()
        self.assertEqual((stop["action"], stop["vendor"], stop["record_result"]), ("stop_worker", "codex", False))
        (result,) = w.events("result_recorded")
        self.assertEqual((result["status"], result["stopped"]), ("READY", False))

    def test_codex_stop_before_the_proxy_ran_prewrites_the_request(self):
        w = FakeWorld(self, card_list(), installed=("claude", "codex"))
        w.start()
        a = w.next()
        w.launch(a)
        w.advance(66 * 60)
        stop = w.next()
        self.assertEqual(stop["action"], "stop_worker")
        wd = w.run_dir / "workers" / a["dispatch_id"]
        self.assertTrue((wd / "request.json").is_file())
        result = json.loads((wd / "result.json").read_text())
        self.assertEqual(result["exit_reason"], "cancelled")
        (event,) = w.events("result_recorded")
        self.assertEqual(event["status"], "SPENT")
        out = w.run_proxy(a, {})                 # a late proxy finds the cancelled result
        self.assertIn("session:", out)
        log = w.tmp / "codex log.jsonl"
        starts = [json.loads(l) for l in log.read_text().splitlines()] if log.exists() else []
        self.assertEqual([r for r in starts if r.get("event") == "start"], [])
        self.assertEqual(json.loads((wd / "result.json").read_text()), result)


class Untested(unittest.TestCase):
    def test_reviewer_spent_twice_parks_the_card(self):
        w = FakeWorld(self, card_list())
        w.start()
        w.script(1, "review", {"status": "SPENT"}, {"status": "SPENT"})
        w.run()
        self.assertEqual(w.dispatch_ids(), ["c1-a1-implement-1", "c1-a1-review-1", "c1-a1-review-2"])
        (parked,) = w.events("parked")
        self.assertEqual(parked["reason"], "review ended SPENT twice without a verdict")

    def test_pinned_vendor_not_installed_parks_the_card(self):
        w = FakeWorld(self, [card(1, route="codex gpt-6-luna low"), card(2)])
        w.start()
        w.run()
        (parked,) = w.events("parked")
        self.assertEqual((parked["card"], parked["reason"]), (1, "pinned vendor not installed"))
        self.assertEqual(w.dispatch_ids(), ["c2-a1-implement-1", "c2-a1-review-1"])

    def worker_crash(self, point):
        w = FakeWorld(self, card_list())
        w.start()
        w.script(1, "implement", {"crash": point})
        a = w.next()
        worker = w.launch(a)
        self.assertIsNone(w.perform(a, worker))
        self.assertEqual(w.next()["action"], "wait")
        w.advance(61 * 60)
        stop = w.next()
        self.assertEqual((stop["action"], stop["record_result"]), ("stop_worker", True))
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                              reply="(stopped)", stopped=True)
        self.assertEqual(event["status"], "SPENT")
        nxt = w.next()
        self.assertEqual((nxt["dispatch_id"], nxt["resume"]), ("c1-a2-implement-1", None))
        return w, event

    def test_worker_crash_before_commit_escalates(self):
        w, event = self.worker_crash("before_commit")
        self.assertEqual(event["tip"], w.base)

    def test_worker_crash_after_commit_keeps_the_commit(self):
        w, event = self.worker_crash("after_commit")
        self.assertNotEqual(event["tip"], w.base)
        self.assertEqual(event["tip"], w.branch_tip("leadhand/demo--card-1"))

    def test_crash_before_result_record_writes_nothing(self):
        w = FakeWorld(self, card_list())
        w.start()
        a = w.next()
        worker = w.launch(a)
        reply = w.perform(a, worker)
        r = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                  "--reply", "-", crash="before_result_record", stdin=reply)
        self.assertEqual(r.returncode, 97, r.stderr)
        self.assertFalse((w.run_dir / "cards" / "1" / a["dispatch_id"]).exists())
        self.assertEqual(w.charges(), [])
        self.assertEqual(w.events("result_recorded"), [])
        event = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker, reply=reply)
        self.assertEqual(event["status"], "READY")
        self.assertEqual(len(w.charges()), 1)


if __name__ == "__main__":
    unittest.main()
