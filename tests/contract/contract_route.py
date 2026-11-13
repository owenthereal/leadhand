"""Route resolution, as `route` prints it: the route line, then one reason per line.

Converted from tests/test_route.py. The card comes from tests/fixtures/deck-basic.md (card 1:
Hard: no, Risk: low), changed where a test changes the card. The installed vendors are passed
as `--installed`, as the reference test does.
"""
import unittest

from harness import ROOT, hermetic_env, run_cli, temp_root

FIXTURE = ROOT / "tests" / "fixtures" / "deck-basic.md"
BOTH = "claude,codex"


def fixture_text(*, hard=False, risk="low", pin=None, profile=None) -> str:
    """deck-basic.md with card 1 changed."""
    text = FIXTURE.read_text(encoding="utf-8")
    card1 = "- Hard: no\n- Risk: low\n"
    assert text.count(card1) == 1
    text = text.replace(card1, f"- Hard: {'yes — x' if hard else 'no'}\n- Risk: {risk}\n")
    if pin:
        stop = "- Stop if: the schema has to change\n"
        text = text.replace(stop, f"{stop}- Route: {pin}\n")
    if profile:
        text = text.replace("Test: make test\n", f"Test: make test\nProfile: {profile}\n")
    return text


class RouteTestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = temp_root(self, "lh route ")
        self.env = hermetic_env(self, self.tmp)
        self.decks = 0

    def cfg(self, text):
        """Replace the user configuration file (LEADHAND_CONFIG) with `text`."""
        with open(self.env["LEADHAND_CONFIG"], "w", encoding="utf-8") as f:
            f.write(text)

    def deck(self, **card):
        self.decks += 1
        p = self.tmp / f"deck {self.decks}.md"
        p.write_text(fixture_text(**card), encoding="utf-8")
        return p

    def run_route(self, role="implementer", deck=None, *, attempt=None, last=None, installed=BOTH):
        args = ["route", deck or FIXTURE, "1", role, "--repo", self.tmp, "--installed", installed]
        if attempt is not None:
            args += ["--attempt", attempt]
        if last is not None:
            args += ["--last-vendor", last]
        return run_cli(*args, env=self.env, cwd=self.tmp)

    def r(self, role="implementer", deck=None, **kw) -> tuple[tuple[str, ...], list[str]]:
        """The route as (vendor, model, effort), and the reasons."""
        res = self.run_route(role, deck, **kw).ok()
        lines = res.stdout.splitlines()
        self.assertTrue(lines, res)
        key = tuple(lines[0].split(" "))
        self.assertEqual(len(key), 3, res)
        return key, lines[1:]

    def exhausted(self, res):
        self.assertEqual(res.code, 1, res)
        self.assertEqual(res.stdout, "", res)
        self.assertIn("exhaust", res.stderr, res)


