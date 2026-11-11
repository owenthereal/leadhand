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

from fakeworld import FakeWorld, card
from gitfixture import make_repo, run_git, write
from leadhand import dispatch as dsp
from leadhand import engine, gitops, launch, procs
from test_dispatch import CARD_BRANCH, CLI, FAKES, Base, alive
from test_launch import LaunchTestCase


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
        if session is True:
            e["LEADHAND_RUN_DIR"] = str(self.run_dir)
        elif session is False:
            e.pop("LEADHAND_RUN_DIR", None)
        else:
            e["LEADHAND_RUN_DIR"] = session
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

    def test_an_empty_session_variable_is_refused_not_read_as_unset(self):
        # An emptied anchor is a worker clearing it, never an operator's terminal.
        for blank in ("", " "):
            self.assert_refused(self.cli(*self.dispatch_args(), session=blank))
        self.assert_nothing_ran()
        wd = self.worker_dir("y")
        wd.mkdir(parents=True)
        rec = self.decoy()
        (wd / "request.json").write_text(json.dumps(dsp.request_to_json(self.request("y"))))
        (wd / "runner.json").write_text(json.dumps({"runner": rec, "codex": rec, "generation": 1}))
        self.assert_refused(self.cli("stop-dispatch", self.run_dir, "--dispatch", "y", session=""))
        time.sleep(0.3)
        self.assertTrue(alive(rec["pid"]))
        (wd / "runner.json").unlink()
        self.assert_refused(self.cli("dispatch-runner", wd, session=""))
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


FAKE_LFS = """\
import hashlib, os, re, sys
from pathlib import Path
# A git-lfs stand-in for single-file clean/smudge: clean stores content and prints a pointer;
# smudge prints stored content, prints the pointer as is under GIT_LFS_SKIP_SMUDGE=1 (as
# git-lfs does), and otherwise "fetches" the object from .lfsconfig's URL: logged, then failed.
STORE, LOG = Path(sys.argv[1]), Path(sys.argv[2])
POINTER = b"version https://git-lfs.github.com/spec/v1\\n"
data = sys.stdin.buffer.read()
if sys.argv[3] == "clean":
    if data.startswith(POINTER):
        out = data
    else:
        oid = hashlib.sha256(data).hexdigest()
        STORE.mkdir(exist_ok=True)
        (STORE / oid).write_bytes(data)
        out = POINTER + f"oid sha256:{oid}\\nsize {len(data)}\\n".encode()
else:
    oid = re.search(rb"oid sha256:([0-9a-f]{64})", data)
    if not data.startswith(POINTER) or os.environ.get("GIT_LFS_SKIP_SMUDGE") == "1":
        out = data
    elif oid and (STORE / oid.group(1).decode()).exists():
        out = (STORE / oid.group(1).decode()).read_bytes()
    else:
        url = Path(".lfsconfig").read_text() if Path(".lfsconfig").exists() else ""
        with open(LOG, "a") as f:
            f.write("FETCH " + " ".join(url.split()) + "\\n")
        sys.exit(1)
sys.stdout.buffer.write(out)
"""


