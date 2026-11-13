#!/usr/bin/env python3
"""Attacks from the security review (docs/compatibility.md, Security), each reproduced with the
fakes and scratch repositories; each must fail closed.

Converted from tests/test_security.py. The dispatch scenarios (H1, H2, M1) run on a run that
`start` made (contract_dispatch.World: a worktree workspace, Claude and Codex installed), with
tests/fakes first on PATH and no LEADHAND_TEST_CODEX_BIN, so the `codex` found on PATH is the
fake; a second repository, the victim, is what a forged request aims at. `dispatch`,
`stop-dispatch` and `dispatch-runner` run with $LEADHAND_RUN_DIR set (a command of the
supervisor's session), unset (an operator's terminal) or emptied, as each test says. A refusal
is a non-zero exit other than 75 with `refused` on stderr.

What a refusal must leave alone is judged from outside the binary: the fake Codex's own log
(whether any Codex started), the victim repository's HEAD and status, a decoy process of the
test's own (alive over a window, by pid and start time), the files under the folders a worker
pointed elsewhere, and Git. The engine scenarios (L1, L2, M3) drive FakeSupervisor; L6 drives
`run` through contract_launch.World. A path the binary uses for itself comes from the testing
hook LEADHAND_TEST_CLI, as contract_launch does (S1-R11).

M3's Python tests ran in place, where <repo>/.worktrees/ is worker-writable; the checks they
cover (the evidence base worktree and the run's own worktree are never reached through a
symlink) apply to worktree runs too, so they are worktree scenarios here. H3 drives whole runs
with an operator's global Git configuration holding a fake git-lfs filter whose smudge logs a
"fetch" unless GIT_LFS_SKIP_SMUDGE=1; the control (plain Git with the same configuration does
fetch from the worker's URL) runs last, inside the same test, so it never feeds the binary's
checks.

GitSafety, at the end, is converted from tests/test_git_safety.py: leadhand's own Git calls run
no hook or fsmonitor command, and the runner never commits a gitlink.
"""
import json
import os
import subprocess
import sys
import time
import unittest
from datetime import date, timedelta
from pathlib import Path

from contract_cards import CODEX_CARD, codex_world
from contract_dispatch import World, holds, wait_for
from contract_finish import base_wt, drive_until_refused, gate_names, marker
from contract_launch import World as LaunchWorld
from gitfixture import commit, make_repo, run_git, write
from harness import FAKES, read_json, run_cli, same_process, started_by
from supervisor import FakeSupervisor, card

SECRET = "SECRET-KEY-MATERIAL"


def alive(rec: dict) -> bool:
    return same_process(rec.get("pid"), rec.get("start"))


# --------------------------------------------------------------------------
# H1, H2, M1: the dispatch commands act only on the session's run

class Anchored:
    """A run `start` made, the dispatch commands on it, and a second repository (the victim)
    a worker must never reach. `cli` runs a command as the CLI found on PATH would with the fake
    `codex` first on PATH and no test override of the Codex binary."""

    def __init__(self, test: unittest.TestCase):
        self.test = test
        self.w = World(test)
        self.run_dir, self.ws, self.repo, self.tmp = self.w.run_dir, self.w.ws, self.w.repo, self.w.tmp
        self.branch = run_git(self.ws, "symbolic-ref", "--short", "HEAD")
        self.victim = make_repo(self.tmp / "victim")
        self.victim_head = run_git(self.victim, "rev-parse", "HEAD")
        self.w.scenario("ready")

    def env(self, session=True) -> dict:
        e = dict(self.w.env, PATH=f"{FAKES}{os.pathsep}{self.w.env['PATH']}")
        e.pop("LEADHAND_TEST_CODEX_BIN", None)
        if session is True:
            e["LEADHAND_RUN_DIR"] = str(self.run_dir)
        elif session is False:
            e.pop("LEADHAND_RUN_DIR", None)
        else:
            e["LEADHAND_RUN_DIR"] = session
        return e

    def cli(self, *args, session=True):
        """Run the CLI as a worker in the supervisor's session would (LEADHAND_RUN_DIR set), with
        it unset (`session=False`), or with it set to the string `session`."""
        return run_cli(*args, env=self.env(session), cwd=self.tmp, timeout=60)

    def dispatch_args(self, *, run=None, cwd=None, brief=None, report=None, did="d1") -> list:
        cwd = self.ws if cwd is None else cwd
        folder = cwd / ".leadhand" / "1" / did
        folder.mkdir(parents=True, exist_ok=True)
        if brief is None:
            brief = folder / "brief.md"
            brief.write_text("# Brief\n", encoding="utf-8")
        return ["dispatch", "--run", run or self.run_dir, "--dispatch-id", did, "--card", "1", "--attempt", "1",
                "--role", "implementer", "--model", "gpt-6-luna", "--effort", "low", "--brief", brief,
                "--report", report or folder / "report.md", "--cwd", cwd, "--kind", "implement",
                "--branch", self.branch]

    def assert_refused(self, r) -> None:
        self.test.assertNotIn(r.code, (0, 75), r)
        self.test.assertIn("refused", r.stderr, r)

    def starts(self) -> list[dict]:
        return self.w.records("start")

    def assert_nothing_ran(self) -> None:
        self.test.assertEqual(self.starts(), [], "a Codex worker was started")
        self.test.assertEqual(run_git(self.victim, "rev-parse", "HEAD"), self.victim_head)

    def decoy(self) -> dict:
        """An unrelated process of the test's own: {pid, pgid, start}, the start time read from
        the process table."""
        p = self.w.decoy()
        start = started_by(p.pid, time.time())
        self.test.assertIsNotNone(start)
        return {"pid": p.pid, "pgid": p.pid, "start": start}

    def victim_request(self, root: Path, did: str) -> dict:
        req = self.w.request_json(did)
        req.update(cwd=str(self.victim), kind="implement", branch="main", run_dir=str(root), dispatch_id=did)
        return req

    def forged_folder(self, root: Path, did="y") -> tuple[Path, dict]:
        """A worker folder under `root` whose runner.json names a live decoy process group and whose
        request.json commits in the victim repository."""
        wd = root / "workers" / did
        wd.mkdir(parents=True)
        rec = self.decoy()
        (wd / "request.json").write_text(json.dumps(self.victim_request(root, did)), encoding="utf-8")
        (wd / "runner.json").write_text(json.dumps({"runner": rec, "codex": rec, "generation": 1}),
                                        encoding="utf-8")
        write(self.victim, {"stray.env": "SECRET=1\n"})
        return wd, rec

    def assert_untouched(self, rec: dict) -> None:
        holds(lambda: alive(rec), 0.3, "the decoy outside the run stays alive")
        self.test.assertEqual(run_git(self.victim, "rev-parse", "HEAD"), self.victim_head)
        self.test.assertIn("stray.env", run_git(self.victim, "status", "--porcelain"))


