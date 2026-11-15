//! Recovery (`engine.py`): reconciling a relaunched run (`_reconcile`, `_unmatched`, `_settle`,
//! `_reconcile_dispatch`, `_fail_reconciliation`, `_synth_started`, `_charge_earlier_sessions`),
//! the operator's flags recorded (`_cancel_requested`, `_pause_requested`), and R42's commit of
//! the partial work a stopped or interrupted Claude worker left (`_commit_partial`).
//!
//! Reconciliation runs after a `resumed`, before anything new, and only records what really
//! happened, so a pending cancel then ends the run on true state. It is repeat-safe: a crash part
//! way leaves what was settled recorded, and the next call settles the rest. It follows the
//! Python's rules exactly, from Git and the run directory:
//!
//! - a dispatch the run waits on whose worker (or proxy) died with an earlier session: a live
//!   Codex runner is waited for; a finished runner's result is recorded; a hung, orphaned or dead
//!   runner is cancelled first (confirmed, after a `reconciled{stage: cancel}` intent). Otherwise
//!   the worker counts as interrupted: a report in the scratch folder is recorded as its result,
//!   else the result is marked interrupted, and the next step continues from the branch's commits
//!   or requests the same attempt again (`next::recovery`);
//! - a Codex dispatch the run no longer waits on is cancelled; a finished one's leftover
//!   processes are stopped (P14);
//! - an acceptance whose fast-forward may not have happened is completed;
//! - integration, a fix round and the gates resume from the recorded events and Git;
//! - anything else ends the run failed with the mismatch, after every Codex dispatch without a
//!   result is stopped (confirmed), so nothing writes after the outcome.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::Path;

use super::cards::{
    Dispatch, Log, Run, card_branch, card_state, current_session, final_branch, heads, is_card,
    session_env, short_sha,
};
use super::finish::{end, stop_codex_runners};
use super::next::{Decision, Ending, Progress, RunState, branch_progress};
use super::protocol::{IMPLEMENTER_KINDS, budget_seconds, format_time, now};
use super::record::{cancel_codex, store_result};
use super::shell::{charge_session, finish_accept};
use crate::deck::dependency_order;
use crate::dispatch::cancel::CANCEL_TIMEOUT;
use crate::dispatch::cmd::reply_lines;
use crate::dispatch::commit::TRAILER;
use crate::dispatch::{
    ExitReason, RunnerState, StoppedProc, load_result, read_json_object, runner_state, stop_strays,
    untracked_to_take,
};
use crate::error::{Error, Result};
use crate::git::cards::{current_branch, is_ancestor, operation, ref_sha};
use crate::git::workspace::head;
use crate::git::{GIT_TIMEOUT, stage_work, status_entries, unhide_index_entries};
use crate::state::events::{self, given};
use crate::state::{CardRef, CardStage, Event, append_event};
use crate::store::state_lock;
use crate::wire::PyStr;

/// Reconciles the run in `run_dir` after a relaunch: the summary `reconciled` event appended,
/// or `None` when no relaunch waits on reconciliation (the Python reconciles only from `next`,
/// which asks first; once it does, it always records a summary, "nothing was in flight"
/// included). An in-place run is refused, as `next` refuses it.
pub fn reconcile(run_dir: &Path) -> Result<Option<Event>> {
    let run = Run::load(run_dir)?;
    crate::launch::refuse_in_place(&run.meta)?;
    let st = RunState::new(&run, Log::read(&run.paths)?);
    if !st.log.needs_reconcile() {
        return Ok(None);
    }
    reconcile_run(&st).map(Some)
}

/// After a `resumed`, before anything new (`_reconcile`): settles every step a dead session
/// left in flight, then appends `reconciled{resumed_seq, found, headline}`, the event returned.
/// A state no rule matches ends the run failed (decided here, so a pending cancel cannot mask
/// it).
pub(super) fn reconcile_run(st: &RunState) -> Result<Event> {
    let run = st.run;
    let since = st.log.latest_resumed();
    charge_earlier_sessions(run)?;
    if let Some(problem) = unmatched(st)? {
        return fail_reconciliation(run, since, problem);
    }
    match settle(st, since)? {
        Settled::Recorded(event) => Ok(*event),
        Settled::Mismatch(problem) => fail_reconciliation(run, since, problem),
    }
}

