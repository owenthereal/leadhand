"""Worker briefs, dispatch messages and report heads.

A brief is a Markdown file written into the attempt's scratch folder inside the
workspace (`<ws>/.leadhand/<card or "run">/<dispatch_id>/brief.md`); the worker
writes its report beside it (`report.md`). The worker contract and the role
rules live in the agent files under `agents/`; Codex workers get the same text
through `worker_prompt`.
"""
from __future__ import annotations

import os
import re
import shlex
import tempfile
from collections.abc import Mapping
from pathlib import Path
from string import Template
from types import MappingProxyType

from leadhand.config import VENDORS
from leadhand.deck import Card, Deck
from leadhand.gitops import ensure_excluded

BRIEF_KINDS = ("implement", "fix", "answer", "continue", "integrate", "review", "rereview",
               "final_review", "final_fix", "security", "evidence", "verify")
ROLE_AGENT: Mapping[str, str] = MappingProxyType({
    "implementer": "leadhand:implementer",
    "implementer_hard": "leadhand:implementer",
    "reviewer": "leadhand:reviewer",
    "reviewer_hard": "leadhand:reviewer",
    "final_reviewer": "leadhand:reviewer",
    "verifier": "leadhand:verifier",
    "scout": "leadhand:scout",
})
CODEX_AGENT = "leadhand:codex"
TEMPLATES = Path(__file__).resolve().parent / "templates"
AGENTS_DIR = Path(__file__).resolve().parents[3] / "agents"
SCRATCH = ".leadhand"
TEST_FIRST = ("when-cheap", "always", "off")

# Kinds that are about one card and cannot be written without it.
CARD_KINDS = frozenset({"implement", "fix", "answer", "continue", "integrate", "review", "rereview"})
# Template variables write_brief fills itself; every other one must come from `extra`.
_COMPUTED = frozenset({"context", "card_list", "build", "test"})

STATUS_VALUES = ("READY", "QUESTION", "BLOCKED", "SPENT")
HEAD_VALUES = {"Status": STATUS_VALUES, "Verdict": ("clean", "defects"), "Result": ("pass", "fail")}
_HEAD = re.compile(r"^(status|verdict|result)\s*:\s*([A-Za-z]+)\s*$", re.IGNORECASE)
_BLOCKED_BY = re.compile(r"^blocked by\s*:\s*(.+?)\s*$", re.IGNORECASE)
_MARKUP = re.compile(r"[*_`]")
_UNPARSED = ("Status", "BLOCKED", "")
_BLOCKED_BY_LINES = 5


def agent_for(role: str, vendor: str) -> str:
    if vendor not in VENDORS:
        raise ValueError(f"unknown vendor {vendor!r}")
    if role not in ROLE_AGENT:
        raise ValueError(f"no worker agent for role {role!r}")
    return CODEX_AGENT if vendor == "codex" else ROLE_AGENT[role]


def scratch_dir(ws: Path, card: int | None, dispatch_id: str) -> Path:
    if not dispatch_id or "/" in dispatch_id or dispatch_id in (".", ".."):
        raise ValueError(f"bad dispatch id {dispatch_id!r}")
    return Path(ws) / SCRATCH / ("run" if card is None else str(card)) / dispatch_id


# -- rendering -------------------------------------------------------------


def _render_card(card: Card) -> str:
    lines = [f"## Card {card.number}: {card.title}"]

    def field(key: str, value: str) -> None:
        if value:
            lines.append(f"- {key}: {value}")

    field("Covers", ", ".join(card.covers))
    field("Role", card.role)
    field("Hard", (f"yes — {card.hard_reason}" if card.hard_reason else "yes") if card.hard else "no")
    field("Risk", card.risk)
    field("Depends on", ", ".join(map(str, card.depends_on)) or "none")
    field("Source", card.source)
    field("Allowed paths", ", ".join(card.allowed_paths))
    field("Out of scope", card.out_of_scope)
    field("Done when", card.done_when)
    field("Stop if", card.stop_if)
    for item in card.inferred:
        field("Inferred", item)
    if card.route_pin is not None:
        r = card.route_pin
        field("Route", f"{r.vendor} {r.model} {r.effort}")
    if card.context.strip():
        lines += ["", card.context.strip("\n")]
    return "\n".join(lines)


def _selected_cards(deck: Deck, spec: str | None) -> list[Card]:
    if not spec:
        return list(deck.cards)
    by_number = {c.number: c for c in deck.cards}
    out = []
    for part in (p.strip() for p in spec.split(",")):
        if not part:
            continue
        if not part.isdigit() or int(part) not in by_number:
            raise ValueError(f"cards: no card {part!r} in the deck")
        out.append(by_number[int(part)])
    return out


def _card_list(cards: list[Card]) -> str:
    return "\n".join(f"- Card {c.number}: {c.title}\n  Done when: {c.done_when}" for c in cards) or "- (none)"


def _context(deck: Deck, card: Card | None, *, attempt: int, base: str, branch: str,
             brief: Path, report: Path, test_first: str) -> str:
    pointers = [f"- Deck: {deck.path}"]
    if deck.source:
        pointers.append(f"- Plan: {deck.source}")
    if card is not None:
        if card.source:
            pointers.append(f"- Card source: {card.source}")
        if card.allowed_paths:
            pointers.append(f"- Files: {', '.join(card.allowed_paths)}")
    rules = {
        "always": "always — write a failing test and see it fail before the code that makes it pass.",
        "when-cheap": "when-cheap — write a failing test first whenever one is cheap to write.",
        "off": "off — test as you see fit.",
    }
    parts = [
        "## Where things are",
        "",
        *pointers,
        f"- Base commit: {base}",
        f"- Branch: {branch}",
        f"- Attempt: {attempt}",
        f"- This brief: {brief}",
        f"- Report file: {report}",
        f"- test_first: {rules[test_first]}",
    ]
    if card is not None:
        parts += ["", "## The card, verbatim", "", _render_card(card)]
    return "\n".join(parts)


