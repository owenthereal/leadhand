import os
import subprocess
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path

from leadhand.config import load_config
from leadhand.deck import DeckError, check_deck, dependency_order, parse_deck

ROOT = Path(__file__).resolve().parent.parent
CLI = str(ROOT / "scripts" / "leadhand")
FIXTURE = ROOT / "tests" / "fixtures" / "deck-basic.md"

SPEC_EXAMPLE = """# Deck: Retry uploads
Source: docs/plans/2026-11-09-retry.md
Base: origin/main @ 3f2a9c1      Branch: leadhand/retry-uploads
Build: make build                 Test: make test
Profile: standard                 Finish: none

## Requirements
- R1 Uploads retry on transient errors (plan: Goal) → cards 2, 3
- R2 Retries are bounded and logged (plan: Task 3) → card 3

## Card 3: Retry uploads on transient errors
- Covers: R1, R2
- Role: implementer
- Hard: no
- Risk: medium
- Depends on: 2
- Source: plan Task 3
- Allowed paths: src/sync/, tests/sync/
- Out of scope: the upload queue
- Done when: tests/sync/retry.rs passes; full test suite passes
- Stop if: the client API has to change; two approaches fail
- Inferred: "Done when" test name (plan named no test)

Context: pointers to the plan section and files, not copies.
"""

CARD = """
## Card {n}: Title {n}
- Covers: R1
- Role: {role}
- Hard: {hard}
- Risk: low
- Depends on: {deps}
- Source: plan
- Allowed paths: src/
- Out of scope: nothing
- Done when: tests pass
- Stop if: stuck
"""


def deck_text(cards, requirements="- R1 Thing → cards 1\n", build="make build", header_extra=""):
    head = (f"# Deck: T\nSource: s\nBase: main @ abc1234\nBranch: b\n"
            f"Build: {build}\nTest: make test\n{header_extra}\n## Requirements\n{requirements}")
    return head + "".join(CARD.format(n=n, role=r, hard=h, deps=d) for n, r, h, d in cards)


class DeckTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.cfgfile = self.dir / "user.toml"
        self.cfgfile.write_text("")
        self.config = load_config(self.dir, user_file=self.cfgfile)

    def write(self, text, name="deck.md"):
        p = self.dir / name
        p.write_text(text, encoding="utf-8")
        return p

    def make_repo(self):
        repo = self.dir / "repo"
        repo.mkdir()
        git = ["git", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null",
               "-c", "user.name=T", "-c", "user.email=t@example.com"]
        subprocess.run(git + ["init", "-q"], cwd=repo, check=True)
        (repo / "src").mkdir()
        (repo / "src" / "a.txt").write_text("a")
        subprocess.run(git + ["add", "."], cwd=repo, check=True)
        subprocess.run(git + ["commit", "-q", "-m", "init"], cwd=repo, check=True)
        sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, check=True,
                             capture_output=True, text=True).stdout.strip()
        return repo, sha

    def messages(self, deck, repo=None):
        repo = repo or self.make_repo()[0]
        return [(p.level, p.message) for p in check_deck(deck, repo, self.config)]

    def test_parse_basic_fixture(self):
        d = parse_deck(FIXTURE)
        self.assertEqual(len(d.cards), 2)
        self.assertEqual(d.cards[1].depends_on, (1,))
        self.assertIs(d.cards[1].hard, False)
        self.assertIsNone(d.cards[1].route_pin)
        self.assertEqual(d.cards[0].allowed_paths, ("src/", "tests/"))
        self.assertEqual(d.requirements[0].id, "R1")
        self.assertEqual(d.requirements[0].cards, (1, 2))
        self.assertEqual(d.cards[0].context, "see plan Task 1.")
        self.assertEqual(len(d.cards[1].inferred), 2)
        self.assertEqual((d.base_ref, d.base_sha), ("main", "3f2a9c1"))

    def test_spec_example_header_parses(self):
        d = parse_deck(self.write(SPEC_EXAMPLE))
        self.assertEqual((d.base_ref, d.base_sha, d.branch), ("origin/main", "3f2a9c1", "leadhand/retry-uploads"))
        self.assertEqual((d.build, d.test, d.profile, d.finish), ("make build", "make test", "standard", "none"))
        self.assertEqual([r.cards for r in d.requirements], [(2, 3), (3,)])
        self.assertEqual(d.cards[0].number, 3)
        self.assertEqual(d.cards[0].covers, ("R1", "R2"))
        self.assertEqual(d.cards[0].stop_if, "the client API has to change; two approaches fail")

    def test_hard_yes_requires_reason(self):
        d = parse_deck(self.write(deck_text([(1, "implementer", "no", "none"), (3, "implementer", "yes", "1")])))
        self.assertIn(("error", "card 3: Hard: yes needs a reason"), self.messages(d))

    def test_hard_yes_with_reason_forms(self):
        for sep in ("—", "–", "-", ":"):
            d = parse_deck(self.write(deck_text([(1, "implementer", f"yes {sep} no test can check it", "none")])))
            self.assertTrue(d.cards[0].hard)
            self.assertEqual(d.cards[0].hard_reason, "no test can check it")

    def test_fields_after_context_are_parsed(self):
        text = (deck_text([(1, "implementer", "no", "none"), (2, "implementer", "no", "none")])
                .replace("- Depends on: none\n- Source: plan\n- Allowed paths: src/\n- Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n", "- Source: plan\n- Allowed paths: src/\n- Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n")
                + "\nContext: see plan.\n- Depends on: 1\n- Route: codex gpt-6.1-sol xhigh\n"
                "- Inferred: one\n- Inferred: two\n")
        c = parse_deck(self.write(text)).cards[1]
        self.assertEqual(c.depends_on, (1,))
        self.assertEqual(c.route_pin.model, "gpt-6.1-sol")
        self.assertEqual(c.inferred, ("one", "two"))
        self.assertEqual(c.context, "see plan.")

    def test_unknown_key_bullet_is_context(self):
        text = deck_text([(1, "implementer", "no", "none")]) + "\n- Files: src/a.py\n"
        c = parse_deck(self.write(text)).cards[0]
        self.assertEqual(c.context, "- Files: src/a.py")

    def test_scout_card_rejected_in_this_version(self):
        d = parse_deck(self.write(deck_text([(1, "scout", "no", "none")])))
        self.assertTrue(any("not supported in this version: scout cards" in m for l, m in self.messages(d) if l == "error"))

    def test_unsupported_config_reported_as_errors(self):
        self.cfgfile.write_text('[run]\ncost_limit_usd = 5\n')
        self.config = load_config(self.dir, user_file=self.cfgfile)
        d = parse_deck(self.write(deck_text([(1, "implementer", "no", "none")])))
        self.assertIn(("error", "not supported in this version: run.cost_limit_usd = 5"), self.messages(d))

    def test_dependency_cycle_is_error(self):
        d = parse_deck(self.write(deck_text([(1, "implementer", "no", "2"), (2, "implementer", "no", "1")])))
        self.assertTrue(any(l == "error" and "cycle" in m for l, m in self.messages(d)))

    def test_unknown_dependency_and_duplicates_and_finish_and_build(self):
        text = deck_text([(1, "implementer", "no", "9"), (1, "implementer", "no", "none")],
                         build="", header_extra="Finish: merge\n")
        msgs = self.messages(parse_deck(self.write(text)))
        joined = "\n".join(m for l, m in msgs if l == "error")
        for needle in ("unknown card 9", "duplicate card number 1", "Finish", "Build"):
            self.assertIn(needle, joined)

    def test_requirement_to_missing_card_is_error(self):
        d = parse_deck(self.write(deck_text([(1, "implementer", "no", "none")], requirements="- R1 Thing → cards 1, 7\n")))
        self.assertTrue(any(l == "error" and "card 7" in m for l, m in self.messages(d)))

    def test_missing_required_fields_are_errors(self):
        text = deck_text([(1, "implementer", "no", "none")]).replace("- Stop if: stuck\n", "")
        msgs = self.messages(parse_deck(self.write(text)))
        self.assertIn(("error", "card 1: missing Stop if"), msgs)

    def test_dependency_order_is_stable_topological(self):
        d = parse_deck(self.write(deck_text([
            (3, "implementer", "no", "1"), (1, "implementer", "no", "none"),
            (2, "implementer", "no", "none"), (4, "implementer", "no", "3, 2")])))
        self.assertEqual(dependency_order(d), [1, 2, 3, 4])

    def test_route_pin_parsed(self):
        text = deck_text([(1, "implementer", "no", "none")]) + "- Route: codex gpt-6.1-sol xhigh\n"
        d = parse_deck(self.write(text))
        r = d.cards[0].route_pin
        self.assertEqual((r.vendor, r.model, r.effort), ("codex", "gpt-6.1-sol", "xhigh"))

    def test_digest_changes_when_file_changes(self):
        p = self.write(deck_text([(1, "implementer", "no", "none")]))
        a = parse_deck(p).digest
        p.write_text(p.read_text() + "\nmore\n")
        self.assertNotEqual(a, parse_deck(p).digest)

    def test_parse_error_reports_line_number(self):
        text = deck_text([(1, "implementer", "no", "none")]).replace("Risk: low", "Risk: severe")
        with self.assertRaises(DeckError) as cm:
            parse_deck(self.write(text))
        line = text.splitlines().index("- Risk: severe") + 1
        self.assertEqual(cm.exception.line, line)

    def test_uncovered_requirement_is_warning(self):
        d = parse_deck(self.write(deck_text([(1, "implementer", "no", "none")],
                                            requirements="- R1 Thing → cards 1\n- R2 Other → cards none\n")))
        msgs = self.messages(d)
        self.assertTrue(any(l == "warning" and "R2" in m for l, m in msgs))
        self.assertFalse(any(l == "error" for l, m in msgs if "R2" in m))

    def test_base_commit_and_paths(self):
        repo, sha = self.make_repo()
        good = deck_text([(1, "implementer", "no", "none")]).replace("abc1234", sha)
        msgs = self.messages(parse_deck(self.write(good)), repo)
        self.assertFalse([m for l, m in msgs if l == "error"])
        bad = deck_text([(1, "implementer", "no", "none")])
        self.assertTrue(any(l == "error" and "base commit" in m for l, m in self.messages(parse_deck(self.write(bad)), repo)))
        gone = good.replace("Allowed paths: src/", "Allowed paths: nope/")
        self.assertTrue(any(l == "warning" and "nope/" in m for l, m in self.messages(parse_deck(self.write(gone)), repo)))

    def test_check_cli_exits_1_on_error(self):
        repo, sha = self.make_repo()
        env = dict(os.environ, LEADHAND_CONFIG=str(self.cfgfile))
        good = self.write(deck_text([(1, "implementer", "no", "none")]).replace("abc1234", sha), "good.md")
        bad = self.write(deck_text([(1, "scout", "no", "none")]).replace("abc1234", sha), "bad.md")
        run = lambda p: subprocess.run([sys.executable, CLI, "check", str(p), "--repo", str(repo)],
                                       capture_output=True, text=True, env=env, cwd=ROOT)
        r = run(good)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        r = run(bad)
        self.assertEqual(r.returncode, 1)
        self.assertIn("scout", r.stdout)


if __name__ == "__main__":
    unittest.main()
