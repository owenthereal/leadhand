//! `next`'s core (`engine.next_action`): fold the events, then decide the next step.
//!
//! `decide(state, observations)` is pure: it reads the folded log (`RunState`) and what the
//! shell observed of the world (`Observations`: the clock, the run branch's tip, the installed
//! vendors, …) and returns one `Decision`: a mechanical step for the shell to carry out (cut a
//! card branch, request a dispatch, append an event, accept a card), an action for the
//! supervisor, or a request for one more observation (`Decision::Need`: a Codex result, the
//! paths a fix changed, a report's text, whether a worker left what it guards as it found it),
//! which the shell gathers before deciding again. The shell (`engine::shell`) does the I/O: it
//! loops step after step, as the Python does, until a step yields an action.
//!
//! Integration (rebasing a card onto the run tip, the integrate attempt after a conflict, the
//! re-review and the verify of an integrated card) and violations (a check that changed what
//! it guards, an implementer's leftovers) are decided here; their Git work is the shell's, in
//! `integrate` and `violations`. A step that spans I/O and a decision in the Python is two
//! steps here: the I/O first (recording a violation and restoring it, clearing leftovers),
//! then, on the next step, what follows from what it recorded.
//!
//! The end of a run is decided in two child modules: `gates` (the commit-bound end-of-run gates,
//! the fix waves they get, and which outcome the run ends with) and `run_level` (what follows
//! a gate's or a fix wave's result). Carrying out the outcome, the branch release and the
//! report is the shell's (`finish`, `release`). Recovery is Task 17's: where the loop reaches
//! it, `decide` returns `Decision::Unported`, and `next` fails with `not ported yet: …` rather
//! than guess.

mod gates;
mod run_level;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;

pub(super) use gates::waiting_on_decisions;

use super::cards::{
    Dispatch, Log, Run, active, card_branch, heads, is_card, pick, report_copy, short_sha,
    work_branch,
};
use super::protocol::{
    Action, AnswerAction, DispatchAction, IMPLEMENTER_KINDS, REPLY_FILE, REVIEW_KINDS, Resume,
    RouteView, StopWorkerAction, VIOLATION_CAP, WaitAction, budget_seconds, card_dispatch_id,
    format_time, parse_time, tag, voided_notes,
};
use super::report_text::question;
use crate::briefs::{CODEX_AGENT, agent_for, scratch_dir};
use crate::briefs_write::dispatch_message;
use crate::config::Route;
use crate::dispatch::DispatchResult;
use crate::error::Error;
use crate::git::cards::is_test_or_doc_path;
use crate::pytext::path_text;
use crate::route::{RouteError, RouteRequest, resolve};
use crate::state::events::{self, given};
use crate::state::{Blocked, CardRef, CardStage, CardState, Event};
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
    /// A dispatch's guard checked, by dispatch ID: the `found` violation to record when its
    /// worker changed what the dispatch guards, `None` when it left it as it found it.
    pub guards: HashMap<String, Option<events::Violation>>,
    /// Whether the run tip is not in a card branch's history, by the card branch.
    pub needs_integration: HashMap<PyStr, bool>,
    /// Whether a branch's tip has a commit in its history, by (branch, commit).
    pub contains: HashMap<(PyStr, String), bool>,
    /// `git merge-base a b`, by (a, b).
    pub merge_bases: HashMap<(PyStr, PyStr), String>,
    /// A branch's tip, by branch.
    pub tips: HashMap<PyStr, String>,
    /// An evidence dispatch's base worktree checked, by dispatch ID: the `found` violation to
    /// record when its worker changed the base worktree, `None` when it left it as it was.
    pub base_guards: HashMap<String, Option<events::Violation>>,
    /// Whether a ref's tip is in a commit's history, by (ref, commit) (`is_ancestor`).
    pub ancestors: HashMap<(PyStr, String), bool>,
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
    /// Whether the run tip is not in this card branch's history (`needs_integration`).
    NeedsIntegration(PyStr),
    /// Whether this branch's tip has this commit in its history (`_contains`).
    Contains(PyStr, String),
    /// The merge base of two revisions.
    MergeBase(PyStr, PyStr),
    /// This branch's tip.
    Tip(PyStr),
    /// Whether an evidence dispatch's worker left the evidence base worktree at the base
    /// commit with no tracked change (`_base_violation`'s check).
    BaseGuard(String),
    /// Whether this ref's tip is in this commit's history (`is_ancestor(ref, commit)`).
    Ancestor(PyStr, String),
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

/// How the run ends (`engine._end`'s arguments); the shell decides the outcome from them and
/// from what Git shows of the run branch (`finish::end`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ending {
    pub blocked: Option<Blocked>,
    pub why: Vec<PyStr>,
    /// A mismatch between the run branch and what the run accepted.
    pub unreconciled: Option<String>,
    /// A review gate failed on the run head for good.
    pub open_findings: bool,
    /// The evidence gaps the run holds (`evidence = "record"`).
    pub holds: Vec<PyStr>,
    /// Every required gate passed on the run head.
    pub gates_current: bool,
    /// The verify gate failed on the run head for good.
    pub verification_failed: bool,
    /// Stop every Codex dispatch without a result first (`_stop_codex_runners`), so no
    /// detached runner commits after the outcome.
    pub stop_codex: bool,
}

