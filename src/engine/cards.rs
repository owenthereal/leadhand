//! The card loop's records and steps:
//!
//! - the run as one `next` or `record` call loads it (`engine._Run`, `_load`,
//!   `_deck_problem`);
//! - the dispatches its event log records (`_Dispatch`, `_dispatches`), and the questions the
//!   card loop asks of the log (`_fix_round`, `_card_dispatches`, `_pending_unpark`,
//!   `_voided`, `_gone`, `_pick`, `_card_summary`, …), which only read;
//! - the card steps the shell carries out for `next::decide`'s decisions: cutting a card
//!   branch, requesting a dispatch (its guard, its brief, `dispatch_requested`), accepting a
//!   card, writing a test-only fix's diff, stopping a Codex dispatch, and the guard check
//!   that tells whether a worker left what it guards as it found it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use super::next::{DiffCheck, Request, RunState, dispatch_of, path_text, work_branch};
use super::protocol::{
    ACTIVE_STATES, Action, CHECK_KINDS, CardRow, CheckDiffAction, IMPLEMENTER_KINDS, REPLY_FILE,
    tag,
};
use super::record::store_result;
use crate::briefs_write::{BriefSpec, remove_scratch_file, write_brief};
use crate::config::{Config, Route, limits_key, load_run_config};
use crate::deck::{Card, Deck, dependency_order, parse_deck};
use crate::dispatch::cmd::{Worker, write_dispatch_request};
use crate::dispatch::{DispatchResult, ExitReason, cancel, field, request_for};
use crate::error::{Error, Result};
use crate::git::cards::{
    branch_ref, card_branch_name, current_branch, cut_card_branch, detect_violation, diff_text,
    fast_forward, is_ancestor, operation, ref_snapshot,
};
use crate::git::workspace::head;
use crate::git::{GIT_TIMEOUT, Git, untracked_files};
use crate::pytext::pure_path;
use crate::state::events::{
    self, DispatchRequested, Opt, Outcome, ResultRecorded, Unparked, WorkerStarted, given,
};
use crate::state::{
    CardRef, CardStage, CardState, Event, RunPaths, append_event, fold_cards, read_events,
    run_paths, two_phase, two_phase_resume,
};
use crate::store::write_file_atomic;
use crate::wire::{PyMap, PyStr, RawLine};

/// `Path(p).absolute()`: `p` joined to the current directory when relative, spelled as
/// `pathlib` spells it (no `..` resolved).
pub fn py_absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        return Ok(pure_path(p.as_os_str()));
    }
    let cwd = std::env::current_dir().map_err(|e| Error::io(Path::new("."), e))?;
    Ok(pure_path(cwd.join(p).as_os_str()))
}

/// The run, with its frozen deck (`<run>/deck.md`) and configuration snapshot (`engine._Run`).
pub struct Run {
    pub root: PathBuf,
    pub paths: RunPaths,
    /// `run.json` as read: its entries in file order.
    pub meta: PyMap<Box<RawValue>>,
    pub run_id: Option<String>,
    pub repo: PathBuf,
    /// The run's worktree (`run.json`'s `workspace`).
    pub ws: PathBuf,
    /// The run branch.
    pub branch: PyStr,
    pub base_sha: String,
    pub deck: Deck,
    pub config: Config,
}

