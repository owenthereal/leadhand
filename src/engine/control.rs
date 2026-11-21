//! The operator's controls (`engine.request_pause`, `request_cancel`, `_flag`, `unpark`;
//! `cli._cmd_unpark`), and the commands a person types for them.
//!
//! `pause` and `cancel` only write their flag, which the next `next` records and acts on
//! (`recover::record_flag`, `next::recovery`); `unpark` only appends `unparked`, which the fold
//! reads. The flags and the event are exactly the Python's, so a Python `next` reads them too.
//! None of them refuses an in-place run, as the Python's do not: they never touch the
//! workspace, and `next` refuses such a run before acting on anything they wrote.
//!
//! The commands find the run as `status` does (`runsel`), take `--json`, and print an error as
//! `error: …`, with the one command that fixes it alone on the last line when there is one.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use super::cards::{Log, Run, card_state, py_absolute};
use super::record::read_stdin;
use crate::briefs_write::strip_pystr;
use crate::cli::human::shell_quote;
use crate::error::{Error, Result};
use crate::runsel;
use crate::state::events::{self, Outcome};
use crate::state::{CardRef, CardStage, Event, append_event, run_paths, utc_now};
use crate::store::{state_lock, write_json_atomic};
use crate::wire::{self, PyStr};

/// Which flag a request writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Pause,
    Cancel,
}

impl Request {
    fn as_str(self) -> &'static str {
        match self {
            Request::Pause => "pause",
            Request::Cancel => "cancel",
        }
    }
}

/// A flag file, in the Python's key order.
#[derive(Serialize)]
struct FlagRecord {
    requested_at: String,
    by_pid: u32,
}

/// `leadhand pause` (`engine.request_pause`): writes the pause flag. Running workers finish,
/// nothing new starts, the handoff is written and the run ends blocked (paused); `run`
/// continues it. The run's directory, as the message names it.
pub fn pause(run_dir: &Path) -> Result<PathBuf> {
    flag(run_dir, Request::Pause)
}

/// `leadhand cancel` (`engine.request_cancel`): writes the cancel flag. Every running worker
/// is stopped (confirmed), then the run ends incomplete (cancelled); partial work stays on the
/// card branches. A blocked run can still be cancelled: a relaunch finishes it. The run's
/// directory, as the message names it.
pub fn cancel(run_dir: &Path) -> Result<PathBuf> {
    flag(run_dir, Request::Cancel)
}

/// `_flag`: refuses a directory with no `run.json`, and a run that has ended (a blocked one
/// only for a pause), then writes the flag.
fn flag(run_dir: &Path, what: Request) -> Result<PathBuf> {
    let root = py_absolute(run_dir)?;
    let paths = run_paths(&root);
    if !paths.run_json.is_file() {
        return Err(Error::msg(format!(
            "not a run directory (no run.json): {}",
            root.display()
        )));
    }
    let log = Log::read(&paths)?;
    if let Some(o) = log.ended()
        && (what == Request::Pause || o.outcome.as_deref() != Some("blocked"))
    {
        return Err(Error::msg(format!(
            "this run has ended: {}; there is nothing to {}",
            outcome_text(o),
            what.as_str()
        )));
    }
    let path = match what {
        Request::Pause => &paths.pause_flag,
        Request::Cancel => &paths.cancel_flag,
    };
    let record = FlagRecord {
        requested_at: utc_now(),
        by_pid: std::process::id(),
    };
    write_json_atomic(path, &record)?;
    Ok(root)
}

/// `outcome (qualifier)`, or the outcome alone (`_outcome_text`).
fn outcome_text(o: &Outcome) -> String {
    let outcome = o.outcome.as_deref().unwrap_or("None");
    match o.qualifier.as_deref().filter(|q| !q.is_empty()) {
        Some(q) => format!("{outcome} ({q})"),
        None => outcome.to_string(),
    }
}

/// `leadhand unpark` (`engine.unpark`): a parked card continues, its last worker resumed with
/// the note as the answer (or a fresh worker given the note, R32 and R44's rules in `next`).
/// Only a blocked or unfinished run reopens a parked card. The `unparked` event appended.
pub fn unpark(run_dir: &Path, card: i64, note: &PyStr) -> Result<Event> {
    let run = Run::load(run_dir)?;
    let note = strip_pystr(note);
    if note.is_empty() {
        return Err(Error::msg("unparking a card needs a note"));
    }
    let _lock = state_lock(&run.root)?;
    let log = Log::read(&run.paths)?;
    if let Some(o) = log.ended()
        && o.outcome.as_deref() != Some("blocked")
    {
        // Only a blocked run continues: a relaunch never reopens any other ending.
        let head = format!(
            "this run has ended: {}; a parked card can only continue in a blocked or unfinished \
             run. Start a new run from the deck with:",
            outcome_text(o)
        );
        return Err(crate::launch::new_run_refusal(&head, &run.meta));
    }
    let cards = log.cards(&run.numbers());
    let c = card_state(&cards, Some(&CardRef::Number(card)))?;
    if c.state != CardStage::Parked {
        return Err(Error::msg(format!(
            "card {} is not parked (it is {})",
            c.number,
            c.state.as_str()
        )));
    }
    let (event, _) = append_event(
        &run.paths,
        Event::Unparked(events::Unparked {
            card: Some(CardRef::Number(c.number.into())),
            note: Some(note),
            ..events::Unparked::default()
        }),
    )?;
    Ok(event)
}

