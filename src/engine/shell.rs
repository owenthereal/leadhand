//! `next`'s shell (`engine.next_action`): the I/O around `next::decide`. It loads the run,
//! charges the supervisor's session, then loops step after step, as the Python does: read the
//! log, observe what `decide` needs (`gather`), carry out the decision (`execute`), until a
//! step yields the supervisor's action. The card steps it carries out are here too: cutting a
//! card branch, requesting a dispatch (its guard, its brief, `dispatch_requested`), accepting
//! a card, writing a test-only fix's diff, recording a Codex runner's result, and the guard
//! check that tells whether a worker left what it guards as it found it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

use serde::Serialize;
use serde_json::value::RawValue;

use super::cards::{Dispatch, Log, Run, by, current_session, is_card, session_env};
use super::next::{Decision, DiffCheck, Need, Observations, Request, RunState, decide};
use super::protocol::{
    Action, CHECK_KINDS, CheckDiffAction, IMPLEMENTER_KINDS, MAX_STEPS, REPLY_FILE, now, tag,
};
use super::record::{cancel_codex, store_result};
use super::report_text::read_text;
use crate::briefs_write::{BriefSpec, remove_scratch_file, write_brief};
use crate::config::limits_key;
use crate::dispatch::{DispatchResult, ExitReason, load_result};
use crate::error::{Error, Result};
use crate::git::cards::{
    branch_ref, card_branch_name, changed_paths, current_branch, cut_card_branch, detect_violation,
    diff_text, fast_forward, is_ancestor, operation, ref_sha, ref_snapshot,
};
use crate::git::workspace::head;
use crate::git::{GIT_TIMEOUT, untracked_files};
use crate::pytext::path_text;
use crate::state::events::{self, Opt, given};
use crate::state::{CardRef, CardStage, Event, append_event, two_phase, two_phase_resume};
use crate::store::{state_lock, write_file_atomic, write_json_atomic};
use crate::usage::{
    ChargeSource, Source, charge, charged_checkpoint, claude_session_transcript, new_claude_usage,
    projects_dir, read_charges,
};
use crate::wire::{PyMap, PyStr};

/// `leadhand next RUN_DIR`: the supervisor's next action, as one line of ASCII JSON.
pub fn command(run_dir: &OsStr) -> Result<ExitCode> {
    let action = next_action(Path::new(run_dir))?;
    crate::cli::human::print_or_json("", &action, true)?;
    Ok(ExitCode::SUCCESS)
}

/// Performs every mechanical step that is due, then returns the supervisor's next action
/// (`engine.next_action`). `status.json` is rewritten afterwards, whatever happened.
pub fn next_action(run_dir: &Path) -> Result<Action> {
    let out = next_action_steps(run_dir);
    crate::status::checkpoint(run_dir);
    out
}

fn next_action_steps(run_dir: &Path) -> Result<Action> {
    let run = Run::load(run_dir)?;
    let deck_problem = run.deck_problem();
    charge_supervisor(&run)?;
    let session = current_session(&run);
    let installed =
        crate::testing::installed_vendors().unwrap_or_else(crate::route::installed_on_path);
    let base = Observations {
        minute_seconds: crate::testing::minute_seconds(),
        installed,
        session,
        deck_problem,
        card_loop_only: crate::testing::card_loop_only(),
        cli: crate::testing::self_path(),
        ..Observations::default()
    };
    for _ in 0..MAX_STEPS {
        let st = RunState::new(&run, Log::read(&run.paths)?);
        let mut obs = Observations {
            now: now(),
            run_tip: run_tip(&run),
            cancel_flag: run.paths.cancel_flag.exists(),
            pause_flag: run.paths.pause_flag.exists(),
            ..base.clone()
        };
        let mut asked = 0;
        let decision = loop {
            match decide(&st, &obs) {
                Decision::Need(need) if asked < 16 => {
                    asked += 1;
                    gather(&st, &mut obs, need)?;
                }
                Decision::Need(need) => {
                    return Err(Error::msg(format!(
                        "the engine asked for {need:?} again; the run state does not settle"
                    )));
                }
                d => break d,
            }
        };
        if let Some(action) = execute(&st, &obs, decision)? {
            return Ok(action);
        }
    }
    Err(Error::msg(format!(
        "no action after {MAX_STEPS} steps; the run state does not settle"
    )))
}

