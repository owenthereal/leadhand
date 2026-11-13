"""The deck format and its checks, as `check` (one problem per line) and `route` report them,
and the parsed card as the worker's brief renders it ("The card, verbatim").

Converted from tests/test_deck.py. Dependency order, the digest and the spec example's header
are catalogued as Rust unit tests.
"""
import unittest

from pathlib import Path

from gitfixture import commit, make_repo
from harness import ROOT, hermetic_env, run_cli, temp_root
from supervisor import FakeSupervisor, card

FIXTURE = ROOT / "tests" / "fixtures" / "deck-basic.md"

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


def card_section(brief: str, number: int) -> list[str]:
    """The lines of card `number` as the brief renders it, from its heading to the next heading."""
    lines = brief.splitlines()
    start = lines.index(next(line for line in lines if line.startswith(f"## Card {number}:")))
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("#")), len(lines))
    return lines[start:end]


class Briefs(unittest.TestCase):
    """What the parser took from a card, read back from the card's first implement brief."""

    def brief_card(self, cards, number, **kw) -> list[str]:
        w = FakeSupervisor(self, cards, **kw)
        w.start()
        for _ in range(20):
            a = w.next()
            if a["action"] == "dispatch" and a["card"] == number and a["kind"] == "implement":
                return card_section(Path(a["brief"]).read_text(encoding="utf-8"), number)
            self.assertNotEqual(a["action"], "done", a)
            w.act(a)
        self.fail(f"card {number} was never dispatched")

    def test_basic_fixture_cards_reach_the_brief(self):
        text = FIXTURE.read_text(encoding="utf-8")
        cards = ["## Card " + part for part in text.split("\n## Card ")[1:]]
        self.assertEqual(len(cards), 2)
        one = self.brief_card(cards, 1)
        self.assertIn("- Hard: no", one)
        self.assertIn("- Allowed paths: src/, tests/", one)
        self.assertEqual(one[-1], "see plan Task 1.", one)
        two = self.brief_card(cards, 2)
        self.assertIn("- Hard: no", two)
        self.assertIn("- Depends on: 1", two)
        self.assertFalse([line for line in two if line.startswith("- Route:")], two)
        self.assertEqual(len([line for line in two if line.startswith("- Inferred: ")]), 2, two)
        # The requirement maps to both cards, and the base is the header's commit.
        tmp = temp_root(self, "lh deck ")
        env = hermetic_env(self, tmp)
        repo = make_repo(tmp)
        r = run_cli("check", FIXTURE, "--repo", repo, env=env, cwd=tmp)
        self.assertIn(f"error: base commit 3f2a9c1 not found in {repo.resolve()}", r.stdout.splitlines(), r)
        self.assertFalse([line for line in r.stdout.splitlines() if "requirement" in line], r)

    def test_hard_yes_reason_is_rendered(self):
        for sep in ("—", "–", "-", ":"):
            with self.subTest(sep=sep):
                one = card(1).replace("- Hard: no\n", f"- Hard: yes {sep} no test can check it\n")
                self.assertIn("- Hard: yes — no test can check it", self.brief_card([one], 1))

    def test_fields_after_context_are_parsed(self):
        two = (card(2).replace("- Depends on: none\n", "")
               + "\nContext: see plan.\n- Depends on: 1\n- Route: codex gpt-6.1-sol xhigh\n"
               "- Inferred: one\n- Inferred: two\n")
        lines = self.brief_card([card(1), two], 2, installed=("claude", "codex"))
        self.assertIn("- Depends on: 1", lines)
        self.assertIn("- Route: codex gpt-6.1-sol xhigh", lines)
        self.assertEqual([line for line in lines if line.startswith("- Inferred: ")],
                         ["- Inferred: one", "- Inferred: two"])
        self.assertEqual(lines[-2:], ["", "see plan."], lines)

    def test_unknown_key_bullet_is_context(self):
        lines = self.brief_card([card(1) + "\n- Files: src/a.py\n"], 1)
        self.assertEqual(lines[-2:], ["", "- Files: src/a.py"], lines)


if __name__ == "__main__":
    unittest.main()
