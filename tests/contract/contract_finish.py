#!/usr/bin/env python3
"""The end of a run: the commit-bound gates (final review, a fix wave's implementer and review,
evidence, security, verify), the outcome, the report and the `<docs_dir>/runs.md` line, as
`start`, `next`, `record`, `unpark`, `run`, `pause`, `cancel`, `timer` and `check` show them.

Converted from tests/test_engine_finish.py and tests/test_final_review_minors.py.
FakeSupervisor(..., gates=True) plays the run skill through the real end of run. Where the
Python test called an engine function, this calls the command that does it; an EngineError is
exit code 1 with `error: ...` on stderr. Where the Python test appended `resumed` itself, the
run is relaunched with `run`, which records it. A crash inside a Git call is injected by making
that call fail (a held ref lock); a crash right after `git worktree add` is set up as the state
it leaves (the `base_worktree_added` event and the worktree, without its marker).

Gate outcomes are judged from Git and the filesystem: the run branch's tip and tree, the
refs under refs/leadhand/<run id>/, the evidence base worktree at
`<repo>/.worktrees/<workspace name>--base` with its `leadhand-run` marker (R60), and the
runs.md file. The binary's events are asserted beside that, never as the only evidence.

The branch release (deliberate change 2, ruling S1-R15): a run that ends complete,
incomplete or failed may record more events after `outcome`, detach its worktree and add a
branch line to the report. So these tests check the event kinds and order they care about,
never a complete list, and the report's specific lines.
"""
import fcntl
import json
import os
import re
import shlex
import shutil
import subprocess
import threading
import time
import unittest
from pathlib import Path

from contract_cards import codex_world, relaunch_env
from gitfixture import commit, run_git, write
from harness import BIN, FAKES, is_bin_command, run_cli
from rundir import append_event
from supervisor import FakeSupervisor, card

RUN = "leadhand/demo"
FINAL = "leadhand/demo--card-final"
MARKER = "leadhand-run"          # in the evidence base worktree's admin folder: the run that added it (R60)


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


def base_wt(w) -> Path:
    """The evidence base worktree: `<repo>/.worktrees/<workspace name>--base`."""
    ws = w.workspace()
    return ws.parent / f"{ws.name}--base"


def marker(path: Path) -> Path:
    return Path(run_git(path, "rev-parse", "--absolute-git-dir")) / MARKER


def on_run_branch(w, path: str) -> bool:
    return path in run_git(w.repo, "ls-tree", "-r", "--name-only", f"refs/heads/{RUN}").splitlines()


def found_violations(w):
    return [e for e in w.events("violation") if e.get("stage") == "found" and (e.get("by") or "check") != "implementer"]


def drive_to_run(w, pred):
    """Act on every action until a run-level dispatch matching `pred`; return it unacted."""
    for _ in range(80):
        a = w.next()
        if a["action"] == "dispatch" and a["card"] is None and pred(a):
            return a
        w.act(a)
    raise AssertionError("no matching run-level dispatch")


def finish(w, a, worker):
    """The launched worker `worker` does dispatch `a`'s work; its result is recorded through the reply file."""
    reply = w.perform(a, worker)
    reply_file = Path(a["reply_file"])
    reply_file.parent.mkdir(parents=True, exist_ok=True)
    reply_file.write_text(reply, encoding="utf-8")
    w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
          "--reply-file", reply_file).ok()


def drive_until_refused(w, limit=80):
    """Act on every action until `next` fails; return that failing result."""
    for _ in range(limit):
        r = w.cli("next", w.run_dir)
        if r.code != 0:
            return r
        a = r.json()
        if a["action"] == "done":
            raise AssertionError(f"the run ended without the refusal: {a}")
        w.act(a)
    raise AssertionError(f"`next` was never refused in {limit} actions")


def names_bin_before(text: str, rest: str) -> bool:
    """True when `rest` occurs in `text` right after one shell word naming the binary under
    test (compared by file, S1-R11)."""
    start = 0
    while (j := text.find(rest, start)) != -1:
        if any(is_bin_command(text[i:j], "", "") for i in range(j - 1, max(-1, j - 4096), -1)):
            return True
        start = j + 1
    return False