class H1ForgedCodexBinary(unittest.TestCase):
    """H1: request.json is never a source of an executable (ruling R49)."""

    def test_runner_never_runs_a_binary_named_in_request_json(self):
        s = Anchored(self)
        flag = s.tmp / "PWNED"
        evil = s.tmp / "evil.sh"
        evil.write_text(f'#!/bin/sh\necho "ran $0" >> "{flag}"\n', encoding="utf-8")
        evil.chmod(0o755)
        wd = s.run_dir / "workers" / "d1"
        wd.mkdir(parents=True)
        forged = s.w.request_json("d1")
        forged["codex_bin"] = str(evil)
        (wd / "request.json").write_text(json.dumps(forged), encoding="utf-8")
        r = s.cli("dispatch-runner", wd, session=False)
        self.assertFalse(flag.exists(), f"the forged binary ran\n{r}")
        # The `codex` on PATH (the fake) ran instead.
        self.assertEqual(len(s.starts()), 1, r)

    def test_test_override_is_honoured_only_under_leadhand_testing(self):
        # Observed through the program that runs: an override that leaves a mark, then runs the
        # fake, against the fake found on PATH.
        s = Anchored(self)
        flag = s.tmp / "override ran"
        override = s.tmp / "fake codex override"
        override.write_text(f'#!/bin/sh\necho ran >> "{flag}"\nexec "{FAKES / "codex"}" "$@"\n', encoding="utf-8")
        override.chmod(0o755)
        cases = (("d1", {"LEADHAND_TEST_CODEX_BIN": str(override), "LEADHAND_TESTING": "0"}, False),
                 ("d2", {"LEADHAND_TEST_CODEX_BIN": str(override), "LEADHAND_TESTING": "1"}, True),
                 ("d3", {"LEADHAND_TESTING": "1"}, False))
        for n, (did, overrides, honoured) in enumerate(cases, 1):
            with self.subTest(did=did):
                env = dict(s.env(session=False), **overrides)
                run_cli(*s.dispatch_args(did=did), env=env, cwd=s.tmp, timeout=60).ok()
                self.assertEqual(len(s.starts()), n)
                lines = flag.read_text(encoding="utf-8").splitlines() if flag.exists() else []
                self.assertEqual(len(lines), sum(c[2] for c in cases[:n]), (did, lines))

    def test_request_json_carries_no_executable(self):
        s = Anchored(self)
        s.cli(*s.dispatch_args()).ok()
        request = read_json(s.run_dir / "workers" / "d1" / "request.json")
        self.assertNotIn("codex_bin", request)
        self.assertEqual(len(s.starts()), 1)


