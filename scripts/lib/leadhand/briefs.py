"""Worker briefs, dispatch messages and report heads.

A brief is a Markdown file written into the attempt's scratch folder inside the
workspace (`<ws>/.leadhand/<card or "run">/<dispatch_id>/brief.md`); the worker
writes its report beside it (`report.md`). The worker contract and the role
rules live in the agent files under `agents/`; Codex workers get the same text
through `worker_prompt`.
"""
from __future__ import annotations

import errno
import os
import re
import shlex
import stat
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
# Keys each kind needs in `extra`. Card kinds also need `base` and `branch`.
REQUIRED: Mapping[str, tuple[str, ...]] = MappingProxyType({
    "implement": (),
    "fix": ("findings", "round"),
    "answer": ("ruling",),
    "continue": (),
    "integrate": ("run_tip", "conflicts", "card_intent", "run_intent"),
    "review": ("diff_range",),
    "rereview": ("diff_range", "findings"),
    "final_review": ("diff_range",),
    "final_fix": ("gate", "findings", "branch"),
    "security": ("diff_range",),
    "evidence": ("base_worktree", "cards"),
    "verify": (),
})
CARD_REQUIRED = ("base", "branch")
# Reviews need the implementer's report unless test_first is off.
REPORT_KINDS = frozenset({"review", "rereview"})
CONTRACT_POINTER = "Your worker contract and report format are in your instructions."

STATUS_VALUES = ("READY", "QUESTION", "BLOCKED", "SPENT")
HEAD_VALUES = {"Status": STATUS_VALUES, "Verdict": ("clean", "defects"), "Result": ("pass", "fail")}
_HEAD = re.compile(r"^(status|verdict|result)\s*:\s*([A-Za-z]+)\s*$", re.IGNORECASE)
_BLOCKED_BY = re.compile(r"^blocked by\s*:\s*(.+?)\s*$", re.IGNORECASE)
_MARKUP = re.compile(r"[*_`]")
_UNPARSED = ("Status", "BLOCKED", "")
BLOCKED_REASONS = ("environment", "usage limit")
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


# -- scratch files ---------------------------------------------------------
#
# The CLI runs outside Claude Code's sandbox, and a worker can plant a symlink (or a file)
# at a later dispatch's scratch folder. Every read, write or removal under <ws>/.leadhand/ walks
# the folders from the workspace down with O_NOFOLLOW | O_DIRECTORY and acts relative to
# the last folder's descriptor, so it never follows a symlink out of <ws>/.leadhand/ (a
# folder swapped after the check is not followed either). A symlink or a non-folder on the
# way is refused (ScratchError); nothing is written or removed.


class ScratchError(ValueError):
    """A folder under <ws>/.leadhand/ is a symlink or not a folder."""


_DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0)


def _open_scratch(ws: Path, card: int | None, dispatch_id: str, *, create: bool) -> int | None:
    """A descriptor for the dispatch's scratch folder (None when it is missing and not
    `create`), or ScratchError naming the offending path."""
    folder = scratch_dir(ws, card, dispatch_id)
    parts = folder.relative_to(Path(ws)).parts
    fd = os.open(ws, os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_CLOEXEC", 0))
    try:
        for i, name in enumerate(parts):
            shown = Path(ws).joinpath(*parts[:i + 1])
            try:
                nxt = os.open(name, _DIR_FLAGS, dir_fd=fd)
            except FileNotFoundError:
                if not create:
                    return None
                try:
                    os.mkdir(name, dir_fd=fd)
                except FileExistsError:
                    pass
                nxt = _reopen(name, fd, shown)
            except OSError as e:
                raise _refusal(shown, e) from None
            os.close(fd)
            fd = nxt
        out, fd = fd, -1
        return out
    finally:
        if fd >= 0:
            os.close(fd)


def _reopen(name: str, dir_fd: int, shown: Path) -> int:
    try:
        return os.open(name, _DIR_FLAGS, dir_fd=dir_fd)
    except OSError as e:
        raise _refusal(shown, e) from None


def _refusal(shown: Path, e: OSError) -> OSError:
    if e.errno in (errno.ELOOP, errno.ENOTDIR, errno.ENOENT):
        what = "a symlink" if os.path.islink(shown) else "not a folder"
        return ScratchError(f"refused: the scratch path {shown} is {what}; remove it and run the "
                            "command again (a worker may have planted it)")
    return e


def write_scratch_file(ws: Path, card: int | None, dispatch_id: str, name: str, text: str) -> Path:
    """Atomically write `name` in the dispatch's scratch folder (created as needed), never
    through a symlink. Returns the file's path."""
    fd = _open_scratch(ws, card, dispatch_id, create=True)
    tmp = f".{name}.{os.getpid()}.{os.urandom(4).hex()}"
    try:
        f = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644, dir_fd=fd)
        try:
            with os.fdopen(f, "w", encoding="utf-8") as out:
                out.write(text)
                out.flush()
                os.fsync(out.fileno())
            os.replace(tmp, name, src_dir_fd=fd, dst_dir_fd=fd)
        except BaseException:
            try:
                os.unlink(tmp, dir_fd=fd)
            except OSError:
                pass
            raise
    finally:
        os.close(fd)
    return scratch_dir(ws, card, dispatch_id) / name