class H3FilterDrivers(Base):
    """H3: a worker's .gitattributes and .lfsconfig never make leadhand's own Git calls fetch
    LFS objects (a smudge POSTs to the URL in .lfsconfig, outside every sandbox), while the
    operator's LFS clean filter keeps working (follow-up: no LFS file reads as modified)."""

    def setUp(self):
        super().setUp()
        self.fetch_log = self.tmp / "LFS FETCHES"
        fake = self.tmp / "fake-lfs.py"
        fake.write_text(FAKE_LFS)
        store = self.tmp / "lfs store"
        cmd = f"'{sys.executable}' '{fake}' '{store}' '{self.fetch_log}'"
        self.filters = (f"filter.lfs.clean={cmd} clean", f"filter.lfs.smudge={cmd} smudge",
                        "filter.lfs.required=true")
        # The operator's global configuration, as `git lfs install` writes it (single-file form),
        # isolated to this test.
        config = self.tmp / "global gitconfig"
        config.write_text("[filter \"lfs\"]\n" + "".join(
            f"\t{f.split('=', 1)[0].removeprefix('filter.lfs.')} = {f.split('=', 1)[1]}\n" for f in self.filters))
        self.base_sha = run_git(self.repo, "rev-parse", "HEAD")
        # The worker's commit (made here without any filter, as the runner's commit would be).
        run_git(self.repo, "checkout", "--quiet", "-b", CARD_BRANCH)
        write(self.repo, {".gitattributes": "*.bin filter=lfs diff=lfs merge=lfs -text\n",
                          ".lfsconfig": "[lfs]\n\turl = http://127.0.0.1:9/exfil/SECRET-DATA\n",
                          "p.bin": "version https://git-lfs.github.com/spec/v1\n"
                                   f"oid sha256:{'ab' * 32}\nsize 12\n"})
        run_git(self.repo, "add", "-A")
        run_git(self.repo, "commit", "--quiet", "-m", "worker")
        self.card_sha = run_git(self.repo, "rev-parse", "HEAD")
        run_git(self.repo, "checkout", "--quiet", "main")
        os.environ["GIT_CONFIG_GLOBAL"] = str(config)
        os.environ.pop("GIT_LFS_SKIP_SMUDGE", None)

    def with_filter(self, *args):
        """Plain git with the operator's filter (run_git shuts the global config out)."""
        return run_git(self.repo, *[x for f in self.filters for x in ("-c", f)], *args)

    def assert_no_fetch(self):
        self.assertFalse(self.fetch_log.exists(), self.fetch_log.read_text() if self.fetch_log.exists() else "")

    def test_the_global_filter_fetches_for_plain_git(self):
        # Control: without leadhand's environment, the same checkout fetches from the worker's URL.
        env = {k: v for k, v in os.environ.items() if not k.startswith("LEADHAND")}
        subprocess.run(["git", "checkout", "--quiet", CARD_BRANCH, "--"], cwd=self.repo, env=env,
                       capture_output=True, timeout=60, stdin=subprocess.DEVNULL)
        self.assertIn("127.0.0.1:9/exfil/SECRET-DATA", self.fetch_log.read_text())

    def test_checkout_reset_and_stash_fetch_nothing(self):
        gitops.git(self.repo, "checkout", "--quiet", CARD_BRANCH, "--")       # engine _check_out
        self.assertTrue((self.repo / "p.bin").read_text().startswith("version https://git-lfs"))
        os.unlink(self.repo / "p.bin")                                      # a later worker deletes it
        write(self.repo, {"README.md": "changed\n"})
        gitops.preserve_and_restore(self.repo, ref="refs/leadhand/x/1", expected_head=self.card_sha,
                                    expected_branch=CARD_BRANCH)              # checkout + reset --hard
        self.assertTrue((self.repo / "p.bin").exists())
        gitops.git(self.repo, "checkout", "--quiet", "--force", "--detach", self.base_sha, "--")
        gitops.git(self.repo, "reset", "--quiet", "--hard", self.card_sha)
        gitops.git(self.repo, "stash", "create")
        gitops.status_entries(self.repo)
        self.assert_no_fetch()

    def test_runner_git_calls_fetch_nothing(self):
        gitops.git(self.repo, "checkout", "--quiet", CARD_BRANCH, "--")
        write(self.repo, {"q.bin": "new binary\n", "README.md": "changed\n"})
        req = self.request(kind="implement", branch=CARD_BRANCH)
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        out = dsp._commit_work(wd, req)
        self.assertTrue(out["committed"], out)
        self.assert_no_fetch()

    def test_every_call_carries_the_environment(self):
        for run in (lambda *a: gitops._run(self.repo, a), lambda *a: dsp._git(self.repo, *a)):
            env = run("-c", "alias.showenv=!env", "showenv").stdout
            self.assertIn("GIT_LFS_SKIP_SMUDGE=1", env.splitlines())

    def lfs_checkout(self):
        """An operator's checkout with real LFS content: big.bin committed through the clean
        filter (a pointer in the index), its content in the work tree, its stat data stale."""
        run_git(self.repo, "checkout", "--quiet", CARD_BRANCH)
        write(self.repo, {"big.bin": "large binary content\n"})
        self.with_filter("add", "big.bin")
        self.with_filter("commit", "--quiet", "-m", "big")
        self.assertTrue(run_git(self.repo, "cat-file", "-p", ":big.bin").startswith("version https://git-lfs"))
        old = time.time() - 3600
        os.utime(self.repo / "big.bin", (old, old))       # Git must re-run the clean filter to compare

    def test_checked_out_lfs_content_does_not_read_as_modified(self):
        self.lfs_checkout()
        self.assertEqual(gitops.status_entries(self.repo), (False, []))
        self.assertFalse(gitops._has_changes(self.repo))
        self.assert_no_fetch()

    def test_runner_commits_lfs_files_as_pointers(self):
        self.lfs_checkout()
        write(self.repo, {"big.bin": "changed large content\n"})
        req = self.request(kind="implement", branch=CARD_BRANCH)
        wd = self.worker_dir()
        wd.mkdir(parents=True)
        out = dsp._commit_work(wd, req)
        self.assertTrue(out["committed"], out)
        self.assertTrue(run_git(self.repo, "cat-file", "-p", f"{out['committed']}:big.bin")
                        .startswith("version https://git-lfs"))
        self.assert_no_fetch()


