//! `record` (`engine.record`): what the supervisor saw or decided, appended to the event log:
//! a worker's start, its result, a ruling on its question, a park, or a test-only fix's diff
//! check.
//!
//! A result is recorded exactly once (`_store_result`): the dispatch's brief, report and reply
//! are copied into the run folder (never through a symlink), the report's head is read, the
//! worker is charged under its own charge ID, the attempt record is written, then
//! `result_recorded`, each repeat-safe, with the crash points between them. A repeated
//! `started` for the same worker, or a `result` for a dispatch already recorded, prints the
//! event already there, as it was read.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::cards::{Dispatch, Log, Run, cancel_codex};
use super::next::{
    budget_seconds, card_state, current_session, line_starts, projects_dir, read_text,
};
use super::protocol::{
    Action, IMPLEMENTER_KINDS, REPLY_FILE, RecordOutput, WaitAction, format_time, now, tag,
};
use crate::briefs::{CODEX_AGENT, after_folded, agent_for, parse_report_head, scratch_dir};
use crate::briefs_write::{ScratchError, read_scratch_file, strip_pystr};
use crate::cli::{RecordArgs, RecordKind, YesNo};
use crate::dispatch::commit::is_decimal;
use crate::dispatch::runner::has_status_line;
use crate::dispatch::{DispatchResult, ExitReason, load_result, runner_alive};
use crate::error::{Error, Result};
use crate::git::cards::{branch_ref, card_branch_name};
use crate::git::workspace::{head, resolves_in_place};
use crate::pytext::{abspath, pure_path, py_isspace};
use crate::state::events;
use crate::state::{
    AttemptRecord, CardRef, CardStage, CardState, Event, append_attempt, append_event,
    read_attempts, utc_now,
};
use crate::store::{state_lock, write_file_atomic};
use crate::testing::crash_point;
use crate::usage::{ChargeSource, charge, claude_subagent_transcript, estimate_cost};
use crate::wire::{PyMap, PyStr};

/// What `record` was given (its flags). A `-` for `--reply` or `--text` has been read from
/// stdin already.
#[derive(Clone, Debug, Default)]
pub struct RecordFields {
    pub dispatch: Option<PyStr>,
    pub worker: Option<PyStr>,
    pub reply: Option<PyStr>,
    pub reply_file: Option<PyStr>,
    pub stopped: bool,
    pub card: Option<i64>,
    pub text: Option<PyStr>,
    pub reason: Option<PyStr>,
    pub ok: Option<bool>,
    pub note: PyStr,
}

/// `leadhand record RUN_DIR KIND …`: the event recorded (or already there), as one line of
/// ASCII JSON.
pub fn command(args: RecordArgs) -> Result<ExitCode> {
    let from_stdin = |value: Option<&OsStr>| -> Result<Option<PyStr>> {
        match value {
            Some(v) if v == "-" => read_stdin().map(Some),
            Some(v) => Ok(Some(PyStr::from_os(v))),
            None => Ok(None),
        }
    };
    let reply = match args.kind {
        RecordKind::Result => Some(from_stdin(args.reply.as_deref())?.unwrap_or_default()),
        _ => None,
    };
    let text = match args.kind {
        RecordKind::Ruling => from_stdin(args.text.as_deref())?,
        _ => args.text.as_deref().map(PyStr::from_os),
    };
    let fields = RecordFields {
        dispatch: args.dispatch.as_deref().map(PyStr::from_os),
        worker: args.worker.as_deref().map(PyStr::from_os),
        reply,
        reply_file: args.reply_file.as_deref().map(PyStr::from_os),
        stopped: args.stopped,
        card: args.card,
        text,
        reason: args.reason.as_deref().map(PyStr::from_os),
        ok: args.ok.map(|ok| ok == YesNo::Yes),
        note: PyStr::from_os(&args.note),
    };
    let out = record(Path::new(&args.run_dir), args.kind, fields)?;
    let mut line = out.to_output()?;
    line.push('\n');
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(line.as_bytes())?;
    stdout.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// `sys.stdin.read()`: UTF-8, strictly, as the Python's text stdin decodes it.
fn read_stdin() -> Result<PyStr> {
    let mut data = Vec::new();
    std::io::stdin().read_to_end(&mut data)?;
    match String::from_utf8(data) {
        Ok(text) => Ok(PyStr::from(text)),
        Err(e) => {
            let err = e.utf8_error();
            let at = err.valid_up_to();
            let byte = e.as_bytes()[at];
            let why = match err.error_len() {
                None => "unexpected end of data",
                Some(_) if (0xC2..=0xF4).contains(&byte) => "invalid continuation byte",
                Some(_) => "invalid start byte",
            };
            Err(Error::msg(format!(
                "'utf-8' codec can't decode byte 0x{byte:02x} in position {at}: {why}"
            )))
        }
    }
}

/// Records what the supervisor saw or decided (`engine.record`); `status.json` is rewritten
/// afterwards, whatever happened.
pub fn record(run_dir: &Path, kind: RecordKind, fields: RecordFields) -> Result<RecordOutput> {
    let out = Run::load(run_dir).and_then(|run| match kind {
        RecordKind::Started => record_started(&run, &fields),
        RecordKind::Result => record_result(&run, &fields),
        RecordKind::Ruling => record_ruling(&run, &fields),
        RecordKind::Park => record_park(&run, &fields),
        RecordKind::DiffCheck => record_diff_check(&run, &fields),
    });
    crate::status::checkpoint(run_dir);
    out
}

fn repr(id: Option<&PyStr>) -> String {
    id.map_or_else(|| "None".to_string(), PyStr::repr)
}

/// The dispatch `id` names (`_known_dispatch`).
fn known_dispatch<'l>(log: &'l Log, id: Option<&PyStr>) -> Result<Dispatch<'l>> {
    let found = id
        .and_then(|id| std::str::from_utf8(id.as_wtf8()).ok())
        .and_then(|id| log.dispatch(Some(id)));
    found.ok_or_else(|| Error::msg(format!("no dispatch {} was requested", repr(id))))
}

