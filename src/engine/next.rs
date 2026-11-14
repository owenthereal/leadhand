//! `next` (`engine.next_action`): fold the events, then decide the next step.
//!
//! The core is pure: `decide(state, observations)` reads the folded log (`RunState`) and what
//! the shell observed of the world (`Observations`: the clock, the run branch's tip, the
//! installed vendors, …) and returns one `Decision`: a mechanical step for the shell to carry
//! out (cut a card branch, request a dispatch, append an event, accept a card), an action for
//! the supervisor, or a request for one more observation (`Decision::Need`: a Codex result, the
//! paths a fix changed, a report's text, whether a worker left what it guards as it found it),
//! which the shell gathers before deciding again. The shell (`next_action`) does the I/O: it
//! loops step after step, as the Python does, until a step yields an action.
//!
//! Integration, violations, the end-of-run gates and the outcome, and recovery are Tasks
//! 15–17: where the loop reaches one of them, `decide` returns `Decision::Unported` (or
//! `Decision::End`, the outcome's seam), and `next` fails with `not ported yet: …` rather than
//! guess.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use serde_json::value::RawValue;

use super::cards::{
    Dispatch, Log, Run, accept, active, cancel_codex, card_summary, cut_card, finish_accept,
    guard_moved, is_card, pick, report_copy, request_dispatch, store_codex, write_diff_check,
};
use super::protocol::{
    Action, AnswerAction, CODEX_GRACE_MINUTES, DispatchAction, DoneAction, IMPLEMENTER_KINDS,
    MAX_STEPS, REPLY_FILE, REVIEW_KINDS, Resume, RouteView, StopWorkerAction, WaitAction,
    card_dispatch_id, format_time, now, parse_time, tag,
};
use super::record::store_result;
use crate::briefs::{CODEX_AGENT, after_folded, agent_for, scratch_dir};
use crate::briefs_write::{dispatch_message, strip_pystr};
use crate::config::Route;
use crate::dispatch::{DispatchResult, load_result};
use crate::error::{Error, Result};
use crate::git::cards::{card_branch_name, changed_paths, is_test_or_doc_path, operation, ref_sha};
use crate::pytext::{py_isspace, py_strip};
use crate::route::{RouteError, RouteRequest, resolve};
use crate::state::events::{self, given};
use crate::state::{Blocked, CardRef, CardStage, CardState, Event, append_event};
use crate::store::{state_lock, write_json_atomic};
use crate::usage::{
    ChargeSource, Source, charge, charged_checkpoint, claude_session_transcript, new_claude_usage,
    read_charges,
};
use crate::wire::{PyMap, PyStr};

// --------------------------------------------------------------------------
// The core: state, observations, decisions

/// The run as one step sees it: its records and each card's state, folded from the events.
pub struct RunState<'r> {
    pub run: &'r Run,
    pub log: Log,
    pub cards: BTreeMap<u32, CardState>,
}

impl<'r> RunState<'r> {
    pub fn new(run: &'r Run, log: Log) -> RunState<'r> {
        let cards = log.cards(&run.numbers());
        RunState { run, log, cards }
    }
}

/// What the shell observed of the world for one step. The first fields are read before every
/// step; the maps fill as `decide` asks for them (`Decision::Need`).
#[derive(Clone, Debug, Default)]
pub struct Observations {
    /// The engine's clock (`engine._now`).
    pub now: f64,
    /// The length of a worker-limit minute.
    pub minute_seconds: f64,
    /// The vendors whose CLI is installed.
    pub installed: Vec<String>,
    /// The supervisor session making this call (`engine._current_session`).
    pub session: Option<PyStr>,
    /// The run branch's commit; `None` when the workspace or the branch is missing.
    pub run_tip: Option<String>,
    /// Why the source deck no longer matches the run's (`engine._deck_problem`).
    pub deck_problem: Option<String>,
    /// `LEADHAND_TEST_CARD_LOOP_ONLY`: stop where the card loop ends.
    pub card_loop_only: bool,
    /// The operator's `cancel` and `pause` flags.
    pub cancel_flag: bool,
    pub pause_flag: bool,
    /// The path that names this CLI in the dispatch lines it prints.
    pub cli: PathBuf,
    /// A Codex dispatch's `result.json`, by dispatch ID.
    pub codex_results: HashMap<String, Option<DispatchResult>>,
    /// The Git operation left in progress in the workspace, once looked for.
    pub operation: Option<Option<String>>,
    /// The paths changed between two commits.
    pub changed: HashMap<(String, String), Vec<PyStr>>,
    /// A report's text, by its path (`None` when there is none).
    pub texts: HashMap<PyStr, Option<String>>,
    /// Whether a dispatch's worker changed what its dispatch guards, by dispatch ID.
    pub guards: HashMap<String, bool>,
}

/// One more thing to observe before deciding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// A Codex dispatch's `result.json`.
    CodexResult(String),
    /// The Git operation left in progress in the workspace.
    Operation,
    /// The paths changed between two commits.
    Changed(String, String),
    /// A report's text.
    Text(PyStr),
    /// Whether a dispatch's worker left what its dispatch guards as it found it: the card tip,
    /// a clean workspace and the run's refs for a check; the run branch for an implementer.
    Guard(String),
}

/// A dispatch to request (`engine._request`): the shell checks out the card branch, takes the
/// dispatch's guard, writes the brief and appends `dispatch_requested`.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub card: u32,
    /// `c<card>-a<attempt>-<kind>-<n>`: deterministic, so a crash before the event requests
    /// the same dispatch again.
    pub dispatch_id: String,
    pub kind: String,
    pub attempt: i64,
    pub role: String,
    pub route: Route,
    /// The kind's brief values; recorded in the event.
    pub extra: PyMap<Option<PyStr>>,
    /// The commit the card's work starts from (`_card_base`).
    pub base: String,
    /// The dispatch whose worker this one resumes.
    pub resume: Option<String>,
    pub notes: PyStr,
    /// The interrupted dispatch a `continue` carries on from.
    pub continues: Option<String>,
}

impl Request {
    fn new(card: u32, kind: &str, attempt: i64, role: &str, route: Route) -> Request {
        Request {
            card,
            dispatch_id: String::new(),
            kind: kind.to_string(),
            attempt,
            role: role.to_string(),
            route,
            extra: PyMap::default(),
            base: String::new(),
            resume: None,
            notes: PyStr::default(),
            continues: None,
        }
    }
}

/// How the run ends (`engine._end`'s arguments): the outcome is Task 16's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ending {
    pub blocked: Option<Blocked>,
    pub why: Vec<PyStr>,
    /// A mismatch between the run branch and what the run accepted.
    pub unreconciled: Option<String>,
}

/// A test-only fix's diff check: the shell writes `fix.diff` and returns `check_diff`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffCheck {
    pub card: u32,
    pub dispatch_id: String,
    pub pre: Option<String>,
    pub tip: Option<String>,
    pub changed: Vec<PyStr>,
    pub findings: Option<PyStr>,
}

/// What one step does.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Observe this, then decide again.
    Need(Need),
    /// Return this action to the supervisor.
    Act(Action),
    /// Cut the card's branch from the run tip (`card_cut`).
    Cut(u32),
    /// Request a dispatch.
    Request(Box<Request>),
    /// Append this event (a park, a verdict, a fix round).
    Append(Box<Event>),
    /// Accept the card at this tip: intent, fast-forward, completion (`_accept`).
    Accept { card: u32, target: Option<String> },
    /// Complete an acceptance whose intent is recorded (`_finish_accept`).
    FinishAccept { card: u32, target: Option<String> },
    /// Write a test-only fix's diff, and ask for its check.
    CheckDiff(DiffCheck),
    /// Record a finished Codex runner's result, then return `then` (or carry on).
    StoreCodex {
        dispatch_id: String,
        then: Option<Action>,
    },
    /// Stop a Codex dispatch past its deadline (runner and Codex confirmed gone), record its
    /// result, then return `then` (or carry on).
    StopCodex {
        dispatch_id: String,
        then: Option<Action>,
    },
    /// Record a Claude worker reconciliation started, past its deadline, as interrupted.
    StoreInterrupted { dispatch_id: String },
    /// End the run (Task 16).
    End(Ending),
    /// A step a later task ports.
    Unported { what: &'static str, task: u8 },
    /// The step cannot be taken.
    Fail(Error),
}

/// One step's way through `decide`: a decision found early is the `Err` side, so `?` returns
/// it (a `Need`, a seam, a failure).
type Step<T> = std::result::Result<T, Box<Decision>>;

impl From<Error> for Box<Decision> {
    fn from(e: Error) -> Box<Decision> {
        Box::new(Decision::Fail(e))
    }
}

fn unported<T>(what: &'static str, task: u8) -> Step<T> {
    Err(Box::new(Decision::Unported { what, task }))
}

fn fail<T>(message: String) -> Step<T> {
    Err(Box::new(Decision::Fail(Error::msg(message))))
}

fn need<T>(need: Need) -> Step<T> {
    Err(Box::new(Decision::Need(need)))
}