class M3SymlinkedBaseWorktree(unittest.TestCase):
    """M3: in an in-place run <repo>/.worktrees/ is worker-writable; a symlink there must never
    turn the evidence reset (checkout --force, reset --hard, clean -fdx) on another worktree."""

    def setUp(self):
        self.w = FakeWorld(self, [card(1)], config="[run]\nworktree = false\n")
        self.w.start()
        repo = self.w.repo
        self.other = repo / ".worktrees" / "other"
        run_git(repo, "worktree", "add", "--quiet", "-b", "other", str(self.other))
        write(self.other, {"README.md": "uncommitted work\n", "untracked.env": "KEEP=1\n"})

    def assert_other_untouched(self):
        self.assertEqual((self.other / "README.md").read_text(), "uncommitted work\n")
        self.assertTrue((self.other / "untracked.env").exists())
        self.assertEqual(run_git(self.other, "symbolic-ref", "--short", "HEAD"), "other")

    def test_evidence_reset_refuses_a_symlinked_base_worktree(self):
        ctx = engine._load(self.w.run_dir, check_digest=False)
        link = engine._base_worktree_path(ctx)
        link.symlink_to(self.other)
        with self.assertRaises(engine.EngineError) as cm:
            engine._base_worktree(ctx)
        self.assertIn("symlink", str(cm.exception))
        self.assert_other_untouched()

    def test_evidence_reset_refuses_a_symlinked_worktrees_folder(self):
        ctx = engine._load(self.w.run_dir, check_digest=False)
        name = engine._base_worktree_path(ctx).name
        moved = self.w.repo / ".worktrees moved"
        (self.w.repo / ".worktrees").rename(moved)
        self.other = moved / "other"
        run_git(self.w.repo, "worktree", "repair", str(self.other))
        # .worktrees itself is the symlink; the base worktree's own name inside it is too.
        decoy = self.w.repo / "decoy"
        decoy.mkdir()
        (decoy / name).symlink_to(self.other)
        (self.w.repo / ".worktrees").symlink_to(decoy)
        with self.assertRaises(engine.EngineError):
            engine._base_worktree(ctx)
        self.assert_other_untouched()

    def test_prepare_workspace_refuses_a_symlinked_worktree(self):
        repo = self.w.repo
        base = run_git(repo, "rev-parse", "HEAD")
        (repo / ".worktrees" / "slug").symlink_to(self.other)
        with self.assertRaises(gitops.GitError):
            gitops.prepare_workspace(repo, run_branch="leadhand/sym", base_sha=base, slug="slug", use_worktree=True)
        self.assert_other_untouched()


class L1ReplyFile(unittest.TestCase):
    """L1: `record result --reply-file` reads only the dispatch's own reply file."""

    def setUp(self):
        self.w = FakeWorld(self, [card(1)])
        self.w.start()
        self.a = self.w.next()
        self.worker = self.w.launch(self.a)
        self.w.perform(self.a, self.worker)
        self.secret = self.w.tmp / "id_secret"
        self.secret.write_text("SECRET-KEY-MATERIAL\n")

    def record(self, path):
        return self.w.cli("record", self.w.run_dir, "result", "--dispatch", self.a["dispatch_id"],
                          "--worker", self.worker, "--reply-file", path)

    def assert_nothing_copied(self, r):
        self.assertNotEqual(r.returncode, 0, r.stdout)
        self.assertIn("refused", r.stderr)
        self.assertEqual(self.w.events("result_recorded"), [])
        for f in self.w.run_dir.rglob("*"):
            if f.is_file():
                self.assertNotIn("SECRET-KEY-MATERIAL", f.read_text(errors="replace"), f)

    def test_another_path_is_refused(self):
        self.assert_nothing_copied(self.record(self.secret))

    def test_the_reply_file_as_a_symlink_is_refused(self):
        link = Path(self.a["reply_file"])
        link.parent.mkdir(parents=True, exist_ok=True)
        link.unlink(missing_ok=True)
        link.symlink_to(self.secret)
        self.assert_nothing_copied(self.record(link))

    def test_a_symlinked_scratch_folder_is_refused(self):
        reply = Path(self.a["reply_file"])
        folder = reply.parent
        elsewhere = self.w.tmp / "elsewhere"
        elsewhere.mkdir()
        (elsewhere / reply.name).write_text("Status: READY\nSECRET-KEY-MATERIAL\n")
        for f in folder.iterdir():
            os.replace(f, elsewhere / f.name) if not (elsewhere / f.name).exists() else f.unlink()
        folder.rmdir()
        folder.symlink_to(elsewhere)
        self.assert_nothing_copied(self.record(reply))