class Complete(unittest.TestCase):
    def test_complete_run_writes_outcome_and_report(self):
        w = FakeSupervisor(self, [card(1), card(2, depends=[1])], gates=True)
        w.start()
        done = w.run()[-1]

        head = w.branch_tip(RUN)
        for n in (1, 2):
            self.assertTrue(on_run_branch(w, f"src/card{n}/c{n}-a1-implement-1.txt"), n)
        self.assertEqual(gates(w), [("final_review", head, True), ("evidence", head, True),
                                    ("verify", head, True)])
        self.assertEqual(w.run_dispatch_ids(), ["run-final_review-1", "run-evidence-1", "run-verify-1"])
        roles = [a["role"] for a in w.run_started]
        self.assertEqual(roles, ["final_reviewer", "verifier", "verifier"])
        self.assertTrue(all(a["card"] is None for a in w.run_started))

        # the briefs name the run branch, the base and the accepted cards
        review_brief = Path(w.run_started[0]["brief"]).read_text(encoding="utf-8")
        self.assertIn(f"{w.base}..{head}", review_brief)
        evidence_brief = Path(w.run_started[1]["brief"]).read_text(encoding="utf-8")
        bw = base_wt(w)
        self.assertEqual(bw.resolve(), (w.repo / ".worktrees" / f"{w.run_dir.name}--base").resolve())
        self.assertIn(str(bw), evidence_brief)
        # the evidence base worktree, read from Git: detached at the base, marked as this run's (R60)
        self.assertEqual(run_git(bw, "rev-parse", "HEAD"), w.base)
        self.assertEqual(run_git(bw, "symbolic-ref", "--quiet", "HEAD", check=False), "")
        self.assertEqual(marker(bw).read_text(encoding="utf-8").strip(), w.meta()["run_id"])
        for a in w.run_started:
            self.assertIn(f"Branch: {RUN}", Path(a["brief"]).read_text(encoding="utf-8"))
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
        lines = report.read_text(encoding="utf-8").splitlines()
        self.assertIn("Outcome: complete", lines)
        self.assertIn(f"- final_review: passed on commit {head[:12]}", lines)
        self.assertFalse(w.paths.handoff.exists())
        self.assertEqual((done["action"], done["outcome"], done["report"]), ("done", "complete", str(report)))
        self.assertEqual(len(runs_lines(w)), 1)

        # repeating next changes nothing
        self.assertEqual(w.next()["outcome"], "complete")
        self.assertEqual(len(w.events("outcome")), 1)
        self.assertEqual(len(w.events("report_written")), 1)
        self.assertEqual(w.branch_tip(RUN), head)

    def test_runs_md_line_appended_once(self):
        w = FakeSupervisor(self, [card(1), card(2)], title="Widgets | phase 1", gates=True)
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


def to_last_step(w):
    """Act on every action up to and including the run's verify gate: the next `next` ends the run."""
    a = drive_to_run(w, lambda a: a["kind"] == "verify")
    w.act(a)


class SharedRunIndex(unittest.TestCase):
    def test_concurrent_runs_never_lose_a_runs_md_line(self):
        # Two runs of two repositories share one docs_dir and end at once. runs.md is
        # read-modified-replaced under an exclusive flock on <docs_dir>/runs.md.lock; the test
        # holds that lock while both runs end, so both wait for it and then read the same file.
        w1 = FakeSupervisor(self, [card(1)], gates=True)
        w2 = FakeSupervisor(self, [card(1)], gates=True)
        w2.config_file.write_text(f'docs_dir = "{w1.docs}"\n', encoding="utf-8")
        w2.docs = w1.docs
        for w in (w1, w2):
            w.start()
            to_last_step(w)
        self.assertNotEqual(w1.run_dir, w2.run_dir)

        lock = w1.docs / "runs.md.lock"
        fd = os.open(lock, os.O_RDWR | os.O_CREAT, 0o600)
        results: dict[int, object] = {}

        def end(i, w):
            try:
                results[i] = w.cli("next", w.run_dir)
            except BaseException as e:       # a timeout's AssertionError, reported below
                results[i] = e

        threads = [threading.Thread(target=end, args=(i, w), daemon=True) for i, w in enumerate((w1, w2))]
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            for t in threads:
                t.start()
            deadline = time.monotonic() + 60
            while not all(w.events("outcome") for w in (w1, w2)):
                self.assertLess(time.monotonic(), deadline, "the runs did not reach their outcome in 60 s")
                time.sleep(0.05)
            self.assertTrue(all(t.is_alive() for t in threads), results)
            self.assertEqual(runs_lines(w1) + runs_lines(w2), [])
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)
            for t in threads:
                t.join(timeout=150)
        for i, w in enumerate((w1, w2)):
            r = results[i]
            self.assertNotIsInstance(r, BaseException, r)
            self.assertEqual(r.ok().json()["outcome"], "complete", r)
        text = (w1.docs / "runs.md").read_text(encoding="utf-8")
        for w in (w1, w2):
            self.assertEqual(sum(1 for ln in text.splitlines() if ln.endswith(f"| {w.run_dir} |")), 1, text)


class CwdSupervisor(FakeSupervisor):
    """A FakeSupervisor whose engine steps run in `cwd` (the world's temp root by default)."""

    def __init__(self, *args, **kw):
        super().__init__(*args, **kw)
        self.cwd = self.tmp

    def _cli(self, *args):
        return run_cli(*args, env=self._env(), cwd=self.cwd)


class RelativeDocsDir(unittest.TestCase):
    def test_relative_docs_dir_resolves_against_the_repository_from_any_cwd(self):
        # `start --repo` from another directory and the wrap-up run from the workspace must
        # use one docs dir: the repository-relative one.
        w = CwdSupervisor(self, [card(1)], gates=True)
        w.config_file.write_text('docs_dir = "../notes/{repo}"\n', encoding="utf-8")
        w.docs = (w.repo.parent / "notes" / w.repo.name).resolve()
        elsewhere = w.tmp / "a" / "b"
        elsewhere.mkdir(parents=True)
        w.cwd = elsewhere
        w.start()
        self.assertEqual(w.run_dir.parent.parent.resolve(), w.docs)
        w.cwd = w.workspace()
        w.run()
        self.assertEqual(len(runs_lines(w)), 1, sorted(str(p) for p in w.tmp.rglob("runs.md")))
        self.assertEqual([p.resolve() for p in w.tmp.rglob("runs.md")], [w.docs / "runs.md"])


