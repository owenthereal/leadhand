"""Configuration: presets, the user and repository files, validation, the settings this version
refuses, and the snapshot `start` takes.

Converted from tests/test_config.py and tests/test_config_snapshot.py. The effective
configuration is read from the run's `config.json` snapshot after `start`, from `route`, or
from `check`'s problem lines.
"""
import json
import unittest
from pathlib import Path

from harness import read_json, read_jsonl
from supervisor import FakeSupervisor, card

NOT_SUPPORTED = "not supported in this version: "


def refusals(result) -> list[str]:
    """`check`'s "not supported in this version" lines, in order."""
    return [line for line in result.stdout.splitlines() if NOT_SUPPORTED in line]


class ConfigTestCase(unittest.TestCase):
    def world(self, **kw) -> FakeSupervisor:
        return FakeSupervisor(self, [card(1)], **kw)

    def repo_config(self, w, text):
        (w.repo / ".leadhand.toml").write_text(text, encoding="utf-8")

    def check(self, w):
        return w.cli("check", w.deck_path, "--repo", w.repo)

    def snapshot(self, w) -> dict:
        """The effective configuration, as the run's snapshot records it."""
        w.start()
        return read_json(w.run_dir / "config.json")

    def route(self, w, role, *flags):
        r = w.cli("route", w.deck_path, "1", role, "--repo", w.repo, *flags).ok()
        lines = r.stdout.splitlines()
        self.assertTrue(lines, r)
        return lines[0], lines[1:]


class Config(ConfigTestCase):
    def test_default_preset_matches_spec(self):
        w = self.world()
        self.assertEqual(self.route(w, "implementer", "--installed", "claude,codex"),
                         ("codex gpt-6.1-terra high", ["vendor codex: prefer"]))
        snap = self.snapshot(w)
        roles = snap["roles"]
        self.assertEqual(roles["implementer"]["codex"], "gpt-6.1-terra high")
        self.assertEqual(roles["implementer"]["prefer"], "codex")
        self.assertEqual(roles["reviewer"]["prefer"], "other")
        self.assertEqual(roles["dealer"]["prefer"], "claude")
        self.assertEqual(set(roles["dealer"]) - {"prefer"}, {"claude"})
        self.assertEqual(snap["run"]["branch_prefix"], "leadhand/")
        self.assertEqual(snap["after_run"]["step"], "none")
        self.assertEqual(snap["capacity"]["codex_pause_percent"], 95)
        self.assertIs(snap["codex"]["network"], False)
        self.assertEqual(snap["limits"]["implementer"], {"steps": 150, "minutes": 60})
        self.assertEqual(snap["prices"]["opus"]["cache_write_1h"], 8)
        self.assertEqual(snap["prices"]["haiku"]["cache_read"], 0.01)
        self.assertEqual(snap["notify"]["command"], "")

    def test_repo_file_overrides_user_file_key_by_key(self):
        w = self.world(config='profile = "lean"\n[run]\nbranch_prefix = "u/"\ntest_first = "off"\n')
        self.repo_config(w, '[run]\nbranch_prefix = "r/"\n[roles.implementer]\nclaude = "opus low"\n')
        snap = self.snapshot(w)
        self.assertEqual(snap["run"]["branch_prefix"], "r/")
        self.assertEqual(snap["run"]["test_first"], "off")
        self.assertEqual(snap["profile"], "lean")
        self.assertEqual(snap["roles"]["implementer"]["claude"], "opus low")
        self.assertEqual(snap["roles"]["implementer"]["codex"].split(" ")[0], "gpt-6.1-terra")

    def test_preset_key_selects_claude_only(self):
        w = self.world()
        self.repo_config(w, 'preset = "claude-only"\n')
        snap = self.snapshot(w)
        self.assertEqual(set(snap["roles"]["implementer"]) - {"prefer"}, {"claude"})
        self.assertEqual(snap["roles"]["final_reviewer"]["claude"], "fable medium")
        self.assertEqual(snap["roles"]["implementer"]["prefer"], "claude")

    def test_invalid_effort_raises_config_error_naming_key(self):
        w = self.world()
        self.repo_config(w, '[roles.implementer]\nclaude = "sonnet extreme"\n')
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stdout.startswith("error: "), r)
        self.assertIn("roles.implementer.claude", r.stdout)

    def test_unknown_top_level_key_raises(self):
        w = self.world()
        self.repo_config(w, 'bogus = 1\n')
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stdout.startswith("error: "), r)
        self.assertIn("bogus", r.stdout)

    def test_docs_dir_expands_repo_and_home(self):
        # The default docs_dir, ~/.leadhand/{repo}: the run directory lands under it.
        w = self.world()
        home = w.tmp / "home dir"
        home.mkdir()
        w.env["HOME"] = str(home)
        w.config_file.write_text("", encoding="utf-8")
        run_dir = w.start()
        docs = home / ".leadhand" / "my repo"
        self.assertTrue(run_dir.resolve().is_relative_to(docs.resolve()), run_dir)

    def test_after_run_step_validation(self):
        w = self.world()
        self.repo_config(w, '[after_run]\nstep = "none"\n')
        r = self.check(w)
        self.assertEqual(r.code, 0, r)
        self.assertNotIn("after_run.step", r.stdout)
        for ok in ("pr", "skill:x", "command:x"):
            with self.subTest(step=ok):
                # accepted, then refused by this version under its own value
                self.repo_config(w, f'[after_run]\nstep = "{ok}"\n')
                r = self.check(w)
                self.assertEqual(refusals(r), [f"error: {NOT_SUPPORTED}after_run.step = {ok}"], r)
        self.repo_config(w, '[after_run]\nstep = "merge"\n')
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stdout.startswith("error: "), r)
        self.assertIn("after_run.step", r.stdout)
        self.assertEqual(refusals(r), [], r)

    def test_unsupported_settings_lists_each_rejection(self):
        w = self.world()
        r = self.check(w)
        self.assertEqual(r.code, 0, r)
        self.assertEqual(refusals(r), [], r)
        self.repo_config(w, '[after_run]\nstep = "pr"\non_incomplete = "draft-pr"\n[run]\ncost_limit_usd = 5\n')
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(refusals(r), [
            "error: not supported in this version: after_run.step = pr",
            "error: not supported in this version: after_run.on_incomplete = draft-pr",
            "error: not supported in this version: run.cost_limit_usd = 5",
        ], r)

    def test_step_limits_claude_workers_cannot_honour_are_rejected(self):
        # Ruling R52: a Claude worker's turn cap is its agent file's maxTurns.
        w = self.world()
        for preset in ("claude-codex", "claude-only"):
            with self.subTest(preset=preset):
                self.repo_config(w, f'preset = "{preset}"\n')
                r = self.check(w)
                self.assertEqual(r.code, 0, r)
                self.assertEqual(refusals(r), [], r)
        self.repo_config(w, '[limits]\nreviewer = { steps = 80, minutes = 20 }\nscout = { steps = 30, minutes = 5 }\n')
        r = self.check(w)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(refusals(r), [
            "error: not supported in this version: limits.reviewer.steps = 80 "
            "(Claude workers use the agent file's maxTurns = 60)"], r)

    def test_leadhand_config_env_names_the_user_file(self):
        w = self.world()
        other = w.tmp / "other user config.toml"
        other.write_text('profile = "thorough"\n', encoding="utf-8")
        w.env["LEADHAND_CONFIG"] = str(other)
        _, reasons = self.route(w, "implementer", "--installed", "claude,codex")
        self.assertIn("profile thorough: +1", reasons)