/// Refuses once the run has ended (`_not_ended`).
fn not_ended(log: &Log) -> Result<()> {
    match log.ended() {
        Some(o) => Err(Error::msg(format!(
            "the run has ended ({})",
            o.outcome.as_deref().unwrap_or("None")
        ))),
        None => Ok(()),
    }
}

/// The card a command names (`--card`), by number.
fn named_card(cards: &BTreeMap<u32, CardState>, card: Option<i64>) -> Result<&CardState> {
    card_state(cards, card.map(CardRef::Number).as_ref())
}

/// `record started`: the worker the supervisor launched for a dispatch, with its deadline
/// (`_record_started`).
fn record_started(run: &Run, f: &RecordFields) -> Result<RecordOutput> {
    let log = Log::read(&run.paths)?;
    let d = known_dispatch(&log, f.dispatch.as_ref())?;
    let worker = f
        .worker
        .clone()
        .filter(|w| !strip_pystr(w).is_empty())
        .ok_or_else(|| Error::msg("record started needs the worker's agent ID"))?;
    if let Some(started) = d.started {
        if started.worker_id.as_ref() == Some(&worker) {
            let raw = d.started_raw.expect("a start has its line");
            return Ok(RecordOutput::Existing(raw.clone()));
        }
        return Err(Error::msg(format!(
            "dispatch {} already started worker {}",
            d.id(),
            started
                .worker_id
                .as_ref()
                .map_or_else(|| "None".into(), PyStr::shown)
        )));
    }
    not_ended(&log)?;
    if d.req.card.is_none() {
        let runs = log.run_dispatches();
        if runs.last().map(Dispatch::id) != Some(d.id()) {
            return Err(Error::msg(format!(
                "dispatch {} is not the run's current dispatch",
                d.id()
            )));
        }
    } else {
        let cards = log.cards(&run.numbers());
        let c = card_state(&cards, d.req.card.as_ref())?;
        if c.dispatch_id.as_deref() != Some(d.id()) {
            return Err(Error::msg(format!(
                "dispatch {} is not card {}'s current dispatch",
                d.id(),
                c.number
            )));
        }
    }
    let deadline = format_time(now() + budget_seconds(d.req, crate::testing::minute_seconds()));
    // The supervisor session the worker runs under: a Claude worker dies with it.
    let (event, _) = append_event(
        &run.paths,
        Event::WorkerStarted(events::WorkerStarted {
            dispatch_id: Some(d.id().to_string()),
            worker_id: Some(worker),
            card: d.req.card.clone(),
            deadline: Some(deadline),
            session: current_session(run),
            ..events::WorkerStarted::default()
        }),
    )?;
    Ok(RecordOutput::Event(Box::new(event)))
}

/// `record result`: the worker's reply, or for a Codex route the runner's result
/// (`_record_result`).
fn record_result(run: &Run, f: &RecordFields) -> Result<RecordOutput> {
    let reply = f.reply.clone().unwrap_or_default();
    if f.reply_file.is_some() && !reply.is_empty() {
        return Err(Error::msg(
            "record result takes a reply or a reply file, not both",
        ));
    }
    let log = Log::read(&run.paths)?;
    let d = known_dispatch(&log, f.dispatch.as_ref())?;
    if let Some(path) = &f.reply_file {
        check_reply_file(run, &d, path)?;
    }
    if d.result.is_some() {
        let raw = d.result_raw.expect("a result has its line");
        return Ok(RecordOutput::Existing(raw.clone()));
    }
    let Some(started) = d.started else {
        return Err(Error::msg(format!(
            "dispatch {} has no started worker; record started first",
            d.id()
        )));
    };
    if started.worker_id != f.worker {
        return Err(Error::msg(format!(
            "dispatch {} started worker {}, not {}",
            d.id(),
            started
                .worker_id
                .as_ref()
                .map_or_else(|| "None".into(), PyStr::shown),
            f.worker
                .as_ref()
                .map_or_else(|| "None".into(), PyStr::shown)
        )));
    }
    let worker = f.worker.clone().unwrap_or_default();
    if d.vendor() == "codex" {
        return record_codex_result(run, &log, &d, &worker, f.stopped);
    }
    let reply = match &f.reply_file {
        Some(path) => read_reply_file(run, &d, path)?,
        None => reply,
    };
    store_result(run, &d, &worker, &reply, f.stopped, false, true)
}

/// The dispatch's own reply file (`_reply_file`).
fn reply_file_of(run: &Run, d: &Dispatch) -> Result<PathBuf> {
    let card = d.req.card.as_ref().and_then(CardRef::number);
    Ok(scratch_dir(&run.ws, card, d.id())?.join(REPLY_FILE))
}

/// Only the dispatch's own reply file is read (security review L1): the CLI runs outside the
/// sandbox, so any other path would copy a file the worker cannot read into the run
/// (`_check_reply_file`).
fn check_reply_file(run: &Run, d: &Dispatch, path: &PyStr) -> Result<()> {
    let expected = reply_file_of(run, d)?;
    let given_path = PathBuf::from(path.to_os()?);
    let same = matches!((abspath(&given_path), abspath(&expected)), (Ok(a), Ok(b)) if a == b);
    if !same {
        return Err(Error::msg(format!(
            "refused: --reply-file must be dispatch {}'s reply file {}",
            d.id(),
            expected.display()
        )));
    }
    if expected.is_symlink() || !resolves_in_place(&run.ws, &expected) {
        return Err(Error::msg(format!(
            "refused: the reply file {} is a symlink or lies behind one",
            expected.display()
        )));
    }
    Ok(())
}

