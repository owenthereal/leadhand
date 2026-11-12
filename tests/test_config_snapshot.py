"""The run's configuration is snapshotted at `start` (final review I5).

`<run>/config.json` holds the effective configuration; the engine, the dispatcher and the
routes read only that snapshot afterwards, so a worker that edits `.leadhand.toml` (or the
user file) mid-run changes nothing for the run."""
import json
import unittest

from fakeworld import FakeWorld, card
from leadhand import config as cfg_mod
from leadhand import engine


class Snapshot(unittest.TestCase):
    def test_snapshot_round_trips(self):
        w = FakeWorld(self, [card(1)])
        live = cfg_mod.load_config(w.repo)
        self.assertEqual(cfg_mod.config_from_dict(json.loads(json.dumps(cfg_mod.config_to_dict(live)))), live)

    def test_start_writes_the_snapshot_and_the_engine_reads_only_it(self):
        w = FakeWorld(self, [card(1)], config='[codex]\nnetwork = false\nwritable_dirs = []\n')
        w.start()
        snap = json.loads((w.run_dir / "config.json").read_text())
        self.assertEqual(snap["codex"], {"network": False, "writable_dirs": []})
        self.assertIn("limits", snap)
        self.assertIn("roles", snap)
        self.assertIn("prices", snap)
        w.config_file.write_text(w.config_file.read_text() + '\n[run]\ncard_review = "nonsense"\n')
        self.assertEqual(w.next()["action"], "dispatch")     # the broken live file is never read

    def test_worker_editing_the_repo_config_changes_nothing(self):
        w = FakeWorld(self, [card(1, route="codex gpt-6-luna low")], installed=("claude", "codex"),
                      config="[run]\nworktree = false\n")
        w.start()
        self.assertEqual(w.workspace().resolve(), w.repo.resolve())
        w.script(1, "implement", {"commit": False, "files": {
            "src/card1/a.txt": "x\n",
            ".leadhand.toml": '[codex]\nnetwork = true\nwritable_dirs = ["/"]\n'}})
        w.script(1, "review", {"verdict": "defects"})
        w.script(1, "fix", {"commit": False})
        for _ in range(12):
            a = w.next()
            if a["action"] != "dispatch":
                break
            w.act(a)
            if a["kind"] == "fix":
                break
        self.assertTrue((w.repo / ".leadhand.toml").exists())
        runs = [json.loads(line) for line in (w.tmp / "codex log.jsonl").read_text().splitlines()]
        argvs = [r["argv"] for r in runs if r.get("argv")]
        self.assertGreaterEqual(len(argvs), 2)
        for argv in argvs:
            self.assertIn("sandbox_workspace_write.network_access=false", argv)
            adds = [argv[i + 1] for i, x in enumerate(argv) if x == "--add-dir"]
            self.assertNotIn("/", adds)


if __name__ == "__main__":
    unittest.main()
