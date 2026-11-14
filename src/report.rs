//! `report.md` (`report.py`): the run's account of itself, rendered from recorded state only,
//! and `leadhand report`, which writes it and prints its path.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::error::{Error, Result};
use crate::pytext::{pure_path, py_fixed};
use crate::runsel;
use crate::state::events;
use crate::state::{AttemptRecord, CardRef, CardStage, Event, run_paths};
use crate::status::{self, Snapshot, Status, UsageRow};
use crate::store;
use crate::usage::{self, Meter};
use crate::wire::{self, PyMap, PyStr};

const IMPLEMENTER_KINDS: [&str; 6] = [
    "implement",
    "continue",
    "fix",
    "answer",
    "integrate",
    "conflict",
];

fn dollars(value: Option<f64>) -> String {
    value.map_or("not priced".into(), |x| format!("${}", py_fixed(x, 2)))
}

/// `(sha or "")[:12]`.
fn short(sha: Option<&str>) -> String {
    sha.unwrap_or_default().chars().take(12).collect()
}

/// `a` then `b`, joined losslessly.
fn cat(a: &str, b: &PyStr) -> PyStr {
    let mut out = PyStr::from(a);
    out.push(b);
    out
}

/// A field `meta.get(key, default)` reads: missing is `default`, `null` is `None`.
fn or_missing(field: &Option<Option<PyStr>>, default: &str) -> PyStr {
    match field {
        None => default.into(),
        Some(None) => "None".into(),
        Some(Some(value)) => value.clone(),
    }
}

/// `str(card)`, `None` when there is none.
fn card_text(card: Option<&CardRef>) -> PyStr {
    card.map_or_else(|| "None".into(), CardRef::text)
}

fn section(title: &str, lines: Vec<PyStr>) -> Vec<PyStr> {
    let mut out = vec![PyStr::from(format!("## {title}")), PyStr::default()];
    if lines.is_empty() {
        out.push("None.".into());
    } else {
        out.extend(lines);
    }
    out.push(PyStr::default());
    out
}

fn attention(status: &Status) -> Vec<PyStr> {
    status.attention.iter().map(|a| cat("- ", a)).collect()
}

fn result(snap: &Snapshot) -> Vec<PyStr> {
    let meta = &snap.meta;
    let mut base = or_missing(&meta.base_ref, "");
    base.push_str(&format!(" @ {}", short(meta.base_sha.as_deref())));
    let base = base.strip_ascii(b" @", true);
    let mut out = vec![
        cat("- Run branch: ", &or_missing(&meta.branch, "unknown")),
        cat("- Workspace: ", &or_missing(&meta.workspace, "unknown")),
        cat(
            "- Base: ",
            &if base.is_empty() {
                "unknown".into()
            } else {
                base
            },
        ),
    ];
    let accepted: Vec<&events::Accepted> = snap
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Accepted(a) => Some(a),
            _ => None,
        })
        .collect();
    if accepted.is_empty() {
        out.push("- Commits: none accepted".into());
    } else {
        out.push("- Commits:".into());
        for a in accepted {
            let mut line = cat("  - card ", &card_text(a.card.as_ref()));
            line.push_str(&format!(": {}", short(a.sha.as_deref())));
            out.push(line);
        }
    }
    let next = snap
        .outcome()
        .and_then(|o| o.next_step.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".into());
    out.push(format!("- Next step: {next}").into());
    out
}

/// The run branch tip as recorded: the last accepted commit, else the base.
fn run_head(snap: &Snapshot) -> String {
    snap.events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Accepted(a) => a.sha.clone().filter(|s| !s.is_empty()),
            _ => None,
        })
        .unwrap_or_else(|| snap.meta.base_sha.clone().unwrap_or_default())
}