/// The reply file's text (`_read_reply_file`): read through the scratch folder's checked
/// descriptor, so never through a symlink, and only a regular file. A missing or unreadable
/// file gives a note instead; the status then comes from the report head.
fn read_reply_file(run: &Run, d: &Dispatch, path: &PyStr) -> Result<PyStr> {
    let card = d.req.card.as_ref().and_then(CardRef::number);
    let reason = match read_scratch_file(&run.ws, card, d.id(), REPLY_FILE) {
        Ok(Some(data)) => return Ok(PyStr::from(String::from_utf8_lossy(&data).into_owned())),
        Ok(None) => PyStr::from("No such file or directory"),
        Err(e @ ScratchError::Refused(_)) => return Err(e.into()),
        Err(e) => e.reason(),
    };
    let mut note = PyStr::from("(leadhand: the reply file ");
    note.push(path);
    note.push_str(" could not be read: ");
    note.push(&reason);
    note.push_str("; the status comes from the report)\n");
    Ok(note)
}

/// A Codex route's result comes only from the runner's `result.json`, never from the proxy's
/// reply (`_record_codex_result`): while the runner runs nothing is recorded and `wait` is
/// returned; with no live runner and no result, or for `--stopped`, the runner is stopped and
/// the dispatch finalized first.
fn record_codex_result(
    run: &Run,
    log: &Log,
    d: &Dispatch,
    worker: &PyStr,
    stopped: bool,
) -> Result<RecordOutput> {
    let wd = run.root.join("workers").join(d.id());
    let mut result = load_result(&wd)?;
    if result.is_none() && !stopped && runner_alive(&wd)? {
        let whose = match &d.req.card {
            None => "the run".to_string(),
            Some(c) => format!("card {}", c.text().lossy()),
        };
        return Ok(RecordOutput::Action(Action::Wait(WaitAction {
            action: tag::Wait,
            dispatch_id: d.id().to_string(),
            worker_id: Some(worker.clone()),
            deadline: d.started.and_then(|s| s.deadline.clone()),
            reply_file: path_text(&reply_file_of(run, d)?),
            reason: format!(
                "the Codex worker for {whose} is still running; its result is recorded from \
                 the runner when it finishes (the timer reports it)"
            ),
        })));
    }
    if result.is_none() {
        let cards = log.cards(&run.numbers());
        result = Some(cancel_codex(run, &cards, d)?);
    }
    let result = result.expect("a result was loaded or made");
    // A runner that finished before the cancel keeps its own status.
    let reply = PyStr::from(crate::dispatch::cmd::reply_lines(&result).join("\n"));
    store_result(
        run,
        d,
        worker,
        &reply,
        result.exit_reason == ExitReason::Cancelled,
        false,
        true,
    )
}

fn path_text(p: &Path) -> PyStr {
    PyStr::from_os(pure_path(p.as_os_str()).as_os_str())
}

/// Copies a worker's file into the run folder once (`_copy_if_missing`): `text`, or the
/// dispatch's scratch file `src`, read through the scratch folder's checked descriptor (never
/// through a symlink, and only a regular file); anything else is noted. Whether the copy is
/// there.
fn copy_if_missing(
    d: &Dispatch,
    src: Option<&PyStr>,
    dest: &Path,
    text: Option<&PyStr>,
    notes: &mut Vec<PyStr>,
) -> Result<bool> {
    if dest.exists() {
        return Ok(true);
    }
    let data = match text {
        Some(text) => text.to_os()?.into_encoded_bytes(),
        None => {
            let Some(src) = src else {
                return Ok(false);
            };
            match read_scratch(d, src) {
                Ok(Some(data)) => data,
                Ok(None) => return Ok(false),
                Err(reason) => {
                    let mut note = src.clone();
                    note.push_str(
                        " not copied: it is a symlink, lies behind one, is not a regular file or \
                         cannot be opened (",
                    );
                    note.push(&reason);
                    note.push_str(")");
                    notes.push(note);
                    return Ok(false);
                }
            }
        }
    };
    write_file_atomic(dest, &data)?;
    Ok(true)
}

/// `path` (`<ws>/.leadhand/<card or run>/<dispatch id>/<name>`, as the request names it) read
/// through the scratch folder's checked descriptor; `None` when missing; the reason it could
/// not be read (`_read_scratch`).
fn read_scratch(d: &Dispatch, path: &PyStr) -> std::result::Result<Option<Vec<u8>>, PyStr> {
    let not_scratch = || PyStr::from("not in the dispatch's scratch folder");
    let path = pure_path(&path.to_os().map_err(|_| not_scratch())?);
    let ws: PathBuf = match path.ancestors().nth(4) {
        Some(ws) => ws.to_path_buf(),
        None => return Err(not_scratch()),
    };
    let card = d.req.card.as_ref().and_then(CardRef::number);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let folder = scratch_dir(&ws, card, d.id()).map_err(|e| PyStr::from(e.message))?;
    if folder.join(&name) != path {
        return Err(not_scratch());
    }
    read_scratch_file(&ws, card, d.id(), &name).map_err(|e| e.reason())
}

/// The head of a result (`_head`): the report's head line; else, for an interrupted worker,
/// SPENT marked interrupted; for a stopped one, SPENT; then the reply's head, the Codex
/// runner's status, a Claude turn-limit note (SPENT), else BLOCKED with the raw reply kept.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Head {
    status: String,
    blocked_by: String,
    head: String,
    verdict: Option<String>,
    result: Option<String>,
    source: &'static str,
    raw_kept: bool,
    interrupted: bool,
    cross_checked: bool,
}

impl Head {
    fn status(status: &str, blocked_by: &str, source: &'static str, raw_kept: bool) -> Head {
        Head {
            status: status.to_string(),
            blocked_by: blocked_by.to_string(),
            head: "Status".into(),
            verdict: None,
            result: None,
            source,
            raw_kept,
            interrupted: false,
            cross_checked: false,
        }
    }
}

