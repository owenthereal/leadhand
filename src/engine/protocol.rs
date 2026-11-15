//! What the engine and the supervisor say to each other (contract item 2; `skills/run`): the
//! action objects `next` prints (`engine.ACTIONS`), what `record` prints, the dispatch ID
//! format (contract item 8), the deadlines, and the kinds the card loop tells apart.
//!
//! Each action is a struct of its own whose `action` field is a marker that writes the action's
//! name, serialized in the Python's field order, as events are: no `#[serde(tag)]`. Paths and
//! worker text are `PyStr`.

use std::time::UNIX_EPOCH;

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

use crate::error::{Error, Result};
use crate::state::events::{self, Opt};
use crate::state::{CardRef, Event};
use crate::wire::{self, PyStr, RawLine};

/// The actions `next` returns (`engine.ACTIONS`).
pub const ACTIONS: [&str; 6] = [
    "dispatch",
    "wait",
    "check_diff",
    "answer",
    "stop_worker",
    "done",
];

/// What `record` takes (`engine.RECORD_KINDS`).
pub const RECORD_KINDS: [&str; 5] = ["started", "result", "ruling", "park", "diff-check"];

/// The implementer-side kinds (`engine.IMPLEMENTER_KINDS`).
pub const IMPLEMENTER_KINDS: [&str; 5] = ["implement", "fix", "answer", "continue", "integrate"];
/// The reviews (`engine.REVIEW_KINDS`).
pub const REVIEW_KINDS: [&str; 2] = ["review", "rereview"];
/// The checks: they may build, but must leave the workspace and the run's refs as they were
/// (`engine.CHECK_KINDS`).
pub const CHECK_KINDS: [&str; 3] = ["review", "rereview", "verify"];

/// The card states whose card has work open (`engine.ACTIVE_STATES`).
pub const ACTIVE_STATES: [&str; 8] = [
    "requested",
    "started",
    "reported",
    "reviewing",
    "reviewed",
    "fixing",
    "integrating",
    "accepting",
];

/// The Codex runner enforces its own limit; the proxy gets this many minutes more
/// (`engine.CODEX_GRACE_MINUTES`).
pub const CODEX_GRACE_MINUTES: i64 = 5;
/// How often `timer` looks (`engine.TIMER_INTERVAL`).
pub const TIMER_INTERVAL: f64 = 10.0;
/// The reply file in a dispatch's scratch folder (`engine.REPLY_FILE`, R38).
pub const REPLY_FILE: &str = "reply.txt";
/// The mechanical steps one `next` may take (`engine.MAX_STEPS`).
pub const MAX_STEPS: usize = 200;
/// Voided re-runs per card and kind; one more violation parks the card (`engine.VIOLATION_CAP`).
pub const VIOLATION_CAP: usize = 2;
/// The heading of the warning a voided check's re-run gets in its brief (`engine.VOIDED_NOTE`).
pub const VOIDED_NOTE: &str = "## An earlier run of this check was voided";

/// The brief notes for the re-run of a voided check, the warning added once
/// (`engine._voided_notes`).
pub fn voided_notes(notes: &PyStr) -> PyStr {
    if notes.contains(VOIDED_NOTE) {
        return notes.clone();
    }
    let mut out = if notes.is_empty() {
        PyStr::default()
    } else {
        let mut kept = notes.strip_ascii(b"\n", false);
        kept.push_str("\n\n");
        kept
    };
    out.push_str(VOIDED_NOTE);
    out.push_str(
        "\n\nAn earlier worker on this step changed the workspace or moved a branch it must \
         leave alone, so its result was voided. Never stage or commit what this brief does not \
         ask for, check out another branch, or create, move or delete any other ref.",
    );
    out
}

/// A card dispatch's ID: `c<card>-a<attempt>-<kind>-<n>`, `n` counting that kind within the
/// card's attempt (contract item 8).
pub fn card_dispatch_id(card: u32, attempt: i64, kind: &str, n: usize) -> String {
    format!("c{card}-a{attempt}-{kind}-{n}")
}

// --------------------------------------------------------------------------
// Time