class FixWaves(unittest.TestCase):
    def test_final_review_defects_get_a_fix_wave_then_all_gates_rerun(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "final_review", {"verdict": "defects"})
        w.run()

        self.assertEqual(w.run_dispatch_ids(), [
            "run-final_review-1", "run-w1-final_fix-1", "run-w1-final_review-1",
            "run-final_review-2", "run-evidence-1", "run-verify-1"])
        fix = w.run_started[1]
        self.assertEqual((fix["kind"], fix["role"], fix["wave"]), ("final_fix", "implementer", 1))
        fix_brief = Path(fix["brief"]).read_text(encoding="utf-8")
        self.assertIn('"final_review" failed', fix_brief)
        self.assertIn(FINAL, fix_brief)
        (accepted_fix,) = [e for e in w.events("accepted") if e["card"] == "final"]
        head = w.branch_tip(RUN)
        self.assertEqual(accepted_fix["sha"], head)
        old = w.events("gate_result")[0]["examined_sha"]
        self.assertNotEqual(old, head)
        run_git(w.repo, "merge-base", "--is-ancestor", old, head)
        self.assertEqual(gates(w), [("final_review", old, False), ("final_review", head, True),
                                    ("evidence", head, True), ("verify", head, True)])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/final/run-w1-final_fix-1.txt"),
                         "run-w1-final_fix-1")
        self.assertEqual(outcome(w)["outcome"], "complete")
        self.assertIn("| complete |", runs_lines(w)[0])

    def test_verify_fix_after_security_pass_reruns_security_and_evidence(self):
        w = FakeSupervisor(self, [card(1, risk="medium")], gates=True)
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
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        defects = {"verdict": "defects"}
        w.script(None, "final_review", defects, defects, defects, defects)
        w.run()

        fixes = [i for i in w.run_dispatch_ids() if "final_fix" in i]
        self.assertEqual(fixes, ["run-w1-final_fix-1", "run-w2-final_fix-1"])     # two waves at most
        head = w.branch_tip(RUN)
        for wave in (1, 2):
            self.assertTrue(on_run_branch(w, f"src/final/run-w{wave}-final_fix-1.txt"), wave)
        self.assertEqual(gate_names(w), ["final_review", "final_review", "final_review", "evidence", "verify"])
        self.assertEqual(gates(w)[-3:], [("final_review", head, False), ("evidence", head, True),
                                         ("verify", head, True)])
        out = outcome(w)
        self.assertEqual(out["outcome"], "incomplete")
        self.assertIn("unresolved review findings", out["reasons"])
        self.assertIn("| incomplete |", runs_lines(w)[0])

    def test_rejected_fix_is_not_accepted_and_spends_its_wave(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
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
        (wave1,) = [r for r in kept if r.endswith("/final/1")]
        self.assertEqual(run_git(w.repo, "show", f"{wave1}:src/final/run-w1-final_fix-1.txt"), "run-w1-final_fix-1")
        self.assertEqual(outcome(w)["outcome"], "complete")


class Evidence(unittest.TestCase):
    def evidence_fails(self, config=""):
        w = FakeSupervisor(self, [card(1)], config=config, gates=True)
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
        handoff = w.paths.handoff.read_text(encoding="utf-8")
        self.assertIn("decision needed", handoff)
        self.assertIn("evidence", handoff)
        self.assertIn("leadhand run", handoff)
        self.assertEqual(len(w.events("report_written")), 1)
        self.assertEqual(runs_lines(w)[0].split("|")[3].strip(), "blocked (decision needed)")
        # a blocked run keeps its branch checked out: it resumes there
        self.assertEqual(run_git(w.workspace(), "symbolic-ref", "--short", "HEAD"), RUN)

    def test_evidence_record_gap_makes_incomplete_with_hold(self):
        w, done = self.evidence_fails()
        self.assertEqual(done["outcome"], "incomplete")
        self.assertTrue(any(r.startswith("hold: evidence") for r in done["reasons"]), done["reasons"])
        head = w.branch_tip(RUN)
        self.assertEqual(gates(w)[-2:], [("evidence", head, False), ("verify", head, True)])
        self.assertFalse(w.paths.handoff.exists())
        self.assertIn("| incomplete |", runs_lines(w)[0])

    def test_resumed_blocked_run_keeps_gates_that_passed_on_the_same_head(self):
        w, done = self.evidence_fails('[run]\nevidence = "block"\n')
        before = len(w.events("gate_result"))
        head = w.branch_tip(RUN)
        w.cli("run", w.run_dir, env=relaunch_env()).ok()       # the operator decided the gap is fine to re-check
        self.assertTrue(w.events("resumed"))
        w.script(None, "evidence", {"result": "pass"})
        done = w.run()[-1]
        later = [e["gate"] for e in w.events("gate_result")[before:]]
        self.assertEqual(later, ["evidence", "verify"])          # final_review stayed current
        self.assertEqual(done["outcome"], "complete")
        self.assertEqual(w.branch_tip(RUN), head)
        self.assertFalse(w.paths.handoff.exists())          # the blocked run's handoff is gone
        self.assertEqual(len(runs_lines(w)), 1)
        self.assertIn("| complete |", runs_lines(w)[0])


class Security(unittest.TestCase):
    def security_ran(self, deck_card, files=None):
        w = FakeSupervisor(self, [deck_card], gates=True)
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
                self.assertTrue(on_run_branch(w, path), path)

        # paths that are not sensitive
        for path in ("src/widget.py", "docs/authoring.md"):
            with self.subTest(path=path):
                w, ran = self.security_ran(card(1), files={path: "x\n"})
                self.assertEqual(ran, [], path)
                self.assertNotIn("security", gate_names(w), path)
                self.assertTrue(on_run_branch(w, path), path)


class Verification(unittest.TestCase):
    def test_verification_failure_after_last_wave_is_failed(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
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
        self.assertIn("Outcome: failed", w.paths.report.read_text(encoding="utf-8").splitlines())
        self.assertFalse(w.paths.handoff.exists())
        self.assertIn("| failed |", runs_lines(w)[0])


class Unpark(unittest.TestCase):
    def test_unparked_card_waits_for_an_open_gate_then_gates_rerun(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision: storage format"
        gate = drive_to_run(w, lambda a: True)
        self.assertEqual(gate["dispatch_id"], "run-final_review-1")
        worker = w.launch(gate)
        w.cli("unpark", w.run_dir, "--card", "1", "--note", "Use JSON.").ok()
        waiting = w.next()
        self.assertEqual((waiting["action"], waiting["dispatch_id"]), ("wait", "run-final_review-1"))
        finish(w, gate, worker)
        del w.parks[1]
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", (done["reasons"], w.events("accepted")))
        head = w.branch_tip(RUN)
        self.assertEqual([e["card"] for e in w.events("accepted")], [2, 1])
        for n in (1, 2):
            self.assertTrue(on_run_branch(w, f"src/card{n}/c{n}-a1-implement-1.txt"), n)
        self.assertEqual(gates(w)[-3:], [("final_review", head, True), ("evidence", head, True),
                                         ("verify", head, True)])


class Guards(unittest.TestCase):
    def test_final_fix_moving_the_run_branch_is_voided_and_restored(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
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
        for n in (1, 2):
            self.assertTrue(on_run_branch(w, f"src/card{n}/c{n}-a1-implement-1.txt"), n)

    def test_accepted_work_missing_from_the_run_branch_fails_the_run(self):
        # Card 1 is accepted; while card 2 waits on the supervisor and no worker is out, the run
        # branch is moved back to the base. The run ends failed, naming the lost accepted commit.
        w = FakeSupervisor(self, [card(1), card(2, depends=[1])], gates=True)
        w.start()
        w.script(2, "implement", {"status": "QUESTION"})
        for _ in range(40):
            a = w.next()
            if a["action"] == "answer":
                break
            w.act(a)
        self.assertEqual((a["action"], a["card"]), ("answer", 2))
        (accepted,) = w.events("accepted")
        self.assertEqual(w.branch_tip(RUN), accepted["sha"])
        run_git(w.repo, "update-ref", f"refs/heads/{RUN}", w.base)
        w.cli("record", w.run_dir, "park", "--card", "2", "--reason", "operator: skip").ok()
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "failed")
        # Python's `_end` alone words this "accepted work missing from the run branch"; through the
        # CLI the restart check finds it first and names the accepted commit the branch lost.
        self.assertTrue(any("could not be reconciled" in r and RUN in r and accepted["sha"][:12] in r
                            for r in done["reasons"]), done["reasons"])
        self.assertEqual(w.branch_tip(RUN), w.base)
        self.assertIn("| failed |", runs_lines(w)[0])

    def test_evidence_base_worktree_is_reset_and_guarded(self):
        def tamper(world, ws, a):
            bw = base_wt(world)
            write(bw, {"tests/leftover.txt": "x\n"})
            commit(bw, {"src/moved.txt": "moved\n"}, "verifier moved the base")
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": tamper}, {})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        (v,) = found_violations(w)
        self.assertEqual((v["dispatch_id"], v["dispatch_kind"]), ("run-evidence-1", "evidence"))
        rerun = next(a for a in w.run_started if a["dispatch_id"] == "run-evidence-2")
        self.assertIsNone(rerun["resume"])
        bw = base_wt(w)
        self.assertEqual(run_git(bw, "rev-parse", "HEAD"), w.base)
        self.assertEqual(run_git(bw, "status", "--porcelain", "--ignored"), "")
        self.assertEqual(marker(bw).read_text(encoding="utf-8").strip(), w.meta()["run_id"])
        self.assertEqual([e["dispatch_id"] for e in w.events("gate_result") if e["gate"] == "evidence"],
                         ["run-evidence-2"])

    def test_evidence_base_edit_hidden_behind_an_index_flag_is_violation(self):
        def tamper(world, ws, a):
            bw = base_wt(world)
            run_git(bw, "update-index", "--assume-unchanged", "README.md")
            write(bw, {"README.md": "edited in the base, hidden\n"})
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": tamper}, {})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        (v,) = found_violations(w)
        self.assertEqual((v["dispatch_id"], v["dispatch_kind"]), ("run-evidence-1", "evidence"))
        bw = base_wt(w)
        self.assertEqual(run_git(bw, "ls-files", "-v", "--", "README.md"), "H README.md")
        self.assertEqual((bw / "README.md").read_text(encoding="utf-8"), "hello\n")

    def test_gate_violation_reruns_with_a_fresh_worker(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the final reviewer\n"})
        w = FakeSupervisor(self, [card(1)], gates=True)
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
        self.assertEqual((w.workspace() / "README.md").read_text(encoding="utf-8"), "hello\n")
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")

    def test_gate_violation_cap_ends_blocked(self):
        def tamper(world, ws, a):
            write(ws, {"README.md": "changed by the verifier\n"})
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        t = {"tamper": tamper}
        w.script(None, "verify", t, t, t, t)
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"))
        self.assertEqual(len(found_violations(w)), 3)          # the cap of two voided re-runs, plus one
        self.assertEqual([i for i in w.run_dispatch_ids() if "verify" in i],
                         ["run-verify-1", "run-verify-2", "run-verify-3"])
        self.assertTrue(any("verify workers change tracked files: README.md" in r for r in done["reasons"]))
        self.assertTrue(w.paths.handoff.is_file())
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:README.md"), "hello")


class RunWorkerFailures(unittest.TestCase):
    def test_gate_without_a_result_twice_ends_blocked_environment(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "verify", {"status": "READY"}, {"status": "READY"})
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "environment"))
        self.assertEqual([i for i in w.run_dispatch_ids() if "verify" in i], ["run-verify-1", "run-verify-2"])
        self.assertTrue(w.paths.handoff.is_file())

    def test_gate_usage_limit_ends_blocked_provider_capacity(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"status": "BLOCKED", "blocked_by": "usage limit"})
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "provider capacity"))
        w.cli("run", w.run_dir, env=relaunch_env()).ok()       # capacity is back
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        self.assertEqual([i for i in w.run_dispatch_ids() if "evidence" in i], ["run-evidence-1", "run-evidence-2"])


