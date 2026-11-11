"""Attacks from the security review (security-review.md), reproduced with fakes and scratch
repositories. Each test is the reviewer's scenario; it must fail closed."""
import contextlib
import json
import os
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path
from unittest import mock

from gitfixture import make_repo, run_git, write
from leadhand import dispatch as dsp
from leadhand import procs
from test_dispatch import CLI, FAKES, Base, alive


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


class Anchored(Base):
    """A real run (run.json) for the repo, and a second repository the worker must never reach."""

    def setUp(self):
        super().setUp()
        self.victim = make_repo(self.tmp / "victim")
        self.victim_head = run_git(self.victim, "rev-parse", "HEAD")
        self.scenario("ready")

    def cli(self, *args, session=True, **env):
        """Run the CLI as a worker in the supervisor's session would (LEADHAND_RUN_DIR set), or
        with it unset (`session=False`)."""
        e = cli_env(**env)
        if session:
            e["LEADHAND_RUN_DIR"] = str(self.run_dir)
        else:
            e.pop("LEADHAND_RUN_DIR", None)
        return subprocess.run([sys.executable, "-B", str(CLI), *map(str, args)], env=e, capture_output=True,
                              text=True, timeout=60, stdin=subprocess.DEVNULL)

    def dispatch_args(self, *, run=None, cwd=None, brief=None, report=None, did="d1"):
        cwd = self.repo if cwd is None else cwd
        folder = cwd / ".leadhand" / "1" / did
        folder.mkdir(parents=True, exist_ok=True)
        if brief is None:
            brief = folder / "brief.md"
            brief.write_text("# Brief\n")
        return ["dispatch", "--run", run or self.run_dir, "--dispatch-id", did, "--card", "1", "--attempt", "1",
                "--role", "implementer", "--model", "gpt-6-luna", "--effort", "low", "--brief", brief,
                "--report", report or folder / "report.md", "--cwd", cwd, "--kind", "implement",
                "--branch", "main"]

    def assert_refused(self, r):
        self.assertNotIn(r.returncode, (0, dsp.EXIT_STILL_RUNNING), r.stdout + r.stderr)
        self.assertIn("refused", r.stderr)

    def assert_nothing_ran(self):
        self.assertEqual(self.records("start"), [], "a Codex worker was started")
        self.assertEqual(run_git(self.victim, "rev-parse", "HEAD"), self.victim_head)

    def decoy(self):
        p = subprocess.Popen(["sleep", "60"], start_new_session=True, stdin=subprocess.DEVNULL)
        self.addCleanup(lambda: (p.kill(), p.wait()))
        return {"pid": p.pid, "pgid": p.pid, "start": procs.process_start_time(p.pid)}

    def forged_folder(self, root: Path, did="y"):
        """A worker folder under `root` whose runner.json names a live decoy process group and whose
        request.json commits in the victim repository."""
        wd = root / "workers" / did
        wd.mkdir(parents=True)
        rec = self.decoy()
        req = self.request(cwd=self.victim, kind="implement", branch="main", run_dir=root, dispatch_id=did)
        (wd / "request.json").write_text(json.dumps(dsp.request_to_json(req)))
        (wd / "runner.json").write_text(json.dumps({"runner": rec, "codex": rec, "generation": 1}))
        write(self.victim, {"stray.env": "SECRET=1\n"})
        return wd, rec


class H2ConfinedDispatch(Anchored):
    """H2: `leadhand dispatch` acts only on the session's run, in its workspace."""

    def test_cwd_in_another_repository_is_refused(self):
        r = self.cli(*self.dispatch_args(cwd=self.victim))
        self.assert_refused(r)
        self.assertFalse((self.worker_dir() / "request.json").exists())
        self.assert_nothing_ran()

    def test_run_dir_other_than_the_sessions_is_refused(self):
        forged = self.repo / ".leadhand" / "fake"
        forged.mkdir(parents=True)
        (forged / "run.json").write_text(json.dumps(
            {"run_id": "fake", "repo": str(self.victim), "workspace": str(self.victim)}))
        r = self.cli(*self.dispatch_args(run=forged, cwd=self.victim))
        self.assert_refused(r)
        self.assertFalse((forged / "workers").exists())
        self.assert_nothing_ran()

    def test_without_the_session_variable_a_dir_without_run_json_is_refused(self):
        forged = self.repo / ".leadhand" / "fake"
        forged.mkdir(parents=True)
        r = self.cli(*self.dispatch_args(run=forged), session=False)
        self.assert_refused(r)
        self.assertFalse((forged / "workers").exists())
        self.assert_nothing_ran()

    def test_without_the_session_variable_a_run_owned_by_a_live_supervisor_is_refused(self):
        rec = self.decoy()
        (self.run_dir / "holder.json").write_text(json.dumps(
            {"run_id": self.run_dir.name, "supervisor_pid": rec["pid"], "supervisor_start": rec["start"]}))
        r = self.cli(*self.dispatch_args(), session=False)
        self.assert_refused(r)
        self.assert_nothing_ran()

    def test_run_dir_inside_its_own_workspace_is_refused(self):
        inside = self.repo / ".leadhand" / "run"
        inside.mkdir(parents=True)
        (inside / "run.json").write_text(json.dumps(
            {"run_id": "inside", "repo": str(self.repo), "workspace": str(self.repo)}))
        r = self.cli(*self.dispatch_args(run=inside), session=False)
        self.assert_refused(r)
        self.assert_nothing_ran()

    def test_brief_or_report_outside_the_workspace_scratch_dir_is_refused(self):
        outside = self.tmp / "evil brief.md"
        outside.write_text("# Brief\n")
        self.assert_refused(self.cli(*self.dispatch_args(brief=outside)))
        self.assert_refused(self.cli(*self.dispatch_args(report=self.repo / "report.md")))
        self.assert_nothing_ran()

    def test_the_sessions_run_in_its_workspace_is_accepted(self):
        r = self.cli(*self.dispatch_args())
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.records("start")), 1)

    def test_the_runs_evidence_base_worktree_is_accepted_as_cwd(self):
        base = self.repo / ".worktrees" / f"{self.run_dir.name}--base"
        run_git(self.repo, "worktree", "add", "--quiet", "--detach", str(base), "HEAD")
        r = self.cli(*self.dispatch_args(cwd=base))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.records("start")), 1)