/// The next step (the body of `engine._next_action`'s loop, then `_step`). Pure: everything it
/// knows of the world is in `obs`.
pub fn decide(st: &RunState, obs: &Observations) -> Decision {
    decide_step(st, obs).unwrap_or_else(|d| *d)
}

fn decide_step(st: &RunState, obs: &Observations) -> Step<Decision> {
    if st.log.ended().is_some() {
        return unported("the wrap-up and the done action of a run that ended", 16);
    }
    if st.log.needs_reconcile() {
        return unported("reconciling a relaunched run", 17);
    }
    if st.log.cancelled() || obs.cancel_flag {
        return unported("the operator's cancel", 17);
    }
    if st.log.paused() || obs.pause_flag {
        return unported("the operator's pause", 17);
    }
    if let Some(problem) = &obs.deck_problem {
        return fail(problem.clone());
    }
    step(st, obs)
}

/// One mechanical step, or an action (`engine._step`).
fn step(st: &RunState, obs: &Observations) -> Step<Decision> {
    if let Some(problem) = run_head_problem(st, obs)
        && !run_ref_guarded(st)
    {
        // The run branch moved with no acceptance and no worker guard to explain it (I4).
        return Ok(Decision::End(Ending {
            unreconciled: Some(problem),
            ..Ending::default()
        }));
    }
    if !st.log.pending_leftovers().is_empty() {
        return unported("restoring an implementer's leftovers", 15);
    }
    if !st.log.run_dispatches().is_empty() {
        return unported(
            "the run-level dispatches (end-of-run gates and fix waves)",
            16,
        );
    }
    let Some(n) = pick(st.run, &st.log, &st.cards)? else {
        return end_of_run(st, obs);
    };
    let c = &st.cards[&n];
    match c.state {
        CardStage::Pending => Ok(Decision::Cut(n)),
        CardStage::Cut => match st.log.pending_unpark(n) {
            Some(u) => resume_unparked(st, obs, c, u.note.clone().unwrap_or_default()),
            None => start_attempt(st, obs, c, c.attempt.max(1), None, PyStr::default()),
        },
        CardStage::Accepting => Ok(Decision::FinishAccept {
            card: n,
            target: c.accept_target.clone(),
        }),
        CardStage::Integrating => unported("integrating a card with the run branch", 15),
        CardStage::Reviewed => after_verdict(st, obs, c),
        CardStage::Fixing if c.dispatch_kind.as_deref() != Some("fix") => request_fix(st, c),
        _ => {
            let Some(d) = st.log.dispatch(c.dispatch_id.as_deref()) else {
                return fail(format!(
                    "card {n} is {} but its dispatch {} is not recorded",
                    c.state.as_str(),
                    c.dispatch_id.as_deref().unwrap_or("None")
                ));
            };
            if d.result.is_none() {
                in_flight(st, obs, &d)
            } else {
                after_result(st, obs, c, &d)
            }
        }
    }
}

/// Why the run head is not the last accepted commit (else the base), or `None`
/// (`engine._run_head_problem`). An acceptance whose fast-forward happened but is not
/// recorded yet is not a problem.
fn run_head_problem(st: &RunState, obs: &Observations) -> Option<String> {
    let branch = st.run.branch.shown();
    let Some(now) = &obs.run_tip else {
        return Some(format!("the run branch {branch} is missing"));
    };
    let accepted: Vec<&events::Accepted> = st
        .log
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Accepted(a) if a.sha.as_deref().is_some_and(|s| !s.is_empty()) => Some(a),
            _ => None,
        })
        .collect();
    let expected = accepted
        .last()
        .and_then(|a| a.sha.clone())
        .unwrap_or_else(|| st.run.base_sha.clone());
    if *now == expected {
        return None;
    }
    let after = accepted.last().map_or(0, |a| a.seq);
    let accepting = st.log.events.iter().any(|e| {
        matches!(e, Event::AcceptStarted(a) if a.seq > after && a.target.as_deref() == Some(now))
    });
    if accepting {
        return None;
    }
    let short = |s: &str| s.chars().take(12).collect::<String>();
    Some(format!(
        "the run branch {branch} is at {}, not at the last accepted commit {}{}",
        short(now),
        short(&expected),
        if accepted.is_empty() {
            " (the base)"
        } else {
            ""
        }
    ))
}

/// Whether a dispatch that guards the run branch will still check it (`_run_ref_guarded`):
/// one with no result yet, a card's current dispatch whose result is not acted on yet, the
/// latest run-level dispatch whose result is not settled, or a violation not restored yet.
fn run_ref_guarded(st: &RunState) -> bool {
    let key = |v: &events::Violation| (given(&v.dispatch_id).cloned(), v.n);
    let stage = |s: &str| {
        st.log
            .check_violations()
            .filter(|v| v.stage.as_deref() == Some(s))
            .map(key)
            .collect::<HashSet<_>>()
    };
    if !stage("found").is_subset(&stage("restored")) {
        return true;
    }
    let run_ref = st.run.run_ref();
    let current: HashSet<Option<&str>> = st
        .cards
        .values()
        .filter(|c| {
            active(c.state)
                && !matches!(
                    c.state,
                    CardStage::Reviewed | CardStage::Integrating | CardStage::Accepting
                )
                && !(c.state == CardStage::Fixing && c.dispatch_kind.as_deref() != Some("fix"))
        })
        .map(|c| c.dispatch_id.as_deref())
        .collect();
    let runs = st.log.run_dispatches();
    for d in st.log.dispatches() {
        let guards_run = given(&d.req.refs).is_some_and(|refs| refs.get(&run_ref).is_some());
        if !guards_run {
            continue;
        }
        if d.req.card.is_some() {
            if current.contains(&Some(d.id())) {
                return true;
            }
        } else if runs.last().is_some_and(|last| last.id() == d.id()) {
            let settled = st.log.events.iter().any(|e| match e {
                Event::GateResult(g) => g.dispatch_id.as_deref() == Some(d.id()),
                Event::VerdictRecorded(v) => v.dispatch_id.as_deref() == Some(d.id()),
                _ => false,
            });
            if d.result.is_none() || !settled {
                return true;
            }
        }
    }
    false
}

/// Every card is accepted, parked, or waits on a parked card (`engine._end_of_run`). The
/// card-loop-only test mode stops here; the gates and the outcome are Task 16's.
fn end_of_run(st: &RunState, obs: &Observations) -> Step<Decision> {
    if !obs.card_loop_only {
        return unported("the end-of-run gates and the outcome", 16);
    }
    let cards = card_summary(st.run, &st.cards)?;
    let reasons = cards
        .iter()
        .filter(|row| !row.reason.is_empty())
        .map(|row| {
            let mut text = PyStr::from(format!("card {} {}: ", row.card, row.state));
            text.push(&row.reason);
            text
        })
        .collect();
    Ok(Decision::Act(Action::Done(DoneAction {
        outcome: None,
        qualifier: String::new(),
        reasons,
        report: None,
        cards,
        card_loop_only: true,
    })))
}

// -- dispatching -------------------------------------------------------------

/// The route for a role, or the card parked on why there is none (`_resolve_or_park`).
fn resolve_or_park(
    st: &RunState,
    obs: &Observations,
    card: u32,
    role: &str,
    attempt: i64,
    last_vendor: Option<&str>,
) -> Step<(String, Route)> {
    let deck_card = st.run.card(card)?;
    let found = resolve(
        &st.run.config,
        &RouteRequest {
            role,
            card: Some(deck_card),
            attempt,
            last_implementer_vendor: last_vendor,
            installed: &obs.installed,
            security: false,
            profile: st.run.deck.profile.as_deref(),
        },
    );
    let reason = match found {
        Ok(res) => return Ok((res.role, res.route)),
        Err(RouteError::LadderExhausted(_)) => "ladder exhausted".to_string(),
        Err(RouteError::PinnedVendorMissing(_)) => "pinned vendor not installed".to_string(),
        Err(RouteError::Invalid(m)) => m,
    };
    Err(Box::new(park(card, reason.into())))
}

/// Append this event.
fn append(event: Event) -> Decision {
    Decision::Append(Box::new(event))
}

/// The `parked` event for a card.
fn park(card: u32, reason: PyStr) -> Decision {
    append(Event::Parked(events::Parked {
        card: Some(CardRef::Number(card.into())),
        reason: Some(reason),
        ..events::Parked::default()
    }))
}

/// `extra` from its pairs.
fn extra(pairs: Vec<(&str, PyStr)>) -> PyMap<Option<PyStr>> {
    PyMap(
        pairs
            .into_iter()
            .map(|(k, v)| (PyStr::from(k), Some(v)))
            .collect(),
    )
}

/// The commit the card's work starts from: its cut base (`_card_base`). Once it has been
/// integrated, where its branch meets the run branch: Task 15's.
fn card_base(st: &RunState, c: &CardState) -> Step<String> {
    let integrated = c.branch.as_ref().is_some_and(|b| !b.is_empty())
        && st.log.events.iter().any(
            |e| matches!(e, Event::IntegrationStarted(i) if is_card(i.card.as_ref(), c.number)),
        );
    if integrated {
        return unported("the base of an integrated card", 15);
    }
    Ok(c.base.clone().unwrap_or_default())
}

