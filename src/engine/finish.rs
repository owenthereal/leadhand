//! The end of a run (`engine.py`: `_end`, `_wrap_up`, `_write_handoff`, `_record_run_line`,
//! `_done_action`): the outcome decided and recorded, then, once per outcome, the branch
//! release (a terminal outcome: `release`), the handoff (a blocked run) or its removal,
//! `report.md`, the run's line in `<docs_dir>/runs.md` and `report_written`; then `done`.
//!
//! The outcome's Git reads are in the run's worktree: the run branch's tip, and whether every
//! accepted commit is still on it. Nothing here writes Git but the release.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustix::fs::{FlockOperation, OFlags, flock};
use rustix::io::Errno;

use super::cards::{Log, Run, card_summary, os, short_sha};
use super::next::{Ending, RunState, head_problem, waiting_on_decisions};
use super::protocol::{Action, DoneAction};
use super::record::cancel_codex;
use super::release::release;
use crate::cli::human::shell_quote;
use crate::config::docs_dir_for;
use crate::error::{Error, Result};
use crate::git::cards::{is_ancestor, ref_sha};
use crate::git::workspace::head;
use crate::pytext::{path_text, pure_path, py_fixed, py_isspace, py_split, py_splitlines};
use crate::state::events::{self, Outcome};
use crate::state::{
    Blocked, CardStage, Event, OutcomeInput, allowed_next_step, append_event, decide_outcome,
};
use crate::store::write_file_atomic;
use crate::usage::{Meter, default_sessions_dir};
use crate::wire::PyStr;

/// Decides the outcome and records it, with the allowed next step and the run head (`_end`).
/// The wrap-up follows on the next step (`wrap_up`).
///
/// `gates_current` means every required gate passed on the run head; a gate that failed there
/// for good is also reported through `open_findings`, `holds` or `verification_failed`. Every
/// accepted card and fix must still be on the run branch; otherwise the state cannot be
/// reconciled and the run fails (`unreconciled` names another mismatch). When everything left
/// needs the operator's decision, the run ends blocked for it (R44). `cancelled`: the
/// operator's cancel, after every worker stopped.
pub(super) fn end(st: &RunState, ending: &Ending) -> Result<()> {
    let run = st.run;
    if ending.stop_codex {
        stop_codex_runners(st)?;
    }
    let log = Log::read(&run.paths)?;
    let cards = log.cards(&run.numbers());
    let run_ref = run.run_ref().to_os()?;
    let git = if run.ws.is_dir() {
        Some(run.git()?)
    } else {
        None
    };
    let run_tip = git.as_ref().and_then(|g| ref_sha(g, &run_ref));
    let mut unreconciled = ending
        .unreconciled
        .clone()
        .filter(|u| !u.is_empty())
        .map(PyStr::from);
    // With no run branch at all, every accepted card's work is missing from it.
    let mut missing = Vec::new();
    for e in &log.events {
        let Event::Accepted(a) = e else { continue };
        let Some(sha) = a.sha.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let on_branch = match (&git, &run_tip) {
            (Some(g), Some(_)) => is_ancestor(g, os(sha), &run_ref)?,
            _ => false,
        };
        if !on_branch {
            let card = a.card.as_ref().map_or_else(|| "None".into(), |c| c.text());
            let mut item = PyStr::from("card ");
            item.push(&card);
            item.push_str(&format!(" at {}", short_sha(sha)));
            missing.push(item);
        }
    }
    if !missing.is_empty() && unreconciled.is_none() {
        let mut why = PyStr::from("accepted work missing from the run branch: ");
        why.push(&PyStr::join(&missing, ", "));
        unreconciled = Some(why);
    }
    if run_tip.is_some() && unreconciled.is_none() {
        // Only acceptance moves the run branch: anything else on it is unaccepted work (I4).
        unreconciled = head_problem(run, &log, run_tip.as_deref()).map(PyStr::from);
    }
    let mut blocked = ending.blocked;
    let mut why = ending.why.clone();
    if blocked.is_none()
        && !ending.cancelled
        && unreconciled.is_none()
        && !ending.verification_failed
    {
        let waiting = waiting_on_decisions(run, &cards, &log)?;
        if !waiting.is_empty() {
            // R44: everything left needs the operator's decision: the run can continue once
            // they unpark the cards, so it ends blocked (decision needed), with the handoff.
            blocked = Some(Blocked::DecisionNeeded);
            let mut all = waiting;
            all.extend(why);
            if ending.open_findings {
                all.push("unresolved review findings".into());
            }
            for hold in &ending.holds {
                let mut h = PyStr::from("hold: ");
                h.push(hold);
                all.push(h);
            }
            why = all;
        }
    }
    let decision = decide_outcome(&OutcomeInput {
        cards: &cards,
        removed: Default::default(),
        open_findings: ending.open_findings,
        holds: ending.holds.clone(),
        gates_current: ending.gates_current,
        verification_failed: ending.verification_failed,
        cancelled: ending.cancelled,
        blocked_reason: blocked,
        unreconciled,
    });
    let mut reasons = decision.reasons.clone();
    reasons.extend(why);
    append_event(
        &run.paths,
        Event::Outcome(events::Outcome {
            outcome: Some(decision.outcome.as_str().to_string()),
            qualifier: Some(decision.qualifier.clone()),
            reasons: Some(reasons),
            next_step: Some(allowed_next_step(&decision, &run.config.after_run)),
            head: run_tip,
            ..events::Outcome::default()
        }),
    )?;
    Ok(())
}

