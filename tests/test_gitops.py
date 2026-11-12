import os
import shutil
import tempfile
import unittest
from pathlib import Path

from gitfixture import (
    commit, current_branch, hermetic, make_origin, make_repo, run_git, update_origin, write,
)
from leadhand.gitops import (
    GitError, Violation, card_branch_name, changed_paths, cut_card_branch, default_base,
    detect_violation, fast_forward, git, git_common_dir, head, is_test_or_doc_path,
    is_ancestor, needs_integration, preserve_and_restore, prepare_workspace, rebase_onto, stage_work,
    untracked_files,
)

RUN = "leadhand/run-1"


class GitopsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="leadhand gitops "))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        hermetic(self, self.tmp)

    # -- helpers ---------------------------------------------------------

    def workspace(self, *, use_worktree=True, default_branch="main"):
        repo = make_repo(self.tmp, default_branch=default_branch)
        base = run_git(repo, "rev-parse", "HEAD")
        ws = prepare_workspace(repo, run_branch=RUN, base_sha=base, slug="run-1", use_worktree=use_worktree)
        return repo, ws, base

    def card(self, ws, n, files):
        branch, base = cut_card_branch(ws, RUN, n)
        tip = commit(ws, files, f"card {n}")
        return branch, base, tip

    def exclude_lines(self, repo):
        return (git_common_dir(repo) / "info" / "exclude").read_text().splitlines()

    # -- default base ----------------------------------------------------

    def test_default_base_uses_origin_default_branch(self):
        repo = make_repo(self.tmp, default_branch="develop")
        origin = make_origin(repo)
        stale = run_git(repo, "rev-parse", "refs/remotes/origin/develop")
        upstream = commit(repo, {"up.txt": "1\n"}, "upstream work")
        self.assertEqual(update_origin(origin, repo, "develop"), upstream)
        commit(repo, {"local.txt": "1\n"}, "local only")  # local develop is ahead of origin

        self.assertNotEqual(stale, upstream)
        self.assertEqual(default_base(repo), ("origin/develop", upstream))  # fetched
        self.assertEqual(run_git(repo, "rev-parse", "refs/remotes/origin/develop"), upstream)

        # Without refs/remotes/origin/HEAD it asks the remote.
        run_git(repo, "remote", "set-head", "origin", "--delete")
        self.assertEqual(default_base(repo), ("origin/develop", upstream))

    def test_default_base_without_origin_uses_local_default(self):
        repo = make_repo(self.tmp, default_branch="trunk")
        trunk = run_git(repo, "rev-parse", "HEAD")
        run_git(repo, "branch", "feature")
        self.assertEqual(default_base(repo), ("trunk", trunk))

        # From a linked worktree on another branch, the main repo's branch still wins.
        other = self.tmp / "feature tree"
        run_git(repo, "worktree", "add", "--quiet", str(other), "feature")
        commit(other, {"f.txt": "f\n"}, "feature work")
        self.assertEqual(default_base(other), ("trunk", trunk))

    # -- workspace -------------------------------------------------------

    def test_prepare_workspace_is_idempotent_and_excludes_paths(self):
        repo, ws, base = self.workspace()
        self.assertEqual(ws, repo / ".worktrees" / "run-1")
        self.assertTrue(ws.is_dir())
        self.assertEqual(current_branch(ws), RUN)
        self.assertEqual(head(ws), base)

        advanced = commit(ws, {"run.txt": "1\n"}, "run work")
        again = prepare_workspace(repo, run_branch=RUN, base_sha=base, slug="run-1", use_worktree=True)
        self.assertEqual(again, ws)
        self.assertEqual(head(repo, RUN), advanced)  # an existing run branch is not reset

        lines = self.exclude_lines(repo)
        self.assertEqual(lines.count(".worktrees/"), 1)
        self.assertEqual(lines.count(".leadhand/"), 1)
        write(repo, {".leadhand/state.json": "{}\n"})
        self.assertEqual(run_git(repo, "status", "--porcelain"), "")

    def test_prepare_workspace_without_worktree_uses_checkout(self):
        repo, ws, base = self.workspace(use_worktree=False)
        self.assertEqual(ws, repo)
        self.assertEqual(current_branch(repo), RUN)
        self.assertEqual(head(repo), base)
        self.assertFalse((repo / ".worktrees").exists())
        self.assertEqual(prepare_workspace(repo, run_branch=RUN, base_sha=base, slug="run-1", use_worktree=False), repo)
        self.assertEqual(current_branch(repo), RUN)
        self.assertIn(".leadhand/", self.exclude_lines(repo))

    # -- cards -----------------------------------------------------------

    def test_cut_card_branch_from_run_tip_and_reuse(self):
        repo, ws, base = self.workspace()
        tip = commit(ws, {"run.txt": "1\n"}, "run work")
        self.assertEqual(card_branch_name(RUN, 3), "leadhand/run-1--card-3")

        branch, cut_from = cut_card_branch(ws, RUN, 3)
        self.assertEqual((branch, cut_from), ("leadhand/run-1--card-3", tip))
        self.assertEqual(current_branch(ws), branch)
        work = commit(ws, {"card.txt": "3\n"}, "card work")

        run_git(ws, "checkout", "--quiet", RUN)
        commit(ws, {"run.txt": "2\n"}, "more run work")
        self.assertEqual(cut_card_branch(ws, RUN, "3"), (branch, tip))
        self.assertEqual(current_branch(ws), branch)
        self.assertEqual(head(ws, branch), work)  # reused, not reset

    def test_cut_card_branch_refuses_an_existing_branch_the_run_did_not_record(self):
        repo, ws, base = self.workspace()
        run_git(ws, "branch", "leadhand/run-1--card-2", base)
        run_git(ws, "checkout", "--quiet", "leadhand/run-1--card-2")
        theirs = commit(ws, {"theirs.txt": "1\n"}, "not this run's")
        run_git(ws, "checkout", "--quiet", RUN)
        with self.assertRaisesRegex(GitError, "leadhand/run-1--card-2"):
            cut_card_branch(ws, RUN, 2, recorded=False)
        self.assertEqual(current_branch(ws), RUN)
        self.assertEqual(head(ws, "leadhand/run-1--card-2"), theirs)
        # A recorded one is reused; an unrecorded one at the run tip (a cut whose record
        # was lost to a crash) holds nothing of its own and is taken up.
        self.assertEqual(cut_card_branch(ws, RUN, 2, recorded=True)[0], "leadhand/run-1--card-2")
        run_git(ws, "branch", "leadhand/run-1--card-3", RUN)
        self.assertEqual(cut_card_branch(ws, RUN, 3, recorded=False), ("leadhand/run-1--card-3", base))

    def test_fast_forward_is_idempotent(self):
        repo, ws, base = self.workspace()
        branch, _, tip = self.card(ws, 1, {"a.txt": "a\n"})
        self.assertEqual(fast_forward(ws, RUN, tip), tip)
        self.assertEqual(head(ws, RUN), tip)
        self.assertEqual(fast_forward(ws, RUN, tip), tip)
        self.assertEqual(fast_forward(ws, RUN, branch), tip)
        self.assertEqual(head(ws, RUN), tip)

    def test_fast_forward_refuses_divergence(self):
        repo, ws, base = self.workspace()
        _, _, tip_a = self.card(ws, 1, {"a.txt": "a\n"})
        run_git(ws, "checkout", "--quiet", RUN)
        _, _, tip_b = self.card(ws, 2, {"b.txt": "b\n"})
        fast_forward(ws, RUN, tip_b)
        with self.assertRaises(GitError):
            fast_forward(ws, RUN, tip_a)
        self.assertEqual(head(ws, RUN), tip_b)

    def test_fast_forward_moves_a_run_branch_checked_out_in_the_workspace(self):
        repo, ws, base = self.workspace(use_worktree=False)
        run_git(ws, "checkout", "--quiet", "-b", "side")
        tip = commit(ws, {"s.txt": "s\n"}, "side")
        run_git(ws, "checkout", "--quiet", RUN)
        self.assertEqual(fast_forward(ws, RUN, tip), tip)
        self.assertEqual(head(ws), tip)
        self.assertEqual(run_git(ws, "status", "--porcelain"), "")
        self.assertTrue((ws / "s.txt").exists())

    def test_needs_integration_after_independent_accept(self):
        repo, ws, x = self.workspace()
        a, base_a, _ = self.card(ws, 1, {"a.txt": "a\n"})
        b, base_b, tip_b = self.card(ws, 2, {"b.txt": "b\n"})
        self.assertEqual((base_a, base_b), (x, x))
        self.assertFalse(needs_integration(ws, RUN, a))
        fast_forward(ws, RUN, tip_b)
        self.assertTrue(needs_integration(ws, RUN, a))
        self.assertFalse(needs_integration(ws, RUN, b))

    def test_is_ancestor(self):
        repo, ws, x = self.workspace()
        a, _, tip_a = self.card(ws, 1, {"a.txt": "a\n"})
        self.assertTrue(is_ancestor(ws, x, tip_a))
        self.assertTrue(is_ancestor(ws, tip_a, tip_a))
        self.assertFalse(is_ancestor(ws, tip_a, x))

    def test_rebase_onto_success(self):
        repo, ws, x = self.workspace()
        a, _, _ = self.card(ws, 1, {"a.txt": "a\n"})
        _, _, tip_b = self.card(ws, 2, {"b.txt": "b\n"})
        fast_forward(ws, RUN, tip_b)
        self.assertEqual(rebase_onto(ws, a, RUN), [])
        self.assertFalse(needs_integration(ws, RUN, a))
        self.assertEqual(changed_paths(ws, RUN, a), ["a.txt"])
        self.assertEqual(fast_forward(ws, RUN, a), head(ws, a))

    def test_rebase_onto_conflict_returns_paths_and_leaves_branch_unchanged(self):
        repo, ws, x = self.workspace()
        a, _, tip_a = self.card(ws, 1, {"shared file.txt": "from a\n", "only a.txt": "a\n"})
        _, _, tip_b = self.card(ws, 2, {"shared file.txt": "from b\n"})
        fast_forward(ws, RUN, tip_b)
        self.assertEqual(rebase_onto(ws, a, RUN), ["shared file.txt"])
        self.assertEqual(head(ws, a), tip_a)
        self.assertEqual(current_branch(ws), a)
        self.assertEqual(run_git(ws, "status", "--porcelain"), "")
        for state in ("rebase-merge", "rebase-apply"):
            p = Path(run_git(ws, "rev-parse", "--path-format=absolute", "--git-path", state))
            self.assertFalse(p.exists(), state)

    # -- violations ------------------------------------------------------

    def test_detect_violation_head_moved(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"a.txt": "a\n"})
        self.assertIsNone(detect_violation(ws, h, branch))
        commit(ws, {"r.txt": "reviewer\n"}, "reviewer commit")
        self.assertEqual(detect_violation(ws, h, branch), Violation(True, False, ()))
        run_git(ws, "reset", "--quiet", "--hard", h)
        run_git(ws, "checkout", "--quiet", "--detach", h)
        self.assertEqual(detect_violation(ws, h, branch), Violation(True, False, ()))

    def test_detect_violation_index_dirty(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"a.txt": "a\n"})
        write(ws, {"a.txt": "staged\n"})
        run_git(ws, "add", "a.txt")
        self.assertEqual(detect_violation(ws, h, branch), Violation(False, True, ()))

    def test_detect_violation_tracked_change(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"a.txt": "a\n"})
        write(ws, {"a.txt": "edited\n"})
        (ws / "README.md").unlink()
        self.assertEqual(detect_violation(ws, h, branch), Violation(False, False, ("README.md", "a.txt")))

    def test_untracked_build_output_is_not_a_violation(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"a.txt": "a\n"})
        write(ws, {"build/out.o": "bin\n", "new file.txt": "x\n"})
        (ws / "a.txt").touch()  # stat-only change
        self.assertIsNone(detect_violation(ws, h, branch))

    # -- preserve and restore -------------------------------------------

    def test_preserve_keeps_staged_and_unstaged_versions_separately(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        write(ws, {"f.txt": "A\n"})
        run_git(ws, "add", "f.txt")
        write(ws, {"f.txt": "B\n"})
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(run_git(ws, "rev-parse", "refs/leadhand/saved/1"), saved)
        self.assertEqual(run_git(ws, "show", f"{saved}:f.txt"), "B")
        self.assertEqual(run_git(ws, "show", f"{saved}^2:f.txt"), "A")
        self.assertEqual(run_git(ws, "rev-parse", f"{saved}^1"), h)
        self.assertEqual((ws / "f.txt").read_text(), "orig\n")
        self.assertEqual(run_git(ws, "stash", "list"), "")  # refs/stash untouched

    def test_preserve_keeps_staged_change_whose_working_copy_was_restored(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        write(ws, {"f.txt": "A\n"})
        run_git(ws, "add", "f.txt")
        write(ws, {"f.txt": "orig\n"})
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(run_git(ws, "show", f"{saved}^2:f.txt"), "A")
        self.assertEqual(run_git(ws, "show", f"{saved}:f.txt"), "orig")
        self.assertIsNone(detect_violation(ws, h, branch))

    def test_restore_leaves_expected_head_and_clean_index(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})

        # HEAD moved by a commit and nothing else: the saved commit is the moved HEAD.
        moved = commit(ws, {"f.txt": "committed\n"}, "reviewer commit")
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(saved, moved)
        self.assertEqual(head(ws), h)
        self.assertIsNone(detect_violation(ws, h, branch))

        # Moved to another branch with a commit, a staged change and an untracked file.
        run_git(ws, "checkout", "--quiet", "-b", "elsewhere")
        moved = commit(ws, {"g.txt": "g\n"}, "elsewhere")
        write(ws, {"f.txt": "staged\n", "scratch.log": "keep me\n"})
        run_git(ws, "add", "f.txt")
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/2", expected_head=h, expected_branch=branch)
        self.assertEqual(run_git(ws, "rev-parse", f"{saved}^1"), moved)
        self.assertEqual(current_branch(ws), branch)
        self.assertEqual(head(ws), h)
        self.assertEqual(head(ws, branch), h)
        self.assertIsNone(detect_violation(ws, h, branch))
        self.assertEqual(run_git(ws, "diff", "--cached", "--name-only"), "")
        self.assertEqual((ws / "scratch.log").read_text(), "keep me\n")
        self.assertFalse((ws / "g.txt").exists())

    def test_preserve_refuses_a_ref_outside_refs(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        write(ws, {"f.txt": "edited\n"})
        with self.assertRaises(GitError):
            preserve_and_restore(ws, ref="SAVED", expected_head=h, expected_branch=branch)
        self.assertEqual((ws / "f.txt").read_text(), "edited\n")

    def test_commits_created_by_library_are_never_signed(self):
        repo, ws, _ = self.workspace()
        # Fixture commits pass -c commit.gpgsign=false; the library must do the same.
        run_git(repo, "config", "commit.gpgsign", "true")
        run_git(repo, "config", "gpg.program", "false")  # any signing attempt fails
        a, _, _ = self.card(ws, 1, {"a.txt": "a\n"})
        _, _, tip_b = self.card(ws, 2, {"b.txt": "b\n"})
        fast_forward(ws, RUN, tip_b)
        self.assertEqual(rebase_onto(ws, a, RUN), [])
        h = head(ws)
        write(ws, {"a.txt": "edited\n"})
        run_git(ws, "add", "a.txt")
        preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=a)

    def ref_exists(self, cwd, ref):
        return run_git(cwd, "rev-parse", "--verify", "--quiet", ref, check=False) != ""

    def git_path_exists(self, ws, name):
        return Path(run_git(ws, "rev-parse", "--path-format=absolute", "--git-path", name)).exists()

    def test_preserve_keeps_a_moved_branch_tip_when_head_is_detached(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        stray = commit(ws, {"f.txt": "stray\n"}, "reviewer commit on the card branch")
        run_git(ws, "checkout", "--quiet", "--detach", h)
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(saved, h)
        self.assertEqual(run_git(ws, "rev-parse", "refs/leadhand/saved/1-branch"), stray)
        self.assertEqual(current_branch(ws), branch)
        self.assertEqual(head(ws, branch), h)
        self.assertIsNone(detect_violation(ws, h, branch))

    def test_preserve_keeps_a_moved_branch_tip_when_another_branch_is_checked_out(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        stray = commit(ws, {"f.txt": "stray\n"}, "reviewer commit on the card branch")
        run_git(ws, "checkout", "--quiet", RUN)
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(saved, head(ws, RUN))
        self.assertEqual(run_git(ws, "rev-parse", "refs/leadhand/saved/1-branch"), stray)
        self.assertEqual(current_branch(ws), branch)
        self.assertEqual(head(ws), h)

    def test_preserve_recreates_a_deleted_branch(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        run_git(ws, "checkout", "--quiet", RUN)
        run_git(ws, "branch", "--quiet", "-D", branch)
        preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(current_branch(ws), branch)
        self.assertEqual(head(ws, branch), h)
        self.assertFalse(self.ref_exists(ws, "refs/leadhand/saved/1-branch"))
        self.assertIsNone(detect_violation(ws, h, branch))

    def test_preserve_does_not_keep_a_branch_tip_already_in_the_saved_commit(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        moved = commit(ws, {"f.txt": "committed\n"}, "reviewer commit")
        write(ws, {"f.txt": "dirty\n"})
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(run_git(ws, "rev-parse", f"{saved}^1"), moved)
        self.assertFalse(self.ref_exists(ws, "refs/leadhand/saved/1-branch"))

    def test_preserve_saves_and_clears_a_conflicted_merge(self):
        repo, ws, _ = self.workspace()
        branch, _, h0 = self.card(ws, 1, {"f.txt": "orig\n"})
        run_git(ws, "checkout", "--quiet", "-b", "side")
        side = commit(ws, {"f.txt": "side\n"}, "side")
        run_git(ws, "checkout", "--quiet", branch)
        h = commit(ws, {"f.txt": "card\n"}, "card")
        # Signing must not be attempted for the fallback commit either.
        run_git(repo, "config", "commit.gpgsign", "true")
        run_git(repo, "config", "gpg.program", "false")
        run_git(ws, "merge", "--no-edit", "side", check=False)
        self.assertTrue(run_git(ws, "ls-files", "-u"))  # unmerged index

        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertEqual(run_git(ws, "rev-parse", "refs/leadhand/saved/1"), saved)
        self.assertEqual(run_git(ws, "rev-parse", f"{saved}^1"), h)
        self.assertEqual(run_git(ws, "rev-parse", f"{saved}^2"), side)
        self.assertIn("<<<<<<<", run_git(ws, "show", f"{saved}:f.txt"))
        self.assertFalse(self.git_path_exists(ws, "MERGE_HEAD"))
        self.assertEqual(current_branch(ws), branch)
        self.assertIsNone(detect_violation(ws, h, branch))
        self.assertEqual((ws / "f.txt").read_text(), "card\n")
        self.assertEqual(run_git(ws, "stash", "list"), "")

    def test_preserve_saves_and_clears_a_conflicted_rebase_and_cherry_pick(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "card\n"})
        run_git(ws, "checkout", "--quiet", "-b", "other", RUN)
        pick1 = commit(ws, {"f.txt": "other\n"}, "other 1")
        pick2 = commit(ws, {"g.txt": "g\n"}, "other 2")
        run_git(ws, "checkout", "--quiet", branch)

        run_git(ws, "rebase", "other", check=False)
        self.assertTrue(self.git_path_exists(ws, "rebase-merge") or self.git_path_exists(ws, "rebase-apply"))
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertIn("<<<<<<<", run_git(ws, "show", f"{saved}:f.txt"))
        self.assertFalse(self.git_path_exists(ws, "rebase-merge"))
        self.assertFalse(self.git_path_exists(ws, "rebase-apply"))
        self.assertEqual(current_branch(ws), branch)
        self.assertIsNone(detect_violation(ws, h, branch))

        run_git(ws, "cherry-pick", pick1, pick2, check=False)
        self.assertTrue(self.git_path_exists(ws, "sequencer"))
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/2", expected_head=h, expected_branch=branch)
        self.assertIn("<<<<<<<", run_git(ws, "show", f"{saved}:f.txt"))
        self.assertFalse(self.git_path_exists(ws, "sequencer"))
        self.assertFalse(self.git_path_exists(ws, "CHERRY_PICK_HEAD"))
        self.assertIsNone(detect_violation(ws, h, branch))

    def autostash_fixture(self):
        repo, ws, _ = self.workspace()
        branch, _, _ = self.card(ws, 1, {"f.txt": "orig\n", "g.txt": "g\n"})
        run_git(ws, "checkout", "--quiet", "-b", "side")
        commit(ws, {"f.txt": "side\n"}, "side")
        run_git(ws, "checkout", "--quiet", branch)
        h = commit(ws, {"f.txt": "card\n"}, "card")
        write(ws, {"g.txt": "dirty\n"})  # the reviewer's uncommitted change, autostashed
        return ws, branch, h

    def assert_autostash_kept(self, ws, branch, h):
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertIn("<<<<<<<", run_git(ws, "show", f"{saved}:f.txt"))
        self.assertEqual(run_git(ws, "stash", "list"), "")
        self.assertEqual(run_git(ws, "show", "refs/leadhand/saved/1-autostash:g.txt"), "dirty")
        self.assertEqual(current_branch(ws), branch)
        self.assertIsNone(detect_violation(ws, h, branch))
        self.assertEqual((ws / "g.txt").read_text(), "g\n")

        # A later violation under the same ref drops the stale sibling.
        write(ws, {"g.txt": "again\n"})
        preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertFalse(self.ref_exists(ws, "refs/leadhand/saved/1-autostash"))
        self.assertEqual(run_git(ws, "stash", "list"), "")

    def test_preserve_keeps_a_conflicted_merge_autostash(self):
        ws, branch, h = self.autostash_fixture()
        run_git(ws, "merge", "--autostash", "--no-edit", "side", check=False)
        self.assertTrue(self.ref_exists(ws, "MERGE_AUTOSTASH"))
        self.assert_autostash_kept(ws, branch, h)
        self.assertFalse(self.ref_exists(ws, "MERGE_AUTOSTASH"))

    def test_preserve_keeps_a_conflicted_rebase_autostash(self):
        ws, branch, h = self.autostash_fixture()
        run_git(ws, "rebase", "--autostash", "side", check=False)
        self.assertTrue((self.git_path_exists(ws, "rebase-merge/autostash")
                         or self.git_path_exists(ws, "rebase-apply/autostash")))
        self.assert_autostash_kept(ws, branch, h)
        self.assertFalse(self.git_path_exists(ws, "rebase-merge"))

    def test_preserve_drops_a_stale_branch_sibling(self):
        repo, ws, _ = self.workspace()
        branch, _, h = self.card(ws, 1, {"f.txt": "orig\n"})
        commit(ws, {"f.txt": "stray\n"}, "reviewer commit on the card branch")
        run_git(ws, "checkout", "--quiet", "--detach", h)
        preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertTrue(self.ref_exists(ws, "refs/leadhand/saved/1-branch"))
        write(ws, {"f.txt": "edited\n"})
        preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=h, expected_branch=branch)
        self.assertFalse(self.ref_exists(ws, "refs/leadhand/saved/1-branch"))

    def test_prepare_workspace_without_worktree_refuses_uncommitted_changes(self):
        repo = make_repo(self.tmp)
        base = run_git(repo, "rev-parse", "HEAD")
        write(repo, {"README.md": "local edit\n"})
        with self.assertRaises(GitError):
            prepare_workspace(repo, run_branch=RUN, base_sha=base, slug="run-1", use_worktree=False)
        run_git(repo, "add", "README.md")
        write(repo, {"README.md": "hello\n"})  # staged only
        with self.assertRaises(GitError):
            prepare_workspace(repo, run_branch=RUN, base_sha=base, slug="run-1", use_worktree=False)
        self.assertEqual(current_branch(repo), "main")
        self.assertFalse(self.ref_exists(repo, f"refs/heads/{RUN}"))

    def test_prepare_workspace_rejects_unsafe_slugs(self):
        repo = make_repo(self.tmp)
        base = run_git(repo, "rev-parse", "HEAD")
        for slug in ("", "a/b", ".hidden", "..", "../escape"):
            with self.subTest(slug=slug), self.assertRaises(GitError):
                prepare_workspace(repo, run_branch=RUN, base_sha=base, slug=slug, use_worktree=True)
        self.assertFalse((repo / ".worktrees").exists())
        self.assertFalse(self.ref_exists(repo, f"refs/heads/{RUN}"))

    def test_default_base_after_a_run_without_worktree(self):
        repo, ws, base = self.workspace(use_worktree=False)
        self.assertEqual(current_branch(repo), RUN)
        self.assertEqual(default_base(repo), ("main", base))

        repo = make_repo(self.tmp, name="trunk repo", default_branch="trunk")
        trunk = run_git(repo, "rev-parse", "HEAD")
        run_git(repo, "config", "init.defaultBranch", "trunk")
        run_git(repo, "branch", "main")  # init.defaultBranch outranks main
        prepare_workspace(repo, run_branch=RUN, base_sha=trunk, slug="run-1", use_worktree=False)
        self.assertEqual(default_base(repo), ("trunk", trunk))

    # -- misc ------------------------------------------------------------

    def test_paths_with_spaces(self):
        repo, ws, base = self.workspace()
        self.assertIn(" ", str(ws))
        branch, _, tip = self.card(ws, 1, {"my dir/a file.txt": "a\n", "café notes.md": "n\n"})
        self.assertEqual(changed_paths(ws, base, tip), ["café notes.md", "my dir/a file.txt"])
        write(ws, {"my dir/a file.txt": "edited\n"})
        self.assertEqual(detect_violation(ws, tip, branch), Violation(False, False, ("my dir/a file.txt",)))
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=tip, expected_branch=branch)
        self.assertEqual(run_git(ws, "show", f"{saved}:my dir/a file.txt"), "edited")
        self.assertIsNone(detect_violation(ws, tip, branch))

    def test_git_helpers_refuse_paths_outside_the_fixture(self):
        here = Path(__file__).resolve().parent  # inside the project's own checkout
        # Should the guard regress, leave nothing behind in tests/ or the temp dir.
        self.addCleanup(lambda: (here / "escaped-fixture-file.txt").unlink(missing_ok=True))
        self.addCleanup(shutil.rmtree, here / "escaped repo", ignore_errors=True)
        self.addCleanup(lambda: (self.tmp.parent / "escaped.txt").unlink(missing_ok=True))
        for cwd in (None, here, self.tmp.parent, Path("/")):
            with self.subTest(cwd=cwd), self.assertRaises(AssertionError):
                run_git(cwd, "status")
        with self.assertRaises(AssertionError):
            write(here, {"escaped-fixture-file.txt": "x\n"})
        self.assertFalse((here / "escaped-fixture-file.txt").exists())
        with self.assertRaises(AssertionError):
            write(self.tmp, {"../escaped.txt": "x\n"})
        self.assertFalse((self.tmp.parent / "escaped.txt").exists())
        with self.assertRaises(AssertionError):
            make_repo(here, name="escaped repo")
        self.assertFalse((here / "escaped repo").exists())
        self.assertEqual(os.environ["GIT_CEILING_DIRECTORIES"], str(self.tmp.resolve().parent))

    def test_git_refuses_a_missing_workspace_directory(self):
        # Read-only commands only: before the fix, None ran them in the process cwd.
        repo = make_repo(self.tmp)
        not_a_dir = repo / "README.md"
        # "", "." and relative paths would resolve against the process cwd.
        for cwd in (None, self.tmp / "missing", not_a_dir, "", Path(""), Path("."), ".", Path("tests"), "my repo"):
            with self.subTest(cwd=cwd):
                with self.assertRaises(GitError):
                    git(cwd, "rev-parse", "HEAD")
                with self.assertRaises(GitError):
                    head(cwd)
                with self.assertRaises(GitError):
                    detect_violation(cwd, "0" * 40, "main")

    def test_git_error_includes_stderr(self):
        repo = make_repo(self.tmp)
        with self.assertRaises(GitError) as cm:
            git(repo, "rev-parse", "--verify", "no-such-ref")
        self.assertIn("no-such-ref", str(cm.exception))
        self.assertEqual(git(repo, "rev-parse", "--abbrev-ref", "HEAD"), "main")
        self.assertEqual(git_common_dir(repo), (repo / ".git").resolve())

    def test_is_test_or_doc_path(self):
        yes = [
            "tests/test_x.py", "test/a.c", "spec/a_spec.rb", "docs/guide.html", "tests/deep/helper.py",
            "test_foo.py", "src/foo_test.go", "src/foo.test.ts", "src/foo.spec.js",
            "README.md", "notes/usage.rst", "notes/todo.txt",
        ]
        # Ruling R13: directory rules are top-level only; dependency and build inputs are production.
        no = [
            "src/main.py", "scripts/build.sh", "src/contest.py", "src/latest.py", "docs", "tests", "src/tester.py",
            "pkg/tests/helper.py", "pkg/spec/x.py", "app/docs/views.py",
            "requirements.txt", "requirements-dev.txt", "tests/requirements.txt", "constraints.txt",
            "CMakeLists.txt", "src/CMakeLists.txt",
        ]
        for p in yes:
            self.assertTrue(is_test_or_doc_path(p), p)
        for p in no:
            self.assertFalse(is_test_or_doc_path(p), p)


class SubdirectoryWorkspaceTests(unittest.TestCase):
    """An in-place run started with `--repo <checkout>/src` works in that subdirectory, while
    Git lists paths relative to the checkout's top: staging and preserving must take them so."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="leadhand gitops sub "))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        hermetic(self, self.tmp)
        self.repo = make_repo(self.tmp)
        commit(self.repo, {"src/a.py": "A = 1\n"}, "src")
        self.ws = self.repo / "src"
        self.head = run_git(self.repo, "rev-parse", "HEAD")

    def staged(self) -> list[str]:
        return run_git(self.repo, "diff", "--cached", "--name-only", "HEAD").splitlines()

    def test_stage_work_takes_a_new_file_in_the_subdirectory(self):
        write(self.ws, {"new.py": "NEW = 1\n", "pkg/deep.py": "DEEP = 1\n"})
        write(self.repo, {"src/a.py": "A = 2\n"})
        new = untracked_files(self.ws)
        self.assertEqual(sorted(new), ["src/new.py", "src/pkg/deep.py"])
        staged, gitlinks = stage_work(self.ws, new_paths=new)
        self.assertTrue(staged)
        self.assertEqual(gitlinks, [])
        self.assertEqual(sorted(self.staged()), ["src/a.py", "src/new.py", "src/pkg/deep.py"])

    def test_stage_work_leaves_out_a_gitlink_in_the_subdirectory(self):
        nested = self.ws / "sub"
        nested.mkdir()
        run_git(nested, "init", "--quiet")
        commit(nested, {"x.txt": "first\n"}, "nested")
        run_git(self.repo, "add", "src/sub")
        run_git(self.repo, "commit", "--quiet", "-m", "nested")
        commit(nested, {"x.txt": "moved\n"}, "move the nested repository")
        staged, gitlinks = stage_work(self.ws, new_paths=[])
        self.assertEqual(gitlinks, ["src/sub"])
        self.assertFalse(staged)
        self.assertEqual(self.staged(), [])

    def test_preserve_keeps_and_removes_untracked_files_named_from_the_top(self):
        write(self.ws, {"new.py": "NEW = 1\n", "pkg/deep.py": "DEEP = 1\n"})
        write(self.repo, {"src/a.py": "A = 2\n"})
        saved = preserve_and_restore(self.ws, ref="refs/leadhand/saved/1", expected_head=self.head,
                                     expected_branch="main", untracked=["src/new.py", "src/pkg/deep.py"])
        self.assertEqual(run_git(self.repo, "show", f"{saved}:src/new.py"), "NEW = 1")
        self.assertEqual(run_git(self.repo, "show", f"{saved}:src/pkg/deep.py"), "DEEP = 1")
        self.assertEqual(run_git(self.repo, "show", f"{saved}:src/a.py"), "A = 2")
        self.assertFalse((self.ws / "new.py").exists())
        self.assertFalse((self.ws / "pkg").exists())
        self.assertEqual((self.ws / "a.py").read_text(), "A = 1\n")
        self.assertEqual(run_git(self.repo, "status", "--porcelain"), "")

    def test_preserve_leaves_untracked_files_outside_the_workspace_alone(self):
        write(self.repo, {"other/user notes.txt": "the operator's\n"})
        write(self.ws, {"new.py": "NEW = 1\n"})
        saved = preserve_and_restore(self.ws, ref="refs/leadhand/saved/1", expected_head=self.head,
                                     expected_branch="main", untracked=["other/user notes.txt", "src/new.py"])
        self.assertEqual(run_git(self.repo, "show", f"{saved}:src/new.py"), "NEW = 1")
        self.assertNotIn("other/user notes.txt",
                         run_git(self.repo, "ls-tree", "-r", "--name-only", saved).splitlines())
        self.assertEqual((self.repo / "other" / "user notes.txt").read_text(), "the operator's\n")
        self.assertFalse((self.ws / "new.py").exists())

    def test_stage_work_never_takes_a_new_file_outside_the_workspace(self):
        write(self.repo, {"other/user notes.txt": "the operator's\n"})
        write(self.ws, {"new.py": "NEW = 1\n"})
        staged, _ = stage_work(self.ws, new_paths=["other/user notes.txt", "src/new.py"])
        self.assertTrue(staged)
        self.assertEqual(self.staged(), ["src/new.py"])

    def test_cleanliness_probes_see_the_whole_checkout_whatever_diff_relative_says(self):
        # Codex 4546501970: diff.relative=true scopes `git diff` to the cwd and names paths from it.
        run_git(self.repo, "config", "diff.relative", "true")
        write(self.repo, {"README.md": "uncommitted at the top\n"})
        with self.assertRaises(GitError) as cm:
            prepare_workspace(self.ws, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        self.assertIn("uncommitted changes to tracked files", str(cm.exception))
        v = detect_violation(self.ws, self.head, "main")
        self.assertEqual(v.tracked_changes, ("README.md",))
        run_git(self.repo, "add", "README.md")
        self.assertTrue(detect_violation(self.ws, self.head, "main").index_dirty)
        moved = commit(self.repo, {"README.md": "committed at the top\n"}, "top")
        self.assertEqual(changed_paths(self.ws, self.head, moved), ["README.md"])

    def test_patches_keep_the_standard_prefixes_whatever_the_users_diff_settings(self):
        moved = commit(self.repo, {"src/a.py": "A = 2\n"}, "change")
        for key, value in (("diff.noprefix", "true"), ("diff.mnemonicPrefix", "true"),
                           ("diff.srcPrefix", "old/"), ("diff.dstPrefix", "new/")):
            run_git(self.repo, "config", key, value)
        patch = git(self.ws, "diff", "--no-ext-diff", "--no-color", self.head, moved, "--")
        self.assertIn("diff --git a/src/a.py b/src/a.py\n", patch)
        self.assertIn("--- a/src/a.py\n+++ b/src/a.py\n", patch)

    def test_preserve_never_removes_the_workspace_folder_itself(self):
        ws = self.repo / "fresh"                  # holds nothing tracked
        write(ws, {"only.py": "ONLY = 1\n"})
        saved = preserve_and_restore(ws, ref="refs/leadhand/saved/1", expected_head=self.head,
                                     expected_branch="main", untracked=["fresh/only.py"])
        self.assertEqual(run_git(self.repo, "show", f"{saved}:fresh/only.py"), "ONLY = 1")
        self.assertFalse((ws / "only.py").exists())
        self.assertTrue(ws.is_dir())


class OperatorFilesInPlaceTests(unittest.TestCase):
    """An in-place run works in the operator's checkout: Git's checkout and merge overwrite an
    ignored file by default, and rebase cannot be told not to (Codex 4546946539); an edit
    behind assume-unchanged or skip-worktree hides from every cleanliness check (4546946532)."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="leadhand gitops operator "))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        hermetic(self, self.tmp)
        self.repo = make_repo(self.tmp)
        self.old = commit(self.repo, {"local.env": "historic\n"}, "track local.env")
        run_git(self.repo, "rm", "--quiet", "--cached", "local.env")
        run_git(self.repo, "commit", "--quiet", "-m", "stop tracking local.env")
        self.head = run_git(self.repo, "rev-parse", "HEAD")
        (git_common_dir(self.repo) / "info" / "exclude").write_text("local.env\n")
        (self.repo / "local.env").write_text("the operator's settings\n")

    def assert_kept(self, branch="main"):
        self.assertEqual((self.repo / "local.env").read_text(), "the operator's settings\n")
        self.assertEqual(current_branch(self.repo), branch)

    def test_start_refuses_a_base_that_would_overwrite_an_ignored_file(self):
        with self.assertRaises(GitError) as cm:
            prepare_workspace(self.repo, run_branch=RUN, base_sha=self.old, slug="run-1", use_worktree=False)
        self.assertIn("local.env", str(cm.exception))
        self.assert_kept()
        self.assertEqual(run_git(self.repo, "branch", "--list", RUN), "")

    def test_switching_to_a_card_branch_never_overwrites_an_ignored_file(self):
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        run_git(self.repo, "branch", f"{RUN}--card-1", self.old)       # a reused card branch tracking it
        with self.assertRaises(GitError) as cm:
            cut_card_branch(self.repo, RUN, 1)
        self.assertIn("local.env", str(cm.exception))
        self.assert_kept(RUN)

    def test_fast_forward_never_overwrites_an_ignored_file(self):
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        tracked = run_git(self.repo, "commit-tree", run_git(self.repo, "rev-parse", f"{self.old}^{{tree}}"),
                          "-p", self.head, "-m", "track it again")
        run_git(self.repo, "checkout", "--quiet", RUN)
        with self.assertRaises(GitError) as cm:
            fast_forward(self.repo, RUN, tracked)
        self.assertIn("local.env", str(cm.exception))
        self.assert_kept(RUN)
        self.assertEqual(run_git(self.repo, "rev-parse", RUN), self.head)

    def test_rebase_never_overwrites_an_ignored_file(self):
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        card_branch, _ = cut_card_branch(self.repo, RUN, 1)
        commit(self.repo, {"src/card.py": "CARD = 1\n"}, "card 1")
        onto = run_git(self.repo, "commit-tree", run_git(self.repo, "rev-parse", f"{self.old}^{{tree}}"),
                       "-p", self.head, "-m", "the run tip tracks it again")
        before = run_git(self.repo, "rev-parse", card_branch)
        with self.assertRaises(GitError) as cm:
            rebase_onto(self.repo, card_branch, onto)
        self.assertIn("local.env", str(cm.exception))
        self.assert_kept(card_branch)
        self.assertEqual(run_git(self.repo, "rev-parse", card_branch), before)

    def commit_elsewhere(self, parent: str, files: dict, remove=(), message="elsewhere") -> str:
        """A commit on `parent` made in a scratch worktree, never in the operator's checkout."""
        wt = self.tmp / f"scratch {len(list(self.tmp.iterdir()))}"
        run_git(self.repo, "worktree", "add", "--quiet", "--detach", str(wt), parent)
        write(wt, files)
        if files:
            run_git(wt, "add", "--force", "--", *files)
        if remove:
            run_git(wt, "rm", "--quiet", "--", *remove)
        run_git(wt, "commit", "--quiet", "--allow-empty", "-m", message)
        sha = run_git(wt, "rev-parse", "HEAD")
        run_git(self.repo, "worktree", "remove", "--force", str(wt))
        return sha

    def test_rebase_never_overwrites_an_ignored_file_an_intermediate_commit_adds(self):
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        card_branch, _ = cut_card_branch(self.repo, RUN, 1)
        adds = self.commit_elsewhere(self.head, {"local.env": "a worker's\n"}, message="add it")
        removes = self.commit_elsewhere(adds, {}, remove=["local.env"], message="drop it")
        run_git(self.repo, "update-ref", f"refs/heads/{card_branch}", removes)
        onto = self.commit_elsewhere(self.head, {"src/run.py": "RUN = 1\n"}, message="the run moved on")
        with self.assertRaises(GitError) as cm:
            rebase_onto(self.repo, card_branch, onto)
        self.assertIn("local.env", str(cm.exception))
        self.assert_kept(card_branch)
        self.assertEqual(run_git(self.repo, "rev-parse", card_branch), removes)

    def ignored_cache_file(self):
        with (git_common_dir(self.repo) / "info" / "exclude").open("a") as f:
            f.write("cache\n")
        (self.repo / "cache").write_text("the operator's cache file\n")

    def test_rebase_never_replaces_an_ignored_file_with_a_folder(self):
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        card_branch, _ = cut_card_branch(self.repo, RUN, 1)
        commit(self.repo, {"src/card.py": "CARD = 1\n"}, "card 1")
        self.ignored_cache_file()
        onto = self.commit_elsewhere(self.head, {"cache/t": "tracked\n"}, message="the run tracks cache/")
        with self.assertRaises(GitError) as cm:
            rebase_onto(self.repo, card_branch, onto)
        self.assertIn("cache", str(cm.exception))
        self.assertEqual((self.repo / "cache").read_text(), "the operator's cache file\n")

    def test_start_refuses_a_folder_over_an_ignored_file_before_changing_anything(self):
        self.ignored_cache_file()
        base = self.commit_elsewhere(self.head, {"cache/t": "tracked\n"}, message="tracks cache/")
        exclude = (git_common_dir(self.repo) / "info" / "exclude").read_text()
        with self.assertRaises(GitError) as cm:
            prepare_workspace(self.repo, run_branch=RUN, base_sha=base, slug="run-1", use_worktree=False)
        self.assertIn("cache", str(cm.exception))
        self.assertEqual((self.repo / "cache").read_text(), "the operator's cache file\n")
        self.assertEqual(run_git(self.repo, "branch", "--list", RUN), "")
        self.assertEqual((git_common_dir(self.repo) / "info" / "exclude").read_text(), exclude)

    def test_a_folder_of_tracked_files_a_rebase_turns_into_a_file_is_not_in_the_way(self):
        commit(self.repo, {"cfg/a": "tracked\n"}, "cfg is a folder")
        self.head = run_git(self.repo, "rev-parse", "HEAD")
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        card_branch, _ = cut_card_branch(self.repo, RUN, 1)
        commit(self.repo, {"src/card.py": "CARD = 1\n"}, "card 1")
        wt = self.tmp / "scratch cfg"
        run_git(self.repo, "worktree", "add", "--quiet", "--detach", str(wt), self.head)
        run_git(wt, "rm", "--quiet", "-r", "cfg")
        write(wt, {"cfg": "now a file\n"})
        run_git(wt, "add", "cfg")
        run_git(wt, "commit", "--quiet", "-m", "cfg is a file")
        onto = run_git(wt, "rev-parse", "HEAD")
        run_git(self.repo, "worktree", "remove", "--force", str(wt))
        self.assertEqual(rebase_onto(self.repo, card_branch, onto), [])
        self.assertEqual((self.repo / "cfg").read_text(), "now a file\n")
        self.assert_kept(card_branch)

    def test_rebase_never_replaces_a_folder_holding_an_ignored_file_with_a_file(self):
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        card_branch, _ = cut_card_branch(self.repo, RUN, 1)
        commit(self.repo, {"src/card.py": "CARD = 1\n"}, "card 1")
        with (git_common_dir(self.repo) / "info" / "exclude").open("a") as f:
            f.write("cache/\n")
        write(self.repo, {"cache/mine.txt": "the operator's\n"})
        onto = self.commit_elsewhere(self.head, {"cache": "a tracked file\n"}, message="cache is a file")
        with self.assertRaises(GitError) as cm:
            rebase_onto(self.repo, card_branch, onto)
        self.assertIn("cache", str(cm.exception))
        self.assertEqual((self.repo / "cache" / "mine.txt").read_text(), "the operator's\n")

    def test_a_case_only_rename_is_not_in_the_way_on_a_case_insensitive_file_system(self):
        probe = self.tmp / "Case Probe"
        probe.write_text("x")
        insensitive = (self.tmp / "case probe").exists()
        probe.unlink()
        if not insensitive:
            self.skipTest("the file system here is case-sensitive; a case-only rename is two paths")
        commit(self.repo, {"Notes.md": "notes\n"}, "notes")
        self.head = run_git(self.repo, "rev-parse", "HEAD")
        wt = self.tmp / "scratch rename"
        run_git(self.repo, "worktree", "add", "--quiet", "--detach", str(wt), self.head)
        run_git(wt, "mv", "Notes.md", "NOTES.md")
        run_git(wt, "commit", "--quiet", "-m", "rename the notes")
        renamed = run_git(wt, "rev-parse", "HEAD")
        run_git(self.repo, "worktree", "remove", "--force", str(wt))
        prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        run_git(self.repo, "branch", f"{RUN}--card-1", renamed)
        card_branch, _ = cut_card_branch(self.repo, RUN, 1)
        onto = self.commit_elsewhere(self.head, {"src/run.py": "RUN = 1\n"}, message="the run moved on")
        self.assertEqual(rebase_onto(self.repo, card_branch, onto), [])
        self.assertEqual(run_git(self.repo, "ls-files", "--", "NOTES.md"), "NOTES.md")
        self.assert_kept(card_branch)

    def test_a_worktree_run_may_overwrite_an_ignored_file_it_holds(self):
        # In a worktree run an ignored file is a worker's or a build's: never the operator's.
        wt = self.tmp / "run worktree"
        run_git(self.repo, "worktree", "add", "--quiet", "-b", RUN, str(wt), self.head)
        run_git(self.repo, "branch", f"{RUN}--card-1", self.old)
        (wt / "local.env").write_text("a worker's leftover\n")
        cut_card_branch(wt, RUN, 1, protect_ignored=False)
        self.assertEqual((wt / "local.env").read_text(), "historic\n")
        self.assertEqual(current_branch(wt), f"{RUN}--card-1")

    def test_a_conflicted_merge_is_refused_as_uncommitted_changes(self):
        side = self.commit_elsewhere(self.head, {"README.md": "theirs\n"}, message="theirs")
        commit(self.repo, {"README.md": "ours\n"}, "ours")
        run_git(self.repo, "merge", "--quiet", side, check=False)
        with self.assertRaises(GitError) as cm:
            prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        self.assertIn("uncommitted changes", str(cm.exception))
        self.assertNotIn("hidden", str(cm.exception))
        run_git(self.repo, "merge", "--abort")

    def test_a_sparse_checkout_is_refused_in_place_with_short_advice(self):
        commit(self.repo, {f"far/f{i}.txt": f"{i}\n" for i in range(30)}, "many files")
        run_git(self.repo, "sparse-checkout", "set", "--no-cone", "/README.md")
        with self.assertRaises(GitError) as cm:
            prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1", use_worktree=False)
        message = str(cm.exception)
        self.assertIn("sparse", message)
        self.assertIn("run.worktree = true", message)
        self.assertNotIn("--no-skip-worktree", message)
        self.assertLess(len(message), 600, message)

    def test_start_refuses_hidden_index_entries_in_place(self):
        for flag in ("--assume-unchanged", "--skip-worktree"):
            with self.subTest(flag=flag):
                write(self.repo, {"README.md": "an edit hidden from status\n"})
                run_git(self.repo, "update-index", flag, "README.md")
                with self.assertRaises(GitError) as cm:
                    prepare_workspace(self.repo, run_branch=RUN, base_sha=self.head, slug="run-1",
                                      use_worktree=False)
                self.assertIn("README.md", str(cm.exception))
                self.assertIn(f"git update-index --no-{flag[2:]}", str(cm.exception))
                self.assertEqual(run_git(self.repo, "branch", "--list", RUN), "")
                run_git(self.repo, "update-index", f"--no-{flag[2:]}", "README.md")
                run_git(self.repo, "checkout", "--quiet", "--", "README.md")


if __name__ == "__main__":
    unittest.main()