/// After a relaunch: charges what earlier supervisor sessions used after their last charge
/// (they ended with the crash, so their transcripts are final) (`_charge_earlier_sessions`).
pub(super) fn charge_earlier_sessions(run: &Run) -> Result<()> {
    let current = session_env().unwrap_or_default();
    let _lock = state_lock(&run.root)?;
    let sessions: Vec<PyStr> = read_json_object(&run.paths.run_json)
        .ok()
        .flatten()
        .and_then(|meta| crate::dispatch::field(&meta, "supervisor_sessions"))
        .unwrap_or_default();
    for sid in sessions {
        if sid != current {
            charge_session(run, &sid)?;
        }
    }
    Ok(())
}

/// What a restart finds that matches no recovery rule (the run then fails), or `None`
/// (`_unmatched`).
fn unmatched(st: &RunState) -> Result<Option<String>> {
    let run = st.run;
    if !run.ws.is_dir() {
        return Ok(Some(format!(
            "the workspace {} is missing",
            run.ws.display()
        )));
    }
    let git = run.git()?;
    let run_ref = run.run_ref().to_os()?;
    let Some(run_tip) = ref_sha(&git, &run_ref) else {
        return Ok(Some(format!(
            "the run branch {} is missing",
            run.branch.shown()
        )));
    };
    let mut missing = Vec::new();
    for e in &st.log.events {
        let Event::Accepted(a) = e else { continue };
        let Some(sha) = a.sha.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        if !is_ancestor(&git, OsStr::new(sha), &run_ref)? {
            let card = a.card.as_ref().map_or_else(|| "None".into(), CardRef::text);
            missing.push(format!("card {} at {}", card.lossy(), short_sha(sha)));
        }
    }
    if !missing.is_empty() {
        return Ok(Some(format!(
            "accepted work missing from the run branch: {}",
            missing.join(", ")
        )));
    }
    for (n, c) in &st.cards {
        let open = c.state == CardStage::Cut || super::cards::active(c.state);
        if let Some(branch) = c.branch.as_ref().filter(|b| !b.is_empty())
            && open
            && ref_sha(&git, &heads(branch).to_os()?).is_none()
        {
            return Ok(Some(format!(
                "card {n}'s branch {} is missing",
                branch.shown()
            )));
        }
        let target = c.accept_target.as_deref().filter(|t| !t.is_empty());
        if let (CardStage::Accepting, Some(target)) = (c.state, target) {
            let completes = is_ancestor(&git, OsStr::new(target), &run_ref)?
                || is_ancestor(&git, &run_ref, OsStr::new(target))?;
            if !completes {
                return Ok(Some(format!(
                    "card {n}'s acceptance of {} cannot complete: the run branch is at {}, \
                     which neither contains it nor is behind it",
                    short_sha(target),
                    short_sha(&run_tip)
                )));
            }
        }
    }
    Ok(None)
}

/// How `settle` ended.
enum Settled {
    /// Every step in flight is settled, and the summary is recorded.
    Recorded(Box<Event>),
    /// A step matches no rule (`_Mismatch`): why.
    Mismatch(String),
}