/// `_has_status`: text that is not blank and has a status line.
fn has_status(text: Option<&str>) -> Result<bool> {
    match text {
        Some(t) if !t.trim_matches(py_isspace).is_empty() => Ok(has_status_line(t)?),
        _ => Ok(false),
    }
}

fn head_of(
    report: Option<&str>,
    reply: &str,
    codex: Option<&DispatchResult>,
    stopped: bool,
    interrupted: bool,
) -> Result<Head> {
    let from_text = |text: &str, source: &'static str| -> Result<Head> {
        let (key, value, blocked_by) = parse_report_head(text)?;
        let mut out = Head {
            status: if key == "Status" { value } else { "READY" }.to_string(),
            blocked_by: blocked_by.to_string(),
            head: key.to_string(),
            verdict: (key == "Verdict").then(|| value.to_string()),
            result: (key == "Result").then(|| value.to_string()),
            source,
            raw_kept: false,
            interrupted: false,
            cross_checked: false,
        };
        if out.verdict.as_deref() == Some("clean")
            && findings_sections(text).iter().any(|s| severe_finding(s))
        {
            out.verdict = Some("defects".into());
            out.cross_checked = true;
        }
        Ok(out)
    };
    if has_status(report)? {
        return from_text(report.unwrap_or_default(), "report");
    }
    if interrupted {
        let mut out = Head::status("SPENT", "", "interrupted", false);
        out.interrupted = true;
        return Ok(out);
    }
    if stopped {
        return Ok(Head::status("SPENT", "", "stopped", false));
    }
    if has_status(Some(reply))? {
        return from_text(reply, "reply");
    }
    if let Some(codex) = codex {
        return Ok(Head::status(
            codex.status.as_str(),
            codex.blocked_by.as_str(),
            "runner",
            true,
        ));
    }
    if turn_limit(reply) {
        return Ok(Head::status("SPENT", "", "turn limit", true));
    }
    Ok(Head::status("BLOCKED", "", "unparsed", true))
}

/// The `## Findings` sections of a reviewer's report (`_FINDINGS_SECTION.finditer`): from a
/// `## Findings` line to the next `## ` heading.
fn findings_sections(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(start) =
        line_starts(text).find(|s| *s >= from && findings_head(&text[*s..]).is_some())
    {
        let group = start + findings_head(&text[start..]).expect("found above");
        let end = (group..=text.len())
            .filter(|i| text.is_char_boundary(*i))
            .find(|i| *i == text.len() || at_h2(text, *i))
            .unwrap_or(text.len());
        out.push(&text[group..end]);
        if end <= from {
            break;
        }
        from = end;
    }
    out
}

/// `^##[ \t]*Findings[ \t\r]*$` at the start of `s`: where the heading's line ends.
fn findings_head(s: &str) -> Option<usize> {
    let rest = s.strip_prefix("##")?.trim_start_matches([' ', '\t']);
    let (_, after) = after_folded(rest, "findings")?;
    let after_marks = after.trim_start_matches([' ', '\t', '\r']);
    (after_marks.is_empty() || after_marks.starts_with('\n')).then(|| s.len() - after_marks.len())
}

/// `^##\s` at `at`.
fn at_h2(text: &str, at: usize) -> bool {
    (at == 0 || text.as_bytes()[at - 1] == b'\n')
        && text[at..]
            .strip_prefix("##")
            .and_then(|r| r.chars().next())
            .is_some_and(py_isspace)
}

/// `_SEVERE_FINDING.search(section)`: a numbered or bulleted line whose severity tag is high or
/// medium (`1. [high] …`, `- [Medium] …`).
fn severe_finding(section: &str) -> bool {
    line_starts(section).any(|at| {
        let rest = section[at..].trim_start_matches(py_isspace);
        let digits: usize = rest
            .chars()
            .take_while(|c| is_decimal(*c))
            .map(char::len_utf8)
            .sum();
        let rest = if digits > 0 {
            match rest[digits..].strip_prefix(['.', ')']) {
                Some(r) => r,
                None => return false,
            }
        } else if let Some(r) = rest.strip_prefix(['-', '*']) {
            r
        } else {
            return false;
        };
        let Some(rest) = rest.trim_start_matches(py_isspace).strip_prefix('[') else {
            return false;
        };
        ["high", "medium"]
            .iter()
            .any(|level| after_folded(rest, level).is_some_and(|(_, after)| after.starts_with(']')))
    })
}

/// `_TURN_LIMIT.search(reply)`: `stopped at its <n>-turn limit`, any case.
fn turn_limit(reply: &str) -> bool {
    reply.char_indices().any(|(at, _)| {
        let Some((_, rest)) = after_folded(&reply[at..], "stopped at its ") else {
            return false;
        };
        let digits: usize = rest
            .chars()
            .take_while(|c| is_decimal(*c))
            .map(char::len_utf8)
            .sum();
        digits > 0 && after_folded(&rest[digits..], "-turn limit").is_some()
    })
}

/// Whether a cancel is recorded, or requested and not yet seen by `next` (`_cancelling`).
fn cancelling(run: &Run, log: &Log) -> bool {
    log.cancelled() || run.paths.cancel_flag.exists()
}

