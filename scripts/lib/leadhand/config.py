"""Configuration loading: presets, user file and repo file, merged and validated."""
from __future__ import annotations

import json
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


SNAPSHOT = "config.json"


def config_to_dict(config: Config) -> dict:
    """The effective configuration as a plain table that `config_from_dict` (the same
    validation as the TOML files) turns back into an equal Config."""
    return {
        "profile": config.profile,
        "docs_dir": config.docs_dir,
        "run": {"worktree": config.run.worktree, "test_first": config.run.test_first,
                "evidence": config.run.evidence, "card_review": config.run.card_review,
                "cost_limit_usd": config.run.cost_limit_usd, "branch_prefix": config.run.branch_prefix},
        "after_run": {"step": config.after_run.step, "on_incomplete": config.after_run.on_incomplete,
                      "repeat_safe": config.after_run.repeat_safe, "check": config.after_run.check},
        "capacity": {"codex_pause_percent": config.codex_pause_percent},
        "codex": {"network": config.codex.network, "writable_dirs": list(config.codex.writable_dirs)},
        "limits": {role: {"steps": lim.steps, "minutes": lim.minutes} for role, lim in config.limits.items()},
        "roles": {role: {**{v: f"{r.model} {r.effort}" for v, r in spec.routes.items()}, "prefer": spec.prefer}
                  for role, spec in config.roles.items()},
        "notify": {"command": config.notify_command},
        "prices": {fam: dict(p) for fam, p in config.prices.items()},
    }


def config_from_dict(d: Mapping[str, Any]) -> Config:
    if not isinstance(d, dict):
        raise ConfigError("the configuration snapshot is not a table")
    return _build(dict(d))


def load_run_config(run_dir: Path, repo: Path) -> Config:
    """A run's configuration: the snapshot `start` wrote into the run directory (I5), so a
    worker that edits `.leadhand.toml` or the user file mid-run changes nothing for the run.
    A run started before snapshots existed reads the live files."""
    path = Path(run_dir) / SNAPSHOT
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return load_config(Path(repo))
    except (OSError, ValueError) as e:
        raise ConfigError(f"{path}: {e}") from None
    return config_from_dict(data)


def docs_dir_for(config: Config, repo: Path) -> Path:
    """The run docs directory, always absolute: a relative docs_dir is resolved against the
    repository, so start (from any cwd), run discovery and the wrap-up (from the workspace)
    all use the same directory."""
    root = Path(repo).resolve()
    docs = Path(config.docs_dir.replace("{repo}", root.name)).expanduser()
    return docs if docs.is_absolute() else (root / docs).resolve()


def unsupported_settings(config: Config, repo: Path | None = None) -> list[str]:
    """Settings this version refuses (an error in check, and start refuses). With `repo`,
    also a docs_dir that resolves inside the repository (a relative one is resolved
    against the repository): the run's own files would sit in the workspace (ruling R48)."""
    out = []
    if repo is not None:
        root = Path(os.path.realpath(repo))
        docs = Path(os.path.realpath(docs_dir_for(config, root)))
        if docs == root or root in docs.parents:
            out.append(f"not supported in this version: docs_dir inside the repository ({docs})")
    if config.after_run.step != "none":
        out.append(f"not supported in this version: after_run.step = {config.after_run.step}")
    if config.after_run.on_incomplete != "none":
        out.append(f"not supported in this version: after_run.on_incomplete = {config.after_run.on_incomplete}")
    if config.run.cost_limit_usd != 0:
        c = config.run.cost_limit_usd
        out.append(f"not supported in this version: run.cost_limit_usd = {int(c) if c == int(c) else c}")
    out += _unhonoured_step_limits(config)
    return out


def _unhonoured_step_limits(config: Config) -> list[str]:
    """`limits.<role>.steps` that a Claude worker cannot honour (ruling R52): the Agent tool
    has no per-dispatch turn limit, so a Claude worker stops at its agent file's maxTurns. A
    limit is refused when it differs from that for any role using it that has a Claude route."""
    from leadhand.briefs import ROLE_AGENT, agent_max_turns
    from leadhand.dispatch import LIMITS_ROLE
    out = []
    for key, lim in config.limits.items():
        turns = sorted({agent_max_turns(ROLE_AGENT[role]) for role, spec in config.roles.items()
                        if role in ROLE_AGENT and "claude" in spec.routes and LIMITS_ROLE.get(role, role) == key}
                       - {None, lim.steps})
        for m in turns:
            out.append(f"not supported in this version: limits.{key}.steps = {lim.steps} "
                       f"(Claude workers use the agent file's maxTurns = {m})")
    return out


DEFAULT_CODEX_PAUSE_PERCENT = 95       # the presets' value


def unsupported_warnings(config: Config) -> list[str]:
    """Settings this version reads but does not act on yet: a warning, not an error."""
    out = []
    if config.notify_command:
        out.append(f"not supported in this version: notify.command = {config.notify_command}")
    if config.codex_pause_percent != DEFAULT_CODEX_PAUSE_PERCENT:
        out.append(f"not supported in this version: capacity.codex_pause_percent = {config.codex_pause_percent}")
    return out
