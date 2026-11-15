//! The card loop's records, which only read:
//!
//! - the run as one `next` or `record` call loads it (`engine._Run`, `_load`,
//!   `_deck_problem`), and the supervisor session making the call (`_current_session`);
//! - the dispatches its event log records (`_Dispatch`, `_dispatches`), and the questions the
//!   card loop asks of the log (`_fix_round`, `_card_dispatches`, `_pending_unpark`,
//!   `_voided`, `_gone`, `_pick`, `_card_summary`, `_card_state`, `_work_branch`, …).
//!
//! The steps the shell carries out are `shell`'s; the pure decisions `next`'s.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

use super::protocol::{ACTIVE_STATES, CardRow, FINAL};
use crate::briefs_write::strip_pystr;
use crate::config::{Config, Route, load_run_config};
use crate::deck::{Card, Deck, dependency_order, parse_deck};
use crate::dispatch::field;
use crate::error::{Error, Result};
use crate::git::Git;
use crate::git::cards::card_branch_name;
use crate::pytext::pure_path;
use crate::state::events::{
    self, DispatchRequested, Outcome, ResultRecorded, Unparked, WorkerStarted, given,
};
use crate::state::{
    CardRef, CardStage, CardState, Event, RunPaths, fold_cards, read_events, run_paths,
};
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

    /// `the run`, or `card <n>` (`_whose`).
    pub fn whose(&self) -> String {
        match &self.req.card {
            None => "the run".to_string(),
            Some(c) => format!("card {}", c.text().lossy()),
        }
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

    /// The dispatches the run waits on (`_open_dispatches`): each card's current dispatch while
    /// the card has work open and the dispatch no result, and the latest run-level one while it
    /// has no result; by request.
    pub fn open_dispatches(&self, cards: &BTreeMap<u32, CardState>) -> Vec<Dispatch<'_>> {
        let mut out: Vec<Dispatch> = cards
            .values()
            .filter(|c| active(c.state))
            .filter_map(|c| {
                c.dispatch_id
                    .as_deref()
                    .and_then(|id| self.dispatch(Some(id)))
            })
            .filter(|d| d.result.is_none())
            .collect();
        if let Some(last) = self.run_dispatches().pop()
            && last.result.is_none()
        {
            out.push(last);
        }
        out.sort_by_key(Dispatch::seq);
        out
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

/// The branch a dispatch works on (`_work_branch`): its card branch, the final fix branch for
/// a fix wave's implementer, else the run branch. The Codex runner commits implementer work
/// there (R39).
pub(crate) fn work_branch(
    run: &Run,
    cards: &BTreeMap<u32, CardState>,
    d: &Dispatch,
) -> Result<PyStr> {
    let Some(card) = &d.req.card else {
        return Ok(if d.kind() == "final_fix" {
            final_branch(run)
        } else {
            run.branch.clone()
        });
    };
    let c = card_state(cards, Some(card))?;
    Ok(c.branch
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| card_branch_name(&run.branch, &card.text().lossy())))
}

/// `refs/heads/<branch>`.
pub fn heads(branch: &PyStr) -> PyStr {
    let mut name = PyStr::from("refs/heads/");
    name.push(branch);
    name
}

/// `OsStr::new`, for the words of a command line.
pub(super) fn os(s: &str) -> &OsStr {
    OsStr::new(s)
}

/// The card's branch as the run records it, else the name it is cut under
/// (`c.branch or card_branch_name(ctx.branch, n)`).
pub fn card_branch(run: &Run, c: &CardState) -> PyStr {
    c.branch
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| card_branch_name(&run.branch, &c.number.to_string()))
}

/// The fix waves' branch, `<run branch>--card-final` (`_final_branch`).
pub fn final_branch(run: &Run) -> PyStr {
    card_branch_name(&run.branch, FINAL)
}

/// The fix waves' `card`, `"final"`, as their events record it.
pub fn final_card() -> CardRef {
    CardRef::Text(FINAL.into())
}

/// A commit's first 12 characters, as messages and reports show it (`sha[:12]`).
pub fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Whether an event's `card` is the fix waves' (`e.get("card") == FINAL`).
pub fn is_final(card: Option<&CardRef>) -> bool {
    matches!(card, Some(CardRef::Text(t)) if t.as_wtf8() == FINAL.as_bytes())
}

/// The accepted deck cards, in dependency order (`_accepted_cards`).
pub fn accepted_cards<'r>(run: &'r Run, cards: &BTreeMap<u32, CardState>) -> Result<Vec<&'r Card>> {
    let mut out = Vec::new();
    for n in dependency_order(&run.deck)? {
        if cards
            .get(&n)
            .is_some_and(|c| c.state == CardStage::Accepted)
        {
            out.push(run.card(n)?);
        }
    }
    Ok(out)
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
