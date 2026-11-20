"""The run worktree, recognised by identity before leadhand runs Git there (ruling S2-R54; spec,
bug classes 1 and 4). Rust-only.

The run worktree `<repo>/.worktrees/<slug>` lies inside the operator's checkout. Once its `.git`
file is gone, Git run there finds the operator's repository, so every write meant for the
workspace would land in the operator's checkout. Bug class 1: leadhand never checks out, resets,
cleans, stashes or stages in the operator's working tree. Bug class 4: the run worktree is
recognised by identity, so a folder is acted on as the run's worktree only while Git's top there
is the folder itself and the run's repository lists it as a linked worktree.

Otherwise the command refuses before it writes anything: exit 1, `error:` naming the workspace
and the top Git found, and the one command that fixes it alone on the last line. While the
repository still lists the worktree, that is `git -C <repo> worktree repair`, which restores the
`.git` file. The scenario is the final review's probe. The Python reference acts on the operator's
checkout there (it switches it to the card branch and keeps the operator's edit only on a
leftovers ref); it is not changed in this stage.
"""
import os
import shlex
import subprocess
import unittest
from pathlib import Path

from contract_launch import World
from gitfixture import run_git
from harness import rust_only
from supervisor import FakeSupervisor, card


def checkout(repo: Path, tracked: str) -> dict:
    """What the operator sees of their checkout: branch, HEAD, index (with its flags), status and
    the tracked file they edited."""
    return {
        "branch": run_git(repo, "symbolic-ref", "HEAD"),
        "head": run_git(repo, "rev-parse", "HEAD"),
        "index": run_git(repo, "ls-files", "--stage"),
        "flags": run_git(repo, "ls-files", "-v"),
        "status": run_git(repo, "status", "--porcelain", "--untracked-files=all"),
        "file": (repo / tracked).read_text(encoding="utf-8"),
    }


def refusal_head(ws: Path, repo: Path) -> str:
    return (f"error: the run worktree {ws} is not a worktree top (Git's top there is "
            f"{os.path.realpath(repo)}); leadhand will not run Git there. ")


def broken(w: FakeSupervisor) -> tuple[Path, str, dict]:
    """The run's worktree with its `.git` file gone, and an uncommitted edit of the operator's to a
    tracked file in their own checkout: the workspace, that file, and the checkout as it is now."""
    ws = w.workspace()
    tracked = run_git(w.repo, "ls-files").splitlines()[0]
    (w.repo / tracked).write_text("the operator's uncommitted edit\n", encoding="utf-8")
    (ws / ".git").unlink()
    return ws, tracked, checkout(w.repo, tracked)


@rust_only(uses=("next",))
class MissingGitFile(unittest.TestCase):
    def test_next_refuses_and_leaves_the_operators_checkout_untouched(self):
        # Bug classes 1 and 4: the review's probe. Before the fix, `next` switched the operator's
        # checkout to the card branch and moved the edit onto a leftovers ref.
        w = FakeSupervisor(self, [card(1), card(2)])
        w.start()
        run_id = w.meta()["run_id"]
        ws, tracked, before = broken(w)
        events = w.events()

        r = w.cli("next", w.run_dir)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stdout, "", r)
        lines = r.stderr.splitlines()
        self.assertEqual(len(lines), 2, r)
        self.assertEqual(lines[0], refusal_head(ws, w.repo) + "Restore its .git file with:", r)
        repair = ["git", "-C", os.path.realpath(w.repo), "worktree", "repair"]
        self.assertEqual(lines[1], shlex.join(repair), r)

        # Nothing was written: the operator's checkout, their edit included, is as it was, no
        # leftovers ref holds it, and no event was appended (no `violation` among them).
        self.assertEqual(checkout(w.repo, tracked), before)
        self.assertEqual(run_git(w.repo, "for-each-ref", "--format=%(refname)", f"refs/leadhand/{run_id}/"), "")
        self.assertEqual(w.events(), events)
        self.assertEqual(w.events("violation"), [])

        # The fix line is true: once it has run, the worktree is the run's again and `next`
        # carries on there, still leaving the operator's checkout alone.
        subprocess.run(shlex.split(lines[1]), env=w.env, cwd=w.tmp, check=True, capture_output=True)
        self.assertEqual(run_git(ws, "rev-parse", "--show-toplevel"), os.path.realpath(ws))
        self.assertEqual(w.next()["action"], "dispatch")
        self.assertEqual(checkout(w.repo, tracked), before)

    def test_next_names_a_new_run_once_the_worktree_record_is_pruned(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        ws, tracked, _ = broken(w)
        run_git(w.repo, "worktree", "prune")
        before = checkout(w.repo, tracked)
        events = w.events()

        r = w.cli("next", w.run_dir)
        self.assertEqual(r.code, 1, r)
        lines = r.stderr.splitlines()
        self.assertEqual(lines[0], refusal_head(ws, w.repo) + "Start a new run of the deck with:", r)
        # Alone on the last line: the command that starts a new run of the same deck.
        argv = shlex.split(lines[1])
        self.assertEqual(argv[1:], ["start", w.meta()["deck_source"], "--repo", w.meta()["repo"]], r)
        self.assertEqual(checkout(w.repo, tracked), before)
        self.assertEqual(w.events(), events)


@rust_only(uses=("run",))
class RunRefuses(unittest.TestCase):
    def test_run_refuses_before_the_supervisor_starts(self):
        # `run` would start the supervisor in the folder, where every command it runs acts.
        w = World(self)
        ws, tracked, before = broken(w.sup)
        events = w.sup.events()

        r = w.run()
        self.assertEqual(r.code, 1, r)
        lines = r.stderr.splitlines()
        self.assertEqual(lines[0], refusal_head(ws, w.repo) + "Restore its .git file with:", r)
        self.assertEqual(lines[-1], shlex.join(["git", "-C", os.path.realpath(w.repo), "worktree", "repair"]), r)
        self.assertFalse(w.log.exists(), "the supervisor started")
        self.assertEqual(w.sup.events(), events)
        self.assertEqual(checkout(w.repo, tracked), before)


if __name__ == "__main__":
    unittest.main()
