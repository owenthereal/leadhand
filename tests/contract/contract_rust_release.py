#!/usr/bin/env python3
"""The branch release (deliberate change 2, spec "The branch release"). Rust-only.

When a run ends complete, incomplete or failed, leadhand detaches the run worktree at the run
head, between the `outcome` event and the report: `branch_release_started {worktree, branch,
sha}`, the detach, then `branch_released` (with `skipped` and a reason when there was nothing to
detach). The report's Result section then says what Git shows afterwards, not what leadhand did:
the branch is free only when `git worktree list --porcelain` shows no worktree, prunable ones
included, with the run branch checked out; otherwise it names the holder and the command that
frees it. The release never prunes worktree records. Blocked runs keep the branch checked out:
they resume there.

What the release did is judged from Git: the worktree's HEAD, the worktree list, and whether the
operator can `git switch` to the branch in their own checkout. The events are asserted beside
that. `test_release_never_runs_worktree_prune` logs every Git command line the binary runs
through a `git` wrapper first on PATH.
"""
import json
import os
import shutil
import unittest
from pathlib import Path

from contract_finish import base_wt, to_last_step
from gitfixture import run_git
from harness import rust_only
from rundir import append_event
from supervisor import FakeSupervisor, card

RUN = "leadhand/demo"
FREE = f"- Branch `{RUN}` is free: check it out anywhere with `git switch {RUN}`"

GIT_LOG = r'''#!/usr/bin/env python3
"""git, logging each command line (one JSON list per line) to $GIT_ARGV_LOG first."""
import json, os, sys
with open(os.environ["GIT_ARGV_LOG"], "a", encoding="utf-8") as f:
    f.write(json.dumps(sys.argv[1:]) + "\n")
real = os.environ["GIT_ARGV_REAL"]
os.execv(real, [real, *sys.argv[1:]])
'''


def kinds(w, *names):
    """The kinds of the events named, in log order."""
    return [e["kind"] for e in w.events() if e["kind"] in names]


def seq(w, kind):
    (e,) = w.events(kind)
    return e["seq"]


def result_lines(w) -> list[str]:
    text = w.paths.report.read_text(encoding="utf-8")
    return text.split("## Result")[1].split("## Evidence")[0].strip().splitlines()


def listed_branches(w) -> dict[str, str]:
    """Each registered worktree's checked-out branch ref (`git worktree list --porcelain`)."""
    out, path = {}, None
    for line in run_git(w.repo, "worktree", "list", "--porcelain").splitlines():
        if line.startswith("worktree "):
            path = line[len("worktree "):]
        elif line.startswith("branch ") and path is not None:
            out[path] = line[len("branch "):]
    return out


def detached(path: Path) -> bool:
    return run_git(path, "symbolic-ref", "--quiet", "HEAD", check=False) == ""


def can_switch(w) -> bool:
    """Whether the operator's own checkout can switch to the run branch (then back to main)."""
    run_git(w.repo, "switch", "--quiet", RUN, check=False)
    if run_git(w.repo, "symbolic-ref", "--short", "HEAD") != RUN:
        return False
    run_git(w.repo, "switch", "--quiet", "main")
    return True


def git_wrapper(w) -> Path:
    """A `git` first on PATH that logs its argv; returns the log."""
    folder = w.tmp / "git wrapper"
    folder.mkdir()
    (folder / "git").write_text(GIT_LOG, encoding="utf-8")
    (folder / "git").chmod(0o755)
    log = w.tmp / "git argv.jsonl"
    w.env.update({"PATH": f"{folder}{os.pathsep}{w.env['PATH']}", "GIT_ARGV_REAL": shutil.which("git"),
                  "GIT_ARGV_LOG": str(log)})
    return log


def logged(log: Path) -> list[list[str]]:
    return [json.loads(ln) for ln in log.read_text(encoding="utf-8").splitlines()] if log.exists() else []


def runs(argv: list[str], *words: str) -> bool:
    """Whether `words` appear in `argv` one after another."""
    return any(argv[i:i + len(words)] == list(words) for i in range(len(argv)))