/// Copies the dispatch's files, works out its head, charges the worker and appends the
/// attempt and `result_recorded`, each repeat-safe (`_store_result`). `interrupted`: the
/// worker died with an earlier supervisor session; without a report the result is marked
/// interrupted. `charge` false: no worker is known (its start was never recorded), so nothing
/// is charged and no attempt is written.
pub(crate) fn store_result(
    run: &Run,
    d: &Dispatch,
    worker: &PyStr,
    reply: &PyStr,
    stopped: bool,
    interrupted: bool,
    charge_it: bool,
) -> Result<RecordOutput> {
    let did = d.id();
    crash_point("before_result_record");
    let req = d.req;
    let folder = run.card_dir(req.card.as_ref(), did);
    std::fs::create_dir_all(&folder).map_err(|e| Error::io(&folder, e))?;
    let mut files: PyMap<PyStr> = PyMap::default();
    let mut skipped: Vec<PyStr> = Vec::new();
    for (name, src) in [("brief", &req.brief), ("report", &req.report)] {
        let dest = folder.join(format!("{name}.md"));
        if copy_if_missing(d, src.as_ref(), &dest, None, &mut skipped)? {
            files.0.push((name.into(), path_text(&dest)));
        }
    }
    let reply_copy = folder.join("reply.md");
    copy_if_missing(d, None, &reply_copy, Some(reply), &mut skipped)?;
    files.0.push(("reply".into(), path_text(&reply_copy)));
    let codex_vendor = d.vendor() == "codex";
    let codex = if codex_vendor {
        load_result(&run.root.join("workers").join(did))?
    } else {
        None
    };
    let report_text = read_text(&path_text(&folder.join("report.md")))?;
    let mut head = head_of(
        report_text.as_deref(),
        &reply.lossy(),
        codex.as_ref(),
        stopped,
        interrupted,
    )?;
    if let Some(result) = &codex
        && commit_failed(result)
    {
        // The runner could not commit the worker's changes (R39): whatever the report says,
        // the work is not on the branch, so the card must not go on as if it were.
        head.status = "BLOCKED".into();
        head.blocked_by = "environment".into();
        head.head = "Status".into();
        head.verdict = None;
        head.result = None;
        head.source = "runner commit";
        head.interrupted = false;
    }
    let implementer_side = IMPLEMENTER_KINDS.contains(&d.kind()) || d.kind() == "final_fix";
    if !codex_vendor
        && implementer_side
        && (stopped || head.interrupted)
        && cancelling(run, &Log::read(&run.paths)?)
    {
        // R42: a Claude worker the cancel stopped, or found dead, leaves its partial work
        // committed on its branch.
        return Err(Error::msg(
            "not ported yet: committing a cancelled worker's partial work (Task 17)",
        ));
    }
    let tip = if IMPLEMENTER_KINDS.contains(&d.kind()) {
        let log = Log::read(&run.paths)?;
        let cards = log.cards(&run.numbers());
        let c = card_state(&cards, req.card.as_ref())?;
        let branch = c
            .branch
            .clone()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| card_branch_name(&run.branch, &c.number.to_string()));
        Some(head_sha(run, &branch)?)
    } else if d.kind() == "final_fix" {
        Some(head_sha(run, &card_branch_name(&run.branch, "final"))?)
    } else {
        None
    };
    crash_point("after_result_files");

    let role = if codex_vendor { "proxy" } else { d.role() };
    let mut charge_id = PyStr::from(format!("{did}:claude:"));
    charge_id.push(worker);
    let charged = if charge_it {
        let transcript = claude_subagent_transcript(worker, &projects_dir());
        Some(charge(
            &run.paths,
            &charge_id,
            worker,
            did,
            role,
            ChargeSource::ClaudeTranscript(transcript.as_deref()),
            stopped || head.interrupted,
        )?)
    } else {
        None
    };
    crash_point("after_charge_before_attempt");
    let event = |charge_ids: Vec<PyStr>, reconciled: bool| {
        result_event(
            d,
            worker,
            tip.clone(),
            codex.as_ref(),
            files.clone(),
            stopped,
            charge_ids,
            reconciled,
            &head,
            &skipped,
        )
    };
    let Some(charged) = charged else {
        let _lock = state_lock(&run.root)?;
        let log = Log::read(&run.paths)?;
        if let Some(raw) = log.dispatch(Some(did)).and_then(|again| again.result_raw) {
            return Ok(RecordOutput::Existing(raw.clone()));
        }
        let (written, _) = append_event(&run.paths, event(Vec::new(), true))?;
        return Ok(RecordOutput::Event(Box::new(written)));
    };

    let ended_at = utc_now();
    // A Claude worker's model and effort are the route's (a Codex proxy has none of its own);
    // its cost is priced with the route's model (two prices per Claude attempt: status.json
    // prices the same charge with the transcript's).
    let model = (!codex_vendor).then(|| PyStr::from(req.model.clone().unwrap_or_default()));
    let priced = model.as_ref().filter(|m| !m.is_empty());
    let known = charged.usage.known;
    let cost = match priced {
        Some(m) if known => estimate_cost(&charged.usage, Some(m), &run.config.prices),
        _ => None,
    };
    let effort = priced.map(|_| PyStr::from(req.effort.clone().unwrap_or_default()));
    let agent = if codex_vendor {
        CODEX_AGENT
    } else {
        agent_for(d.role(), "claude")?
    };
    let exit_reason = if stopped {
        "stopped"
    } else if head.interrupted {
        "interrupted"
    } else {
        "completed"
    };
    let attempt = AttemptRecord {
        dispatch: Some(did.to_string()),
        run: run.run_id.clone(),
        card: req.card.clone(),
        attempt: req.attempt,
        role: Some(role.to_string()),
        vendor: Some("claude".into()),
        effort,
        model,
        agent: Some(agent.to_string()),
        worker_id: Some(worker.clone()),
        started_at: d.started.map(|s| s.ts.clone()),
        ended_at: Some(ended_at),
        steps: None,
        charge_ids: vec![charge_id.clone()],
        usage: known.then(|| charged.usage.clone()),
        usage_known: Some(known),
        cost_estimate: cost,
        exit_reason: Some(exit_reason.into()),
        status: Some(head.status.clone()),
        blocked_by: Some(head.blocked_by.clone()),
        recorded_by: Some("engine".into()),
        ..AttemptRecord::default()
    };
    let _lock = state_lock(&run.root)?;
    let written = read_attempts(&run.paths)?
        .iter()
        .any(|a| a.dispatch.as_deref() == Some(did) && a.recorded_by.as_deref() == Some("engine"));
    if !written {
        append_attempt(&run.paths, &attempt)?;
    }
    let log = Log::read(&run.paths)?;
    if let Some(raw) = log.dispatch(Some(did)).and_then(|again| again.result_raw) {
        return Ok(RecordOutput::Existing(raw.clone()));
    }
    let (written, _) = append_event(&run.paths, event(vec![charge_id], interrupted))?;
    Ok(RecordOutput::Event(Box::new(written)))
}