class H2ConfinedDispatch(unittest.TestCase):
    """H2: `leadhand dispatch` acts only on the session's run, in its workspace."""

    def setUp(self):
        self.s = Anchored(self)

    def test_cwd_in_another_repository_is_refused(self):
        s = self.s
        s.assert_refused(s.cli(*s.dispatch_args(cwd=s.victim)))
        self.assertFalse((s.run_dir / "workers" / "d1" / "request.json").exists())
        s.assert_nothing_ran()

    def test_run_dir_other_than_the_sessions_is_refused(self):
        s = self.s
        forged = s.ws / ".leadhand" / "fake"
        forged.mkdir(parents=True)
        (forged / "run.json").write_text(json.dumps(
            {"run_id": "fake", "repo": str(s.victim), "workspace": str(s.victim)}), encoding="utf-8")
        s.assert_refused(s.cli(*s.dispatch_args(run=forged, cwd=s.victim)))
        self.assertFalse((forged / "workers").exists())
        s.assert_nothing_ran()

    def test_without_the_session_variable_a_dir_without_run_json_is_refused(self):
        s = self.s
        forged = s.ws / ".leadhand" / "fake"
        forged.mkdir(parents=True)
        s.assert_refused(s.cli(*s.dispatch_args(run=forged), session=False))
        self.assertFalse((forged / "workers").exists())
        s.assert_nothing_ran()

    def test_without_the_session_variable_a_run_owned_by_a_live_supervisor_is_refused(self):
        s = self.s
        rec = s.decoy()
        (s.run_dir / "holder.json").write_text(json.dumps(
            {"run_id": s.w.run_id, "supervisor_pid": rec["pid"], "supervisor_start": rec["start"]}),
            encoding="utf-8")
        s.assert_refused(s.cli(*s.dispatch_args(), session=False))
        s.assert_nothing_ran()

    def test_an_empty_session_variable_is_refused_not_read_as_unset(self):
        # An emptied anchor is a worker clearing it, never an operator's terminal.
        s = self.s
        for blank in ("", " "):
            s.assert_refused(s.cli(*s.dispatch_args(), session=blank))
        s.assert_nothing_ran()
        wd = s.run_dir / "workers" / "y"
        wd.mkdir(parents=True)
        rec = s.decoy()
        (wd / "request.json").write_text(json.dumps(s.w.request_json("y")), encoding="utf-8")
        (wd / "runner.json").write_text(json.dumps({"runner": rec, "codex": rec, "generation": 1}),
                                        encoding="utf-8")
        s.assert_refused(s.cli("stop-dispatch", s.run_dir, "--dispatch", "y", session=""))
        holds(lambda: alive(rec), 0.3, "the process runner.json names stays alive")
        (wd / "runner.json").unlink()
        s.assert_refused(s.cli("dispatch-runner", wd, session=""))
        self.assertFalse((wd / "result.json").exists())
        s.assert_nothing_ran()

    def test_run_dir_inside_its_own_workspace_is_refused(self):
        s = self.s
        inside = s.ws / ".leadhand" / "run"
        inside.mkdir(parents=True)
        (inside / "run.json").write_text(json.dumps(
            {"run_id": "inside", "repo": str(s.repo), "workspace": str(s.ws)}), encoding="utf-8")
        s.assert_refused(s.cli(*s.dispatch_args(run=inside), session=False))
        self.assertFalse((inside / "workers").exists())
        s.assert_nothing_ran()

    def test_brief_or_report_outside_the_workspace_scratch_dir_is_refused(self):
        s = self.s
        outside = s.tmp / "evil brief.md"
        outside.write_text("# Brief\n", encoding="utf-8")
        s.assert_refused(s.cli(*s.dispatch_args(brief=outside)))
        s.assert_refused(s.cli(*s.dispatch_args(report=s.ws / "report.md")))
        s.assert_nothing_ran()

    def test_the_sessions_run_in_its_workspace_is_accepted(self):
        s = self.s
        r = s.cli(*s.dispatch_args())
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(s.starts()), 1)

    def test_the_runs_evidence_base_worktree_is_accepted_as_cwd(self):
        s = self.s
        base = s.repo / ".worktrees" / f"{s.ws.name}--base"
        run_git(s.repo, "worktree", "add", "--quiet", "--detach", str(base), "HEAD")
        r = s.cli(*s.dispatch_args(cwd=base))
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(s.starts()), 1)


class M1ConfinedStop(unittest.TestCase):
    """M1 (and H1/H2's runner half): stop-dispatch and dispatch-runner act only on worker
    folders of the session's run, and signal only what those record."""

    def setUp(self):
        self.s = Anchored(self)

    def test_stop_dispatch_refuses_a_forged_folder_outside_the_run(self):
        s = self.s
        fk = s.ws / ".leadhand" / "fk"
        _, rec = s.forged_folder(fk)
        for session in (True, False):
            s.assert_refused(s.cli("stop-dispatch", fk, "--dispatch", "y", session=session))
        s.assert_untouched(rec)

    def test_stop_dispatch_refuses_a_dispatch_id_that_leaves_the_workers_folder(self):
        s = self.s
        fk = s.ws / ".leadhand" / "fk"
        wd, rec = s.forged_folder(fk)
        (s.run_dir / "workers").mkdir(exist_ok=True)
        escape = os.path.relpath(wd, s.run_dir / "workers")
        s.assert_refused(s.cli("stop-dispatch", s.run_dir, "--dispatch", escape))
        s.assert_untouched(rec)

    def test_stop_dispatch_refuses_a_worker_folder_that_is_a_symlink_out_of_the_run(self):
        s = self.s
        fk = s.ws / ".leadhand" / "fk"
        wd, rec = s.forged_folder(fk)
        (s.run_dir / "workers").mkdir(exist_ok=True)
        (s.run_dir / "workers" / "y").symlink_to(wd)
        s.assert_refused(s.cli("stop-dispatch", s.run_dir, "--dispatch", "y"))
        s.assert_untouched(rec)

    def test_stop_dispatch_refuses_a_request_for_another_repository(self):
        # A worker folder inside the run whose request.json was forged to name the victim.
        s = self.s
        _, rec = s.forged_folder(s.run_dir)
        s.assert_refused(s.cli("stop-dispatch", s.run_dir, "--dispatch", "y"))
        s.assert_untouched(rec)

    def test_runner_commit_refuses_a_request_for_another_repository(self):
        # The runner's entry check already refuses a request naming the victim, so the commit's
        # own re-check is reached by changing the run instead: while Codex works, run.json is
        # rewritten to name the victim as the workspace. The commit then refuses the request,
        # and commits nothing anywhere.
        s = self.s
        write(s.victim, {"stray.env": "SECRET=1\n"})
        ws_head = run_git(s.ws, "rev-parse", "HEAD")
        s.w.scenario("ready", delay=2)
        p = s.w.popen("d1", kind="implement", branch=s.branch)
        wait_for(lambda: (s.w.runner_json("d1") or {}).get("codex"), "Codex started", proc=p)
        meta = read_json(s.run_dir / "run.json")
        meta["workspace"] = str(s.victim)
        (s.run_dir / "run.json").write_text(json.dumps(meta), encoding="utf-8")
        p.communicate(timeout=120)
        res = s.w.result("d1")
        self.assertIsNone(res["committed"], res)
        self.assertIn("refused", res["commit_error"])
        self.assertEqual(len(s.starts()), 1)
        self.assertEqual(run_git(s.victim, "rev-parse", "HEAD"), s.victim_head)
        self.assertIn("stray.env", run_git(s.victim, "status", "--porcelain"))
        self.assertEqual(run_git(s.ws, "rev-parse", "HEAD"), ws_head)
        self.assertIn("?? done.txt", run_git(s.ws, "status", "--porcelain").splitlines())

    def test_runner_refuses_a_folder_outside_the_run(self):
        s = self.s
        fk = s.ws / ".leadhand" / "fk"
        wd, _ = s.forged_folder(fk)
        (wd / "runner.json").unlink()
        for session in (True, False):
            s.assert_refused(s.cli("dispatch-runner", wd, session=session))
        self.assertFalse((wd / "result.json").exists())
        s.assert_nothing_ran()
        self.assertIn("stray.env", run_git(s.victim, "status", "--porcelain"))

    def test_runner_refuses_a_request_for_another_repository(self):
        s = self.s
        wd, _ = s.forged_folder(s.run_dir)
        (wd / "runner.json").unlink()
        s.assert_refused(s.cli("dispatch-runner", wd))
        self.assertFalse((wd / "result.json").exists())
        s.assert_nothing_ran()
        self.assertIn("stray.env", run_git(s.victim, "status", "--porcelain"))