/// `_reconcile`'s body once the run as a whole matches (`_settle`): settles each step in
/// flight, then records the summary.
fn settle(st: &RunState, since: u64) -> Result<Settled> {
    let run = st.run;
    let current: HashSet<String> = st
        .log
        .open_dispatches(&st.cards)
        .iter()
        .map(|d| d.id().to_string())
        .collect();
    let mut found: Vec<(Option<CardRef>, String)> = Vec::new();
    let mut dispatches: Vec<Dispatch> = st.log.dispatches().collect();
    dispatches.sort_by_key(Dispatch::seq);
    for d in &dispatches {
        if d.vendor() == "codex" && d.seq() < since && d.result.is_some() {
            stopped_strays(run, d)?;
        }
        if d.result.is_some() || !st.log.gone(d) {
            continue;
        }
        if current.contains(d.id()) {
            let log = Log::read(&run.paths)?;
            match reconcile_dispatch(run, &log, &st.cards, d)? {
                Ok(text) => found.push((d.req.card.clone(), text)),
                Err(mismatch) => return Ok(Settled::Mismatch(mismatch)),
            }
        } else if d.vendor() == "codex" && d.started.is_some() {
            // Nothing waits on it: it must not write any more.
            cancel_codex(run, &st.cards, d)?;
        }
    }
    let log = Log::read(&run.paths)?;
    let cards = log.cards(&run.numbers());
    for n in dependency_order(&run.deck)? {
        let Some(c) = cards.get(&n) else { continue };
        match c.state {
            CardStage::Accepting => {
                finish_accept(run, n, c.accept_target.clone())?;
                found.push((
                    Some(CardRef::Number(n.into())),
                    format!("card {n}'s acceptance was completed"),
                ));
            }
            CardStage::Integrating if !found.iter().any(|(m, _)| is_card(m.as_ref(), n)) => {
                found.push((
                    Some(CardRef::Number(n.into())),
                    format!("card {n}'s integration resumes from Git's state"),
                ));
            }
            CardStage::Fixing if c.dispatch_kind.as_deref() != Some("fix") => {
                found.push((
                    Some(CardRef::Number(n.into())),
                    format!("card {n}'s fix round resumes from its findings"),
                ));
            }
            _ => {}
        }
    }
    let headline = match found.first() {
        Some((Some(card), text)) => format!("Resumed at card {}; {text}", card.text().lossy()),
        Some((None, text)) => format!("Resumed at the end of the run; {text}"),
        None => "Resumed; nothing was in flight".to_string(),
    };
    let lines = found
        .into_iter()
        .map(|(_, text)| PyStr::from(text))
        .collect();
    let event = summary(since, lines, headline);
    let (written, _) = append_event(&run.paths, event)?;
    Ok(Settled::Recorded(Box::new(written)))
}

/// The summary `reconciled{resumed_seq, found, headline}`.
fn summary(since: u64, found: Vec<PyStr>, headline: String) -> Event {
    Event::Reconciled(events::Reconciled {
        resumed_seq: Some(Some(i64::try_from(since).unwrap_or(i64::MAX))),
        found: Some(Some(found)),
        headline: Some(Some(PyStr::from(headline))),
        ..events::Reconciled::default()
    })
}

/// Stops what a finished Codex dispatch's dead runner left running (`dispatch.stop_strays`, P14);
/// an error naming what outlived the stop.
fn stopped_strays(run: &Run, d: &Dispatch) -> Result<()> {
    let wd = run.root.join("workers").join(d.id());
    let left: Vec<StoppedProc> = stop_strays(&wd, CANCEL_TIMEOUT)?;
    if left.is_empty() {
        return Ok(());
    }
    let pids: Vec<String> = left.iter().map(|p| format!("pid {}", p.pid)).collect();
    Err(Error::msg(format!(
        "could not stop what Codex dispatch {} left running: {}",
        d.id(),
        pids.join(", ")
    )))
}

