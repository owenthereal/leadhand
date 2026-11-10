"""Configuration loading: presets, user file and repo file, merged and validated."""
from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

EFFORTS = ("low", "medium", "high", "xhigh", "max")
VENDORS = ("claude", "codex")
PRICE_KEYS = ("input", "output", "cache_read", "cache_write_5m", "cache_write_1h")
PRESETS_DIR = Path(__file__).resolve().parents[3] / "presets"
DEFAULT_PRESET = "claude-codex"


class ConfigError(ValueError):
    pass


@dataclass(frozen=True)
class Route:
    vendor: str
    model: str
    effort: str


@dataclass(frozen=True)
class RoleSpec:
    routes: Mapping[str, Route]
    prefer: str  # "claude" | "codex" | "other"


@dataclass(frozen=True)
class Limits:
    steps: int
    minutes: int


@dataclass(frozen=True)
class RunSettings:
    worktree: bool
    test_first: str
    evidence: str
    card_review: str
    cost_limit_usd: float
    branch_prefix: str


@dataclass(frozen=True)
class AfterRun:
    step: str
    on_incomplete: str
    repeat_safe: bool
    check: str


@dataclass(frozen=True)
class CodexSettings:
    network: bool
    writable_dirs: tuple[str, ...]


@dataclass(frozen=True)
class Config:
    profile: str
    docs_dir: str
    run: RunSettings
    after_run: AfterRun
    codex_pause_percent: int
    codex: CodexSettings
    limits: Mapping[str, Limits]
    roles: Mapping[str, RoleSpec]
    notify_command: str
    prices: Mapping[str, Mapping[str, float]]


TOP_KEYS = {"preset", "profile", "docs_dir", "run", "after_run", "capacity",
            "codex", "limits", "roles", "notify", "prices"}


def parse_route(vendor: str, text: str) -> Route:
    if vendor not in VENDORS:
        raise ConfigError(f"unknown vendor {vendor!r}")
    parts = text.split() if isinstance(text, str) else []
    if len(parts) != 2:
        raise ConfigError(f"expected '<model> <effort>', got {text!r}")
    model, effort = parts
    if effort not in EFFORTS:
        raise ConfigError(f"effort {effort!r} is not one of {', '.join(EFFORTS)}")
    return Route(vendor, model, effort)


def _read(path: Path | None) -> dict:
    if path is None or not path.is_file():
        return {}
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except tomllib.TOMLDecodeError as e:
        raise ConfigError(f"{path}: {e}") from e


def _merge(base: dict, over: dict) -> dict:
    out = dict(base)
    for k, v in over.items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = _merge(out[k], v)
        else:
            out[k] = v
    return out


def _table(d: Mapping, key: str, allowed: set[str] | None = None) -> Mapping:
    v = d.get(key, {})
    if not isinstance(v, dict):
        raise ConfigError(f"{key}: expected a table")
    if allowed is not None:
        extra = set(v) - allowed
        if extra:
            raise ConfigError(f"{key}.{sorted(extra)[0]}: unknown key")
    return v


def _get(d: Mapping, prefix: str, key: str, typ, choices=None):
    path = f"{prefix}.{key}" if prefix else key
    if key not in d:
        raise ConfigError(f"{path}: missing")
    v = d[key]
    if typ is float:
        ok = isinstance(v, (int, float)) and not isinstance(v, bool)
    elif typ is int:
        ok = isinstance(v, int) and not isinstance(v, bool)
    else:
        ok = isinstance(v, typ)
    if not ok:
        raise ConfigError(f"{path}: expected {typ.__name__}, got {v!r}")
    if choices is not None and v not in choices:
        raise ConfigError(f"{path}: {v!r} is not one of {', '.join(choices)}")
    return v


def _step_ok(step: str) -> bool:
    if step in ("none", "pr"):
        return True
    return any(step.startswith(p) and len(step) > len(p) for p in ("skill:", "command:"))