/// The last result of each gate and the commit it examined; stale if that is not the run head.
fn evidence(snap: &Snapshot) -> Vec<PyStr> {
    let mut last: Vec<(String, &events::GateResult)> = Vec::new();
    for e in &snap.events {
        if let Event::GateResult(g) = e {
            let gate = g.gate.clone().unwrap_or_else(|| "gate".into());
            match last.iter_mut().find(|(name, _)| *name == gate) {
                Some(entry) => entry.1 = g,
                None => last.push((gate, g)),
            }
        }
    }
    let head = run_head(snap);
    last.into_iter()
        .map(|(gate, g)| {
            let verdict = if g.passed == Some(true) {
                "passed"
            } else {
                "failed"
            };
            let mut line = format!(
                "- {gate}: {verdict} on commit {}",
                short(g.examined_sha.as_deref())
            );
            if g.examined_sha.as_deref() != Some(head.as_str()) {
                line += &format!(" (stale: the run head is {})", short(Some(&head)));
            }
            line.into()
        })
        .collect()
}

fn unresolved(snap: &Snapshot) -> Vec<PyStr> {
    let mut out = Vec::new();
    let or = |v: &Option<PyStr>, default: &str| {
        v.clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default.into())
    };
    for c in snap.cards.values() {
        let mut line = cat(
            &format!("- Card {} (branch ", c.number),
            &or(&c.branch, "not cut"),
        );
        if c.state == CardStage::Parked {
            line.push_str(") parked, decision deferred: ");
            line.push(&or(&c.park_reason, "no reason recorded"));
        } else if c.state != CardStage::Accepted && c.verdict.as_deref() == Some("defects") {
            line.push_str(") has open review findings: ");
            line.push(&or(&c.findings, "see the review"));
        } else {
            continue;
        }
        out.push(line);
    }
    for reason in snap
        .outcome()
        .and_then(|o| o.reasons.as_ref())
        .into_iter()
        .flatten()
    {
        if !status::is_park_reason(reason) {
            out.push(cat("- ", reason));
        }
    }
    out
}

/// One record per dispatch, proxy records excluded: they are the engine's record of the proxy
/// that ran a Codex worker, not a worker's own result. A later record of a dispatch replaces
/// its earlier one in place, as a `dict` keyed by dispatch does.
fn worker_attempts(snap: &Snapshot) -> Vec<&AttemptRecord> {
    let mut seen: Vec<&AttemptRecord> = Vec::new();
    for a in &snap.attempts {
        if a.role.as_deref() == Some("proxy") {
            continue;
        }
        let dispatch = a.dispatch.as_deref().filter(|d| !d.is_empty());
        match dispatch.and_then(|d| seen.iter().position(|s| s.dispatch.as_deref() == Some(d))) {
            Some(at) => seen[at] = a,
            None => seen.push(a),
        }
    }
    seen
}

fn cards(snap: &Snapshot) -> Vec<PyStr> {
    // The first request of each dispatch, in the order they were first made.
    let mut seen = HashSet::new();
    let order: Vec<&events::DispatchRequested> = snap
        .events
        .iter()
        .filter_map(|e| match e {
            Event::DispatchRequested(d) if seen.insert(d.dispatch_id.as_deref()) => Some(d),
            _ => None,
        })
        .collect();
    let workers = worker_attempts(snap);
    let mut out: Vec<PyStr> = vec![
        "| Card | Outcome | Attempts | Final route |".into(),
        "| --- | --- | --- | --- |".into(),
    ];
    for c in snap.cards.values() {
        let mut attempts: Vec<Option<i64>> = workers
            .iter()
            .filter(|a| a.card.as_ref() == Some(&CardRef::Number(i64::from(c.number))))
            .map(|a| a.attempt)
            .collect();
        attempts.sort_unstable();
        attempts.dedup();
        let implementer = order.iter().rev().find(|r| {
            r.card.as_ref() == Some(&CardRef::Number(i64::from(c.number)))
                && r.dispatch_kind
                    .as_deref()
                    .is_some_and(|k| IMPLEMENTER_KINDS.contains(&k))
        });
        let route = implementer
            .map(|r| status::route_text(Some(*r)))
            .unwrap_or_default();
        let mut outcome = PyStr::from(c.state.as_str());
        if let Some(reason) = c
            .park_reason
            .as_ref()
            .filter(|r| c.state == CardStage::Parked && !r.is_empty())
        {
            outcome.push_str(": ");
            outcome.push(reason);
        }
        let title = snap.titles.get(&c.number).cloned().unwrap_or_default();
        let mut line = PyStr::from(format!("| {}. {title} | ", c.number));
        line.push(&outcome);
        let route = if route.is_empty() {
            "-".to_string()
        } else {
            route
        };
        line.push_str(&format!(" | {} | {route} |", attempts.len()));
        out.push(line);
    }
    out
}