/// The run branch's commit, `None` when the workspace or the branch is missing.
fn run_tip(run: &Run) -> Option<String> {
    if !run.ws.is_dir() {
        return None;
    }
    let git = run.git().ok()?;
    ref_sha(&git, &run.run_ref().to_os().ok()?)
}

/// One observation `decide` asked for.
fn gather(st: &RunState, obs: &mut Observations, need: Need) -> Result<()> {
    let run = st.run;
    match need {
        Need::CodexResult(id) => {
            let result = load_result(&run.root.join("workers").join(&id))?;
            obs.codex_results.insert(id, result);
        }
        Need::Operation => {
            obs.operation = Some(operation(&run.git()?)?.map(str::to_string));
        }
        Need::Changed(a, b) => {
            let paths = changed_paths(&run.git()?, &a, &b)?;
            obs.changed.insert((a, b), paths);
        }
        Need::Text(path) => {
            let text = read_text(&path)?;
            obs.texts.insert(path, text);
        }
        Need::Guard(id) => {
            let d = st
                .log
                .dispatch(Some(&id))
                .ok_or_else(|| Error::msg(format!("no dispatch {id} was requested")))?;
            let moved = guard_moved(st, &d)?;
            obs.guards.insert(id, moved);
        }
    }
    Ok(())
}

/// Carries out one decision; the action to return, if it yields one.
fn execute(st: &RunState, obs: &Observations, decision: Decision) -> Result<Option<Action>> {
    let run = st.run;
    match decision {
        Decision::Act(action) => Ok(Some(action)),
        Decision::Cut(card) => cut_card(st, card).map(|()| None),
        Decision::Request(r) => request_dispatch(st, &r).map(|()| None),
        Decision::Append(event) => append_event(&run.paths, *event).map(|_| None),
        Decision::Accept { card, target } => accept(run, card, target).map(|()| None),
        Decision::FinishAccept { card, target } => finish_accept(run, card, target).map(|()| None),
        Decision::CheckDiff(check) => write_diff_check(st, &check).map(Some),
        Decision::StoreCodex { dispatch_id, then } => {
            let d = dispatch_of(st, &dispatch_id)?;
            let result = obs
                .codex_results
                .get(&dispatch_id)
                .cloned()
                .flatten()
                .ok_or_else(|| Error::msg(format!("no result for {dispatch_id}")))?;
            store_codex(st, &d, &result)?;
            Ok(then)
        }
        Decision::StopCodex { dispatch_id, then } => {
            let d = dispatch_of(st, &dispatch_id)?;
            let result = cancel_codex(run, &st.cards, &d)?;
            store_codex(st, &d, &result)?;
            Ok(then)
        }
        Decision::StoreInterrupted { dispatch_id } => {
            let d = dispatch_of(st, &dispatch_id)?;
            let worker = d
                .started
                .and_then(|s| s.worker_id.clone())
                .unwrap_or_default();
            store_result(run, &d, &worker, &PyStr::default(), false, true, false)?;
            Ok(None)
        }
        Decision::End(ending) => Err(Error::msg(format!(
            "not ported yet: ending the run{} (Task 16)",
            match (&ending.blocked, &ending.unreconciled) {
                (Some(b), _) => format!(" blocked ({})", b.as_str()),
                (None, Some(why)) => format!(" failed: {why}"),
                (None, None) => String::new(),
            }
        ))),
        Decision::Unported { what, task } => {
            Err(Error::msg(format!("not ported yet: {what} (Task {task})")))
        }
        Decision::Fail(e) => Err(e),
        Decision::Need(need) => Err(Error::msg(format!("unanswered: {need:?}"))),
    }
}

fn dispatch_of<'l>(st: &'l RunState, id: &str) -> Result<Dispatch<'l>> {
    st.log
        .dispatch(Some(id))
        .ok_or_else(|| Error::msg(format!("no dispatch {id} was requested")))
}

// --------------------------------------------------------------------------
// The card steps

/// Records a Codex runner's result (`_store_result` with the runner's reply lines).
fn store_codex(st: &RunState, d: &Dispatch, result: &DispatchResult) -> Result<()> {
    let started = d
        .started
        .ok_or_else(|| Error::msg(format!("dispatch {} has no started worker", d.id())))?;
    let worker = started.worker_id.clone().unwrap_or_default();
    let reply = PyStr::from(crate::dispatch::cmd::reply_lines(result).join("\n"));
    let reconciled = given(&started.reconciled).copied().unwrap_or(false);
    store_result(
        st.run,
        d,
        &worker,
        &reply,
        result.exit_reason == ExitReason::Cancelled,
        false,
        !reconciled,
    )?;
    Ok(())
}

