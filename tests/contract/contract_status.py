#!/usr/bin/env python3
"""What `status [RUN_DIR]` prints about a run directory, and the status.json and meter-start.json
that the engine's checkpoints write.

Converted from tests/test_status.py. Run directories are built with RunDir; the checkpoint
files come from FakeSupervisor runs, since only `next` and `record` write them. `status` with
no argument is reference-only (deliberate change 3).
"""
import os
import shlex
import shutil
import threading
import traceback
import unittest

from harness import BIN, hermetic_env, read_json, reference_only, run_cli, temp_root
from rundir import RunDir, write_rate_limit
from supervisor import FakeSupervisor, card

KEYS = {"run_id", "deck", "state", "headline", "activity", "progress", "attention", "cards", "usage",
        "updated_at"}
METER_SCAN_FILES = 20      # the newest rollout files the Codex meter reads


def status(d: RunDir, env: dict | None = None) -> list[str]:
    """`status <run dir>`'s lines; it must succeed."""
    r = run_cli("status", d.root, env=env or d.env, cwd=d.root.parent).ok()
    return r.stdout.splitlines()


def usage_line(lines: list[str]) -> str:
    return next(line for line in lines if line.startswith("Usage: "))


def unpark_line(n: int, root, reason: str) -> str:
    return (f"Card {n} parked: {reason} — next: {shlex.quote(str(BIN))} unpark "
            f"{shlex.quote(str(root))} --card {n} --note '…'")


class Status(unittest.TestCase):
    def test_status_headline_while_implementing(self):
        w = RunDir(self, cards=5)
        w.accept(1)
        w.accept(2)
        w.dispatch(3, effort="high")
        lines = status(w)
        self.assertEqual(lines[0], "Running · Implementing card 3 (attempt 1, codex gpt-6.1-terra high)"
                                   " · 2 of 5 accepted")
        self.assertIn("Card 3: Card number 3 · started · attempt 1 · codex gpt-6.1-terra high", lines)

    def test_status_headline_while_reviewing(self):
        w = RunDir(self, cards=5)
        w.accept(1)
        w.accept(2)
        w.dispatch(3, kind="review", role="reviewer", vendor="claude", model="opus", effort="high")
        self.assertEqual(status(w)[0], "Running · Reviewing card 3 · 2 of 5 accepted")

    def test_status_attention_lists_parked_cards_and_decisions(self):
        w = RunDir(self, cards=5)
        w.accept(1)
        w.ev("card_cut", card=5, branch="b5", base="b")
        w.ev("parked", card=5, reason="needs a design decision")
        w.dispatch(2)
        lines = status(w)
        self.assertIn(" ", str(w.root))
        self.assertEqual(lines[0], "Needs your decision on card 5; independent work continues")
        self.assertEqual(lines[1], unpark_line(5, w.root, "needs a design decision"))
        self.assertTrue(any(l.startswith("Card 5") or l.startswith("5 ") or " 5 " in l for l in lines[2:]))
        self.assertIn("Usage", lines[-1])

    def test_ended_run_states(self):
        w = RunDir(self, cards=1)
        w.accept(1)
        w.ev("outcome", outcome="complete", qualifier="", reasons=[])
        self.assertEqual(status(w)[0], "Complete · 1 of 1 accepted")
        w2 = RunDir(self, cards=2)
        w2.ev("outcome", outcome="blocked", qualifier="paused", reasons=["blocked: paused"])
        self.assertEqual(status(w2)[0], "Blocked (paused) · 0 of 2 accepted")

    def test_status_cli_without_status_json(self):
        w = RunDir(self, cards=3)
        w.dispatch(1)
        self.assertFalse(w.paths.status.exists())
        r = run_cli("status", w.root, env=w.env, cwd=w.root.parent)
        self.assertEqual(r.code, 0, r)
        self.assertTrue(r.stdout.startswith("Running · Implementing card 1"), r)

    def test_corrupt_event_line_is_an_attention_line_not_a_failure(self):
        w = RunDir(self, cards=2)
        w.accept(1)
        w.ev("card_cut", card=2, branch="b2", base="b")
        lines = w.paths.events.read_bytes().split(b"\n")
        lines.insert(3, b"{not json}")
        w.paths.events.write_bytes(b"\n".join(lines))
        r = run_cli("status", w.root, env=w.env, cwd=w.root.parent)
        self.assertEqual(r.code, 0, r)
        out = r.stdout.splitlines()
        self.assertTrue(out[0].endswith(" · 1 of 2 accepted"), r)
        self.assertIn("event log has an unreadable line at seq 4/line 4", out)

    def test_partial_last_line_is_ignored(self):
        w = RunDir(self, cards=1)
        with open(w.paths.events, "ab") as f:
            f.write(b'{"seq": 9, "kind": "acc')
        lines = status(w)
        # no attention lines: the card rows follow the headline
        self.assertTrue(lines[0].startswith("Running · "), lines)
        self.assertTrue(lines[1].startswith("Card 1: "), lines)

    def test_usage_comes_from_charges_and_excludes_nothing_by_role(self):
        w = RunDir(self, cards=1)
        w.charge("c1", "implementer", tokens=(1000, 100))
        w.charge("c2", "proxy", tokens=(10, 1))
        w.charge("c3", "supervisor", known=False)
        d = w.dispatch(1)
        w.attempt(d, 1, usage_known=False)
        unknown = usage_line(status(w)).split(" attempts with unknown usage")[0].rsplit(" · ", 1)[1]
        self.assertGreaterEqual(int(unknown), 1)
        run_cli("report", w.root, env=w.env, cwd=w.root.parent).ok()
        usage = w.paths.report.read_text(encoding="utf-8").split("By role:\n")[1].split("\n\n")[0]
        roles = {line[2:].split(":")[0] for line in usage.splitlines()}
        self.assertEqual(roles, {"implementer", "proxy", "supervisor"})

    def test_codex_meter_reads_latest_record(self):
        w = RunDir(self, cards=1)
        self.assertIn("· Codex meter unknown ·", usage_line(status(w)))
        w.rate_limit(12.5, ts="2026-01-02T03:04:05.000Z")
        w.rate_limit(15.0, ts="2026-01-02T05:00:00.000Z")
        self.assertIn("· Codex meter 15.0% as of 2026-01-02T05:00:00.000Z ·", usage_line(status(w)))
        missing = dict(w.env, CODEX_HOME=str(w.root.parent / "missing"))
        self.assertIn("· Codex meter unknown ·", usage_line(status(w, env=missing)))