def _atomic_write(path: Path, text: str) -> None:
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def write_brief(kind: str, *, ws: Path, deck: Deck, card: Card | None, attempt: int, dispatch_id: str,
                extra: Mapping[str, str]) -> tuple[Path, Path]:
    """Write the brief for `kind` and return (brief, report) paths.

    `extra` supplies the kind's template variables (for example `findings` and
    `round` for `fix`) plus optional `base` (default the deck's base commit),
    `branch` (default the deck's run branch), `test_first` (default
    `when-cheap`) and `cards` (comma-separated card numbers for final_review and
    evidence; default every card in the deck)."""
    if kind not in BRIEF_KINDS:
        raise ValueError(f"unknown brief kind {kind!r}")
    if kind in CARD_KINDS and card is None:
        raise ValueError(f"a {kind} brief needs a card")
    test_first = extra.get("test_first") or "when-cheap"
    if test_first not in TEST_FIRST:
        raise ValueError(f"test_first must be one of {', '.join(TEST_FIRST)}, got {test_first!r}")
    template = Template((TEMPLATES / f"brief-{kind}.md").read_text(encoding="utf-8"))
    needed = set(template.get_identifiers()) - _COMPUTED
    missing = sorted(k for k in needed if not str(extra.get(k, "")).strip())
    if missing:
        raise ValueError(f"{kind} brief needs {', '.join(missing)}")

    ws = Path(ws)
    folder = scratch_dir(ws, None if card is None else card.number, dispatch_id)
    brief, report = folder / "brief.md", folder / "report.md"
    values = {k: str(v) for k, v in extra.items()}
    values.update(
        context=_context(deck, card, attempt=attempt, base=extra.get("base") or deck.base_sha,
                         branch=extra.get("branch") or deck.branch, brief=brief, report=report,
                         test_first=test_first),
        card_list=_card_list(_selected_cards(deck, extra.get("cards"))),
        build=deck.build or "(none given)",
        test=deck.test or "(none given)",
    )
    text = template.substitute(values)
    ensure_excluded(ws, f"{SCRATCH}/")
    folder.mkdir(parents=True, exist_ok=True)
    _atomic_write(brief, text.rstrip("\n") + "\n")
    return brief, report


# -- dispatch --------------------------------------------------------------


def _agent_body(agent: str) -> str:
    text = (AGENTS_DIR / f"{agent.split(':', 1)[-1]}.md").read_text(encoding="utf-8")
    if text.startswith("---\n"):
        _, sep, body = text[4:].partition("\n---\n")
        if sep:
            text = body
    return text.strip()


def worker_prompt(role: str, brief: Path, report: Path) -> str:
    """The instructions a Codex worker gets: the Claude agent's body (contract and
    role rules) followed by the brief and report paths."""
    body = _agent_body(agent_for(role, "claude"))
    return (f"{body}\n\n## Your files\n\n- Brief: {brief}\n- Report file: {report}\n\n"
            "Read the brief now, do the work, write the report, then reply.\n")


def dispatch_message(agent: str, brief: Path, report: Path, *, codex_command: list[str] | None = None) -> str:
    """The message for the dispatched agent. For the Codex proxy it is exactly one
    command line with every argument shell-quoted."""
    if agent == CODEX_AGENT:
        if not codex_command:
            raise ValueError("the Codex proxy needs codex_command")
        return " ".join(shlex.quote(str(a)) for a in codex_command)
    if codex_command is not None:
        raise ValueError(f"codex_command is only for {CODEX_AGENT}")
    if agent not in ROLE_AGENT.values():
        raise ValueError(f"unknown worker agent {agent!r}")
    return (f"Brief: {brief}\n"
            f"Report file: {report}\n"
            "Read the brief first and do the work it describes. Write your full report to the "
            "report file, then reply in at most 15 lines, ending with `Report: <path>`.")


# -- reports ---------------------------------------------------------------


def _clean(line: str) -> str:
    return _MARKUP.sub("", line).strip().lstrip("#>").strip()


def parse_report_head(text: str) -> tuple[str, str, str]:
    """(key, value, blocked_by) from a report's or reply's first line.

    Tolerates Markdown emphasis, leading blank lines and the value's case.
    Anything unparseable is ("Status", "BLOCKED", "")."""
    lines = [_clean(ln) for ln in (text or "").splitlines()]
    lines = [ln for ln in lines if ln]
    if not lines:
        return _UNPARSED
    m = _HEAD.match(lines[0])
    if not m:
        return _UNPARSED
    key = m.group(1).capitalize()
    allowed = HEAD_VALUES[key]
    value = next((v for v in allowed if v.lower() == m.group(2).lower()), None)
    if value is None:
        return _UNPARSED
    blocked_by = ""
    if key == "Status" and value == "BLOCKED":
        for ln in lines[1:_BLOCKED_BY_LINES]:
            b = _BLOCKED_BY.match(ln)
            if b:
                blocked_by = b.group(1).lower()
                break
    return key, value, blocked_by