/// A run-level dispatch to request (`engine._request_run`): a gate, or a fix wave's
/// implementer or review. The shell checks out the run branch (a gate) or the final branch (a
/// wave), takes the dispatch's guard, prepares the evidence base worktree for an evidence
/// gate, records `gate_started` for a gate, writes the brief and appends `dispatch_requested`.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRequest {
    /// `run-<kind>-<n>`, or `run-w<wave>-<kind>-<n>`: deterministic, as a card's.
    pub dispatch_id: String,
    pub kind: String,
    pub gate: Option<String>,
    pub wave: Option<i64>,
    pub attempt: i64,
    pub role: String,
    pub route: Route,
    /// The kind's brief values; recorded in the event.
    pub extra: PyMap<Option<PyStr>>,
    pub notes: PyStr,
    /// The accepted cards, in dependency order: the brief's `cards`.
    pub cards: Vec<u32>,
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
    /// Void a check dispatch whose worker changed what it guards (`_void`): record `found`
    /// when given, then keep what the worker changed, put it back and record `restored`,
    /// unless that is recorded.
    Void {
        dispatch_id: String,
        found: Option<Box<events::Violation>>,
    },
    /// An implementer dispatch left a Git operation in progress (`_left_operation`): keep and
    /// clear it as leftovers, which makes the dispatch BLOCKED (environment).
    LeftOperation { dispatch_id: String, branch: PyStr },
    /// Keep and clear every implementer leftovers recorded and not restored yet
    /// (`_finish_leftovers`).
    FinishLeftovers,
    /// Integrate the card with the run tip: `integration_started`, then the rebase (`_accept`).
    Integrate { card: u32 },
    /// Rebase the integrating card again from its latest `integration_started`
    /// (`_integration_step` after a crash).
    Rebase { card: u32 },
    /// End the run: decide its outcome and record it (`_end`).
    End(Ending),
    /// The run has an outcome: release its branch (a terminal outcome), write the handoff, the
    /// report and the `runs.md` line once, then return `done` (`_wrap_up`, `_done_action`).
    WrapUp,
    /// Request a run-level dispatch.
    RunRequest(Box<RunRequest>),
    /// Cut the final branch for fix wave `wave` at the run head `head`, keeping an earlier
    /// wave's unaccepted commits (`_start_wave`'s cut).
    CutFinal { wave: i64, head: String },
    /// Move the final branch back to the run head for the fresh re-run of a voided fix wave
    /// implementer (`_recut_final`, R36).
    RecutFinal { dispatch_id: String },
    /// Record a gate's result (`gate_result`), past the crash point `before_gate_record`.
    RecordGate(Box<Event>),
    /// Fast-forward the run branch to a fix wave's reviewed tip: intent (unless `started`),
    /// move, completion (`_after_run_result`'s acceptance).
    AcceptFix {
        dispatch_id: String,
        wave: Option<i64>,
        target: String,
        started: bool,
    },
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
        return Ok(Decision::WrapUp);
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
            stop_codex: true,
            ..Ending::default()
        }));
    }
    if !st.log.pending_leftovers().is_empty() {
        return Ok(Decision::FinishLeftovers);
    }
    if let Some(last) = st.log.run_dispatches().last() {
        if last.result.is_none() {
            // A gate or fix wave worker is using the workspace (a card unparked meanwhile
            // waits).
            return in_flight(st, obs, last);
        }
        // Its result is acted on before any card moves the workspace or the run branch, so
        // its guard is checked against what the worker saw.
        if let Some(decision) = run_level::after_run_result(st, obs, last)? {
            return Ok(decision);
        }
    }
    let Some(n) = pick(st.run, &st.log, &st.cards)? else {
        return gates::end_of_run(st, obs);
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
        CardStage::Integrating => integration_step(st, obs, c),
        CardStage::Reviewed => after_verdict(st, obs, c),
        CardStage::Fixing if c.dispatch_kind.as_deref() != Some("fix") => request_fix(st, obs, c),
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
    head_problem(st.run, &st.log, obs.run_tip.as_deref())
}