# --------------------------------------------------------------------------
# H3: a worker's .gitattributes and .lfsconfig never make leadhand's Git calls fetch

FAKE_LFS = """\
import hashlib, os, re, sys
from pathlib import Path
# A git-lfs stand-in for single-file clean/smudge: clean stores content and prints a pointer;
# smudge logs that it ran (with GIT_LFS_SKIP_SMUDGE), prints stored content, prints the pointer
# as is under GIT_LFS_SKIP_SMUDGE=1 (as git-lfs does), and otherwise "fetches" the object from
# .lfsconfig's URL: logged, then failed.
STORE, LOG, CALLS = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
POINTER = b"version https://git-lfs.github.com/spec/v1\\n"
data = sys.stdin.buffer.read()
if sys.argv[4] == "clean":
    if data.startswith(POINTER):
        out = data
    else:
        oid = hashlib.sha256(data).hexdigest()
        STORE.mkdir(exist_ok=True)
        (STORE / oid).write_bytes(data)
        out = POINTER + f"oid sha256:{oid}\\nsize {len(data)}\\n".encode()
else:
    with open(CALLS, "a") as f:
        f.write("SMUDGE skip=" + os.environ.get("GIT_LFS_SKIP_SMUDGE", "(unset)") + "\\n")
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
POINTER = "version https://git-lfs.github.com/spec/v1\n"
EXFIL = "127.0.0.1:9/exfil/SECRET-DATA"
ATTRIBUTES = "*.bin filter=lfs diff=lfs merge=lfs -text\n"


class H3FilterDrivers(unittest.TestCase):
    """H3: a worker's .gitattributes and .lfsconfig never make leadhand's own Git calls fetch
    LFS objects (a smudge POSTs to the URL in .lfsconfig, outside every sandbox), while the
    operator's LFS clean filter keeps working (no LFS file reads as modified).

    The operator's global configuration (GIT_CONFIG_GLOBAL, as `git lfs install` writes it)
    reaches only the binary; the harness's own Git calls and the fake workers' commits shut it
    out, as a worker's commit made without the filter would be."""

    def world(self, cards, *, codex=False) -> FakeSupervisor:
        """Claude workers only (their steps can tamper), or with Codex installed too."""
        w = codex_world(self, cards) if codex else FakeSupervisor(self, cards)
        self.fetch_log = w.tmp / "LFS FETCHES"
        self.calls = w.tmp / "LFS CALLS"
        fake = w.tmp / "fake-lfs.py"
        fake.write_text(FAKE_LFS, encoding="utf-8")
        store = w.tmp / "lfs store"
        cmd = f"'{sys.executable}' '{fake}' '{store}' '{self.fetch_log}' '{self.calls}'"
        self.filters = (f"filter.lfs.clean={cmd} clean", f"filter.lfs.smudge={cmd} smudge",
                        "filter.lfs.required=true")
        self.config = w.tmp / "global gitconfig"
        self.config.write_text("[filter \"lfs\"]\n" + "".join(
            f"\t{f.split('=', 1)[0].removeprefix('filter.lfs.')} = {f.split('=', 1)[1]}\n" for f in self.filters),
            encoding="utf-8")
        w.env["GIT_CONFIG_GLOBAL"] = str(self.config)
        w.env.pop("GIT_LFS_SKIP_SMUDGE", None)
        # The operator's repository uses LFS: big.bin committed through the clean filter (a
        # pointer in the tree, its content in the store and in the operator's checkout).
        write(w.repo, {".gitattributes": ATTRIBUTES, "big.bin": "large binary content\n"})
        self.with_filter(w.repo, "add", ".gitattributes", "big.bin")
        self.with_filter(w.repo, "commit", "--quiet", "-m", "big")
        self.assertTrue(run_git(w.repo, "cat-file", "-p", "HEAD:big.bin").startswith(POINTER))
        w.base = run_git(w.repo, "rev-parse", "HEAD")
        w.write_deck(cards)
        return w

    def with_filter(self, cwd, *args):
        """Plain git with the operator's filter (run_git shuts the global config out)."""
        return run_git(cwd, *[x for f in self.filters for x in ("-c", f)], *args)

    def assert_no_fetch(self):
        self.assertFalse(self.fetch_log.exists(), self.fetch_log.read_text() if self.fetch_log.exists() else "")

    def assert_every_smudge_skipped(self):
        calls = self.calls.read_text(encoding="utf-8").splitlines() if self.calls.exists() else []
        self.assertTrue(calls, "leadhand's checkouts ran no smudge at all")
        self.assertEqual(set(calls), {"SMUDGE skip=1"})

    def test_checkouts_resets_and_stashes_fetch_nothing(self):
        # Card 1's worker commits a pointer to an object nobody has and an .lfsconfig naming its
        # URL; card 2's worker deletes that file and leaves the deletion uncommitted, so before
        # its review leadhand saves the leftovers (stash create) and resets the workspace, which
        # writes the pointer file back through the smudge filter.
        def plant(world, ws, a):
            write(ws, {".lfsconfig": f"[lfs]\n\turl = http://{EXFIL}\n",
                       "p.bin": f"{POINTER}oid sha256:{'ab' * 32}\nsize 12\n"})
            run_git(ws, "add", "--", ".lfsconfig", "p.bin")
            run_git(ws, "commit", "--quiet", "-m", "worker's lfs files")

        def delete(world, ws, a):
            (ws / "p.bin").unlink()

        w = self.world([card(1), card(2)])
        w.start()
        w.script(1, "implement", {"tamper": plant})
        w.script(2, "implement", {"tamper": delete})
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2])
        (left,) = [e for e in w.events("violation") if e.get("stage") == "found" and e.get("by") == "implementer"]
        self.assertEqual(left["paths"], ["p.bin"])
        self.assertNotIn("p.bin", run_git(w.repo, "ls-tree", "--name-only", left["ref"]).splitlines())
        ws = w.workspace()
        self.assertTrue((ws / "p.bin").read_text(encoding="utf-8").startswith(POINTER))
        self.assert_no_fetch()
        self.assert_every_smudge_skipped()
        # Control: without leadhand's environment, a checkout of the same run branch fetches from
        # the worker's URL.
        env = {k: v for k, v in w.env.items() if not k.startswith("LEADHAND") and k != "GIT_LFS_SKIP_SMUDGE"}
        subprocess.run(["git", "worktree", "add", "--quiet", "--detach", str(w.tmp / "plain checkout"),
                        "refs/heads/leadhand/demo"], cwd=w.repo, env=env, capture_output=True, timeout=60,
                       stdin=subprocess.DEVNULL)
        self.assertIn(EXFIL, self.fetch_log.read_text(encoding="utf-8"))

    def test_checked_out_lfs_content_does_not_read_as_modified(self):
        # The reviewer pulls the real content of big.bin into the workspace (as `git lfs pull`
        # would), its stat data stale: Git must run the clean filter to compare, and finds the
        # pointer the index holds. Not a change, so not a violation.
        def pull(world, ws, a):
            write(ws, {"big.bin": "large binary content\n"})
            old = time.time() - 3600
            os.utime(ws / "big.bin", (old, old))

        w = self.world([card(1)])
        w.start()
        w.script(1, "review", {"tamper": pull})
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e for e in w.events("violation")], [])
        self.assertEqual([e["dispatch_id"] for e in w.events("verdict_recorded")], ["c1-a1-review-1"])
        self.assertEqual([e["card"] for e in w.events("accepted")], [1])
        self.assertEqual((w.workspace() / "big.bin").read_text(encoding="utf-8"), "large binary content\n")
        self.assertTrue(run_git(w.repo, "cat-file", "-p", "leadhand/demo:big.bin").startswith(POINTER))
        self.assert_no_fetch()

    def test_runner_commits_lfs_files_as_pointers_and_fetches_nothing(self):
        w = self.world([card(1, route=CODEX_CARD)], codex=True)
        w.start()
        w.script(1, "implement", {"commit": False,
                                  "files": {"q.bin": "new binary\n", "big.bin": "changed large content\n",
                                            "README.md": "changed\n"}})
        self.assertEqual(w.run()[-1]["action"], "done")
        (accepted,) = w.events("accepted")
        tip = accepted["sha"]
        for path in ("q.bin", "big.bin"):
            self.assertTrue(run_git(w.repo, "cat-file", "-p", f"{tip}:{path}").startswith(POINTER), path)
        self.assertEqual(run_git(w.repo, "show", f"{tip}:README.md"), "changed")
        trailer = run_git(w.repo, "log", "-1", "--format=%(trailers:key=Leadhand-Dispatch,valueonly)", tip)
        self.assertEqual(trailer, "c1-a1-implement-1")
        self.assert_no_fetch()
        self.assert_every_smudge_skipped()


