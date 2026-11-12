"""Deck parsing and validation. See docs/deck-format.md."""
from __future__ import annotations

import hashlib
import heapq
import re
from dataclasses import dataclass, replace
from pathlib import Path

from leadhand.config import (Config, ConfigError, Route, load_config, parse_route, unsupported_settings,
                             unsupported_warnings)
from leadhand.gitops import GitError, git

HEADER_KEYS = ("Source", "Base", "Branch", "Build", "Test", "Profile", "Finish")
CARD_KEYS = ("Covers", "Role", "Hard", "Risk", "Depends on", "Source", "Allowed paths",
             "Out of scope", "Done when", "Stop if", "Inferred", "Route")
REQUIRED = ("Role", "Hard", "Risk", "Allowed paths", "Done when", "Stop if")
ROLES = ("implementer", "scout")
RISKS = ("low", "medium", "high")
GIT_TIMEOUT = 20


class DeckError(ValueError):
    def __init__(self, message: str, line: int = 0):
        super().__init__(f"line {line}: {message}" if line else message)
        self.line = line


@dataclass(frozen=True)
class Card:
    number: int
    title: str
    role: str
    hard: bool
    hard_reason: str
    risk: str
    depends_on: tuple[int, ...]
    covers: tuple[str, ...]
    source: str
    allowed_paths: tuple[str, ...]
    out_of_scope: str
    done_when: str
    stop_if: str
    inferred: tuple[str, ...]
    route_pin: Route | None
    context: str
    missing: tuple[str, ...] = ()  # required fields absent or empty
    line: int = 0
    unknown_keys: tuple[tuple[int, str], ...] = ()  # (line, key) of context bullets


@dataclass(frozen=True)
class Requirement:
    id: str
    text: str
    cards: tuple[int, ...]


@dataclass(frozen=True)
class Deck:
    path: Path
    title: str
    source: str
    base_ref: str
    base_sha: str
    branch: str
    build: str
    test: str
    profile: str | None
    finish: str | None
    requirements: tuple[Requirement, ...]
    cards: tuple[Card, ...]
    digest: str


@dataclass(frozen=True)
class Problem:
    level: str  # "error" | "warning"
    card: int | None
    message: str


_HEADER_SPLIT = re.compile(r"\s{2,}(?=(?:%s):)" % "|".join(HEADER_KEYS))
_HEADER_KV = re.compile(r"^(%s):\s*(.*)$" % "|".join(HEADER_KEYS))
_FIELD = re.compile(r"^- ([A-Z][A-Za-z ]*?):\s*(.*)$")
_CARD = re.compile(r"^## Card (\d+):\s*(.*?)\s*$")
_REQ = re.compile(r"^- (R\d+)\s+(.*)(?:→|->)\s*cards?\s+(.+?)\s*$")
_HARD = re.compile(r"^yes(?:\s*(?:—|–|-|:)\s*(.*))?$")


def _csv(text: str) -> tuple[str, ...]:
    return tuple(p.strip() for p in text.split(",") if p.strip())


def _ints(text: str, line: int, what: str) -> tuple[int, ...]:
    if text.strip().lower() == "none":
        return ()
    out = []
    for p in _csv(text):
        if not p.isdigit():
            raise DeckError(f"{what}: {p!r} is not a card number", line)
        out.append(int(p))
    return tuple(out)


