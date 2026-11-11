"""Minor fixes from the final whole-branch review."""
import json
import os
import shlex
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from fakeworld import FAKES, FakeWorld, card
from gitfixture import run_git, write
from leadhand import config as cfg_mod
from leadhand import dispatch as dsp
from leadhand import engine
from leadhand.dispatch import CLI_PATH
from leadhand.state import run_paths
from test_dispatch import Base, CARD_BRANCH

CLI_Q = shlex.quote(str(CLI_PATH))


class Timer(unittest.TestCase):
    def test_timer_returns_promptly_when_the_run_is_paused_or_cancelled(self):
        for request in (engine.request_pause, engine.request_cancel):
            w = FakeWorld(self, [card(1)])
            w.start()
            a = w.next()
            w.launch(a)
            out = []
            t = threading.Thread(target=lambda: out.append(engine.timer(w.run_dir, a["dispatch_id"], interval=0.1)),
                                 daemon=True)
            t.start()
            time.sleep(0.3)
            self.assertEqual(out, [])
            request(w.run_dir)
            t.join(timeout=5)
            self.assertFalse(t.is_alive(), f"timer still waiting after {request.__name__}")
            self.assertEqual(out, ["flag"])


class PinnedIntegrate(unittest.TestCase):
    def test_integrate_after_a_codex_pinned_card_runs_on_claude(self):
        w = FakeWorld(self, [card(1, route="codex gpt-6-luna low"), card(2)], installed=("claude", "codex"))
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"status": "QUESTION", "files": {"src/shared.txt": "card one\n"}})
        w.script(2, "implement", {"files": {"src/shared.txt": "card two\n"}})
        w.parks[1] = "decision needed: storage format"
        w.run()
        del w.parks[1]
        engine.unpark(w.run_dir, card=1, note="Use JSON.")
        w.run()
        self.assertNotIn("integration needs a Claude worker", [e["reason"] for e in w.events("parked")])
        self.assertNotIn("pinned vendor not installed", [e["reason"] for e in w.events("parked")])
        integrate = [e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "integrate"]
        self.assertTrue(integrate)
        self.assertEqual(integrate[0]["vendor"], "claude")
        self.assertEqual(sorted(e["card"] for e in w.events("accepted")), [1, 2])