def read_scratch_file(ws: Path, card: int | None, dispatch_id: str, name: str) -> bytes | None:
    """The bytes of the regular file `name` in the dispatch's scratch folder, or None when it
    or the folder is missing. Never through a symlink: a symlinked folder on the way raises
    ScratchError; a symlink or anything but a regular file named `name` raises OSError."""
    fd = _open_scratch(ws, card, dispatch_id, create=False)
    if fd is None:
        return None
    try:
        try:
            f = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | getattr(os, "O_CLOEXEC", 0),
                        dir_fd=fd)
        except FileNotFoundError:
            return None
        with os.fdopen(f, "rb") as src:
            if not stat.S_ISREG(os.fstat(src.fileno()).st_mode):
                raise OSError(0, "not a regular file")
            return src.read()
    finally:
        os.close(fd)


def remove_scratch_file(ws: Path, card: int | None, dispatch_id: str, name: str) -> None:
    """Remove `name` from the dispatch's scratch folder if it is there (a symlink named `name`
    is removed itself, never its target); a symlinked folder on the way is refused."""
    fd = _open_scratch(ws, card, dispatch_id, create=False)
    if fd is None:
        return
    try:
        os.unlink(name, dir_fd=fd)
    except FileNotFoundError:
        pass
    finally:
        os.close(fd)


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
    out = []
    for c in cards:
        out.append(f"- Card {c.number}: {c.title}")
        out.append(f"  Allowed paths: {', '.join(c.allowed_paths) or '(none)'}")
        if c.out_of_scope:
            out.append(f"  Out of scope: {c.out_of_scope}")
        out.append(f"  Done when: {c.done_when}")
    return "\n".join(out) or "- (none)"


def _union_paths(cards: list[Card]) -> str:
    seen: dict[str, None] = {}
    for c in cards:
        seen.update(dict.fromkeys(c.allowed_paths))
    return ", ".join(seen) or "(none)"


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
        CONTRACT_POINTER,
        "",
        *pointers,
        f"- Base commit: {base}",
        f"- {'Card branch' if card is not None else 'Branch'}: {branch}",
        f"- Attempt: {attempt}",
        f"- This brief: {brief}",
        f"- Report file: {report}",
        f"- test_first: {rules[test_first]}",
    ]
    if card is not None:
        parts += ["", "## The card, verbatim", "", _render_card(card)]
    return "\n".join(parts)