class Crashes(unittest.TestCase):
    def test_crash_before_gate_record_records_once(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.act(drive_to_run(w, lambda a: a["gate"] == "final_review"))
        r = w.cli("next", w.run_dir, crash="before_gate_record")
        self.assertEqual(r.code, 97, r)
        self.assertEqual(w.events("gate_result"), [])
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        self.assertEqual([e["dispatch_id"] for e in w.events("gate_result")],
                         ["run-final_review-1", "run-evidence-1", "run-verify-1"])

    def crash_final_accept(self, point):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "final_review", {"verdict": "defects"})
        w.act(drive_to_run(w, lambda a: a["kind"] == "final_review" and a["wave"]))
        before = w.branch_tip(RUN)
        r = w.cli("next", w.run_dir, crash=point)
        self.assertEqual(r.code, 97, r)
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
        self.assertEqual(w.branch_tip(FINAL), intent["target"])

    def test_crash_after_final_fast_forward_reconciles(self):
        w, before, intent = self.crash_final_accept("after_fast_forward")
        self.assertEqual(w.branch_tip(RUN), intent["target"])
        reflog = run_git(w.repo, "reflog", "show", "--format=%H", f"refs/heads/{RUN}")
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        (acc,) = [e for e in w.events("accepted") if e["card"] == "final"]
        self.assertEqual(acc["sha"], intent["target"])
        self.assertEqual(run_git(w.repo, "reflog", "show", "--format=%H", f"refs/heads/{RUN}"), reflog)


