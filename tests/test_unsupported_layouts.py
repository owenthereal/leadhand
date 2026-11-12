"""In-repo docs dirs are not supported in this version (rulings R48, R48'):
`leadhand check` errors and `leadhand start` refuses before creating anything."""
import unittest

from fakeworld import FakeWorld, card
from gitfixture import run_git
from leadhand import config as cfg_mod
from leadhand import engine
from leadhand.engine import EngineError


def branches(w):
    return run_git(w.repo, "for-each-ref", "--format=%(refname)", "refs/heads/").splitlines()


class Unsupported(unittest.TestCase):
    def assert_refused(self, w, docs, expected):
        before = branches(w)
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn(f"error: {expected}", r.stdout)
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn(expected, r.stderr)
        with self.assertRaises(EngineError):
            engine.start(w.deck_path, repo=w.repo)
        self.assertFalse(docs.exists(), docs)
        self.assertEqual(branches(w), before)
        self.assertFalse((w.repo / ".worktrees").exists())

    def test_docs_dir_inside_the_repo_is_refused(self):
        w = FakeWorld(self, [card(1)])
        docs = w.repo / "runs docs"
        w.config_file.write_text(f'docs_dir = "{docs}"\n', encoding="utf-8")
        self.assert_refused(w, docs, f"not supported in this version: docs_dir inside the repository ({docs.resolve()})")

    def test_relative_docs_dir_is_resolved_against_the_repo(self):
        w = FakeWorld(self, [card(1)])
        w.config_file.write_text('docs_dir = "notes/leadhand"\n', encoding="utf-8")
        docs = (w.repo / "notes" / "leadhand").resolve()
        self.assert_refused(w, docs, f"not supported in this version: docs_dir inside the repository ({docs})")

    def test_defaults_are_supported(self):
        w = FakeWorld(self, [card(1)])
        self.assertEqual(cfg_mod.unsupported_settings(cfg_mod.load_config(w.repo), repo=w.repo), [])


if __name__ == "__main__":
    unittest.main()
