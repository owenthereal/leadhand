import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
AGENTS = ROOT / "agents"
CONTRACT = ROOT / "scripts" / "lib" / "leadhand" / "templates" / "contract.md"

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


class AgentTests(unittest.TestCase):
    def test_agent_frontmatter_matches_table(self):
        self.assertEqual(sorted(p.stem for p in AGENTS.glob("*.md")), sorted(EXPECTED))
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

    def test_codex_proxy_runs_one_command(self):
        _, body = split(AGENTS / "codex.md")
        for phrase in ("exactly the one command", "verbatim", "Never run another command",
                       "If the command prints `still running`, run the exact\nsame command again."):
            self.assertIn(phrase, body)


if __name__ == "__main__":
    unittest.main()
