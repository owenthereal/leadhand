"""A Codex-route dispatch's result comes only from the runner's result.json (final review I1).

The proxy (a small Claude agent) only runs `leadhand dispatch`; whatever text it returns is
never taken as the worker's result. While the runner runs, recording the proxy's reply
changes nothing; with no runner and no result.json, the dispatch is finalized (cancelled)
first; a `--stopped` record stops the runner first."""
import json
import time
import unittest

from fakeworld import FakeWorld, card
from leadhand import engine, procs


def wait_for(cond, timeout=30.0, step=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = cond()
        if value:
            return value
        time.sleep(step)
    raise AssertionError(f"timed out after {timeout}s waiting for {cond}")


def runner_codex(wd):
    return wait_for(lambda: (json.loads((wd / "runner.json").read_text())
                             if (wd / "runner.json").exists() else {}).get("codex"))


class CodexResultFromRunner(unittest.TestCase):
    def test_early_still_running_reply_waits_and_starts_no_second_implementer(self):
        w = FakeWorld(self, [card(1)], installed=("claude", "codex"))
        w.start()
        a = w.next()
        self.assertEqual(a["agent"], "leadhand:codex")
        worker = w.launch(a)
        proxy = w.start_proxy(a, {"sleep": 4})
        wd = w.run_dir / "workers" / a["dispatch_id"]
        runner = runner_codex(wd)
        # The proxy gives up early and replies with the dispatcher's "still running" line.
        out = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                            reply=f"still running: run the same command again (dispatch {a['dispatch_id']})")
        self.assertEqual(out["action"], "wait", out)
        self.assertEqual(out["dispatch_id"], a["dispatch_id"])
        self.assertEqual(w.events("result_recorded"), [])
        self.assertTrue(procs.same_process(runner["pid"], runner["start"]))
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["dispatch_id"]), ("wait", a["dispatch_id"]))
        # The timer reports the runner's result once result.json is there; `next` records it.
        self.assertEqual(engine.timer(w.run_dir, a["dispatch_id"], interval=0.1), "result")
        proxy.communicate(timeout=60)
        review = w.next()
        (result,) = w.events("result_recorded")
        self.assertEqual((result["dispatch_id"], result["status"]), (a["dispatch_id"], "READY"))
        self.assertEqual((review["action"], review["kind"]), ("dispatch", "review"))
        implements = [e["dispatch_id"] for e in w.events("dispatch_requested")
                      if e["dispatch_kind"] == "implement"]
        self.assertEqual(implements, [a["dispatch_id"]])

    def test_invented_verdict_without_a_worker_is_not_accepted(self):
        cfg = '[roles.reviewer]\ncodex = "gpt-6.1-terra medium"\nprefer = "codex"\n'
        w = FakeWorld(self, [card(1, route="claude sonnet low", risk="medium")], installed=("claude", "codex"),
                      config=cfg)
        w.start()
        w.act(w.next())                                  # the Claude implementer
        r = w.next()
        self.assertEqual((r["action"], r["kind"], r["agent"]), ("dispatch", "review", "leadhand:codex"))
        worker = w.launch(r)
        # The proxy never ran the command and invents a clean verdict.
        ev = engine.record(w.run_dir, "result", dispatch_id=r["dispatch_id"], worker_id=worker,
                           reply="Verdict: clean\nReport: none")
        self.assertNotEqual(ev.get("verdict"), "clean", ev)
        wd = w.run_dir / "workers" / r["dispatch_id"]
        self.assertEqual(json.loads((wd / "result.json").read_text())["exit_reason"], "cancelled")
        nxt = w.next()
        self.assertEqual(w.events("accepted"), [])
        self.assertEqual(w.events("verdict_recorded"), [])
        self.assertEqual((nxt["action"], nxt["kind"]), ("dispatch", "review"))
        self.assertNotEqual(nxt["dispatch_id"], r["dispatch_id"])

    def test_skipping_a_codex_card_stops_its_runner(self):
        w = FakeWorld(self, [card(1)], installed=("claude", "codex"))
        w.start()
        a = w.next()
        worker = w.launch(a)
        proxy = w.start_proxy(a, {"sleep": 120})
        wd = w.run_dir / "workers" / a["dispatch_id"]
        runner = runner_codex(wd)
        # The run skill's skip path: stop the proxy, record it stopped, park the card.
        ev = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                           reply="Status: SPENT (time limit)", stopped=True)
        self.assertEqual(ev["kind"], "result_recorded")
        self.assertTrue(ev["stopped"])
        self.assertFalse(procs.same_process(runner["pid"], runner["start"]))
        self.assertEqual(json.loads((wd / "result.json").read_text())["exit_reason"], "cancelled")
        engine.record(w.run_dir, "park", card=1, reason="operator: skip")
        proxy.communicate(timeout=60)
        self.assertEqual(w.next()["action"], "done")


if __name__ == "__main__":
    unittest.main()