class FixRound2(unittest.TestCase):
    def test_spent_final_fix_is_not_rechecked_after_a_card_accept_crash(self):
        w = FakeSupervisor(self, [card(1), card(2)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision"
        w.script(None, "final_review", {"verdict": "defects"})
        w.script(None, "final_fix", {"status": "QUESTION"})
        fix = drive_to_run(w, lambda a: a["kind"] == "final_fix")
        worker = w.launch(fix)
        w.cli("unpark", w.run_dir, "--card", "1", "--note", "Use JSON.").ok()
        del w.parks[1]
        finish(w, fix, worker)
        for _ in range(60):
            a = w.next()
            w.act(a)
            if a["action"] == "dispatch" and a["card"] == 1 and a["kind"] == "verify":
                break
        r = w.cli("next", w.run_dir, crash="after_fast_forward")
        self.assertEqual(r.code, 97, r)
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete", done["reasons"])
        self.assertEqual(found_violations(w), [])
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])
        head = w.branch_tip(RUN)
        for e in w.events("accepted"):
            run_git(w.repo, "merge-base", "--is-ancestor", e["sha"], head)
        for n in (1, 2):
            self.assertTrue(on_run_branch(w, f"src/card{n}/c{n}-a1-implement-1.txt"), n)
        (spent,) = [e for e in w.events("verdict_recorded") if e.get("dispatch_id") == fix["dispatch_id"]]
        self.assertEqual(spent["verdict"], "spent")

    def test_crash_between_base_violation_found_and_restored_recovers(self):
        # The Python test crashed the engine's `git update-ref` of the kept base HEAD; here that
        # Git call fails, as a held ref lock makes it, and `next` stops there.
        def move(world, ws, a):
            commit(base_wt(world), {"src/moved.txt": "m\n"}, "moved base")
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": move}, {})
        keep = f"refs/leadhand/{w.meta()['run_id']}/violations/1-base"
        lock = Path(run_git(w.repo, "rev-parse", "--path-format=absolute", "--git-common-dir")) / f"{keep}.lock"
        lock.parent.mkdir(parents=True)
        lock.write_text("", encoding="utf-8")
        r = drive_until_refused(w)
        self.assertNotEqual(r.code, 0, r)
        self.assertEqual([e["stage"] for e in found_violations(w)], ["found"])
        self.assertEqual(run_git(w.repo, "for-each-ref", "--format=%(refname)", keep), "")
        lock.unlink()
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        stages = [e["stage"] for e in w.events("violation")]
        self.assertEqual(stages, ["found", "restored"])
        self.assertEqual(run_git(base_wt(w), "rev-parse", "HEAD"), w.base)
        kept = run_git(w.repo, "for-each-ref", "--format=%(refname)", "refs/leadhand/").splitlines()
        self.assertTrue(any(k.endswith("-base") for k in kept), kept)
        self.assertEqual(run_git(w.repo, "log", "-1", "--format=%s", keep), "moved base")

    def test_evidence_rerun_after_a_worker_failure_gets_a_reset_base(self):
        seen = {}

        def leave(world, ws, a):
            write(base_wt(world), {"tests/leftover_test.py": "x\n", "build/cache.o": "y\n"})

        def look(world, ws, a):
            seen["files"] = {p.name for p in base_wt(world).rglob("*") if p.is_file() and ".git" not in p.parts}
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": leave, "status": "BLOCKED"}, {"tamper": look})
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        self.assertEqual([i for i in w.run_dispatch_ids() if "evidence" in i], ["run-evidence-1", "run-evidence-2"])
        self.assertIn("README.md", seen["files"])
        self.assertNotIn("leftover_test.py", seen["files"])
        self.assertNotIn("cache.o", seen["files"])

    def test_voided_final_fix_reruns_from_the_run_head(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
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
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(1, "implement", {"status": "QUESTION"})
        w.parks[1] = "product decision"
        done = w.run()[-1]
        self.assertEqual(w.run_started, [])
        self.assertEqual(w.events("gate_started"), [])
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"))
        self.assertTrue(w.paths.report.is_file())
        self.assertFalse(base_wt(w).exists())
        self.assertEqual(w.branch_tip(RUN), w.base)


class BaseWorktreeOwnership(unittest.TestCase):
    """The evidence base worktree is checked out, hard-reset and cleaned before every evidence
    request: only ever one this run recorded adding, never an unrelated worktree that happens
    to sit at its path. A run refuses one with exit code 1 and `error: <path> is a worktree
    this run did not add ...`."""

    def foreign_worktree(self, w, path: Path):
        run_git(w.repo, "worktree", "add", "--quiet", "-b", "operator-notes", str(path), w.base)
        write(path, {"valuable.txt": "the operator's untracked notes\n"})
        return path

    def assert_untouched(self, w, path: Path):
        self.assertEqual((path / "valuable.txt").read_text(encoding="utf-8"), "the operator's untracked notes\n")
        self.assertEqual(run_git(path, "symbolic-ref", "--short", "HEAD"), "operator-notes")

    def assert_refused(self, r, path: Path):
        self.assertEqual(r.code, 1, r)
        self.assertIn(f"error: {path} is a worktree this run did not add", r.stderr)

    def crash_window(self, w) -> Path:
        """The state a crash right after the run's `git worktree add` leaves: the add recorded,
        the worktree detached at the base, without the run's marker."""
        path = base_wt(w)
        append_event(w.run_dir, "base_worktree_added", path=str(path))
        run_git(w.repo, "worktree", "add", "--quiet", "--detach", str(path), w.base)
        self.assertFalse(marker(path).exists())
        return path

    def test_worktree_run_refuses_a_foreign_worktree_at_its_base_path(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        path = self.foreign_worktree(w, base_wt(w))
        self.assert_refused(drive_until_refused(w), path)
        self.assert_untouched(w, path)
        self.assertEqual(w.events("gate_result")[0]["gate"], "final_review")
        self.assertNotIn("evidence", gate_names(w))

    def test_a_foreign_worktree_after_a_failed_add_is_refused(self):
        # The intent was recorded, then the add failed or crashed, and a worktree appeared there.
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        path = base_wt(w)
        append_event(w.run_dir, "base_worktree_added", path=str(path))
        self.foreign_worktree(w, path)
        self.assert_refused(drive_until_refused(w), path)
        self.assert_untouched(w, path)

    def test_a_foreign_worktree_replacing_the_runs_base_worktree_is_refused(self):
        # While the first evidence worker runs, the run's base worktree is removed and an
        # unrelated one added at its path; the worker ends without a report, so the evidence
        # gate is requested again.
        def replace(world, ws, a):
            path = base_wt(world)
            self.assertEqual(marker(path).read_text(encoding="utf-8").strip(), world.meta()["run_id"])
            run_git(world.repo, "worktree", "remove", "--force", str(path))
            self.foreign_worktree(world, path)
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.script(None, "evidence", {"tamper": replace, "status": "BLOCKED"})
        path = base_wt(w)
        self.assert_refused(drive_until_refused(w), path)
        self.assertEqual([i for i in w.run_dispatch_ids() if "evidence" in i], ["run-evidence-1"])
        self.assert_untouched(w, path)

    def test_a_base_worktree_left_by_a_crash_is_refused_once_it_holds_work(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        path = self.crash_window(w)
        write(path, {"valuable.txt": "written there while the run was down\n"})
        r = drive_until_refused(w)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        self.assertEqual((path / "valuable.txt").read_text(encoding="utf-8"), "written there while the run was down\n")

    def test_a_crash_window_worktree_hiding_a_tracked_edit_is_refused(self):
        # `status` does not show an edit behind assume-unchanged (or a skip-worktree file),
        # but the reset would overwrite it (or leave the evidence base off its commit).
        for flag in ("--assume-unchanged", "--skip-worktree"):
            with self.subTest(flag=flag):
                w = FakeSupervisor(self, [card(1)], gates=True)
                w.start()
                path = self.crash_window(w)
                write(path, {"README.md": "an edit someone hid from status\n"})
                run_git(path, "update-index", flag, "README.md")
                self.assertEqual(run_git(path, "status", "--porcelain", "--ignored", "--untracked-files=all"), "")
                r = drive_until_refused(w)
                self.assertEqual(r.code, 1, r)
                self.assertTrue(r.stderr.startswith("error: "), r)
                self.assertEqual((path / "README.md").read_text(encoding="utf-8"), "an edit someone hid from status\n")

    def test_a_base_worktree_added_before_a_crash_is_still_the_runs(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        path = self.crash_window(w)
        self.assertEqual(w.run()[-1]["outcome"], "complete")
        self.assertEqual(run_git(path, "rev-parse", "HEAD"), w.base)
        self.assertEqual(marker(path).read_text(encoding="utf-8").strip(), w.meta()["run_id"])
        self.assertEqual([e["gate"] for e in w.events("gate_result") if e["passed"]],
                         ["final_review", "evidence", "verify"])


class SparseCheckout(unittest.TestCase):
    """R63: sparse checkouts are not supported in this version; so every assume-unchanged or
    skip-worktree entry seen after a worker is the worker's, a sparse checkout it turned on
    itself included."""

    def test_a_worktree_run_in_a_sparse_checkout_is_refused_at_start(self):
        w = FakeSupervisor(self, [card(1)])
        w.base = commit(w.repo, {"far/away.txt": "outside the sparse patterns\n"}, "far")
        w.write_deck([card(1)])
        run_git(w.repo, "sparse-checkout", "set", "--no-cone", "/README.md", "/src/")
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertIn("sparse checkouts are not supported", r.stderr)
        self.assertFalse((w.docs / "runs").exists())
        self.assertEqual(run_git(w.repo, "worktree", "list", "--porcelain").count("worktree "), 1)

    def test_a_sparse_checkout_a_reviewer_turns_on_is_a_violation(self):
        # Turning sparse checkout on hides README.md (skip-worktree, file removed): the gates
        # would check a tree without it. Leadhand unhides it, counts it, and turns sparse off.
        def tamper(world, ws, a):
            run_git(ws, "sparse-checkout", "set", "--no-cone", "/src/")
        w = FakeSupervisor(self, [card(1)])
        w.start()
        w.script(1, "review", {"tamper": tamper}, {})
        w.run()
        found = [e for e in w.events("violation") if e.get("stage") == "found"]
        self.assertEqual([e.get("dispatch_id") for e in found], ["c1-a1-review-1"])
        self.assertIn("README.md", found[0]["hidden"])
        self.assertIn("README.md", found[0]["paths"])
        ws = w.workspace()
        self.assertEqual((ws / "README.md").read_text(encoding="utf-8"), "hello\n")
        self.assertNotIn("S README.md", run_git(ws, "ls-files", "-v").splitlines())
        self.assertEqual(run_git(ws, "config", "--bool", "--get", "core.sparseCheckout", check=False), "false")


# --------------------------------------------------------------------------
# test_final_review_minors.py

class Timer(unittest.TestCase):
    def test_timer_returns_promptly_when_the_run_is_paused_or_cancelled(self):
        # `timer` looks for the flags between its polls (every 10 s at most), not only at its start.
        for request in ("pause", "cancel"):
            with self.subTest(request=request):
                w = FakeSupervisor(self, [card(1)])
                w.start()
                a = w.next()
                w.launch(a)
                timer = TimerProc(self, w, a["dispatch_id"])
                with self.assertRaises(subprocess.TimeoutExpired):
                    timer.proc.wait(timeout=1.5)       # still waiting: no result, no flag, no deadline
                w.cli(request, w.run_dir).ok()
                out, err = timer.proc.communicate(timeout=30)
                self.assertEqual(timer.proc.returncode, 0, err)
                self.assertEqual(out.splitlines(), ["pause or cancel requested"])


class TimerProc:
    """`timer RUN_DIR --dispatch ID` running in the background; killed at cleanup."""

    def __init__(self, test, w, dispatch_id):
        self.proc = subprocess.Popen([str(BIN), "timer", str(w.run_dir), "--dispatch", dispatch_id],
                                     env=w._env(), cwd=w.tmp, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
        test.addCleanup(self.stop)

    def stop(self):
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.communicate(timeout=10)


class PinnedIntegrate(unittest.TestCase):
    def test_integrate_after_a_codex_pinned_card_runs_on_claude(self):
        w = codex_world(self, [card(1, route="codex gpt-6-luna low"), card(2)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"status": "QUESTION", "files": {"src/shared.txt": "card one\n"}})
        w.script(2, "implement", {"files": {"src/shared.txt": "card two\n"}})
        w.parks[1] = "decision needed: storage format"
        w.run()
        del w.parks[1]
        w.cli("unpark", w.run_dir, "--card", "1", "--note", "Use JSON.").ok()
        w.run()
        self.assertNotIn("integration needs a Claude worker", [e["reason"] for e in w.events("parked")])
        self.assertNotIn("pinned vendor not installed", [e["reason"] for e in w.events("parked")])
        integrate = [e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "integrate"]
        self.assertTrue(integrate)
        self.assertEqual(integrate[0]["vendor"], "claude")
        self.assertTrue(any(a["kind"] == "integrate" and a["agent"] != "leadhand:codex" for a in w.started))
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])
        self.assertEqual(run_git(w.repo, "show", f"{RUN}:src/shared.txt"), "card one")


class OperatorCommands(unittest.TestCase):
    def test_pause_cancel_and_handoff_print_the_quoted_absolute_cli(self):
        # The commands name the binary (compared by file, S1-R11) and the quoted run directory.
        w = FakeSupervisor(self, [card(1)])
        w.start()
        root = shlex.quote(str(w.run_dir))
        self.assertIn(" ", str(w.run_dir))
        self.assertTrue(names_bin_before(w.cli("pause", w.run_dir).ok().stdout, f" run {root}"))
        self.assertTrue(names_bin_before(w.cli("cancel", w.run_dir).ok().stdout, f" run {root}"))
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        w.paths.cancel_flag.unlink()
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "blocked")
        text = w.paths.handoff.read_text(encoding="utf-8")
        self.assertTrue(names_bin_before(text, f" run {root}"), text)
        self.assertTrue(names_bin_before(text, f" launch-line {root}"), text)
        self.assertTrue(names_bin_before(text, f" unpark {root}"), text)

    def test_relaunch_refusal_names_the_quoted_start_command(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        w.run()
        r = w.cli("run", w.run_dir, env={"PATH": f"{FAKES}{os.pathsep}{os.environ['PATH']}"})
        self.assertEqual(r.code, 1, r)
        self.assertTrue(names_bin_before(r.stderr, f" start {shlex.quote(str(w.deck_path.resolve()))}"), r)


class Settings(unittest.TestCase):
    def test_notify_and_capacity_are_warned_as_not_supported(self):
        w = FakeSupervisor(self, [card(1)], config='[notify]\ncommand = "say done"\n[capacity]\ncodex_pause_percent = 80\n')
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 0, r)
        self.assertIn("notify.command", r.stdout + r.stderr)
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 0, r)              # warned, not refused
        self.assertIn("not supported in this version: notify.command = say done", r.stderr)
        self.assertIn("not supported in this version: capacity.codex_pause_percent = 80", r.stderr)

    def test_defaults_are_not_warned(self):
        w = FakeSupervisor(self, [card(1)])
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 0, r)
        self.assertNotIn("not supported", r.stdout + r.stderr)
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 0, r)
        self.assertIn("run: ", r.stdout)
        self.assertNotIn("not supported", r.stdout + r.stderr)