class StatusWithoutRunDir(unittest.TestCase):
    @reference_only
    def test_no_run_dir_picks_latest_unfinished_run(self):
        """Deliberate change 3: Rust picks the run from the current directory instead."""
        w = RunDir(self, cards=1)
        docs = w.root.parent / "docs dir"          # hermetic_env's docs_dir
        runs = docs / "runs"
        runs.mkdir(parents=True)
        done = RunDir(self, cards=1)
        done.ev("outcome", outcome="complete", qualifier="", reasons=[])
        shutil.copytree(done.root, runs / "2026-01-01-done")
        shutil.copytree(w.root, runs / "2026-01-02-live")
        r = run_cli("status", env=w.env, cwd=w.root.parent)
        self.assertEqual(r.code, 0, r)
        self.assertEqual(r.stdout.splitlines()[0], "Running · Waiting for the supervisor · 0 of 1 accepted", r)

        tmp = temp_root(self, "lh status ")
        r = run_cli("status", env=hermetic_env(self, tmp), cwd=tmp)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stderr.strip(), f"error: no unfinished run under {tmp / 'docs dir' / 'runs'}", r)


class FixRound(unittest.TestCase):
    def test_attention_lists_violations_with_preserved_ref_and_next_action(self):
        w = RunDir(self, cards=2)
        w.ev("violation", stage="found", by="check", dispatch_id="c1-a1-review-1", card=1,
             dispatch_kind="review", n=1, ref="refs/leadhand/abc123/violations/1", preserved={})
        w.ev("violation", stage="found", by="implementer", n=1, ref="refs/leadhand/abc123/leftovers/1",
             after_dispatch="c2-a1-implement-1", card=2)
        att = status(w)
        self.assertTrue(any("review worker on card 1" in a and "refs/leadhand/abc123/violations/1" in a
                            and "next: inspect refs/leadhand/abc123/violations/1" in a for a in att), att)
        self.assertTrue(any("refs/leadhand/abc123/leftovers/1" in a for a in att), att)

    def test_meter_scan_is_capped_to_newest_files(self):
        w = RunDir(self, cards=1)
        old = write_rate_limit(w.sessions, 5.0, name="rollout-0-old.jsonl")
        os.utime(old, (1, 1))
        self.assertIn("· Codex meter 5.0% as of ", usage_line(status(w)))
        for i in range(METER_SCAN_FILES):
            d = w.sessions / "x"
            d.mkdir(exist_ok=True)
            (d / f"rollout-{i}.jsonl").write_text("{}\n")
        self.assertIn("· Codex meter unknown ·", usage_line(status(w)))

    def test_unreadable_log_is_an_attention_line(self):
        w = RunDir(self, cards=1)
        w.paths.events.chmod(0)
        self.addCleanup(w.paths.events.chmod, 0o644)
        att = status(w)
        self.assertTrue(any(a.startswith("event log could not be read") for a in att), att)


