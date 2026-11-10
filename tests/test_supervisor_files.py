import argparse
import re
import unittest
from pathlib import Path

from leadhand import cli, engine

ROOT = Path(__file__).resolve().parents[1]
SUPERVISOR = ROOT / "agents" / "supervisor.md"
SKILL = ROOT / "skills" / "run" / "SKILL.md"

# `<cli> <command>` with its arguments, up to the end of the inline code span or the line.
CLI_CALL = re.compile(r"<cli> ([a-z][a-z-]*)([^`\n]*)")


def split(path: Path) -> tuple[dict[str, str], str]:
    text = path.read_text(encoding="utf-8")
    assert text.startswith("---\n"), f"{path}: no frontmatter"
    head, _, body = text[4:].partition("\n---\n")
    fields = {}
    for line in head.splitlines():
        key, sep, value = line.partition(":")
        assert sep and not line.startswith(" "), f"{path}: unexpected frontmatter line {line!r}"
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] == '"':
            value = value[1:-1]
        fields[key.strip()] = value
    return fields, body


def section(body: str, heading: str) -> str:
    """The text under a `### ` heading, up to the next heading of any level."""
    start = body.index(heading + "\n")
    rest = body[start + len(heading) + 1:]
    m = re.search(r"^#{1,3} ", rest, re.MULTILINE)
    return rest[:m.start()] if m else rest


def subcommands() -> dict[str, argparse.ArgumentParser]:
    parser = cli.build_parser()
    action = next(a for a in parser._actions if isinstance(a, argparse._SubParsersAction))
    return dict(action.choices)


class SupervisorFileTests(unittest.TestCase):
    def test_supervisor_frontmatter(self):
        fields, body = split(SUPERVISOR)
        self.assertEqual(fields, {
            "name": "supervisor",
            "description": "leadhand supervisor: runs one deck. Start it with the leadhand launch command, "
                           "never by hand.",
            "model": "opus",
            "effort": "medium",
            "skills": "[leadhand:run]",
        })
        # P2: the preload is not relied on; the body invokes the skill first.
        self.assertIn("First invoke the Skill tool with `leadhand:run`, then follow it.", body)
        self.assertIn("Follow the run skill.", body)
        skill_fields, _ = split(SKILL)
        self.assertEqual(skill_fields.get("name"), "run")
        self.assertTrue(skill_fields.get("description"))

    def test_run_skill_covers_every_action(self):
        _, body = split(SKILL)
        for action in engine.ACTIONS:
            self.assertIn(f"`{action}`", body, action)
        for kind in engine.RECORD_KINDS:
            self.assertIn(f"record <run_dir> {kind}", body, kind)
        # The required parts, in the brief's order.
        order = [body.index(h) for h in ("## Preflight", "## The loop", "## Hygiene", "## Steering")]
        self.assertEqual(order, sorted(order))

    def test_run_skill_commands_match_cli(self):
        _, body = split(SKILL)
        commands = subcommands()
        calls = CLI_CALL.findall(body)
        self.assertTrue(calls)
        for name, args in calls:
            self.assertIn(name, commands, f"<cli> {name}")
            options = set(commands[name]._option_string_actions)
            for flag in re.findall(r"(?<![\w-])(--[a-z][a-z-]*)", args):
                self.assertIn(flag, options, f"<cli> {name}{args}")
            if name == "record":
                kind = args.split()[1]
                self.assertIn(kind, engine.RECORD_KINDS, f"<cli> {name}{args}")
        # P5/R28: CLI calls are plain commands (the sandbox exclusion matches nothing else).
        # The one exception is the quoted heredoc that records a worker's reply.
        heredoc = " <run_dir> result --dispatch <dispatch_id> --worker <agent id> --reply - <<'LEADHAND_REPLY'"
        self.assertEqual(sum(1 for c in calls if c == ("record", heredoc)), 1)
        for name, args in calls:
            if (name, args) == ("record", heredoc):
                continue
            plain = re.sub(r"<[a-z_ ]+>", "", args)     # drop placeholders such as <run_dir>
            self.assertNotRegex(plain, r"[|;&<>$]|\bcd\b", f"<cli> {name}{args}")

    def test_run_skill_records_started_before_waiting(self):
        _, body = split(SKILL)
        for heading in ("### `dispatch` without `resume`", "### `dispatch` with `resume`"):
            text = section(body, heading)
            started = text.index("<cli> record <run_dir> started --dispatch <dispatch_id> --worker <agent id>")
            timer = text.index("<cli> timer <run_dir> --dispatch <dispatch_id>")
            self.assertLess(started, timer, heading)
        # P8: waiting never means sleeping.
        self.assertNotRegex(body, r"(?m)^\s*`?sleep ")


if __name__ == "__main__":
    unittest.main()
