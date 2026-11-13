"""The harness's own tests: what a failing or hung binary reports, the sealed environment,
the suite's isolation from the Python package, and the tags. Static: these never run
LEADHAND_BIN (fake binaries stand in for it)."""
import os
import re
import shlex
import time
import unittest
from pathlib import Path
from unittest import mock

import harness
from harness import hermetic_env, run_cli, temp_root

HERE = Path(__file__).resolve().parent
IMPORT = re.compile(r"^\s*(from|import)\s+leadhand\b", re.MULTILINE)


def fake_binary(tmp: Path, body: str) -> Path:
    path = tmp / "fake leadhand"
    path.write_text("#!/bin/sh\n" + body, encoding="utf-8")
    path.chmod(0o755)
    return path


def outcome(decorate) -> str:
    """'ran' or 'skipped': what happens to a passing test class under `decorate`."""
    class Probe(unittest.TestCase):
        def test_probe(self):
            pass
    result = unittest.TestResult()
    decorate(Probe)("test_probe").run(result)
    if result.skipped:
        return "skipped"
    return "ran" if result.wasSuccessful() and result.testsRun == 1 else "failed"


class HarnessReports(unittest.TestCase):
    def setUp(self):
        self.tmp = temp_root(self, "lh self ")
        self.env = hermetic_env(self, self.tmp)

    def use(self, body: str) -> Path:
        binary = fake_binary(self.tmp, body)
        patch = mock.patch.object(harness, "BIN", binary)
        patch.start()
        self.addCleanup(patch.stop)
        return binary

    def test_failure_message_names_argv_code_and_stderr(self):
        binary = self.use("echo boom >&2\nexit 4\n")
        r = run_cli("first arg", "--flag", env=self.env, cwd=self.tmp)
        self.assertEqual((r.argv, r.code), ([str(binary), "first arg", "--flag"], 4))
        with self.assertRaises(AssertionError) as cm:
            r.ok()
        text = str(cm.exception)
        for part in (str(binary), "first arg", "--flag", "exit code: 4", "boom"):
            self.assertIn(part, text)

    def test_output_that_is_not_json_names_argv_and_stdout(self):
        binary = self.use("echo not json\n")
        r = run_cli("next", env=self.env, cwd=self.tmp)
        with self.assertRaises(AssertionError) as cm:
            r.json()
        text = str(cm.exception)
        for part in (str(binary), "next", "not JSON", "not json"):
            self.assertIn(part, text)

    def test_hung_binary_times_out_with_command_named(self):
        # The sleeper is a child of the script, holding its stdout and stderr open: killing
        # only the script would leave communicate() waiting on the pipes for 30 s.
        binary = self.use('sleep 30 &\necho $! > "$1"\nwait\n')
        pid_file = self.tmp / "sleeper pid"
        began = time.monotonic()
        with self.assertRaises(AssertionError) as cm:
            run_cli(pid_file, env=self.env, cwd=self.tmp, timeout=1)
        self.assertLess(time.monotonic() - began, 5)
        text = str(cm.exception)
        for part in (str(binary), str(pid_file), "timed out after 1"):
            self.assertIn(part, text)
        sleeper = int(pid_file.read_text(encoding="utf-8"))
        deadline = time.monotonic() + 2
        while harness._start_time(sleeper) is not None and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertIsNone(harness._start_time(sleeper), f"sleeper {sleeper} outlived the timeout")


class SelfPath(unittest.TestCase):
    """S1-R11: a path the binary prints for itself is compared by resolved file, not spelling."""

    def setUp(self):
        tmp = temp_root(self, "lh self ")
        self.real = fake_binary(tmp, "exit 0\n")
        (tmp / "link dir").symlink_to(tmp)
        self.linked = tmp / "link dir" / self.real.name     # another spelling of the same file
        self.other = tmp / "other leadhand"
        self.other.write_text("#!/bin/sh\n", encoding="utf-8")
        patch = mock.patch.object(harness, "BIN", self.real)
        patch.start()
        self.addCleanup(patch.stop)

    def test_names_bin_accepts_any_spelling_of_bin_and_rejects_another_file(self):
        self.assertNotEqual(str(self.linked), str(self.real))
        self.assertTrue(harness.names_bin(str(self.real)))
        self.assertTrue(harness.names_bin(str(self.linked)))
        self.assertFalse(harness.names_bin(str(self.other)))

    def test_bin_command_checks_the_rest_of_the_line_exactly(self):
        before, after = "next: ", " unpark '/a run' --card 1"
        line = lambda path: before + shlex.quote(str(path)) + after
        self.assertTrue(harness.is_bin_command(line(self.linked), before, after))
        self.assertFalse(harness.is_bin_command(line(self.other), before, after))
        self.assertFalse(harness.is_bin_command(line(self.real) + " x", before, after))
        self.assertFalse(harness.is_bin_command("Next: " + shlex.quote(str(self.real)) + after, before, after))
        self.assertFalse(harness.is_bin_command(before + " " + shlex.quote(str(self.real)) + after, before, after))
        # an unquoted path with a space is two shell words, not one
        self.assertFalse(harness.is_bin_command(before + str(self.real) + after, before, after))

    def test_bin_between_takes_the_path_verbatim_and_checks_the_rest_exactly(self):
        before, after = "leadhand CLI: ", ". Then go."
        self.assertTrue(harness.names_bin_between(before + str(self.linked) + after, before, after))
        self.assertFalse(harness.names_bin_between(before + str(self.other) + after, before, after))
        self.assertFalse(harness.names_bin_between(before + str(self.real) + after + " x", before, after))
        self.assertFalse(harness.names_bin_between(before + shlex.quote(str(self.real) + " x") + after,
                                                   before, after))
        self.assertFalse(harness.names_bin_between(before + after, before, after))