fn rulings(snap: &Snapshot) -> Vec<PyStr> {
    snap.events
        .iter()
        .filter_map(|e| match e {
            Event::Ruling(r) => {
                let mut line = cat("- Card ", &card_text(r.card.as_ref()));
                line.push_str(": ");
                line.push(r.text.as_ref().unwrap_or(&"None".into()));
                Some(line)
            }
            _ => None,
        })
        .collect()
}

fn usage_lines(snap: &Snapshot, summary: &status::UsageSummary) -> Vec<PyStr> {
    let mut out: Vec<String> = vec![
        format!(
            "Estimated API-equivalent cost (an estimate that ranks roles and routes, not \
             subscription use): {}",
            dollars(summary.estimated_usd)
        ),
        String::new(),
        "By role:".into(),
    ];
    let rows = |table: &PyMap<UsageRow>, out: &mut Vec<String>| {
        let sorted: BTreeMap<&[u8], (&PyStr, &UsageRow)> =
            table.0.iter().map(|(k, v)| (k.as_wtf8(), (k, v))).collect();
        for (name, row) in sorted.values() {
            out.push(format!(
                "- {}: {} ({} input, {} output tokens)",
                name.lossy(),
                dollars(row.estimated_usd),
                row.input,
                row.output
            ));
        }
        if table.is_empty() {
            out.push("- none charged".into());
        }
    };
    rows(&summary.by_role, &mut out);
    out.extend([String::new(), "By vendor:".into()]);
    rows(&summary.by_vendor, &mut out);
    out.extend([
        String::new(),
        format!("Attempts with unknown usage: {}", summary.unknown_attempts),
    ]);
    if summary.undercounted_attempts > 0 {
        out.push(format!(
            "{} attempts may be undercounted (stopped before their usage was complete)",
            summary.undercounted_attempts
        ));
    }
    let mut lines: Vec<PyStr> = out.into_iter().map(PyStr::from).collect();
    let meter = |label: &str, m: &Meter| match m.percent {
        None => PyStr::from(format!("{label}unknown")),
        Some(p) => {
            let mut text = PyStr::from(format!("{label}{}% (observed ", py_fixed(p, 1)));
            text.push(m.observed_at.as_ref().unwrap_or(&"None".into()));
            text.push_str(")");
            text
        }
    };
    lines.push(PyStr::default());
    lines.push(meter(
        "Codex meter before: ",
        &status::read_meter_start(&snap.root),
    ));
    lines.push(meter("Codex meter after: ", &summary.codex_meter));
    lines.push("Claude subscription meter: unknown".into());
    lines
}