impl Run {
    /// The run in `run_dir` (`engine._load(check_digest=False)`).
    pub fn load(run_dir: &Path) -> Result<Run> {
        let root = py_absolute(run_dir)?;
        let paths = run_paths(&root);
        let bytes = match std::fs::read(&paths.run_json) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::msg(format!(
                    "not a run directory (no run.json): {}",
                    root.display()
                )));
            }
            Err(e) => return Err(Error::io(&paths.run_json, e)),
        };
        let unreadable =
            |e: &dyn std::fmt::Display| Error::msg(format!("run.json is unreadable: {e}"));
        let text = String::from_utf8(bytes).map_err(|e| unreadable(&e))?;
        let meta: PyMap<Box<RawValue>> = serde_json::from_str(&text).map_err(|e| unreadable(&e))?;
        let deck = parse_deck(&paths.deck)?;
        let path = |key: &str| -> Result<PathBuf> {
            let text = field::<PyStr>(&meta, key)
                .ok_or_else(|| Error::msg(format!("run.json: no {key}")))?;
            Ok(PathBuf::from(text.to_os()?))
        };
        let repo = path("repo")?;
        let config = load_run_config(&root, &repo).map_err(Error::from)?;
        let ws = path("workspace")?;
        let branch =
            field::<PyStr>(&meta, "branch").ok_or_else(|| Error::msg("run.json: no branch"))?;
        Ok(Run {
            run_id: field(&meta, "run_id"),
            base_sha: field(&meta, "base_sha").unwrap_or_default(),
            root,
            paths,
            repo,
            ws,
            branch,
            deck,
            config,
            meta,
        })
    }

    /// Why the source deck no longer matches the one the run started from (edited, moved or
    /// deleted), or `None` (`engine._deck_problem`).
    pub fn deck_problem(&self) -> Option<String> {
        let source = field::<PyStr>(&self.meta, "deck_source").unwrap_or_default();
        let path = PathBuf::from(source.to_os_lossy());
        let digest = match std::fs::read(&path) {
            Ok(bytes) => hex(&Sha256::digest(&bytes)),
            Err(e) => {
                return Some(format!(
                    "cannot read the deck {}: {}",
                    pure_path(path.as_os_str()).display(),
                    Error::io(&path, e)
                ));
            }
        };
        let expected = field::<String>(&self.meta, "deck_digest").unwrap_or_default();
        (digest != expected).then(|| format!("deck changed since start (expected {expected})"))
    }

    /// The deck card `number` (`_Run.card`).
    pub fn card(&self, number: u32) -> Result<&Card> {
        self.deck
            .cards
            .iter()
            .find(|c| c.number == number)
            .ok_or_else(|| Error::msg(format!("no card {number} in the deck")))
    }

    /// The deck's card numbers, in deck order.
    pub fn numbers(&self) -> Vec<u32> {
        self.deck.cards.iter().map(|c| c.number).collect()
    }

    /// `<run>/cards/<card or run>/<dispatch id>`: a dispatch's run-folder copies.
    pub fn card_dir(&self, card: Option<&CardRef>, dispatch_id: &str) -> PathBuf {
        let folder = card.map_or_else(|| "run".into(), |c| c.text().to_os_lossy());
        self.root.join("cards").join(folder).join(dispatch_id)
    }

    /// Git in the run's worktree.
    pub fn git(&self) -> Result<Git> {
        Git::at(&self.ws)
    }

    /// `refs/heads/<run branch>`.
    pub fn run_ref(&self) -> PyStr {
        let mut out = PyStr::from("refs/heads/");
        out.push(&self.branch);
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// --------------------------------------------------------------------------
// The log

/// One dispatch, as its events record it (`engine._Dispatch`): the request, and the first
/// start and result recorded for it.
#[derive(Clone, Copy, Debug)]
pub struct Dispatch<'a> {
    pub req: &'a DispatchRequested,
    pub started: Option<&'a WorkerStarted>,
    pub result: Option<&'a ResultRecorded>,
    /// The start's and the result's lines, as read.
    pub started_raw: Option<&'a RawLine>,
    pub result_raw: Option<&'a RawLine>,
}

impl Dispatch<'_> {
    pub fn id(&self) -> &str {
        self.req.dispatch_id.as_deref().unwrap_or_default()
    }

    pub fn kind(&self) -> &str {
        self.req.dispatch_kind.as_deref().unwrap_or_default()
    }

    pub fn seq(&self) -> u64 {
        self.req.seq
    }

    pub fn attempt(&self) -> i64 {
        self.req.attempt.unwrap_or_default()
    }

    pub fn role(&self) -> &str {
        self.req.role.as_deref().unwrap_or_default()
    }

    pub fn vendor(&self) -> &str {
        self.req.vendor.as_deref().unwrap_or_default()
    }

    /// The route it was requested on.
    pub fn route(&self) -> Route {
        Route {
            vendor: self.vendor().to_string(),
            model: self.req.model.clone().unwrap_or_default(),
            effort: self.req.effort.clone().unwrap_or_default(),
        }
    }

    /// Whether it is a dispatch of deck card `n` (`d.req.get("card") == n`).
    pub fn is_card(&self, n: u32) -> bool {
        is_card(self.req.card.as_ref(), n)
    }

    /// Its request's `extra`, an empty map when it has none (`d.req.get("extra") or {}`).
    pub fn extra(&self) -> PyMap<Option<PyStr>> {
        self.req.extra.clone().unwrap_or_default()
    }

    /// Its request's `notes` (`d.req.get("notes") or ""`).
    pub fn notes(&self) -> PyStr {
        self.req.notes.clone().unwrap_or_default()
    }
}

