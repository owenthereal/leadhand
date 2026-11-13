"""The deck format and its checks, as `check` (one problem per line) and `route` report them.

Converted from tests/test_deck.py. The parser-only tests (parsed fields, dependency order,
digest) are catalogued as Rust unit tests.
"""
import unittest

from gitfixture import commit, make_repo
from harness import hermetic_env, run_cli, temp_root

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


def problems(result) -> list[tuple[str, str]]:
    """`check`'s problem lines as (level, message); its closing `ok:` line is not a problem."""
    out = []
    for line in result.stdout.splitlines():
        level, sep, message = line.partition(": ")
        if sep and level in ("error", "warning"):
            out.append((level, message))
    return out


class DeckTestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = temp_root(self, "lh deck ")
        self.env = hermetic_env(self, self.tmp)
        self.repos = 0

    def configure(self, text):
        """Append `text` to the user configuration file (LEADHAND_CONFIG)."""
        with open(self.env["LEADHAND_CONFIG"], "a", encoding="utf-8") as f:
            f.write(text)

    def write(self, text, name="deck.md"):
        p = self.tmp / name
        p.write_text(text, encoding="utf-8")
        return p

    def make_repo(self):
        self.repos += 1
        repo = make_repo(self.tmp, name=f"repo {self.repos}")
        return repo, commit(repo, {"src/a.txt": "a"}, "init")

    def check(self, deck, repo=None):
        repo = repo or self.make_repo()[0]
        return run_cli("check", deck, "--repo", repo, env=self.env, cwd=self.tmp)

    def messages(self, deck, repo=None):
        r = self.check(deck, repo)
        self.assertIn(r.code, (0, 1), r)
        return problems(r)

    def route(self, deck, card, role="implementer"):
        return run_cli("route", deck, card, role, "--repo", self.tmp, "--installed", "claude,codex",
                       env=self.env, cwd=self.tmp)