fn routing(snap: &Snapshot) -> Vec<PyStr> {
    // (role, route) -> status -> count, both sorted as the Python sorts them.
    // A route is carried text (the model and effort a dispatch was given); PyStr's byte order
    // is code point order, as Python sorts `str`.
    let mut table: BTreeMap<(String, PyStr), BTreeMap<String, usize>> = BTreeMap::new();
    for a in worker_attempts(snap) {
        if a.attempt != Some(1) {
            continue;
        }
        let vendor = a.vendor.as_deref().map(PyStr::from);
        let mut route = PyStr::default();
        for part in [vendor.as_ref(), a.model.as_ref(), a.effort.as_ref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
        {
            if !route.is_empty() {
                route.push_str(" ");
            }
            route.push(part);
        }
        let role = a.role.clone().unwrap_or_else(|| "None".into());
        let row = table.entry((role, route)).or_default();
        let status = a
            .status
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into());
        *row.entry(status).or_default() += 1;
    }
    table
        .into_iter()
        .map(|((role, route), row)| {
            let counts: Vec<String> = row.iter().map(|(s, n)| format!("{n} {s}")).collect();
            let mut line = PyStr::from(format!("- {role} · "));
            line.push(&route);
            line.push_str(&format!(": {}", counts.join(", ")));
            line
        })
        .collect()
}

/// `report.md`'s text (`report.render_report`); the Codex meter is read from `sessions_dir`.
pub fn render_report(run_dir: &Path, sessions_dir: &Path) -> Result<PyStr> {
    let snap = status::load(run_dir);
    let summary = status::usage_summary(&snap, sessions_dir, None);
    let status = status::build_status(&snap, sessions_dir, Some(summary.codex_meter.clone()))?;
    let line = match snap.outcome() {
        None => "Outcome: in progress".to_string(),
        Some(o) => {
            let qualifier = o.qualifier.clone().unwrap_or_default();
            format!(
                "Outcome: {}{}",
                o.outcome.as_deref().unwrap_or("None"),
                if qualifier.is_empty() {
                    String::new()
                } else {
                    format!(" ({qualifier})")
                }
            )
        }
    };
    let mut parts: Vec<PyStr> = vec![
        cat("# Run report: ", &snap.title),
        PyStr::default(),
        line.into(),
        PyStr::default(),
    ];
    parts.extend(section("Needs your attention", attention(&status)));
    parts.extend(section("Result", result(&snap)));
    parts.extend(section("Evidence", evidence(&snap)));
    parts.extend(section("Unresolved", unresolved(&snap)));
    parts.extend(section("Cards", cards(&snap)));
    parts.extend(section("Rulings", rulings(&snap)));
    parts.extend(section("Usage", usage_lines(&snap, &summary)));
    parts.extend(section("Routing suggestions", routing(&snap)));
    // "\n".join(parts).rstrip("\n") + "\n"
    let mut text = PyStr::default();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            text.push_str("\n");
        }
        text.push(part);
    }
    let mut text = text.strip_ascii(b"\n", false);
    text.push_str("\n");
    Ok(text)
}

/// Writes `report.md` atomically and returns its path (`report.write_report`).
pub fn write_report(run_dir: &Path, sessions_dir: &Path) -> Result<PathBuf> {
    let path = run_paths(run_dir).report;
    let text = render_report(run_dir, sessions_dir)?;
    store::write_file_atomic(&path, text.to_os_lossy().as_bytes())?;
    Ok(path)
}

/// `report`'s `--json`: the same information as its text output, the report's path.
#[derive(serde::Serialize)]
struct Written {
    report: PyStr,
}