/// Whether an event's `card` is deck card `n`, as the Python's `==` compares a number.
pub fn is_card(card: Option<&CardRef>, n: u32) -> bool {
    matches!(card, Some(CardRef::Number(m)) if *m == i64::from(n))
}

#[derive(Clone, Copy, Debug)]
struct Ix {
    req: usize,
    started: Option<usize>,
    result: Option<usize>,
}

/// The run's events, as one call read them, and the dispatches among them.
pub struct Log {
    pub events: Vec<Event>,
    pub raws: Vec<RawLine>,
    ix: Vec<Ix>,
    by_id: HashMap<Option<String>, usize>,
}

impl Log {
    /// The run's events (`state.read_events`).
    pub fn read(paths: &RunPaths) -> Result<Log> {
        Ok(Log::new(read_events(paths)?))
    }

    /// The dispatches the events record (`engine._dispatches`): keyed by dispatch ID in the
    /// order they were first requested; a start or result counts only once, the first.
    pub fn new(lines: Vec<(Event, RawLine)>) -> Log {
        let (events, raws): (Vec<Event>, Vec<RawLine>) = lines.into_iter().unzip();
        let mut ix: Vec<Ix> = Vec::new();
        let mut by_id: HashMap<Option<String>, usize> = HashMap::new();
        for (i, e) in events.iter().enumerate() {
            match e {
                Event::DispatchRequested(d) if !by_id.contains_key(&d.dispatch_id) => {
                    by_id.insert(d.dispatch_id.clone(), ix.len());
                    ix.push(Ix {
                        req: i,
                        started: None,
                        result: None,
                    });
                }
                Event::WorkerStarted(w) => {
                    if let Some(&at) = by_id.get(&w.dispatch_id) {
                        ix[at].started.get_or_insert(i);
                    }
                }
                Event::ResultRecorded(r) => {
                    if let Some(&at) = by_id.get(&r.dispatch_id) {
                        ix[at].result.get_or_insert(i);
                    }
                }
                _ => {}
            }
        }
        Log {
            events,
            raws,
            ix,
            by_id,
        }
    }