/// The engine's clock in seconds since the epoch (`engine._now`: shifted by
/// `LEADHAND_TEST_CLOCK_OFFSET` in a testing build).
pub fn now() -> f64 {
    crate::testing::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// A deadline as the Python writes it (`engine._format_time`): UTC, to the second, with `Z`.
pub fn format_time(ts: f64) -> String {
    // `datetime.fromtimestamp` rounds to the microsecond (half to even), then the seconds are
    // shown alone.
    let whole = ts.trunc();
    let micros = round_half_even((ts - whole) * 1e6);
    let mut secs = whole as i64;
    if micros >= 1_000_000.0 {
        secs += 1;
    } else if micros < 0.0 {
        secs -= 1;
    }
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = crate::state::civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r - x.signum()
    } else {
        r
    }
}

/// The seconds since the epoch a deadline names (`engine._parse_time`:
/// `datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()`), for the forms the
/// engines write: `YYYY-MM-DDTHH:MM:SS`, an optional fraction, and `Z` or an offset.
pub fn parse_time(text: &str) -> Result<f64> {
    let bad = || {
        Error::msg(format!(
            "Invalid isoformat string: {}",
            crate::pytext::py_repr(text)
        ))
    };
    let replaced = text.replace('Z', "+00:00");
    let s = replaced.as_str();
    let num = |part: &str| -> Option<i64> {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    if s.len() < 19 || !s.is_char_boundary(19) {
        return Err(bad());
    }
    let (date, rest) = s.split_at(10);
    let (y, mo, d) = match date.split('-').collect::<Vec<_>>()[..] {
        [y, mo, d] if y.len() == 4 && mo.len() == 2 && d.len() == 2 => (
            num(y).ok_or_else(bad)?,
            num(mo).ok_or_else(bad)?,
            num(d).ok_or_else(bad)?,
        ),
        _ => return Err(bad()),
    };
    let rest = rest.strip_prefix(['T', ' ']).ok_or_else(bad)?;
    let (clock, mut tail) = rest.split_at(8.min(rest.len()));
    let (h, mi, sec) = match clock.split(':').collect::<Vec<_>>()[..] {
        [h, mi, sec] if h.len() == 2 && mi.len() == 2 && sec.len() == 2 => (
            num(h).ok_or_else(bad)?,
            num(mi).ok_or_else(bad)?,
            num(sec).ok_or_else(bad)?,
        ),
        _ => return Err(bad()),
    };
    let mut frac = 0.0;
    if let Some(f) = tail.strip_prefix(['.', ',']) {
        let digits = f.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(bad());
        }
        frac = format!("0.{}", &f[..digits]).parse().map_err(|_| bad())?;
        tail = &f[digits..];
    }
    let offset = if tail.is_empty() {
        0
    } else {
        let sign = match tail.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return Err(bad()),
        };
        let off: Vec<&str> = tail[1..].split(':').collect();
        let (oh, om) = match off[..] {
            [oh, om] => (num(oh).ok_or_else(bad)?, num(om).ok_or_else(bad)?),
            [ohm] if ohm.len() == 4 => (
                num(&ohm[..2]).ok_or_else(bad)?,
                num(&ohm[2..]).ok_or_else(bad)?,
            ),
            _ => return Err(bad()),
        };
        sign * (oh * 3600 + om * 60)
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 59 {
        return Err(bad());
    }
    let days = days_from_civil(y, mo, d);
    Ok((days * 86_400 + h * 3600 + mi * 60 + sec - offset) as f64 + frac)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// --------------------------------------------------------------------------
// Actions

/// One action marker per action: written as the action's name.
macro_rules! markers {
    ($($name:ident = $text:literal,)*) => {
        $(
            #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
            pub struct $name;

            impl Serialize for $name {
                fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                    s.serialize_str($text)
                }
            }
        )*
    };
}

pub mod tag {
    use super::{Serialize, Serializer};

    markers! {
        Dispatch = "dispatch",
        Wait = "wait",
        CheckDiff = "check_diff",
        Answer = "answer",
        StopWorker = "stop_worker",
    }
}

/// A route as an action shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RouteView {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// The worker a dispatch resumes (`resume`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Resume {
    pub worker_id: PyStr,
}

/// Launch a worker in the background (or SendMessage to `resume.worker_id`), then `record
/// started`. Repeated until `started` is recorded.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DispatchAction {
    pub action: tag::Dispatch,
    pub dispatch_id: String,
    pub kind: Option<String>,
    pub card: Option<CardRef>,
    pub attempt: Option<i64>,
    pub role: Option<String>,
    pub agent: String,
    pub agent_model: Option<String>,
    pub agent_effort: Option<String>,
    pub route: RouteView,
    pub message: PyStr,
    pub brief: Option<PyStr>,
    pub report: Option<PyStr>,
    pub resume: Option<Resume>,
    pub reply_file: PyStr,
    pub deadline: String,
    /// A run-level dispatch's gate and wave (both written, `null` when not set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Opt<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave: Opt<i64>,
}

