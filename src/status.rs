//! `leadhand status` (the Python `status` module): what the operator sees while a run is going,
//! built from the event log (the source of truth), and the `status.json` each checkpoint writes.
//!
//! Reading never fails on a live or crashed run: a log line that cannot be read is skipped
//! and reported as an attention line, and a missing file is an empty log. Every text a run
//! carries (paths, refs, park reasons) stays lossless (`PyStr`) into `--json` and
//! `status.json`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::cli::human;
use crate::config;
use crate::deck;
use crate::error::{Error, Failure, Result};
use crate::pytext::{pure_path, py_fixed};
use crate::runsel;
use crate::state::events::{self, given};
use crate::state::{
    AttemptRecord, CardRef, CardStage, CardState, Event, RunMeta, RunPaths, decode_event,
    fold_cards, read_run_meta, run_paths, utc_now,
};
use crate::store;
use crate::testing;
use crate::usage::{self, Charge, Meter, codex_meter};
use crate::wire::{self, PyMap, PyStr, RawLine};

/// The Codex meter reading taken when a run's first status is written: the run's "before".
pub const METER_START: &str = "meter-start.json";

const IMPLEMENTER_VERBS: [(&str, &str); 6] = [
    ("implement", "Implementing"),
    ("continue", "Continuing"),
    ("fix", "Fixing"),
    ("answer", "Answering a question on"),
    ("integrate", "Integrating"),
    ("conflict", "Resolving conflicts on"),
];
const CHECK_VERBS: [(&str, &str); 3] = [
    ("review", "Reviewing"),
    ("rereview", "Re-reviewing"),
    ("verify", "Verifying"),
];
const STATE_VERBS: [(CardStage, &str); 9] = [
    (CardStage::Cut, "Starting"),
    (CardStage::Integrating, "Integrating"),
    (CardStage::Accepting, "Accepting"),
    (CardStage::Reviewed, "Processing the review of"),
    (CardStage::Fixing, "Fixing"),
    (CardStage::Reviewing, "Reviewing"),
    (CardStage::Requested, "Starting"),
    (CardStage::Started, "Working on"),
    (CardStage::Reported, "Processing the result of"),
];
const IDLE_STATES: [CardStage; 3] = [CardStage::Pending, CardStage::Accepted, CardStage::Parked];