    fn view(&self, ix: Ix) -> Dispatch<'_> {
        let Event::DispatchRequested(req) = &self.events[ix.req] else {
            unreachable!("a dispatch's index names its request");
        };
        Dispatch {
            req,
            started: ix.started.and_then(|i| match &self.events[i] {
                Event::WorkerStarted(w) => Some(w),
                _ => None,
            }),
            result: ix.result.and_then(|i| match &self.events[i] {
                Event::ResultRecorded(r) => Some(r),
                _ => None,
            }),
            started_raw: ix.started.map(|i| &self.raws[i]),
            result_raw: ix.result.map(|i| &self.raws[i]),
        }
    }

    /// The dispatch with this ID (`index.get(id)`).
    pub fn dispatch(&self, id: Option<&str>) -> Option<Dispatch<'_>> {
        let key = id.map(str::to_string);
        self.by_id.get(&key).map(|&at| self.view(self.ix[at]))
    }

    /// Every dispatch, in the order first requested.
    pub fn dispatches(&self) -> impl Iterator<Item = Dispatch<'_>> + '_ {
        self.ix.iter().map(|&ix| self.view(ix))
    }

    /// Each deck card's state (`state.fold_cards`).
    pub fn cards(&self, numbers: &[u32]) -> BTreeMap<u32, CardState> {
        fold_cards(&self.events, numbers)
    }

    /// The outcome event that ended the run, unless a later `resumed` reopened it
    /// (`engine._ended`).
    pub fn ended(&self) -> Option<&Outcome> {
        let mut last = None;
        for e in &self.events {
            match e {
                Event::Outcome(o) => last = Some(o),
                Event::Resumed(_) => last = None,
                _ => {}
            }
        }
        last
    }

    /// The card's dispatches, of an attempt and of kinds when given (`_card_dispatches`).
    pub fn card_dispatches(
        &self,
        card: u32,
        attempt: Option<i64>,
        kinds: Option<&[&str]>,
    ) -> Vec<Dispatch<'_>> {
        self.dispatches()
            .filter(|d| {
                d.is_card(card)
                    && attempt.is_none_or(|a| d.req.attempt == Some(a))
                    && kinds.is_none_or(|k| k.contains(&d.kind()))
            })
            .collect()
    }

    /// The run-level dispatches (card `None`), by request (`_run_dispatches`).
    pub fn run_dispatches(&self) -> Vec<Dispatch<'_>> {
        let mut out: Vec<Dispatch> = self.dispatches().filter(|d| d.req.card.is_none()).collect();
        out.sort_by_key(Dispatch::seq);
        out
    }

    /// The card attempt's last fix round, of those before `before_seq` when given
    /// (`_fix_round`).
    pub fn fix_round(
        &self,
        card: u32,
        attempt: i64,
        before_seq: Option<u64>,
    ) -> Option<&events::FixRound> {
        self.events.iter().rev().find_map(|e| match e {
            Event::FixRound(f)
                if is_card(f.card.as_ref(), card)
                    && f.attempt == Some(attempt)
                    && before_seq.is_none_or(|s| f.seq < s) =>
            {
                Some(f)
            }
            _ => None,
        })
    }

    /// The `unparked` event the card has not acted on yet (`_pending_unpark`): a card parked
    /// before it was cut keeps its note through the cut.
    pub fn pending_unpark(&self, card: u32) -> Option<&Unparked> {
        let mut last = None;
        for e in &self.events {
            if !is_card(e.card(), card) {
                continue;
            }
            match e {
                Event::Unparked(u) => last = Some(u),
                Event::DispatchRequested(_) | Event::Parked(_) => last = None,
                _ => {}
            }
        }
        last
    }

    /// The violations recorded, other than an implementer's leftovers (`_by(e) != "implementer"`).
    pub fn check_violations(&self) -> impl Iterator<Item = &events::Violation> {
        self.violations().filter(|v| by(v) != "implementer")
    }

    /// Every violation event.
    pub fn violations(&self) -> impl Iterator<Item = &events::Violation> {
        self.events.iter().filter_map(|e| match e {
            Event::Violation(v) => Some(v),
            _ => None,
        })
    }

    /// The dispatches whose result a check violation voided (`_voided`).
    pub fn voided(&self) -> HashSet<String> {
        self.check_violations()
            .filter_map(|v| given(&v.dispatch_id).filter(|id| !id.is_empty()).cloned())
            .collect()
    }

    /// Implementer dispatches that left a Git operation in progress: they count as BLOCKED
    /// (environment) (`_broken_dispatches`).
    pub fn broken_dispatches(&self) -> HashSet<String> {
        self.violations()
            .filter(|v| {
                by(v) == "implementer"
                    && v.stage.as_deref() == Some("found")
                    && given(&v.operation).is_some_and(|op| !op.is_empty())
            })
            .filter_map(|v| given(&v.after_dispatch).filter(|d| !d.is_empty()).cloned())
            .collect()
    }

    /// The implementer leftovers recorded `found` and not yet `restored` (`_finish_leftovers`'s
    /// pending list).
    pub fn pending_leftovers(&self) -> Vec<&events::Violation> {
        let restored: HashSet<Option<i64>> = self
            .violations()
            .filter(|v| by(v) == "implementer" && v.stage.as_deref() == Some("restored"))
            .map(|v| v.n)
            .collect();
        self.violations()
            .filter(|v| {
                by(v) == "implementer"
                    && v.stage.as_deref() == Some("found")
                    && !restored.contains(&v.n)
            })
            .collect()
    }

    /// The seq of the latest ending a `resumed` followed, 0 when none (`_reopened_since`):
    /// failures recorded before it are retried; a relaunch without an ending keeps them.
    pub fn reopened_since(&self) -> u64 {
        let (mut since, mut ending) = (0, None);
        for e in &self.events {
            match e {
                Event::Outcome(o) => ending = Some(o.seq),
                Event::Resumed(_) => {
                    if let Some(seq) = ending {
                        since = seq;
                    }
                }
                _ => {}
            }
        }
        since
    }

    /// The seq of the latest `resumed`, 0 when the run was never relaunched
    /// (`_latest_resumed`).
    pub fn latest_resumed(&self) -> u64 {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Resumed(r) => Some(r.seq),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Whether the dispatch's Claude worker (or, for a Codex route, its proxy) died with an
    /// earlier supervisor session (`_gone`, P9): requested or started before the latest
    /// relaunch, or its start recorded by reconciliation.
    pub fn gone(&self, d: &Dispatch) -> bool {
        let since = self.latest_resumed();
        match d.started {
            None => d.req.seq < since,
            Some(s) => given(&s.reconciled).copied().unwrap_or(false) || s.seq < since,
        }
    }

    /// Whether a relaunch has not been reconciled yet (`_needs_reconcile`).
    pub fn needs_reconcile(&self) -> bool {
        let since = self.latest_resumed();
        since > 0
            && !self.events.iter().any(|e| {
                matches!(e, Event::Reconciled(r)
                    if r.found.is_some() && given(&r.resumed_seq) == Some(&(since as i64)))
            })
    }

    /// Whether the operator's cancel is recorded (`cancelled`).
    pub fn cancelled(&self) -> bool {
        self.events.iter().any(|e| matches!(e, Event::Cancelled(_)))
    }

    /// Whether a pause is recorded since the latest relaunch.
    pub fn paused(&self) -> bool {
        let since = self.latest_resumed();
        self.events
            .iter()
            .any(|e| matches!(e, Event::Paused(p) if p.seq > since))
    }
}