class OperatorCommands(unittest.TestCase):
    def test_pause_cancel_and_handoff_print_the_quoted_absolute_cli(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        root = shlex.quote(str(w.run_dir))
        self.assertIn(" ", str(w.run_dir))
        self.assertIn(f"{CLI_Q} run {root}", engine.request_pause(w.run_dir))
        self.assertIn(f"{CLI_Q} run {root}", engine.request_cancel(w.run_dir))
        w.script(1, "implement", {"status": "BLOCKED", "blocked_by": "usage limit"})
        run_paths(w.run_dir).cancel_flag.unlink()
        done = w.run()[-1]
        self.assertEqual(done["outcome"], "blocked")
        text = w.paths.handoff.read_text()
        self.assertIn(f"{CLI_Q} run {root}", text)
        self.assertIn(f"{CLI_Q} launch-line {root}", text)
        self.assertIn(f"{CLI_Q} unpark {root}", text)

    def test_relaunch_refusal_names_the_quoted_start_command(self):
        w = FakeWorld(self, [card(1)], gates=True)
        w.start()
        w.run()
        r = w.cli("run", w.run_dir, env={"PATH": f"{FAKES}{os.pathsep}{os.environ['PATH']}"})
        self.assertEqual(r.returncode, 1)
        self.assertIn(f"{CLI_Q} start {shlex.quote(str(w.deck_path.resolve()))}", r.stderr)


class Settings(unittest.TestCase):
    def test_notify_and_capacity_are_warned_as_not_supported(self):
        w = FakeWorld(self, [card(1)], config='[notify]\ncommand = "say done"\n[capacity]\ncodex_pause_percent = 80\n')
        config = cfg_mod.load_config(w.repo)
        warnings = cfg_mod.unsupported_warnings(config)
        self.assertIn("not supported in this version: notify.command = say done", warnings)
        self.assertIn("not supported in this version: capacity.codex_pause_percent = 80", warnings)
        self.assertEqual(cfg_mod.unsupported_settings(config), [])
        r = w.cli("check", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("notify.command", r.stdout + r.stderr)
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("not supported in this version: notify.command", r.stderr)

    def test_defaults_are_not_warned(self):
        w = FakeWorld(self, [card(1)])
        self.assertEqual(cfg_mod.unsupported_warnings(cfg_mod.load_config(w.repo)), [])


class Start(unittest.TestCase):
    def test_start_skips_an_old_run_whose_event_log_is_unreadable(self):
        w = FakeWorld(self, [card(1)])
        old = w.start()
        with open(run_paths(old).events, "ab") as f:
            f.write(b"{not json\n")
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("warning", r.stderr)
        self.assertIn(str(old), r.stderr)
        self.assertIn("run:", r.stdout)


class CopyFiles(unittest.TestCase):
    def test_a_symlinked_report_is_not_followed(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        a = w.next()
        worker = w.launch(a)
        secret = w.tmp / "secret.txt"
        secret.write_text("Status: READY\nnot the worker's\n")
        report = Path(a["report"])
        report.parent.mkdir(parents=True, exist_ok=True)
        report.symlink_to(secret)
        ev = engine.record(w.run_dir, "result", dispatch_id=a["dispatch_id"], worker_id=worker,
                           reply="no status here")
        folder = w.run_dir / "cards" / "1" / a["dispatch_id"]
        self.assertFalse((folder / "report.md").exists())
        self.assertNotIn("report", ev["files"])
        self.assertTrue(any("symlink" in n or "regular file" in n for n in ev.get("files_skipped") or []), ev)
        self.assertEqual(ev["status"], "BLOCKED")


class RunnerFinalize(Base):
    def test_heartbeat_is_touched_while_finalizing(self):
        run_git(self.repo, "checkout", "--quiet", "-b", CARD_BRANCH)
        req = self.request(kind="implement", branch=CARD_BRANCH)
        wd = dsp.write_request(req)
        (wd / "runner.json").write_text(json.dumps({"codex": {"pid": 1, "pgid": 1, "start": "x"}, "generation": 1}))
        beat = wd / "heartbeat"
        beat.write_text("")
        os.utime(beat, (time.time() - 100, time.time() - 100))
        seen = []

        def slow_commit(*args, **kw):
            time.sleep(2.5)
            seen.append(beat.stat().st_mtime)
            return {"committed": None, "changes": False, "commit_skipped": "no changes"}

        with mock.patch.object(dsp, "_commit_work", slow_commit):
            dsp._finalize(wd, req, stopped_for=None, codex_exit=0, tracker=dsp._Tracker(wd, None),
                          started_at=None, beat=beat)
        self.assertGreater(seen[0], time.time() - 10)

    def test_non_utf8_git_output_does_not_kill_the_commit(self):
        run_git(self.repo, "checkout", "--quiet", "-b", CARD_BRANCH)
        msg = self.tmp / "msg"
        msg.write_bytes(b"caf\xe9 \xff\n")
        run_git(self.repo, "-c", "i18n.commitEncoding=latin-1", "commit", "--quiet", "--allow-empty", "-F", str(msg))
        req = self.request(kind="implement", branch=CARD_BRANCH)
        wd = dsp.write_request(req)
        (wd / "commit.json").write_text(json.dumps({"head_before": "0" * 40}))
        write(self.repo, {"src/w.py": "W = 1\n"})
        out = dsp._commit_work(wd, req)
        self.assertTrue(out.get("committed"), out)


if __name__ == "__main__":
    unittest.main()