class Snapshot(ConfigTestCase):
    """The run's configuration is snapshotted at `start` (final review I5): `<run>/config.json`
    holds the effective configuration, and the run reads only that afterwards."""

    def test_start_writes_the_snapshot_and_the_engine_reads_only_it(self):
        w = self.world(config='[codex]\nnetwork = false\nwritable_dirs = []\n')
        w.start()
        snap = json.loads((w.run_dir / "config.json").read_text())
        self.assertEqual(snap["codex"], {"network": False, "writable_dirs": []})
        self.assertIn("limits", snap)
        self.assertIn("roles", snap)
        self.assertIn("prices", snap)
        w.config_file.write_text(w.config_file.read_text() + '\n[run]\ncard_review = "nonsense"\n')
        self.assertEqual(w.next()["action"], "dispatch")     # the broken live file is never read

    def test_worker_editing_the_repo_config_changes_nothing(self):
        # The reference ran in place, so the worker's .leadhand.toml was the repository's. In a
        # worktree run the worker writes the repository's file by its absolute path.
        w = FakeSupervisor(self, [card(1, route="codex gpt-6-luna low")], installed=("claude", "codex"))
        w.start()
        repo_config = str(w.repo / ".leadhand.toml")
        w.script(1, "implement", {"commit": False, "files": {
            "src/card1/a.txt": "x\n",
            repo_config: '[codex]\nnetwork = true\nwritable_dirs = ["/"]\n'}})
        w.script(1, "review", {"verdict": "defects"})
        w.script(1, "fix", {"commit": False})
        for _ in range(12):
            a = w.next()
            if a["action"] != "dispatch":
                break
            w.act(a)
            if a["kind"] == "fix":
                break
        self.assertTrue(Path(repo_config).exists())
        runs = read_jsonl(w.tmp / "codex log.jsonl")
        argvs = [r["argv"] for r in runs if r.get("argv")]
        self.assertGreaterEqual(len(argvs), 2)
        for argv in argvs:
            self.assertIn("sandbox_workspace_write.network_access=false", argv)
            adds = [argv[i + 1] for i, x in enumerate(argv) if x == "--add-dir"]
            self.assertNotIn("/", adds)


if __name__ == "__main__":
    unittest.main()
