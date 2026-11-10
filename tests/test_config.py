import os
import tempfile
import unittest
from pathlib import Path

from leadhand import config as cfg
from leadhand.config import ConfigError, Limits, Route, load_config


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmp, ignore_errors=True))
        self.repo = self.tmp / "my repo"
        self.repo.mkdir()
        self.user = self.tmp / "user.toml"
        saved = os.environ.pop("LEADHAND_CONFIG", None)
        if saved is not None:
            self.addCleanup(os.environ.__setitem__, "LEADHAND_CONFIG", saved)

    def load(self):
        return load_config(self.repo, user_file=self.user)

    def test_default_preset_matches_spec(self):
        c = self.load()
        self.assertEqual(c.roles["implementer"].routes["codex"], Route("codex", "gpt-6.1-terra", "high"))
        self.assertEqual(c.roles["implementer"].prefer, "codex")
        self.assertEqual(c.roles["reviewer"].prefer, "other")
        self.assertEqual(c.roles["dealer"].prefer, "claude")
        self.assertEqual(set(c.roles["dealer"].routes), {"claude"})
        self.assertEqual(c.run.branch_prefix, "leadhand/")
        self.assertEqual(c.after_run.step, "none")
        self.assertEqual(c.codex_pause_percent, 95)
        self.assertIs(c.codex.network, False)
        self.assertEqual(c.limits["implementer"], Limits(150, 60))
        self.assertEqual(c.prices["opus"]["cache_write_1h"], 8)
        self.assertEqual(c.prices["haiku"]["cache_read"], 0.01)
        self.assertEqual(c.notify_command, "")

    def test_repo_file_overrides_user_file_key_by_key(self):
        self.user.write_text('profile = "lean"\n[run]\nbranch_prefix = "u/"\ntest_first = "off"\n')
        (self.repo / ".leadhand.toml").write_text('[run]\nbranch_prefix = "r/"\n[roles.implementer]\nclaude = "opus low"\n')
        c = self.load()
        self.assertEqual(c.run.branch_prefix, "r/")
        self.assertEqual(c.run.test_first, "off")
        self.assertEqual(c.profile, "lean")
        self.assertEqual(c.roles["implementer"].routes["claude"], Route("claude", "opus", "low"))
        self.assertEqual(c.roles["implementer"].routes["codex"].model, "gpt-6.1-terra")

    def test_preset_key_selects_claude_only(self):
        (self.repo / ".leadhand.toml").write_text('preset = "claude-only"\n')
        c = self.load()
        self.assertEqual(set(c.roles["implementer"].routes), {"claude"})
        self.assertEqual(c.roles["final_reviewer"].routes["claude"], Route("claude", "fable", "medium"))
        self.assertEqual(c.roles["implementer"].prefer, "claude")

    def test_invalid_effort_raises_config_error_naming_key(self):
        (self.repo / ".leadhand.toml").write_text('[roles.implementer]\nclaude = "sonnet extreme"\n')
        with self.assertRaises(ConfigError) as cm:
            self.load()
        self.assertIn("roles.implementer.claude", str(cm.exception))

    def test_unknown_top_level_key_raises(self):
        (self.repo / ".leadhand.toml").write_text('bogus = 1\n')
        with self.assertRaises(ConfigError) as cm:
            self.load()
        self.assertIn("bogus", str(cm.exception))

    def test_docs_dir_expands_repo_and_home(self):
        c = self.load()
        self.assertEqual(cfg.docs_dir_for(c, self.repo), Path.home() / ".leadhand" / "my repo")

    def test_after_run_step_validation(self):
        for ok in ("none", "pr", "skill:x", "command:x"):
            (self.repo / ".leadhand.toml").write_text(f'[after_run]\nstep = "{ok}"\n')
            self.assertEqual(self.load().after_run.step, ok)
        (self.repo / ".leadhand.toml").write_text('[after_run]\nstep = "merge"\n')
        with self.assertRaises(ConfigError) as cm:
            self.load()
        self.assertIn("after_run.step", str(cm.exception))

    def test_unsupported_settings_lists_each_rejection(self):
        self.assertEqual(cfg.unsupported_settings(self.load()), [])
        (self.repo / ".leadhand.toml").write_text(
            '[after_run]\nstep = "pr"\non_incomplete = "draft-pr"\n[run]\ncost_limit_usd = 5\n')
        self.assertEqual(cfg.unsupported_settings(self.load()), [
            "not supported in this version: after_run.step = pr",
            "not supported in this version: after_run.on_incomplete = draft-pr",
            "not supported in this version: run.cost_limit_usd = 5",
        ])

    def test_parse_route_and_leadhand_config_env(self):
        self.assertEqual(cfg.parse_route("codex", "gpt-6.1-terra high"), Route("codex", "gpt-6.1-terra", "high"))
        self.user.write_text('profile = "thorough"\n')
        os.environ["LEADHAND_CONFIG"] = str(self.user)
        self.addCleanup(os.environ.pop, "LEADHAND_CONFIG", None)
        self.assertEqual(load_config(self.repo).profile, "thorough")


if __name__ == "__main__":
    unittest.main()
