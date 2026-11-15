"""The Rust binary's own command line: the version, clap's closest-match suggestion, the help
of every command a person types, and a release build that ignores every test hook
(deliberate changes 4 and 5, and the conventions for the commands a person types). Rust-only.

The surface tests run the Rust binary itself, not the router (ruling S2-R6): the router sends
a command it doesn't list (`statu`, an unported command's `--help`) to the Python CLI.
"""
import os
import signal
import subprocess
import time
import unittest
from pathlib import Path

import harness
from harness import ROOT, hermetic_env, read_json, run_argv, run_cli, rust_only, temp_root

# The commands a person types (spec, "Commands you type").
HUMAN_COMMANDS = ("run", "start", "status", "pause", "cancel", "unpark", "report", "check", "launch-line",
                  "version")
RELEASE_BIN = ROOT / "target" / "release" / "leadhand"


def rust_bin(test: unittest.TestCase) -> Path:
    """The Rust binary itself: LEADHAND_RUST_BIN behind the router, else LEADHAND_BIN."""
    if not harness.router_mode():
        return harness.BIN
    if harness.RUST_BIN is None:
        test.fail("LEADHAND_BIN is the router but LEADHAND_RUST_BIN is not set")
    return harness.RUST_BIN


def python_bin() -> Path:
    return harness.PY_BIN or ROOT / "scripts" / "leadhand"


@rust_only(uses=("version",))
class Version(unittest.TestCase):
    def test_version_matches_reference(self):
        tmp = temp_root(self, "lh rust cli ")
        env = hermetic_env(self, tmp)
        r = run_cli("version", env=env, cwd=tmp).ok()
        self.assertEqual(r.stdout, "leadhand 0.1.0\n", r)
        self.assertEqual(r.stderr, "", r)
        reference = run_argv([str(python_bin()), "version"], env=env, cwd=tmp).ok()
        self.assertEqual(r.stdout, reference.stdout, reference)

    def test_version_reaches_the_rust_binary(self):
        # The help of a listed command comes from clap, with its example: not from argparse.
        tmp = temp_root(self, "lh rust cli ")
        r = run_cli("version", "--help", env=hermetic_env(self, tmp), cwd=tmp).ok()
        self.assertIn("\nExample:\n  leadhand version\n", r.stdout, r)


@rust_only(uses=("version",))
class Surface(unittest.TestCase):
    def setUp(self):
        self.tmp = temp_root(self, "lh rust cli ")
        self.env = hermetic_env(self, self.tmp)
        self.bin = rust_bin(self)

    def cli(self, *args) -> harness.Result:
        return run_argv([str(self.bin), *args], env=self.env, cwd=self.tmp)

    def test_unknown_command_suggests_closest(self):
        r = self.cli("statu")
        self.assertEqual(r.code, 2, r)
        self.assertTrue(r.stderr.startswith("error: unrecognized subcommand 'statu'\n"), r)
        # clap names every close match: "a similar subcommand exists: 'x'" or "some similar
        # subcommands exist: 'start', 'status'".
        tips = [line.strip() for line in r.stderr.splitlines() if line.strip().startswith("tip: ")]
        self.assertEqual(len(tips), 1, r)
        self.assertRegex(tips[0], r"^tip: (a similar subcommand exists|some similar subcommands exist): ", r)
        self.assertIn("'status'", tips[0], r)
        self.assertEqual(r.stdout, "", r)

    def test_unknown_flag_suggests_closest(self):
        r = self.cli("check", "deck.md", "--rpeo", ".")
        self.assertEqual(r.code, 2, r)
        self.assertTrue(r.stderr.startswith("error: unexpected argument '--rpeo' found\n"), r)
        self.assertIn("tip: a similar argument exists: '--repo'", r.stderr, r)

    def test_help_has_example_for_each_human_command(self):
        for name in HUMAN_COMMANDS:
            with self.subTest(command=name):
                r = self.cli(name, "--help").ok()
                lines = r.stdout.splitlines()
                # The first line is the one-line purpose.
                self.assertTrue(lines and lines[0].strip() and not lines[0].startswith("Usage:"), r)
                self.assertEqual(lines[1], "", r)
                # An "Example:" section whose command runs this command.
                self.assertIn("Example:", lines, r)
                example = lines[lines.index("Example:") + 1]
                self.assertTrue(example == f"  leadhand {name}" or example.startswith(f"  leadhand {name} "), r)


