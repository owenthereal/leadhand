"""The fake supervisor end to end: a two-card run, the cleanup that leaves no runner behind, and a
crash drive whose group kill takes down every process the drive started.

Smoke is converted from test_engine_cards.HappyPath.test_happy_path_two_cards_accepted_in_order.
"""
import json
import os
import signal
import subprocess
import time
import traceback
import unittest
from unittest import mock

import harness
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
                    try:
                        output = proxy.communicate(timeout=10)
                    except subprocess.TimeoutExpired:   # a descendant still holds the pipes
                        output = "(not read: a descendant held the pipes)"
                    self.fail(f"the dispatch line exited ({proxy.returncode}) before its runner and Codex "
                              f"were both running:\n{output}")
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


class CrashDrive(unittest.TestCase):
    def test_runner_crash_takes_down_everything_the_drive_started(self):
        # The drive runs in a forked child that is its own process group, as a supervisor session
        # is. The runner's crash point kills that group: the dispatch line in flight must die with
        # it, as FakeWorld's in-process engine and proxies did. Codex ignores SIGINT, so a dispatch
        # line left alive would sit in its cancel of the orphaned Codex for up to 30 s.
        w = FakeSupervisor(self, [card(1)], installed=("claude", "codex"))
        w.start()
        w.script(1, "implement", {"sleep": 120})
        started = w.tmp / "started processes.jsonl"
        errors = w.tmp / "drive error.txt"

        class Logged(subprocess.Popen):
            def __init__(self, argv, *args, **kwargs):
                super().__init__(argv, *args, **kwargs)
                with open(started, "a", encoding="utf-8") as f:
                    f.write(json.dumps({"pid": self.pid, "start": harness._start_time(self.pid),
                                        "argv": [str(a) for a in argv]}) + "\n")

        pid = os.fork()
        if pid == 0:
            code = 1
            try:
                os.setpgid(0, 0)
                w.env.update({"LEADHAND_CRASH_AT": "after_worker_launch", "LEADHAND_CRASH_KILL_PGID": str(os.getpid()),
                              "FAKE_CODEX_IGNORE_SIGINT": "1"})
                with mock.patch.object(subprocess, "Popen", Logged):
                    w.run()
                code = 0
            except BaseException:
                errors.write_text(traceback.format_exc(), encoding="utf-8")
            finally:
                os._exit(code)
        deadline = time.monotonic() + 120
        while not os.waitpid(pid, os.WNOHANG)[0]:
            if time.monotonic() > deadline:
                os.killpg(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
                self.fail("the crash drive hung")
            time.sleep(0.02)
        procs = harness.read_jsonl(started)
        try:
            self.assertFalse(errors.exists(), errors.read_text(encoding="utf-8") if errors.exists() else "")
            self.assertTrue(any(p["argv"][1:2] == ["dispatch"] for p in procs), procs)
            end = time.monotonic() + 5
            while any(same_process(p["pid"], p["start"]) for p in procs) and time.monotonic() < end:
                time.sleep(0.05)
            self.assertEqual([p["argv"] for p in procs if same_process(p["pid"], p["start"])], [])
        finally:
            for p in procs:
                if same_process(p["pid"], p["start"]):
                    os.kill(p["pid"], signal.SIGKILL)


if __name__ == "__main__":
    unittest.main()
