"""Repository layouts this version refuses, and the in-place layout the reference still accepts.

Converted from tests/test_unsupported_layouts.py: in-repo docs dirs are not supported in this
version (rulings R48, R48'), so `check` errors and `start` refuses before creating anything.

`InPlace` records Python's acceptance of `run.worktree = false`, which deliberate change 1
removes; it also carries the in-place assertion of test_config_snapshot's worker-edit test.
"""
import unittest

from gitfixture import run_git
from harness import reference_only
from supervisor import FakeSupervisor, card

NOT_SUPPORTED = "not supported in this version: "


def branches(w):
    return run_git(w.repo, "for-each-ref", "--format=%(refname)", "refs/heads/").splitlines()


class Unsupported(unittest.TestCase):
    def assert_refused(self, w, docs, expected):
        before = branches(w)
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertIn(f"error: {expected}", r.stdout)
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertIn(expected, r.stderr)
        self.assertFalse(docs.exists(), docs)
        self.assertEqual(branches(w), before)
        self.assertFalse((w.repo / ".worktrees").exists())

    def test_docs_dir_inside_the_repo_is_refused(self):
        w = FakeSupervisor(self, [card(1)])
        docs = w.repo / "runs docs"
        w.config_file.write_text(f'docs_dir = "{docs}"\n', encoding="utf-8")
        self.assert_refused(w, docs, f"not supported in this version: docs_dir inside the repository ({docs.resolve()})")

    def test_relative_docs_dir_is_resolved_against_the_repo(self):
        w = FakeSupervisor(self, [card(1)])
        w.config_file.write_text('docs_dir = "notes/leadhand"\n', encoding="utf-8")
        docs = (w.repo / "notes" / "leadhand").resolve()
        self.assert_refused(w, docs, f"not supported in this version: docs_dir inside the repository ({docs})")

    def test_defaults_are_supported(self):
        w = FakeSupervisor(self, [card(1)])
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 0, r)
        self.assertEqual([line for line in r.stdout.splitlines() if NOT_SUPPORTED in line], [], r)


class InPlace(unittest.TestCase):
    @reference_only
    def test_python_accepts_run_worktree_false(self):
        # Deliberate change 1: Rust refuses this setting; Python runs in the checkout itself.
        w = FakeSupervisor(self, [card(1)], config="[run]\nworktree = false\n")
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 0, r)
        self.assertNotIn("worktree", r.stdout, r)
        w.start()
        self.assertEqual(w.workspace().resolve(), w.repo.resolve())
        self.assertFalse((w.repo / ".worktrees").exists())
        self.assertEqual(w.next()["action"], "dispatch")


if __name__ == "__main__":
    unittest.main()