class Route(RouteTestCase):
    def test_standard_implementer_uses_codex_terra_high(self):
        key, reasons = self.r()
        self.assertEqual(key, ("codex", "gpt-6.1-terra", "high"))
        self.assertIn("vendor codex: prefer", reasons)

    def test_hard_card_uses_implementer_hard(self):
        key, reasons = self.r(deck=self.deck(hard=True))
        self.assertEqual(key, ("codex", "gpt-6.1-sol", "high"))
        self.assertIn("role implementer_hard: card is Hard", reasons)

    def test_reviewer_other_vendor_than_implementer(self):
        key, reasons = self.r("reviewer", last="codex")
        self.assertEqual(key, ("claude", "sonnet", "medium"))
        self.assertIn("vendor claude: other than codex", reasons)
        key, _ = self.r("reviewer", last="claude")
        self.assertEqual(key[0], "codex")
        # only one vendor installed: the same vendor is used
        key, _ = self.r("reviewer", last="codex", installed="codex")
        self.assertEqual(key[0], "codex")

    def test_high_risk_reviewer_uses_reviewer_hard_plus_one(self):
        key, reasons = self.r("reviewer", deck=self.deck(risk="high"), last="codex")
        self.assertEqual(key, ("claude", "opus", "xhigh"))
        self.assertIn("risk high: +1", reasons)

    def test_profile_lean_lowers_every_role(self):
        self.cfg('profile = "lean"\n')
        key, reasons = self.r()
        self.assertEqual(key[2], "medium")
        self.assertIn("profile lean: -1", reasons)
        key, _ = self.r("verifier")
        self.assertEqual(key[2], "low")
        # the deck's profile overrides the config's
        key, reasons = self.r(deck=self.deck(profile="thorough"))
        self.assertEqual(key[2], "xhigh")
        self.assertIn("profile thorough: +1", reasons)

    def test_pin_wins_for_first_two_attempts(self):
        pinned = self.deck(pin="claude opus xhigh")
        key, reasons = self.r(deck=pinned)
        self.assertEqual(key, ("claude", "opus", "xhigh"))
        self.assertIn("pinned: claude opus xhigh", reasons)
        key, reasons = self.r(deck=pinned, attempt=2)
        self.assertEqual(key, ("claude", "opus", "xhigh"))
        self.assertIn("clamped to xhigh", reasons)
        maxed = self.deck(pin="claude opus max")
        self.assertEqual(self.r(deck=maxed, attempt=2)[0][2], "max")
        key, _ = self.r(deck=pinned, attempt=3)
        self.assertEqual(key, ("codex", "gpt-6.1-sol", "xhigh"))  # high +1 (pinned)

    def test_ladder_attempts_1_to_4_then_exhausted(self):
        a1, a2, a3, a4 = (self.r(attempt=n)[0] for n in (1, 2, 3, 4))
        self.assertEqual(a1, ("codex", "gpt-6.1-terra", "high"))
        self.assertEqual(a2, ("codex", "gpt-6.1-terra", "xhigh"))
        self.assertEqual(a3, ("codex", "gpt-6.1-sol", "high"))
        self.assertEqual(a4, ("claude", "opus", "high"))
        self.assertIn("attempt 2: +1", self.r(attempt=2)[1])
        self.assertIn("role implementer_hard: attempt 3", self.r(attempt=3)[1])
        self.exhausted(self.run_route(attempt=5))
        # a Hard card gets one more step at attempt 3
        hard = self.deck(hard=True)
        self.assertEqual(self.r(deck=hard, attempt=3)[0][2], "xhigh")
        self.assertEqual(self.r(deck=hard, attempt=4)[0][2], "xhigh")

    def test_attempt_4_with_prefer_other_switches_from_attempt_3s_actual_vendor(self):
        # implementer_hard prefers "other": attempt 4 must leave the vendor attempt 3 really
        # ran on (passed as --last-vendor), not a recomputed guess.
        self.cfg('[roles.implementer_hard]\nprefer = "other"\n')
        for third, fourth in (("claude", "codex"), ("codex", "claude")):
            with self.subTest(attempt_3=third):
                key, reasons = self.r(attempt=4, last=third)
                self.assertEqual(key[0], fourth)
                self.assertIn(f"vendor {fourth}: other than {third}", reasons)

    def test_claude_only_install_exhausts_at_attempt_4(self):
        key, reasons = self.r(attempt=1, installed="claude")
        self.assertEqual(key, ("claude", "sonnet", "medium"))
        self.assertIn("vendor claude: codex not installed", reasons)
        self.assertEqual(self.r(attempt=3, installed="claude")[0][0], "claude")
        self.exhausted(self.run_route(attempt=4, installed="claude"))
        # no vendor installed: a configuration error
        res = self.run_route(installed="")
        self.assertEqual(res.code, 1, res)
        self.assertEqual(res.stdout, "", res)
        self.assertTrue(res.stderr.startswith("error: "), res)

    def test_effort_clamped_to_xhigh_unless_pinned_max(self):
        # shift_effort's own cases are a Rust unit test; these are the resolutions.
        self.cfg('profile = "thorough"\n')
        key, reasons = self.r("implementer_hard", deck=self.deck(risk="high"))   # high +1 +1 = max -> xhigh
        self.assertEqual(key[2], "xhigh")
        self.assertIn("clamped to xhigh", reasons)
        key, reasons = self.r(deck=self.deck(risk="high", pin="codex gpt-6.1-sol max"))
        self.assertEqual(key[2], "max")
        self.assertNotIn("clamped to xhigh", reasons)

    def test_pin_requires_installed_vendor(self):
        res = self.run_route(deck=self.deck(pin="codex gpt-6.1-sol high"), installed="claude")
        self.assertEqual(res.code, 1, res)
        self.assertRegex(res.stderr, "^error: .*pinned to codex, which is not installed")

    def test_invalid_profile_is_config_error(self):
        res = self.run_route(deck=self.deck(profile="Thorough"))
        self.assertEqual(res.code, 1, res)
        self.assertTrue(res.stderr.startswith("error: "), res)
        self.assertIn("Thorough", res.stderr)

    def test_low_pin_not_shifted_by_profile_or_risk(self):
        self.cfg('profile = "thorough"\n')
        key, _ = self.r(deck=self.deck(risk="high", pin="claude sonnet low"))
        self.assertEqual(key, ("claude", "sonnet", "low"))

    def test_cli_route_prints_route_and_reasons(self):
        r = self.run_route().ok()
        lines = r.stdout.splitlines()
        self.assertEqual(lines[0], "codex gpt-6.1-terra high", r)
        self.assertIn("vendor codex: prefer", lines[1:])
        r = self.run_route(attempt=5)
        self.assertEqual(r.code, 1, r)
        self.assertEqual(r.stdout, "", r)
        self.assertIn("exhaust", r.stderr)
        r = run_cli("route", FIXTURE, "99", "implementer", "--repo", self.tmp, "--installed", BOTH,
                    env=self.env, cwd=self.tmp)
        self.assertEqual(r.code, 1, r)
        self.assertIn("no card 99", r.stderr)


if __name__ == "__main__":
    unittest.main()