/// `_run_head_problem` for the run tip `run_tip` (`None`: the workspace or the branch is
/// missing).
pub(super) fn head_problem(run: &Run, log: &Log, run_tip: Option<&str>) -> Option<String> {
    let branch = run.branch.shown();
    let Some(now) = run_tip else {
        return Some(format!("the run branch {branch} is missing"));
    };
    let accepted: Vec<&events::Accepted> = log
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
        .unwrap_or_else(|| run.base_sha.clone());
    if now == expected {
        return None;
    }
    let after = accepted.last().map_or(0, |a| a.seq);
    let accepting = log.events.iter().any(|e| {
        matches!(e, Event::AcceptStarted(a) if a.seq > after && a.target.as_deref() == Some(now))
    });
    if accepting {
        return None;
    }
    Some(format!(
        "the run branch {branch} is at {}, not at the last accepted commit {}{}",
        short_sha(now),
        short_sha(&expected),
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

/// The commit the card's work starts from: its cut base, or, once it has been integrated,
/// where its branch meets the run branch, the run tip it was rebased onto (`_card_base`).
fn card_base(st: &RunState, obs: &Observations, c: &CardState) -> Step<String> {
    if let Some(branch) = c.branch.as_ref().filter(|b| !b.is_empty())
        && integrated(st, c.number)
    {
        let key = (st.run.run_ref(), heads(branch));
        return match obs.merge_bases.get(&key) {
            Some(base) => Ok(base.clone()),
            None => need(Need::MergeBase(key.0, key.1)),
        };
    }
    Ok(c.base.clone().unwrap_or_default())
}

/// Whether the card was ever integrated (an `integration_started` for it).
fn integrated(st: &RunState, card: u32) -> bool {
    st.log
        .events
        .iter()
        .any(|e| matches!(e, Event::IntegrationStarted(i) if is_card(i.card.as_ref(), card)))
}

/// The request, with its deterministic dispatch ID and the card's base.
fn request(st: &RunState, obs: &Observations, c: &CardState, mut r: Request) -> Step<Decision> {
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
    r.base = card_base(st, obs, c)?;
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
    request(st, obs, c, r)
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
    let notes = PyStr::join(&lines, "\n");
    start_attempt(st, obs, c, attempt + 1, Some(last.vendor()), notes)
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
    let base = card_base(st, obs, c)?;
    let mut r = Request::new(c.number, "review", implementer.attempt(), &role, route);
    r.extra = extra(vec![
        (
            "diff_range",
            format!("{base}..{}", tip.as_deref().unwrap_or("None")).into(),
        ),
        ("implementer_report", report_copy(implementer)),
    ]);
    request(st, obs, c, r)
}

/// The fix round's implementer, resumed with the findings (`_request_fix`).
fn request_fix(st: &RunState, obs: &Observations, c: &CardState) -> Step<Decision> {
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
    request(st, obs, c, r)
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
        let whose = d.whose();
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

/// The scratch folder's card component of a dispatch.
fn scratch_card(d: &Dispatch) -> Option<i64> {
    d.req.card.as_ref().and_then(CardRef::number)
}

/// Where the supervisor writes the worker's reply (`_reply_file`, R38).
fn reply_file(st: &RunState, d: &Dispatch) -> Step<PyStr> {
    let folder = scratch_dir(&st.run.ws, scratch_card(d), d.id())?;
    Ok(path_text(&folder.join(REPLY_FILE)))
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
    if d.req.refs.is_some() && !workspace_kept(st, obs, d)? {
        // The result is void: what the worker changed was kept and undone, and the same kind
        // is requested again (or the card parked).
        return after_void(st, obs, c, d);
    }
    if IMPLEMENTER_KINDS.contains(&d.kind()) {
        // A rebase, merge or cherry-pick left in progress is the worker's failure: its state
        // is kept and cleared, and the dispatch counts as BLOCKED (environment).
        if st.log.broken_dispatches().contains(d.id()) {
            return worker_failed(st, obs, c, d);
        }
        if let Some(branch) = c.branch.as_ref().filter(|b| !b.is_empty()) {
            match &obs.operation {
                None => return need(Need::Operation),
                Some(Some(_)) => {
                    return Ok(Decision::LeftOperation {
                        dispatch_id: d.id().to_string(),
                        branch: branch.clone(),
                    });
                }
                Some(None) => {}
            }
        }
    }
    if given(&r.interrupted).copied().unwrap_or(false) {
        return unported("continuing after an interrupted worker", 17);
    }
    if d.kind() == "verify" {
        return match r.result.as_deref() {
            Some("pass") => {
                // `d.req["extra"]["tip"]`.
                let Some(tip) = d.extra().get(&PyStr::from("tip")).cloned() else {
                    return fail("KeyError: 'tip'".into());
                };
                accept(st, obs, c.number, tip.map(|t| t.lossy().into_owned()))
            }
            Some("fail") => escalate(
                st,
                obs,
                c,
                "failed the deck's checks after integration".into(),
                Some(d),
                "The verifier's report",
            ),
            _ => worker_failed(st, obs, c, d),
        };
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
                return after_integrate(st, obs, c, d);
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
                return accept(st, obs, c.number, tip);
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
            request(st, obs, c, r)
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
fn worker_failed(st: &RunState, obs: &Observations, c: &CardState, d: &Dispatch) -> Step<Decision> {
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
    request(st, obs, c, again)
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
        return accept(st, obs, c.number, c.tip.clone());
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
        let base = card_base(st, obs, c)?;
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
        return request(st, obs, c, r);
    }
    let check = st.log.events.iter().find_map(|e| match e {
        Event::DiffChecked(k) if k.dispatch_id.as_deref() == Some(d.id()) => Some(k),
        _ => None,
    });
    if let Some(check) = check {
        if check.ok == Some(true) {
            return accept(st, obs, c.number, tip);
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

// -- violations ------------------------------------------------------------------

/// Whether the worker left what its dispatch guards as it found it (`_workspace_kept`'s
/// `_void`): for a reviewer or verifier the card tip, a clean workspace and the run's refs; for
/// an implementer-side worker the run branch. A change found now, or one recorded and not
/// restored yet (a crash between the two phases), is recorded, kept and undone first
/// (`Decision::Void`); then the result is void, `false`.
fn workspace_kept(st: &RunState, obs: &Observations, d: &Dispatch) -> Step<bool> {
    let mine: Vec<&events::Violation> = st
        .log
        .check_violations()
        .filter(|v| given(&v.dispatch_id).map(String::as_str) == Some(d.id()))
        .collect();
    let stage = |s: &str| mine.iter().any(|v| v.stage.as_deref() == Some(s));
    if mine.is_empty() {
        return match obs.guards.get(d.id()) {
            None => need(Need::Guard(d.id().to_string())),
            Some(None) => Ok(true),
            Some(Some(found)) => Err(Box::new(Decision::Void {
                dispatch_id: d.id().to_string(),
                found: Some(Box::new(found.clone())),
            })),
        };
    }
    if !stage("found") {
        // `next(e for e in mine if e.get("stage") == "found")`.
        return fail("StopIteration".into());
    }
    if !stage("restored") {
        return Err(Box::new(Decision::Void {
            dispatch_id: d.id().to_string(),
            found: None,
        }));
    }
    Ok(false)
}

/// What follows a voided result (`_workspace_kept`): the same kind again on the same route,
/// with a fresh worker for a check (R33) and the voided dispatch's resume chain for an
/// implementer-side step, the warning added to its brief. After `VIOLATION_CAP` voided runs of
/// the kind since the card was last unparked, the card is parked instead, and unparking it
/// re-runs the kind (R32).
fn after_void(st: &RunState, obs: &Observations, c: &CardState, d: &Dispatch) -> Step<Decision> {
    let since = st
        .log
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Unparked(u) if is_card(u.card.as_ref(), c.number) => Some(u.seq),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let count: Vec<&events::Violation> = st
        .log
        .check_violations()
        .filter(|v| {
            v.stage.as_deref() == Some("found")
                && is_card(given(&v.card), c.number)
                && given(&v.dispatch_kind).map(String::as_str) == Some(d.kind())
                && v.seq > since
        })
        .collect();
    if count.len() > VIOLATION_CAP {
        // `sorted({...})`: WTF-8 order is code point order.
        let mut paths: Vec<PyStr> = count
            .iter()
            .flat_map(|v| given(&v.paths).into_iter().flatten())
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();
        let mut refs: Vec<PyStr> = count
            .iter()
            .flat_map(|v| given(&v.refs).into_iter().flat_map(|r| r.0.iter()))
            .map(|(name, _)| name.clone())
            .collect();
        refs.sort();
        refs.dedup();
        let mut reason = if paths.is_empty() {
            PyStr::from(format!("{} workers move refs: ", d.kind()))
        } else {
            PyStr::from("checks change tracked files: ")
        };
        reason.push(&PyStr::join(
            if paths.is_empty() { &refs } else { &paths },
            ", ",
        ));
        return Ok(append(Event::Parked(events::Parked {
            card: Some(CardRef::Number(c.number.into())),
            reason: Some(reason),
            cap_dispatch: Some(Some(d.id().to_string())),
            cap_kind: Some(Some(d.kind().to_string())),
            ..events::Parked::default()
        })));
    }
    let mut r = Request::new(c.number, d.kind(), d.attempt(), d.role(), d.route());
    r.extra = d.extra();
    r.notes = voided_notes(&d.notes());
    if IMPLEMENTER_KINDS.contains(&d.kind()) {
        r.resume = st
            .log
            .dispatch(d.req.resumes.as_deref())
            .map(|x| x.id().to_string());
    }
    request(st, obs, c, r)
}

// -- acceptance and integration --------------------------------------------------------

/// Accepts a card at `target` (`_accept`): a card branch that does not contain the run tip is
/// integrated first, and a card that was integrated is accepted only at a tip a verify passed
/// on (a verify of it is requested otherwise). Then the run branch is fast-forwarded to it.
fn accept(st: &RunState, obs: &Observations, card: u32, target: Option<String>) -> Step<Decision> {
    let Some(target) = target.filter(|t| !t.is_empty()) else {
        return fail(format!("card {card}: no tip to accept"));
    };
    // `fold_cards(events, ctx.numbers)[card]`.
    let Some(c) = st.cards.get(&card) else {
        return fail(format!("KeyError: {card}"));
    };
    let branch = card_branch(st.run, c);
    match obs.needs_integration.get(&branch) {
        None => return need(Need::NeedsIntegration(branch)),
        Some(true) => return Ok(Decision::Integrate { card }),
        Some(false) => {}
    }
    if integrated(st, card) && !verified(st, card, &target) {
        return request_verify(st, obs, c, target);
    }
    Ok(Decision::Accept {
        card,
        target: Some(target),
    })
}

/// Whether a verify of exactly `target` passed, and was not voided (`_verified`).
fn verified(st: &RunState, card: u32, target: &str) -> bool {
    let voided = st.log.voided();
    let target = PyStr::from(target);
    st.log.dispatches().any(|d| {
        d.kind() == "verify"
            && d.is_card(card)
            && d.extra().get(&PyStr::from("tip")).cloned().flatten() == Some(target.clone())
            && d.result
                .is_some_and(|r| r.result.as_deref() == Some("pass"))
            && !voided.contains(d.id())
    })
}

/// A verifier on the integrated card's tip, which must pass before it is accepted
/// (`_request_verify`).
fn request_verify(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    target: String,
) -> Step<Decision> {
    let (role, route) = resolve_or_park(st, obs, c.number, "verifier", 1, None)?;
    let mut r = Request::new(c.number, "verify", c.attempt.max(1), &role, route);
    r.extra = extra(vec![("tip", target.into())]);
    request(st, obs, c, r)
}

/// The card is integrating and nothing is dispatched for the step (`_integration_step`):
/// rebase (again, after a crash), dispatch an integrate attempt for a conflict, or re-review a
/// rebased card.
fn integration_step(st: &RunState, obs: &Observations, c: &CardState) -> Step<Decision> {
    let steps: Vec<&Event> = st
        .log
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::IntegrationStarted(_) | Event::Rebased(_) | Event::Conflict(_)
            ) && is_card(e.card(), c.number)
        })
        .collect();
    if !steps
        .iter()
        .any(|e| matches!(e, Event::IntegrationStarted(_)))
    {
        return fail("list index out of range".into());
    }
    match steps.last() {
        Some(Event::Conflict(conflict)) => request_integrate(st, obs, c, conflict),
        Some(Event::Rebased(rebased)) => request_integration_review(st, obs, c, rebased),
        _ => Ok(Decision::Rebase { card: c.number }),
    }
}

/// After a conflict, a new attempt on the route of the card's last implementer attempt (not
/// escalated) that may rebase the card branch, and only it, onto the run tip
/// (`_request_integrate`). Rebasing writes the Git directory, which Codex's sandbox refuses
/// (R39), so a Codex route integrates on the role's Claude route, a card's pin set aside; with
/// no Claude worker the card is parked.
fn request_integrate(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    conflict: &events::Conflict,
) -> Step<Decision> {
    let card = st.run.card(c.number)?;
    let empty = || fail("max() arg is an empty sequence".into());
    let Some(last) = st
        .log
        .card_dispatches(c.number, None, Some(&IMPLEMENTER_KINDS))
        .into_iter()
        .max_by_key(Dispatch::seq)
    else {
        return empty();
    };
    let Some(top) = st
        .log
        .card_dispatches(c.number, None, None)
        .iter()
        .map(Dispatch::attempt)
        .max()
    else {
        return empty();
    };
    let Some(cut) = st
        .log
        .events
        .iter()
        .filter_map(|e| match e {
            Event::CardCut(k) if is_card(k.card.as_ref(), c.number) => Some(k.seq),
            _ => None,
        })
        .max()
    else {
        return empty();
    };
    let numbers = st.run.numbers();
    let mut landed = Vec::new();
    for e in &st.log.events {
        if let Event::Accepted(a) = e
            && a.seq > cut
            && let Some(CardRef::Number(n)) = a.card
            && n != i64::from(c.number)
            && let Some(n) = u32::try_from(n).ok().filter(|n| numbers.contains(n))
        {
            let x = st.run.card(n)?;
            landed.push(format!("card {} ({}): {}", x.number, x.title, x.done_when));
        }
    }
    let mut route = last.route();
    if route.vendor != "claude" {
        let mut claude = None;
        if obs.installed.iter().any(|v| v == "claude") {
            // A card pinned to Codex still integrates on Claude: the pin is set aside.
            let mut pinless = card.clone();
            pinless.route_pin = None;
            let only = ["claude".to_string()];
            claude = resolve(
                &st.run.config,
                &RouteRequest {
                    role: last.role(),
                    card: Some(&pinless),
                    attempt: 1,
                    last_implementer_vendor: None,
                    installed: &only,
                    security: false,
                    profile: st.run.deck.profile.as_deref(),
                },
            )
            .ok()
            .map(|res| res.route);
        }
        match claude {
            Some(found) if found.vendor == "claude" => route = found,
            _ => return Ok(park(c.number, "integration needs a Claude worker".into())),
        }
    }
    let run_intent = if landed.is_empty() {
        "(no card was accepted since this card was cut)".to_string()
    } else {
        landed.join("; ")
    };
    let mut r = Request::new(c.number, "integrate", top + 1, last.role(), route);
    r.extra = PyMap(vec![
        ("run_tip".into(), conflict.onto.clone().map(PyStr::from)),
        (
            "conflicts".into(),
            Some(PyStr::join(
                conflict.paths.as_deref().unwrap_or_default(),
                ", ",
            )),
        ),
        (
            "card_intent".into(),
            Some(format!("card {} ({}): {}", card.number, card.title, card.done_when).into()),
        ),
        ("run_intent".into(), Some(run_intent.into())),
    ]);
    request(st, obs, c, r)
}

/// Re-reviews the integrated diff with a fresh reviewer (`_request_integration_review`). Every
/// integrated card gets it, a low-risk card that `card_review = "by-risk"` let through without
/// a review included.
fn request_integration_review(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    rebased: &events::Rebased,
) -> Step<Decision> {
    let last = st
        .log
        .card_dispatches(c.number, None, Some(&IMPLEMENTER_KINDS))
        .into_iter()
        .filter(|d| d.result.is_some())
        .max_by_key(Dispatch::seq);
    let (role, route) = resolve_or_park(
        st,
        obs,
        c.number,
        "reviewer",
        1,
        last.as_ref().map(Dispatch::vendor),
    )?;
    let findings = st
        .log
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::VerdictRecorded(v) if is_card(v.card.as_ref(), c.number) => Some(v),
            _ => None,
        })
        .and_then(|v| v.findings.clone())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "(none: no review findings before integration)".into());
    let onto = rebased.onto.as_deref().unwrap_or("None");
    let tip = rebased.tip.as_deref().unwrap_or("None");
    let notes = format!(
        "## Integration\n\nOther cards were accepted while this card waited, so its branch was \
         rebased onto the run tip {onto}. Review the integrated diff {onto}..{tip} as a whole. \
         The findings above are from the card's review before integration, if any; check that \
         the card still does what it should on top of the run tip."
    );
    let report = last
        .as_ref()
        .map(report_copy)
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "(none recorded)".into());
    let mut r = Request::new(c.number, "rereview", c.attempt.max(1), &role, route);
    r.extra = extra(vec![
        ("diff_range", format!("{onto}..{tip}").into()),
        ("findings", findings),
        ("implementer_report", report),
    ]);
    r.notes = notes.into();
    request(st, obs, c, r)
}

/// An integrate attempt (or an answer or continuation of one) reported READY
/// (`_after_integrate`): its branch must now contain the run tip and still hold the card's
/// changes. Then the card is `rebased`, which its re-review follows; otherwise a fresh worker
/// on the next rung gets the card, told where its commits before integration are kept.
fn after_integrate(
    st: &RunState,
    obs: &Observations,
    c: &CardState,
    d: &Dispatch,
) -> Step<Decision> {
    let Some(integ) = st.log.events.iter().rev().find_map(|e| match e {
        Event::IntegrationStarted(i) if is_card(i.card.as_ref(), c.number) => Some(i),
        _ => None,
    }) else {
        return fail("list index out of range".into());
    };
    let branch = card_branch(st.run, c);
    let onto = integ.onto.clone().unwrap_or_default();
    let mut kept = PyStr::default();
    if let Some(keep) = integ.keep.as_ref().filter(|k| !k.is_empty()) {
        kept.push_str(" The card's commits before integration are kept on ");
        kept.push(keep);
        kept.push_str(".");
    }
    let why = |text: &str| {
        let mut why = PyStr::from(text);
        why.push(&kept);
        why
    };
    let key = (branch.clone(), onto.clone());
    match obs.contains.get(&key) {
        None => return need(Need::Contains(key.0, key.1)),
        Some(false) => {
            let why = why("reported READY without rebasing the card branch onto the run tip.");
            return escalate(st, obs, c, why, None, "The review findings");
        }
        Some(true) => {}
    }
    let tip = match d
        .result
        .and_then(|r| r.tip.clone())
        .filter(|t| !t.is_empty())
    {
        Some(tip) => tip,
        None => match obs.tips.get(&branch) {
            Some(tip) => tip.clone(),
            None => return need(Need::Tip(branch)),
        },
    };
    let integ_tip = integ.tip.clone().unwrap_or_default();
    let base_key = (PyStr::from(onto.as_str()), PyStr::from(integ_tip.as_str()));
    let Some(old_base) = obs.merge_bases.get(&base_key).cloned() else {
        return need(Need::MergeBase(base_key.0, base_key.1));
    };
    if !changed(obs, &old_base, &integ_tip)?.is_empty() && changed(obs, &onto, &tip)?.is_empty() {
        // The card's work vanished (`rebase --skip`, a reset to the run tip): SPENT, not
        // integrated.
        let why = why(
            "ended SPENT: its integration left none of the card's changes on top of the run tip.",
        );
        return escalate(st, obs, c, why, None, "The review findings");
    }
    Ok(append(Event::Rebased(events::Rebased {
        card: Some(CardRef::Number(c.number.into())),
        onto: Some(onto),
        tip: Some(tip),
        dispatch_id: Some(Some(d.id().to_string())),
        ..events::Rebased::default()
    })))
}

/// The paths changed between two commits, once observed.
fn changed(obs: &Observations, a: &str, b: &str) -> Step<Vec<PyStr>> {
    match obs.changed.get(&(a.to_string(), b.to_string())) {
        Some(paths) => Ok(paths.clone()),
        None => need(Need::Changed(a.to_string(), b.to_string())),
    }
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
        return request(st, obs, c, r);
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
            return request(st, obs, c, r);
        }
    }
    let mut notes = PyStr::from(
        "## The operator's note\n\nThis card was parked; the operator resumed it with:\n\n",
    );
    notes.push(&note);
    start_attempt(st, obs, c, c.attempt.max(1), None, notes)
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
    pub(super) struct World {
        _dir: tempfile::TempDir,
        pub(super) run: Run,
    }

    pub(super) fn world(config_text: &str) -> World {
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
    pub(super) fn log(lines: &[(&str, &str)]) -> Log {
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

    pub(super) fn obs(installed: &[&str]) -> Observations {
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

    /// Decides with every guard observed clean and every card branch holding the run tip.
    fn decide_clean(st: &RunState, obs: &mut Observations) -> Decision {
        loop {
            match decide(st, obs) {
                Decision::Need(Need::Guard(id)) => {
                    obs.guards.insert(id, None);
                }
                Decision::Need(Need::NeedsIntegration(branch)) => {
                    obs.needs_integration.insert(branch, false);
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
        fresh.guards.insert("c1-a1-implement-1".into(), None);
        fresh.operation = None;
        assert_eq!(decide(&st, &fresh), Decision::Need(Need::Operation));
        // A worker that moved what it guards: its violation is recorded and undone.
        let found = events::Violation {
            stage: Some("found".into()),
            ..events::Violation::default()
        };
        fresh
            .guards
            .insert("c1-a1-implement-1".into(), Some(found.clone()));
        assert_eq!(
            decide(&st, &fresh),
            Decision::Void {
                dispatch_id: "c1-a1-implement-1".into(),
                found: Some(Box::new(found)),
            }
        );
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

    /// A review of card 1's implementer work (`c1-a1-review-<n>`), started and reported with
    /// `verdict`.
    fn review(n: u32, verdict: &str) -> [(&'static str, String); 3] {
        let id = format!("c1-a1-review-{n}");
        let req = requested(&id, "review", 1, SONNET)
            .replace(r#""role":"implementer""#, r#""role":"reviewer""#);
        let reported = result(&id, "review", "READY", "").replace(
            r#""head":"Status","verdict":null"#,
            &format!(r#""head":"Verdict","verdict":"{verdict}""#),
        );
        [
            ("dispatch_requested", req),
            ("worker_started", started(&id, &format!("w{}", n + 1))),
            ("result_recorded", reported),
        ]
    }

    /// Card 1 cut, implemented and reported READY at `cccc…`.
    fn implemented() -> Vec<(&'static str, String)> {
        let id = "c1-a1-implement-1";
        vec![
            (CUT.0, CUT.1.to_string()),
            ("dispatch_requested", requested(id, "implement", 1, SONNET)),
            ("worker_started", started(id, "w1")),
            ("result_recorded", result(id, "implement", "READY", "")),
        ]
    }

    fn check_violation(n: u32, stage: &str) -> (&'static str, String) {
        let fields = if stage == "found" {
            format!(
                r#""stage":"found","by":"check","dispatch_id":"c1-a1-review-{n}","card":1,"dispatch_kind":"review","n":{n},"ref":"refs/leadhand/abc123abc123/violations/{n}","paths":["README.md"],"head_moved":false,"index_dirty":false,"expected_head":"cccccccccccccccccccccccccccccccccccccccc","expected_branch":"leadhand/demo--card-1","refs":{{}},"preserved":{{}}"#
            )
        } else {
            format!(
                r#""stage":"restored","by":"check","dispatch_id":"c1-a1-review-{n}","card":1,"n":{n},"ref":"refs/leadhand/abc123abc123/violations/{n}","saved":"dddddddddddddddddddddddddddddddddddddddd","siblings":[],"preserved":{{}}"#
            )
        };
        ("violation", fields)
    }

    fn lines<'a>(owned: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
        owned.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }

    // `_void` then `_workspace_kept` (R33): a recorded violation not restored yet is restored
    // first; then the review is requested again, fresh, with the warning in its brief.
    #[test]
    fn decide_voids_a_changed_check_then_reruns_it_with_a_fresh_worker() {
        let w = world("");
        let mut owned = implemented();
        owned.extend(review(1, "defects"));
        owned.push(check_violation(1, "found"));
        let st = RunState::new(&w.run, log(&lines(&owned)));
        assert_eq!(
            decide_clean(&st, &mut obs(&["claude"])),
            Decision::Void {
                dispatch_id: "c1-a1-review-1".into(),
                found: None
            }
        );
        owned.push(check_violation(1, "restored"));
        let st = RunState::new(&w.run, log(&lines(&owned)));
        let Decision::Request(r) = decide_clean(&st, &mut obs(&["claude"])) else {
            panic!("no re-run");
        };
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.kind.as_str(),
                r.role.as_str(),
                r.resume.as_deref()
            ),
            ("c1-a1-review-2", "review", "reviewer", None)
        );
        assert_eq!(r.notes, voided_notes(&PyStr::default()));
        // The voided defects verdict never counted: no verdict was recorded for it.
        assert!(!st.log.events.iter().any(|e| e.kind() == "verdict_recorded"));
    }

    // R31's violation cap: a third voided review parks the card, naming the paths, and the
    // park names the kind to re-run (R32).
    #[test]
    fn decide_parks_the_card_at_the_violation_cap() {
        let w = world("");
        let mut owned = implemented();
        for n in 1..=3 {
            owned.extend(review(n, "clean"));
            owned.push(check_violation(n, "found"));
            owned.push(check_violation(n, "restored"));
        }
        let st = RunState::new(&w.run, log(&lines(&owned)));
        let Decision::Append(event) = decide_clean(&st, &mut obs(&["claude"])) else {
            panic!("no park");
        };
        let Event::Parked(p) = *event else {
            panic!("not a park: {event:?}");
        };
        assert_eq!(
            (p.reason, p.cap_dispatch, p.cap_kind, p.decision),
            (
                Some(PyStr::from("checks change tracked files: README.md")),
                Some(Some("c1-a1-review-3".into())),
                Some(Some("review".into())),
                None
            )
        );
    }

    // `_accept`: a card branch without the run tip is integrated first; an integrated card is
    // accepted only at a tip a verify passed on, so a verify of it is requested.
    #[test]
    fn decide_integrates_a_card_behind_the_run_tip_and_verifies_it_before_accepting() {
        let w = world("");
        let mut owned = implemented();
        owned.extend(review(1, "clean"));
        owned.push((
            "verdict_recorded",
            r#""card":1,"verdict":"clean","findings":"/run/cards/1/c1-a1-review-1/report.md","dispatch_id":"c1-a1-review-1""#.into(),
        ));
        let st = RunState::new(&w.run, log(&lines(&owned)));
        let branch = PyStr::from("leadhand/demo--card-1");
        let mut o = obs(&["claude"]);
        assert_eq!(
            decide(&st, &o),
            Decision::Need(Need::NeedsIntegration(branch.clone()))
        );
        o.needs_integration.insert(branch.clone(), true);
        assert_eq!(decide(&st, &o), Decision::Integrate { card: 1 });
        o.needs_integration.insert(branch.clone(), false);
        let tip = "c".repeat(40);
        assert_eq!(
            decide(&st, &o),
            Decision::Accept {
                card: 1,
                target: Some(tip.clone())
            }
        );
        // Integrated: rebased, re-reviewed clean, then a verify of the tip before acceptance.
        let mut owned = implemented();
        owned.push((
            "integration_started",
            format!(r#""card":1,"onto":"{}","tip":"{tip}","keep":"refs/leadhand/abc123abc123/integration/1-1""#, "e".repeat(40)),
        ));
        owned.push((
            "rebased",
            format!(r#""card":1,"onto":"{}","tip":"{tip}""#, "e".repeat(40)),
        ));
        owned.extend(review(1, "clean"));
        owned.push((
            "verdict_recorded",
            r#""card":1,"verdict":"clean","findings":"/f","dispatch_id":"c1-a1-review-1""#.into(),
        ));
        let st = RunState::new(&w.run, log(&lines(&owned)));
        let mut o = obs(&["claude"]);
        o.needs_integration.insert(branch.clone(), false);
        let key = (
            PyStr::from("refs/heads/leadhand/demo"),
            PyStr::from("refs/heads/leadhand/demo--card-1"),
        );
        assert_eq!(
            decide(&st, &o),
            Decision::Need(Need::MergeBase(key.0.clone(), key.1.clone()))
        );
        o.merge_bases.insert(key, "e".repeat(40));
        let Decision::Request(r) = decide(&st, &o) else {
            panic!("no verify");
        };
        assert_eq!(
            (r.dispatch_id.as_str(), r.role.as_str(), r.base.as_str()),
            ("c1-a1-verify-1", "verifier", "e".repeat(40).as_str())
        );
        assert_eq!(
            r.extra,
            PyMap(vec![("tip".into(), Some(PyStr::from(tip.as_str())))])
        );
    }

    // `_integration_step` and `_request_integrate`: an integration still at its intent is
    // rebased again; after a conflict, a new attempt on the last implementer's route is asked
    // to rebase the card branch onto the run tip.
    #[test]
    fn decide_rebases_again_or_requests_an_integrate_attempt_after_a_conflict() {
        let w = world("");
        let onto = "e".repeat(40);
        let mut owned = implemented();
        owned.push((
            "integration_started",
            format!(r#""card":1,"onto":"{onto}","tip":"{}","keep":"refs/leadhand/abc123abc123/integration/1-1""#, "c".repeat(40)),
        ));
        let st = RunState::new(&w.run, log(&lines(&owned)));
        assert_eq!(st.cards[&1].state, CardStage::Integrating);
        assert_eq!(
            decide_clean(&st, &mut obs(&["claude"])),
            Decision::Rebase { card: 1 }
        );
        owned.push((
            "conflict",
            format!(r#""card":1,"onto":"{onto}","paths":["src/a.txt","src/b.txt"]"#),
        ));
        let st = RunState::new(&w.run, log(&lines(&owned)));
        let mut o = obs(&["claude"]);
        let key = (
            PyStr::from("refs/heads/leadhand/demo"),
            PyStr::from("refs/heads/leadhand/demo--card-1"),
        );
        o.merge_bases.insert(key, "b".repeat(40));
        let Decision::Request(r) = decide_clean(&st, &mut o) else {
            panic!("no integrate attempt");
        };
        assert_eq!(
            (
                r.dispatch_id.as_str(),
                r.attempt,
                r.role.as_str(),
                r.route.vendor.as_str(),
                r.resume.as_deref()
            ),
            ("c1-a2-integrate-1", 2, "implementer", "claude", None)
        );
        let get = |k: &str| {
            r.extra
                .get(&PyStr::from(k))
                .cloned()
                .flatten()
                .map(|v| v.lossy().into_owned())
        };
        assert_eq!(get("run_tip"), Some(onto.clone()));
        assert_eq!(get("conflicts").as_deref(), Some("src/a.txt, src/b.txt"));
        assert_eq!(
            get("card_intent").as_deref(),
            Some("card 1 (Card number 1): the card's file exists and the tests pass")
        );
        assert_eq!(
            get("run_intent").as_deref(),
            Some("(no card was accepted since this card was cut)")
        );
        // With no Claude worker installed, a Codex implementer's card is parked instead.
        let codex = requested(
            "c1-a1-implement-1",
            "implement",
            1,
            ("codex", "gpt-5", "high"),
        );
        owned[1] = ("dispatch_requested", codex);
        let st = RunState::new(&w.run, log(&lines(&owned)));
        let Decision::Append(event) = decide_clean(&st, &mut obs(&["codex"])) else {
            panic!("no park");
        };
        let Event::Parked(p) = *event else {
            panic!("not a park: {event:?}");
        };
        assert_eq!(
            p.reason,
            Some(PyStr::from("integration needs a Claude worker"))
        );
    }
}