/// The request, with its deterministic dispatch ID and the card's base.
fn request(st: &RunState, c: &CardState, mut r: Request) -> Step<Decision> {
    let n = 1 + st
        .log
        .events
        .iter()
        .filter(|e| {
            matches!(e, Event::DispatchRequested(d)
                if is_card(d.card.as_ref(), c.number)
                    && d.attempt == Some(r.attempt)
                    && d.dispatch_kind.as_deref() == Some(r.kind.as_str()))
        })
        .count();
    r.dispatch_id = card_dispatch_id(c.number, r.attempt, &r.kind, n);
    r.base = card_base(st, c)?;
    Ok(Decision::Request(Box::new(r)))
}

/// A fresh implementer on the card's attempt, or the card parked (`_start_attempt`).
fn start_attempt(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    attempt: i64,
    last_vendor: Option<&str>,
    notes: PyStr,
) -> Step<Decision> {
    let (role, route) = resolve_or_park(st, obs, c.number, "implementer", attempt, last_vendor)?;
    let mut r = Request::new(c.number, "implement", attempt, &role, route);
    r.notes = notes;
    request(st, c, r)
}

/// A fresh worker on the next rung of the ladder, with the previous report in its brief
/// (`_escalate`).
fn escalate(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    why: PyStr,
    review: Option<&Dispatch>,
    label: &str,
) -> Step<Decision> {
    let implementers = st
        .log
        .card_dispatches(c.number, None, Some(&IMPLEMENTER_KINDS));
    let Some(last) = implementers.iter().max_by_key(|d| d.seq()) else {
        return fail("max() arg is an empty sequence".into());
    };
    let attempt = last.attempt();
    let mut lines: Vec<PyStr> = vec!["## Previous attempt".into(), "".into()];
    let mut said = PyStr::from(format!("Attempt {attempt} on this card "));
    said.push(&why);
    said.push_str(
        ". Read what it reported before you start; the card branch holds its commits. Keep \
         what is sound and fix or replace the rest.",
    );
    lines.extend([said, "".into()]);
    for (name, d) in [("Its report", Some(last)), (label, review)] {
        if let Some(d) = d.filter(|d| d.result.is_some()) {
            let mut line = PyStr::from(format!("- {name}: "));
            line.push(&report_copy(d));
            lines.push(line);
        }
    }
    let notes = join(&lines, "\n");
    start_attempt(st, obs, c, attempt + 1, Some(last.vendor()), notes)
}

fn join(parts: &[PyStr], sep: &str) -> PyStr {
    let mut out = PyStr::default();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push(p);
    }
    out
}

/// The attempt's latest implementer-side dispatch with a worker, requested before
/// `before_seq` (`_last_implementer`).
fn last_implementer<'l>(
    st: &'l RunState,
    card: u32,
    attempt: i64,
    before_seq: u64,
) -> Step<Dispatch<'l>> {
    st.log
        .card_dispatches(card, Some(attempt), Some(&IMPLEMENTER_KINDS))
        .into_iter()
        .filter(|d| d.started.is_some() && d.seq() < before_seq)
        .max_by_key(Dispatch::seq)
        .ok_or_else(|| {
            Box::new(Decision::Fail(Error::msg(format!(
                "card {card}: no implementer worker in attempt {attempt}"
            ))))
        })
}

/// A review of the implementer's work (`_request_review`).
fn request_review(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    implementer: &Dispatch,
) -> Step<Decision> {
    let (role, route) =
        resolve_or_park(st, obs, c.number, "reviewer", 1, Some(implementer.vendor()))?;
    let tip = implementer
        .result
        .and_then(|r| r.tip.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| c.tip.clone());
    let base = card_base(st, c)?;
    let mut r = Request::new(c.number, "review", implementer.attempt(), &role, route);
    r.extra = extra(vec![
        (
            "diff_range",
            format!("{base}..{}", tip.as_deref().unwrap_or("None")).into(),
        ),
        ("implementer_report", report_copy(implementer)),
    ]);
    request(st, c, r)
}

/// The fix round's implementer, resumed with the findings (`_request_fix`).
fn request_fix(st: &RunState, c: &CardState) -> Step<Decision> {
    let rounds: Vec<&events::FixRound> = st
        .log
        .events
        .iter()
        .filter_map(|e| match e {
            Event::FixRound(f) if is_card(f.card.as_ref(), c.number) => Some(f),
            _ => None,
        })
        .collect();
    let Some(fix) = rounds.last() else {
        return fail("list index out of range".into());
    };
    let attempt = fix.attempt.unwrap_or_default();
    let implementer = last_implementer(st, c.number, attempt, fix.seq)?;
    let mut r = Request::new(
        c.number,
        "fix",
        attempt,
        implementer.role(),
        implementer.route(),
    );
    let round = fix.n.map_or_else(|| "None".to_string(), |n| n.to_string());
    r.extra = PyMap(vec![
        ("findings".into(), fix.findings.clone()),
        ("round".into(), Some(round.into())),
    ]);
    r.resume = Some(implementer.id().to_string());
    request(st, c, r)
}

/// A dispatch whose worker has not reported (`_in_flight`): the dispatch action until a worker
/// is known to exist, then `wait` until its deadline, then `stop_worker`. A Codex dispatch's
/// result is the runner's `result.json`, recorded as soon as it is there.
fn in_flight(st: &RunState, obs: &Observations, d: &Dispatch) -> Step<Decision> {
    let Some(started) = d.started else {
        let action = dispatch_action(st, obs, d)?;
        return Ok(Decision::Act(Action::Dispatch(Box::new(action))));
    };
    let codex = d.vendor() == "codex";
    let orphan = codex && st.log.gone(d);
    let reconciled = given(&started.reconciled).copied().unwrap_or(false);
    let deadline = started.deadline.clone().unwrap_or_default();
    let stop_codex = || {
        Action::StopWorker(StopWorkerAction {
            action: tag::StopWorker,
            dispatch_id: d.id().to_string(),
            worker_id: started.worker_id.clone(),
            vendor: "codex",
            record_result: false,
            reason: None,
        })
    };
    if codex {
        let Some(result) = obs.codex_results.get(d.id()) else {
            return need(Need::CodexResult(d.id().to_string()));
        };
        if result.is_some() {
            let then = if orphan || reconciled || obs.now < parse_time(&deadline)? {
                None
            } else {
                // The runner finished, but this session's proxy is still out past the
                // deadline: stop it.
                Some(stop_codex())
            };
            return Ok(Decision::StoreCodex {
                dispatch_id: d.id().to_string(),
                then,
            });
        }
    }
    if obs.now < parse_time(&deadline)? {
        let whose = whose(d);
        let reason = if orphan {
            format!(
                "the Codex worker for {whose} outlived an earlier supervisor session; no \
                 completion notice comes for it, and the timer reports its result"
            )
        } else {
            format!("{} worker for {whose} is running", d.kind())
        };
        return Ok(Decision::Act(Action::Wait(WaitAction {
            action: tag::Wait,
            dispatch_id: d.id().to_string(),
            worker_id: started.worker_id.clone(),
            deadline: started.deadline.clone(),
            reply_file: reply_file(st, d)?,
            reason,
        })));
    }
    if codex {
        // A runner that finished before the cancel keeps its own status (the proxy only hung);
        // no agent of this session runs a reconciled one, so there is nothing to stop.
        return Ok(Decision::StopCodex {
            dispatch_id: d.id().to_string(),
            then: (!reconciled).then(stop_codex),
        });
    }
    if reconciled {
        return Ok(Decision::StoreInterrupted {
            dispatch_id: d.id().to_string(),
        });
    }
    Ok(Decision::Act(Action::StopWorker(StopWorkerAction {
        action: tag::StopWorker,
        dispatch_id: d.id().to_string(),
        worker_id: started.worker_id.clone(),
        vendor: "claude",
        record_result: true,
        reason: None,
    })))
}

/// `the run`, or `card <n>` (`_whose`).
fn whose(d: &Dispatch) -> String {
    match &d.req.card {
        None => "the run".to_string(),
        Some(c) => format!("card {}", c.text().lossy()),
    }
}

/// The scratch folder's card component of a dispatch.
fn scratch_card(d: &Dispatch) -> Option<i64> {
    d.req.card.as_ref().and_then(CardRef::number)
}

/// Where the supervisor writes the worker's reply (`_reply_file`, R38).
fn reply_file(st: &RunState, d: &Dispatch) -> Step<PyStr> {
    let folder = scratch_dir(&st.run.ws, scratch_card(d), d.id())?;
    Ok(path_text(&folder.join(REPLY_FILE)))
}

pub(crate) fn path_text(p: &Path) -> PyStr {
    PyStr::from_os(crate::pytext::pure_path(p.as_os_str()).as_os_str())
}