/// Before the run ends on a state that could not be matched: stops every Codex dispatch that
/// has no result (`dispatch.cancel`, termination confirmed), so no detached runner writes, or
/// commits, after the outcome (`_stop_codex_runners`). Their results are left unrecorded: the
/// state they would be recorded against could not be matched.
pub(super) fn stop_codex_runners(st: &RunState) -> Result<()> {
    for d in st.log.dispatches() {
        if d.result.is_none() && d.vendor() == "codex" {
            cancel_codex(st.run, &st.cards, &d)?;
        }
    }
    Ok(())
}

/// After an outcome (`_wrap_up`), once per outcome: the branch release (a terminal outcome),
/// the handoff (a blocked run; a resumed blocked run's earlier handoff no longer applies to
/// any other), `report.md`, the `runs.md` line, then `report_written`. Then the `done` action
/// (`_done_action`). Repeat-safe: after a crash part way, the release is finished first, then
/// the rest is written again.
pub fn wrap_up(run: &Run) -> Result<Action> {
    let log = Log::read(&run.paths)?;
    let ended = log
        .ended()
        .cloned()
        .ok_or_else(|| Error::msg("the run has no outcome"))?;
    let written = log
        .events
        .iter()
        .any(|e| matches!(e, Event::ReportWritten(r) if r.seq > ended.seq));
    if !written {
        release(run, &log, &ended)?;
        if ended.outcome.as_deref() == Some("blocked") {
            write_handoff(run, &ended, &log)?;
        } else {
            match std::fs::remove_file(&run.paths.handoff) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(Error::io(&run.paths.handoff, e));
                }
                _ => {}
            }
        }
        let path = crate::report::write_report(&run.root, &default_sessions_dir())?;
        record_run_line(run, &ended, &log)?;
        append_event(
            &run.paths,
            Event::ReportWritten(events::ReportWritten {
                report: Some(path_text(&path)),
                outcome: ended.outcome.clone(),
                outcome_seq: Some(i64::try_from(ended.seq).unwrap_or(i64::MAX)),
                ..events::ReportWritten::default()
            }),
        )?;
    }
    let log = Log::read(&run.paths)?;
    let cards = log.cards(&run.numbers());
    let report = run
        .paths
        .report
        .is_file()
        .then(|| path_text(&run.paths.report));
    Ok(Action::Done(DoneAction {
        outcome: ended.outcome.clone(),
        qualifier: ended.qualifier.clone().unwrap_or_default(),
        reasons: ended.reasons.clone().unwrap_or_default(),
        report,
        cards: card_summary(run, &cards)?,
        card_loop_only: false,
    }))
}

