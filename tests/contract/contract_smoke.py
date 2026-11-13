"""The fake supervisor end to end: a two-card run, and the cleanup that leaves no runner behind.

Smoke is converted from test_engine_cards.HappyPath.test_happy_path_two_cards_accepted_in_order.
"""
import json
import time
import unittest

from gitfixture import run_git
from harness import same_process
from supervisor import FakeSupervisor, card


class Smoke(unittest.TestCase):
    def test_two_cards_accepted_in_order(self):
        w = FakeSupervisor(self, [card(1), card(2, depends=[1], risk="medium")])
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
        charges = {c["charge_id"]: c for c in w.charges()}
        self.assertEqual(len(charges), 4)
        attempts = w.attempts()
        self.assertEqual([a["dispatch"] for a in attempts], w.dispatch_ids())
        for a in attempts:
            self.assertEqual(len(a["charge_ids"]), 1)
            c = charges[a["charge_ids"][0]]
            self.assertEqual(c["charge_id"], f"{a['dispatch']}:claude:{a['worker_id']}")
            self.assertEqual(c["role"], a["role"])
            self.assertTrue(c["usage"]["known"])
        # the run folder keeps a copy of each dispatch's brief, report and reply
        folder = w.run_dir / "cards" / "1" / "c1-a1-review-1"
        for name in ("brief.md", "report.md", "reply.md"):
            self.assertTrue((folder / name).is_file(), name)


class Cleanup(unittest.TestCase):
    def test_no_runner_outlives_its_test(self):
        class Host(unittest.TestCase):         # a test whose cleanups this one runs, then inspects
            def runTest(self):
                pass

        host = Host()
        w = FakeSupervisor(host, [card(1)], installed=("claude", "codex"))
        try:
            w.start()
            a = w.next()
            self.assertEqual(a["agent"], "leadhand:codex", a)
            w.launch(a)
            proxy = w.start_proxy(a, {"sleep": 120})
            runner_json = w.run_dir / "workers" / a["dispatch_id"] / "runner.json"
            deadline = time.monotonic() + 30
            while True:
                data = json.loads(runner_json.read_text(encoding="utf-8")) if runner_json.exists() else {}
                live = [data.get(who) or {} for who in ("runner", "codex")]
                if all(same_process(r.get("pid"), r.get("start")) for r in live):
                    break
                if proxy.poll() is not None:
                    self.fail(f"the dispatch line exited ({proxy.returncode}) before its runner and Codex "
                              f"were both running:\n{proxy.communicate()}")
                if time.monotonic() > deadline:
                    self.fail(f"no live runner and Codex in {runner_json} after 30 s: {data}")
                time.sleep(0.05)
            records = [json.loads(p.read_text(encoding="utf-8")) for p in w.run_dir.glob("workers/*/runner.json")]
        finally:
            host.doCleanups()
        self.assertGreaterEqual(len(records), 1)
        survivors = [(who, rec[who]) for rec in records for who in ("runner", "codex")
                     if rec.get(who) and same_process(rec[who].get("pid"), rec[who].get("start"))]
        self.assertEqual(survivors, [])


if __name__ == "__main__":
    unittest.main()
