"""Static checks of the plugin files: the agents, the supervisor and the run skill.

Converted from tests/test_agents.py and tests/test_supervisor_files.py. These read
only plugin files and never run the binary. The run skill's relation to the binary
(its commands, flags and record kinds) is checked against `<bin> <name> --help` in
contract_cli.PluginCommands.
"""
import re
import unittest
from pathlib import Path

from harness import COMMANDS, ROOT

AGENTS = ROOT / "agents"
SUPERVISOR = AGENTS / "supervisor.md"
SKILL = ROOT / "skills" / "run" / "SKILL.md"
CONTRACT = ROOT / "scripts" / "lib" / "leadhand" / "templates" / "contract.md"

# The contract's names: the actions `next` prints, the kinds `record` takes, the end-of-run gates.
ACTIONS = ("dispatch", "wait", "check_diff", "answer", "stop_worker", "done")
RECORD_KINDS = ("started", "result", "ruling", "park", "diff-check")
GATES = ("final_review", "evidence", "security", "verify")

# `<cli> <command>` with its arguments, up to the end of the inline code span or the line.
CLI_CALL = re.compile(r"<cli> ([a-z][a-z-]*)([^`\n]*)")
FLAG = re.compile(r"(?<![\w-])(--[a-z][a-z-]*)")

TOOLS_READ_BASH = "Read, Grep, Glob, Bash"
EXPECTED = {
    "implementer": {"name": "implementer", "model": "sonnet", "effort": "medium", "maxTurns": "150",
                    "autoCompactWindow": "200000", "experimental": "{cacheTtl: 5m}",
                    "tools": "Read, Edit, Write, Bash, Grep, Glob"},
    "reviewer": {"name": "reviewer", "model": "sonnet", "effort": "medium", "maxTurns": "60",
                 "tools": TOOLS_READ_BASH},
    "verifier": {"name": "verifier", "model": "sonnet", "effort": "medium", "maxTurns": "40",
                 "omitClaudeMd": "true", "tools": TOOLS_READ_BASH},
    "scout": {"name": "scout", "model": "haiku", "effort": "low", "maxTurns": "30",
              "omitClaudeMd": "true", "tools": TOOLS_READ_BASH},
    "codex": {"name": "codex", "model": "haiku", "effort": "low", "maxTurns": "12", "tools": "Bash"},
}
CARRY_CONTRACT = ("implementer", "verifier", "scout")


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


class AgentFiles(unittest.TestCase):
    def test_agent_frontmatter_matches_table(self):
        # The supervisor is not a worker; its frontmatter is checked in SupervisorFiles.
        self.assertEqual(sorted(p.stem for p in AGENTS.glob("*.md")), sorted([*EXPECTED, "supervisor"]))
        for stem, want in EXPECTED.items():
            fields, _ = split(AGENTS / f"{stem}.md")
            desc = fields.pop("description", "")
            self.assertTrue(desc.startswith("leadhand worker:"), stem)
            self.assertIn("leadhand supervisor only", desc, stem)
            self.assertEqual(fields, want, stem)

    def test_contract_carriers_contain_contract_verbatim(self):
        contract = CONTRACT.read_text(encoding="utf-8").strip()
        self.assertTrue(contract)
        for stem in CARRY_CONTRACT:
            _, body = split(AGENTS / f"{stem}.md")
            self.assertIn(contract, body, stem)
        for stem in ("reviewer", "codex"):
            _, body = split(AGENTS / f"{stem}.md")
            self.assertNotIn(contract, body, stem)

    def test_reviewer_rules(self):
        _, body = split(AGENTS / "reviewer.md")
        for phrase in ("Done when", "allowed paths", "Never edit", "Verdict: clean", "Report: <path of the report file>",
                       "implementer's report", "`## Tests`", "throwaway worktree", "never in the workspace"):
            self.assertIn(phrase, body)

    def test_verifier_evidence_rule_follows_the_spec(self):
        # Ruling R46: only a failing assertion on the base counts as failing-before.
        _, body = split(AGENTS / "verifier.md")
        self.assertIn("fails on the base by an assertion", body)
        for phrase in ("compile", "import", "missing module", "did not run"):
            self.assertIn(phrase, body)
        self.assertNotIn("fails (or is absent) on the base", body)

    def test_codex_proxy_runs_one_command(self):
        _, body = split(AGENTS / "codex.md")
        for phrase in ("exactly the one command", "verbatim", "Never run another command",
                       "If the command prints `still running`, run the exact\nsame command again."):
            self.assertIn(phrase, body)


class SupervisorFiles(unittest.TestCase):
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
        for action in ACTIONS:
            self.assertIn(f"`{action}`", body, action)
        for kind in RECORD_KINDS:
            self.assertIn(f"record <run_dir> {kind}", body, kind)
        # The required parts, in the brief's order.
        order = [body.index(h) for h in ("## Preflight", "## The loop", "## Hygiene", "## Steering")]
        self.assertEqual(order, sorted(order))

    def test_run_skill_cli_calls_are_plain_commands(self):
        # The file-only half of test_run_skill_commands_match_cli; the binary's half is
        # contract_cli.PluginCommands.
        _, body = split(SKILL)
        calls = CLI_CALL.findall(body)
        self.assertTrue(calls)
        for name, args in calls:
            self.assertIn(name, COMMANDS, f"<cli> {name}")
            if name == "record":
                kind = args.split()[1]
                self.assertIn(kind, RECORD_KINDS, f"<cli> {name}{args}")
        # P5/R28/R38: CLI calls are plain commands, no exceptions (the sandbox exclusion
        # matched no heredoc in the first live run).
        for name, args in calls:
            plain = re.sub(r"<[a-z_ ]+>", "", args)     # drop placeholders such as <run_dir>
            self.assertNotRegex(plain, r"[|;&<>$]|\bcd\b", f"<cli> {name}{args}")
        self.assertNotIn("<<", body)
        # R38: a worker's reply goes through the action's reply file.
        results = [args for name, args in calls if name == "record" and args.split()[1] == "result"]
        self.assertTrue(any("--reply-file '<reply_file>'" in args for args in results), results)
        self.assertFalse(any("--reply -" in args for args in results), results)

    def test_run_skill_covers_run_level_dispatches(self):
        _, body = split(SKILL)
        text = section(body, "### Run-level dispatches")
        for name in (*GATES, "final_fix", "card", "gate", "wave", "resume"):
            self.assertIn(f"`{name}`", text, name)
        self.assertIn("no `--card`", text)
        dispatch = section(body, "### `dispatch` without `resume`")
        self.assertIn("`card <card> <kind> (attempt <attempt>)`", dispatch)
        self.assertIn("`run <kind> (wave <wave>)`", dispatch)
        # `done` prints the report the action names, not a guessed path.
        done = section(body, "### `done`")
        self.assertIn("the action's `report`", done)
        self.assertNotIn("report.md", done)

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