/// A worker is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WaitAction {
    pub action: tag::Wait,
    pub dispatch_id: String,
    pub worker_id: Option<PyStr>,
    pub deadline: Option<String>,
    pub reply_file: PyStr,
    pub reason: String,
}

/// A fix touched only tests and docs: read the diff and record a diff check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckDiffAction {
    pub action: tag::CheckDiff,
    pub card: u32,
    pub dispatch_id: String,
    pub diff: PyStr,
    pub findings: Option<PyStr>,
    pub paths: Vec<PyStr>,
}

/// The worker asked a question: record a ruling, or park the card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AnswerAction {
    pub action: tag::Answer,
    pub card: u32,
    pub dispatch_id: String,
    pub worker_id: Option<PyStr>,
    pub report: PyStr,
    pub question: PyStr,
}

/// The deadline passed: stop the worker (or the Codex proxy); when `record_result` is true,
/// record its result with `--stopped`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StopWorkerAction {
    pub action: tag::StopWorker,
    pub dispatch_id: String,
    pub worker_id: Option<PyStr>,
    pub vendor: &'static str,
    pub record_result: bool,
    /// A stop for a cancel says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One card's state in a `done` action, with why a card that is not accepted stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CardRow {
    pub card: u32,
    pub state: &'static str,
    pub reason: PyStr,
}

/// The run ended (the report, handoff and runs.md line are written before this is returned),
/// or, under `LEADHAND_TEST_CARD_LOOP_ONLY`, the card loop did (`outcome` null).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoneAction {
    pub outcome: Option<String>,
    pub qualifier: String,
    pub reasons: Vec<PyStr>,
    pub report: Option<PyStr>,
    pub cards: Vec<CardRow>,
    /// The card-loop-only form, whose fields come in the Python's other order.
    pub card_loop_only: bool,
}

impl Serialize for DoneAction {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(6))?;
        map.serialize_entry("action", "done")?;
        map.serialize_entry("outcome", &self.outcome)?;
        map.serialize_entry("qualifier", &self.qualifier)?;
        if self.card_loop_only {
            map.serialize_entry("cards", &self.cards)?;
            map.serialize_entry("report", &self.report)?;
            map.serialize_entry("reasons", &self.reasons)?;
        } else {
            map.serialize_entry("reasons", &self.reasons)?;
            map.serialize_entry("report", &self.report)?;
            map.serialize_entry("cards", &self.cards)?;
        }
        map.end()
    }
}

/// The supervisor's next action (`engine.ACTIONS`).
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Dispatch(Box<DispatchAction>),
    Wait(WaitAction),
    CheckDiff(CheckDiffAction),
    Answer(AnswerAction),
    StopWorker(StopWorkerAction),
    Done(DoneAction),
}

impl Action {
    /// The action's name, as its `action` field writes it.
    pub fn name(&self) -> &'static str {
        match self {
            Action::Dispatch(_) => ACTIONS[0],
            Action::Wait(_) => ACTIONS[1],
            Action::CheckDiff(_) => ACTIONS[2],
            Action::Answer(_) => ACTIONS[3],
            Action::StopWorker(_) => ACTIONS[4],
            Action::Done(_) => ACTIONS[5],
        }
    }
}

impl Serialize for Action {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Action::Dispatch(a) => a.serialize(s),
            Action::Wait(a) => a.serialize(s),
            Action::CheckDiff(a) => a.serialize(s),
            Action::Answer(a) => a.serialize(s),
            Action::StopWorker(a) => a.serialize(s),
            Action::Done(a) => a.serialize(s),
        }
    }
}

// --------------------------------------------------------------------------
// Records

/// What `record` prints.
#[derive(Clone, Debug)]
pub enum RecordOutput {
    /// The event it appended.
    Event(Box<Event>),
    /// A repeated `started` for the same worker, or a `result` for a dispatch already
    /// recorded: the event already there, exactly as it was read.
    Existing(RawLine),
    /// A Codex dispatch whose runner is still running: nothing is recorded, and the `wait`
    /// lets `next` and the timer pick up its result when it lands.
    Action(Action),
}

