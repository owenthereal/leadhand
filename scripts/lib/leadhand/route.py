"""Deterministic route resolution: vendor, model and effort for a card and role."""
from __future__ import annotations

import shutil
from dataclasses import dataclass
from pathlib import Path

from leadhand.config import EFFORTS, VENDORS, Config, ConfigError, Route, RoleSpec, load_config
from leadhand.deck import Card, DeckError, parse_deck

PROFILE_SHIFT = {"lean": -1, "standard": 0, "thorough": 1}
IMPLEMENTER_ROLES = ("implementer", "implementer_hard")
RISK_ROLES = IMPLEMENTER_ROLES + ("reviewer", "reviewer_hard")


class LadderExhausted(Exception):
    pass


@dataclass(frozen=True)
class Resolution:
    route: Route
    role: str
    reasons: tuple[str, ...]


def _signed(n: int) -> str:
    return f"{n:+d}"


def shift_effort(effort: str, steps: int, *, allow_max: bool = False) -> str:
    """Move effort by steps, clamped to low..xhigh (low..max when allow_max)."""
    top = len(EFFORTS) - 1 if allow_max else EFFORTS.index("xhigh")
    return EFFORTS[max(0, min(top, EFFORTS.index(effort) + steps))]


def _spec(config: Config, role: str) -> RoleSpec:
    try:
        return config.roles[role]
    except KeyError:
        raise ConfigError(f"roles.{role}: not configured") from None


def _available(spec: RoleSpec, installed) -> list[str]:
    return [v for v in VENDORS if v in installed and v in spec.routes]


def _pick_vendor(spec: RoleSpec, installed, last: str | None, role: str) -> tuple[str, str]:
    avail = _available(spec, installed)
    if not avail:
        raise ConfigError(f"no vendor available for role {role}: none of its routes is installed")
    if spec.prefer == "other":
        if last is None:
            return avail[0], f"vendor {avail[0]}: prefer other"
        differing = [v for v in avail if v != last]
        if differing:
            return differing[0], f"vendor {differing[0]}: other than {last}"
        return avail[0], f"vendor {avail[0]}: only vendor available"
    if spec.prefer in avail:
        return spec.prefer, f"vendor {spec.prefer}: prefer"
    v = avail[0]
    why = "not installed" if spec.prefer not in installed else "has no route"
    return v, f"vendor {v}: {spec.prefer} {why}"


def _pin_text(pin: Route) -> str:
    return f"{pin.vendor} {pin.model} {pin.effort}"