class Start(unittest.TestCase):
    def test_start_skips_an_old_run_whose_event_log_is_unreadable(self):
        w = FakeSupervisor(self, [card(1)])
        old = w.start()
        with open(old / "events.jsonl", "ab") as f:
            f.write(b"{not json\n")
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 0, r)
        self.assertIn("warning", r.stderr)
        self.assertIn(str(old), r.stderr)
        self.assertIn("run:", r.stdout)


class CopyFiles(unittest.TestCase):
    def test_a_symlinked_report_is_not_followed(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        secret = w.tmp / "secret.txt"
        secret.write_text("Status: READY\nnot the worker's\n", encoding="utf-8")
        report = Path(a["report"])
        report.parent.mkdir(parents=True, exist_ok=True)
        report.symlink_to(secret)
        reply = Path(a["reply_file"])
        reply.write_text("no status here", encoding="utf-8")
        ev = w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
                   "--reply-file", reply).ok().json()
        folder = w.run_dir / "cards" / "1" / a["dispatch_id"]
        self.assertFalse((folder / "report.md").exists())
        self.assertNotIn("report", ev["files"])
        self.assertTrue(any("symlink" in n or "regular file" in n for n in ev.get("files_skipped") or []), ev)
        self.assertEqual(ev["status"], "BLOCKED")
        self.assertEqual(secret.read_text(encoding="utf-8"), "Status: READY\nnot the worker's\n")