@rust_only(uses=("start", "next", "record", "report"))
class BranchRelease(unittest.TestCase):
    def test_complete_run_detaches_worktree_and_reports_branch_free(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        ws = w.workspace()
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "complete")
        head = w.branch_tip(RUN)

        # Recorded between the outcome and the report.
        self.assertEqual(kinds(w, "outcome", "branch_release_started", "branch_released", "report_written"),
                         ["outcome", "branch_release_started", "branch_released", "report_written"])
        (started,) = w.events("branch_release_started")
        self.assertEqual((started["worktree"], started["branch"], started["sha"]), (str(ws), RUN, head))
        (released,) = w.events("branch_released")
        self.assertFalse(released.get("skipped"), released)

        # Judged from Git: the worktree is detached at the run head, no worktree holds the
        # branch, and the operator's own checkout can switch to it.
        self.assertTrue(detached(ws))
        self.assertEqual(run_git(ws, "rev-parse", "HEAD"), head)
        self.assertNotIn(f"refs/heads/{RUN}", listed_branches(w).values())
        self.assertTrue(can_switch(w))

        lines = result_lines(w)
        line = next((ln for ln in lines if ln.startswith(FREE)), None)
        self.assertIsNotNone(line, lines)
        self.assertIn(str(ws), line)

        # Repeating next changes nothing.
        self.assertEqual(w.next()["outcome"], "complete")
        self.assertEqual(len(w.events("branch_release_started")), 1)
        self.assertEqual(len(w.events("branch_released")), 1)

    def test_deleted_worktree_folder_reports_prune_then_switch(self):
        # The run's worktree folder is deleted by hand: Git keeps its record (prunable), which
        # still has the run branch checked out. The run fails (its branch cannot be read there);
        # the release has nothing to detach, and the report gives the fix.
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        ws = w.workspace()
        shutil.rmtree(ws)
        done = w.next()
        self.assertEqual((done["action"], done["outcome"]), ("done", "failed"))
        (released,) = w.events("branch_released")
        self.assertTrue(released.get("skipped"), released)
        self.assertTrue(released.get("reason"), released)

        # Never pruned: Git still records the worktree with the branch checked out.
        self.assertEqual(listed_branches(w).get(str(ws)), f"refs/heads/{RUN}")
        lines = result_lines(w)
        self.assertFalse(any(ln.startswith(FREE) for ln in lines), lines)
        (line,) = [ln for ln in lines if ln.startswith(f"- Branch `{RUN}`")]
        self.assertIn(str(ws), line)
        prune, switch = line.find("`git worktree prune`"), line.find(f"`git switch {RUN}`")
        self.assertTrue(0 <= prune < switch, line)

        # The fix works as the report says.
        self.assertFalse(can_switch(w))
        run_git(w.repo, "worktree", "prune")
        self.assertTrue(can_switch(w))

    def test_blocked_run_keeps_branch_checked_out(self):
        w = FakeSupervisor(self, [card(1)], config='[run]\nevidence = "block"\n', gates=True)
        w.start()
        gap = {"result": "fail"}
        w.script(None, "evidence", gap, gap, gap)
        done = w.run()[-1]
        self.assertEqual((done["outcome"], done["qualifier"]), ("blocked", "decision needed"))
        ws = w.workspace()
        self.assertEqual(kinds(w, "branch_release_started", "branch_released"), [])
        self.assertEqual(run_git(ws, "symbolic-ref", "--short", "HEAD"), RUN)
        self.assertEqual(listed_branches(w).get(str(ws)), f"refs/heads/{RUN}")
        self.assertFalse(any(ln.startswith(f"- Branch `{RUN}`") for ln in result_lines(w)), result_lines(w))

    def test_release_never_runs_worktree_prune(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        to_last_step(w)                         # the next `next` ends the run
        log = git_wrapper(w)
        self.assertEqual(w.next()["outcome"], "complete")
        argvs = logged(log)
        # The wrapper saw the binary's Git calls: the release's detach among them.
        self.assertTrue(any("checkout" in a and "--detach" in a for a in argvs), argvs)
        self.assertTrue(any(runs(a, "worktree", "list") for a in argvs), argvs)
        self.assertEqual([a for a in argvs if runs(a, "worktree", "prune")], [])
        self.assertTrue(detached(w.workspace()))
        # Nor does writing the report again.
        log.unlink()
        w.cli("report", w.run_dir).ok()
        argvs = logged(log)
        self.assertTrue(any(runs(a, "worktree", "list") for a in argvs), argvs)
        self.assertEqual([a for a in argvs if runs(a, "worktree", "prune")], [])
        # The evidence base worktree, which the evidence gate prepared, is still registered.
        self.assertIn(str(base_wt(w)), run_git(w.repo, "worktree", "list", "--porcelain"))

    def test_crash_between_outcome_and_report_finishes_release_first(self):
        # The state a crash between the outcome and the report leaves, rebuilt as stage 1's
        # interrupted-start test rebuilds one: the run is driven to its end, its log cut back to
        # the `outcome`, `report.md` removed, then the release's intent appended in its exact
        # shape, with the worktree still on the run branch (the crash came before the detach)
        # or already detached (after it). The release has no named crash points (S2-R48).
        for detached_before in (False, True):
            with self.subTest(detached_before=detached_before):
                w = FakeSupervisor(self, [card(1)], gates=True)
                w.start()
                self.assertEqual(w.run()[-1]["outcome"], "complete")
                ws, head = w.workspace(), w.branch_tip(RUN)
                lines = w.paths.events.read_text(encoding="utf-8").splitlines(keepends=True)
                (at,) = [i for i, ln in enumerate(lines) if json.loads(ln)["kind"] == "outcome"]
                w.paths.events.write_text("".join(lines[:at + 1]), encoding="utf-8")
                w.paths.report.unlink()
                if not detached_before:
                    run_git(ws, "switch", "--quiet", RUN)
                append_event(w.run_dir, "branch_release_started", worktree=str(ws), branch=RUN, sha=head)
                self.assertEqual(kinds(w, "outcome", "branch_release_started", "branch_released",
                                       "report_written"), ["outcome", "branch_release_started"])
                self.assertEqual(detached(ws), detached_before)

                done = w.next()
                self.assertEqual((done["action"], done["outcome"]), ("done", "complete"))
                # The release is finished first, once, then the report is written.
                self.assertEqual(kinds(w, "outcome", "branch_release_started", "branch_released",
                                       "report_written"),
                                 ["outcome", "branch_release_started", "branch_released", "report_written"])
                self.assertLess(seq(w, "branch_released"), seq(w, "report_written"))
                (released,) = w.events("branch_released")
                self.assertFalse(released.get("skipped"), released)
                self.assertTrue(detached(ws))
                self.assertEqual(run_git(ws, "rev-parse", "HEAD"), head)
                self.assertTrue(any(ln.startswith(FREE) for ln in result_lines(w)), result_lines(w))

if __name__ == "__main__":
    unittest.main()
