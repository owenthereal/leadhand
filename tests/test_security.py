"""Attacks from the security review (security-review.md), reproduced with fakes and scratch
repositories. Each test is the reviewer's scenario; it must fail closed."""
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock

from leadhand import dispatch as dsp
from test_dispatch import CLI, FAKES, Base


def marker_script(path: Path, marker: Path) -> Path:
    path.write_text(f'#!/bin/sh\necho "ran $0" >> "{marker}"\n')
    path.chmod(0o755)
    return path


def cli_env(**overrides) -> dict:
    """The current environment with tests/fakes first on PATH (its `codex` is the fake),
    without any test override of the Codex binary, plus `overrides`."""
    env = dict(os.environ, PATH=f"{FAKES}{os.pathsep}{os.environ['PATH']}")
    env.pop("LEADHAND_TEST_CODEX_BIN", None)
    env.update(overrides)
    return env


class H1ForgedCodexBinary(Base):
    """H1: request.json is never a source of an executable (ruling R49)."""

    def test_runner_never_runs_a_binary_named_in_request_json(self):
        marker = self.tmp / "PWNED"
        evil = marker_script(self.tmp / "evil.sh", marker)
        self.scenario("ready")
        req = self.request()
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        forged = dsp.request_to_json(req)
        forged["codex_bin"] = str(evil)
        (wd / "request.json").write_text(json.dumps(forged))
        r = subprocess.run([sys.executable, "-B", str(CLI), "dispatch-runner", str(wd)], env=cli_env(),
                           capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL)
        self.assertFalse(marker.exists(), f"the forged binary ran: {r.stderr}")
        # The `codex` on PATH (the fake) ran instead.
        self.assertEqual(len(self.records("start")), 1, r.stderr)

    def test_test_override_is_honoured_only_under_leadhand_testing(self):
        req = self.request()
        args = (req, Path("/w/last.md"), Path("/g"), {})
        with mock.patch.dict(os.environ, {"LEADHAND_TEST_CODEX_BIN": "/opt/fake codex", "LEADHAND_TESTING": "0"}):
            self.assertEqual(dsp.codex_argv(*args)[0], "codex")
        with mock.patch.dict(os.environ, {"LEADHAND_TEST_CODEX_BIN": "/opt/fake codex", "LEADHAND_TESTING": "1"}):
            self.assertEqual(dsp.codex_argv(*args)[0], "/opt/fake codex")
        with mock.patch.dict(os.environ, {"LEADHAND_TESTING": "1"}):
            os.environ.pop("LEADHAND_TEST_CODEX_BIN", None)
            self.assertEqual(dsp.codex_argv(*args)[0], "codex")

    def test_request_json_carries_no_executable(self):
        self.assertNotIn("codex_bin", dsp.request_to_json(self.request()))


if __name__ == "__main__":
    unittest.main()