# --------------------------------------------------------------------------
# M3: the evidence base worktree and the run's worktree are never reached through a symlink

class M3SymlinkedBaseWorktree(unittest.TestCase):
    """M3: a symlink at the evidence base worktree's path, or at a folder above it, must never
    turn the evidence reset (checkout --force, reset --hard, clean -fdx) on another worktree;
    nor may `start` take a symlink at the run worktree's path. The other worktree holds the
    operator's uncommitted and untracked work on its own branch."""

    def other_worktree(self, w) -> Path:
        other = w.tmp / "other worktree"
        run_git(w.repo, "worktree", "add", "--quiet", "-b", "other", str(other))
        write(other, {"README.md": "uncommitted work\n", "untracked.env": "KEEP=1\n"})
        self.other_head = run_git(other, "rev-parse", "HEAD")
        return other

    def assert_other_untouched(self, other: Path):
        self.assertEqual((other / "README.md").read_text(encoding="utf-8"), "uncommitted work\n")
        self.assertEqual((other / "untracked.env").read_text(encoding="utf-8"), "KEEP=1\n")
        self.assertEqual(run_git(other, "symbolic-ref", "--short", "HEAD"), "other")
        self.assertEqual(run_git(other, "rev-parse", "HEAD"), self.other_head)
        self.assertFalse(marker(other).exists())

    def test_evidence_reset_refuses_a_symlinked_base_worktree(self):
        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        other = self.other_worktree(w)
        link = base_wt(w)
        link.symlink_to(other, target_is_directory=True)
        r = drive_until_refused(w)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        self.assertIn("symlink", r.stderr)
        self.assertEqual(os.readlink(link), str(other))
        self.assert_other_untouched(other)
        self.assertNotIn("evidence", gate_names(w))

    def test_evidence_reset_refuses_a_symlinked_worktrees_folder(self):
        # While the final reviewer runs, <repo>/.worktrees is moved aside and replaced by a
        # symlink to a decoy folder, where the run's worktree is a symlink to its real place and
        # the base worktree's own name a symlink to the other worktree.
        holder = {}

        def swap(world, ws, a):
            repo = world.repo
            moved = repo / ".worktrees moved"
            os.rename(repo / ".worktrees", moved)
            run_git(repo, "worktree", "repair", str(moved / ws.name))
            decoy = repo / "decoy"
            decoy.mkdir()
            (decoy / ws.name).symlink_to(moved / ws.name, target_is_directory=True)
            (decoy / f"{ws.name}--base").symlink_to(holder["other"], target_is_directory=True)
            (repo / ".worktrees").symlink_to(decoy, target_is_directory=True)

        w = FakeSupervisor(self, [card(1)], gates=True)
        w.start()
        holder["other"] = self.other_worktree(w)
        w.script(None, "final_review", {"tamper": swap})
        r = drive_until_refused(w)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        self.assertIn("symlink", r.stderr)
        self.assert_other_untouched(holder["other"])
        self.assertNotIn("evidence", gate_names(w))

    def test_start_refuses_a_symlinked_worktree(self):
        # The run's worktree goes to <repo>/.worktrees/<run directory name>, named
        # <date>-<deck slug> (the deck is "Demo deck"): a symlink there, to another worktree, is
        # refused. Named for yesterday, today and tomorrow, so neither the date turning before
        # `start` nor a binary that dates its runs in UTC misses it.
        w = FakeSupervisor(self, [card(1)])
        other = self.other_worktree(w)
        (w.repo / ".worktrees").mkdir()
        today = date.today()
        for day in (today - timedelta(days=1), today, today + timedelta(days=1)):
            (w.repo / ".worktrees" / f"{day.isoformat()}-demo-deck").symlink_to(other, target_is_directory=True)
        listed = run_git(w.repo, "worktree", "list", "--porcelain")
        r = w.cli("start", w.deck_path, "--repo", w.repo)
        self.assertEqual(r.code, 1, r)
        self.assertTrue(r.stderr.startswith("error: "), r)
        self.assertIn("symlink", r.stderr)
        self.assertEqual(run_git(w.repo, "worktree", "list", "--porcelain"), listed)
        self.assert_other_untouched(other)


