"""The command line itself: the version, usage errors, and the commands the run skill calls.

Converted from tests/test_cli.py, plus the binary's half of
test_supervisor_files.test_run_skill_commands_match_cli.
"""
import re
import unittest

from contract_plugin_files import CLI_CALL, FLAG, SKILL, split
from harness import hermetic_env, run_cli, temp_root


def has_token(text: str, word: str) -> bool:
    """`word` appears in `text` with no word character or hyphen on either side."""
    return re.search(rf"(?<![\w-]){re.escape(word)}(?![\w-])", text) is not None


class Version(unittest.TestCase):
    def test_prints_package_version(self):
        tmp = temp_root(self, "lh cli ")
        r = run_cli("version", env=hermetic_env(self, tmp), cwd=tmp).ok()
        self.assertEqual(r.stdout, "leadhand 0.1.0\n", r)


class Usage(unittest.TestCase):
    def test_unknown_command_exits_2(self):
        tmp = temp_root(self, "lh cli ")
        r = run_cli("nonsense", env=hermetic_env(self, tmp), cwd=tmp)
        self.assertEqual(r.code, 2, r)


class PluginCommands(unittest.TestCase):
    def test_every_cli_command_in_the_run_skill_is_a_registered_command(self):
        # Facts both argparse and clap expose: `<name> --help` exits 0 for a registered
        # command, an unknown command exits 2, and the help names every flag and value.
        tmp = temp_root(self, "lh cli ")
        env = hermetic_env(self, tmp)
        _, body = split(SKILL)
        calls = CLI_CALL.findall(body)
        self.assertTrue(calls)
        unknown = run_cli("no-such-command", "--help", env=env, cwd=tmp)
        self.assertEqual(unknown.code, 2, unknown)
        helps = {name: run_cli(name, "--help", env=env, cwd=tmp).ok() for name in dict.fromkeys(n for n, _ in calls)}
        for name, args in calls:
            help_ = helps[name]
            for flag in FLAG.findall(args):
                self.assertTrue(has_token(help_.stdout, flag), f"{flag} of `<cli> {name}{args}` is not in\n{help_}")
            if name == "record":
                kind = args.split()[1]
                self.assertTrue(has_token(help_.stdout, kind), f"record kind {kind} is not in\n{help_}")


if __name__ == "__main__":
    unittest.main()