/// Settles one dispatch the run waits on whose worker (or proxy) died with an earlier session
/// (`_reconcile_dispatch`): what was found, or the mismatch no rule matches (`_Mismatch`).
fn reconcile_dispatch(
    run: &Run,
    log: &Log,
    cards: &std::collections::BTreeMap<u32, crate::state::CardState>,
    d: &Dispatch,
) -> Result<std::result::Result<String, String>> {
    let who = d.whose();
    let mut started: Option<events::WorkerStarted> = d.started.cloned();
    if d.vendor() == "codex" {
        let wd = run.root.join("workers").join(d.id());
        let ours = log.events.iter().any(|e| {
            matches!(e, Event::Reconciled(r)
                if given(&r.stage).map(String::as_str) == Some("cancel")
                    && given(&r.dispatch_id).map(String::as_str) == Some(d.id()))
        });
        let state = runner_state(&wd, None)?;
        if state == RunnerState::Running {
            if started.is_none() {
                synth_started(run, d)?;
            }
            return Ok(Ok(format!(
                "{who}'s Codex worker is still running; waiting for it"
            )));
        }
        if state == RunnerState::Finished && !ours {
            stopped_strays(run, d)?;
            let s = match started {
                Some(s) => s,
                None => synth_started(run, d)?,
            };
            let result =
                load_result(&wd)?.ok_or_else(|| Error::msg(format!("no result for {}", d.id())))?;
            let reply = PyStr::from(reply_lines(&result).join("\n"));
            let stopped = result.exit_reason == ExitReason::Cancelled;
            store_result(run, d, &worker(&s), &reply, stopped, false, !reconciled(&s))?;
            return Ok(Ok(format!(
                "{who}'s Codex worker had finished; its result was recorded"
            )));
        }
        if state != RunnerState::Finished {
            // Hung, orphaned (its processes outlived the runner, P14), dead or never started:
            // stopped (confirmed) before anything else; the intent makes a repeat treat the
            // cancelled result as an interruption.
            append_event(
                &run.paths,
                Event::Reconciled(events::Reconciled {
                    stage: Some(Some("cancel".to_string())),
                    dispatch_id: Some(Some(d.id().to_string())),
                    runner: Some(Some(state.as_str().to_string())),
                    ..events::Reconciled::default()
                }),
            )?;
            cancel_codex(run, cards, d)?;
        }
    }
    let s = match started.take() {
        Some(s) => s,
        None => synth_started(run, d)?,
    };
    store_result(
        run,
        d,
        &worker(&s),
        &PyStr::default(),
        false,
        true,
        !reconciled(&s),
    )?;
    let fresh = Log::read(&run.paths)?;
    let interrupted = fresh
        .dispatch(Some(d.id()))
        .and_then(|x| x.result)
        .and_then(|r| given(&r.interrupted).copied())
        .unwrap_or(false);
    if !interrupted {
        return Ok(Ok(format!("{who}'s report was found and recorded")));
    }
    if IMPLEMENTER_KINDS.contains(&d.kind()) || d.kind() == "final_fix" {
        let st = RunState::new(run, fresh);
        return Ok(match progress(&st, d)? {
            Progress::Commits => Ok(format!("{who}'s commits were found and recorded")),
            Progress::Nothing => Ok(format!(
                "{who}'s {} worker was interrupted before it changed anything; it runs again",
                d.kind()
            )),
            Progress::Mismatch(why) => Err(why),
        });
    }
    if let Some(gate) = given(&d.req.gate).filter(|g| !g.is_empty()) {
        return Ok(Ok(format!(
            "the {gate} gate was interrupted; it runs again"
        )));
    }
    Ok(Ok(format!(
        "{who}'s {} worker was interrupted; it runs again",
        d.kind()
    )))
}

/// `next::branch_progress`, with its look-ups made in Git.
fn progress(st: &RunState, d: &Dispatch) -> Result<Progress> {
    let git = st.run.git()?;
    let mut sha = |name: PyStr| -> std::result::Result<Option<String>, Box<Decision>> {
        Ok(ref_sha(&git, &name.to_os()?))
    };
    let mut contains = |start: String, tip: String| -> std::result::Result<bool, Box<Decision>> {
        Ok(is_ancestor(&git, OsStr::new(&start), OsStr::new(&tip))?)
    };
    branch_progress(st, d, &mut sha, &mut contains).map_err(|d| match *d {
        Decision::Fail(e) => e,
        other => Error::msg(format!("unexpected step while reconciling: {other:?}")),
    })
}

fn worker(s: &events::WorkerStarted) -> PyStr {
    s.worker_id.clone().unwrap_or_default()
}

fn reconciled(s: &events::WorkerStarted) -> bool {
    given(&s.reconciled).copied().unwrap_or(false)
}

/// Records a start for a dispatch whose start was never recorded, so its result (or its
/// still-running Codex runner) can be recorded against it (`_synth_started`).
fn synth_started(run: &Run, d: &Dispatch) -> Result<events::WorkerStarted> {
    let deadline = format_time(now() + budget_seconds(d.req, crate::testing::minute_seconds()));
    let (written, _) = append_event(
        &run.paths,
        Event::WorkerStarted(events::WorkerStarted {
            dispatch_id: Some(d.id().to_string()),
            worker_id: Some(PyStr::from(format!("{}:unrecorded", d.id()))),
            card: d.req.card.clone(),
            deadline: Some(deadline),
            session: current_session(run),
            reconciled: Some(Some(true)),
            ..events::WorkerStarted::default()
        }),
    )?;
    match written {
        Event::WorkerStarted(s) => Ok(s),
        _ => unreachable!("append_event returns the event it appended"),
    }
}

/// Ends the run failed with the mismatch (`_fail_reconciliation`): every Codex dispatch without
/// a result is stopped (confirmed) first, then `reconciled` records it, then the outcome
/// (`failed` outranks any cancel). The `reconciled` event.
fn fail_reconciliation(run: &Run, since: u64, problem: String) -> Result<Event> {
    stop_codex_runners(&RunState::new(run, Log::read(&run.paths)?))?;
    let found = vec![PyStr::from(format!(
        "state could not be reconciled: {problem}"
    ))];
    let headline = format!("Resumed; state could not be reconciled: {problem}");
    let (written, _) = append_event(&run.paths, summary(since, found, headline))?;
    let st = RunState::new(run, Log::read(&run.paths)?);
    end(
        &st,
        &Ending {
            unreconciled: Some(problem),
            ..Ending::default()
        },
    )?;
    Ok(written)
}