class L2SymlinkedScratch(unittest.TestCase):
    """L2 (Codex review round 1): an implementer plants a symlink at the next dispatch's scratch
    path; the engine must not write the review brief, nor remove a report, through it."""

    def plant(self, which):
        w = FakeWorld(self, [card(1)])
        outside = w.tmp / "outside"
        outside.mkdir()
        (outside / "report.md").write_text("keep me\n")
        (outside / "reply.txt").write_text("keep me too\n")

        def tamper(world, ws, a):
            scratch = ws / ".leadhand" / "1"
            if which == "dispatch folder":
                (scratch / "c1-a1-review-1").symlink_to(outside, target_is_directory=True)
            elif which == "card folder":
                os.rename(scratch, ws / ".leadhand" / "1-moved")
                scratch.symlink_to(outside, target_is_directory=True)
            else:                               # a file where the dispatch folder should be
                (scratch / "c1-a1-review-1").write_text("not a folder\n")

        w.script(1, "implement", {"tamper": tamper})
        w.start()
        a = w.next()
        self.assertEqual(a["dispatch_id"], "c1-a1-implement-1")
        w.act(a)
        return w, outside

    def assert_refused(self, w, outside, path_part):
        with self.assertRaises(engine.EngineError) as cm:
            w.next()
        self.assertIn(path_part, str(cm.exception))
        self.assertEqual([p for p in outside.rglob("brief.md")], [])
        self.assertEqual((outside / "report.md").read_text(), "keep me\n")
        self.assertEqual((outside / "reply.txt").read_text(), "keep me too\n")
        self.assertEqual([e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "review"], [])
        r = w.cli("next", w.run_dir)
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn(path_part, r.stderr)

    def test_a_symlinked_dispatch_folder_is_refused(self):
        w, outside = self.plant("dispatch folder")
        self.assert_refused(w, outside, str(Path(".leadhand") / "1" / "c1-a1-review-1"))

    def test_a_symlinked_card_folder_is_refused(self):
        w, outside = self.plant("card folder")
        self.assert_refused(w, outside, str(Path(".leadhand") / "1"))

    def test_a_file_in_place_of_the_dispatch_folder_is_refused(self):
        w, outside = self.plant("file")
        self.assert_refused(w, outside, str(Path(".leadhand") / "1" / "c1-a1-review-1"))


class L6ToolsInsideTheWorkspace(LaunchTestCase):
    """L6: workers can edit their workspace, so neither the CLI (which runs outside the sandbox)
    nor the plugin the supervisor loads may live in it."""

    def test_run_refuses_a_cli_inside_the_workspace(self):
        with mock.patch.object(launch, "CLI_PATH", self.workspace / "scripts" / "leadhand"):
            code, _, err = self.run_inprocess()
        self.assertEqual(code, 1)
        self.assertIn("inside the run's workspace", err)
        self.assertEqual(self.starts(), [])

    def test_run_refuses_a_plugin_dir_inside_the_workspace(self):
        code, _, err = self.run_inprocess(plugin_dir=self.workspace / "plugin")
        self.assertEqual(code, 1)
        self.assertIn("inside the run's workspace", err)
        self.assertEqual(self.starts(), [])

    def test_run_accepts_a_plugin_dir_outside_the_workspace(self):
        code, _, err = self.run_inprocess(plugin_dir=self.elsewhere)
        self.assertEqual(code, 0, err)


class L6StartInPlace(unittest.TestCase):
    def test_start_refuses_an_in_place_run_of_the_repository_holding_the_cli(self):
        w = FakeWorld(self, [card(1)], config="[run]\nworktree = false\n")
        with mock.patch.object(launch, "CLI_PATH", w.repo / "scripts" / "leadhand"):
            with self.assertRaises(engine.EngineError) as cm:
                w.start()
        self.assertIn("inside the run's workspace", str(cm.exception))
        self.assertFalse((w.docs / "runs").exists() and any((w.docs / "runs").iterdir()))
        self.assertEqual(run_git(w.repo, "branch", "--list", "leadhand/*"), "")


if __name__ == "__main__":
    unittest.main()