/// `handoff.md` for a blocked run (`_write_handoff`): where it stopped, why, and what to do.
fn write_handoff(run: &Run, ended: &Outcome, log: &Log) -> Result<()> {
    let cards = log.cards(&run.numbers());
    let accepted = cards
        .values()
        .filter(|c| c.state == CardStage::Accepted)
        .count();
    let parked: Vec<String> = cards
        .iter()
        .filter(|(_, c)| c.state == CardStage::Parked)
        .map(|(n, _)| n.to_string())
        .collect();
    let last = log.dispatches().max_by_key(|d| d.seq());
    let qualifier = ended.qualifier.clone().unwrap_or_default();
    let cli = PyStr::from_os(&shell_quote(crate::testing::self_path().as_os_str()));
    let root = PyStr::from_os(&shell_quote(run.root.as_os_str()));
    let head_text = match ended.head.clone().filter(|h| !h.is_empty()) {
        Some(h) => h,
        None => head(&run.git()?, &run.run_ref().to_os()?)?,
    };
    let join = |parts: &[&PyStr]| PyStr::concat(parts);
    let mut lines: Vec<PyStr> = vec![
        join(&[&"# Handoff: ".into(), &run.deck.title.as_str().into()]),
        PyStr::default(),
        PyStr::from(if qualifier.is_empty() {
            "Outcome: blocked".to_string()
        } else {
            format!("Outcome: blocked ({qualifier})")
        }),
        PyStr::default(),
        "## Where it stopped".into(),
        PyStr::default(),
        join(&[
            &"- Run branch: ".into(),
            &run.branch,
            &format!(" at {head_text}").into(),
        ]),
        join(&[
            &"- Workspace: ".into(),
            &path_text(&pure_path(run.ws.as_os_str())),
        ]),
        PyStr::from(format!(
            "- Cards: {accepted} of {} accepted{}",
            cards.len(),
            if parked.is_empty() {
                String::new()
            } else {
                format!("; parked: {}", parked.join(", "))
            }
        )),
        match &last {
            Some(d) => PyStr::from(format!(
                "- Last step: {} {} ({})",
                d.kind(),
                d.id(),
                d.whose()
            )),
            None => "- Last step: none".into(),
        },
    ];
    lines.extend([PyStr::default(), "## Why".into(), PyStr::default()]);
    let reasons = ended
        .reasons
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| vec!["no reason recorded".into()]);
    for r in &reasons {
        lines.push(join(&[&"- ".into(), r]));
    }
    lines.extend([
        PyStr::default(),
        "## What to do".into(),
        PyStr::default(),
        join(&[
            &"1. Read the report: ".into(),
            &path_text(&run.paths.report),
        ]),
        join(&[
            &"2. Settle what the reasons above name: a decision (record it in the decisions file \
               or `"
                .into(),
            &cli,
            &" unpark ".into(),
            &root,
            &" --card N --note '…'` with a note), capacity coming back, or the environment \
               fixed."
                .into(),
        ]),
        join(&[
            &"3. Resume the run with `".into(),
            &cli,
            &" run ".into(),
            &root,
            &"` (`".into(),
            &cli,
            &" launch-line ".into(),
            &root,
            &"` prints the exact command). Gates that passed on the same commit are not run \
               again."
                .into(),
        ]),
    ]);
    let mut text = PyStr::join(&lines, "\n");
    text.push_str("\n");
    write_file_atomic(&run.paths.handoff, text.encode_utf8()?.as_bytes())
}