def write_brief(kind: str, *, ws: Path, deck: Deck, card: Card | None, attempt: int, dispatch_id: str,
                extra: Mapping[str, str], notes: str = "") -> tuple[Path, Path]:
    """Write the brief for `kind` (with `notes` appended) and return (brief, report) paths.
    A symlink or non-folder on the scratch folder's way raises ScratchError.

    `extra` supplies the keys in REQUIRED[kind]; card kinds also need `base` and
    `branch` (the card's base commit and card branch), and review/rereview need
    `implementer_report` unless `test_first` is `off`. Optional: `test_first`
    (default `when-cheap`), `cards` (comma-separated accepted card numbers;
    required for evidence, default every deck card elsewhere), `allowed_paths`
    for final_fix (default the union of those cards' allowed paths), and, for
    run-level kinds, `base`/`branch` (default the deck's base commit and run
    branch)."""
    if kind not in BRIEF_KINDS:
        raise ValueError(f"unknown brief kind {kind!r}")
    if kind in CARD_KINDS and card is None:
        raise ValueError(f"a {kind} brief needs a card")
    test_first = extra.get("test_first") or "when-cheap"
    if test_first not in TEST_FIRST:
        raise ValueError(f"test_first must be one of {', '.join(TEST_FIRST)}, got {test_first!r}")
    needed = list(REQUIRED[kind])
    if kind in CARD_KINDS:
        needed += CARD_REQUIRED
    if kind in REPORT_KINDS and test_first != "off":
        needed.append("implementer_report")
    missing = sorted({k for k in needed if not str(extra.get(k, "")).strip()})
    if missing:
        raise ValueError(f"{kind} brief needs {', '.join(missing)}")
    template = Template((TEMPLATES / f"brief-{kind}.md").read_text(encoding="utf-8"))

    ws = Path(ws)
    folder = scratch_dir(ws, None if card is None else card.number, dispatch_id)
    brief, report = folder / "brief.md", folder / "report.md"
    cards = _selected_cards(deck, extra.get("cards"))
    values = {k: str(v) for k, v in extra.items()}
    values.update(
        context=_context(deck, card, attempt=attempt, base=extra.get("base") or deck.base_sha,
                         branch=extra.get("branch") or deck.branch, brief=brief, report=report,
                         test_first=test_first),
        card_list=_card_list(cards),
        allowed_paths=extra.get("allowed_paths") or _union_paths(cards),
        build=deck.build or "(none given; skip it)",
        test=deck.test or "(none given; skip it)",
        ruling_quoted="\n".join(f"> {ln}".rstrip() for ln in str(extra.get("ruling", "")).splitlines()),
        implementer_report_line=extra.get("implementer_report") or "(none; test_first is off)",
    )
    try:
        text = template.substitute(values)
    except (KeyError, ValueError) as e:
        raise ValueError(f"brief-{kind}.md: cannot fill {e}") from None
    ensure_excluded(ws, f"{SCRATCH}/")
    text = text.rstrip("\n") + "\n"
    if notes:
        text += "\n" + notes.rstrip("\n") + "\n"
    write_scratch_file(ws, None if card is None else card.number, dispatch_id, brief.name, text)
    return brief, report


# -- dispatch --------------------------------------------------------------


def agent_max_turns(agent: str) -> int | None:
    """The `maxTurns` in a Claude worker agent's frontmatter (None when it sets none): a
    Claude worker's turn cap, which no dispatch can change (ruling R52)."""
    text = (AGENTS_DIR / f"{agent.split(':', 1)[-1]}.md").read_text(encoding="utf-8")
    if not text.startswith("---\n"):
        return None
    head, sep, _ = text[4:].partition("\n---\n")
    for line in head.splitlines() if sep else ():
        key, _, value = line.partition(":")
        if key.strip() == "maxTurns" and value.strip().isdigit():
            return int(value.strip())
    return None


def _agent_body(agent: str) -> str:
    text = (AGENTS_DIR / f"{agent.split(':', 1)[-1]}.md").read_text(encoding="utf-8")
    if text.startswith("---\n"):
        _, sep, body = text[4:].partition("\n---\n")
        if sep:
            text = body
    return text.strip()


# Codex's workspace-write sandbox keeps the Git directory read-only (ruling R39), so a
# Codex implementer never commits; the dispatcher's runner commits its work afterwards.
CODEX_NO_COMMIT = (
    "## Commits (Codex workers)\n\n"
    "This overrides the contract's rule to commit on the card branch. Do not run git commit,\n"
    "git add, or any command that writes under .git; leave your changes in the working tree\n"
    "and list the changed files in your report. leadhand commits them on your branch after\n"
    "you finish.")


def worker_prompt(role: str, brief: Path, report: Path) -> str:
    """The instructions a Codex worker gets: the Claude agent's body (contract and
    role rules), for implementers the no-commit rule, then the brief and report paths."""
    agent = agent_for(role, "claude")
    body = _agent_body(agent)
    if agent == "leadhand:implementer":
        body += "\n\n" + CODEX_NO_COMMIT
    return (f"{body}\n\n## Your files\n\n- Brief: {brief}\n- Report file: {report}\n\n"
            "Read the brief now, do the work, write the report, then reply.\n")


def dispatch_message(agent: str, brief: Path, report: Path, *, codex_command: list[str] | None = None) -> str:
    """The message for the dispatched agent. For the Codex proxy it is exactly one
    command line with every argument shell-quoted."""
    if agent == CODEX_AGENT:
        if not codex_command:
            raise ValueError("the Codex proxy needs codex_command")
        args = [str(a) for a in codex_command]
        if any("\n" in a or "\r" in a for a in args):
            raise ValueError("codex_command arguments must not contain newlines")
        return " ".join(shlex.quote(a) for a in args)
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
                reason = b.group(1).lower()
                blocked_by = next((r for r in BLOCKED_REASONS if reason.startswith(r)), "")
                break
    return key, value, blocked_by