def _parse_card(number, title, line, fields, context):
    """fields: list of (key, value, line)."""
    seen: dict[str, tuple[str, int]] = {}
    inferred = []
    for key, value, ln in fields:
        if key == "Inferred":
            inferred.append(value)
            continue
        if key in seen:
            raise DeckError(f"duplicate field {key}", ln)
        seen[key] = (value, ln)

    def val(key):
        return seen.get(key, ("", 0))[0]

    def at(key):
        return seen.get(key, ("", line))[1]

    role = val("Role")
    if role and role not in ROLES:
        raise DeckError(f"Role must be one of {', '.join(ROLES)}, got {role!r}", at("Role"))
    risk = val("Risk")
    if risk and risk not in RISKS:
        raise DeckError(f"Risk must be one of {', '.join(RISKS)}, got {risk!r}", at("Risk"))
    hard, reason = False, ""
    hv = val("Hard")
    if hv:
        if hv == "no":
            pass
        else:
            m = _HARD.match(hv)
            if not m:
                raise DeckError(f"Hard must be 'no' or 'yes — <reason>', got {hv!r}", at("Hard"))
            hard, reason = True, (m.group(1) or "").strip()
    pin = None
    if val("Route"):
        vendor, _, rest = val("Route").partition(" ")
        try:
            pin = parse_route(vendor, rest)
        except ConfigError as e:
            raise DeckError(f"Route: {e}", at("Route"))
    missing = tuple(k for k in REQUIRED if not val(k))
    return Card(
        number=number, title=title, role=role, hard=hard, hard_reason=reason, risk=risk,
        depends_on=_ints(val("Depends on"), at("Depends on"), "Depends on"),
        covers=_csv(val("Covers")), source=val("Source"),
        allowed_paths=_csv(val("Allowed paths")), out_of_scope=val("Out of scope"),
        done_when=val("Done when"), stop_if=val("Stop if"), inferred=tuple(inferred),
        route_pin=pin, context=context, missing=missing, line=line)


def parse_deck(path: Path) -> Deck:
    path = Path(path)
    raw = path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    lines = raw.decode("utf-8").splitlines()
    n = len(lines)
    i = 0
    while i < n and not lines[i].strip():
        i += 1
    if i >= n or not lines[i].startswith("# Deck:"):
        raise DeckError("expected '# Deck: <title>' first", i + 1 if i < n else 1)
    title = lines[i][len("# Deck:"):].strip()
    i += 1

    header: dict[str, str] = {}
    while i < n and not lines[i].startswith("## "):
        text = lines[i].strip()
        i += 1
        if not text:
            continue
        for part in _HEADER_SPLIT.split(text):
            m = _HEADER_KV.match(part.strip())
            if not m:
                raise DeckError(f"unrecognized header line {text!r}", i)
            if m.group(1) in header:
                raise DeckError(f"duplicate header {m.group(1)}", i)
            header[m.group(1)] = m.group(2).strip()
    for key in ("Source", "Base", "Branch"):
        if not header.get(key):
            raise DeckError(f"missing header {key}", 1)
    base_ref, sep, base_sha = header["Base"].partition(" @ ")
    if not sep or not base_ref.strip() or not base_sha.strip():
        raise DeckError("Base must be '<ref> @ <sha>'", 1)

    requirements: list[Requirement] = []
    cards: list[Card] = []
    while i < n:
        line = lines[i]
        ln = i + 1
        if line.strip() == "## Requirements":
            i += 1
            while i < n and not lines[i].startswith("## "):
                text = lines[i].strip()
                i += 1
                if not text:
                    continue
                m = _REQ.match(text)
                if not m:
                    raise DeckError("requirement must be '- R<n> <text> → cards <n>[, <n>]'", i)
                requirements.append(Requirement(m.group(1), m.group(2).strip(),
                                                _ints(m.group(3), i, "requirement cards")))
            continue
        m = _CARD.match(line)
        if m:
            i += 1
            fields = []
            ctx: list[str] = []
            unknown: list[tuple[int, str]] = []
            in_ctx = False
            while i < n and not lines[i].startswith("## "):
                text = lines[i]
                fm = _FIELD.match(text)
                if fm and fm.group(1) in CARD_KEYS:
                    fields.append((fm.group(1), fm.group(2).strip(), i + 1))
                    i += 1
                    continue
                if fm:
                    unknown.append((i + 1, fm.group(1)))
                if text.strip():
                    in_ctx = True
                if in_ctx:
                    ctx.append(text)
                i += 1
            context = "\n".join(ctx).strip()
            if context.startswith("Context:"):
                context = context[len("Context:"):].strip()
            cards.append(replace(_parse_card(int(m.group(1)), m.group(2), ln, fields, context),
                                 unknown_keys=tuple(unknown)))
            continue
        if line.strip():
            raise DeckError(f"unexpected text {line.strip()!r}", ln)
        i += 1

    return Deck(
        path=path, title=title, source=header["Source"], base_ref=base_ref.strip(),
        base_sha=base_sha.strip(), branch=header["Branch"], build=header.get("Build", ""),
        test=header.get("Test", ""), profile=header.get("Profile") or None,
        finish=header.get("Finish") or None, requirements=tuple(requirements),
        cards=tuple(cards), digest=digest)