# --------------------------------------------------------------------------
# L1: `record result --reply-file` reads only the dispatch's own reply file

class L1ReplyFile(unittest.TestCase):
    def setUp(self):
        self.w = FakeSupervisor(self, [card(1)])
        self.w.start()
        self.a = self.w.next()
        self.worker = self.w.launch(self.a)
        self.w.perform(self.a, self.worker)
        self.secret = self.w.tmp / "id_secret"
        self.secret.write_text(f"{SECRET}\n", encoding="utf-8")

    def record(self, path):
        return self.w.cli("record", self.w.run_dir, "result", "--dispatch", self.a["dispatch_id"],
                          "--worker", self.worker, "--reply-file", path)

    def assert_no_secret_in_the_run(self):
        for f in self.w.run_dir.rglob("*"):
            if f.is_file():
                self.assertNotIn(SECRET, f.read_text(encoding="utf-8", errors="replace"), f)

    def assert_nothing_copied(self, r):
        self.assertNotEqual(r.code, 0, r)
        self.assertIn("refused", r.stderr, r)
        self.assertEqual(self.w.events("result_recorded"), [])
        self.assert_no_secret_in_the_run()

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
        (elsewhere / reply.name).write_text(f"Status: READY\n{SECRET}\n", encoding="utf-8")
        for f in folder.iterdir():
            os.replace(f, elsewhere / f.name) if not (elsewhere / f.name).exists() else f.unlink()
        folder.rmdir()
        folder.symlink_to(elsewhere, target_is_directory=True)
        self.assert_nothing_copied(self.record(reply))

    def test_a_report_behind_a_symlinked_scratch_folder_is_never_copied(self):
        # Codex 4546946519: O_NOFOLLOW guards only the last component; the folder is walked too.
        folder = Path(self.a["reply_file"]).parent
        elsewhere = self.w.tmp / "elsewhere"
        elsewhere.mkdir()
        (elsewhere / "report.md").write_text(f"Status: READY\n{SECRET}\n", encoding="utf-8")
        (elsewhere / "brief.md").write_text(f"{SECRET}\n", encoding="utf-8")
        for f in folder.iterdir():
            f.unlink()
        folder.rmdir()
        folder.symlink_to(elsewhere, target_is_directory=True)
        self.w.cli("record", self.w.run_dir, "result", "--dispatch", self.a["dispatch_id"],
                   "--worker", self.worker, "--reply", "Status: READY\n").ok()
        (done,) = self.w.events("result_recorded")
        self.assertTrue(any("report" in note for note in done.get("files_skipped") or []), done)
        self.assert_no_secret_in_the_run()


