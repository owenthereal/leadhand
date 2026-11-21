"""Test-only hooks, honoured only under LEADHAND_TESTING=1: the clock offset
(LEADHAND_TEST_CLOCK_OFFSET), the self-path override (LEADHAND_TEST_CLI) and the foreground
wait (LEADHAND_TEST_FOREGROUND_WAIT)."""
from __future__ import annotations

import json
import os
import shlex
import sys
import time
import unittest
from pathlib import Path
from unittest import mock

from fakeworld import FakeWorld, card
from leadhand import dispatch, engine, launch

ROUTER = "/x/router"


class ClockOffset(unittest.TestCase):
    def test_now_adds_offset_only_when_testing(self):
        with mock.patch.dict(os.environ, {"LEADHAND_TESTING": "1", "LEADHAND_TEST_CLOCK_OFFSET": "3600"}):
            self.assertGreaterEqual(engine._now() - time.time(), 3599)
        with mock.patch.dict(os.environ, {"LEADHAND_TEST_CLOCK_OFFSET": "3600"}):
            os.environ.pop("LEADHAND_TESTING", None)
            self.assertLess(abs(engine._now() - time.time()), 5)

    def test_clock_offset_reaches_cli_deadlines(self):
        def first_deadline(env: dict | None) -> float:
            w = FakeWorld(self, [card(1)])
            w.start()
            r = w.cli("next", w.run_dir, env=env)
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
            a = json.loads(r.stdout)
            self.assertEqual(a["action"], "dispatch")
            return engine._parse_time(a["deadline"])

        plain = first_deadline(None)
        shifted = first_deadline({"LEADHAND_TEST_CLOCK_OFFSET": "3600"})
        self.assertGreaterEqual(shifted - plain, 3600)


class SelfPath(unittest.TestCase):
    def test_self_path_override_names_dispatch_line_and_runner(self):
        w = FakeWorld(self, [card(1)], installed=("claude", "codex"))
        w.start()
        with mock.patch.dict(os.environ, {"LEADHAND_TEST_CLI": ROUTER}):
            a = w.next()
            self.assertEqual(a["agent"], "leadhand:codex")
            self.assertTrue(a["message"].startswith(f"{ROUTER} dispatch"), a["message"])
            self.assertEqual(dispatch.self_argv("dispatch-runner", "w"), [ROUTER, "dispatch-runner", "w"])

    def test_cli_path_ignores_override_without_testing(self):
        with mock.patch.dict(os.environ, {"LEADHAND_TEST_CLI": ROUTER}):
            os.environ.pop("LEADHAND_TESTING", None)
            self.assertEqual(dispatch.cli_path(), dispatch.CLI_PATH)
            self.assertEqual(dispatch.self_argv("dispatch-runner", "w"),
                             [sys.executable, "-B", str(dispatch.CLI_PATH), "dispatch-runner", "w"])
        with mock.patch.dict(os.environ, {"LEADHAND_TESTING": "1"}):
            os.environ.pop("LEADHAND_TEST_CLI", None)
            self.assertEqual(dispatch.cli_path(), dispatch.CLI_PATH)
            self.assertEqual(dispatch.self_argv("x"), [sys.executable, "-B", str(dispatch.CLI_PATH), "x"])
        with mock.patch.dict(os.environ, {"LEADHAND_TESTING": "1", "LEADHAND_TEST_CLI": ROUTER}):
            self.assertEqual(dispatch.cli_path(), Path(ROUTER))

    def test_launch_line_uses_override(self):
        w = FakeWorld(self, [card(1)])
        w.start()
        with mock.patch.dict(os.environ, {"LEADHAND_TEST_CLI": ROUTER}):
            self.assertEqual(shlex.split(launch.launch_line(w.run_dir))[:2], [ROUTER, "run"])
            self.assertEqual(shlex.split(engine._run_command(w.paths))[:2], [ROUTER, "run"])
            self.assertIn(f"leadhand CLI: {ROUTER}.", launch.first_message(w.run_dir))
            self.assertEqual(json.loads(launch.cli_settings()),
                             {"sandbox": {"excludedCommands": [f"{ROUTER} *"]}})


class ForegroundWait(unittest.TestCase):
    def test_default_is_540_seconds(self):
        self.assertEqual(dispatch.FOREGROUND_WAIT, 540.0)
        with mock.patch.dict(os.environ, {"LEADHAND_TESTING": "1"}):
            os.environ.pop("LEADHAND_TEST_FOREGROUND_WAIT", None)
            self.assertEqual(dispatch.foreground_wait(), 540.0)
            # Read at call time, so patching the constant still shortens the wait.
            with mock.patch.object(dispatch, "FOREGROUND_WAIT", 0.2):
                self.assertEqual(dispatch.foreground_wait(), 0.2)

    def test_hook_is_honoured_under_testing(self):
        with mock.patch.dict(os.environ, {"LEADHAND_TESTING": "1", "LEADHAND_TEST_FOREGROUND_WAIT": "2.5"}):
            self.assertEqual(dispatch.foreground_wait(), 2.5)

    def test_hook_is_ignored_without_testing(self):
        with mock.patch.dict(os.environ, {"LEADHAND_TEST_FOREGROUND_WAIT": "2.5"}):
            os.environ.pop("LEADHAND_TESTING", None)
            self.assertEqual(dispatch.foreground_wait(), 540.0)
        with mock.patch.dict(os.environ, {"LEADHAND_TESTING": "0", "LEADHAND_TEST_FOREGROUND_WAIT": "2.5"}):
            self.assertEqual(dispatch.foreground_wait(), 540.0)


if __name__ == "__main__":
    unittest.main()