/// Refuses to touch the workspace for `card` while another card's worker, or a run-level
/// worker, may be using it (`_no_open_dispatch`).
fn no_open_dispatch(st: &RunState, card: u32) -> Result<()> {
    for d in st.log.dispatches() {
        if d.result.is_some() {
            continue;
        }
        match &d.req.card {
            Some(CardRef::Number(other)) if *other != i64::from(card) => {
                let open = u32::try_from(*other)
                    .ok()
                    .and_then(|o| st.cards.get(&o))
                    .is_some_and(|c| {
                        !matches!(c.state, CardStage::Parked | CardStage::Accepted)
                            && c.dispatch_id.as_deref() == Some(d.id())
                    });
                if open {
                    return Err(Error::msg(format!(
                        "card {other} has dispatch {} open; card {card} must wait for it",
                        d.id()
                    )));
                }
            }
            None => {
                return Err(Error::msg(format!(
                    "the run's dispatch {} is open; card {card} must wait for it",
                    d.id()
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// R30's check before a check is dispatched and before the engine switches branches
/// (`_ensure_clean`): anything an implementer left in the workspace (a dirty tree or index,
/// HEAD off `branch`'s tip, an operation in progress, and with `untracked` the untracked files
/// it may sweep). Preserving and clearing what it finds is Task 15's, so finding anything
/// stops here.
fn ensure_clean(run: &Run, branch: &PyStr, untracked: bool) -> Result<()> {
    let git = run.git()?;
    let op = operation(&git)?;
    let tip = head(&git, &branch_ref(branch)?)?;
    let v = detect_violation(&git, &tip, branch)?;
    let loose = if untracked {
        let paths = untracked_files(&run.ws).map_err(Error::from)?;
        crate::dispatch::untracked_to_take(&run.root, &run.ws, &paths, GIT_TIMEOUT)?
    } else {
        Vec::new()
    };
    if op.is_none() && v.is_none() && loose.is_empty() {
        return Ok(());
    }
    Err(Error::msg(
        "not ported yet: preserving and clearing what an implementer left in the workspace \
         (Task 15)",
    ))
}

/// Puts the workspace on the card branch when HEAD is on another one (`_check_out`): never
/// while another card has a dispatch open, and only once the workspace is clean.
fn check_out(st: &RunState, card: u32, branch: &PyStr) -> Result<()> {
    let git = st.run.git()?;
    let current = current_branch(&git);
    if current.as_ref() == Some(branch) {
        return Ok(());
    }
    no_open_dispatch(st, card)?;
    ensure_clean(st.run, current.as_ref().unwrap_or(branch), true)?;
    if current_branch(&git).as_ref() != Some(branch) {
        git.run(&[
            OsStr::new("checkout"),
            OsStr::new("--quiet"),
            &branch.to_os()?,
            OsStr::new("--"),
        ])?;
    }
    Ok(())
}

/// Cuts a pending card's branch from the run tip (`_step`'s pending branch).
fn cut_card(st: &RunState, n: u32) -> Result<()> {
    let run = st.run;
    no_open_dispatch(st, n)?;
    let git = run.git()?;
    let current = current_branch(&git);
    ensure_clean(run, current.as_ref().unwrap_or(&run.branch), true)?;
    let name = card_branch_name(&run.branch, &n.to_string());
    let recorded = st
        .log
        .events
        .iter()
        .any(|e| matches!(e, Event::CardCut(c) if c.branch.as_ref() == Some(&name)));
    let (branch, base) = cut_card_branch(&git, &run.branch, &n.to_string(), recorded)?;
    append_event(
        &run.paths,
        Event::CardCut(events::CardCut {
            card: Some(CardRef::Number(n.into())),
            branch: Some(branch),
            base: Some(base),
            ..events::CardCut::default()
        }),
    )?;
    Ok(())
}

/// The guard a dispatch records: what its worker must leave as it found it.
#[derive(Default)]
struct Guard {
    expected_branch: Opt<PyStr>,
    expected_head: Opt<String>,
    refs: Opt<PyMap<Option<String>>>,
    ref_scope: Opt<Vec<PyStr>>,
    start_tip: Opt<String>,
    untracked: Opt<Vec<PyStr>>,
    continues: Opt<String>,
}

/// Writes the brief and appends `dispatch_requested` (`_request`, `_append_request`).
/// Deterministic: after a crash before the event, the next call computes the same dispatch ID
/// and rewrites the same brief.
fn request_dispatch(st: &RunState, r: &Request) -> Result<()> {
    let run = st.run;
    let c = &st.cards[&r.card];
    let mut full = PyMap(vec![
        (PyStr::from("base"), Some(PyStr::from(r.base.as_str()))),
        (
            PyStr::from("branch"),
            Some(c.branch.clone().unwrap_or_default()),
        ),
        (
            PyStr::from("test_first"),
            Some(PyStr::from(run.config.run.test_first.as_str())),
        ),
    ]);
    for (k, v) in &r.extra.0 {
        *full.entry(k, || None) = v.clone();
    }
    let mut guard = Guard::default();
    if let Some(branch) = c.branch.as_ref().filter(|b| !b.is_empty()) {
        check_out(st, r.card, branch)?;
        let git = run.git()?;
        let card_ref = branch_ref(branch)?;
        if CHECK_KINDS.contains(&r.kind.as_str()) {
            // Implementer leftovers are preserved and cleared first (R30), so they are not
            // blamed on the check.
            ensure_clean(run, branch, false)?;
            // What the worker must leave as it found it (R14'): the card tip on the card
            // branch, and every ref of the run.
            let expected_head = head(&git, &card_ref)?;
            if r.kind == "verify" {
                let tip = r.extra.get(&PyStr::from("tip")).cloned().flatten();
                if tip.as_ref() != Some(&PyStr::from(expected_head.as_str())) {
                    return Err(Error::msg(format!(
                        "card {}: verify of {} requested, but the card branch is at {expected_head}",
                        r.card,
                        tip.map_or_else(|| "None".to_string(), |t| t.shown())
                    )));
                }
            }
            let snap = ref_snapshot(&git, &run.branch, run.run_id.as_deref().unwrap_or_default())?;
            guard.expected_branch = Some(Some(branch.clone()));
            guard.expected_head = Some(Some(expected_head));
            guard.refs = Some(Some(PyMap(
                snap.0.into_iter().map(|(k, v)| (k, Some(v))).collect(),
            )));
        } else if IMPLEMENTER_KINDS.contains(&r.kind.as_str()) {
            // An implementer-side worker commits on the card branch, but must leave the run
            // branch alone: moving it is a violation (I4).
            let run_ref = run.run_ref();
            let run_tip = head(&git, &run_ref.to_os()?)?;
            guard.refs = Some(Some(PyMap(vec![(run_ref.clone(), Some(run_tip))])));
            guard.ref_scope = Some(Some(vec![run_ref]));
        }
        if IMPLEMENTER_KINDS.contains(&r.kind.as_str()) {
            guard.start_tip = Some(Some(head(&git, &card_ref)?));
            // What is untracked already is not this dispatch's (I3).
            guard.untracked = Some(Some(untracked_files(&run.ws).map_err(Error::from)?));
        }
    }
    if let Some(continues) = &r.continues {
        guard.continues = Some(Some(continues.clone()));
    }
    let card = run.card(r.card)?;
    let (brief, report) = write_brief(&BriefSpec {
        kind: &r.kind,
        ws: &run.ws,
        deck: &run.deck,
        card: Some(card),
        attempt: r.attempt,
        dispatch_id: &r.dispatch_id,
        extra: &full,
        notes: &r.notes,
    })?;
    append_request(st, r, &brief, &report, guard)
}

/// Appends `dispatch_requested` for a brief already written (`_append_request`).
fn append_request(
    st: &RunState,
    r: &Request,
    brief: &Path,
    report: &Path,
    guard: Guard,
) -> Result<()> {
    let run = st.run;
    // No worker can have written this dispatch's report yet, nor the supervisor its reply
    // file: one already there is left over from an earlier run that used the same workspace.
    let report_name = report
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for name in [report_name.as_str(), REPLY_FILE] {
        remove_scratch_file(&run.ws, Some(r.card.into()), &r.dispatch_id, name)?;
    }
    let resume = r.resume.as_deref().and_then(|id| st.log.dispatch(Some(id)));
    let (mut resume_worker, mut resume_session) = (None, None);
    if let Some(resume) = &resume {
        if r.route.vendor == "claude" {
            // A Claude worker dies with its supervisor session (P9): one started before the
            // latest relaunch cannot be resumed, so a fresh worker gets the brief instead.
            resume_worker = resume
                .started
                .filter(|_| !st.log.gone(resume))
                .and_then(|s| s.worker_id.clone());
        } else {
            resume_session = resume.result.and_then(|res| res.session_id.clone());
        }
    }
    let key = limits_key(&r.role);
    let Some(limits) = run.config.limits_for(key) else {
        return Err(Error::msg(format!(
            "limits.{key}: not configured (role {})",
            r.role
        )));
    };
    append_event(
        &run.paths,
        Event::DispatchRequested(events::DispatchRequested {
            dispatch_id: Some(r.dispatch_id.clone()),
            card: Some(CardRef::Number(r.card.into())),
            attempt: Some(r.attempt),
            dispatch_kind: Some(r.kind.clone()),
            role: Some(r.role.clone()),
            vendor: Some(r.route.vendor.clone()),
            model: Some(r.route.model.clone()),
            effort: Some(r.route.effort.clone()),
            brief: Some(path_text(brief)),
            report: Some(path_text(report)),
            extra: Some(r.extra.clone()),
            notes: Some(r.notes.clone()),
            resumes: resume.map(|d| d.id().to_string()),
            resume_worker,
            resume_session,
            minutes: Some(limits.minutes),
            expected_branch: guard.expected_branch,
            expected_head: guard.expected_head,
            refs: guard.refs,
            ref_scope: guard.ref_scope,
            start_tip: guard.start_tip,
            untracked: guard.untracked,
            continues: guard.continues,
            ..events::DispatchRequested::default()
        }),
    )?;
    Ok(())
}

/// Accepts a card: fast-forwards the run branch to its tip, as intent, move and completion
/// (`_accept`). A card branch that does not contain the run tip is integrated first, and an
/// integrated card is accepted only at a tip a verify passed on: Task 15's.
fn accept(run: &Run, card: u32, target: Option<String>) -> Result<()> {
    let Some(target) = target.filter(|t| !t.is_empty()) else {
        return Err(Error::msg(format!("card {card}: no tip to accept")));
    };
    let log = Log::read(&run.paths)?;
    let cards = log.cards(&run.numbers());
    let c = &cards[&card];
    let branch = c
        .branch
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| card_branch_name(&run.branch, &card.to_string()));
    let git = run.git()?;
    if !is_ancestor(&git, &run.run_ref().to_os()?, &branch_ref(&branch)?)? {
        return Err(Error::msg(
            "not ported yet: integrating a card with the run branch (Task 15)",
        ));
    }
    let integrated = log
        .events
        .iter()
        .any(|e| matches!(e, Event::IntegrationStarted(i) if is_card(i.card.as_ref(), card)));
    if integrated {
        return Err(Error::msg(
            "not ported yet: accepting an integrated card (Task 15)",
        ));
    }
    let intent = Event::AcceptStarted(events::AcceptStarted {
        card: Some(CardRef::Number(card.into())),
        target: Some(target.clone()),
        ..events::AcceptStarted::default()
    });
    two_phase(
        &run.paths,
        intent,
        "before_fast_forward",
        || fast_forward(&git, &run.branch, &target),
        "after_fast_forward",
        |_| accepted(card, &target),
    )?;
    Ok(())
}

fn accepted(card: u32, target: &str) -> Event {
    Event::Accepted(events::Accepted {
        card: Some(CardRef::Number(card.into())),
        sha: Some(target.to_string()),
        ..events::Accepted::default()
    })
}

/// Completes an acceptance whose intent is recorded (`_finish_accept`). Idempotent: a run
/// branch that already contains the target is left where it is.
fn finish_accept(run: &Run, card: u32, target: Option<String>) -> Result<()> {
    let target = target.unwrap_or_else(|| "None".to_string());
    let git = run.git()?;
    two_phase_resume(
        &run.paths,
        || fast_forward(&git, &run.branch, &target),
        "after_fast_forward",
        |_| accepted(card, &target),
    )?;
    Ok(())
}

/// Writes a test-only fix's diff (`fix.diff` in the card folder) and returns `check_diff`
/// (`_after_fix`'s end).
fn write_diff_check(st: &RunState, check: &DiffCheck) -> Result<Action> {
    let run = st.run;
    let d = dispatch_of(st, &check.dispatch_id)?;
    let diff = run.card_dir(d.req.card.as_ref(), d.id()).join("fix.diff");
    let folder = diff.parent().expect("a card folder has a parent");
    std::fs::create_dir_all(folder).map_err(|e| Error::io(folder, e))?;
    let mut data = Vec::new();
    if let (false, Some(pre), Some(tip)) = (check.changed.is_empty(), &check.pre, &check.tip) {
        data = diff_text(&run.git()?, pre, tip)?;
    }
    if !data.is_empty() {
        data.push(b'\n');
    }
    write_file_atomic(&diff, &data)?;
    Ok(Action::CheckDiff(CheckDiffAction {
        action: tag::CheckDiff,
        card: check.card,
        dispatch_id: check.dispatch_id.clone(),
        diff: path_text(&diff),
        findings: check.findings.clone(),
        paths: check.changed.clone(),
    }))
}

/// Whether the dispatch's worker changed what the dispatch guards (`_find_violation`'s
/// comparison; recording, preserving and undoing it is Task 15's): for a check the card tip
/// and a clean workspace, and for every guarded dispatch the refs it recorded. Moves the
/// engine recorded after the dispatch are its own, not the worker's.
fn guard_moved(st: &RunState, d: &Dispatch) -> Result<bool> {
    let run = st.run;
    let req = d.req;
    let git = run.git()?;
    let changed_workspace = match given(&req.expected_head) {
        Some(expected) => {
            let branch = given(&req.expected_branch).cloned().unwrap_or_default();
            detect_violation(&git, expected, &branch)?.is_some()
        }
        None => false,
    };
    let before = given(&req.refs).cloned().unwrap_or_default();
    let now = ref_snapshot(&git, &run.branch, run.run_id.as_deref().unwrap_or_default())?;
    let (latest, owned) = engine_writes(run, &st.log, req.seq);
    let run_ref = run.run_ref();
    // An acceptance whose fast-forward happened but whose `accepted` is not recorded yet moved
    // the run branch to its target: the engine's move too.
    let targets: HashSet<Option<&str>> = st
        .log
        .events
        .iter()
        .filter_map(|e| match e {
            Event::AcceptStarted(a) if a.seq > req.seq => Some(a.target.as_deref()),
            _ => None,
        })
        .collect();
    let names: BTreeSet<PyStr> = match given(&req.ref_scope) {
        Some(scope) if !scope.is_empty() => scope.iter().cloned().collect(),
        _ => before
            .0
            .iter()
            .map(|(k, _)| k.clone())
            .chain(now.0.iter().map(|(k, _)| k.clone()))
            .collect(),
    };
    let moved = names
        .iter()
        .filter(|name| !owned.contains(*name))
        .any(|name| {
            let expected: Option<String> = match latest.get(name) {
                Some(value) => value.clone(),
                None => before.get(name).cloned().flatten(),
            };
            let current = now.get(name).map(String::as_str);
            if *name == run_ref && targets.contains(&current) {
                return false;
            }
            current != expected.as_deref()
        });
    Ok(changed_workspace || moved)
}

/// What the engine itself recorded moving after event `after` (`_engine_writes`): branch →
/// the value it last set, and the leadhand refs it wrote (owned outright).
fn engine_writes(
    run: &Run,
    log: &Log,
    after: u64,
) -> (HashMap<PyStr, Option<String>>, HashSet<PyStr>) {
    let mut latest: HashMap<PyStr, Option<String>> = HashMap::new();
    let mut owned: HashSet<PyStr> = HashSet::new();
    let heads = |branch: &PyStr| {
        let mut name = PyStr::from("refs/heads/");
        name.push(branch);
        name
    };
    for e in log.events.iter().filter(|e| e.seq() > after) {
        match e {
            Event::Accepted(a) => {
                latest.insert(run.run_ref(), a.sha.clone());
            }
            Event::CardCut(c) => {
                if let Some(branch) = c.branch.as_ref().filter(|b| !b.is_empty()) {
                    latest.insert(heads(branch), c.base.clone());
                }
            }
            Event::Rebased(r) => {
                if let Some(card) = &r.card {
                    let branch = card_branch_name(&run.branch, &card.text().lossy());
                    latest.insert(heads(&branch), r.tip.clone());
                }
            }
            Event::IntegrationStarted(i) => {
                if let Some(keep) = i.keep.as_ref().filter(|k| !k.is_empty()) {
                    owned.insert(keep.clone());
                }
            }
            Event::Violation(v) => {
                if let Some(r) = v.r#ref.as_ref().filter(|r| !r.is_empty()) {
                    for suffix in ["", "-branch", "-autostash"] {
                        let mut name = r.clone();
                        name.push_str(suffix);
                        owned.insert(name);
                    }
                }
                if let Some(preserved) = given(&v.preserved) {
                    owned.extend(preserved.values().cloned());
                }
                if v.stage.as_deref() == Some("found") && by(v) == "implementer" {
                    let branch = given(&v.expected_branch).cloned().unwrap_or_default();
                    latest.insert(heads(&branch), given(&v.expected_head).cloned());
                } else if v.stage.as_deref() == Some("found") {
                    for (name, m) in given(&v.refs).map(|r| r.0.as_slice()).unwrap_or_default() {
                        latest.insert(name.clone(), m.old.clone());
                    }
                }
            }
            _ => {}
        }
    }
    (latest, owned)
}

// --------------------------------------------------------------------------
// The supervisor's session

/// `run.json`, rewritten with one entry's value replaced: every other entry kept as read.
enum MetaValue<'a> {
    Raw(&'a RawValue),
    Sessions(&'a [PyStr]),
}

impl Serialize for MetaValue<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            MetaValue::Raw(raw) => raw.serialize(s),
            MetaValue::Sessions(sessions) => sessions.serialize(s),
        }
    }
}

/// Records the supervisor's session in `run.json` and charges its new usage, once
/// (`_charge_supervisor`).
fn charge_supervisor(run: &Run) -> Result<()> {
    let Some(sid) = session_env() else {
        return Ok(());
    };
    let _lock = state_lock(&run.root)?;
    let text = std::fs::read(&run.paths.run_json).map_err(|e| Error::io(&run.paths.run_json, e))?;
    let text = String::from_utf8(text).map_err(|e| Error::msg(e.to_string()))?;
    let meta: PyMap<Box<RawValue>> =
        serde_json::from_str(&text).map_err(|e| Error::msg(format!("run.json: {e}")))?;
    let key = PyStr::from("supervisor_sessions");
    let mut sessions: Vec<PyStr> = match meta.get(&key) {
        Some(raw) => serde_json::from_str(raw.get())
            .map_err(|e| Error::msg(format!("run.json: supervisor_sessions: {e}")))?,
        None => Vec::new(),
    };
    if !sessions.contains(&sid) {
        sessions.push(sid.clone());
        let mut out: Vec<(PyStr, MetaValue)> = meta
            .0
            .iter()
            .map(|(k, v)| {
                let value = if *k == key {
                    MetaValue::Sessions(&sessions)
                } else {
                    MetaValue::Raw(v)
                };
                (k.clone(), value)
            })
            .collect();
        if meta.get(&key).is_none() {
            out.push((key.clone(), MetaValue::Sessions(&sessions)));
        }
        write_json_atomic(&run.paths.run_json, &PyMap(out))?;
    }
    charge_session(run, &sid)
}

/// Charges a supervisor session's transcript from its checkpoint; the caller holds the state
/// lock (`_charge_session`).
pub(crate) fn charge_session(run: &Run, sid: &PyStr) -> Result<()> {
    let Some(path) = claude_session_transcript(sid, &projects_dir()) else {
        return Ok(());
    };
    let charges = read_charges(&run.paths)?;
    let prior = charged_checkpoint(&charges, Source::Claude, sid);
    let (_, reached) = new_claude_usage(Some(&path), &prior.messages.unwrap_or_default());
    if reached.is_empty() {
        return Ok(());
    }
    let mut prefix = PyStr::from("supervisor:");
    prefix.push(sid);
    prefix.push_str(":");
    let n = 1 + charges
        .iter()
        .filter(|c| c.charge_id.as_wtf8().starts_with(prefix.as_wtf8()))
        .count();
    let mut charge_id = prefix;
    charge_id.push_str(&n.to_string());
    charge(
        &run.paths,
        &charge_id,
        sid,
        "supervisor",
        "supervisor",
        ChargeSource::ClaudeTranscript(Some(&path)),
        false,
    )?;
    Ok(())
}