# --------------------------------------------------------------------------
# L2: a symlink planted at a later dispatch's scratch path

class L2SymlinkedScratch(unittest.TestCase):
    """L2 (Codex review round 1): an implementer plants a symlink at the next dispatch's scratch
    path; the engine must not write the review brief, nor remove a report, through it."""

    def plant(self, which):
        w = FakeSupervisor(self, [card(1)])
        outside = w.tmp / "outside"
        outside.mkdir()
        (outside / "report.md").write_text("keep me\n", encoding="utf-8")
        (outside / "reply.txt").write_text("keep me too\n", encoding="utf-8")

        def tamper(world, ws, a):
            scratch = ws / ".leadhand" / "1"
            if which == "dispatch folder":
                (scratch / "c1-a1-review-1").symlink_to(outside, target_is_directory=True)
            elif which == "card folder":
                os.rename(scratch, ws / ".leadhand" / "1-moved")
                scratch.symlink_to(outside, target_is_directory=True)
            else:                               # a file where the dispatch folder should be
                (scratch / "c1-a1-review-1").write_text("not a folder\n", encoding="utf-8")

        w.script(1, "implement", {"tamper": tamper})
        w.start()
        a = w.next()
        self.assertEqual(a["dispatch_id"], "c1-a1-implement-1")
        worker = w.launch(a)
        reply = w.perform(a, worker)
        # The reply inline, as the Python world recorded it: the card folder may be a symlink now.
        w.cli("record", w.run_dir, "result", "--dispatch", a["dispatch_id"], "--worker", worker,
              "--reply", reply).ok()
        return w, outside

    def assert_refused(self, w, outside, path_part):
        for _ in range(2):                      # the run waits: refused again until the operator acts
            r = w.cli("next", w.run_dir)
            self.assertEqual(r.code, 1, r)
            self.assertTrue(r.stderr.startswith("error: "), r)
            self.assertIn(path_part, r.stderr)
        self.assertEqual(list(outside.rglob("brief.md")), [])
        self.assertEqual((outside / "report.md").read_text(encoding="utf-8"), "keep me\n")
        self.assertEqual((outside / "reply.txt").read_text(encoding="utf-8"), "keep me too\n")
        self.assertEqual([e for e in w.events("dispatch_requested") if e["dispatch_kind"] == "review"], [])

    def test_a_symlinked_dispatch_folder_is_refused(self):
        w, outside = self.plant("dispatch folder")
        self.assert_refused(w, outside, str(Path(".leadhand") / "1" / "c1-a1-review-1"))

    def test_a_symlinked_card_folder_is_refused(self):
        w, outside = self.plant("card folder")
        self.assert_refused(w, outside, str(Path(".leadhand") / "1"))

    def test_a_file_in_place_of_the_dispatch_folder_is_refused(self):
        w, outside = self.plant("file")
        self.assert_refused(w, outside, str(Path(".leadhand") / "1" / "c1-a1-review-1"))


# --------------------------------------------------------------------------
# L6: neither the CLI nor the plugin directory may live in the workspace

class L6ToolsInsideTheWorkspace(unittest.TestCase):
    """L6: workers can edit their workspace, so neither the CLI (which runs outside the sandbox)
    nor the plugin the supervisor loads may live in it. The CLI's own path is the testing hook
    LEADHAND_TEST_CLI's."""

    def setUp(self):
        self.w = LaunchWorld(self)

    def test_run_refuses_a_cli_inside_the_workspace(self):
        r = self.w.run(LEADHAND_TEST_CLI=str(self.w.workspace / "scripts" / "leadhand"))
        self.assertEqual(r.code, 1, r)
        self.assertIn("inside the run's workspace", r.stderr)
        self.assertEqual(self.w.starts(), [])

    def test_run_refuses_a_plugin_dir_inside_the_workspace(self):
        r = self.w.run("--plugin-dir", self.w.workspace / "plugin")
        self.assertEqual(r.code, 1, r)
        self.assertIn("inside the run's workspace", r.stderr)
        self.assertEqual(self.w.starts(), [])

    def test_run_accepts_a_plugin_dir_outside_the_workspace(self):
        r = self.w.run("--plugin-dir", self.w.elsewhere)
        self.assertEqual(r.code, 0, r)
        self.assertEqual(len(self.w.starts()), 1)


# --------------------------------------------------------------------------
# I6, I7 (R45): leadhand's Git calls run no hook or fsmonitor, and the runner commits no gitlink

HOOKS = ("post-checkout", "pre-commit", "post-commit", "reference-transaction")


def marker_script(path: Path, marker: Path) -> Path:
    path.write_text(f'#!/bin/sh\necho "ran $0 in $(pwd)" >> "{marker}"\n', encoding="utf-8")
    path.chmod(0o755)
    return path