/// How long the worker may run, in seconds: its limit, plus the Codex grace for a Codex
/// route (`_budget_seconds`).
pub(crate) fn budget_seconds(req: &events::DispatchRequested, minute_seconds: f64) -> f64 {
    let grace = if req.vendor.as_deref() == Some("codex") {
        CODEX_GRACE_MINUTES
    } else {
        0
    };
    (req.minutes.unwrap_or_default() + grace) as f64 * minute_seconds
}

/// The branch a dispatch works on: its card branch (`_work_branch`); the Codex runner commits
/// implementer work there (R39).
pub(crate) fn work_branch(
    run: &Run,
    cards: &BTreeMap<u32, CardState>,
    d: &Dispatch,
) -> Result<PyStr> {
    let Some(card) = &d.req.card else {
        return Ok(run.branch.clone());
    };
    let c = card_state(cards, Some(card))?;
    Ok(c.branch
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| card_branch_name(&run.branch, &card.text().lossy())))
}

/// The state of the card an event or command names (`engine._card_state`).
pub(crate) fn card_state<'c>(
    cards: &'c BTreeMap<u32, CardState>,
    card: Option<&CardRef>,
) -> Result<&'c CardState> {
    let shown = || match card {
        None => "None".to_string(),
        Some(CardRef::Number(n)) => n.to_string(),
        Some(CardRef::Text(t)) => t.repr(),
    };
    let Some(n) = card.and_then(CardRef::number) else {
        return Err(Error::msg(format!("not a card number: {}", shown())));
    };
    u32::try_from(n)
        .ok()
        .and_then(|n| cards.get(&n))
        .ok_or_else(|| Error::msg(format!("no card {n} in the deck")))
}

/// The dispatch line the Codex proxy runs (`_codex_command`).
fn codex_command(st: &RunState, obs: &Observations, d: &Dispatch) -> Step<Vec<OsString>> {
    let req = d.req;
    let os = |s: &str| OsString::from(s);
    let text = |s: &Option<PyStr>| s.clone().unwrap_or_default().to_os();
    let mut cmd = vec![
        obs.cli.clone().into_os_string(),
        os("dispatch"),
        os("--run"),
        st.run.root.clone().into_os_string(),
        os("--dispatch-id"),
        os(d.id()),
    ];
    if let Some(card) = &req.card {
        cmd.extend([os("--card"), card.text().to_os()?]);
    }
    cmd.extend([
        os("--attempt"),
        os(&req.attempt.map_or_else(|| "None".into(), |a| a.to_string())),
        os("--role"),
        os(d.role()),
        os("--model"),
        os(req.model.as_deref().unwrap_or_default()),
        os("--effort"),
        os(req.effort.as_deref().unwrap_or_default()),
        os("--brief"),
        text(&req.brief)?,
        os("--report"),
        text(&req.report)?,
        os("--cwd"),
        crate::pytext::pure_path(st.run.ws.as_os_str()).into_os_string(),
        os("--kind"),
        os(d.kind()),
        os("--branch"),
        work_branch(st.run, &st.cards, d)?.to_os()?,
    ]);
    if let Some(session) = req.resume_session.as_ref().filter(|s| !s.is_empty()) {
        cmd.extend([os("--resume"), session.to_os()?]);
    }
    Ok(cmd)
}

/// The dispatch action for a requested dispatch (`_dispatch_action`).
fn dispatch_action(st: &RunState, obs: &Observations, d: &Dispatch) -> Step<DispatchAction> {
    let req = d.req;
    let brief = req.brief.clone().unwrap_or_default();
    let report = req.report.clone().unwrap_or_default();
    let (agent, agent_model, agent_effort, resume, message) = if d.vendor() == "codex" {
        let line = codex_command(st, obs, d)?;
        let message = dispatch_message(CODEX_AGENT, &brief, &report, Some(&line))?;
        (CODEX_AGENT.to_string(), None, None, None, message)
    } else {
        let agent = agent_for(d.role(), "claude")?;
        let resume = req
            .resume_worker
            .clone()
            .filter(|w| !w.is_empty())
            .map(|worker_id| Resume { worker_id });
        let message = dispatch_message(agent, &brief, &report, None)?;
        (
            agent.to_string(),
            req.model.clone(),
            req.effort.clone(),
            resume,
            message,
        )
    };
    let run_level = req.card.is_none();
    Ok(DispatchAction {
        action: tag::Dispatch,
        dispatch_id: d.id().to_string(),
        kind: req.dispatch_kind.clone(),
        card: req.card.clone(),
        attempt: req.attempt,
        role: req.role.clone(),
        agent,
        agent_model,
        agent_effort,
        route: RouteView {
            vendor: req.vendor.clone(),
            model: req.model.clone(),
            effort: req.effort.clone(),
        },
        message,
        brief: req.brief.clone(),
        report: req.report.clone(),
        resume,
        reply_file: reply_file(st, d)?,
        deadline: format_time(obs.now + budget_seconds(req, obs.minute_seconds)),
        gate: run_level.then(|| given(&req.gate).cloned()),
        wave: run_level.then(|| given(&req.wave).copied()),
    })
}

// -- after a result ------------------------------------------------------------

/// A dispatch's result, acted on (`_after_result`).
fn after_result(st: &RunState, obs: &Observations, c: &CardState, d: &Dispatch) -> Step<Decision> {
    let r = d.result.expect("after_result is called with a result");
    if d.req.refs.is_some() {
        // What the dispatch guards must be as the worker found it (R14', I4).
        if st
            .log
            .check_violations()
            .any(|v| given(&v.dispatch_id).map(String::as_str) == Some(d.id()))
        {
            return unported("a voided result (violations)", 15);
        }
        match obs.guards.get(d.id()) {
            None => return need(Need::Guard(d.id().to_string())),
            Some(true) => return unported("preserving and undoing a worker's violation", 15),
            Some(false) => {}
        }
    }
    if IMPLEMENTER_KINDS.contains(&d.kind()) {
        // A rebase, merge or cherry-pick left in progress is the worker's failure.
        if st.log.broken_dispatches().contains(d.id()) {
            return unported("an implementer that left a Git operation in progress", 15);
        }
        if c.branch.as_ref().is_some_and(|b| !b.is_empty()) {
            match &obs.operation {
                None => return need(Need::Operation),
                Some(Some(_)) => {
                    return unported("an implementer that left a Git operation in progress", 15);
                }
                Some(None) => {}
            }
        }
    }
    if given(&r.interrupted).copied().unwrap_or(false) {
        return unported("continuing after an interrupted worker", 17);
    }
    if d.kind() == "verify" {
        return unported("the verify of an integrated card", 15);
    }
    if REVIEW_KINDS.contains(&d.kind()) {
        if let Some(verdict) = r
            .verdict
            .as_deref()
            .filter(|v| matches!(*v, "clean" | "defects"))
        {
            return Ok(append(Event::VerdictRecorded(events::VerdictRecorded {
                card: Some(CardRef::Number(c.number.into())),
                verdict: Some(verdict.to_string()),
                findings: Some(report_copy(d)),
                dispatch_id: Some(d.id().to_string()),
                ..events::VerdictRecorded::default()
            })));
        }
        return worker_failed(st, obs, c, d);
    }
    match r.status.as_deref() {
        Some("READY") => {
            if integrate_origin(st, Some(*d)).is_some() {
                return unported("an integrate attempt's result", 15);
            }
            if let Some(fix) = st.log.fix_round(c.number, d.attempt(), Some(d.seq())) {
                return after_fix(st, obs, c, d, fix);
            }
            let card = st.run.card(c.number)?;
            let tip = r
                .tip
                .clone()
                .filter(|t| !t.is_empty())
                .or_else(|| c.tip.clone());
            if st.run.config.run.card_review == "by-risk"
                && card.risk == "low"
                && !card.hard
                && tip != attempt_start_tip(st, c, d.attempt())
            {
                return Ok(Decision::Accept {
                    card: c.number,
                    target: tip,
                });
            }
            request_review(st, obs, c, d)
        }
        Some("QUESTION") => {
            let ruling = st.log.events.iter().find_map(|e| match e {
                Event::Ruling(r) if r.dispatch_id.as_deref() == Some(d.id()) => Some(r),
                _ => None,
            });
            let Some(ruling) = ruling else {
                let report = report_copy(d);
                let Some(text) = obs.texts.get(&report) else {
                    return need(Need::Text(report));
                };
                return Ok(Decision::Act(Action::Answer(AnswerAction {
                    action: tag::Answer,
                    card: c.number,
                    dispatch_id: d.id().to_string(),
                    worker_id: d.started.and_then(|s| s.worker_id.clone()),
                    report,
                    question: question(text.as_deref().unwrap_or_default()).into(),
                })));
            };
            let mut r = Request::new(c.number, "answer", d.attempt(), d.role(), d.route());
            r.extra = PyMap(vec![("ruling".into(), ruling.text.clone())]);
            r.resume = Some(d.id().to_string());
            request(st, c, r)
        }
        Some("SPENT") => escalate(
            st,
            obs,
            c,
            "ended SPENT".into(),
            None,
            "The review findings",
        ),
        _ => worker_failed(st, obs, c, d),
    }
}