fn verb<K: PartialEq + Copy>(table: &[(K, &'static str)], key: K) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `a` then `b`, joined losslessly.
fn cat(a: &str, b: &PyStr) -> PyStr {
    let mut out = PyStr::from(a);
    out.push(b);
    out
}

// --------------------------------------------------------------------------
// Tolerant reading

/// Each non-blank whole line of a run log, read under the state lock, with its number and the
/// raw JSON object it holds (`None` for a line that is not one). A log that cannot be read
/// adds `<label> could not be read: …`; a missing one is empty.
fn whole_lines(
    paths: &RunPaths,
    file: &Path,
    label: &str,
    problems: &mut Vec<PyStr>,
) -> Vec<(usize, Option<RawLine>)> {
    if !paths.root.is_dir() {
        return Vec::new();
    }
    let could_not = |why: PyStr| cat(&format!("{label} could not be read: "), &why);
    let raw = {
        let _lock = match store::try_state_lock(&paths.root) {
            Ok(lock) => lock,
            Err(failure) => {
                problems.push(could_not(failure.text));
                return Vec::new();
            }
        };
        match std::fs::read(file) {
            Ok(raw) => raw,
            Err(e) if e.kind() == ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                problems.push(could_not(Failure::io(file, e).text));
                return Vec::new();
            }
        }
    };
    let mut lines: Vec<&[u8]> = raw.split(|b| *b == b'\n').collect();
    lines.pop(); // after the last newline: empty, or a line still being written
    lines
        .into_iter()
        .enumerate()
        // `bytes.strip()` whitespace: a line of nothing else is no record.
        .filter(|(_, line)| {
            !line
                .iter()
                .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        })
        .map(|(i, line)| (i + 1, json_object(line)))
        .collect()
}

/// The line as a raw JSON object, as `json.loads` reads one.
fn json_object(line: &[u8]) -> Option<RawLine> {
    let text = std::str::from_utf8(line).ok()?;
    let raw = RawValue::from_string(text.trim_matches([' ', '\t', '\n', '\r']).to_owned()).ok()?;
    raw.get().starts_with('{').then_some(raw)
}

/// The events, tolerantly: each unreadable line adds `event log has an unreadable line at
/// seq <n>/line <m>`, `n` following the last `seq` read.
fn read_event_log(paths: &RunPaths, problems: &mut Vec<PyStr>) -> Vec<Event> {
    #[derive(Deserialize)]
    struct Seq {
        seq: Option<i64>,
    }
    let mut out = Vec::new();
    let mut last_seq: i64 = 0;
    for (number, raw) in whole_lines(paths, &paths.events, "event log", problems) {
        let seq = raw
            .as_ref()
            .and_then(|r| serde_json::from_str::<Seq>(r.get()).ok())
            .and_then(|s| s.seq);
        match raw.map(decode_event) {
            Some(Ok((event, _))) => out.push(event),
            _ => problems.push(PyStr::from(format!(
                "event log has an unreadable line at seq {}/line {number}",
                last_seq + 1
            ))),
        }
        if let Some(seq) = seq {
            last_seq = seq;
        }
    }
    out
}

/// A log of typed records, tolerantly: each line that is not one adds `<label> has an
/// unreadable line <n>`.
fn read_log<T: serde::de::DeserializeOwned>(
    paths: &RunPaths,
    file: &Path,
    label: &str,
    problems: &mut Vec<PyStr>,
) -> Vec<T> {
    let mut out = Vec::new();
    for (number, raw) in whole_lines(paths, file, label, problems) {
        match raw.and_then(|r| serde_json::from_str::<T>(r.get()).ok()) {
            Some(record) => out.push(record),
            None => problems.push(PyStr::from(format!(
                "{label} has an unreadable line {number}"
            ))),
        }
    }
    out
}

/// The charges, tolerantly: a line that is not JSON is an unreadable line, then each JSON
/// object that is not a charge an unreadable record (the Python's two passes, in its order).
fn read_charge_log(paths: &RunPaths, problems: &mut Vec<PyStr>) -> Vec<Charge> {
    let mut records = Vec::new();
    for (number, raw) in whole_lines(paths, &paths.charges, "charge log", problems) {
        match raw {
            Some(raw) => records.push(raw),
            None => problems.push(PyStr::from(format!(
                "charge log has an unreadable line {number}"
            ))),
        }
    }
    let mut out = Vec::new();
    for raw in records {
        match serde_json::from_str::<Charge>(raw.get()) {
            Ok(charge) => out.push(charge),
            Err(_) => problems.push("charge log has an unreadable record".into()),
        }
    }
    out
}

/// Everything the run directory holds, read once and tolerantly (`status.Snapshot`).
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub root: PathBuf,
    pub paths: RunPaths,
    pub meta: RunMeta,
    /// The deck's title, or the run directory's name when the deck does not load.
    pub title: PyStr,
    pub numbers: Vec<u32>,
    pub titles: BTreeMap<u32, String>,
    pub events: Vec<Event>,
    pub attempts: Vec<AttemptRecord>,
    pub charges: Vec<Charge>,
    pub cards: BTreeMap<u32, CardState>,
    pub problems: Vec<PyStr>,
}

impl Snapshot {
    /// The run's last outcome, unless a relaunch (`resumed`) came after it.
    pub fn outcome(&self) -> Option<&events::Outcome> {
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
}

/// The run directory `run_dir`, read (`status.load`).
pub fn load(run_dir: &Path) -> Snapshot {
    let paths = run_paths(run_dir);
    let root = paths.root.clone();
    let mut problems = Vec::new();
    let meta = read_run_meta(&paths).unwrap_or_else(|_| {
        problems.push("run.json is missing or unreadable".into());
        RunMeta::default()
    });
    let events = read_event_log(&paths, &mut problems);
    let attempts = read_log(&paths, &paths.attempts, "attempt log", &mut problems);
    let charges = read_charge_log(&paths, &mut problems);
    let (title, numbers, titles) = match deck::parse_deck(&paths.deck) {
        Ok(deck) => (
            PyStr::from(deck.title.as_str()),
            deck.cards.iter().map(|c| c.number).collect(),
            deck.cards
                .iter()
                .map(|c| (c.number, c.title.clone()))
                .collect(),
        ),
        Err(failure) => {
            let mut numbers: Vec<u32> = events
                .iter()
                .filter_map(|e| match e.card() {
                    Some(CardRef::Number(n)) => u32::try_from(*n).ok(),
                    _ => None,
                })
                .collect();
            numbers.sort_unstable();
            numbers.dedup();
            problems.push(cat("deck is missing or unreadable: ", &failure.text));
            let name = PyStr::from_os(root.file_name().unwrap_or_default());
            (name, numbers, BTreeMap::new())
        }
    };
    let cards = fold_cards(&events, &numbers);
    Snapshot {
        root,
        paths,
        meta,
        title,
        numbers,
        titles,
        events,
        attempts,
        charges,
        cards,
        problems,
    }
}

// --------------------------------------------------------------------------
// The Codex meter

/// The meter reading kept when the run's first status was written (`status.read_meter_start`).
pub fn read_meter_start(root: &Path) -> Meter {
    std::fs::read(root.join(METER_START))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

// --------------------------------------------------------------------------
// Usage

/// One row of usage per role or vendor.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub estimated_usd: Option<f64>,
    pub unknown_charges: u64,
}

/// The run's usage: totals from the charge log (the source of truth), each charge priced by
/// its own model (`status.usage_summary`).
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSummary {
    pub estimated_usd: Option<f64>,
    pub unknown_attempts: usize,
    pub by_role: PyMap<UsageRow>,
    pub by_vendor: PyMap<UsageRow>,
    pub undercounted_attempts: usize,
    pub codex_meter: Meter,
}

/// `claude` for Claude charges, `codex` for every other source.
pub fn vendor_of(source: &str) -> &'static str {
    if source == "claude" {
        "claude"
    } else {
        "codex"
    }
}

/// The run's prices: its configuration's, or none when it does not load.
fn run_prices(snap: &Snapshot) -> Vec<(String, config::Prices)> {
    let Some(repo) = snap.meta.repo.as_ref().and_then(|r| r.to_os().ok()) else {
        return Vec::new();
    };
    config::load_run_config(&snap.root, Path::new(&repo))
        .map(|c| c.prices)
        .unwrap_or_default()
}

/// `round(x, 6)`.
fn round6(x: f64) -> f64 {
    py_fixed(x, 6).parse().unwrap_or(x)
}

/// The run's usage; the Codex meter is read from `sessions_dir` unless `meter` is given.
pub fn usage_summary(snap: &Snapshot, sessions_dir: &Path, meter: Option<Meter>) -> UsageSummary {
    let prices = run_prices(snap);
    let mut by_role: PyMap<UsageRow> = PyMap::default();
    let mut by_vendor: PyMap<UsageRow> = PyMap::default();
    let mut total = 0.0;
    let mut priced_any = false;
    let mut incomplete: HashSet<&str> = HashSet::new();
    for c in &snap.charges {
        let u = &c.usage;
        let has_model = u.model.as_ref().is_some_and(|m| !m.is_empty())
            || u.by_model.as_ref().is_some_and(|b| !b.is_empty());
        let cost = if u.known && has_model {
            usage::estimate_cost(u, u.model.as_ref(), &prices)
        } else {
            None
        };
        for (table, key) in [
            (&mut by_role, c.role.as_str()),
            (&mut by_vendor, vendor_of(&c.source)),
        ] {
            let row = table.entry(&PyStr::from(key), UsageRow::default);
            row.input += u.tokens.input;
            row.output += u.tokens.output;
            row.cached += u.tokens.cached;
            if !u.known {
                row.unknown_charges += 1;
            }
            if let Some(cost) = cost {
                row.estimated_usd = Some(row.estimated_usd.unwrap_or(0.0) + cost);
            }
        }
        if let Some(cost) = cost {
            total += cost;
            priced_any = true;
        }
        if c.possibly_incomplete {
            incomplete.insert(&c.dispatch_id);
        }
    }
    let mut unknown: HashSet<(Option<&str>, Option<&str>)> = snap
        .attempts
        .iter()
        .filter(|a| a.usage_known == Some(false))
        .map(|a| (a.dispatch.as_deref(), a.role.as_deref()))
        .collect();
    unknown.extend(
        snap.charges
            .iter()
            .filter(|c| !c.usage.known)
            .map(|c| (Some(c.dispatch_id.as_str()), Some(c.role.as_str()))),
    );
    UsageSummary {
        estimated_usd: priced_any.then(|| round6(total)),
        unknown_attempts: unknown.len(),
        by_role,
        by_vendor,
        undercounted_attempts: incomplete.len(),
        codex_meter: meter.unwrap_or_else(|| codex_meter(sessions_dir)),
    }
}

// --------------------------------------------------------------------------
// Status

/// What `status --json` prints and each checkpoint writes to `status.json` (contract item 4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub run_id: String,
    pub deck: PyStr,
    pub state: String,
    pub headline: PyStr,
    pub activity: String,
    pub progress: Progress,
    pub attention: Vec<PyStr>,
    pub cards: Vec<CardRow>,
    pub usage: StatusUsage,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub accepted: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardRow {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub attempt: i64,
    pub route: String,
    pub note: PyStr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusUsage {
    pub estimated_usd: Option<f64>,
    pub unknown_attempts: usize,
    pub by_role: PyMap<UsageRow>,
    pub codex_meter: Meter,
}

/// One attention line per recorded violation, with the ref that holds the preserved work
/// (`status.violation_lines`).
pub fn violation_lines(events: &[Event]) -> Vec<PyStr> {
    let mut out = Vec::new();
    for e in events {
        let Event::Violation(v) = e else { continue };
        if v.stage.as_deref() != Some("found") {
            continue;
        }
        let who = [given(&v.dispatch_kind), v.by.as_ref()]
            .into_iter()
            .flatten()
            .find(|w| !w.is_empty())
            .map_or("worker", String::as_str);
        let mut line = PyStr::from(format!("Violation by a {who} worker"));
        if let Some(card) = given(&v.card) {
            line.push_str(" on card ");
            line.push(&card.text());
        }
        let mut refs = v
            .r#ref
            .iter()
            .chain(given(&v.preserved).into_iter().flat_map(PyMap::values));
        let place = refs
            .find(|r| !r.is_empty())
            .cloned()
            .unwrap_or_else(|| "no ref recorded".into());
        let dispatch = [given(&v.dispatch_id), given(&v.after_dispatch)]
            .into_iter()
            .flatten()
            .find(|d| !d.is_empty())
            .map_or("unknown", String::as_str);
        line.push_str(&format!(
            " (dispatch {dispatch}): its changes were preserved at "
        ));
        line.push(&place);
        line.push_str(" — next: inspect ");
        line.push(&place);
        out.push(line);
    }
    out
}

/// The first `dispatch_requested` of each dispatch ID (`status._requests`).
pub fn requests(events: &[Event]) -> HashMap<Option<&str>, &events::DispatchRequested> {
    let mut out = HashMap::new();
    for e in events {
        if let Event::DispatchRequested(d) = e {
            out.entry(d.dispatch_id.as_deref()).or_insert(d);
        }
    }
    out
}

/// `vendor model effort`, each when set (`status.route_text`).
pub fn route_text(req: Option<&events::DispatchRequested>) -> String {
    let Some(req) = req else {
        return String::new();
    };
    [&req.vendor, &req.model, &req.effort]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

fn activity(c: &CardState, req: Option<&events::DispatchRequested>) -> String {
    let in_dispatch = matches!(
        c.state,
        CardStage::Requested
            | CardStage::Started
            | CardStage::Reported
            | CardStage::Reviewing
            | CardStage::Fixing
    );
    let kind = c.dispatch_kind.as_deref().filter(|_| in_dispatch);
    if let Some(verb) = kind.and_then(|k| verb(&CHECK_VERBS, k)) {
        return format!("{verb} card {}", c.number);
    }
    if let Some(verb) = kind.and_then(|k| verb(&IMPLEMENTER_VERBS, k)) {
        let route = route_text(req);
        let route = if route.is_empty() {
            String::new()
        } else {
            format!(", {route}")
        };
        return format!("{verb} card {} (attempt {}{route})", c.number, c.attempt);
    }
    let verb = verb(&STATE_VERBS, c.state).unwrap_or("Working on");
    format!("{verb} card {}", c.number)
}

/// While the run has no outcome: a cancel or pause in progress, or, after a relaunch, what
/// reconciliation found, until the next worker starts (`status._steering_headline`).
fn steering_headline(events: &[Event], progress: &str) -> Option<PyStr> {
    if events.iter().any(|e| matches!(e, Event::Cancelled(_))) {
        return Some(format!("Cancelling · stopping every worker · {progress}").into());
    }
    let since = events
        .iter()
        .filter(|e| matches!(e, Event::Resumed(_)))
        .map(Event::seq)
        .max()
        .unwrap_or(0);
    if events
        .iter()
        .any(|e| matches!(e, Event::Paused(_)) && e.seq() > since)
    {
        return Some(
            format!("Pausing · running workers finish, nothing new starts · {progress}").into(),
        );
    }
    let rec = events.iter().rev().find_map(|e| match e {
        Event::Reconciled(r)
            if r.found.is_some()
                && since != 0
                && given(&r.resumed_seq).copied() == i64::try_from(since).ok() =>
        {
            Some(r)
        }
        _ => None,
    })?;
    let started_since = events.iter().any(|e| match e {
        Event::WorkerStarted(w) => {
            w.seq > rec.seq && !given(&w.reconciled).copied().unwrap_or(false)
        }
        _ => false,
    });
    if started_since {
        return None;
    }
    given(&rec.headline).filter(|h| !h.is_empty()).cloned()
}

fn number_list(numbers: &[u32]) -> String {
    match numbers {
        [one] => format!("card {one}"),
        [rest @ .., last] => {
            let rest: Vec<String> = rest.iter().map(u32::to_string).collect();
            format!("cards {} and {last}", rest.join(", "))
        }
        [] => String::new(),
    }
}

/// Whether a reason is a parked card's, which the card's own attention line already covers.
pub fn is_park_reason(reason: &PyStr) -> bool {
    reason.starts_with("card ") && reason.contains(" parked")
}

/// The run's status from its snapshot (`status.build_status`); the Codex meter is read from
/// `sessions_dir` unless `meter` is given.
pub fn build_status(
    snap: &Snapshot,
    sessions_dir: &Path,
    meter: Option<Meter>,
) -> std::result::Result<Status, Failure> {
    let reqs = requests(&snap.events);
    let cards = &snap.cards;
    let accepted = cards
        .values()
        .filter(|c| c.state == CardStage::Accepted)
        .count();
    let total = snap.numbers.len();
    let parked: Vec<u32> = cards
        .values()
        .filter(|c| c.state == CardStage::Parked)
        .map(|c| c.number)
        .collect();
    let active: Vec<&CardState> = cards
        .values()
        .filter(|c| !IDLE_STATES.contains(&c.state))
        .collect();
    let outcome = snap.outcome();

    // `unpark` is a next step only while the run can continue: unfinished, or ended blocked.
    let actionable = outcome.is_none_or(|o| o.outcome.as_deref() == Some("blocked"));
    let mut attention = Vec::new();
    if !parked.is_empty() && actionable {
        // Only built when needed: it names this binary and the run directory.
        // `Path.absolute()`, spelled as pathlib spells it.
        let root = std::path::absolute(&snap.root).map_err(|e| Failure::io(&snap.root, e))?;
        let root = pure_path(root.as_os_str());
        let cli = human::shell_quote(testing::self_path().as_os_str());
        let dir = human::shell_quote(root.as_os_str());
        for n in &parked {
            let mut line = park_line(&cards[n]);
            line.push(&PyStr::from_os_parts(&[
                OsStr::new(" — next: "),
                &cli,
                OsStr::new(" unpark "),
                &dir,
                OsStr::new(&format!(" --card {n} --note '…'")),
            ]));
            attention.push(line);
        }
    } else {
        attention.extend(parked.iter().map(|n| park_line(&cards[n])));
    }
    attention.extend(violation_lines(&snap.events));

    let progress = format!("{accepted} of {total} accepted");
    let mut state = "running".to_string();
    let mut activity_text = String::new();
    let mut headline: PyStr;
    if let Some(o) = outcome {
        let kind = o.outcome.clone().unwrap_or_default();
        let qualifier = o.qualifier.clone().unwrap_or_default();
        state = match kind.as_str() {
            "blocked" => "blocked",
            "failed" => "failed",
            _ => "done",
        }
        .into();
        let label = capitalize(&kind)
            + &if qualifier.is_empty() {
                String::new()
            } else {
                format!(" ({qualifier})")
            };
        headline = format!("{label} · {progress}").into();
        for reason in o.reasons.iter().flatten() {
            if !is_park_reason(reason) {
                attention.push(reason.clone());
            }
        }
    } else if !parked.is_empty() && active.is_empty() {
        headline = format!("Needs your decision on {}", number_list(&parked)).into();
    } else if !parked.is_empty() {
        headline = format!(
            "Needs your decision on {}; independent work continues",
            number_list(&parked)
        )
        .into();
        activity_text = activity(
            active[0],
            reqs.get(&active[0].dispatch_id.as_deref()).copied(),
        );
    } else if let Some(c) = active.first() {
        activity_text = activity(c, reqs.get(&c.dispatch_id.as_deref()).copied());
        headline = format!("Running · {activity_text} · {progress}").into();
    } else {
        activity_text = "Waiting for the supervisor".into();
        headline = format!("Running · {activity_text} · {progress}").into();
    }
    if outcome.is_none()
        && let Some(steering) = steering_headline(&snap.events, &progress)
    {
        headline = steering;
    }
    attention.extend(snap.problems.iter().cloned());

    let rows = cards
        .values()
        .map(|c| {
            let mut note = c.park_reason.clone().unwrap_or_default();
            if note.is_empty() && c.state == CardStage::Fixing {
                note = format!("fix round {}", c.fix_rounds).into();
            }
            CardRow {
                number: c.number,
                title: snap.titles.get(&c.number).cloned().unwrap_or_default(),
                state: c.state.as_str().into(),
                attempt: c.attempt,
                route: route_text(reqs.get(&c.dispatch_id.as_deref()).copied()),
                note,
            }
        })
        .collect();
    let summary = usage_summary(snap, sessions_dir, meter);
    Ok(Status {
        run_id: snap.meta.run_id.clone().unwrap_or_default(),
        deck: snap.title.clone(),
        state,
        headline,
        activity: activity_text,
        progress: Progress { accepted, total },
        attention,
        cards: rows,
        usage: StatusUsage {
            estimated_usd: summary.estimated_usd,
            unknown_attempts: summary.unknown_attempts,
            by_role: summary.by_role,
            codex_meter: summary.codex_meter,
        },
        updated_at: utc_now(),
    })
}

/// `Card <n> parked: <reason>`.
fn park_line(c: &CardState) -> PyStr {
    let mut line = PyStr::from(format!("Card {} parked: ", c.number));
    match c.park_reason.as_ref().filter(|r| !r.is_empty()) {
        Some(reason) => line.push(reason),
        None => line.push_str("no reason recorded"),
    }
    line
}

/// `str.capitalize` for the outcome names: the first character upper-cased, the rest lower.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// The text `status` prints (`status.render_status`), without its final newline.
pub fn render_status(s: &Status) -> PyStr {
    let mut lines: Vec<PyStr> = vec![s.headline.clone()];
    lines.extend(s.attention.iter().cloned());
    for c in &s.cards {
        let mut line = PyStr::from(format!("Card {}: {} · {}", c.number, c.title, c.state));
        if c.attempt != 0 {
            line.push_str(&format!(" · attempt {}", c.attempt));
        }
        if !c.route.is_empty() {
            line.push_str(&format!(" · {}", c.route));
        }
        if !c.note.is_empty() {
            line.push_str(" · ");
            line.push(&c.note);
        }
        lines.push(line);
    }
    let u = &s.usage;
    let cost = u
        .estimated_usd
        .map_or("unpriced".to_string(), |x| format!("${}", py_fixed(x, 2)));
    let meter = match u.codex_meter.percent {
        None => PyStr::from("unknown"),
        Some(p) => {
            let mut text = PyStr::from(format!("{}% as of ", py_fixed(p, 1)));
            text.push(u.codex_meter.observed_at.as_ref().unwrap_or(&"None".into()));
            text
        }
    };
    let mut usage = PyStr::from(format!(
        "Usage: estimated {cost} API-equivalent (an estimate, not subscription use) · {} \
         attempts with unknown usage · Codex meter ",
        u.unknown_attempts
    ));
    usage.push(&meter);
    usage.push_str(" · Claude meter unknown");
    lines.push(usage);
    let mut out = PyStr::default();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("\n");
        }
        out.push(line);
    }
    out
}

/// Rewrites `status.json` atomically (`status.write_status`). The Codex meter is read on the
/// first write (kept in `meter-start.json` as the run's "before", even when there is no
/// reading) and once the run has ended; other checkpoints carry the last reading forward
/// instead of scanning rollout files.
pub fn write_status(run_dir: &Path, sessions_dir: &Path) -> Result<Status> {
    #[derive(Deserialize)]
    struct Previous {
        usage: PreviousUsage,
    }
    #[derive(Deserialize)]
    struct PreviousUsage {
        codex_meter: Meter,
    }
    let first = !run_dir.join(METER_START).exists();
    let snap = load(run_dir);
    let meter = if first || snap.outcome().is_some() {
        None
    } else {
        std::fs::read(run_dir.join("status.json"))
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| serde_json::from_str::<Previous>(&t).ok())
            .map(|p| p.usage.codex_meter)
    };
    let s = build_status(&snap, sessions_dir, meter)?;
    if first {
        store::write_json_atomic(&run_dir.join(METER_START), &s.usage.codex_meter)?;
    }
    store::write_json_atomic(&snap.paths.status, &s)?;
    Ok(s)
}