// --------------------------------------------------------------------------
// The operator's flags

/// One of the operator's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    Cancel,
    Pause,
}

/// The operator's flag, seen by `next` (`_cancel_requested`, `_pause_requested`): `cancelled`
/// (or `paused`) is recorded unless it is (a pause: since the latest relaunch), then the flag
/// is removed. A cancel is never undone; a relaunch ends a pause.
pub(super) fn record_flag(st: &RunState, flag: Flag) -> Result<()> {
    let run = st.run;
    let (path, recorded) = match flag {
        Flag::Cancel => (&run.paths.cancel_flag, st.log.cancelled()),
        Flag::Pause => (&run.paths.pause_flag, st.log.paused()),
    };
    if !recorded && path.exists() {
        let by = Some("operator".to_string());
        let event = match flag {
            Flag::Cancel => Event::Cancelled(events::Cancelled {
                by,
                ..events::Cancelled::default()
            }),
            Flag::Pause => Event::Paused(events::Paused {
                by,
                ..events::Paused::default()
            }),
        };
        append_event(&run.paths, event)?;
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(Error::io(path, e)),
        _ => Ok(()),
    }
}

// --------------------------------------------------------------------------
// R42: a stopped or interrupted Claude worker's partial work

/// What `commit_partial` did, for the result event: the commit it made, or Git's error, and
/// the gitlinks it left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partial {
    pub commit: Option<String>,
    pub error: Option<PyStr>,
    pub gitlinks: Vec<PyStr>,
}

/// R42 (`_commit_partial`): commits what a stopped or interrupted Claude worker left
/// uncommitted, when the workspace is on its branch and no Git operation is in progress: its
/// tracked changes (an edit hidden behind an index flag included: the flags are cleared
/// first) and the untracked files Git does not ignore that were not there when it was
/// dispatched (I3), unsigned and without hooks, with the `Leadhand-Dispatch` trailer, after a
/// `reconciled{stage: partial_work}` intent. Repeat-safe: after a crash past the commit the
/// workspace is clean and nothing more is committed. A Git failure is reported in `error`;
/// only the intent's own failure is an error.
pub(crate) fn commit_partial(run: &Run, d: &Dispatch) -> Result<Partial> {
    let branch = match &d.req.card {
        None => final_branch(run),
        Some(card) => {
            let log = Log::read(&run.paths)?;
            let cards = log.cards(&run.numbers());
            card_branch(run, card_state(&cards, Some(card))?)
        }
    };
    let mut intent_result: Result<()> = Ok(());
    let outcome = partial_steps(run, d, &branch, &mut |before| {
        intent_result = record_partial_intent(run, d, &branch, before);
        intent_result.clone()
    });
    intent_result?;
    Ok(outcome.unwrap_or_else(|e| Partial {
        error: Some(PyStr::from(e.message)),
        ..Partial::default()
    }))
}