/// Where the card branch stood when the attempt's first implementer-side dispatch was
/// requested, else the card's base (`_attempt_start_tip`). A READY at that tip changed
/// nothing: with `card_review = "by-risk"` it still goes to review (I8).
fn attempt_start_tip(st: &RunState, c: &CardState, attempt: i64) -> Option<String> {
    let first = st
        .log
        .card_dispatches(c.number, Some(attempt), Some(&IMPLEMENTER_KINDS))
        .into_iter()
        .min_by_key(Dispatch::seq);
    first
        .and_then(|d| given(&d.req.start_tip).cloned())
        .filter(|t| !t.is_empty())
        .or_else(|| c.base.clone())
}

/// BLOCKED from any worker, or a reviewer that ended without a verdict (`_worker_failed`): a
/// usage limit ends the run blocked; the same step is dispatched again once; a second such
/// ending ends the run blocked (environment), or for a missing verdict parks the card.
fn worker_failed(
    st: &RunState,
    _obs: &Observations,
    c: &CardState,
    d: &Dispatch,
) -> Step<Decision> {
    let r = d.result.expect("worker_failed is called with a result");
    let broken = st.log.broken_dispatches();
    let head_of = |x: &Dispatch| -> (String, String) {
        if broken.contains(x.id()) {
            return ("BLOCKED".into(), "environment".into());
        }
        let r = x.result.expect("only results are judged");
        (
            r.status.clone().unwrap_or_default(),
            r.blocked_by.clone().unwrap_or_default(),
        )
    };
    let (status, blocked_by) = head_of(d);
    let ended_after = st
        .log
        .events
        .iter()
        .any(|e| matches!(e, Event::Outcome(o) if o.seq > r.seq));
    if !ended_after {
        if status == "BLOCKED" && blocked_by == "usage limit" {
            return Ok(Decision::End(Ending {
                blocked: Some(Blocked::ProviderCapacity),
                ..Ending::default()
            }));
        }
        // The budget restarts only after an ending the run was resumed from.
        let since = st.log.reopened_since();
        let voided = st.log.voided();
        let same: Vec<Dispatch> = st
            .log
            .card_dispatches(c.number, Some(d.attempt()), Some(&[d.kind()]))
            .into_iter()
            .filter(|x| {
                x.result.is_some_and(|xr| {
                    xr.seq > since && !given(&xr.interrupted).copied().unwrap_or(false)
                }) && !voided.contains(x.id())
            })
            .collect();
        if status == "BLOCKED" {
            let failures = same
                .iter()
                .filter(|x| {
                    let h = head_of(x);
                    h != ("BLOCKED".into(), "usage limit".into()) && h.0 == "BLOCKED"
                })
                .count();
            if failures >= 2 {
                return Ok(Decision::End(Ending {
                    blocked: Some(Blocked::Environment),
                    ..Ending::default()
                }));
            }
        } else {
            let failures = same
                .iter()
                .filter(|x| {
                    let xr = x.result.expect("filtered on results");
                    head_of(x).0 != "BLOCKED" && xr.verdict.is_none() && xr.result.is_none()
                })
                .count();
            if failures >= 2 {
                let what = if d.kind() == "verify" {
                    "a result"
                } else {
                    "a verdict"
                };
                let reason = format!("{} ended {status} twice without {what}", d.kind());
                return Ok(park(c.number, reason.into()));
            }
        }
    }
    let mut again = Request::new(c.number, d.kind(), d.attempt(), d.role(), d.route());
    again.extra = d.extra();
    again.notes = d.notes();
    again.resume = st
        .log
        .dispatch(d.req.resumes.as_deref())
        .map(|x| x.id().to_string());
    request(st, c, again)
}

/// After a review's verdict (`_after_verdict`): clean is accepted; defects get one fix round,
/// then a fresh worker on the next rung.
fn after_verdict(st: &RunState, obs: &Observations, c: &CardState) -> Step<Decision> {
    let verdict = st.log.events.iter().rev().find_map(|e| match e {
        Event::VerdictRecorded(v) if is_card(v.card.as_ref(), c.number) => Some(v),
        _ => None,
    });
    let Some(verdict) = verdict else {
        return fail("list index out of range".into());
    };
    let Some(review) = st.log.dispatch(verdict.dispatch_id.as_deref()) else {
        return fail(format!(
            "card {}: the review {} of its verdict is not recorded",
            c.number,
            verdict.dispatch_id.as_deref().unwrap_or("None")
        ));
    };
    let attempt = review.attempt();
    if c.verdict.as_deref() == Some("clean") {
        return Ok(Decision::Accept {
            card: c.number,
            target: c.tip.clone(),
        });
    }
    if st.log.fix_round(c.number, attempt, None).is_none() {
        return Ok(append(Event::FixRound(events::FixRound {
            card: Some(CardRef::Number(c.number.into())),
            n: Some(c.fix_rounds + 1),
            attempt: Some(attempt),
            pre_tip: c.tip.clone(),
            findings: c.findings.clone(),
            review: Some(review.id().to_string()),
            ..events::FixRound::default()
        })));
    }
    escalate(
        st,
        obs,
        c,
        "still had review defects after its fix round".into(),
        Some(&review),
        "The review findings",
    )
}

/// A READY in the fix round (`_after_fix`): production changes get a narrow re-review; a fix
/// that only touched tests and docs gets the supervisor's diff check.
fn after_fix(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    d: &Dispatch,
    fix: &events::FixRound,
) -> Step<Decision> {
    let pre = fix.pre_tip.clone().filter(|p| !p.is_empty());
    let tip = d
        .result
        .and_then(|r| r.tip.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| c.tip.clone());
    let changed = match (&pre, &tip) {
        (Some(pre), Some(tip)) if pre != tip && !tip.is_empty() => {
            let key = (pre.clone(), tip.clone());
            match obs.changed.get(&key) {
                Some(paths) => paths.clone(),
                None => return need(Need::Changed(key.0, key.1)),
            }
        }
        _ => Vec::new(),
    };
    if changed.iter().any(|p| !is_test_or_doc_path(p)) {
        let Some(review) = st.log.dispatch(fix.review.as_deref()) else {
            return fail(format!(
                "card {}: the review {} of its fix round is not recorded",
                c.number,
                fix.review.as_deref().unwrap_or("None")
            ));
        };
        let base = card_base(st, c)?;
        let mut r = Request::new(
            c.number,
            "rereview",
            d.attempt(),
            review.role(),
            review.route(),
        );
        r.resume = Some(review.id().to_string());
        r.extra = PyMap(vec![
            (
                "diff_range".into(),
                Some(format!("{base}..{}", tip.as_deref().unwrap_or("None")).into()),
            ),
            ("findings".into(), fix.findings.clone()),
            ("implementer_report".into(), Some(report_copy(d))),
        ]);
        return request(st, c, r);
    }
    let check = st.log.events.iter().find_map(|e| match e {
        Event::DiffChecked(k) if k.dispatch_id.as_deref() == Some(d.id()) => Some(k),
        _ => None,
    });
    if let Some(check) = check {
        if check.ok == Some(true) {
            return Ok(Decision::Accept {
                card: c.number,
                target: tip,
            });
        }
        let mut why = PyStr::from("had its fix rejected by the diff check: ");
        why.push(&check.note.clone().unwrap_or_default());
        return escalate(st, obs, c, why, None, "The review findings");
    }
    Ok(Decision::CheckDiff(DiffCheck {
        card: c.number,
        dispatch_id: d.id().to_string(),
        pre,
        tip,
        changed,
        findings: fix.findings.clone(),
    }))
}

/// The integrate dispatch `d` is, or that `d` answers or continues, through any chain
/// (`_integrate_origin`).
fn integrate_origin<'l>(st: &'l RunState, d: Option<Dispatch<'l>>) -> Option<Dispatch<'l>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut d = d;
    while let Some(x) = d {
        if seen.contains(x.id()) {
            break;
        }
        if x.kind() == "integrate" {
            return Some(x);
        }
        if !matches!(x.kind(), "answer" | "continue") {
            return None;
        }
        seen.insert(x.id().to_string());
        let next = given(&x.req.continues)
            .cloned()
            .filter(|s| !s.is_empty())
            .or_else(|| x.req.resumes.clone().filter(|s| !s.is_empty()))
            .unwrap_or_default();
        d = st.log.dispatch(Some(&next));
    }
    None
}