class Checkpoints(unittest.TestCase):
    """status.json and meter-start.json, written at the engine's checkpoints."""

    def test_checkpoint_writes_status_json(self):
        w = FakeSupervisor(self, [card(1), card(2)])
        w.start()
        w.next()
        s = read_json(w.paths.status)
        self.assertEqual(set(s), KEYS)
        self.assertEqual(s["run_id"], w.meta()["run_id"])
        self.assertEqual(s["deck"], "Demo deck")
        self.assertEqual(s["state"], "running")
        self.assertEqual(s["progress"], {"accepted": 0, "total": 2})
        self.assertEqual(set(s["usage"]), {"estimated_usd", "unknown_attempts", "by_role", "codex_meter"})
        self.assertEqual(set(s["cards"][1]), {"number", "title", "state", "attempt", "route", "note"})

    def test_engine_checkpoints_write_status_json_and_survive_status_errors(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.next()
        self.assertEqual(read_json(w.paths.status)["progress"], {"accepted": 0, "total": 1})
        w.paths.status.unlink()
        w.paths.status.mkdir()          # status.json can no longer be written
        r = w.cli("next", w.run_dir)
        self.assertEqual(r.code, 0, r)
        self.assertIn("action", r.json())
        self.assertIn("could not write status.json: ", r.stderr)

    def test_meter_before_is_taken_once_even_when_null(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.next()
        meter_start = w.run_dir / "meter-start.json"
        self.assertEqual(read_json(meter_start), {"percent": None, "observed_at": None})
        write_rate_limit(w.codex_home / "sessions", 40.0)
        w.next()
        self.assertEqual(read_json(meter_start), {"percent": None, "observed_at": None})

    def test_meter_is_not_scanned_at_every_checkpoint(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        sessions = w.codex_home / "sessions"
        write_rate_limit(sessions, 10.0, ts="2026-01-02T03:04:05.000Z")
        w.start()
        a = w.next()
        meter = lambda: read_json(w.paths.status)["usage"]["codex_meter"]["percent"]
        self.assertEqual(meter(), 10.0)
        write_rate_limit(sessions, 11.0, ts="2026-01-02T05:00:00.000Z")
        w.act(a)                        # `record started` and `record result` are checkpoints too
        self.assertEqual(meter(), 10.0)
        actions = [a, *w.run()]
        self.assertEqual(actions[-1]["action"], "done", actions[-1])
        self.assertEqual(meter(), 11.0)  # read again once the run has ended


class StatusDuringRun(unittest.TestCase):
    FINAL = "Complete · 3 of 3 accepted"
    TORN = ("unreadable line", "could not be read", "is missing or unreadable", "status rebuilt from status.json")

    def test_status_polled_during_a_run_never_fails(self):
        """Polling `status` during a run never exits non-zero and never sees a torn line."""
        w = FakeSupervisor(self, [card(1), card(2), card(3)], gates=True)
        run_dir = w.start()
        stop = threading.Event()
        cond = threading.Condition()
        seen = {"polls": 0, "running": 0, "dead": False}
        failures: list[str] = []

        def poll():
            try:
                while not stop.is_set():
                    r = w.cli("status", run_dir)
                    first = r.stdout.split("\n", 1)[0]
                    if r.code != 0:
                        failures.append(f"status exited {r.code}\n{r}")
                    elif not (first.startswith("Running") or first == self.FINAL):
                        failures.append(f"unexpected headline {first!r}\n{r}")
                    elif any(m in r.stdout for m in self.TORN):
                        failures.append(f"status saw a torn or unreadable file\n{r}")
                    with cond:
                        seen["polls"] += 1
                        seen["running"] += first.startswith("Running")
                        cond.notify_all()
            except BaseException:
                failures.append(traceback.format_exc())
            finally:
                with cond:
                    seen["dead"] = True
                    cond.notify_all()

        thread = threading.Thread(target=poll, name="status poller", daemon=True)

        def stop_poller():
            stop.set()
            thread.join(timeout=150)    # one status call is bounded by run_cli's timeout

        thread.start()
        self.addCleanup(stop_poller)

        # Between engine steps, wait (bounded) for at least one more completed poll, so the
        # polls spread over the whole run rather than bunching where the scheduler puts them.
        actions = []
        for _ in range(80):
            with cond:
                target = seen["polls"] + 1
                self.assertTrue(cond.wait_for(lambda: seen["polls"] >= target or seen["dead"], timeout=60),
                                "the status poller made no progress in 60 s")
                self.assertFalse(seen["dead"], "the status poller died:\n" + "\n".join(failures))
            a = w.next()
            actions.append(a)
            if a["action"] == "done":
                break
            w.act(a)
        else:
            self.fail(f"the run did not finish in 80 actions: {actions[-3:]}")
        with cond:
            running_polls = seen["running"]
        stop_poller()
        self.assertFalse(thread.is_alive(), "the status poller did not stop")
        self.assertEqual(failures, [], "\n\n".join(failures))

        self.assertEqual(a.get("outcome"), "complete", a)
        self.assertEqual(w.cli("status", run_dir).ok().stdout.splitlines()[0], self.FINAL)
        # Every poll that finished before the last `next` saw the run in progress; there was
        # at least one per engine step before it.
        self.assertGreaterEqual(running_polls, len(actions) - 1)
        self.assertGreaterEqual(len(actions), 9, actions)


if __name__ == "__main__":
    unittest.main()