/// `_commit_partial`'s Git steps; `intent` records the intent before anything is staged.
fn partial_steps(
    run: &Run,
    d: &Dispatch,
    branch: &PyStr,
    intent: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<Partial> {
    let git = run.git()?;
    if current_branch(&git).as_ref() != Some(branch) || operation(&git)?.is_some() {
        return Ok(Partial::default());
    }
    // An edit a worker hid behind an index flag is its work too.
    unhide_index_entries(&git)?;
    let (tracked, loose) = status_entries(&git)?;
    let earlier: Vec<PyStr> = given(&d.req.untracked).cloned().unwrap_or_default();
    let new: Vec<PyStr> = untracked_to_take(&run.root, &run.ws, &loose, GIT_TIMEOUT)?
        .into_iter()
        .filter(|p| !earlier.contains(p))
        .collect();
    if !tracked && new.is_empty() {
        return Ok(Partial::default());
    }
    let branch_ref = heads(branch).to_os()?;
    let before = head(&git, &branch_ref)?;
    intent(&before)?;
    // Tracked changes and the files that appeared during the dispatch; never another worker's
    // untracked leftovers (I3).
    let (staged, gitlinks) = stage_work(&run.ws, &new, None)?;
    if !staged {
        return Ok(Partial {
            gitlinks,
            ..Partial::default()
        });
    }
    let subject = format!("{}: partial work (stopped)", d.id());
    let trailer = format!("{TRAILER}: {}", d.id());
    let args = [
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "--quiet",
        "--no-verify",
        "-m",
        &subject,
        "-m",
        &trailer,
    ]
    .map(OsStr::new);
    git.run(&args)?;
    Ok(Partial {
        commit: Some(head(&git, &branch_ref)?),
        gitlinks,
        error: None,
    })
}

/// `reconciled{stage: partial_work, dispatch_id, branch, head_before}`, once per dispatch.
fn record_partial_intent(run: &Run, d: &Dispatch, branch: &PyStr, before: &str) -> Result<()> {
    let log = Log::read(&run.paths)?;
    let recorded = log.events.iter().any(|e| {
        matches!(e, Event::Reconciled(r)
            if given(&r.stage).map(String::as_str) == Some("partial_work")
                && given(&r.dispatch_id).map(String::as_str) == Some(d.id()))
    });
    if !recorded {
        append_event(
            &run.paths,
            Event::Reconciled(events::Reconciled {
                stage: Some(Some("partial_work".to_string())),
                dispatch_id: Some(Some(d.id().to_string())),
                branch: Some(Some(branch.clone())),
                head_before: Some(Some(before.to_string())),
                ..events::Reconciled::default()
            }),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::git::fixture::Fixture;

    const DECK: &str = "# Deck: Demo deck\nSource: docs/plan.md\nBase: main @ BASE\n\
        Branch: leadhand/demo\nBuild: true\nTest: true\n\n\
        ## Card 1: Card number 1\n- Role: implementer\n- Hard: no\n- Risk: low\n\
        - Depends on: none\n- Allowed paths: src/, tests/\n\
        - Done when: the card's file exists and the tests pass\n- Stop if: the design has to change\n";
    const ID: &str = "c1-a1-implement-1";

    /// A run whose card 1 is cut and checked out in the run's worktree, with its implementer
    /// started; `before.txt` was untracked there when it was requested. The run folder and the
    /// workspace.
    fn cut_run(f: &Fixture) -> (PathBuf, PathBuf, String) {
        let repo = f.make_repo("repo");
        let ws = f.root().join("work space");
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "leadhand/demo",
                ws.to_str().unwrap(),
            ],
        );
        f.git(&ws, &["checkout", "-q", "-b", "leadhand/demo--card-1"]);
        let base = f.git(&repo, &["rev-parse", "HEAD"]);
        let root = f.root().join("docs dir/runs/2026-11-14-demo-deck");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("deck.md"), DECK.replace("BASE", &base)).unwrap();
        let user = f.root().join("user.toml");
        std::fs::write(&user, "").unwrap();
        let config = crate::config::load_config_from(&repo, &user).unwrap();
        crate::store::write_json_atomic(
            &root.join(crate::config::SNAPSHOT),
            &crate::config::ConfigSnapshot(&config),
        )
        .unwrap();
        let meta = serde_json::json!({
            "run_id": "abc123abc123", "repo": repo, "deck_source": root.join("deck.md"),
            "deck_digest": "0", "base_ref": "main", "base_sha": base, "branch": "leadhand/demo",
            "branch_requested": "leadhand/demo", "workspace": ws, "slug": "demo-deck",
            "created_at": "2026-11-14T00:00:00.000Z", "supervisor_sessions": [],
        });
        std::fs::write(root.join("run.json"), meta.to_string()).unwrap();
        let events = format!(
            r#"{{"seq":1,"ts":"t","kind":"card_cut","card":1,"branch":"leadhand/demo--card-1","base":"{base}"}}
{{"seq":2,"ts":"t","kind":"dispatch_requested","dispatch_id":"{ID}","card":1,"attempt":1,"dispatch_kind":"implement","role":"implementer","vendor":"claude","model":"sonnet","effort":"medium","brief":"/b","report":"/r","extra":{{}},"notes":"","resumes":null,"resume_worker":null,"resume_session":null,"minutes":60,"start_tip":"{base}","untracked":["before.txt"]}}
{{"seq":3,"ts":"t","kind":"worker_started","dispatch_id":"{ID}","worker_id":"agent0001","card":1,"deadline":"2099-01-01T00:00:00Z","session":null}}
"#
        );
        std::fs::write(crate::state::run_paths(&root).events, events).unwrap();
        (root, ws, base)
    }

    fn intents(run: &Run) -> Vec<events::Reconciled> {
        Log::read(&run.paths)
            .unwrap()
            .events
            .into_iter()
            .filter_map(|e| match e {
                Event::Reconciled(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    // R42: what a stopped worker left is committed on its branch, an edit hidden behind an index
    // flag included, never a file that was untracked when it was dispatched (I3); once.
    #[test]
    fn a_partial_commit_takes_the_workers_changes_and_only_those() {
        let f = Fixture::new();
        let (root, ws, base) = cut_run(&f);
        f.write(&ws, &[("before.txt", "the operator's\n")]);
        f.write(&ws, &[("README.md", "hello, edited\n")]);
        f.git(&ws, &["update-index", "--skip-worktree", "README.md"]);
        f.write(&ws, &[("src/new.txt", "the worker's\n")]);
        let run = Run::load(&root).unwrap();
        let log = Log::read(&run.paths).unwrap();
        let d = log.dispatch(Some(ID)).unwrap();
        let partial = commit_partial(&run, &d).unwrap();
        let tip = f.git(&ws, &["rev-parse", "refs/heads/leadhand/demo--card-1"]);
        assert_eq!(
            partial,
            Partial {
                commit: Some(tip.clone()),
                ..Partial::default()
            }
        );
        assert_ne!(tip, base);
        assert_eq!(
            f.git(&ws, &["log", "-1", "--format=%B", &tip]),
            format!("{ID}: partial work (stopped)\n\nLeadhand-Dispatch: {ID}")
        );
        assert_eq!(
            f.git(
                &ws,
                &["show", "--name-only", "--format=", "--no-renames", &tip]
            ),
            "README.md\nsrc/new.txt"
        );
        assert_eq!(
            f.git(&ws, &["status", "--porcelain", "--untracked-files=all"]),
            "?? before.txt"
        );
        let found = intents(&run);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (
                given(&found[0].stage).map(String::as_str),
                given(&found[0].dispatch_id).map(String::as_str),
                given(&found[0].branch).map(PyStr::lossy),
                given(&found[0].head_before).map(String::as_str),
            ),
            (
                Some("partial_work"),
                Some(ID),
                Some("leadhand/demo--card-1".into()),
                Some(base.as_str())
            )
        );
        // Repeat-safe: nothing left to commit, and no second intent.
        let log = Log::read(&run.paths).unwrap();
        let d = log.dispatch(Some(ID)).unwrap();
        assert_eq!(commit_partial(&run, &d).unwrap(), Partial::default());
        assert_eq!(intents(&run).len(), 1);
        // Off its branch, nothing is committed.
        f.write(&ws, &[("src/new.txt", "more\n")]);
        f.git(&ws, &["checkout", "-q", "--detach"]);
        assert_eq!(commit_partial(&run, &d).unwrap(), Partial::default());
        assert_eq!(intents(&run).len(), 1);
    }

    // `_cancel_requested`, `_pause_requested`: the flag is recorded once and removed.
    #[test]
    fn a_flag_is_recorded_once_and_removed() {
        let f = Fixture::new();
        let (root, _, _) = cut_run(&f);
        let run = Run::load(&root).unwrap();
        let kinds = |run: &Run| -> Vec<&'static str> {
            Log::read(&run.paths)
                .unwrap()
                .events
                .iter()
                .map(Event::kind)
                .filter(|k| matches!(*k, "cancelled" | "paused"))
                .collect()
        };
        for _ in 0..2 {
            std::fs::write(&run.paths.cancel_flag, "{}").unwrap();
            let st = RunState::new(&run, Log::read(&run.paths).unwrap());
            record_flag(&st, Flag::Cancel).unwrap();
            assert!(!run.paths.cancel_flag.exists());
        }
        assert_eq!(kinds(&run), ["cancelled"]);
        std::fs::write(&run.paths.pause_flag, "{}").unwrap();
        let st = RunState::new(&run, Log::read(&run.paths).unwrap());
        record_flag(&st, Flag::Pause).unwrap();
        assert!(!run.paths.pause_flag.exists());
        assert_eq!(kinds(&run), ["cancelled", "paused"]);
    }
}
