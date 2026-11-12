"""leadhand's own Git calls never run a repository's hooks or fsmonitor command, and the
runner never commits a gitlink (final review I6 and I7, ruling R45)."""
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path

from gitfixture import hermetic, make_repo, run_git, write
from leadhand import dispatch as dsp
from leadhand import gitops
from test_dispatch import Base, CARD_BRANCH


def marker_script(path: Path, marker: Path) -> Path:
    path.write_text(f'#!/bin/sh\necho "ran $0 in $(pwd)" >> "{marker}"\n')
    path.chmod(0o755)
    return path


class EngineGitCalls(unittest.TestCase):
    def setUp(self):
        hermetic(self)
        self.tmp = Path(tempfile.mkdtemp(prefix="lh git safety "))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = make_repo(self.tmp)
        self.marker = self.tmp / "MARKER"
        evil = marker_script(self.tmp / "evil.sh", self.marker)
        # A nested repository with an fsmonitor command, committed as a gitlink.
        sub = self.repo / "sub"
        sub.mkdir()
        run_git(sub, "init", "--quiet", "-b", "main")
        write(sub, {"f.txt": "x\n"})
        run_git(sub, "add", "f.txt")
        run_git(sub, "commit", "--quiet", "-m", "nested")
        run_git(sub, "config", "core.fsmonitor", str(evil))
        run_git(self.repo, "add", "sub")
        run_git(self.repo, "commit", "--quiet", "-m", "add the nested repo")
        # The outer repository's own fsmonitor and hooks.
        run_git(self.repo, "config", "core.fsmonitor", str(evil))
        hooks = Path(run_git(self.repo, "rev-parse", "--path-format=absolute", "--git-path", "hooks"))
        hooks.mkdir(parents=True, exist_ok=True)
        for name in ("post-checkout", "pre-commit", "post-commit", "reference-transaction"):
            marker_script(hooks / name, self.marker)
        self.marker.unlink(missing_ok=True)
        write(sub, {"f.txt": "dirty\n"})               # a dirty nested repo is what triggers fsmonitor

    def test_engine_git_calls_run_no_fsmonitor_or_hook(self):
        repo = self.repo
        h = gitops.head(repo)
        branch = run_git(repo, "symbolic-ref", "--short", "HEAD")
        gitops.detect_violation(repo, h, branch)
        gitops.status_entries(repo)
        gitops.untracked_files(repo)
        gitops.cut_card_branch(repo, branch, 1)
        write(repo, {"README.md": "changed\n"})
        gitops.preserve_and_restore(repo, ref="refs/leadhand/x/1", expected_head=h,
                                    expected_branch=gitops.card_branch_name(branch, 1))
        self.assertFalse(self.marker.exists(), self.marker.read_text() if self.marker.exists() else "")

    def test_safe_options_are_passed_on_every_call(self):
        r = gitops._run(self.repo, ("config", "--get", "core.fsmonitor"))
        self.assertEqual(r.stdout.strip(), "false")
        r = gitops._run(self.repo, ("config", "--get", "core.hooksPath"))
        self.assertEqual(r.stdout.strip(), "/dev/null")
        r = gitops._run(self.repo, ("config", "--get", "diff.ignoreSubmodules"))
        self.assertEqual(r.stdout.strip(), "all")
        r = dsp._git(self.repo, "config", "--get", "core.fsmonitor")
        self.assertEqual(r.stdout.strip(), "false")


class RunnerGitlinks(Base):
    def setUp(self):
        super().setUp()
        run_git(self.repo, "checkout", "--quiet", "-b", CARD_BRANCH)

    def nested(self, name):
        sub = self.repo / name
        sub.mkdir()
        run_git(sub, "init", "--quiet", "-b", "main")
        write(sub, {"f.txt": "x\n"})
        run_git(sub, "add", "f.txt")
        run_git(sub, "commit", "--quiet", "-m", "nested")
        return sub

    def test_runner_never_commits_a_gitlink(self):
        tracked = self.nested("tracked sub")
        run_git(self.repo, "add", "tracked sub")
        run_git(self.repo, "commit", "--quiet", "-m", "a submodule pointer")
        old = run_git(self.repo, "rev-parse", "HEAD:tracked sub")
        req = self.request(kind="implement", branch=CARD_BRANCH)
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        # The worker moves the tracked nested repo, creates another one, and edits a file.
        write(tracked, {"f.txt": "moved\n"})
        run_git(tracked, "commit", "--quiet", "-am", "moved")
        self.nested("new sub")
        write(self.repo, {"src/widget.py": "W = 1\n"})
        out = dsp._commit_work(wd, req)
        self.assertTrue(out["committed"], out)
        tree = run_git(self.repo, "ls-tree", "-r", out["committed"])
        self.assertNotIn("new sub", tree)
        self.assertEqual(run_git(self.repo, "rev-parse", f"{out['committed']}:tracked sub"), old)
        self.assertIn("src/widget.py", tree)
        self.assertEqual(out.get("gitlinks_skipped"), ["tracked sub"])


if __name__ == "__main__":
    unittest.main()
