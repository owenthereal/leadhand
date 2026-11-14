"""The Rust binary's own command line: the version, clap's closest-match suggestion, the help
of every command a person types, and a release build that ignores every test hook
(deliberate changes 4 and 5, and the conventions for the commands a person types). Rust-only.

The surface tests run the Rust binary itself, not the router (ruling S2-R6): the router sends
a command it doesn't list (`statu`, an unported command's `--help`) to the Python CLI.
"""
import os
import signal
import subprocess
import unittest
from pathlib import Path

import harness
from harness import ROOT, hermetic_env, run_argv, run_cli, rust_only, temp_root

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


if __name__ == "__main__":
    unittest.main()