/// A checkpoint's rewrite of `status.json` (the engine's `_write_status`): a run directory
/// with no `run.json` is skipped, and a failure is only a warning, since rendering must never
/// break the engine.
pub fn checkpoint(run_dir: &Path) {
    checkpoint_to(
        run_dir,
        &usage::default_sessions_dir(),
        &mut std::io::stderr().lock(),
    );
}

/// `checkpoint`, with the warning written to `warnings`.
fn checkpoint_to(run_dir: &Path, sessions_dir: &Path, warnings: &mut dyn Write) {
    if !run_paths(run_dir).run_json.is_file() {
        return;
    }
    if let Err(e) = write_status(run_dir, sessions_dir) {
        let _ = writeln!(warnings, "leadhand: could not write status.json: {e}");
    }
}

// --------------------------------------------------------------------------
// Command

/// The run's status; when it cannot be built, the last `status.json` with an attention line
/// saying why, since reading must not fail on a live or crashed run. The reason is the
/// failure's own lossless text.
fn status_or_rebuilt(snap: &Snapshot, sessions_dir: &Path) -> Result<Status> {
    let failure = match build_status(snap, sessions_dir, None) {
        Ok(s) => return Ok(s),
        Err(failure) => failure,
    };
    let read = std::fs::read(&snap.paths.status)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| serde_json::from_str::<Status>(&t).ok());
    let Some(mut s) = read else {
        return Err(Error::msg(format!("cannot read the run: {failure}")));
    };
    s.attention
        .push(cat("status rebuilt from status.json: ", &failure.text));
    Ok(s)
}