def resolve(config: Config, *, role: str, card: Card | None = None, attempt: int = 1,
            last_implementer_vendor: str | None = None,
            installed: frozenset[str] = frozenset(VENDORS),
            security: bool = False, profile: str | None = None) -> Resolution:
    """Resolve the route for a role.

    Escalation (implementer roles only): attempt 2 is attempt 1 plus one effort
    step. Attempt 3 uses implementer_hard with the profile and risk steps, plus
    one more step when the card was already Hard or pinned. Attempt 4 uses
    implementer_hard on the vendor other than attempt 3's, with the same effort
    computation as attempt 3. Attempt 5 or later, or no other vendor for attempt
    4, raises LadderExhausted.
    """
    if attempt < 1:
        raise ValueError(f"attempt must be 1 or more, got {attempt}")
    hard = bool(card and card.hard)
    pin = card.route_pin if card else None
    reasons: list[str] = []
    is_impl = role in IMPLEMENTER_ROLES or role == "implementer"
    if is_impl and attempt >= 5:
        raise LadderExhausted(f"attempt {attempt}: the escalation ladder is exhausted")

    # Pin: replaces role, vendor and effort steps for attempts 1 and 2.
    if is_impl and pin is not None and attempt <= 2:
        if pin.vendor not in installed:
            raise ConfigError(f"card {card.number} is pinned to {pin.vendor}, which is not installed")
        steps = 0
        reasons.append(f"pinned: {_pin_text(pin)}")
        if attempt == 2:
            steps = 1
            reasons.append("attempt 2: +1")
        effort = shift_effort(pin.effort, steps, allow_max=pin.effort == "max")
        if EFFORTS.index(pin.effort) + steps != EFFORTS.index(effort):
            reasons.append(f"clamped to {effort}")
        return Resolution(Route(pin.vendor, pin.model, effort), role, tuple(reasons))

    # Step 1: role key.
    key = role
    if role == "implementer":
        if hard:
            key = "implementer_hard"
            reasons.append("role implementer_hard: card is Hard")
    elif role == "reviewer":
        why = ("card is Hard" if hard else "risk high" if card and card.risk == "high"
               else "security review" if security else None)
        if why:
            key = "reviewer_hard"
            reasons.append(f"role reviewer_hard: {why}")
    if is_impl and attempt >= 3:
        if key != "implementer_hard":
            reasons.append("role implementer_hard: attempt %d" % attempt)
        key = "implementer_hard"
    spec = _spec(config, key)

    # Step 2: vendor.
    vendor, why = _pick_vendor(spec, installed, last_implementer_vendor, key)
    if is_impl and attempt == 4:
        third, _ = _pick_vendor(spec, installed, last_implementer_vendor, key)  # attempt 3's vendor
        others = [v for v in _available(spec, installed) if v != third]
        if not others:
            raise LadderExhausted("attempt 4: no other vendor is installed with an implementer_hard route")
        vendor, why = others[0], f"vendor {others[0]}: other than {third}"
    reasons.append(why)
    base = spec.routes[vendor]

    # Step 3: effort steps.
    steps = 0
    if (profile or config.profile) not in PROFILE_SHIFT:
        raise ConfigError(f"profile: {(profile or config.profile)!r} is not one of lean, standard, thorough")
    shift = PROFILE_SHIFT[profile or config.profile]
    if shift:
        steps += shift
        reasons.append(f"profile {profile or config.profile}: {_signed(shift)}")
    if card and card.risk == "high" and key in RISK_ROLES:
        steps += 1
        reasons.append("risk high: +1")
    if is_impl:
        if attempt == 2:
            steps += 1
            reasons.append("attempt 2: +1")
        elif attempt >= 3 and (hard or pin is not None):
            steps += 1
            reasons.append(f"attempt {attempt}: +1")

    # Step 6: clamp.
    raw = EFFORTS.index(base.effort) + steps
    effort = shift_effort(base.effort, steps)
    if raw != EFFORTS.index(effort):
        reasons.append(f"clamped to {effort}")
    return Resolution(Route(vendor, base.model, effort), key, tuple(reasons))


def _installed_from(text: str | None) -> frozenset[str]:
    if text is None:
        return frozenset(v for v in VENDORS if shutil.which(v))
    names = frozenset(t.strip() for t in text.split(",") if t.strip())
    bad = names - set(VENDORS)
    if bad:
        raise ConfigError(f"--installed: unknown vendor {sorted(bad)[0]!r}")
    return names


def _cmd_route(args) -> int:
    import sys
    try:
        deck = parse_deck(Path(args.deck))
        config = load_config(Path(args.repo))
        card = next((c for c in deck.cards if c.number == args.card), None)
        if card is None:
            raise DeckError(f"no card {args.card} in {args.deck}")
        res = resolve(config, role=args.role, card=card, attempt=args.attempt,
                      last_implementer_vendor=args.last_vendor,
                      installed=_installed_from(args.installed), profile=deck.profile)
    except LadderExhausted as e:
        print(f"ladder exhausted: {e}", file=sys.stderr)
        return 1
    except (DeckError, ConfigError, ValueError, OSError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    r = res.route
    print(f"{r.vendor} {r.model} {r.effort}")
    for reason in res.reasons:
        print(reason)
    return 0


def add_commands(sub) -> None:
    import argparse
    p = sub.add_parser("route", help="print the route for a card and role")
    p.add_argument("deck")
    p.add_argument("card", type=int)
    p.add_argument("role")
    p.add_argument("--attempt", type=int, default=1)
    p.add_argument("--last-vendor", choices=VENDORS)
    p.add_argument("--repo", default=".")
    p.add_argument("--installed", help=argparse.SUPPRESS)
    p.set_defaults(func=_cmd_route)
