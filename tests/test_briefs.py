import shlex
import shutil
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path

from gitfixture import hermetic, make_repo, run_git
from leadhand.briefs import (
    BRIEF_KINDS, CONTRACT_POINTER, ROLE_AGENT, TEMPLATES, agent_for, dispatch_message, parse_report_head,
    scratch_dir, worker_prompt, write_brief,
)
from leadhand.config import Route
from leadhand.deck import Card, Deck

CONTRACT = (TEMPLATES / "contract.md").read_text(encoding="utf-8").strip()


def make_card(**kw):
    fields = dict(
        number=3, title="Parse the widget header", role="implementer", hard=True,
        hard_reason="touches the on-disk format", risk="high", depends_on=(1, 2),
        covers=("R1", "R4"), source="docs/plan.md#task-3",
        allowed_paths=("src/widget/**", "tests/test_widget.py"),
        out_of_scope="Rendering; the CLI flags.",
        done_when="`test_header_roundtrip` passes and `make check` is clean.",
        stop_if="the header needs a new version byte.",
        inferred=('"Done when" test name (plan named no test)',),
        route_pin=Route("codex", "gpt-6.1-sol", "xhigh"),
        context="The header is 16 bytes, little-endian.\nKeep the parser pure.",
        missing=("SENTINEL-missing",), line=4242, unknown_keys=((77, "SENTINEL-unknown"),),
    )
    fields.update(kw)
    return Card(**fields)


def make_deck(path, cards):
    return Deck(
        path=path, title="Widget work", source="docs/plan.md", base_ref="main",
        base_sha="a" * 40, branch="leadhand/widget", build="make build", test="make test",
        profile=None, finish=None, requirements=(), cards=tuple(cards), digest="0" * 64,
    )


EXTRA = {
    "fix": {"findings": "/x/findings.md", "round": "2"},
    "answer": {"ruling": "Use option B: keep the old byte order."},
    "integrate": {"run_tip": "c" * 40, "conflicts": "src/widget/header.py", "card_intent": "add a field",
                  "run_intent": "rename the struct"},
    "review": {"diff_range": "abc123..def456", "implementer_report": "/x/impl report.md"},
    "rereview": {"diff_range": "abc123..def456", "findings": "/x/findings.md",
                 "implementer_report": "/x/impl report.md"},
    "final_review": {"diff_range": "base0..tip9", "cards": "3"},
    "final_fix": {"gate": "security", "findings": "/x/security.md", "branch": "leadhand/widget"},
    "security": {"diff_range": "base0..tip9"},
    "evidence": {"base_worktree": "/x/base tree", "cards": "3"},
}


class BriefTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="leadhand briefs "))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        hermetic(self)
        self.ws = make_repo(self.tmp, name="work space")
        self.card = make_card()
        self.deck = make_deck(self.ws / "docs" / "my deck.md", [self.card])

    def brief(self, kind, card="default", **extra):
        card = self.card if card == "default" else card
        merged = {"base": "b" * 40, "branch": "leadhand/widget--card-3", **EXTRA.get(kind, {}), **extra}
        return write_brief(kind, ws=self.ws, deck=self.deck, card=card, attempt=1,
                           dispatch_id=f"{kind}-1", extra=merged)

    def test_implement_brief_contains_contract_fields_verbatim(self):
        brief, report = self.brief("implement", test_first="always")
        text = brief.read_text(encoding="utf-8")
        c = self.card
        for value in (c.title, c.hard_reason, c.risk, c.source, c.out_of_scope, c.done_when,
                      c.stop_if, c.inferred[0], c.context, "R1, R4", "1, 2",
                      "src/widget/**, tests/test_widget.py", "codex gpt-6.1-sol xhigh"):
            self.assertIn(value, text)
        for value in (str(self.deck.path), self.deck.source, "b" * 40,
                      "leadhand/widget--card-3", str(report), "always"):
            self.assertIn(value, text)
        self.assertNotIn("SENTINEL", text)
        self.assertNotIn("4242", text)

    def test_test_first_defaults_to_when_cheap(self):
        brief, _ = self.brief("implement")
        self.assertIn("when-cheap", brief.read_text(encoding="utf-8"))

    def test_every_brief_kind_has_a_template(self):
        self.assertEqual(len(BRIEF_KINDS), 12)
        for kind in BRIEF_KINDS:
            self.assertTrue((TEMPLATES / f"brief-{kind}.md").is_file(), kind)
            card = None if kind in ("final_review", "final_fix", "security", "evidence", "verify") else self.card
            brief, _ = self.brief(kind, card=card)
            text = brief.read_text(encoding="utf-8")
            self.assertNotIn("$", text, kind)
            self.assertIn(CONTRACT_POINTER, text, kind)
            for value in EXTRA.get(kind, {}).values():
                if kind in ("final_review", "evidence") and value == "3":
                    continue
                self.assertIn(value, text, kind)

    def test_kind_specific_content(self):
        text = lambda kind, card="default": self.brief(kind, card=card)[0].read_text(encoding="utf-8")
        self.assertIn("continue from the branch as it stands and write your report", text("continue"))
        self.assertIn("security-review skill", text("security", card=None))
        self.assertIn(self.card.done_when, text("evidence", card=None))
        self.assertIn(self.card.title, text("final_review", card=None))
        verify = text("verify", card=None)
        self.assertIn("make build", verify)
        self.assertIn("make test", verify)

    def test_card_briefs_require_card_base_and_branch(self):
        for kind in ("implement", "fix", "review", "integrate"):
            for key in ("base", "branch"):
                extra = {"base": "b" * 40, "branch": "x--card-3", **EXTRA.get(kind, {})}
                del extra[key]
                with self.assertRaisesRegex(ValueError, key):
                    write_brief(kind, ws=self.ws, deck=self.deck, card=self.card, attempt=1,
                                dispatch_id=f"{kind}-1", extra=extra)
        text = self.brief("implement")[0].read_text(encoding="utf-8")
        self.assertIn("- Card branch: leadhand/widget--card-3", text)

    def test_review_needs_implementer_report_unless_test_first_off(self):
        for kind in ("review", "rereview"):
            extra = {k: v for k, v in EXTRA[kind].items() if k != "implementer_report"}
            with self.assertRaisesRegex(ValueError, "implementer_report"):
                self.brief(kind, **{**extra, "implementer_report": ""})
            brief, _ = self.brief(kind, implementer_report="", test_first="off")
            self.assertIn("(none; test_first is off)", brief.read_text(encoding="utf-8"))
        text = self.brief("review")[0].read_text(encoding="utf-8")
        self.assertIn("/x/impl report.md", text)
        self.assertIn("## Tests", text)

    def test_integrate_permits_rebase_onto_run_tip(self):
        text = self.brief("integrate")[0].read_text(encoding="utf-8")
        self.assertIn("c" * 40, text)
        self.assertIn("rebase the card branch", text)
        self.assertIn("rebase the card branch onto a named commit", " ".join(CONTRACT.split()))

    def test_final_briefs_list_scope(self):
        other = make_card(number=5, title="Second", allowed_paths=("lib/b.py", "src/widget/**"),
                          out_of_scope="Docs.", depends_on=(), route_pin=None)
        self.deck = make_deck(self.deck.path, [self.card, other])
        review = self.brief("final_review", card=None, cards="3, 5")[0].read_text(encoding="utf-8")
        for value in ("Second", "lib/b.py", "Docs.", self.card.out_of_scope):
            self.assertIn(value, review)
        fix = self.brief("final_fix", card=None)[0].read_text(encoding="utf-8")
        self.assertIn("- Commit on: leadhand/widget", fix)
        self.assertIn("- Allowed paths: src/widget/**, tests/test_widget.py, lib/b.py", fix)
        only = self.brief("final_fix", card=None, cards="5")[0].read_text(encoding="utf-8")
        self.assertIn("- Allowed paths: lib/b.py, src/widget/**\n", only)
        with self.assertRaisesRegex(ValueError, "branch"):
            write_brief("final_fix", ws=self.ws, deck=self.deck, card=None, attempt=1,
                        dispatch_id="ff", extra=EXTRA["final_fix"] | {"branch": ""})

    def test_evidence_requires_cards(self):
        with self.assertRaisesRegex(ValueError, "cards"):
            write_brief("evidence", ws=self.ws, deck=self.deck, card=None, attempt=1,
                        dispatch_id="ev", extra={"base_worktree": "/x"})

    def test_answer_quotes_every_ruling_line(self):
        text = self.brief("answer", ruling="Use B.\nKeep the old order.")[0].read_text(encoding="utf-8")
        self.assertIn("> Use B.\n> Keep the old order.\n", text)

    def test_verify_says_skip_for_empty_commands(self):
        self.deck = replace(self.deck, build="")
        text = self.brief("verify", card=None)[0].read_text(encoding="utf-8")
        self.assertIn("- Build: (none given; skip it)", text)
        self.assertIn("- Test: make test", text)

    def test_missing_extra_and_unknown_kind_raise(self):
        with self.assertRaises(ValueError):
            write_brief("fix", ws=self.ws, deck=self.deck, card=self.card, attempt=1,
                        dispatch_id="fix-1", extra={})
        with self.assertRaises(ValueError):
            self.brief("polish")

    def test_brief_and_report_paths_are_inside_workspace_scratch(self):
        self.assertEqual(scratch_dir(self.ws, 3, "d-7"), self.ws / ".leadhand" / "3" / "d-7")
        self.assertEqual(scratch_dir(self.ws, None, "d-8"), self.ws / ".leadhand" / "run" / "d-8")
        brief, report = self.brief("implement")
        self.assertEqual(brief, self.ws / ".leadhand" / "3" / "implement-1" / "brief.md")
        self.assertEqual(report, self.ws / ".leadhand" / "3" / "implement-1" / "report.md")
        self.assertTrue(brief.is_file())
        brief2, _ = self.brief("security", card=None)
        self.assertEqual(brief2.parent, self.ws / ".leadhand" / "run" / "security-1")

    def test_scratch_dir_is_ignored_by_git(self):
        self.brief("implement")
        self.brief("verify", card=None)
        self.assertEqual(run_git(self.ws, "status", "--porcelain", "--untracked-files=all"), "")

    def test_agent_for_maps_every_role_and_vendor(self):
        want = {
            "implementer": "leadhand:implementer", "implementer_hard": "leadhand:implementer",
            "reviewer": "leadhand:reviewer", "reviewer_hard": "leadhand:reviewer",
            "final_reviewer": "leadhand:reviewer", "verifier": "leadhand:verifier",
            "scout": "leadhand:scout",
        }
        self.assertEqual(dict(ROLE_AGENT), want)
        for role, agent in want.items():
            self.assertEqual(agent_for(role, "claude"), agent)
            self.assertEqual(agent_for(role, "codex"), "leadhand:codex")
        with self.assertRaises(ValueError):
            agent_for("dealer", "claude")
        with self.assertRaises(ValueError):
            agent_for("implementer", "gemini")

    def test_claude_dispatch_message_names_paths(self):
        brief, report = self.brief("implement")
        msg = dispatch_message("leadhand:implementer", brief, report)
        self.assertIn(str(brief), msg)
        self.assertIn(str(report), msg)
        self.assertLessEqual(len(msg.splitlines()), 15)

    def test_codex_dispatch_message_is_one_quoted_command(self):
        brief, report = self.brief("implement")
        argv = ["/opt/lead hand/scripts/leadhand", "dispatch", "--brief", str(brief),
                "--report", str(report), "--note", "it's $HOME; rm -rf /"]
        msg = dispatch_message("leadhand:codex", brief, report, codex_command=argv)
        self.assertEqual(len(msg.splitlines()), 1)
        self.assertEqual(shlex.split(msg), argv)
        self.assertIn(" ", str(brief))
        with self.assertRaises(ValueError):
            dispatch_message("leadhand:codex", brief, report)
        with self.assertRaises(ValueError):
            dispatch_message("leadhand:codex", brief, report, codex_command=["run", "a\nb"])

    def test_worker_prompt_for_codex_workers(self):
        brief, report = self.brief("implement")
        prompt = worker_prompt("implementer_hard", brief, report)
        self.assertIn(CONTRACT, prompt)
        self.assertIn(str(brief), prompt)
        self.assertIn(str(report), prompt)
        self.assertNotIn("---\nname:", prompt)
        review = worker_prompt("reviewer", brief, report)
        self.assertIn("Verdict: clean", review)
        self.assertNotIn(CONTRACT, review)

    def test_codex_implementer_prompt_forbids_commits_and_claude_agents_do_not(self):
        no_commit = ("Do not run git commit, git add, or any command that writes under .git; leave your "
                     "changes in the working tree and list the changed files in your report. leadhand "
                     "commits them on your branch after you finish.")
        brief, report = self.brief("implement")
        for role in ("implementer", "implementer_hard"):
            self.assertIn(no_commit, " ".join(worker_prompt(role, brief, report).split()), role)
        self.assertNotIn("leadhand commits them", worker_prompt("reviewer", brief, report))
        root = Path(__file__).resolve().parents[1]
        for agent in sorted((root / "agents").glob("*.md")):
            self.assertNotIn("leadhand commits them", agent.read_text(encoding="utf-8"), agent.name)

    def test_parse_report_head_variants(self):
        cases = {
            "Status: READY\n## Summary\n": ("Status", "READY", ""),
            "\n\n**Status:** QUESTION\n## Question\n": ("Status", "QUESTION", ""),
            "Status: BLOCKED\nBlocked by: usage limit\n": ("Status", "BLOCKED", "usage limit"),
            "**Status: BLOCKED**\n**Blocked by:** environment\n": ("Status", "BLOCKED", "environment"),
            "Status: spent\n": ("Status", "SPENT", ""),
            "Status: BLOCKED\nBlocked by: Environment — no network\n": ("Status", "BLOCKED", "environment"),
            "Status: BLOCKED\nBlocked by: usage limits hit\n": ("Status", "BLOCKED", "usage limit"),
            "Status: BLOCKED\nBlocked by: a hard bug\n": ("Status", "BLOCKED", ""),
            "Verdict: defects\n## Findings\n1. [high] a.py:3 — x — y\n": ("Verdict", "defects", ""),
            "Verdict: Clean\n": ("Verdict", "clean", ""),
            "Result: pass\n## Checks\n": ("Result", "pass", ""),
            "": ("Status", "BLOCKED", ""),
            "I finished the work, all good.\n": ("Status", "BLOCKED", ""),
            "Status: DONE\n": ("Status", "BLOCKED", ""),
            "Verdict: maybe\n": ("Status", "BLOCKED", ""),
        }
        for text, want in cases.items():
            self.assertEqual(parse_report_head(text), want, repr(text))


if __name__ == "__main__":
    unittest.main()
