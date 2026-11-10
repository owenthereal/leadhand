import os
import subprocess
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path

from leadhand.config import ConfigError, Route, load_config
from leadhand.deck import parse_deck
from leadhand.route import LadderExhausted, resolve, shift_effort

ROOT = Path(__file__).resolve().parent.parent
CLI = str(ROOT / "scripts" / "leadhand")
FIXTURE = ROOT / "tests" / "fixtures" / "deck-basic.md"


class RouteTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.cfgfile = self.dir / "user.toml"
        self.cfgfile.write_text("")
        self.config = load_config(self.dir, user_file=self.cfgfile)
        self.card = parse_deck(FIXTURE).cards[0]  # Hard: no, Risk: low

    def cfg(self, text):
        self.cfgfile.write_text(text)
        return load_config(self.dir, user_file=self.cfgfile)

    def r(self, role="implementer", card=None, **kw):
        return resolve(self.config, role=role, card=card or self.card, **kw)

    def key(self, res):
        return (res.route.vendor, res.route.model, res.route.effort)

    def test_standard_implementer_uses_codex_terra_high(self):
        res = self.r()
        self.assertEqual(self.key(res), ("codex", "gpt-6.1-terra", "high"))
        self.assertEqual(res.role, "implementer")
        self.assertIn("vendor codex: prefer", res.reasons)

    def test_hard_card_uses_implementer_hard(self):
        res = self.r(card=replace(self.card, hard=True, hard_reason="x"))
        self.assertEqual(res.role, "implementer_hard")
        self.assertEqual(self.key(res), ("codex", "gpt-6.1-sol", "high"))
        self.assertIn("role implementer_hard: card is Hard", res.reasons)

    def test_reviewer_other_vendor_than_implementer(self):
        res = self.r("reviewer", last_implementer_vendor="codex")
        self.assertEqual(self.key(res), ("claude", "sonnet", "medium"))
        self.assertIn("vendor claude: other than codex", res.reasons)
        res = self.r("reviewer", last_implementer_vendor="claude")
        self.assertEqual(res.route.vendor, "codex")
        # only one vendor installed: the same vendor is used
        res = self.r("reviewer", last_implementer_vendor="codex", installed=frozenset({"codex"}))
        self.assertEqual(res.route.vendor, "codex")

    def test_high_risk_reviewer_uses_reviewer_hard_plus_one(self):
        res = self.r("reviewer", card=replace(self.card, risk="high"), last_implementer_vendor="codex")
        self.assertEqual(res.role, "reviewer_hard")
        self.assertEqual(self.key(res), ("claude", "opus", "xhigh"))
        self.assertIn("risk high: +1", res.reasons)

    def test_security_uses_reviewer_hard(self):
        res = self.r("reviewer", security=True, last_implementer_vendor="claude")
        self.assertEqual(res.role, "reviewer_hard")
        self.assertEqual(self.key(res), ("codex", "gpt-6.1-sol", "high"))
        res = resolve(self.config, role="reviewer", security=True)
        self.assertEqual(res.role, "reviewer_hard")

    def test_profile_lean_lowers_every_role(self):
        lean = self.cfg('profile = "lean"\n')
        self.assertEqual(lean.profile, "lean")
        res = resolve(lean, role="implementer", card=self.card)
        self.assertEqual(res.route.effort, "medium")
        self.assertIn("profile lean: -1", res.reasons)
        res = resolve(lean, role="verifier")
        self.assertEqual(res.route.effort, "low")
        # the deck's profile overrides the config's
        res = resolve(lean, role="implementer", card=self.card, profile="thorough")
        self.assertEqual(res.route.effort, "xhigh")
        self.assertIn("profile thorough: +1", res.reasons)

    def test_pin_wins_for_first_two_attempts(self):
        card = replace(self.card, route_pin=Route("claude", "opus", "xhigh"))
        res = self.r(card=card)
        self.assertEqual(self.key(res), ("claude", "opus", "xhigh"))
        self.assertIn("pinned: claude opus xhigh", res.reasons)
        res = self.r(card=card, attempt=2)
        self.assertEqual(self.key(res), ("claude", "opus", "max"))
        res = self.r(card=card, attempt=3)
        self.assertEqual(self.key(res), ("codex", "gpt-6.1-sol", "xhigh"))  # high +1 (pinned)

    def test_ladder_attempts_1_to_4_then_exhausted(self):
        a1, a2, a3, a4 = (self.key(self.r(attempt=n)) for n in (1, 2, 3, 4))
        self.assertEqual(a1, ("codex", "gpt-6.1-terra", "high"))
        self.assertEqual(a2, ("codex", "gpt-6.1-terra", "xhigh"))
        self.assertEqual(a3, ("codex", "gpt-6.1-sol", "high"))
        self.assertEqual(a4, ("claude", "opus", "high"))
        self.assertIn("attempt 2: +1", self.r(attempt=2).reasons)
        self.assertIn("role implementer_hard: attempt 3", self.r(attempt=3).reasons)
        with self.assertRaises(LadderExhausted):
            self.r(attempt=5)
        # a Hard card gets one more step at attempt 3
        hard = replace(self.card, hard=True, hard_reason="x")
        self.assertEqual(self.r(card=hard, attempt=3).route.effort, "xhigh")
        self.assertEqual(self.r(card=hard, attempt=4).route.effort, "xhigh")

    def test_claude_only_install_exhausts_at_attempt_4(self):
        only = frozenset({"claude"})
        res = self.r(attempt=1, installed=only)
        self.assertEqual(self.key(res), ("claude", "sonnet", "medium"))
        self.assertIn("vendor claude: codex not installed", res.reasons)
        self.assertEqual(self.r(attempt=3, installed=only).route.vendor, "claude")
        with self.assertRaises(LadderExhausted):
            self.r(attempt=4, installed=only)
        with self.assertRaises(ConfigError):
            self.r(installed=frozenset())

    def test_effort_clamped_to_xhigh_unless_pinned_max(self):
        self.assertEqual(shift_effort("xhigh", 1), "xhigh")
        self.assertEqual(shift_effort("xhigh", 1, allow_max=True), "max")
        self.assertEqual(shift_effort("low", -2), "low")
        self.assertEqual(shift_effort("max", 0), "xhigh")
        thorough = self.cfg('profile = "thorough"\n')
        card = replace(self.card, risk="high")
        res = resolve(thorough, role="implementer_hard", card=card)  # high +1 +1 = max -> xhigh
        self.assertEqual(res.route.effort, "xhigh")
        self.assertIn("clamped to xhigh", res.reasons)
        pinned = replace(card, route_pin=Route("codex", "gpt-6.1-sol", "max"))
        res = resolve(thorough, role="implementer", card=pinned)
        self.assertEqual(res.route.effort, "max")
        self.assertNotIn("clamped to xhigh", res.reasons)

    def test_cli_route_prints_route_and_reasons(self):
        env = dict(os.environ, LEADHAND_CONFIG=str(self.cfgfile))
        args = [sys.executable, CLI, "route", str(FIXTURE), "1", "implementer",
                "--repo", str(self.dir), "--installed", "claude,codex"]
        r = subprocess.run(args, capture_output=True, text=True, env=env, cwd=self.dir)
        self.assertEqual(r.returncode, 0, r.stderr)
        lines = r.stdout.splitlines()
        self.assertEqual(lines[0], "codex gpt-6.1-terra high")
        self.assertIn("vendor codex: prefer", lines[1:])
        r = subprocess.run(args + ["--attempt", "5"], capture_output=True, text=True, env=env, cwd=self.dir)
        self.assertEqual(r.returncode, 1)
        self.assertEqual(r.stdout, "")
        self.assertIn("exhaust", r.stderr)
        r = subprocess.run(args[:4] + ["99"] + args[5:], capture_output=True, text=True, env=env, cwd=self.dir)
        self.assertEqual(r.returncode, 1)
        self.assertIn("no card 99", r.stderr)


if __name__ == "__main__":
    unittest.main()