// --------------------------------------------------------------------------
// The commands

/// `pause` and `cancel --json`: the same information as the text output.
#[derive(Serialize)]
struct Requested {
    run: PyStr,
    requested: &'static str,
    /// The command the message names: `run` continues a paused run, or finishes a cancel.
    command: PyStr,
    message: PyStr,
}

/// `unpark --json`.
#[derive(Serialize)]
struct Unparked {
    run: PyStr,
    card: i64,
    note: PyStr,
    message: String,
}

/// `leadhand pause [RUN] [--json]`.
pub fn pause_command(arg: Option<&OsStr>, json: bool) -> Result<ExitCode> {
    flag_command(arg, json, Request::Pause)
}

/// `leadhand cancel [RUN] [--json]`.
pub fn cancel_command(arg: Option<&OsStr>, json: bool) -> Result<ExitCode> {
    flag_command(arg, json, Request::Cancel)
}

fn flag_command(arg: Option<&OsStr>, json: bool, what: Request) -> Result<ExitCode> {
    let cwd = std::env::current_dir().map_err(|e| Error::io(Path::new("."), e))?;
    let run_dir = runsel::select_run(what.as_str(), arg, &cwd)?;
    let root = flag(&run_dir, what)?;
    let command = run_command(&root);
    let mut message = OsString::from(match what {
        Request::Pause => {
            "pause requested: running workers finish, nothing new starts, and the run ends \
             blocked (paused) with a handoff. Continue it later with: "
        }
        Request::Cancel => {
            "cancel requested: every running worker is stopped and the run ends incomplete \
             (cancelled); partial work stays on the card branches. If no supervisor is \
             running, "
        }
    });
    message.push(&command);
    if what == Request::Cancel {
        message.push(" finishes the cancel.");
    }
    let report = Requested {
        run: PyStr::from_os(root.as_os_str()),
        requested: what.as_str(),
        command: PyStr::from_os(&command),
        message: PyStr::from_os(&message),
    };
    print_line(&message, &report, json)
}

/// `<cli> run <run dir>`, each word quoted (`_run_command`).
fn run_command(root: &Path) -> OsString {
    let mut out = shell_quote(crate::testing::self_path().as_os_str());
    out.push(" run ");
    out.push(shell_quote(root.as_os_str()));
    out
}

/// `leadhand unpark [RUN] --card N --note NOTE [--json]` (`cli._cmd_unpark`): a note of `-`
/// is read from stdin.
pub fn unpark_command(
    arg: Option<&OsStr>,
    card: i64,
    note_arg: &OsStr,
    json: bool,
) -> Result<ExitCode> {
    let note = if note_arg == "-" {
        read_stdin()?
    } else {
        PyStr::from_os(note_arg)
    };
    let cwd = std::env::current_dir().map_err(|e| Error::io(Path::new("."), e))?;
    // A candidate's command repeats the card and the note, so it works as printed.
    let after = [
        OsString::from("--card"),
        OsString::from(card.to_string()),
        OsString::from("--note"),
        note_arg.to_os_string(),
    ];
    let run_dir = runsel::select_run_with("unpark", &after, arg, &cwd)?;
    let event = unpark(&run_dir, card, &note)?;
    let Event::Unparked(u) = event else {
        unreachable!("unpark appends `unparked`");
    };
    let message = format!("card {card} unparked");
    let report = Unparked {
        run: PyStr::from_os(py_absolute(&run_dir)?.as_os_str()),
        card: u.card.as_ref().and_then(CardRef::number).unwrap_or(card),
        note: u.note.unwrap_or_default(),
        message: message.clone(),
    };
    print_line(OsStr::new(&message), &report, json)
}

/// Prints `text` as one line, its bytes kept, or with `json`, `value` as one JSON object.
fn print_line<T: Serialize>(text: &OsStr, value: &T, json: bool) -> Result<ExitCode> {
    let mut out = std::io::stdout().lock();
    if json {
        writeln!(out, "{}", wire::to_output(value)?)?;
    } else {
        out.write_all(text.as_encoded_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}