/// The commit a branch is at.
fn head_sha(run: &Run, branch: &PyStr) -> Result<String> {
    head(&run.git()?, &branch_ref(branch)?)
}

/// `dispatch.commit_failed`: the worker left changes the runner could not commit.
fn commit_failed(result: &DispatchResult) -> bool {
    if result.committed.as_deref().is_some_and(|c| !c.is_empty()) || !result.changes {
        return false;
    }
    let reason = |r: &Option<PyStr>| r.as_ref().is_some_and(|t| !t.is_empty());
    reason(&result.commit_error) || reason(&result.commit_skipped)
}

/// The `result_recorded` event, in the Python's field order.
#[allow(clippy::too_many_arguments)]
fn result_event(
    d: &Dispatch,
    worker: &PyStr,
    tip: Option<String>,
    codex: Option<&DispatchResult>,
    files: PyMap<PyStr>,
    stopped: bool,
    charge_ids: Vec<PyStr>,
    reconciled: bool,
    head: &Head,
    skipped: &[PyStr],
) -> Event {
    let flag = |on: bool| on.then_some(Some(true));
    let mut e = events::ResultRecorded {
        dispatch_id: Some(d.id().to_string()),
        card: d.req.card.clone(),
        attempt: d.req.attempt,
        dispatch_kind: d.req.dispatch_kind.clone(),
        worker_id: Some(worker.clone()),
        tip,
        session_id: codex.and_then(|c| c.session_id.clone()).map(PyStr::from),
        files: Some(files),
        stopped: Some(stopped),
        charge_ids: Some(charge_ids),
        reconciled: flag(reconciled),
        status: Some(head.status.clone()),
        blocked_by: Some(head.blocked_by.clone()),
        head: Some(head.head.clone()),
        verdict: head.verdict.clone(),
        result: head.result.clone(),
        source: Some(head.source.to_string()),
        raw_kept: Some(head.raw_kept),
        interrupted: flag(head.interrupted),
        cross_checked: flag(head.cross_checked),
        files_skipped: (!skipped.is_empty()).then(|| Some(skipped.to_vec())),
        ..events::ResultRecorded::default()
    };
    // A Codex route's result carries the runner's commit outcome (`dsp.load_commit`: each
    // field `null` when `result.json` lacks it); `gitlinks_skipped` only when there are some.
    if d.vendor() == "codex" {
        e.committed = Some(codex.and_then(|c| c.committed.clone()));
        e.commit_error = Some(codex.and_then(|c| c.commit_error.clone()));
        e.commit_skipped = Some(codex.and_then(|c| c.commit_skipped.clone()));
        e.gitlinks_skipped = codex
            .and_then(|c| c.gitlinks_skipped.clone())
            .filter(|g| !g.is_empty())
            .map(Some);
    }
    Event::ResultRecorded(e)
}

/// `record ruling`: the supervisor's answer to a worker's question (`_record_ruling`).
fn record_ruling(run: &Run, f: &RecordFields) -> Result<RecordOutput> {
    let log = Log::read(&run.paths)?;
    not_ended(&log)?;
    let cards = log.cards(&run.numbers());
    let c = named_card(&cards, f.card)?;
    let d = log.dispatch(Some(c.dispatch_id.as_deref().unwrap_or_default()));
    let asked = d.filter(|d| asked_question(c, d));
    let Some(d) = asked else {
        return Err(Error::msg(format!(
            "card {} has no question waiting for a ruling",
            c.number
        )));
    };
    if has_ruling(&log, d.id()) {
        return Err(Error::msg(format!(
            "the question from {} already has a ruling",
            d.id()
        )));
    }
    let text = f.text.as_ref().map(strip_pystr).unwrap_or_default();
    if text.is_empty() {
        return Err(Error::msg("a ruling needs text"));
    }
    let (event, _) = append_event(
        &run.paths,
        Event::Ruling(events::Ruling {
            card: Some(CardRef::Number(c.number.into())),
            dispatch_id: Some(d.id().to_string()),
            text: Some(text),
            ..events::Ruling::default()
        }),
    )?;
    Ok(RecordOutput::Event(Box::new(event)))
}

/// Whether the card's current dispatch is an implementer's that asked a question.
fn asked_question(c: &CardState, d: &Dispatch) -> bool {
    c.state == CardStage::Reported
        && IMPLEMENTER_KINDS.contains(&d.kind())
        && d.result
            .is_some_and(|r| r.status.as_deref() == Some("QUESTION"))
}

fn has_ruling(log: &Log, dispatch_id: &str) -> bool {
    log.events
        .iter()
        .any(|e| matches!(e, Event::Ruling(r) if r.dispatch_id.as_deref() == Some(dispatch_id)))
}