/// The run directory a command was given as a path, checked: the Python's `not a run
/// directory` error unless it holds `run.json` or `events.jsonl`.
fn checked_run_dir(root: &Path) -> Result<()> {
    let paths = run_paths(root);
    if !paths.run_json.is_file() && !paths.events.is_file() {
        return Err(Error::msg(format!(
            "not a run directory: {}",
            pure_path(root.as_os_str()).display()
        )));
    }
    Ok(())
}

/// `leadhand status [RUN] [--json]`: the run's headline, attention items, cards and usage;
/// with `--json`, the same as one JSON object (`status.json`'s shape). The run is found as
/// `runsel::select_run` finds it.
pub fn command(arg: Option<&OsStr>, json: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir().map_err(|e| Error::io(Path::new("."), e))?;
    let root = runsel::select_run("status", arg, &cwd)?;
    checked_run_dir(&root)?;
    let s = status_or_rebuilt(&load(&root), &usage::default_sessions_dir())?;
    let mut out = std::io::stdout().lock();
    if json {
        writeln!(out, "{}", wire::to_output(&s)?)?;
    } else {
        out.write_all(render_status(&s).to_os_lossy().as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn run_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("run dir ")
            .tempdir()
            .unwrap()
    }

    fn write_events(root: &Path, lines: &[&str]) {
        let mut text = String::new();
        for (i, fields) in lines.iter().enumerate() {
            text += &format!(
                "{{\"seq\":{},\"ts\":\"2026-01-01T00:00:00.000Z\",{fields}}}\n",
                i + 1
            );
        }
        fs::write(root.join("events.jsonl"), text).unwrap();
    }

    #[test]
    fn unreadable_lines_are_attention_lines_numbered_as_python_numbers_them() {
        let dir = run_dir();
        let root = dir.path();
        fs::write(
            root.join("events.jsonl"),
            "{\"seq\":1,\"ts\":\"t\",\"kind\":\"run_started\"}\n\
             \n\
             {not json}\n\
             {\"seq\":7,\"ts\":\"t\",\"kind\":\"no_such_kind\"}\n\
             [1]\n\
             {\"seq\":8,\"ts\":\"t\",\"kind\":\"card_cut\",\"card\":1}\n\
             {\"seq\":9,\"kind\":\"acc",
        )
        .unwrap();
        fs::write(root.join("charges.jsonl"), "{\"charge_id\":\"c\"}\nnope\n").unwrap();
        fs::write(root.join("attempts.jsonl"), "{oops\n").unwrap();
        let snap = load(root);
        let problems: Vec<String> = snap
            .problems
            .iter()
            .map(|p| p.lossy().into_owned())
            .collect();
        assert_eq!(
            problems,
            [
                "run.json is missing or unreadable",
                "event log has an unreadable line at seq 2/line 3",
                "event log has an unreadable line at seq 2/line 4",
                "event log has an unreadable line at seq 8/line 5",
                "attempt log has an unreadable line 1",
                "charge log has an unreadable line 2",
                "charge log has an unreadable record",
                &problems[7],
            ]
        );
        assert!(problems[7].starts_with("deck is missing or unreadable: [Errno 2] "));
        assert_eq!(snap.events.len(), 2);
        // Without a deck, the cards are the ones the events name, and the title is the
        // directory's name.
        assert_eq!(snap.numbers, [1]);
        assert_eq!(snap.title, PyStr::from_os(root.file_name().unwrap()));
    }

    #[test]
    fn an_unreadable_log_is_a_problem_naming_it() {
        let dir = run_dir();
        fs::create_dir(dir.path().join("events.jsonl")).unwrap();
        let snap = load(dir.path());
        let text = snap.problems[1].lossy().into_owned();
        assert!(
            text.starts_with("event log could not be read: [Errno 21] Is a directory: '"),
            "{text}"
        );
    }

    #[test]
    fn steering_headlines() {
        let dir = run_dir();
        let root = dir.path();
        let headline = |lines: &[&str]| {
            write_events(root, lines);
            let snap = load(root);
            build_status(&snap, &root.join("no sessions"), Some(Meter::default()))
                .unwrap()
                .headline
                .lossy()
                .into_owned()
        };
        assert_eq!(
            headline(&[r#""kind":"paused","by":"operator""#]),
            "Pausing · running workers finish, nothing new starts · 0 of 0 accepted"
        );
        // A pause before the latest relaunch is over.
        assert_eq!(
            headline(&[
                r#""kind":"paused","by":"operator""#,
                r#""kind":"resumed","run_id":"abc""#
            ]),
            "Running · Waiting for the supervisor · 0 of 0 accepted"
        );
        let resumed = [
            r#""kind":"resumed","run_id":"abc""#,
            r#""kind":"reconciled","resumed_seq":1,"found":[],"headline":"Resumed at card 2""#,
        ];
        assert_eq!(headline(&resumed), "Resumed at card 2");
        // A later relaunch not yet reconciled: the earlier relaunch's summary is not current.
        let mut again = resumed.to_vec();
        again.push(r#""kind":"resumed","run_id":"abc""#);
        assert_eq!(
            headline(&again),
            "Running · Waiting for the supervisor · 0 of 0 accepted"
        );
        let mut started = resumed.to_vec();
        started.push(r#""kind":"worker_started","dispatch_id":"d","worker_id":"w""#);
        assert_eq!(
            headline(&started),
            "Running · Waiting for the supervisor · 0 of 0 accepted"
        );
        assert_eq!(
            headline(&[r#""kind":"cancelled","by":"operator""#]),
            "Cancelling · stopping every worker · 0 of 0 accepted"
        );
    }

    #[test]
    fn violation_lines_name_the_preserved_ref() {
        let dir = run_dir();
        let root = dir.path();
        write_events(
            root,
            &[
                r#""kind":"violation","stage":"found","by":"check","dispatch_id":"c1-a1-review-1","card":1,"dispatch_kind":"review","n":1,"ref":null,"preserved":{"refs/heads/x":"refs/leadhand/r/violations/1-x"}"#,
                r#""kind":"violation","stage":"found","by":"implementer","n":1,"ref":"refs/leadhand/r/leftovers/1","after_dispatch":"c2-a1-implement-1""#,
                r#""kind":"violation","stage":"restored","by":"implementer","n":1,"ref":"refs/leadhand/r/leftovers/1""#,
            ],
        );
        let lines: Vec<String> = violation_lines(&load(root).events)
            .iter()
            .map(|l| l.lossy().into_owned())
            .collect();
        assert_eq!(
            lines,
            [
                "Violation by a review worker on card 1 (dispatch c1-a1-review-1): its changes \
                 were preserved at refs/leadhand/r/violations/1-x — next: inspect \
                 refs/leadhand/r/violations/1-x",
                "Violation by a implementer worker (dispatch c2-a1-implement-1): its changes \
                 were preserved at refs/leadhand/r/leftovers/1 — next: inspect \
                 refs/leadhand/r/leftovers/1",
            ]
        );
    }

    #[test]
    fn number_lists() {
        assert_eq!(number_list(&[5]), "card 5");
        assert_eq!(number_list(&[1, 2]), "cards 1 and 2");
        assert_eq!(number_list(&[1, 2, 3]), "cards 1, 2 and 3");
    }

    #[test]
    fn write_status_takes_the_meter_once_and_fails_on_an_unwritable_status() {
        let dir = run_dir();
        let root = dir.path();
        fs::write(root.join("run.json"), "{\"run_id\": \"abc123\"}").unwrap();
        write_events(root, &[r#""kind":"run_started""#]);
        let sessions = root.join("no sessions");
        let s = write_status(root, &sessions).unwrap();
        assert_eq!(s.run_id, "abc123");
        let start = fs::read_to_string(root.join(METER_START)).unwrap();
        assert_eq!(
            start,
            "{\n  \"percent\": null,\n  \"observed_at\": null\n}\n"
        );
        let written: Status =
            serde_json::from_str(&fs::read_to_string(root.join("status.json")).unwrap()).unwrap();
        assert_eq!(written, s);
        fs::remove_file(root.join("status.json")).unwrap();
        fs::create_dir(root.join("status.json")).unwrap();
        assert!(write_status(root, &sessions).is_err());
    }

    #[test]
    fn a_lock_that_cannot_be_taken_is_a_lossless_problem() {
        let dir = run_dir();
        // A directory name that is not UTF-8 where the system allows one (Linux; macOS
        // refuses to create it), else a plain one: the problem names the lock file exactly.
        let odd = dir.path().join(OsStr::from_bytes(b"run-\xff"));
        let root = if fs::create_dir(&odd).is_ok() {
            odd
        } else {
            let plain = dir.path().join("run-\u{e9}");
            fs::create_dir(&plain).unwrap();
            plain
        };
        fs::create_dir(root.join("state.lock")).unwrap();
        let snap = load(&root);
        let lock = run_paths(&root).state_lock;
        let open = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap_err();
        let want = cat(
            "event log could not be read: ",
            &Failure::io(&lock, open).text,
        );
        assert_eq!(snap.problems[1], want);
        let text = wire::ascii_json(&snap.problems[1]);
        assert!(!text.contains("\\ufffd"), "{text}");
    }

    #[test]
    fn a_status_rebuilt_from_status_json_says_why_losslessly() {
        let dir = run_dir();
        let root = dir.path();
        fs::write(root.join("run.json"), "{\"run_id\": \"abc123\"}").unwrap();
        write_events(
            root,
            &[
                r#""kind":"card_cut","card":1,"branch":"b","base":"a""#,
                r#""kind":"parked","card":1,"reason":"why""#,
            ],
        );
        let sessions = root.join("no sessions");
        let previous = write_status(root, &sessions).unwrap();
        // A run directory that cannot be made absolute (an empty path): the status cannot
        // name it in the unpark hint, so the last status.json is shown, saying why.
        let mut snap = load(root);
        snap.numbers = vec![1];
        snap.cards = fold_cards(&snap.events, &snap.numbers);
        snap.root = PathBuf::new();
        let failure = build_status(&snap, &sessions, None).unwrap_err();
        let s = status_or_rebuilt(&snap, &sessions).unwrap();
        let mut want = previous.attention.clone();
        want.push(cat("status rebuilt from status.json: ", &failure.text));
        assert_eq!(s.attention, want);
        assert_eq!(
            failure.text,
            Failure::io(Path::new(""), std::path::absolute("").unwrap_err()).text
        );
    }

    #[test]
    fn checkpoint_warns_with_the_python_prefix_and_carries_on() {
        let dir = run_dir();
        let root = dir.path();
        let sessions = root.join("no sessions");
        let mut warnings = Vec::new();
        // No run.json: nothing is written, nothing said.
        checkpoint_to(root, &sessions, &mut warnings);
        assert!(warnings.is_empty() && !root.join("status.json").exists());
        fs::write(root.join("run.json"), "{\"run_id\": \"abc123\"}").unwrap();
        checkpoint_to(root, &sessions, &mut warnings);
        assert!(warnings.is_empty() && root.join("status.json").is_file());
        // status.json can no longer be written.
        fs::remove_file(root.join("status.json")).unwrap();
        fs::create_dir(root.join("status.json")).unwrap();
        checkpoint_to(root, &sessions, &mut warnings);
        let text = String::from_utf8(warnings).unwrap();
        assert!(
            text.starts_with("leadhand: could not write status.json: "),
            "{text}"
        );
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.ends_with('\n'));
    }
}
