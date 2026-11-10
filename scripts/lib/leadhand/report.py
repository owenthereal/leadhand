"""`report.md`: the run's account of itself, rendered from recorded state only."""
from __future__ import annotations

import sys
from pathlib import Path

from leadhand import status as st
from leadhand.state import run_paths

IMPLEMENTER_KINDS = ("implement", "continue", "fix", "answer", "integrate", "conflict")


def _dollars(value: float | None) -> str:
    return "not priced" if value is None else f"${value:.2f}"


def _short(sha: str | None) -> str:
    return (sha or "")[:12]


def _section(title: str, lines: list[str]) -> list[str]:
    return [f"## {title}", "", *(lines or ["None."]), ""]


def _attention(snap: st.Snapshot, status: dict) -> list[str]:
    return [f"- {a}" for a in status["attention"]]


def _result(snap: st.Snapshot) -> list[str]:
    meta = snap.meta
    accepted = [(e.get("card"), e.get("sha")) for e in snap.events if e.get("kind") == "accepted"]
    base = f"{meta.get('base_ref', '')} @ {_short(meta.get('base_sha'))}".strip(" @")
    out = [f"- Run branch: {meta.get('branch', 'unknown')}",
           f"- Workspace: {meta.get('workspace', 'unknown')}",
           f"- Base: {base or 'unknown'}"]
    if accepted:
        out.append("- Commits:")
        out.extend(f"  - card {n}: {_short(sha)}" for n, sha in accepted)
    else:
        out.append("- Commits: none accepted")
    outcome = snap.outcome or {}
    out.append(f"- Next step: {outcome.get('next_step') or 'none'}")
    return out


def _evidence(snap: st.Snapshot) -> list[str]:
    out = []
    for e in snap.events:
        if e.get("kind") == "gate_result":
            verdict = "passed" if e.get("passed") else "failed"
            out.append(f"- {e.get('gate', 'gate')}: {verdict} on commit {_short(e.get('examined_sha'))}")
    return out


def _unresolved(snap: st.Snapshot) -> list[str]:
    out = []
    for reason in (snap.outcome or {}).get("reasons") or []:
        if not (isinstance(reason, str) and reason.startswith("card ") and " parked" in reason):
            out.append(f"- {reason}")
    return out


def _worker_attempts(snap: st.Snapshot) -> list[dict]:
    """One record per dispatch, proxy records excluded (they are the engine's record of the
    proxy that ran a Codex worker, not a worker's own result)."""
    seen: dict[str, dict] = {}
    for a in snap.attempts:
        if a.get("role") == "proxy":
            continue
        seen[a.get("dispatch") or id(a)] = a
    return list(seen.values())


def _cards(snap: st.Snapshot) -> list[str]:
    requests = st._requests(snap.events)
    workers = _worker_attempts(snap)
    out = ["| Card | Outcome | Attempts | Final route |", "| --- | --- | --- | --- |"]
    for n in sorted(snap.cards):
        c = snap.cards[n]
        attempts = len({a.get("attempt") for a in workers if a.get("card") == n})
        impl = [r for r in requests.values() if r.get("card") == n and r.get("dispatch_kind") in IMPLEMENTER_KINDS]
        route = st.route_text(impl[-1]) if impl else ""
        outcome = c.state + (f": {c.park_reason}" if c.state == "parked" and c.park_reason else "")
        out.append(f"| {n}. {snap.titles.get(n, '')} | {outcome} | {attempts} | {route or '-'} |")
    return out


def _rulings(snap: st.Snapshot) -> list[str]:
    return [f"- Card {e.get('card')}: {e.get('text')}" for e in snap.events if e.get("kind") == "ruling"]


def _usage(snap: st.Snapshot, sessions_dir: Path | None) -> list[str]:
    u = st.usage_summary(snap, sessions_dir)
    out = [f"Estimated API-equivalent cost (an estimate that ranks roles and routes, not subscription "
           f"use): {_dollars(u['estimated_usd'])}", "", "By role:"]
    for role in sorted(u["by_role"]):
        r = u["by_role"][role]
        out.append(f"- {role}: {_dollars(r['estimated_usd'])} ({r['input']} input, {r['output']} output tokens)")
    if not u["by_role"]:
        out.append("- none charged")
    out += ["", "By vendor:"]
    for vendor in sorted(u["by_vendor"]):
        r = u["by_vendor"][vendor]
        out.append(f"- {vendor}: {_dollars(r['estimated_usd'])} ({r['input']} input, {r['output']} output tokens)")
    if not u["by_vendor"]:
        out.append("- none charged")
    out += ["", f"Attempts with unknown usage: {u['unknown_attempts']}"]
    if u["undercounted_attempts"]:
        out.append(f"{u['undercounted_attempts']} attempts may be undercounted (stopped before their "
                   f"usage was complete)")
    before_p, before_t = st.read_meter_start(snap.root)
    after = u["codex_meter"]

    def meter(p, t):
        return "unknown" if p is None else f"{p:.1f}% (observed {t})"
    out += ["", f"Codex meter before: {meter(before_p, before_t)}",
            f"Codex meter after: {meter(after['percent'], after['observed_at'])}",
            "Claude subscription meter: unknown"]
    return out


def _routing(snap: st.Snapshot) -> list[str]:
    table: dict[tuple[str, str], dict[str, int]] = {}
    for a in _worker_attempts(snap):
        if a.get("attempt") != 1:
            continue
        route = " ".join(str(a[k]) for k in ("vendor", "model", "effort") if a.get(k))
        row = table.setdefault((str(a.get("role")), route), {})
        status = str(a.get("status") or "unknown")
        row[status] = row.get(status, 0) + 1
    return [f"- {role} · {route}: " + ", ".join(f"{n} {s}" for s, n in sorted(row.items()))
            for (role, route), row in sorted(table.items())]


def render_report(run_dir: Path, sessions_dir: Path | None = None) -> str:
    snap = st.load(run_dir)
    status = st.build_status(run_dir, sessions_dir)
    outcome = snap.outcome
    if outcome is None:
        line = "Outcome: in progress"
    else:
        qualifier = outcome.get("qualifier") or ""
        line = f"Outcome: {outcome.get('outcome')}" + (f" ({qualifier})" if qualifier else "")
    parts = [f"# Run report: {snap.title}", "", line, ""]
    parts += _section("Needs your attention", _attention(snap, status))
    parts += _section("Result", _result(snap))
    parts += _section("Evidence", _evidence(snap))
    parts += _section("Unresolved", _unresolved(snap))
    parts += _section("Cards", _cards(snap))
    parts += _section("Rulings", _rulings(snap))
    parts += _section("Usage", _usage(snap, sessions_dir))
    parts += _section("Routing suggestions", _routing(snap))
    return "\n".join(parts).rstrip("\n") + "\n"


def write_report(run_dir: Path, sessions_dir: Path | None = None) -> Path:
    path = run_paths(Path(run_dir)).report
    text = render_report(run_dir, sessions_dir)
    tmp = path.with_name(f".{path.name}.tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(path)
    return path


def _cmd_report(args) -> int:
    root = Path(args.run_dir)
    if not run_paths(root).run_json.is_file():
        print(f"error: not a run directory (no run.json): {root}", file=sys.stderr)
        return 1
    print(write_report(root))
    return 0


def add_commands(sub) -> None:
    p = sub.add_parser("report", help="write report.md for a run and print its path")
    p.add_argument("run_dir")
    p.set_defaults(func=_cmd_report)
