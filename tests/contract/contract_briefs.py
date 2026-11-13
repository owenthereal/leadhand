#!/usr/bin/env python3
"""Worker briefs: the brief and report files `next` writes for every dispatch, under
<workspace>/.leadhand/<card or run>/<dispatch id>/, and the lines they render.

Converted from tests/test_briefs.py. A brief's text lines are contract output (ruling S1-R10),
so where the Python test filled a template by hand, this runs a deck to the dispatch and reads
the brief that `next` wrote, asserting the rendered lines. Template-only checks with no outside
effect (a missing value or an unknown kind raising, the report-head parser on its own) are
catalogued as Rust unit tests. The card section is read with contract_deck.card_section.
"""
import shlex
import unittest
from pathlib import Path

import harness
from contract_cards import recorded, relaunch_env
from contract_deck import card_section
from contract_dispatch import World as DispatchWorld, implementer_prompt
from gitfixture import run_git
from harness import ROOT
from supervisor import FakeSupervisor, card

CONTRACT_POINTER = "Your worker contract and report format are in your instructions."
RUN = "leadhand/demo"
CARD1 = "leadhand/demo--card-1"
WHEN_CHEAP = "- test_first: when-cheap — write a failing test first whenever one is cheap to write."
ALWAYS = "- test_first: always — write a failing test and see it fail before the code that makes it pass."
RULING = "Use B.\nKeep the old order."

RICH_CARD = """## Card 3: Parse the widget header
- Covers: R1, R4
- Role: implementer
- Hard: yes — touches the on-disk format
- Risk: high
- Depends on: 1, 2
- Source: docs/plan.md#task-3
- Allowed paths: src/widget/**, tests/test_widget.py
- Out of scope: Rendering; the CLI flags.
- Done when: `test_header_roundtrip` passes and `make check` is clean.
- Stop if: the header needs a new version byte.
- Inferred: "Done when" test name (plan named no test)
- Route: codex gpt-6.1-sol xhigh

Context: The header is 16 bytes, little-endian.
Keep the parser pure.
"""


def agent_body(name: str) -> str:
    """A worker agent file's body (agents/<name>.md without its front matter), as a Codex
    worker's prompt carries it."""
    text = (ROOT / "agents" / f"{name}.md").read_text(encoding="utf-8")
    if text.startswith("---\n"):
        _, sep, body = text[4:].partition("\n---\n")
        if sep:
            text = body
    return text.strip()


class BriefWorld(FakeSupervisor):
    """A FakeSupervisor that keeps every dispatch's brief as it was when the worker started,
    and the workspace's `git status` at that moment."""

    def __init__(self, *args, **kw):
        super().__init__(*args, **kw)
        self.briefs: dict[str, tuple[dict, str]] = {}
        self.status_at_launch: dict[str, str] = {}

    def launch(self, a: dict) -> str:
        self.briefs[a["dispatch_id"]] = (a, Path(a["brief"]).read_text(encoding="utf-8"))
        self.status_at_launch[a["dispatch_id"]] = run_git(self.workspace(), "status", "--porcelain",
                                                          "--untracked-files=all")
        return super().launch(a)

    def brief(self, dispatch_id: str) -> tuple[dict, list[str]]:
        a, text = self.briefs[dispatch_id]
        return a, text.splitlines()


def gates_world(test) -> BriefWorld:
    """A finished run with the end-of-run gates, reaching every brief kind but integrate and
    continue: card 1 (medium risk) asks a question and gets a two-line ruling, then a review
    with defects, a fix and a re-review; card 2 is Hard; the security gate finds defects, so a
    final fix wave runs. The deck's Build and Test are `make build` and `make test`."""
    one = card(1, risk="medium", title="Parse the widget header").replace(
        "- Done when", "- Out of scope: Rendering.\n- Done when")
    two = card(2, title="Second", hard=True, allowed="lib/b.py, src/").replace(
        "- Done when", "- Out of scope: Docs.\n- Done when")
    w = BriefWorld(test, [one, two], gates=True)
    w.deck_path.write_text(w.deck_path.read_text(encoding="utf-8").replace(
        "Build: true\nTest: true\n", "Build: make build\nTest: make test\n"), encoding="utf-8")
    w.start()
    w.script(1, "implement", {"status": "QUESTION"})
    w.rulings[1] = [RULING]
    w.script(1, "review", {"verdict": "defects"})
    w.script(1, "fix", {"files": {"src/widget.py": "fixed\n"}})
    w.script(None, "security", {"verdict": "defects"})
    done = w.run()[-1]
    test.assertEqual(done["outcome"], "complete", done)
    return w