/// Who a violation was by (`_by`: `by`, else `check`).
pub fn by(v: &events::Violation) -> &str {
    v.by.as_deref().filter(|b| !b.is_empty()).unwrap_or("check")
}

/// The run-folder copy of the dispatch's report, or of its reply when it wrote no report
/// (`_report_copy`).
pub fn report_copy(d: &Dispatch) -> PyStr {
    let files = d.result.and_then(|r| r.files.as_ref());
    let get = |name: &str| {
        files
            .and_then(|f| f.get(&PyStr::from(name)))
            .filter(|p| !p.is_empty())
            .cloned()
    };
    get("report").or_else(|| get("reply")).unwrap_or_default()
}

/// Whether a card state is one with work open.
pub fn active(stage: CardStage) -> bool {
    ACTIVE_STATES.contains(&stage.as_str())
}

/// The card to work on (`engine._pick`, R31: one card in flight at a time): a card with open
/// work always comes first; then a card that was cut but not dispatched yet; then an unparked
/// card (it waits until nothing else is in flight); else the first pending card whose
/// dependencies are all accepted.
pub fn pick(run: &Run, log: &Log, cards: &BTreeMap<u32, CardState>) -> Result<Option<u32>> {
    let order = dependency_order(&run.deck)?;
    let state = |n: &u32| cards.get(n).map(|c| c.state);
    if let Some(n) = order.iter().find(|n| state(n).is_some_and(active)) {
        return Ok(Some(*n));
    }
    let cut: Vec<u32> = order
        .iter()
        .copied()
        .filter(|n| state(n) == Some(CardStage::Cut))
        .collect();
    if let Some(n) = cut.iter().find(|n| log.pending_unpark(**n).is_none()) {
        return Ok(Some(*n));
    }
    if let Some(n) = cut.first() {
        return Ok(Some(*n));
    }
    for n in &order {
        if state(n) != Some(CardStage::Pending) {
            continue;
        }
        let ready = run
            .card(*n)?
            .depends_on
            .iter()
            .all(|d| cards.get(d).is_none_or(|c| c.state == CardStage::Accepted));
        if ready {
            return Ok(Some(*n));
        }
    }
    Ok(None)
}

/// Each card's state, with why a card that is not accepted stopped: parked, or waiting on a
/// parked card (`engine._card_summary`).
pub fn card_summary(run: &Run, cards: &BTreeMap<u32, CardState>) -> Result<Vec<CardRow>> {
    fn waits_on(
        run: &Run,
        cards: &BTreeMap<u32, CardState>,
        n: u32,
        seen: &[u32],
    ) -> Result<Vec<u32>> {
        let mut parked = Vec::new();
        for d in &run.card(n)?.depends_on {
            let Some(c) = cards.get(d) else { continue };
            if seen.contains(d) {
                continue;
            }
            if c.state == CardStage::Parked {
                parked.push(*d);
            } else if c.state != CardStage::Accepted {
                let mut deeper = seen.to_vec();
                deeper.push(n);
                parked.extend(waits_on(run, cards, *d, &deeper)?);
            }
        }
        parked.sort_unstable();
        parked.dedup();
        Ok(parked)
    }

    let mut out = Vec::new();
    for n in dependency_order(&run.deck)? {
        let Some(c) = cards.get(&n) else { continue };
        let reason = match c.state {
            CardStage::Accepted => PyStr::default(),
            CardStage::Parked => c
                .park_reason
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "no reason recorded".into()),
            _ => {
                let blockers = waits_on(run, cards, n, &[])?;
                if blockers.is_empty() {
                    "waits on its dependencies".into()
                } else {
                    let names: Vec<String> = blockers.iter().map(u32::to_string).collect();
                    let s = if blockers.len() > 1 { "s" } else { "" };
                    format!("waits on parked card{s} {}", names.join(", ")).into()
                }
            }
        };
        out.push(CardRow {
            card: n,
            state: c.state.as_str(),
            reason,
        });
    }
    Ok(out)
}