def _build(d: dict) -> Config:
    extra = set(d) - TOP_KEYS
    if extra:
        raise ConfigError(f"{sorted(extra)[0]}: unknown top-level key")
    profile = _get(d, "", "profile", str, ("lean", "standard", "thorough"))
    docs_dir = _get(d, "", "docs_dir", str)

    r = _table(d, "run", {"worktree", "test_first", "evidence", "card_review", "cost_limit_usd", "branch_prefix"})
    run = RunSettings(
        _get(r, "run", "worktree", bool),
        _get(r, "run", "test_first", str, ("when-cheap", "always", "off")),
        _get(r, "run", "evidence", str, ("record", "block")),
        _get(r, "run", "card_review", str, ("always", "by-risk")),
        float(_get(r, "run", "cost_limit_usd", float)),
        _get(r, "run", "branch_prefix", str),
    )
    a = _table(d, "after_run", {"step", "on_incomplete", "repeat_safe", "check"})
    step = _get(a, "after_run", "step", str)
    if not _step_ok(step):
        raise ConfigError(f"after_run.step: {step!r} must be none, pr, skill:<name> or command:<cmd>")
    after = AfterRun(
        step,
        _get(a, "after_run", "on_incomplete", str, ("none", "draft-pr")),
        _get(a, "after_run", "repeat_safe", bool),
        _get(a, "after_run", "check", str),
    )
    cap = _table(d, "capacity", {"codex_pause_percent"})
    pause = _get(cap, "capacity", "codex_pause_percent", int)
    if not 0 <= pause <= 100:
        raise ConfigError(f"capacity.codex_pause_percent: {pause} must be 0-100")
    cx = _table(d, "codex", {"network", "writable_dirs"})
    dirs = _get(cx, "codex", "writable_dirs", list)
    if not all(isinstance(x, str) for x in dirs):
        raise ConfigError("codex.writable_dirs: expected a list of strings")
    codex = CodexSettings(_get(cx, "codex", "network", bool), tuple(dirs))

    limits = {}
    for role, v in _table(d, "limits").items():
        if not isinstance(v, dict) or set(v) != {"steps", "minutes"}:
            raise ConfigError(f"limits.{role}: expected {{ steps, minutes }}")
        limits[role] = Limits(_get(v, f"limits.{role}", "steps", int), _get(v, f"limits.{role}", "minutes", int))

    roles = {}
    for role, v in _table(d, "roles").items():
        if not isinstance(v, dict):
            raise ConfigError(f"roles.{role}: expected a table")
        extra = set(v) - {*VENDORS, "prefer"}
        if extra:
            raise ConfigError(f"roles.{role}.{sorted(extra)[0]}: unknown key")
        routes = {}
        for vendor in VENDORS:
            if vendor in v:
                try:
                    routes[vendor] = parse_route(vendor, v[vendor])
                except ConfigError as e:
                    raise ConfigError(f"roles.{role}.{vendor}: {e}") from e
        if not routes:
            raise ConfigError(f"roles.{role}: needs a claude or codex route")
        prefer = v.get("prefer", "claude")
        if prefer not in ("claude", "codex", "other"):
            raise ConfigError(f"roles.{role}.prefer: {prefer!r} is not one of claude, codex, other")
        if prefer in VENDORS and prefer not in routes:
            raise ConfigError(f"roles.{role}.prefer: no {prefer} route defined")
        roles[role] = RoleSpec(routes, prefer)

    notify = _table(d, "notify", {"command"})
    prices = {}
    for fam, v in _table(d, "prices").items():
        if not isinstance(v, dict) or set(v) != set(PRICE_KEYS):
            raise ConfigError(f"prices.{fam}: expected keys {', '.join(PRICE_KEYS)}")
        prices[fam] = {k: float(_get(v, f"prices.{fam}", k, float)) for k in PRICE_KEYS}

    return Config(profile, docs_dir, run, after, pause, codex, limits, roles,
                  _get(notify, "notify", "command", str), prices)


def load_config(repo: Path, *, user_file: Path | None = None, presets_dir: Path = PRESETS_DIR) -> Config:
    repo = Path(repo)
    if user_file is None:
        env = os.environ.get("LEADHAND_CONFIG")
        user_file = Path(env) if env else Path.home() / ".config" / "leadhand" / "config.toml"
    user = _read(user_file)
    repo_cfg = _read(repo / ".leadhand.toml")
    name = repo_cfg.get("preset", user.get("preset", DEFAULT_PRESET))
    if not isinstance(name, str) or not name or "/" in name or name.startswith("."):
        raise ConfigError(f"preset: invalid name {name!r}")
    preset_path = presets_dir / f"{name}.toml"
    if not preset_path.is_file():
        raise ConfigError(f"preset: unknown preset {name!r}")
    merged = _merge(_merge(_read(preset_path), user), repo_cfg)
    return _build(merged)


def docs_dir_for(config: Config, repo: Path) -> Path:
    text = config.docs_dir.replace("{repo}", Path(repo).resolve().name)
    return Path(text).expanduser()


def unsupported_settings(config: Config) -> list[str]:
    out = []
    if config.after_run.step != "none":
        out.append(f"not supported in this version: after_run.step = {config.after_run.step}")
    if config.after_run.on_incomplete != "none":
        out.append(f"not supported in this version: after_run.on_incomplete = {config.after_run.on_incomplete}")
    if config.run.cost_limit_usd != 0:
        c = config.run.cost_limit_usd
        out.append(f"not supported in this version: run.cost_limit_usd = {int(c) if c == int(c) else c}")
    return out