/// Resumes a parked card the operator unparked (`_resume_unparked`): its last implementer
/// worker with the note as the answer when that worker can still be reached, else a fresh
/// worker on the card's attempt. A card the violation cap parked re-runs the kind it was
/// parked on, on the same route (R32).
fn resume_unparked(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    note: PyStr,
) -> Step<Decision> {
    let parks: Vec<&events::Parked> = st
        .log
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Parked(p) if is_card(p.card.as_ref(), c.number) => Some(p),
            _ => None,
        })
        .collect();
    let capped = parks
        .last()
        .and_then(|p| st.log.dispatch(given(&p.cap_dispatch).map(String::as_str)));
    if let (Some(capped), Some(last_park)) = (capped, parks.last()) {
        let earlier = capped.notes().strip_ascii(b"\n", false);
        let mut notes = earlier.clone();
        if !earlier.is_empty() {
            notes.push_str("\n\n");
        }
        notes.push_str("## The operator's note\n\nThis card was parked because ");
        notes.push(&last_park.reason.clone().unwrap_or_else(|| "None".into()));
        notes.push_str(". The operator resumed it with:\n\n");
        notes.push(&note);
        let mut r = Request::new(
            c.number,
            capped.kind(),
            capped.attempt(),
            capped.role(),
            capped.route(),
        );
        r.extra = capped.extra();
        r.notes = notes;
        if IMPLEMENTER_KINDS.contains(&capped.kind()) {
            r.resume = st
                .log
                .dispatch(capped.req.resumes.as_deref())
                .map(|x| x.id().to_string());
        }
        return request(st, c, r);
    }
    let last = st
        .log
        .card_dispatches(c.number, None, Some(&IMPLEMENTER_KINDS))
        .into_iter()
        .filter(|d| d.started.is_some())
        .max_by_key(Dispatch::seq);
    if let Some(last) = last {
        let reachable = if last.vendor() == "claude" {
            let session = last
                .started
                .and_then(|s| s.session.clone())
                .filter(|s| !s.is_empty());
            session.is_some() && session == obs.session
        } else {
            last.result
                .and_then(|r| r.session_id.clone())
                .is_some_and(|s| !s.is_empty())
        };
        if reachable {
            let mut r = Request::new(
                c.number,
                "answer",
                last.attempt(),
                last.role(),
                last.route(),
            );
            r.extra = PyMap(vec![("ruling".into(), Some(note))]);
            r.resume = Some(last.id().to_string());
            return request(st, c, r);
        }
    }
    let mut notes = PyStr::from(
        "## The operator's note\n\nThis card was parked; the operator resumed it with:\n\n",
    );
    notes.push(&note);
    start_attempt(st, obs, c, c.attempt.max(1), None, notes)
}

/// `_QUESTION.search(text).group(1).strip()`, else `""`: the section under the first
/// `# Question` heading (any level, any case), up to the next heading.
pub fn question(text: &str) -> String {
    for start in line_starts(text) {
        let rest = &text[start..];
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        if hashes == 0 {
            continue;
        }
        let rest = rest[hashes..].trim_start_matches(py_isspace);
        let Some((_, after)) = after_folded(rest, "question") else {
            continue;
        };
        // `\s*$`: the longest run of whitespace that ends at a line end or the text's end.
        let run: usize = after
            .chars()
            .take_while(|c| py_isspace(*c))
            .map(char::len_utf8)
            .sum();
        let end_of_head = (0..=run)
            .rev()
            .filter(|i| after.is_char_boundary(*i))
            .find(|i| after[*i..].is_empty() || after[*i..].starts_with('\n'));
        let Some(end_of_head) = end_of_head else {
            continue;
        };
        let group_start = text.len() - after.len() + end_of_head;
        let group_end = (group_start..=text.len())
            .filter(|i| text.is_char_boundary(*i))
            .find(|i| *i == text.len() || at_heading(text, *i))
            .unwrap_or(text.len());
        return py_strip(&text[group_start..group_end]).to_string();
    }
    String::new()
}

/// The starts of `text`'s lines: 0 and every position after a `\n` (`^` under `MULTILINE`).
pub(crate) fn line_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        text.bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i + 1),
    )
}

/// `^#+\s` at `at`.
fn at_heading(text: &str, at: usize) -> bool {
    if at != 0 && text.as_bytes()[at - 1] != b'\n' {
        return false;
    }
    let rest = &text[at..];
    let hashes = rest.bytes().take_while(|b| *b == b'#').count();
    hashes > 0 && rest[hashes..].chars().next().is_some_and(py_isspace)
}

// --------------------------------------------------------------------------
// The shell

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

/// `Path(path).read_text(encoding="utf-8", errors="replace")` (`engine._read_text`): `None`
/// for an empty name, a missing file or a folder; line ends translated, as text mode does.
pub(crate) fn read_text(path: &PyStr) -> Result<Option<String>> {
    if path.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(path.to_os()?);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(Error::io(&path, e)),
    };
    Ok(Some(
        String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n"),
    ))
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

pub(crate) fn dispatch_of<'l>(st: &'l RunState, id: &str) -> Result<Dispatch<'l>> {
    st.log
        .dispatch(Some(id))
        .ok_or_else(|| Error::msg(format!("no dispatch {id} was requested")))
}

// -- the supervisor's session --------------------------------------------------

/// `$CLAUDE_CODE_SESSION_ID`, stripped; `None` when unset or blank.
pub(crate) fn session_env() -> Option<PyStr> {
    let sid = std::env::var_os("CLAUDE_CODE_SESSION_ID")?;
    Some(strip_pystr(&PyStr::from_os(&sid))).filter(|s| !s.is_empty())
}

/// The supervisor session making this call: `CLAUDE_CODE_SESSION_ID`, else the last one
/// `run.json` recorded (`_current_session`).
pub(crate) fn current_session(run: &Run) -> Option<PyStr> {
    if let Some(sid) = session_env() {
        return Some(sid);
    }
    let meta = crate::dispatch::read_json_object(&run.paths.run_json).ok()??;
    crate::dispatch::field::<Vec<PyStr>>(&meta, "supervisor_sessions")?.pop()
}