SLOW_GIT = r'''#!/usr/bin/env python3
"""git, except that leadhand's own commit of a worker's changes takes 2.5 s; meanwhile the
worker folders' heartbeats are looked at, after the wait."""
import json, os, sys, time
from pathlib import Path
args = sys.argv[1:]
if "commit" in args and any(a.startswith("Leadhand-Dispatch: ") for a in args):
    began = time.time()
    time.sleep(2.5)
    beats = {str(p): p.stat().st_mtime for p in Path(os.environ["SLOW_GIT_WORKERS"]).glob("*/heartbeat")}
    with open(os.environ["SLOW_GIT_LOG"], "a", encoding="utf-8") as f:
        f.write(json.dumps({"began": began, "beats": beats}) + "\n")
real = os.environ["SLOW_GIT_REAL"]
os.execv(real, [real, *args])
'''


class RunnerFinalize(unittest.TestCase):
    def test_heartbeat_is_touched_while_finalizing(self):
        # A Codex worker leaves its change uncommitted; the runner's own commit of it is slow.
        w = codex_world(self, [card(1, route="codex gpt-6-luna low")])
        w.start()
        w.script(1, "implement", {"commit": False})
        shim = w.tmp / "slow git"
        shim.mkdir()
        (shim / "git").write_text(SLOW_GIT, encoding="utf-8")
        (shim / "git").chmod(0o755)
        log = w.tmp / "slow git.jsonl"
        w.env.update({"PATH": f"{shim}{os.pathsep}{w.env['PATH']}", "SLOW_GIT_REAL": shutil.which("git"),
                      "SLOW_GIT_LOG": str(log), "SLOW_GIT_WORKERS": str(w.run_dir / "workers")})
        a = w.next()
        self.assertEqual((a["kind"], a["agent"]), ("implement", "leadhand:codex"))
        w.act(a)
        (res,) = [e for e in w.events("result_recorded") if e["dispatch_id"] == a["dispatch_id"]]
        self.assertTrue(res.get("committed"), res)
        (slow,) = [json.loads(ln) for ln in log.read_text(encoding="utf-8").splitlines()]
        beat = str(w.run_dir / "workers" / a["dispatch_id"] / "heartbeat")
        self.assertIn(beat, slow["beats"])
        self.assertGreaterEqual(slow["beats"][beat], slow["began"])     # touched while the commit ran


if __name__ == "__main__":
    unittest.main()