/// `record park`: the card set aside (`_record_park`). A card parked on its worker's question
/// waits on the operator's decision (R44).
fn record_park(run: &Run, f: &RecordFields) -> Result<RecordOutput> {
    let log = Log::read(&run.paths)?;
    not_ended(&log)?;
    let cards = log.cards(&run.numbers());
    let c = named_card(&cards, f.card)?;
    if matches!(c.state, CardStage::Accepted | CardStage::Parked) {
        return Err(Error::msg(format!(
            "card {} is already {}",
            c.number,
            c.state.as_str()
        )));
    }
    let reason = f.reason.as_ref().map(strip_pystr).unwrap_or_default();
    if reason.is_empty() {
        return Err(Error::msg("parking a card needs a reason"));
    }
    let d = log.dispatch(Some(c.dispatch_id.as_deref().unwrap_or_default()));
    let decision = d.is_some_and(|d| asked_question(c, &d) && !has_ruling(&log, d.id()));
    let (event, _) = append_event(
        &run.paths,
        Event::Parked(events::Parked {
            card: Some(CardRef::Number(c.number.into())),
            reason: Some(reason),
            decision: decision.then_some(Some(true)),
            ..events::Parked::default()
        }),
    )?;
    Ok(RecordOutput::Event(Box::new(event)))
}