impl RecordOutput {
    /// The line `record` prints: `json.dumps(value, ensure_ascii=True)`.
    pub fn to_output(&self) -> Result<String> {
        match self {
            RecordOutput::Event(e) => wire::to_output(e),
            RecordOutput::Existing(raw) => Ok(spaced(raw.get())),
            RecordOutput::Action(a) => wire::to_output(a),
        }
    }
}

/// A compact JSON line (`separators=(",", ":")`, as both engines write the log) in the layout
/// `json.dumps` prints (`", "` and `": "`), every other byte kept: what the Python prints for a
/// record it read back and printed again.
pub fn spaced(compact: &str) -> String {
    let mut out = String::with_capacity(compact.len() + compact.len() / 8);
    let (mut in_string, mut escaped) = (false, false);
    for c in compact.chars() {
        out.push(c);
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else {
            match c {
                '"' => in_string = true,
                ',' | ':' => out.push(' '),
                _ => {}
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::tests::python;

    #[test]
    fn deadlines_are_written_and_read_as_the_python_does() {
        let code = "import sys\nfrom leadhand import engine\n\
                    for a in sys.argv[1:]:\n\
                    \x20   print(engine._format_time(float(a)))\n";
        let stamps = [
            "0",
            "1794268800",
            "1794268800.9999996",
            "1794268859.5",
            "951782400.25",
        ];
        let args: Vec<&std::ffi::OsStr> = stamps.iter().map(std::ffi::OsStr::new).collect();
        let theirs = String::from_utf8(python(code, &args)).unwrap();
        let ours: String = stamps
            .iter()
            .map(|s| format!("{}\n", format_time(s.parse().unwrap())))
            .collect();
        assert_eq!(ours, theirs);
        for text in [
            "2026-11-14T10:20:30Z",
            "2026-11-14T10:20:30.250000+00:00",
            "2026-11-14T03:20:30-07:00",
        ] {
            let code = "import sys\nfrom leadhand import engine\nprint(repr(engine._parse_time(sys.argv[1])))";
            let theirs: f64 = String::from_utf8(python(code, &[std::ffi::OsStr::new(text)]))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(parse_time(text).unwrap(), theirs, "{text}");
        }
        assert_eq!(
            parse_time("soon").unwrap_err().message,
            "Invalid isoformat string: 'soon'"
        );
        assert_eq!(
            format_time(parse_time("2026-02-28T23:59:59Z").unwrap() + 1.0),
            "2026-03-01T00:00:00Z"
        );
    }

    #[test]
    fn records_read_back_print_as_the_python_prints_them() {
        let line = r#"{"seq":3,"ts":"t","kind":"ruling","card":1,"text":"a, b: \"c\" \\ x","x":[1,{"y":null}],"f":1e-05}"#;
        let code =
            "import json, sys\nprint(json.dumps(json.loads(sys.argv[1]), ensure_ascii=True))";
        let theirs = String::from_utf8(python(code, &[std::ffi::OsStr::new(line)])).unwrap();
        assert_eq!(format!("{}\n", spaced(line)), theirs);
    }

    #[test]
    fn dispatch_ids_follow_the_contract() {
        assert_eq!(card_dispatch_id(1, 2, "review", 3), "c1-a2-review-3");
    }

    #[test]
    fn voided_notes_add_the_warning_once_as_the_python_does() {
        let code = "import json, sys\nfrom leadhand import engine\n\
                    print(json.dumps([engine._voided_notes(n) for n in sys.argv[1:]]))";
        let notes = [
            "",
            "## The operator's note\n\nGo on.",
            "trailing newlines\n\n\n",
            "\n",
            "caf\u{e9}",
            "already voided\n\n## An earlier run of this check was voided\n\nkeep",
        ];
        let args: Vec<&std::ffi::OsStr> = notes.iter().map(std::ffi::OsStr::new).collect();
        let theirs: Vec<PyStr> = serde_json::from_slice(&python(code, &args)).unwrap();
        let ours: Vec<PyStr> = notes.iter().map(|n| voided_notes(&(*n).into())).collect();
        assert_eq!(ours, theirs);
        // Again on its own result: unchanged.
        assert_eq!(voided_notes(&ours[1]), ours[1]);
        assert_eq!(VIOLATION_CAP, 2);
    }
}