/// `leadhand report [RUN] [--json]`: writes the run's `report.md` and prints its path. The
/// run is found as `runsel::select_run` finds it.
pub fn command(arg: Option<&OsStr>, json: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir().map_err(|e| Error::io(Path::new("."), e))?;
    let root = pure_path(runsel::select_run("report", arg, &cwd)?.as_os_str());
    if !run_paths(&root).run_json.is_file() {
        return Err(Error::msg(format!(
            "not a run directory (no run.json): {}",
            root.display()
        )));
    }
    let path = write_report(&root, &usage::default_sessions_dir())?;
    let mut out = std::io::stdout().lock();
    if json {
        let written = Written {
            report: PyStr::from_os(path.as_os_str()),
        };
        writeln!(out, "{}", wire::to_output(&written)?)?;
    } else {
        out.write_all(path.as_os_str().as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn snap_with(events: &[&str]) -> (tempfile::TempDir, Snapshot) {
        let dir = tempfile::Builder::new()
            .prefix("run dir ")
            .tempdir()
            .unwrap();
        let mut text = String::new();
        for (i, fields) in events.iter().enumerate() {
            text += &format!("{{\"seq\":{},\"ts\":\"t\",{fields}}}\n", i + 1);
        }
        fs::write(dir.path().join("events.jsonl"), text).unwrap();
        fs::write(
            dir.path().join("run.json"),
            r#"{"base_ref": "main", "base_sha": "0123456789abcdef0123", "branch": null}"#,
        )
        .unwrap();
        let snap = status::load(dir.path());
        (dir, snap)
    }

    fn texts(lines: Vec<PyStr>) -> Vec<String> {
        lines.iter().map(|l| l.lossy().into_owned()).collect()
    }

    #[test]
    fn result_reads_meta_as_the_python_does() {
        let (_dir, snap) = snap_with(&[
            r#""kind":"accepted","card":1,"sha":"1111111111111111""#,
            r#""kind":"accepted","card":"final","sha":"2222222222222222""#,
        ]);
        assert_eq!(
            texts(result(&snap)),
            [
                "- Run branch: None",
                "- Workspace: unknown",
                "- Base: main @ 0123456789ab",
                "- Commits:",
                "  - card 1: 111111111111",
                "  - card final: 222222222222",
                "- Next step: none",
            ]
        );
        assert_eq!(PyStr::from(" @ x @ ").strip_ascii(b" @", true).lossy(), "x");
        assert_eq!(PyStr::from(" @ ").strip_ascii(b" @", true).lossy(), "");
        assert_eq!(PyStr::from("a\n\n").strip_ascii(b"\n", false).lossy(), "a");
    }

    #[test]
    fn evidence_keeps_the_first_position_and_the_last_result() {
        let (_dir, snap) = snap_with(&[
            r#""kind":"gate_result","gate":"tests","examined_sha":"aaaa","passed":false"#,
            r#""kind":"gate_result","gate":"lint","examined_sha":"aaaa","passed":true"#,
            r#""kind":"gate_result","gate":"tests","examined_sha":"0123456789abcdef0123","passed":true"#,
        ]);
        assert_eq!(
            texts(evidence(&snap)),
            [
                "- tests: passed on commit 0123456789ab",
                "- lint: passed on commit aaaa (stale: the run head is 0123456789ab)",
            ]
        );
    }

    #[test]
    fn routing_sorts_and_counts_first_attempts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("attempts.jsonl"),
            "{\"dispatch\":\"d2\",\"card\":2,\"attempt\":1,\"role\":\"reviewer\",\"vendor\":\"claude\",\"model\":\"opus\",\"effort\":\"high\",\"status\":\"READY\"}\n\
             {\"dispatch\":\"d1\",\"card\":1,\"attempt\":1,\"role\":\"implementer\",\"vendor\":\"codex\",\"status\":\"SPENT\"}\n\
             {\"dispatch\":\"d1\",\"card\":1,\"attempt\":1,\"role\":\"implementer\",\"vendor\":\"codex\",\"status\":\"READY\"}\n\
             {\"dispatch\":\"d3\",\"card\":1,\"attempt\":2,\"role\":\"implementer\",\"vendor\":\"codex\"}\n\
             {\"dispatch\":\"d1\",\"card\":1,\"attempt\":1,\"role\":\"proxy\",\"vendor\":\"claude\"}\n",
        )
        .unwrap();
        let snap = status::load(dir.path());
        assert_eq!(
            texts(routing(&snap)),
            [
                "- implementer · codex: 1 READY",
                "- reviewer · claude opus high: 1 READY"
            ]
        );
        assert_eq!(worker_attempts(&snap).len(), 3);
    }
}