/// `record diff-check`: whether a test-only fix's diff is acceptable (`_record_diff_check`).
fn record_diff_check(run: &Run, f: &RecordFields) -> Result<RecordOutput> {
    let log = Log::read(&run.paths)?;
    not_ended(&log)?;
    let cards = log.cards(&run.numbers());
    let c = named_card(&cards, f.card)?;
    let d = log.dispatch(Some(c.dispatch_id.as_deref().unwrap_or_default()));
    let pending = d.filter(|d| {
        c.state == CardStage::Reported
            && IMPLEMENTER_KINDS.contains(&d.kind())
            && d.result
                .is_some_and(|r| r.status.as_deref() == Some("READY"))
            && log
                .fix_round(c.number, d.attempt(), Some(d.seq()))
                .is_some()
            && run
                .card_dir(d.req.card.as_ref(), d.id())
                .join("fix.diff")
                .exists()
    });
    let Some(d) = pending else {
        return Err(Error::msg(format!(
            "card {} has no diff check waiting",
            c.number
        )));
    };
    let checked = log
        .events
        .iter()
        .any(|e| matches!(e, Event::DiffChecked(k) if k.dispatch_id.as_deref() == Some(d.id())));
    if checked {
        return Err(Error::msg(format!(
            "the diff of {} was already checked",
            d.id()
        )));
    }
    let (event, _) = append_event(
        &run.paths,
        Event::DiffChecked(events::DiffChecked {
            card: Some(CardRef::Number(c.number.into())),
            dispatch_id: Some(d.id().to_string()),
            ok: Some(f.ok.unwrap_or(false)),
            note: Some(strip_pystr(&f.note)),
            ..events::DiffChecked::default()
        }),
    )?;
    Ok(RecordOutput::Event(Box::new(event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use crate::wire::tests::python;

    const DECK: &str = "# Deck: Demo deck\nSource: docs/plan.md\nBase: main @ BASE\n\
        Branch: leadhand/demo\nBuild: true\nTest: true\n\n\
        ## Card 1: Card number 1\n- Role: implementer\n- Hard: no\n- Risk: low\n\
        - Depends on: none\n- Allowed paths: src/, tests/\n\
        - Done when: the card's file exists and the tests pass\n- Stop if: the design has to change\n";

    /// A run directory whose card 1 has a started review, `c1-a1-review-1`, in `ws` (the
    /// fixture repository itself), with the configuration snapshotted from an empty user file.
    fn started_review(f: &Fixture) -> (PathBuf, PathBuf) {
        let ws = f.make_repo("work space");
        let base = f.git(&ws, &["rev-parse", "HEAD"]);
        let root = f.root().join("docs dir/runs/2026-11-14-demo-deck");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("deck.md"), DECK.replace("BASE", &base)).unwrap();
        let user = f.root().join("user.toml");
        std::fs::write(&user, "").unwrap();
        let config = crate::config::load_config_from(&ws, &user).unwrap();
        crate::store::write_json_atomic(
            &root.join(crate::config::SNAPSHOT),
            &crate::config::ConfigSnapshot(&config),
        )
        .unwrap();
        let meta = serde_json::json!({
            "run_id": "abc123abc123", "repo": ws, "deck_source": root.join("deck.md"),
            "deck_digest": "0", "base_ref": "main", "base_sha": base, "branch": "leadhand/demo",
            "branch_requested": "leadhand/demo", "workspace": ws, "slug": "demo-deck",
            "created_at": "2026-11-14T00:00:00.000Z", "supervisor_sessions": [],
        });
        std::fs::write(root.join("run.json"), meta.to_string()).unwrap();
        let scratch = ws.join(".leadhand/1/c1-a1-review-1");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("brief.md"), "# Brief\n").unwrap();
        let paths = crate::state::run_paths(&root);
        let request = format!(
            r#"{{"seq":1,"ts":"t","kind":"card_cut","card":1,"branch":"leadhand/demo--card-1","base":"{base}"}}
{{"seq":2,"ts":"t","kind":"dispatch_requested","dispatch_id":"c1-a1-review-1","card":1,"attempt":1,"dispatch_kind":"review","role":"reviewer","vendor":"claude","model":"sonnet","effort":"medium","brief":"{b}","report":"{r}","extra":{{}},"notes":"","resumes":null,"resume_worker":null,"resume_session":null,"minutes":60}}
{{"seq":3,"ts":"t","kind":"worker_started","dispatch_id":"c1-a1-review-1","worker_id":"agent0001","card":1,"deadline":"2099-01-01T00:00:00Z","session":null}}
"#,
            b = scratch.join("brief.md").display(),
            r = scratch.join("report.md").display()
        );
        std::fs::write(&paths.events, request).unwrap();
        (root, scratch)
    }

    // S2-R32: the Python's `KeyError` on a crafted head escapes `record` after the report is
    // copied and before the charge, the attempt and `result_recorded`; every retry fails again,
    // because the copy persists.
    #[test]
    fn crafted_report_head_fails_record_as_the_python_does() {
        let f = Fixture::new();
        let (root, scratch) = started_review(&f);
        std::fs::write(scratch.join("report.md"), "re\u{17f}ult: pass\n").unwrap();
        let fields = RecordFields {
            dispatch: Some("c1-a1-review-1".into()),
            worker: Some("agent0001".into()),
            reply: Some("Verdict: clean".into()),
            ..RecordFields::default()
        };
        let paths = crate::state::run_paths(&root);
        let copy = root.join("cards/1/c1-a1-review-1/report.md");
        // `record`'s body, without its status.json checkpoint (which reads the Codex meter
        // from the operator's home): the charge, which reads Claude's transcripts there, is
        // never reached.
        for _ in 0..2 {
            let run = Run::load(&root).unwrap();
            let e = record_result(&run, &fields).unwrap_err();
            assert_eq!(
                (e.message.as_str(), e.exit),
                ("KeyError: 'Re\u{17f}ult'", 1)
            );
            assert_eq!(
                std::fs::read_to_string(&copy).unwrap(),
                "re\u{17f}ult: pass\n"
            );
            assert!(root.join("cards/1/c1-a1-review-1/reply.md").is_file());
            assert!(!paths.charges.exists());
            assert!(!paths.attempts.exists());
            let log = Log::read(&paths).unwrap();
            assert!(
                log.dispatch(Some("c1-a1-review-1"))
                    .unwrap()
                    .result
                    .is_none()
            );
        }
    }

    #[test]
    fn heads_cross_check_findings_as_the_python_does() {
        let texts = [
            "Verdict: clean\n## Findings\n2. [High] src/a.py:3 — leaks — close it\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\n  1) [medium] src/a.py:3 — x — y\n",
            "Verdict: clean\n## Findings\n1. [low] src/a.py:3 — nit — rename\n## Notes\n- [high] earlier round: fixed\n1. [medium] fixed\n",
            "Verdict: clean\n[high] src/a.py:3 outside any section\n## Findings\nNone\n",
            "Verdict: clean\n## Findings\n- [high] src/a.py:3 — leaks — close it\n",
            "Verdict: clean\n## Findings\n  * [Medium] src/a.py:3 — x — y\n## Notes\nRead the diff.\n",
            "Verdict: clean\n## Findings\nNone\n## Notes\n1. [high] src/widget.py:1 — fixed\n",
            "Verdict: clean\n##findings \r\n- [mediuM] a\n",
            "Verdict: clean\n## Findings\n\n\n   \n١٢. [hıgh] unicode digits\n",
            "Verdict: clean\n## Findings x\n- [high] not the heading\n",
            "Verdict: clean\n## Findings\nNone\n## Findings\n- [high] the second one\n",
            "Verdict: defects\n## Findings\n- [high] a\n",
            "Status: READY\n## Findings\n- [high] a\n",
            "Verdict: clean\n## Findings\n-[high]\n#### Findings\n",
            "NOTE: this agent stopped at its 150-turn limit before finishing.\n",
            "stopped at ITS 7-TURN LIMIT",
            "stopped at its -turn limit",
        ];
        let code = "import json, sys\nfrom leadhand import engine\n\
                    out = []\n\
                    for t in json.loads(sys.argv[1]):\n\
                    \x20   h = engine._head(t, '', None, False)\n\
                    \x20   out.append([h['status'], h['verdict'], h.get('cross_checked', False),\n\
                    \x20               bool(engine._TURN_LIMIT.search(t))])\n\
                    print(json.dumps(out))";
        let arg = serde_json::to_string(&texts).unwrap();
        let theirs: Vec<(String, Option<String>, bool, bool)> =
            serde_json::from_slice(&python(code, &[OsStr::new(&arg)])).unwrap();
        let ours: Vec<(String, Option<String>, bool, bool)> = texts
            .iter()
            .map(|t| {
                let h = head_of(Some(t), "", None, false, false).unwrap();
                (h.status, h.verdict, h.cross_checked, turn_limit(t))
            })
            .collect();
        assert_eq!(ours, theirs);
    }

    #[test]
    fn heads_fall_back_as_the_python_does() {
        let cases: [(Option<&str>, &str, bool, bool); 7] = [
            (None, "Status: READY\nReport: x", false, false),
            (Some("   \n"), "I changed some things.\n", false, false),
            (
                None,
                "NOTE: this agent stopped at its 150-turn limit.\n",
                false,
                false,
            ),
            (Some("Status: QUESTION\n"), "Status: READY", true, false),
            (None, "Status: READY", true, false),
            (None, "", false, true),
            (Some("Result: pass\n"), "", false, true),
        ];
        let code = "import json, sys\nfrom leadhand import engine\n\
                    out = []\n\
                    for report, reply, stopped, interrupted in json.loads(sys.argv[1]):\n\
                    \x20   h = engine._head(report, reply, None, stopped, interrupted)\n\
                    \x20   out.append([h['status'], h['head'], h['source'], h['raw_kept'], h.get('interrupted', False)])\n\
                    print(json.dumps(out))";
        let arg = serde_json::to_string(&cases).unwrap();
        let theirs: Vec<(String, String, String, bool, bool)> =
            serde_json::from_slice(&python(code, &[OsStr::new(&arg)])).unwrap();
        let ours: Vec<(String, String, String, bool, bool)> = cases
            .iter()
            .map(|(report, reply, stopped, interrupted)| {
                let h = head_of(*report, reply, None, *stopped, *interrupted).unwrap();
                (
                    h.status,
                    h.head,
                    h.source.to_string(),
                    h.raw_kept,
                    h.interrupted,
                )
            })
            .collect();
        assert_eq!(ours, theirs);
        // A crafted head the Python's parser raises KeyError on fails the same way (S2-R32).
        let e = head_of(Some("re\u{17f}ult: pass\n"), "", None, false, false).unwrap_err();
        assert_eq!(e.message, "KeyError: 'Re\u{17f}ult'");
    }
}