// --------------------------------------------------------------------------
// The card steps the shell carries out

/// Records a Codex runner's result (`_store_result` with the runner's reply lines).
pub(crate) fn store_codex(st: &RunState, d: &Dispatch, result: &DispatchResult) -> Result<()> {
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

/// Stops the dispatch's runner, and Codex with its tool processes, and finalizes
/// `result.json` (`_cancel_codex`). When the proxy never ran the command, the request is
/// written first, so a late run of the same command finds the cancelled result instead of
/// starting Codex.
pub(crate) fn cancel_codex(
    run: &Run,
    cards: &BTreeMap<u32, CardState>,
    d: &Dispatch,
) -> Result<DispatchResult> {
    // The work branch's own errors are the engine's, as the Python's `_work_branch` raises
    // outside the dispatcher's.
    let branch = work_branch(run, cards, d)?;
    let stopped = || -> Result<DispatchResult> {
        let req = d.req;
        let path = |p: &Option<PyStr>| -> Result<PathBuf> {
            Ok(PathBuf::from(p.clone().unwrap_or_default().to_os()?))
        };
        let worker = Worker {
            card: req.card.as_ref().and_then(CardRef::number),
            attempt: req.attempt.unwrap_or_default(),
            role: d.role().to_string(),
            model: req.model.clone().unwrap_or_default().into(),
            effort: req.effort.clone().unwrap_or_default().into(),
            brief: path(&req.brief)?,
            report: path(&req.report)?,
            cwd: run.ws.clone(),
            resume: req.resume_session.clone(),
            kind: req.dispatch_kind.clone(),
            branch: Some(branch),
        };
        let request = request_for(&run.root, d.id(), worker)?;
        let wd = write_dispatch_request(&request)?;
        cancel(&wd, crate::dispatch::cancel::CANCEL_TIMEOUT)
    };
    stopped().map_err(|e| {
        Error::msg(format!(
            "could not stop Codex dispatch {}: {}",
            d.id(),
            e.message
        ))
    })
}

/// Refuses to touch the workspace for `card` while another card's worker, or a run-level
/// worker, may be using it (`_no_open_dispatch`).
pub(crate) fn no_open_dispatch(st: &RunState, card: u32) -> Result<()> {
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
pub(crate) fn ensure_clean(run: &Run, branch: &PyStr, untracked: bool) -> Result<()> {
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
pub(crate) fn check_out(st: &RunState, card: u32, branch: &PyStr) -> Result<()> {
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
pub(crate) fn cut_card(st: &RunState, n: u32) -> Result<()> {
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
pub(crate) struct Guard {
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
pub(crate) fn request_dispatch(st: &RunState, r: &Request) -> Result<()> {
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
pub(crate) fn append_request(
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
pub(crate) fn accept(run: &Run, card: u32, target: Option<String>) -> Result<()> {
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

pub(crate) fn accepted(card: u32, target: &str) -> Event {
    Event::Accepted(events::Accepted {
        card: Some(CardRef::Number(card.into())),
        sha: Some(target.to_string()),
        ..events::Accepted::default()
    })
}

/// Completes an acceptance whose intent is recorded (`_finish_accept`). Idempotent: a run
/// branch that already contains the target is left where it is.
pub(crate) fn finish_accept(run: &Run, card: u32, target: Option<String>) -> Result<()> {
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
pub(crate) fn write_diff_check(st: &RunState, check: &DiffCheck) -> Result<Action> {
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
pub(crate) fn guard_moved(st: &RunState, d: &Dispatch) -> Result<bool> {
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
pub(crate) fn engine_writes(
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