def _cycle_free_order(numbers: list[int], deps: dict[int, tuple[int, ...]]):
    """Kahn's algorithm, smallest ready number first. Returns (order, leftover)."""
    remaining = {c: {d for d in deps[c] if d in deps} for c in numbers}
    ready = [c for c, d in remaining.items() if not d]
    heapq.heapify(ready)
    order = []
    while ready:
        c = heapq.heappop(ready)
        order.append(c)
        for o, d in remaining.items():
            if c in d:
                d.discard(c)
                if not d and o not in order and o not in ready:
                    heapq.heappush(ready, o)
    return order, sorted(c for c in remaining if c not in order)


def dependency_order(deck: Deck) -> list[int]:
    deps = {c.number: c.depends_on for c in deck.cards}
    order, left = _cycle_free_order(sorted(deps), deps)
    if left:
        raise ValueError(f"dependency cycle among cards {', '.join(map(str, left))}")
    return order


def check_deck(deck: Deck, repo: Path, config: Config) -> list[Problem]:
    repo = Path(repo)
    out: list[Problem] = []

    def err(msg, card=None):
        out.append(Problem("error", card, msg))

    def warn(msg, card=None):
        out.append(Problem("warning", card, msg))

    for msg in unsupported_settings(config, repo=repo):
        err(msg)
    for msg in unsupported_warnings(config):
        warn(msg)
    if deck.finish not in (None, "none"):
        err(f"not supported in this version: Finish: {deck.finish}")
    if not deck.build.strip():
        err("Build is empty")
    if not deck.test.strip():
        err("Test is empty")
    if not deck.base_sha or not re.fullmatch(r"[0-9a-fA-F]{4,64}", deck.base_sha):
        err(f"base commit {deck.base_sha!r} is not a commit hash")
    else:
        try:
            git(repo, "cat-file", "-e", f"{deck.base_sha}^{{commit}}", timeout=GIT_TIMEOUT)
        except GitError:
            err(f"base commit {deck.base_sha} not found in {repo}")

    seen: set[int] = set()
    for c in deck.cards:
        if c.number in seen:
            err(f"duplicate card number {c.number}", c.number)
        seen.add(c.number)
    numbers = seen

    for c in deck.cards:
        p = f"card {c.number}: "
        for key in c.missing:
            err(f"{p}missing {key}", c.number)
        if c.role == "scout":
            err(f"{p}not supported in this version: scout cards", c.number)
        if c.hard and not c.hard_reason:
            err(f"{p}Hard: yes needs a reason", c.number)
        for d in c.depends_on:
            if d not in numbers:
                err(f"{p}depends on unknown card {d}", c.number)
        for ln, key in c.unknown_keys:
            warn(f'{p}line {ln}: "{key}" is not a card field; treated as context', c.number)
        for path in c.allowed_paths:
            if not (repo / path).exists():
                warn(f"{p}allowed path {path} does not exist yet", c.number)

    deps = {c.number: c.depends_on for c in deck.cards}
    _, left = _cycle_free_order(sorted(deps), deps)
    if left:
        err(f"dependency cycle among cards {', '.join(map(str, left))}")

    for r in deck.requirements:
        if not r.cards:
            warn(f"requirement {r.id} is uncovered (cards none)")
        for cn in r.cards:
            if cn not in numbers:
                err(f"requirement {r.id} maps to card {cn}, which does not exist")
    return out


def _cmd_check(args) -> int:
    repo = Path(args.repo).resolve()  # git calls need an absolute workspace
    try:
        deck = parse_deck(Path(args.deck))
        config = load_config(repo)
    except (DeckError, ConfigError, OSError, UnicodeDecodeError) as e:
        print(f"error: {e}")
        return 1
    problems = check_deck(deck, repo, config)
    for p in problems:
        print(f"{p.level}: {p.message}")
    errors = sum(p.level == "error" for p in problems)
    if not errors:
        print(f"ok: {len(deck.cards)} cards")
    return 1 if errors else 0


def add_commands(sub) -> None:
    p = sub.add_parser("check", help="validate a deck before a run")
    p.add_argument("deck")
    p.add_argument("--repo", default=".")
    p.set_defaults(func=_cmd_check)