/// `" ".join(str(text).split()).replace("|", "\\|")`: one table cell.
fn md_cell(text: &str) -> String {
    py_split(text)
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// The run's one line in `<docs_dir>/runs.md`, replaced in place when the run ends again (a
/// resumed blocked run), so a run never has two lines (`_record_run_line`). Runs sharing
/// `docs_dir` (other repositories' too) update it concurrently: the whole read, modify and
/// replace holds an exclusive `flock` on `<docs_dir>/runs.md.lock`, so no run's line is lost.
fn record_run_line(run: &Run, ended: &Outcome, log: &Log) -> Result<()> {
    let cards = log.cards(&run.numbers());
    let accepted = cards
        .values()
        .filter(|c| c.state == CardStage::Accepted)
        .count();
    let snap = crate::status::load(&run.root);
    let usd = crate::status::usage_summary(&snap, &default_sessions_dir(), Some(Meter::default()))
        .estimated_usd;
    let qualifier = ended.qualifier.clone().unwrap_or_default();
    let outcome = format!(
        "{}{}",
        ended.outcome.as_deref().unwrap_or("None"),
        if qualifier.is_empty() {
            String::new()
        } else {
            format!(" ({qualifier})")
        }
    );
    // The file is UTF-8, as the Python writes it: a run directory whose name is not fails here.
    let root = path_text(&run.root);
    let run_cell = format!("| {} |", md_cell(root.encode_utf8()?));
    let cost = usd.map_or_else(|| "unknown".to_string(), |u| format!("${}", py_fixed(u, 2)));
    let line = format!(
        "| {} | {} | {outcome} | {accepted}/{} cards | {cost} {run_cell}",
        super::start::today()?,
        md_cell(&run.deck.title),
        cards.len(),
    );
    let docs = docs_dir_for(&run.config, &run.repo);
    std::fs::create_dir_all(&docs).map_err(|e| Error::io(&docs, e))?;
    let path = docs.join("runs.md");
    let _lock = lock(&docs.join("runs.md.lock"))?;
    let old: Vec<String> = match std::fs::read(&path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes)
                .map_err(|e| Error::msg(format!("{}: not UTF-8: {e}", path.display())))?;
            py_splitlines(&text)
                .into_iter()
                .map(str::to_string)
                .collect()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => [
            "# Runs",
            "",
            "| Date | Deck | Outcome | Cards | Estimated cost | Run |",
            "| --- | --- | --- | --- | --- | --- |",
        ]
        .map(str::to_string)
        .to_vec(),
        Err(e) => return Err(Error::io(&path, e)),
    };
    let mut new: Vec<String> = Vec::new();
    let mut placed = false;
    for ln in old {
        if ln.trim_end_matches(py_isspace).ends_with(&run_cell) {
            if !placed {
                new.push(line.clone());
                placed = true;
            }
            continue;
        }
        new.push(ln);
    }
    if !placed {
        new.push(line);
    }
    let mut text = new.join("\n");
    text.push('\n');
    write_file_atomic(&path, text.as_bytes())
}

/// Opens `path` (created `0600`, never through a symlink) and takes an exclusive `flock` on
/// it, waiting; released when the file is dropped.
fn lock(path: &Path) -> Result<std::fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    loop {
        match flock(&file, FlockOperation::LockExclusive) {
            Ok(()) => return Ok(file),
            Err(Errno::INTR) => continue,
            Err(e) => return Err(Error::io(path, e.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_cell_is_written_as_the_python_writes_it() {
        let texts = [
            "Widgets | phase 1",
            "  tabs\tand\nnewlines  ",
            "caf\u{e9}\u{2028}line\u{85}x",
            "a\\|b",
            "",
        ];
        // `engine.json`: the reference's `_md_cell` of each text.
        let theirs: Vec<String> =
            crate::wire::tests::reference_for("engine.json", "md_cell", &texts);
        let ours: Vec<String> = texts.iter().map(|t| md_cell(t)).collect();
        assert_eq!(ours, theirs);
    }
}