@rust_only(uses=("version",))
class ReleaseBuild(unittest.TestCase):
    """Review focus 4: a release build ignores every test hook (deliberate change 5)."""

    HOOKS = ("LEADHAND_TESTING", "LEADHAND_CRASH_AT", "LEADHAND_CRASH_LOG", "LEADHAND_CRASH_COUNTER",
             "LEADHAND_CRASH_SKIP", "LEADHAND_CRASH_KILL_PGID", "LEADHAND_TEST_CODEX_BIN",
             "LEADHAND_TEST_CARD_LOOP_ONLY", "LEADHAND_INSTALLED_VENDORS", "LEADHAND_MINUTE_SECONDS",
             "LEADHAND_TEST_CLOCK_OFFSET", "LEADHAND_TEST_CLI", "LEADHAND_TEST_FOREGROUND_WAIT")

    def setUp(self):
        if not RELEASE_BIN.is_file():
            self.fail(f"no release build at {RELEASE_BIN}: build it with `cargo build --locked --release`")
        self.tmp = temp_root(self, "lh rust release ")
        env = hermetic_env(self, self.tmp)
        self.plain = {k: v for k, v in env.items() if k not in self.HOOKS}
        # A process group that LEADHAND_CRASH_KILL_PGID names: it survives only if the hook is ignored.
        self.decoy = subprocess.Popen(["sleep", "60"], start_new_session=True, stdin=subprocess.DEVNULL)
        self.addCleanup(self.reap, self.decoy)
        self.hooked = dict(self.plain,
                           LEADHAND_TESTING="1",
                           LEADHAND_CRASH_AT="version",
                           LEADHAND_CRASH_LOG=str(self.tmp / "crash log"),
                           LEADHAND_CRASH_COUNTER=str(self.tmp / "crash counter"),
                           LEADHAND_CRASH_SKIP="0",
                           LEADHAND_CRASH_KILL_PGID=str(self.decoy.pid),
                           LEADHAND_TEST_CODEX_BIN=str(self.tmp / "fake codex"),
                           LEADHAND_TEST_CARD_LOOP_ONLY="1",
                           LEADHAND_INSTALLED_VENDORS="claude,codex",
                           LEADHAND_MINUTE_SECONDS="1",
                           LEADHAND_TEST_CLOCK_OFFSET="3600",
                           LEADHAND_TEST_CLI="/x/router",
                           LEADHAND_TEST_FOREGROUND_WAIT="1")
        self.assertEqual(set(self.HOOKS) - set(self.hooked), set())

    @staticmethod
    def reap(p: subprocess.Popen) -> None:
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        p.wait()

    def run_release(self, *args, env: dict) -> harness.Result:
        return run_argv([str(RELEASE_BIN), *args], env=env, cwd=self.tmp)

    def test_release_binary_ignores_every_test_hook(self):
        plain = self.run_release("version", env=self.plain)
        hooked = self.run_release("version", env=self.hooked)
        self.assertEqual(plain.stdout, "leadhand 0.1.0\n", plain)
        self.assertEqual((hooked.code, hooked.stdout, hooked.stderr), (plain.code, plain.stdout, plain.stderr),
                         hooked)
        self.assertFalse((self.tmp / "crash log").exists())
        self.assertFalse((self.tmp / "crash counter").exists())
        self.assertIsNone(self.decoy.poll(), "the release build signalled LEADHAND_CRASH_KILL_PGID's group")
        self.release_next_ignores_every_crash_point()

    # The crash points `next` itself passes: the rest are a runner's (before/after_worker_launch)
    # and the harness's workers' (before/after_commit).
    NEXT_POINTS = {"before_rebase", "after_rebase", "before_fast_forward", "after_fast_forward",
                   "before_result_record", "after_result_files", "after_charge_before_attempt",
                   "after_cancel_request", "before_gate_record"}

    def release_next_ignores_every_crash_point(self):
        """`next` (Task 17): a whole run driven twice from the same deck with fixed commit dates,
        as the crash matrix drives it (contract_recovery.StableSupervisor: a parked card unparked
        and integrated, a Codex card with a fix round, an escalation, the end-of-run gates), and a
        relaunch while a Codex dispatch was handed out but never run, so `next` reconciles it.

        The reference run is the testing build, with no hook beyond the world's own; each `next`
        logs the crash points it passes (LEADHAND_CRASH_LOG). The release run's every `next` gets
        every hook set, with LEADHAND_CRASH_AT naming a point the same call of the reference run
        passed, until each such point is named: it must never crash, log, count or signal, and
        must reach the same final state."""
        from unittest import mock
        from contract_recovery import FIXED_DATE, StableSupervisor, resume
        dates = mock.patch.dict(os.environ, {"GIT_AUTHOR_DATE": FIXED_DATE, "GIT_COMMITTER_DATE": FIXED_DATE})
        dates.start()
        self.addCleanup(dates.stop)
        points = ("before_worker_launch", "after_worker_launch", *sorted(self.NEXT_POINTS),
                  "before_commit", "after_commit")

        def drive(w, next_action, until=lambda: False):
            """The world's drive, with the relaunch, to its end or until `until()`; the actions
            `next` returned, by call."""
            w.session("supervisor-1")
            w.start()
            actions, relaunched = [], False
            for _ in range(200):
                w.operator()
                a = next_action(w, len(actions))
                actions.append(a)
                if a["action"] == "done" or until():
                    return actions
                if a["action"] == "wait":
                    time.sleep(0.1)
                elif not relaunched and a["action"] == "dispatch" and a["agent"] == "leadhand:codex":
                    relaunched = True
                    resume(w)                    # the session died before its proxy ran
                else:
                    w.act(a)
            self.fail("the run did not finish")

        passed: list[list[str]] = []

        def reference_next(w, i):
            log = w.tmp / f"crash log {i}"
            r = run_cli("next", w.run_dir, env=dict(w._env(), LEADHAND_CRASH_LOG=str(log)), cwd=w.tmp).ok()
            passed.append(log.read_text(encoding="utf-8").split() if log.exists() else [])
            return r.json()

        ref = StableSupervisor(self)
        reference = drive(ref, reference_next)
        expected = ref.final_state()
        self.assertEqual(expected["outcome"], ("complete", ""))
        on_path = {p for call in passed for p in call}
        self.assertEqual(on_path, self.NEXT_POINTS)

        named: set[str] = set()
        crash_files = (self.tmp / "next crash log", self.tmp / "next crash counter")

        def release_next(w, i):
            call = passed[i] if i < len(passed) else []
            point = next((p for p in call if p not in named), call[0] if call else points[i % len(points)])
            if point in call:
                named.add(point)
            env = dict(w._env(), PATH=f"{harness.FAKES}{os.pathsep}{w.env['PATH']}")
            env.update({k: v for k, v in self.hooked.items() if k in self.HOOKS})
            env.update(LEADHAND_CRASH_AT=point, LEADHAND_CRASH_LOG=str(crash_files[0]),
                       LEADHAND_CRASH_COUNTER=str(crash_files[1]), LEADHAND_INSTALLED_VENDORS="claude")
            r = self.run_release("next", str(w.run_dir), env=env)
            self.assertEqual(r.code, 0, f"{point}: {r}")
            return r.json()

        w = StableSupervisor(self)
        actions = drive(w, release_next)
        self.assertEqual([(a["action"], a.get("dispatch_id")) for a in actions],
                         [(a["action"], a.get("dispatch_id")) for a in reference])
        self.assertEqual(w.final_state(), expected)
        self.assertTrue(w.events("reconciled"))
        # One call passes several points (the reconciling one passes four), and one `next` names
        # one: the same run again, as far as it takes to name the rest.
        for _ in range(4):
            if named == self.NEXT_POINTS:
                break
            drive(StableSupervisor(self), release_next, until=lambda: named == self.NEXT_POINTS)
        self.assertEqual(named, self.NEXT_POINTS)
        for path in crash_files:
            self.assertFalse(path.exists(), path)
        self.assertIsNone(self.decoy.poll(), "the release build signalled LEADHAND_CRASH_KILL_PGID's group")