class Deck(DeckTestCase):
    def test_hard_yes_requires_reason(self):
        d = self.write(deck_text([(1, "implementer", "no", "none"), (3, "implementer", "yes", "1")]))
        self.assertIn(("error", "card 3: Hard: yes needs a reason"), self.messages(d))

    def test_hard_yes_with_reason_forms(self):
        # The reason itself goes into the brief; here the card counts as Hard (route) and the
        # reason is found (check).
        for sep in ("—", "–", "-", ":"):
            with self.subTest(sep=sep):
                d = self.write(deck_text([(1, "implementer", f"yes {sep} no test can check it", "none")]))
                msgs = self.messages(d)
                self.assertNotIn(("error", "card 1: Hard: yes needs a reason"), msgs)
                self.assertFalse([m for l, m in msgs if m.startswith("card 1:")], msgs)
                r = self.route(d, 1).ok()
                self.assertIn("role implementer_hard: card is Hard", r.stdout.splitlines()[1:], r)

    def test_fields_after_context_are_parsed(self):
        text = (deck_text([(1, "implementer", "no", "none"), (2, "implementer", "no", "none")])
                .replace("- Depends on: none\n- Source: plan\n- Allowed paths: src/\n- Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n", "- Source: plan\n- Allowed paths: src/\n- Out of scope: nothing\n- Done when: tests pass\n- Stop if: stuck\n")
                + "\nContext: see plan.\n- Depends on: 1\n- Route: codex gpt-6.1-sol xhigh\n"
                "- Inferred: one\n- Inferred: two\n")
        d = self.write(text)
        msgs = self.messages(d)
        # Depends on was taken from after the context: card 2 is not missing it.
        self.assertNotIn(("error", "card 2: missing Depends on"), msgs)
        self.assertFalse([m for l, m in msgs if m.startswith("card 2:")], msgs)
        r = self.route(d, 2).ok()
        lines = r.stdout.splitlines()
        self.assertEqual(lines[0], "codex gpt-6.1-sol xhigh", r)
        self.assertIn("pinned: codex gpt-6.1-sol xhigh", lines[1:], r)

    def test_unknown_key_bullet_warns(self):
        text = deck_text([(1, "implementer", "no", "none")]) + "\n- Dependson: 2\n- Files: src/a.py\n"
        p = self.write(text)
        base = len(text.splitlines()) - 1
        msgs = self.messages(p)
        self.assertIn(("warning", f'card 1: line {base}: "Dependson" is not a card field; treated as context'), msgs)
        self.assertIn(("warning", f'card 1: line {base + 1}: "Files" is not a card field; treated as context'), msgs)
        self.assertFalse([m for l, m in msgs if l == "error" and "card field" in m])

    def test_scout_card_rejected_in_this_version(self):
        d = self.write(deck_text([(1, "scout", "no", "none")]))
        self.assertTrue(any("not supported in this version: scout cards" in m for l, m in self.messages(d) if l == "error"))

    def test_unsupported_config_reported_as_errors(self):
        self.configure('[run]\ncost_limit_usd = 5\n')
        d = self.write(deck_text([(1, "implementer", "no", "none")]))
        self.assertIn(("error", "not supported in this version: run.cost_limit_usd = 5"), self.messages(d))

    def test_dependency_cycle_is_error(self):
        d = self.write(deck_text([(1, "implementer", "no", "2"), (2, "implementer", "no", "1")]))
        self.assertTrue(any(l == "error" and "cycle" in m for l, m in self.messages(d)))

    def test_unknown_dependency_and_duplicates_and_finish_and_build(self):
        text = deck_text([(1, "implementer", "no", "9"), (1, "implementer", "no", "none")],
                         build="", header_extra="Finish: merge\n")
        msgs = self.messages(self.write(text))
        joined = "\n".join(m for l, m in msgs if l == "error")
        for needle in ("unknown card 9", "duplicate card number 1", "Finish", "Build"):
            self.assertIn(needle, joined)

    def test_requirement_to_missing_card_is_error(self):
        d = self.write(deck_text([(1, "implementer", "no", "none")], requirements="- R1 Thing → cards 1, 7\n"))
        self.assertTrue(any(l == "error" and "card 7" in m for l, m in self.messages(d)))

    def test_missing_required_fields_are_errors(self):
        text = deck_text([(1, "implementer", "no", "none")]).replace("- Stop if: stuck\n", "")
        msgs = self.messages(self.write(text))
        self.assertIn(("error", "card 1: missing Stop if"), msgs)

    def test_route_pin_parsed(self):
        text = deck_text([(1, "implementer", "no", "none")]) + "- Route: codex gpt-6.1-sol xhigh\n"
        r = self.route(self.write(text), 1).ok()
        lines = r.stdout.splitlines()
        self.assertEqual(lines[0], "codex gpt-6.1-sol xhigh", r)
        self.assertIn("pinned: codex gpt-6.1-sol xhigh", lines[1:], r)

    def test_parse_error_reports_line_number(self):
        text = deck_text([(1, "implementer", "no", "none")]).replace("Risk: low", "Risk: severe")
        r = self.check(self.write(text))
        self.assertEqual(r.code, 1, r)
        line = text.splitlines().index("- Risk: severe") + 1
        self.assertTrue(r.stdout.startswith(f"error: line {line}: "), r)

    def test_uncovered_requirement_is_warning(self):
        d = self.write(deck_text([(1, "implementer", "no", "none")],
                                 requirements="- R1 Thing → cards 1\n- R2 Other → cards none\n"))
        msgs = self.messages(d)
        self.assertTrue(any(l == "warning" and "R2" in m for l, m in msgs))
        self.assertFalse(any(l == "error" for l, m in msgs if "R2" in m))

    def test_base_commit_and_paths(self):
        repo, sha = self.make_repo()
        good = deck_text([(1, "implementer", "no", "none")]).replace("abc1234", sha)
        msgs = self.messages(self.write(good), repo)
        self.assertFalse([m for l, m in msgs if l == "error"])
        bad = deck_text([(1, "implementer", "no", "none")])
        self.assertTrue(any(l == "error" and "base commit" in m for l, m in self.messages(self.write(bad), repo)))
        gone = good.replace("Allowed paths: src/", "Allowed paths: nope/")
        self.assertTrue(any(l == "warning" and "nope/" in m for l, m in self.messages(self.write(gone), repo)))

    def test_check_cli_exits_1_on_error(self):
        repo, sha = self.make_repo()
        good = self.write(deck_text([(1, "implementer", "no", "none")]).replace("abc1234", sha), "good.md")
        bad = self.write(deck_text([(1, "scout", "no", "none")]).replace("abc1234", sha), "bad.md")
        r = self.check(good, repo)
        self.assertEqual(r.code, 0, r)
        r = self.check(bad, repo)
        self.assertEqual(r.code, 1, r)
        self.assertIn("scout", r.stdout)


if __name__ == "__main__":
    unittest.main()