class M1ConfinedStop(Anchored):
    """M1 (and H1/H2's runner half): stop-dispatch and dispatch-runner act only on worker
    folders of the session's run, and signal only what those record."""

    def assert_untouched(self, rec):
        time.sleep(0.3)
        self.assertTrue(alive(rec["pid"]), "a process outside the run was signalled")
        self.assertEqual(run_git(self.victim, "rev-parse", "HEAD"), self.victim_head)
        self.assertIn("stray.env", run_git(self.victim, "status", "--porcelain"))

    def test_stop_dispatch_refuses_a_forged_folder_outside_the_run(self):
        fk = self.repo / ".leadhand" / "fk"
        _, rec = self.forged_folder(fk)
        for session in (True, False):
            self.assert_refused(self.cli("stop-dispatch", fk, "--dispatch", "y", session=session))
        self.assert_untouched(rec)

    def test_stop_dispatch_refuses_a_dispatch_id_that_leaves_the_workers_folder(self):
        fk = self.repo / ".leadhand" / "fk"
        wd, rec = self.forged_folder(fk)
        (self.run_dir / "workers").mkdir()
        escape = os.path.relpath(wd, self.run_dir / "workers")
        self.assert_refused(self.cli("stop-dispatch", self.run_dir, "--dispatch", escape))
        self.assert_untouched(rec)

    def test_stop_dispatch_refuses_a_worker_folder_that_is_a_symlink_out_of_the_run(self):
        fk = self.repo / ".leadhand" / "fk"
        wd, rec = self.forged_folder(fk)
        (self.run_dir / "workers").mkdir()
        (self.run_dir / "workers" / "y").symlink_to(wd)
        self.assert_refused(self.cli("stop-dispatch", self.run_dir, "--dispatch", "y"))
        self.assert_untouched(rec)

    def test_stop_dispatch_refuses_a_request_for_another_repository(self):
        # A worker folder inside the run whose request.json was forged to name the victim.
        wd, rec = self.forged_folder(self.run_dir)
        self.assert_refused(self.cli("stop-dispatch", self.run_dir, "--dispatch", "y"))
        self.assert_untouched(rec)

    def test_runner_commit_refuses_a_request_for_another_repository(self):
        wd, _ = self.forged_folder(self.run_dir)
        out = dsp._commit_work(wd, dsp._load_request(wd))
        self.assertIsNone(out["committed"])
        self.assertIn("refused", out["commit_error"])
        self.assertEqual(run_git(self.victim, "rev-parse", "HEAD"), self.victim_head)
        self.assertIn("stray.env", run_git(self.victim, "status", "--porcelain"))

    def test_runner_refuses_a_folder_outside_the_run(self):
        fk = self.repo / ".leadhand" / "fk"
        wd, _ = self.forged_folder(fk)
        (wd / "runner.json").unlink()
        for session in (True, False):
            self.assert_refused(self.cli("dispatch-runner", wd, session=session))
        self.assertFalse((wd / "result.json").exists())
        self.assert_nothing_ran()

    def test_runner_refuses_a_request_for_another_repository(self):
        wd, _ = self.forged_folder(self.run_dir)
        (wd / "runner.json").unlink()
        self.assert_refused(self.cli("dispatch-runner", wd))
        self.assertFalse((wd / "result.json").exists())
        self.assert_nothing_ran()


if __name__ == "__main__":
    unittest.main()