@rust_only(uses=("dispatch", "dispatch-runner"))
class ReleaseBuildDispatch(unittest.TestCase):
    """Review focus 4 for the dispatch hooks (deliberate change 5, and the foreground-wait ruling):
    a release `dispatch`, with LEADHAND_TESTING=1, ignores LEADHAND_TEST_CODEX_BIN,
    LEADHAND_MINUTE_SECONDS and LEADHAND_TEST_FOREGROUND_WAIT.

    Each runs on a run `start` made (contract_dispatch.World), with tests/fakes first on PATH, so
    the `codex` a release build looks up is the fake: nothing reaches a real Codex. The release
    binary starts its runner as itself (it ignores LEADHAND_TEST_CLI too). Each case's control
    runs the same line through LEADHAND_BIN (the testing build), which honours the hook."""

    def setUp(self):
        if not RELEASE_BIN.is_file():
            self.fail(f"no release build at {RELEASE_BIN}: build it with `cargo build --locked --release`")
        from contract_dispatch import World     # the dispatch world, only where it is used
        self.w = World(self)

    def env(self, **hooks) -> dict:
        """The world's environment as it is now (its scenario included), with the fakes first on
        PATH, LEADHAND_TESTING=1, no Codex override unless `hooks` names one, and `hooks`."""
        env = dict(self.w.env, PATH=f"{harness.FAKES}{os.pathsep}{self.w.env['PATH']}", LEADHAND_TESTING="1")
        env.pop("LEADHAND_TEST_CODEX_BIN", None)
        return dict(env, **hooks)

    def release(self, did: str, env: dict) -> harness.Result:
        return run_argv([str(RELEASE_BIN), *(str(a) for a in self.w.args(did))], env=env, cwd=self.w.tmp,
                        timeout=120)

    def run_testing(self, did: str, env: dict) -> harness.Result:
        return run_cli(*self.w.args(did), env=env, cwd=self.w.tmp, timeout=120)

    def assert_ready(self, r: harness.Result, did: str) -> None:
        lines = r.ok().stdout.splitlines()
        self.assertEqual(lines[0], "Status: READY", r)
        self.assertEqual(lines[-1], "session: 0f0f0f0f-0000-4000-8000-00000000c0de", r)
        self.assertEqual(read_json(self.w.wd(did) / "result.json")["exit_reason"], "completed", r)

    def test_release_dispatch_ignores_the_codex_bin_hook(self):
        flag = self.w.tmp / "override ran"
        override = self.w.tmp / "fake codex override"
        override.write_text(f'#!/bin/sh\necho ran >> "{flag}"\nexec "{harness.FAKES / "codex"}" "$@"\n',
                            encoding="utf-8")
        override.chmod(0o755)
        self.w.scenario("ready")
        env = self.env(LEADHAND_TEST_CODEX_BIN=str(override))
        self.assert_ready(self.release("d1", env), "d1")
        self.assertFalse(flag.exists(), "the release build ran LEADHAND_TEST_CODEX_BIN")
        self.assertEqual(len(self.w.records("start")), 1)      # the fake found on PATH ran
        self.assert_ready(self.run_testing("d2", env), "d2")
        self.assertEqual(flag.read_text(encoding="utf-8"), "ran\n")
        self.assertEqual(len(self.w.records("start")), 2)

    def test_release_dispatch_ignores_the_minute_hook(self):
        # A one-minute limit, a Codex that answers after 2 s, and a "minute" of 0.05 s.
        self.w.set_limits("implementer", minutes=1)
        self.w.scenario("ready", delay=2)
        env = self.env(LEADHAND_MINUTE_SECONDS="0.05")
        self.assert_ready(self.release("d1", env), "d1")
        r = self.run_testing("d2", env).ok()
        self.assertEqual(r.stdout.splitlines()[0], "Status: SPENT", r)
        self.assertEqual(read_json(self.w.wd("d2") / "result.json")["exit_reason"], "time_limit", r)

    def test_release_dispatch_ignores_the_foreground_wait_hook(self):
        # The foreground-wait ruling's case: a 1 s hook and a Codex that answers after 4 s.
        self.w.scenario("ready", delay=4)
        env = self.env(LEADHAND_TEST_FOREGROUND_WAIT="1")
        self.assert_ready(self.release("d1", env), "d1")
        r = self.run_testing("d2", env)
        self.assertEqual(r.code, 75, r)
        self.assertEqual(r.stdout.splitlines(), ["still running: run the same command again (dispatch d2)"])


if __name__ == "__main__":
    unittest.main()