def kinds(w: BriefWorld) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    for did, (a, _) in w.briefs.items():
        out.setdefault(a["kind"], []).append(did)
    return out


class Briefs(unittest.TestCase):
    def assert_filled(self, a: dict, lines: list[str]) -> None:
        """Every placeholder filled, the contract pointer present, the brief and report named."""
        text = "\n".join(lines)
        self.assertNotIn("$", text, a["dispatch_id"])
        self.assertIn(CONTRACT_POINTER, lines, a["dispatch_id"])
        self.assertIn(f"- This brief: {a['brief']}", lines, a["dispatch_id"])
        self.assertIn(f"- Report file: {a['report']}", lines, a["dispatch_id"])

    # -- the card, verbatim -----------------------------------------------------

    def test_implement_brief_contains_contract_fields_verbatim(self):
        cfg = ('[run]\ntest_first = "always"\n[roles.implementer]\nprefer = "claude"\n'
               '[roles.reviewer]\nprefer = "claude"\n')
        w = BriefWorld(self, [card(1), card(2), RICH_CARD], installed=("claude", "codex"), config=cfg)
        w.start()
        for _ in range(20):
            a = w.next()
            if a["action"] == "dispatch" and a["card"] == 3:
                break
            w.act(a)
        self.assertEqual((a["kind"], a["agent"]), ("implement", "leadhand:codex"), a)
        lines = Path(a["brief"]).read_text(encoding="utf-8").splitlines()
        run_tip = w.branch_tip(RUN)
        for line in (f"- Deck: {w.run_dir / 'deck.md'}",
                     "- Plan: docs/plan.md",
                     "- Card source: docs/plan.md#task-3",
                     "- Files: src/widget/**, tests/test_widget.py",
                     f"- Base commit: {run_tip}",
                     "- Card branch: leadhand/demo--card-3",
                     "- Attempt: 1",
                     f"- This brief: {a['brief']}",
                     f"- Report file: {a['report']}",
                     ALWAYS):
            self.assertIn(line, lines)
        self.assertEqual(card_section("\n".join(lines), 3), [
            "## Card 3: Parse the widget header",
            "- Covers: R1, R4",
            "- Role: implementer",
            "- Hard: yes — touches the on-disk format",
            "- Risk: high",
            "- Depends on: 1, 2",
            "- Source: docs/plan.md#task-3",
            "- Allowed paths: src/widget/**, tests/test_widget.py",
            "- Out of scope: Rendering; the CLI flags.",
            "- Done when: `test_header_roundtrip` passes and `make check` is clean.",
            "- Stop if: the header needs a new version byte.",
            '- Inferred: "Done when" test name (plan named no test)',
            "- Route: codex gpt-6.1-sol xhigh",
            "",
            "The header is 16 bytes, little-endian.",
            "Keep the parser pure."])
        self.assert_filled(a, lines)

    def test_test_first_defaults_to_when_cheap(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        self.assertIn(WHEN_CHEAP, Path(a["brief"]).read_text(encoding="utf-8").splitlines())

    # -- every kind ---------------------------------------------------------------

    def test_every_brief_kind_fills_its_values(self):
        w = gates_world(self)
        seen = kinds(w)
        self.assertEqual(sorted(seen), ["answer", "evidence", "final_fix", "final_review", "fix", "implement",
                                        "rereview", "review", "security", "verify"])
        for did in w.briefs:
            a, lines = w.brief(did)
            self.assert_filled(a, lines)
        base = w.base
        reports = w.run_dir / "cards"
        _, fix = w.brief("c1-a1-fix-1")
        self.assertIn(f"- Findings: {reports / '1' / 'c1-a1-review-1' / 'report.md'}", fix)
        self.assertIn("A review of this card found defects. This is fix round 1. Read the", fix)
        _, answer = w.brief("c1-a1-answer-1")
        self.assertIn("> Use B.", answer)
        _, review = w.brief("c1-a1-review-1")
        tip = next(e["tip"] for e in w.events("result_recorded") if e["dispatch_id"] == "c1-a1-answer-1")
        self.assertIn(f"- Diff range: {base}..{tip}", review)
        _, rereview = w.brief("c1-a1-rereview-1")
        self.assertIn(f"- Findings to recheck: {reports / '1' / 'c1-a1-review-1' / 'report.md'}", rereview)
        accepted = {e["card"]: e["sha"] for e in w.events("accepted")}
        _, final_review = w.brief("run-final_review-1")
        self.assertIn(f"- Diff range: {base}..{accepted[2]}", final_review)
        _, security = w.brief("run-security-1")
        self.assertIn(f"- Diff range: {base}..{accepted[2]}", security)
        _, evidence = w.brief("run-evidence-1")
        ws = w.workspace()
        self.assertIn(f"- Base worktree: {ws.parent / (ws.name + '--base')}", evidence)
        _, final_fix = w.brief("run-w1-final_fix-1")
        self.assertIn(f"- Findings: {reports / 'run' / 'run-security-1' / 'report.md'}", final_fix)
        self.assertIn('The run\'s final gate "security" failed. Its findings:', final_fix)

    def test_kind_specific_content(self):
        w = gates_world(self)
        _, security = w.brief("run-security-1")
        self.assertIn("Use the runtime's security-review skill when one is installed. Look for", security)
        _, evidence = w.brief("run-evidence-1")
        text = "\n".join(evidence)
        self.assertIn("  Done when: the card's file exists and the tests pass", evidence)
        self.assertIn("by an assertion", text)          # R46: compile or import errors do not count
        self.assertIn("import", text)
        self.assertNotIn("fail (or do not\nexist) on the base", text)
        _, final_review = w.brief("run-final_review-1")
        self.assertIn("- Card 1: Parse the widget header", final_review)
        _, verify = w.brief("run-verify-1")
        self.assertIn("- Build: make build", verify)
        self.assertIn("- Test: make test", verify)

    def test_continue_brief_carries_on_from_the_branch(self):
        # A worker that committed and died with its supervisor session: the relaunch continues it.
        w = BriefWorld(self, [card(1)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"crash": "after_commit"})
        a = w.next()
        worker = w.launch(a)
        self.assertIsNone(w.perform(a, worker))
        tip = w.branch_tip(CARD1)
        self.assertNotEqual(tip, w.base)
        w.session("supervisor-2")
        w.cli("run", w.run_dir, env=relaunch_env()).ok()
        nxt = w.next()
        self.assertEqual((nxt["action"], nxt["kind"], nxt["dispatch_id"]), ("dispatch", "continue", "c1-a1-continue-1"))
        lines = Path(nxt["brief"]).read_text(encoding="utf-8").splitlines()
        self.assertIn("continue from the branch as it stands and write your report.", lines)
        self.assertIn("## The interrupted step", lines)
        self.assertIn("c1-a1-implement-1", "\n".join(lines))
        self.assertIn("- Card branch: leadhand/demo--card-1", lines)
        self.assert_filled(nxt, lines)
        self.assertEqual(w.branch_tip(CARD1), tip)

    # -- card briefs ----------------------------------------------------------------

    def test_card_briefs_render_card_base_and_branch(self):
        w = gates_world(self)
        cut = {e["card"]: e["base"] for e in w.events("card_cut")}
        for did in ("c1-a1-implement-1", "c1-a1-answer-1", "c1-a1-review-1", "c1-a1-fix-1", "c1-a1-rereview-1"):
            _, lines = w.brief(did)
            self.assertIn(f"- Base commit: {cut[1]}", lines, did)
            self.assertIn("- Card branch: leadhand/demo--card-1", lines, did)
            self.assertIn("## The card, verbatim", lines, did)
        self.assertEqual(cut[1], w.base)

    def test_review_names_the_implementer_report(self):
        w = gates_world(self)
        reports = w.run_dir / "cards" / "1"
        _, review = w.brief("c1-a1-review-1")
        self.assertIn(f"- Implementer's report: {reports / 'c1-a1-answer-1' / 'report.md'}", review)
        self.assertIn("correctness, test evidence (start from the `## Tests` section of the", review)
        _, rereview = w.brief("c1-a1-rereview-1")
        self.assertIn(f"- Implementer's report: {reports / 'c1-a1-fix-1' / 'report.md'}", rereview)

    def test_integrate_permits_rebase_onto_run_tip(self):
        # Card 1 asks a question and is parked; card 2 lands a conflicting change; the unparked
        # card's rebase conflicts, so an integrate attempt is requested.
        w = BriefWorld(self, [card(1), card(2)])
        w.session("supervisor-1")
        w.start()
        w.script(1, "implement", {"status": "QUESTION", "files": {"src/shared.txt": "card one\n"}})
        w.script(2, "implement", {"files": {"src/shared.txt": "card two\n"}})
        w.parks[1] = "product decision: storage format"
        w.run()
        del w.parks[1]
        run_tip = w.branch_tip(RUN)
        w.cli("unpark", w.run_dir, "--card", "1", "--note", "Use JSON; it is simpler.").ok()
        for _ in range(40):
            a = w.next()
            if a["action"] == "dispatch" and a["kind"] == "integrate":
                break
            w.act(a)
        self.assertEqual(a["dispatch_id"], "c1-a2-integrate-1", a)
        lines = Path(a["brief"]).read_text(encoding="utf-8").splitlines()
        self.assertIn(f"- Run tip to rebase onto: {run_tip}", lines)
        self.assertIn("- Conflicting paths: src/shared.txt", lines)
        self.assertIn("- The card's intent: card 1 (Card number 1): the card's file exists and the tests pass",
                      lines)
        self.assertIn("- The run branch's intent: card 2 (Card number 2): the card's file exists and the tests pass",
                      lines)
        self.assertIn("rebase the card branch", " ".join(lines))
        self.assertIn("- Card branch: leadhand/demo--card-1", lines)
        self.assert_filled(a, lines)
        # The worker contract the agents carry permits exactly that rewrite.
        contract = " ".join((ROOT / "agents" / "implementer.md").read_text(encoding="utf-8").split())
        self.assertIn("rebase the card branch onto a named commit", contract)

    def test_answer_quotes_every_ruling_line(self):
        w = gates_world(self)
        a, _ = w.briefs["c1-a1-answer-1"]
        self.assertIn("> Use B.\n> Keep the old order.\n", w.briefs["c1-a1-answer-1"][1])
        (ruling,) = w.events("ruling")
        self.assertEqual(ruling["text"], RULING)
        self.assertEqual(a["resume"], {"worker_id": w.events("worker_started")[0]["worker_id"]})

    # -- run-level briefs -------------------------------------------------------------

    def test_final_briefs_list_scope(self):
        w = gates_world(self)
        _, review = w.brief("run-final_review-1")
        for line in ("- Card 1: Parse the widget header", "  Allowed paths: src/, tests/", "  Out of scope: Rendering.",
                     "- Card 2: Second", "  Allowed paths: lib/b.py, src/", "  Out of scope: Docs."):
            self.assertIn(line, review)
        self.assertIn("- Branch: leadhand/demo", review)
        _, fix = w.brief("run-w1-final_fix-1")
        self.assertIn("- Commit on: leadhand/demo--card-final", fix)
        self.assertIn("- Allowed paths: src/, tests/, lib/b.py", fix)
        self.assertIn("- Branch: leadhand/demo--card-final", fix)

    # -- where briefs live ---------------------------------------------------------------

    def test_brief_and_report_paths_are_inside_workspace_scratch(self):
        w = gates_world(self)
        ws = w.workspace()
        for did, (a, _) in w.briefs.items():
            folder = ws / ".leadhand" / ("run" if a["card"] is None else str(a["card"])) / did
            self.assertEqual(Path(a["brief"]), folder / "brief.md", did)
            self.assertEqual(Path(a["report"]), folder / "report.md", did)
            self.assertEqual(Path(a["reply_file"]), folder / "reply.txt", did)
        self.assertIn("run-verify-1", w.briefs)
        self.assertIn("c1-a1-implement-1", w.briefs)

    def test_scratch_dir_is_ignored_by_git(self):
        w = gates_world(self)
        ws = w.workspace()
        for did in ("c1-a1-implement-1", "c2-a1-implement-1", "run-final_review-1", "run-verify-1"):
            self.assertEqual(w.status_at_launch[did], "", did)
            self.assertTrue(Path(w.briefs[did][0]["brief"]).is_relative_to(ws))
        self.assertEqual(run_git(ws, "check-ignore", ".leadhand/1/c1-a1-implement-1/brief.md"),
                         ".leadhand/1/c1-a1-implement-1/brief.md")

    # -- agents and messages ---------------------------------------------------------------

    def test_agent_for_maps_every_role_and_vendor(self):
        w = gates_world(self)
        seen = {(a["role"], a["agent"]) for a, _ in w.briefs.values()}
        self.assertEqual(seen, {("implementer", "leadhand:implementer"), ("implementer_hard", "leadhand:implementer"),
                                ("reviewer", "leadhand:reviewer"), ("reviewer_hard", "leadhand:reviewer"),
                                ("final_reviewer", "leadhand:reviewer"), ("verifier", "leadhand:verifier")})

    def test_claude_dispatch_message_names_paths(self):
        w = FakeSupervisor(self, [card(1)])
        w.start()
        a = w.next()
        self.assertEqual(a["agent"], "leadhand:implementer")
        lines = a["message"].splitlines()
        self.assertIn(f"Brief: {a['brief']}", lines)
        self.assertIn(f"Report file: {a['report']}", lines)
        self.assertLessEqual(len(lines), 15)

    def test_codex_dispatch_message_is_one_quoted_command(self):
        w = FakeSupervisor(self, [card(1)], installed=("claude", "codex"))
        w.start()
        a = w.next()
        self.assertEqual(a["agent"], "leadhand:codex")
        self.assertIn(" ", a["brief"])
        self.assertEqual(len(a["message"].splitlines()), 1)
        argv = shlex.split(a["message"])
        self.assertTrue(harness.names_bin(argv[0]), argv)              # S1-R11: by file
        self.assertEqual(argv[1], "dispatch")
        self.assertEqual(argv[argv.index("--brief") + 1], a["brief"])
        self.assertEqual(argv[argv.index("--report") + 1], a["report"])
        self.assertEqual(argv[argv.index("--cwd") + 1], str(w.workspace()))
        self.assertEqual(argv[argv.index("--run") + 1], str(w.run_dir))

    def prompts(self) -> dict[str, str]:
        """The prompt Codex got for an implementer, a Hard implementer and a reviewer dispatch,
        from the fake Codex's log; with each dispatch's brief and report."""
        w = DispatchWorld(self)
        w.scenario("ready")
        out = {}
        for did, role in (("d1", "implementer"), ("d2", "implementer_hard"), ("d3", "reviewer")):
            w.dispatch(did, role=role).ok()
            out[role] = (w.records("start")[-1]["prompt"], *w.scratch(did))
        return out

    def test_worker_prompt_for_codex_workers(self):
        prompts = self.prompts()
        prompt, brief, report = prompts["implementer_hard"]
        self.assertEqual(prompt, implementer_prompt(brief, report))
        self.assertIn(agent_body("implementer"), prompt)
        self.assertIn(f"- Brief: {brief}", prompt.splitlines())
        self.assertIn(f"- Report file: {report}", prompt.splitlines())
        self.assertNotIn("---\nname:", prompt)
        review, brief, report = prompts["reviewer"]
        self.assertEqual(review, f"{agent_body('reviewer')}\n\n## Your files\n\n- Brief: {brief}\n"
                                 f"- Report file: {report}\n\n"
                                 "Read the brief now, do the work, write the report, then reply.\n")
        self.assertIn("Verdict: clean", review)
        self.assertNotIn(agent_body("implementer"), review)

    def test_codex_implementer_prompt_forbids_commits_and_claude_agents_do_not(self):
        no_commit = ("Do not run git commit, git add, or any command that writes under .git; leave your "
                     "changes in the working tree and list the changed files in your report. leadhand "
                     "commits them on your branch after you finish.")
        prompts = self.prompts()
        for role in ("implementer", "implementer_hard"):
            self.assertIn(no_commit, " ".join(prompts[role][0].split()), role)
        self.assertNotIn("leadhand commits them", prompts["reviewer"][0])
        for agent in sorted((ROOT / "agents").glob("*.md")):
            self.assertNotIn("leadhand commits them", agent.read_text(encoding="utf-8"), agent.name)

    # -- the report head ---------------------------------------------------------------

    def test_parse_report_head_variants(self):
        # A reply with no report file: the result event carries the head parsed from the reply.
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
            with self.subTest(reply=text):
                w = FakeSupervisor(self, [card(1)])
                w.start()
                a = w.next()
                worker = w.launch(a)
                event = recorded(w, "result", "--dispatch", a["dispatch_id"], "--worker", worker, "--reply", text)
                key = event["head"]
                value = {"Status": event["status"], "Verdict": event["verdict"], "Result": event["result"]}[key]
                self.assertEqual((key, value, event["blocked_by"]), want, event)


if __name__ == "__main__":
    unittest.main()