class HermeticEnv(unittest.TestCase):
    def test_inherited_hooks_are_dropped(self):
        tmp = temp_root(self, "lh self ")
        inherited = {"LEADHAND_CRASH_AT": "after_start", "LEADHAND_TEST_CLI": "/elsewhere/leadhand",
                     "LEADHAND_TEST_CLOCK_OFFSET": "3600", "LEADHAND_TEST_CARD_LOOP_ONLY": "1",
                     "LEADHAND_INSTALLED_VENDORS": "claude,codex", "LEADHAND_MINUTE_SECONDS": "1",
                     "LEADHAND_RUN_DIR": "/elsewhere/run", "CLAUDE_CODE_SESSION_ID": "abc",
                     "FAKE_CODEX_SCENARIO": "hang", "GIT_DIR": "/elsewhere/.git",
                     "LEADHAND_RUST_COMMANDS": "check"}
        # Unwound after hermetic_env's own patch of os.environ (cleanups run last-in, first-out).
        patch = mock.patch.dict(os.environ, inherited)
        patch.start()
        self.addCleanup(patch.stop)
        env = hermetic_env(self, tmp, installed=("codex",))
        for name in ("LEADHAND_CRASH_AT", "LEADHAND_TEST_CLI", "LEADHAND_TEST_CLOCK_OFFSET",
                     "LEADHAND_TEST_CARD_LOOP_ONLY", "LEADHAND_MINUTE_SECONDS", "LEADHAND_RUN_DIR",
                     "CLAUDE_CODE_SESSION_ID", "FAKE_CODEX_SCENARIO", "GIT_DIR"):
            self.assertNotIn(name, env)
        self.assertEqual(env["LEADHAND_INSTALLED_VENDORS"], "codex")
        self.assertEqual(env["LEADHAND_RUST_COMMANDS"], "check")
        self.assertEqual(env["LEADHAND_TESTING"], "1")
        self.assertEqual(env["GIT_CONFIG_GLOBAL"], os.devnull)
        config = Path(env["LEADHAND_CONFIG"])
        self.assertEqual(config, tmp / "user config.toml")
        self.assertIn(f'docs_dir = "{tmp / "docs dir"}"', config.read_text(encoding="utf-8"))


class NoPackageImport(unittest.TestCase):
    def test_contract_files_never_import_leadhand(self):
        for line in ("import leadhand", "from leadhand.procs import same_process", "    import leadhand.usage as u"):
            self.assertRegex(line, IMPORT)
        files = sorted(HERE.glob("*.py"))
        self.assertIn(HERE / "harness.py", files)
        self.assertIn(HERE / "contract_self.py", files)
        found = [f"{p.name}: {m.group(0).strip()}" for p in files
                 for m in IMPORT.finditer(p.read_text(encoding="utf-8"))]
        self.assertEqual(found, [])


class Tags(unittest.TestCase):
    def test_reference_only_skips_under_rust(self):
        for impl, rust, want in (("python", frozenset(), "ran"),
                                 ("rust", frozenset(), "skipped"),
                                 ("mixed", frozenset({"check"}), "skipped"),
                                 ("python", frozenset({"all"}), "skipped")):
            with self.subTest(impl=impl, rust=sorted(rust)), \
                    mock.patch.object(harness, "IMPL", impl), mock.patch.object(harness, "RUST_COMMANDS", rust):
                self.assertEqual(outcome(harness.reference_only), want)

    def test_rust_only_runs_once_every_command_it_uses_is_rust(self):
        uses = ("check", "route")
        for rust, want in ((frozenset(), "skipped"),
                           (frozenset({"check"}), "skipped"),
                           (frozenset({"check", "route", "version"}), "ran"),
                           (frozenset({"all"}), "ran")):
            with self.subTest(rust=sorted(rust)), mock.patch.object(harness, "RUST_COMMANDS", rust):
                self.assertEqual(outcome(harness.rust_only(uses=uses)), want)


if __name__ == "__main__":
    unittest.main()