class GitSafety(unittest.TestCase):
    """Converted from tests/test_git_safety.py. The operator's repository has hooks that log to a
    marker file (in its hooks folder, which the run's worktrees share) and an fsmonitor command
    that logs to it too, both in its own configuration and in a dirty nested repository committed
    as a gitlink, as in the Python test. A whole run (start, card cuts, a reviewer's violation
    preserved and restored, the runner's commit of Codex's work, accepts, the end-of-run gates
    with the evidence base worktree) must leave no mark. Nothing else runs Git in the run: the
    Codex implementers leave their work for the runner to commit, and the Claude reviewers run no
    Git. Whether the trap is armed is checked last, inside the same test, with plain Git in the
    binary's environment."""

    def test_leadhand_git_calls_run_no_fsmonitor_or_hook(self):
        w = codex_world(self, [card(1), card(2)], gates=True)
        marker = w.tmp / "MARKER"
        evil = marker_script(w.tmp / "evil.sh", marker)
        quoted = f"'{evil}'"                    # fsmonitor runs through the shell; the path has a space
        sub = make_repo(w.repo, name="sub")
        run_git(sub, "config", "core.fsmonitor", quoted)
        run_git(w.repo, "add", "sub")
        run_git(w.repo, "commit", "--quiet", "-m", "add the nested repo")
        run_git(w.repo, "config", "core.fsmonitor", quoted)
        hooks = Path(run_git(w.repo, "rev-parse", "--path-format=absolute", "--git-path", "hooks"))
        hooks.mkdir(parents=True, exist_ok=True)
        for name in HOOKS:
            marker_script(hooks / name, marker)
        w.base = run_git(w.repo, "rev-parse", "HEAD")
        w.write_deck([card(1), card(2)])
        marker.unlink(missing_ok=True)
        write(sub, {"README.md": "dirty\n"})    # a dirty nested repo is what triggers its fsmonitor

        def leave_a_change(world, ws, a):
            write(ws, {"README.md": "a reviewer's edit\n"})

        w.start()
        w.script(1, "review", {"tamper": leave_a_change})
        for n in (1, 2):                        # Codex leaves its work for the runner to commit
            w.script(n, "implement", {"commit": False})
        self.assertEqual(w.run()[-1]["action"], "done")
        self.assertEqual([e["card"] for e in w.events("accepted")], [1, 2])
        (found,) = [e for e in w.events("violation") if e.get("stage") == "found"]
        self.assertEqual(found["paths"], ["README.md"])
        self.assertEqual(run_git(w.repo, "show", f"{found['ref']}:README.md"), "a reviewer's edit")
        self.assertEqual(run_git(w.repo, "log", "-1", "--format=%(trailers:key=Leadhand-Dispatch,valueonly)",
                                 w.events("accepted")[1]["sha"]), "c2-a1-implement-1")
        self.assertIn("evidence", gate_names(w))
        self.assertFalse(marker.exists(), marker.read_text(encoding="utf-8") if marker.exists() else "")
        # Control: plain Git in the binary's environment runs both.
        env = {k: v for k, v in w.env.items() if not k.startswith("LEADHAND")}
        for cwd, args in ((w.repo, ("status", "--porcelain")), (w.repo, ("update-ref", "refs/control", "HEAD"))):
            subprocess.run(["git", *args], cwd=cwd, env=env, capture_output=True, timeout=60,
                           stdin=subprocess.DEVNULL)
        ran = marker.read_text(encoding="utf-8") if marker.exists() else ""
        self.assertIn("evil.sh", ran)
        self.assertIn("reference-transaction", ran)

    def test_runner_never_commits_a_gitlink(self):
        # The Codex worker moves a tracked nested repository, makes a new one and edits a file;
        # each nested repository is a folder whose .git file points at a repository outside the
        # workspace, as a submodule's does. The runner commits the file, and neither gitlink.
        w = codex_world(self, [card(1, route=CODEX_CARD)])
        tracked = make_repo(w.tmp, name="tracked nested")
        old = run_git(tracked, "rev-parse", "HEAD")
        run_git(w.repo, "update-index", "--add", "--cacheinfo", f"160000,{old},src/tracked sub")
        run_git(w.repo, "commit", "--quiet", "-m", "a submodule pointer")
        w.base = run_git(w.repo, "rev-parse", "HEAD")
        w.write_deck([card(1, route=CODEX_CARD)])
        commit(tracked, {"README.md": "moved\n"}, "moved")
        fresh = make_repo(w.tmp, name="new nested")
        w.start()
        w.script(1, "implement", {"commit": False, "files": {
            "src/tracked sub/.git": f"gitdir: {run_git(tracked, 'rev-parse', '--absolute-git-dir')}\n",
            "src/tracked sub/README.md": "moved\n",
            "src/new sub/.git": f"gitdir: {run_git(fresh, 'rev-parse', '--absolute-git-dir')}\n",
            "src/new sub/README.md": "hello\n",
            "src/widget.py": "W = 1\n"}})
        self.assertEqual(w.run()[-1]["action"], "done")
        (accepted,) = w.events("accepted")
        tip = accepted["sha"]
        tree = run_git(w.repo, "ls-tree", "-r", tip)
        self.assertNotIn("new sub", tree)
        self.assertIn("src/widget.py", tree)
        self.assertEqual(run_git(w.repo, "rev-parse", f"{tip}:src/tracked sub"), old)
        result = read_json(w.run_dir / "workers" / "c1-a1-implement-1" / "result.json")
        self.assertEqual(result["committed"], tip)
        self.assertEqual(result.get("gitlinks_skipped"), ["src/tracked sub"])


if __name__ == "__main__":
    unittest.main()