/// Claude Code's transcript folder: `$CLAUDE_CONFIG_DIR/projects`, else
/// `~/.claude/projects` (`engine._projects_dir`, `usage.default_projects_dir`).
pub(crate) fn projects_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|h| !h.is_empty()) {
        Some(home) => PathBuf::from(crate::pytext::expand_user(&home)).join("projects"),
        None => PathBuf::from(crate::pytext::expand_user(OsStr::new("~"))).join(".claude/projects"),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config_from;
    use crate::state::decode_event;

    const DECK: &str = "# Deck: Demo deck\nSource: docs/plan.md\nBase: main @ BASE\n\
        Branch: leadhand/demo\nBuild: true\nTest: true\n\n\
        ## Card 1: Card number 1\n- Role: implementer\n- Hard: no\n- Risk: low\n\
        - Depends on: none\n- Allowed paths: src/, tests/\n\
        - Done when: the card's file exists and the tests pass\n- Stop if: the design has to change\n";

    /// A run of `DECK` with the given configuration, built without Git: the pure core never
    /// reaches the workspace.
    struct World {
        _dir: tempfile::TempDir,
        run: Run,
    }

    fn world(config_text: &str) -> World {
        let dir = tempfile::Builder::new()
            .prefix("lh core ")
            .tempdir()
            .unwrap();
        let root = dir.path().join("run");
        std::fs::create_dir_all(&root).unwrap();
        let base = "b".repeat(40);
        std::fs::write(root.join("deck.md"), DECK.replace("BASE", &base)).unwrap();
        let user = dir.path().join("user.toml");
        std::fs::write(&user, config_text).unwrap();
        let config = load_config_from(dir.path(), &user).unwrap();
        let paths = crate::state::run_paths(&root);
        let deck = crate::deck::parse_deck(&paths.deck).unwrap();
        let run = Run {
            root: root.clone(),
            paths,
            meta: PyMap::default(),
            run_id: Some("abc123abc123".into()),
            repo: dir.path().to_path_buf(),
            ws: dir.path().join("ws"),
            branch: "leadhand/demo".into(),
            base_sha: base,
            deck,
            config,
        };
        World { _dir: dir, run }
    }

    /// The events, each from `(kind, fields)`, numbered from 1.
    fn log(lines: &[(&str, &str)]) -> Log {
        let lines = lines
            .iter()
            .enumerate()
            .map(|(i, (kind, fields))| {
                let comma = if fields.is_empty() { "" } else { "," };
                let line = format!(
                    r#"{{"seq":{},"ts":"2026-11-14T00:00:00.000Z","kind":"{kind}"{comma}{fields}}}"#,
                    i + 1
                );
                decode_event(serde_json::from_str(&line).unwrap()).unwrap()
            })
            .collect();
        Log::new(lines)
    }

    fn obs(installed: &[&str]) -> Observations {
        Observations {
            now: 1_794_268_800.0,
            minute_seconds: 60.0,
            installed: installed.iter().map(|v| v.to_string()).collect(),
            run_tip: Some("b".repeat(40)),
            card_loop_only: true,
            cli: "/bin/leadhand".into(),
            operation: Some(None),
            ..Observations::default()
        }
    }

    const CUT: (&str, &str) = (
        "card_cut",
        r#""card":1,"branch":"leadhand/demo--card-1","base":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb""#,
    );

    fn requested(id: &str, kind: &str, attempt: i64, route: (&str, &str, &str)) -> String {
        format!(
            r#""dispatch_id":"{id}","card":1,"attempt":{attempt},"dispatch_kind":"{kind}","role":"implementer","vendor":"{}","model":"{}","effort":"{}","brief":"/ws/.leadhand/1/{id}/brief.md","report":"/ws/.leadhand/1/{id}/report.md","extra":{{}},"notes":"","resumes":null,"resume_worker":null,"resume_session":null,"minutes":60,"refs":{{"refs/heads/leadhand/demo":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},"ref_scope":["refs/heads/leadhand/demo"],"start_tip":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","untracked":[]"#,
            route.0, route.1, route.2
        )
    }

    fn started(id: &str, worker: &str) -> String {
        format!(
            r#""dispatch_id":"{id}","worker_id":"{worker}","card":1,"deadline":"2026-11-14T01:00:00Z","session":null"#
        )
    }

    fn result(id: &str, kind: &str, status: &str, extra: &str) -> String {
        format!(
            r#""dispatch_id":"{id}","card":1,"attempt":1,"dispatch_kind":"{kind}","worker_id":"w","tip":"cccccccccccccccccccccccccccccccccccccccc","session_id":null,"files":{{"report":"/run/cards/1/{id}/report.md","reply":"/run/cards/1/{id}/reply.md"}},"stopped":false,"charge_ids":[],"status":"{status}","blocked_by":"","head":"Status","verdict":null,"result":null,"source":"report","raw_kept":false{extra}"#
        )
    }

    /// Decides with every guard observed clean.
    fn decide_clean(st: &RunState, obs: &mut Observations) -> Decision {
        loop {
            match decide(st, obs) {
                Decision::Need(Need::Guard(id)) => {
                    obs.guards.insert(id, false);
                }
                d => return d,
            }
        }
    }

    const SONNET: (&str, &str, &str) = ("claude", "sonnet", "medium");

    #[test]
    fn decide_requests_review_after_ready() {
        let w = world("");
        let impl_req = requested("c1-a1-implement-1", "implement", 1, SONNET);
        let impl_start = started("c1-a1-implement-1", "agent0001");
        let impl_result = result("c1-a1-implement-1", "implement", "READY", "");
        let st = RunState::new(
            &w.run,
            log(&[
                ("run_started", ""),
                CUT,
                ("dispatch_requested", &impl_req),
                ("worker_started", &impl_start),
                ("result_recorded", &impl_result),
            ]),
        );
        let mut o = obs(&["claude"]);
        let Decision::Request(r) = decide_clean(&st, &mut o) else {
            panic!("{:?}", decide_clean(&st, &mut o));
        };
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.kind.as_str(),
                r.attempt,
                r.role.as_str()
            ),
            ("c1-a1-review-1", "review", 1, "reviewer")
        );
        assert_eq!(r.route.vendor, "claude");
        let base = "b".repeat(40);
        let tip = "c".repeat(40);
        assert_eq!(
            r.extra.get(&"diff_range".into()).cloned().flatten(),
            Some(PyStr::from(format!("{base}..{tip}")))
        );
        assert_eq!(
            r.extra.get(&"implementer_report".into()).cloned().flatten(),
            Some(PyStr::from("/run/cards/1/c1-a1-implement-1/report.md"))
        );
        // The implementer's guard is checked first, and its operation looked for.
        let mut fresh = obs(&["claude"]);
        assert_eq!(
            decide(&st, &fresh),
            Decision::Need(Need::Guard("c1-a1-implement-1".into()))
        );
        fresh.guards.insert("c1-a1-implement-1".into(), false);
        fresh.operation = None;
        assert_eq!(decide(&st, &fresh), Decision::Need(Need::Operation));
        // A worker that moved what it guards is Task 15's.
        fresh.guards.insert("c1-a1-implement-1".into(), true);
        assert!(matches!(
            decide(&st, &fresh),
            Decision::Unported { task: 15, .. }
        ));
    }

    #[test]
    fn decide_escalates_after_defects_post_fix_round() {
        let w = world("");
        let review_req = requested("c1-a1-review-1", "review", 1, SONNET)
            .replace(r#""role":"implementer""#, r#""role":"reviewer""#);
        let fix_req = requested("c1-a1-fix-1", "fix", 1, SONNET);
        let rereview_req = requested("c1-a1-rereview-1", "rereview", 1, SONNET)
            .replace(r#""role":"implementer""#, r#""role":"reviewer""#);
        let defects = |id: &str| {
            result(id, "review", "READY", "").replace(
                r#""head":"Status","verdict":null"#,
                r#""head":"Verdict","verdict":"defects""#,
            )
        };
        let verdict = |id: &str| {
            format!(
                r#""card":1,"verdict":"defects","findings":"/run/cards/1/{id}/report.md","dispatch_id":"{id}""#
            )
        };
        let lines = [
            CUT,
            (
                "dispatch_requested",
                &requested("c1-a1-implement-1", "implement", 1, SONNET),
            ),
            ("worker_started", &started("c1-a1-implement-1", "w1")),
            (
                "result_recorded",
                &result("c1-a1-implement-1", "implement", "READY", ""),
            ),
            ("dispatch_requested", &review_req),
            ("worker_started", &started("c1-a1-review-1", "w2")),
            ("result_recorded", &defects("c1-a1-review-1")),
            ("verdict_recorded", &verdict("c1-a1-review-1")),
            (
                "fix_round",
                r#""card":1,"n":1,"attempt":1,"pre_tip":"cccccccccccccccccccccccccccccccccccccccc","findings":"/run/cards/1/c1-a1-review-1/report.md","review":"c1-a1-review-1""#,
            ),
            ("dispatch_requested", &fix_req),
            ("worker_started", &started("c1-a1-fix-1", "w1")),
            (
                "result_recorded",
                &result("c1-a1-fix-1", "fix", "READY", ""),
            ),
            ("dispatch_requested", &rereview_req),
            ("worker_started", &started("c1-a1-rereview-1", "w2")),
            ("result_recorded", &defects("c1-a1-rereview-1")),
            ("verdict_recorded", &verdict("c1-a1-rereview-1")),
        ];
        let st = RunState::new(&w.run, log(&lines));
        assert_eq!(st.cards[&1].state, CardStage::Reviewed);
        let Decision::Request(r) = decide_clean(&st, &mut obs(&["claude"])) else {
            panic!("no request");
        };
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.kind.as_str(),
                r.attempt,
                r.resume.as_deref()
            ),
            ("c1-a2-implement-1", "implement", 2, None)
        );
        assert_eq!(
            (
                r.route.vendor.as_str(),
                r.route.model.as_str(),
                r.route.effort.as_str()
            ),
            ("claude", "sonnet", "high")
        );
        let notes = r.notes.lossy().into_owned();
        assert!(notes.starts_with("## Previous attempt\n\nAttempt 1 on this card still had review defects after its fix round. "), "{notes}");
        assert!(
            notes.contains("- Its report: /run/cards/1/c1-a1-fix-1/report.md"),
            "{notes}"
        );
        assert!(
            notes.contains("- The review findings: /run/cards/1/c1-a1-rereview-1/report.md"),
            "{notes}"
        );
    }

    #[test]
    fn decide_parks_on_exhausted_ladder() {
        let w = world("");
        let mut owned: Vec<(&str, String)> = vec![(CUT.0, CUT.1.to_string())];
        for attempt in 1..=4 {
            let id = format!("c1-a{attempt}-implement-1");
            owned.push((
                "dispatch_requested",
                requested(&id, "implement", attempt, SONNET),
            ));
            owned.push(("worker_started", started(&id, &format!("w{attempt}"))));
            let spent = result(&id, "implement", "SPENT", "")
                .replace(r#""attempt":1"#, &format!(r#""attempt":{attempt}"#));
            owned.push(("result_recorded", spent));
        }
        let lines: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let st = RunState::new(&w.run, log(&lines));
        // Attempt 4 ended SPENT: attempt 5 exhausts the ladder.
        let decision = decide_clean(&st, &mut obs(&["claude", "codex"]));
        let Decision::Append(event) = decision else {
            panic!("{decision:?}");
        };
        let Event::Parked(p) = *event else {
            panic!("not a park: {event:?}");
        };
        assert_eq!(
            (p.card, p.reason),
            (
                Some(CardRef::Number(1)),
                Some(PyStr::from("ladder exhausted"))
            )
        );
        assert_eq!(p.decision, None);
    }

    #[test]
    fn decide_blocks_on_usage_limit_without_switching_vendor() {
        let w = world("[roles.implementer]\nprefer = \"claude\"\n");
        let id = "c1-a1-implement-1";
        let st = RunState::new(
            &w.run,
            log(&[
                CUT,
                ("dispatch_requested", &requested(id, "implement", 1, SONNET)),
                ("worker_started", &started(id, "w1")),
                (
                    "result_recorded",
                    &result(id, "implement", "BLOCKED", "")
                        .replace(r#""blocked_by":"""#, r#""blocked_by":"usage limit""#),
                ),
            ]),
        );
        let decision = decide_clean(&st, &mut obs(&["claude", "codex"]));
        assert_eq!(
            decision,
            Decision::End(Ending {
                blocked: Some(Blocked::ProviderCapacity),
                ..Ending::default()
            })
        );
        // A second BLOCKED for another reason is retried once on the same route, the same
        // attempt.
        let st = RunState::new(
            &w.run,
            log(&[
                CUT,
                ("dispatch_requested", &requested(id, "implement", 1, SONNET)),
                ("worker_started", &started(id, "w1")),
                (
                    "result_recorded",
                    &result(id, "implement", "BLOCKED", "")
                        .replace(r#""blocked_by":"""#, r#""blocked_by":"environment""#),
                ),
            ]),
        );
        let Decision::Request(r) = decide_clean(&st, &mut obs(&["claude", "codex"])) else {
            panic!("no retry");
        };
        assert_eq!(
            (r.dispatch_id.as_str(), r.attempt, r.route.vendor.as_str()),
            ("c1-a1-implement-2", 1, "claude")
        );
    }

    #[test]
    fn decide_dispatches_waits_and_stops_on_the_clock() {
        let w = world("");
        let id = "c1-a1-implement-1";
        let req = requested(id, "implement", 1, SONNET);
        let st = RunState::new(&w.run, log(&[CUT, ("dispatch_requested", &req)]));
        let o = obs(&["claude"]);
        let Decision::Act(Action::Dispatch(a)) = decide(&st, &o) else {
            panic!("no dispatch");
        };
        assert_eq!(
            (
                a.agent.as_str(),
                a.agent_model.as_deref(),
                a.deadline.as_str()
            ),
            (
                "leadhand:implementer",
                Some("sonnet"),
                "2026-11-10T01:00:00Z"
            )
        );
        assert!(
            a.message
                .lossy()
                .starts_with("Brief: /ws/.leadhand/1/c1-a1-implement-1/brief.md\n")
        );
        let start = started(id, "agent0001");
        let st = RunState::new(
            &w.run,
            log(&[
                CUT,
                ("dispatch_requested", &req),
                ("worker_started", &start),
            ]),
        );
        let mut o = obs(&["claude"]);
        o.now = parse_time("2026-11-14T00:59:59Z").unwrap();
        assert!(matches!(decide(&st, &o), Decision::Act(Action::Wait(_))));
        o.now += 2.0;
        let Decision::Act(Action::StopWorker(stop)) = decide(&st, &o) else {
            panic!("no stop");
        };
        assert_eq!((stop.vendor, stop.record_result), ("claude", true));
    }

    #[test]
    fn decide_asks_the_supervisor_a_workers_question() {
        let w = world("");
        let id = "c1-a1-implement-1";
        let req = requested(id, "implement", 1, SONNET);
        let start = started(id, "agent0001");
        let asked = result(id, "implement", "QUESTION", "");
        let lines = [
            CUT,
            ("dispatch_requested", req.as_str()),
            ("worker_started", start.as_str()),
            ("result_recorded", asked.as_str()),
        ];
        let st = RunState::new(&w.run, log(&lines));
        let mut o = obs(&["claude"]);
        let report = PyStr::from(format!("/run/cards/1/{id}/report.md"));
        assert_eq!(
            decide_clean(&st, &mut o),
            Decision::Need(Need::Text(report.clone()))
        );
        o.texts.insert(
            report.clone(),
            Some("Status: QUESTION\n## Question\nJSON or SQLite?\n## Concerns\nNone\n".into()),
        );
        let Decision::Act(Action::Answer(a)) = decide_clean(&st, &mut o) else {
            panic!("no answer");
        };
        assert_eq!(
            (a.card, a.dispatch_id.as_str(), a.question.lossy().as_ref()),
            (1, id, "JSON or SQLite?")
        );
        // The ruling resumes the worker that asked.
        let ruling = format!(r#""card":1,"dispatch_id":"{id}","text":"Use JSON.""#);
        let lines = [
            CUT,
            ("dispatch_requested", req.as_str()),
            ("worker_started", start.as_str()),
            ("result_recorded", asked.as_str()),
            ("ruling", ruling.as_str()),
        ];
        let st = RunState::new(&w.run, log(&lines));
        let Decision::Request(r) = decide_clean(&st, &mut obs(&["claude"])) else {
            panic!("no answer request");
        };
        assert_eq!(
            (r.dispatch_id.as_str(), r.resume.as_deref()),
            ("c1-a1-answer-1", Some(id))
        );
        assert_eq!(
            r.extra.get(&"ruling".into()).cloned().flatten(),
            Some(PyStr::from("Use JSON."))
        );
    }

    #[test]
    fn decide_checks_a_test_only_fix_and_rereviews_a_production_one() {
        let w = world("");
        let fix_round = r#""card":1,"n":1,"attempt":1,"pre_tip":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","findings":"/run/cards/1/c1-a1-review-1/report.md","review":"c1-a1-review-1""#;
        let review_req = requested("c1-a1-review-1", "review", 1, SONNET)
            .replace(r#""role":"implementer""#, r#""role":"reviewer""#);
        let lines = [
            CUT,
            (
                "dispatch_requested",
                &requested("c1-a1-implement-1", "implement", 1, SONNET),
            ),
            ("worker_started", &started("c1-a1-implement-1", "w1")),
            (
                "result_recorded",
                &result("c1-a1-implement-1", "implement", "READY", ""),
            ),
            ("dispatch_requested", &review_req),
            ("worker_started", &started("c1-a1-review-1", "w2")),
            ("fix_round", fix_round),
            (
                "dispatch_requested",
                &requested("c1-a1-fix-1", "fix", 1, SONNET),
            ),
            ("worker_started", &started("c1-a1-fix-1", "w1")),
            (
                "result_recorded",
                &result("c1-a1-fix-1", "fix", "READY", ""),
            ),
        ];
        let st = RunState::new(&w.run, log(&lines));
        let mut o = obs(&["claude"]);
        let key = ("a".repeat(40), "c".repeat(40));
        assert_eq!(
            decide_clean(&st, &mut o),
            Decision::Need(Need::Changed(key.0.clone(), key.1.clone()))
        );
        let paths = vec![PyStr::from("tests/test_x.py"), PyStr::from("docs/n.md")];
        o.changed.insert(key.clone(), paths.clone());
        let Decision::CheckDiff(check) = decide_clean(&st, &mut o) else {
            panic!("no diff check");
        };
        assert_eq!(
            (check.dispatch_id.as_str(), check.changed),
            ("c1-a1-fix-1", paths)
        );
        // A production path gets the reviewer back, resumed, on the fix's diff.
        o.changed.insert(key, vec![PyStr::from("src/main.py")]);
        let Decision::Request(r) = decide_clean(&st, &mut o) else {
            panic!("no rereview");
        };
        assert_eq!(
            (r.dispatch_id.as_str(), r.role.as_str(), r.resume.as_deref()),
            ("c1-a1-rereview-1", "reviewer", Some("c1-a1-review-1"))
        );
    }

    #[test]
    fn decide_accepts_a_low_risk_card_by_risk_only_when_it_changed_something() {
        let w = world("[run]\ncard_review = \"by-risk\"\n");
        let id = "c1-a1-implement-1";
        let req = requested(id, "implement", 1, SONNET);
        let start = started(id, "w1");
        let ready = result(id, "implement", "READY", "");
        let lines = [
            CUT,
            ("dispatch_requested", req.as_str()),
            ("worker_started", start.as_str()),
            ("result_recorded", ready.as_str()),
        ];
        let st = RunState::new(&w.run, log(&lines));
        assert_eq!(
            decide_clean(&st, &mut obs(&["claude"])),
            Decision::Accept {
                card: 1,
                target: Some("c".repeat(40))
            }
        );
        // READY at the attempt's start tip changed nothing: it goes to review (I8).
        let unchanged = ready.replace(&"c".repeat(40), &"b".repeat(40));
        let lines = [
            CUT,
            ("dispatch_requested", req.as_str()),
            ("worker_started", start.as_str()),
            ("result_recorded", unchanged.as_str()),
        ];
        let st = RunState::new(&w.run, log(&lines));
        let Decision::Request(r) = decide_clean(&st, &mut obs(&["claude"])) else {
            panic!("no review");
        };
        assert_eq!(r.kind, "review");
    }

    #[test]
    fn questions_are_read_as_the_python_reads_them() {
        let texts = [
            "Status: QUESTION\n## Summary\nx\n## Question\nWhich storage format should widgets use?\nOptions:\n- A: JSON\n## Concerns\nNone\n",
            "# question  \n\n  Which one?  \n",
            "### QUESTION\nA?\n#Not a heading\n## Next\nB",
            "## Questions\nno\n",
            "## Queſtion\nfolded\n",
            "##Question\r\nCRLF?\r\n## End\n",
            "no question here",
            "## Question\n## Empty\n",
        ];
        let code = "import json, sys\nfrom leadhand import engine\n\
                    out = []\n\
                    for t in json.loads(sys.argv[1]):\n\
                    \x20   m = engine._QUESTION.search(t)\n\
                    \x20   out.append(m.group(1).strip() if m else '')\n\
                    print(json.dumps(out))";
        let arg = serde_json::to_string(&texts).unwrap();
        let theirs: Vec<String> =
            serde_json::from_slice(&crate::wire::tests::python(code, &[OsStr::new(&arg)])).unwrap();
        let ours: Vec<String> = texts.iter().map(|t| question(t)).collect();
        assert_eq!(ours, theirs);
    }
}
