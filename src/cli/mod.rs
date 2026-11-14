//! The command line: all seventeen subcommands, with the Python's flags, positional
//! arguments and defaults (contract item 1), parsed by clap (usage errors exit 2, deliberate
//! change 4); the one caller-check entry point; and how a command's result becomes output
//! and an exit code.
//!
//! The commands a person types (`run`, `start`, `status`, `pause`, `cancel`, `unpark`,
//! `report`, `check`, `launch-line`, `version`) carry a one-line purpose and an example in
//! their help. Long flags may be abbreviated to any unambiguous prefix, as argparse allows.

pub mod human;

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

use crate::error::{EXIT_STILL_RUNNING, Error, Failure, Result};
use crate::wire::PyStr;
use crate::{config, deck};

#[derive(Debug, Parser)]
#[command(
    name = "leadhand",
    bin_name = "leadhand",
    about = "carry an approved deck through implementation and review with bounded workers",
    infer_long_args = true,
    subcommand_required = true,
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands, in the Python's order.
#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(
        about = "print the leadhand version",
        after_help = "Example:\n  leadhand version"
    )]
    Version,

    #[command(
        about = "let a parked card continue, with a note for its worker",
        after_help = "Example:\n  leadhand unpark ~/.leadhand/myapp/runs/2026-11-10-retry-uploads --card 2 \
                      --note 'Keep the v1 endpoint'"
    )]
    Unpark {
        run_dir: OsString,
        #[arg(long, allow_negative_numbers = true)]
        card: i64,
        #[arg(long, help = "the answer or instruction for the card; - reads stdin")]
        note: OsString,
    },

    #[command(
        about = "validate a deck before a run",
        after_help = "Example:\n  leadhand check docs/decks/retry-uploads.md --repo ."
    )]
    Check {
        deck: OsString,
        #[arg(long, default_value = ".")]
        repo: OsString,
        #[arg(long, help = "print the result as one JSON object")]
        json: bool,
    },

    #[command(about = "print the route for a card and role")]
    Route {
        deck: OsString,
        #[arg(allow_negative_numbers = true)]
        card: i64,
        role: OsString,
        #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
        attempt: i64,
        #[arg(long)]
        last_vendor: Option<Vendor>,
        #[arg(long, default_value = ".")]
        repo: OsString,
        /// The installed vendors, in place of the PATH lookup (hidden, as in the Python; part
        /// of the command surface in every build, not a test hook).
        #[arg(long, hide = true)]
        installed: Option<OsString>,
    },

    #[command(about = "run one Codex worker (started detached) and print its reply")]
    Dispatch(DispatchArgs),

    #[command(about = "stop a Codex dispatch and record its result")]
    StopDispatch {
        run_dir: OsString,
        #[arg(long)]
        dispatch: OsString,
    },

    #[command(hide = true)]
    DispatchRunner { worker_dir: OsString },

    #[command(
        about = "start a run for a deck (or report the unfinished one)",
        after_help = "Example:\n  leadhand start docs/decks/retry-uploads.md --repo ."
    )]
    Start {
        deck: OsString,
        #[arg(long, default_value = ".")]
        repo: OsString,
        #[arg(long, help = "start a new run even if one is unfinished")]
        new: bool,
        #[arg(long, help = "print the result as one JSON object")]
        json: bool,
    },

    #[command(about = "perform due steps and print the supervisor's next action")]
    Next { run_dir: OsString },

    #[command(about = "record a worker start or result, a ruling, a park or a diff check")]
    Record(RecordArgs),

    #[command(
        about = "pause a run: running workers finish, nothing new starts, it ends blocked",
        after_help = "Example:\n  leadhand pause ~/.leadhand/myapp/runs/2026-11-10-retry-uploads"
    )]
    Pause { run_dir: OsString },

    #[command(
        about = "cancel a run: stop every worker, end it incomplete (cancelled)",
        after_help = "Example:\n  leadhand cancel ~/.leadhand/myapp/runs/2026-11-10-retry-uploads"
    )]
    Cancel { run_dir: OsString },

    #[command(about = "wait until a dispatch has a result or its deadline passes")]
    Timer {
        run_dir: OsString,
        #[arg(long)]
        dispatch: OsString,
    },

    #[command(
        about = "own a run and start its supervisor session (attended use only)",
        after_help = "Example:\n  leadhand run ~/.leadhand/myapp/runs/2026-11-10-retry-uploads"
    )]
    Run {
        #[arg(help = RUN_HELP)]
        run_dir: Option<OsString>,
        #[arg(long, help = "load the leadhand plugin from this directory")]
        plugin_dir: Option<OsString>,
        #[arg(long = "print", hide = true)]
        print_mode: bool,
    },

    #[command(
        about = "print the exact command that launches or resumes a run",
        after_help = "Example:\n  leadhand launch-line ~/.leadhand/myapp/runs/2026-11-10-retry-uploads"
    )]
    LaunchLine {
        #[arg(help = RUN_HELP)]
        run_dir: Option<OsString>,
        #[arg(long, help = "load the leadhand plugin from this directory")]
        plugin_dir: Option<OsString>,
        #[arg(long, help = "print the line as one JSON object")]
        json: bool,
    },

    #[command(
        about = "show a run's headline, attention items, cards and usage",
        after_help = "Example:\n  leadhand status ~/.leadhand/myapp/runs/2026-11-10-retry-uploads"
    )]
    Status {
        #[arg(help = RUN_HELP)]
        run_dir: Option<OsString>,
        #[arg(long, help = "print the status as one JSON object")]
        json: bool,
    },

    #[command(
        about = "write report.md for a run and print its path",
        after_help = "Example:\n  leadhand report ~/.leadhand/myapp/runs/2026-11-10-retry-uploads"
    )]
    Report {
        #[arg(help = RUN_HELP)]
        run_dir: Option<OsString>,
        #[arg(long, help = "print the report's path as one JSON object")]
        json: bool,
    },
}

/// How the commands that find a run take it (spec, "Commands you type").
const RUN_HELP: &str = "the run: its directory, its deck.md, its run ID, its directory's name or \
                        its slug; default: the run whose worktree holds the current directory, \
                        else the only unfinished run of this repository";

#[derive(Debug, Args)]
pub struct DispatchArgs {
    #[arg(long, help = "the run directory")]
    pub run: OsString,
    #[arg(long)]
    pub dispatch_id: OsString,
    #[arg(long, allow_negative_numbers = true)]
    pub card: Option<i64>,
    #[arg(long, allow_negative_numbers = true)]
    pub attempt: i64,
    #[arg(long)]
    pub role: OsString,
    #[arg(long)]
    pub model: OsString,
    #[arg(long)]
    pub effort: OsString,
    #[arg(long)]
    pub brief: OsString,
    #[arg(long)]
    pub report: OsString,
    #[arg(long, help = "the workspace the worker runs in")]
    pub cwd: OsString,
    #[arg(long, help = "Codex session (thread) ID to resume")]
    pub resume: Option<OsString>,
    #[arg(long, help = "the dispatch kind (implement, fix, review, ...)")]
    pub kind: Option<OsString>,
    #[arg(long, help = "the branch the worker's changes are committed on")]
    pub branch: Option<OsString>,
}

#[derive(Debug, Args)]
pub struct RecordArgs {
    pub run_dir: OsString,
    pub kind: RecordKind,
    #[arg(long, help = "dispatch ID (started, result)")]
    pub dispatch: Option<OsString>,
    #[arg(long, help = "the worker's agent ID (started, result)")]
    pub worker: Option<OsString>,
    #[arg(
        long,
        conflicts_with = "reply_file",
        help = "the worker's reply (result); - reads stdin"
    )]
    pub reply: Option<OsString>,
    #[arg(long, help = "a file holding the worker's reply (result)")]
    pub reply_file: Option<OsString>,
    #[arg(long, help = "the worker was stopped at its deadline (result)")]
    pub stopped: bool,
    #[arg(
        long,
        allow_negative_numbers = true,
        help = "card number (ruling, park, diff-check)"
    )]
    pub card: Option<i64>,
    #[arg(long, help = "the ruling (ruling); - reads stdin")]
    pub text: Option<OsString>,
    #[arg(long, help = "why the card is parked (park)")]
    pub reason: Option<OsString>,
    #[arg(long, help = "whether the diff is acceptable (diff-check)")]
    pub ok: Option<YesNo>,
    #[arg(
        long,
        default_value = "",
        help = "what the diff check found (diff-check)"
    )]
    pub note: OsString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RecordKind {
    Started,
    Result,
    Ruling,
    Park,
    DiffCheck,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Vendor {
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum YesNo {
    Yes,
    No,
}

// --------------------------------------------------------------------------
// The caller

/// Who is calling: the run anchor a supervisor session (and every runner its `dispatch`
/// starts) carries. The one caller-check entry point: `main` resolves it once for every
/// handler, and Plan 2's run guard extends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// `$LEADHAND_RUN_DIR`, when set.
    pub run_dir: Option<PathBuf>,
    /// `$LEADHAND_RUN_ID`, when set and not empty, as given: unvalidated, so the run-anchor
    /// check compares it byte for byte with the run's ID.
    pub run_id: Option<OsString>,
}

/// The caller, from `LEADHAND_RUN_DIR` and `LEADHAND_RUN_ID`. A `LEADHAND_RUN_DIR` that is
/// set but empty (or only whitespace) is a cleared anchor, never an operator's terminal
/// (which has none at all), and is refused. Only the handlers that check the run anchor fail
/// on that refusal (`dispatch`, `stop-dispatch`, `dispatch-runner`); `main` keeps it for them.
pub fn resolve_caller() -> Result<Caller> {
    resolve_caller_in(&|name| std::env::var_os(name))
}

pub(crate) fn resolve_caller_in(env: &dyn Fn(&str) -> Option<OsString>) -> Result<Caller> {
    let run_dir = env("LEADHAND_RUN_DIR");
    // `not session.strip()`: Python's whitespace, U+001C..U+001F included.
    if let Some(dir) = &run_dir
        && crate::pytext::py_strip(&dir.to_string_lossy()).is_empty()
    {
        return Err(Error::msg("refused: $LEADHAND_RUN_DIR is set but empty"));
    }
    let run_id = env("LEADHAND_RUN_ID").filter(|id| !id.is_empty());
    Ok(Caller {
        run_dir: run_dir.map(PathBuf::from),
        run_id,
    })
}

// --------------------------------------------------------------------------
// Running a command

/// Parses the command line, resolves the caller, runs the command and prints its error.
pub fn main() -> ExitCode {
    // First, as every Python command does (R41a): drop the run.lock descriptor a supervisor's
    // Bash command inherited, so a long-lived command never keeps the run locked.
    let lock = std::env::var_os("LEADHAND_RUN_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("run.lock"));
    crate::sys::close_inherited_fd(lock.as_deref());
    let (command, name) = match parse(std::env::args_os()) {
        Ok(parsed) => parsed,
        Err(e) => {
            // Help goes to stdout with exit 0; a usage error to stderr with exit 2.
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let caller = resolve_caller();
    match run(command, &name, &caller) {
        Ok(code) => code,
        Err(e) => report(&e),
    }
}

/// The parsed command and its name as typed (the subcommand clap matched).
fn parse(
    argv: impl IntoIterator<Item = impl Into<OsString> + Clone>,
) -> std::result::Result<(Command, String), clap::Error> {
    let matches = Cli::command().try_get_matches_from(argv)?;
    let name = matches.subcommand_name().unwrap_or_default().to_owned();
    let cli = Cli::from_arg_matches(&matches)?;
    Ok((cli.command, name))
}

fn run(command: Command, name: &str, caller: &Result<Caller>) -> Result<ExitCode> {
    match command {
        Command::Version => version(),
        Command::Check { deck, repo, json } => check(&deck, &repo, json),
        Command::Route {
            deck,
            card,
            role,
            attempt,
            last_vendor,
            repo,
            installed,
        } => route(RouteArgs {
            deck: &deck,
            card,
            role: &role,
            attempt,
            last_vendor,
            repo: &repo,
            installed: installed.as_deref(),
        }),
        // An empty run argument is none at all, as the Python's `if args.run_dir` reads it.
        Command::Status { run_dir, json } => {
            crate::status::command(run_dir.as_deref().filter(|d| !d.is_empty()), json)
        }
        Command::Report { run_dir, json } => crate::report::command(run_dir.as_deref(), json),
        Command::Start {
            deck,
            repo,
            new,
            json,
        } => crate::engine::start::command(&deck, &repo, new, json),
        Command::Dispatch(args) => crate::dispatch::cmd::dispatch_command(&args, caller),
        Command::StopDispatch { run_dir, dispatch } => {
            crate::dispatch::cmd::stop_command(&run_dir, &dispatch, caller)
        }
        Command::DispatchRunner { worker_dir } => {
            crate::dispatch::cmd::runner_command(&worker_dir, caller)
        }
        Command::Run {
            run_dir,
            plugin_dir,
            print_mode,
        } => crate::launch::run_command(run_dir.as_deref(), plugin_dir.as_deref(), print_mode),
        Command::LaunchLine {
            run_dir,
            plugin_dir,
            json,
        } => crate::launch::launch_line_command(run_dir.as_deref(), plugin_dir.as_deref(), json),
        _ => Err(Error::msg(format!("not ported yet: {name}"))),
    }
}

/// Prints `e`: the `still running` line on stdout for exit 75; otherwise `error: <message>`
/// on stderr, then the fixing command, if any, alone as the last line.
fn report(e: &Error) -> ExitCode {
    if e.exit == EXIT_STILL_RUNNING {
        let _ = writeln!(std::io::stdout(), "{}", e.message);
        ExitCode::from(e.exit)
    } else {
        human::fail_with_fix(e.clone())
    }
}

// --------------------------------------------------------------------------
// check

/// `check`'s result as `--json` prints it: the same information as the text output. Its
/// text is lossless (`PyStr`): a path that is not UTF-8 is written as Python writes it.
#[derive(Debug, serde::Serialize)]
struct CheckReport {
    ok: bool,
    /// The deck's card count; `None` when the deck, the repository or the configuration did
    /// not load.
    cards: Option<usize>,
    problems: Vec<CheckProblem>,
    /// The one command that fixes every error, when there is one.
    fix: Option<PyStr>,
}

#[derive(Debug, serde::Serialize)]
struct CheckProblem {
    level: &'static str,
    card: Option<u32>,
    message: PyStr,
}

/// `check` (`deck._cmd_check`), for the repository the `--repo` directory belongs to (the
/// main working tree's top, deliberate change 1). The contract output is the text on stdout:
/// one problem per line, then `ok: <n> cards` when there is no error; a deck, repository or
/// configuration that does not load is its one `error: …` line. When one command fixes every
/// error, it follows on stderr, alone, as the last line.
fn check(deck_arg: &OsStr, repo_arg: &OsStr, json: bool) -> Result<ExitCode> {
    let report = check_report(check_problems(Path::new(deck_arg), Path::new(repo_arg)));
    human::print_or_json(&check_text(&report), &report, json)?;
    if let (false, Some(fix)) = (json, &report.fix) {
        let mut err = std::io::stderr().lock();
        writeln!(err, "{}", fix.shown())?;
        err.flush()?;
    }
    Ok(if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The report for the problems found, or for the failure that kept them from being found.
fn check_report(found: std::result::Result<(usize, Vec<deck::Problem>), Failure>) -> CheckReport {
    match found {
        Ok((cards, problems)) => {
            let errors: Vec<&deck::Problem> = problems
                .iter()
                .filter(|p| p.level == deck::Level::Error)
                .collect();
            let fix = errors.first().and_then(|first| {
                let fix = first.fix.as_ref()?;
                errors
                    .iter()
                    .all(|p| p.fix.as_ref() == Some(fix))
                    .then(|| PyStr::from_os(&human::command_line(fix)))
            });
            CheckReport {
                ok: errors.is_empty(),
                cards: Some(cards),
                problems: problems
                    .into_iter()
                    .map(|p| CheckProblem {
                        level: p.level.as_str(),
                        card: p.card,
                        message: p.message,
                    })
                    .collect(),
                fix,
            }
        }
        // The failure's own lossless text: never an `Error`'s message.
        Err(failure) => CheckReport {
            ok: false,
            cards: None,
            problems: vec![CheckProblem {
                level: deck::Level::Error.as_str(),
                card: None,
                message: failure.text,
            }],
            fix: None,
        },
    }
}

/// The text output: one problem per line, then `ok: <n> cards` when there is no error.
fn check_text(report: &CheckReport) -> String {
    let mut text = String::new();
    for p in &report.problems {
        text += &format!("{}: {}\n", p.level, p.message.shown());
    }
    if let (true, Some(cards)) = (report.ok, report.cards) {
        text += &format!("ok: {cards} cards\n");
    }
    text
}

/// The deck's card count and problems, or why they could not be found.
fn check_problems(
    deck_arg: &Path,
    repo_arg: &Path,
) -> std::result::Result<(usize, Vec<deck::Problem>), Failure> {
    let deck = deck::parse_deck(deck_arg)?;
    let given = if repo_arg.as_os_str().is_empty() {
        Path::new(".")
    } else {
        repo_arg
    };
    let given = std::path::absolute(given).map_err(|e| Failure::io(given, e))?;
    let top = crate::git::repo_identity(&given)?.top;
    let config = config::load_config(&top)?;
    let problems = deck::check_deck(&deck, &top, &config)?;
    Ok((deck.cards.len(), problems))
}

// --------------------------------------------------------------------------
// route

struct RouteArgs<'a> {
    deck: &'a OsStr,
    card: i64,
    role: &'a OsStr,
    attempt: i64,
    last_vendor: Option<Vendor>,
    repo: &'a OsStr,
    installed: Option<&'a OsStr>,
}

/// `route` (`route._cmd_route`), frozen at parity: the route line, then one reason per line;
/// `ladder exhausted: …` on stderr when the ladder is exhausted. The configuration is the
/// `--repo` directory's own, as given.
fn route(args: RouteArgs) -> Result<ExitCode> {
    let deck = deck::parse_deck(Path::new(args.deck))?;
    let config = config::load_config(Path::new(args.repo))?;
    let Some(card) = deck.cards.iter().find(|c| i64::from(c.number) == args.card) else {
        return Err(Error::msg(format!(
            "no card {} in {}",
            args.card,
            args.deck.to_string_lossy()
        )));
    };
    let installed = match args.installed {
        Some(text) => crate::route::installed_from(text)?,
        None => crate::route::installed_on_path(),
    };
    let role = args.role.to_string_lossy();
    let req = crate::route::RouteRequest {
        role: &role,
        card: Some(card),
        attempt: args.attempt,
        last_implementer_vendor: args.last_vendor.map(Vendor::name),
        installed: &installed,
        security: false,
        profile: deck.profile.as_deref(),
    };
    let res = match crate::route::resolve(&config, &req) {
        Ok(res) => res,
        Err(crate::route::RouteError::LadderExhausted(m)) => {
            let mut err = std::io::stderr().lock();
            writeln!(err, "ladder exhausted: {m}")?;
            err.flush()?;
            return Ok(ExitCode::FAILURE);
        }
        Err(e) => return Err(e.into()),
    };
    let r = &res.route;
    let mut text = format!("{} {} {}\n", r.vendor, r.model, r.effort);
    for reason in &res.reasons {
        text += reason;
        text.push('\n');
    }
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

impl Vendor {
    fn name(self) -> &'static str {
        match self {
            Vendor::Claude => "claude",
            Vendor::Codex => "codex",
        }
    }
}

fn version() -> Result<ExitCode> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "leadhand {}", env!("CARGO_PKG_VERSION"))?;
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const PYTHON_COMMANDS: [&str; 17] = [
        "version",
        "unpark",
        "check",
        "route",
        "dispatch",
        "stop-dispatch",
        "dispatch-runner",
        "start",
        "next",
        "record",
        "pause",
        "cancel",
        "timer",
        "run",
        "launch-line",
        "status",
        "report",
    ];
    const HUMAN_COMMANDS: [&str; 10] = [
        "run",
        "start",
        "status",
        "pause",
        "cancel",
        "unpark",
        "report",
        "check",
        "launch-line",
        "version",
    ];

    fn parse_ok(args: &[&str]) -> (Command, String) {
        let argv = std::iter::once("leadhand").chain(args.iter().copied());
        parse(argv).unwrap_or_else(|e| panic!("{args:?} did not parse: {e}"))
    }

    fn usage_error(args: &[&str]) -> clap::Error {
        let argv = std::iter::once("leadhand").chain(args.iter().copied());
        match parse(argv) {
            Ok((command, _)) => panic!("{args:?} parsed: {command:?}"),
            Err(e) => e,
        }
    }

    fn help(name: &str) -> String {
        let mut cli = Cli::command();
        cli.build();
        let sub = cli
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("no {name}"));
        sub.render_help().to_string()
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_seventeen_python_commands_in_order() {
        let cli = Cli::command();
        let names: Vec<&str> = cli.get_subcommands().map(|c| c.get_name()).collect();
        assert_eq!(names, PYTHON_COMMANDS);
    }

    #[test]
    fn python_defaults() {
        match parse_ok(&["check", "deck.md"]).0 {
            Command::Check { deck, repo, json } => {
                assert_eq!((deck, repo, json), (os("deck.md"), os("."), false))
            }
            other => panic!("{other:?}"),
        }
        match parse_ok(&["route", "deck.md", "2", "implementer"]).0 {
            Command::Route {
                deck,
                card,
                role,
                attempt,
                last_vendor,
                repo,
                installed,
            } => {
                assert_eq!(
                    (deck, card, role, attempt),
                    (os("deck.md"), 2, os("implementer"), 1)
                );
                assert_eq!((last_vendor, repo, installed), (None, os("."), None));
            }
            other => panic!("{other:?}"),
        }
        match parse_ok(&["record", "/r", "diff-check", "--card", "1", "--ok", "yes"]).0 {
            Command::Record(r) => {
                assert_eq!(
                    (r.kind, r.card, r.ok, r.note),
                    (RecordKind::DiffCheck, Some(1), Some(YesNo::Yes), os(""))
                );
                assert!(!r.stopped && r.reply.is_none() && r.reply_file.is_none());
            }
            other => panic!("{other:?}"),
        }
        match parse_ok(&["status"]).0 {
            Command::Status { run_dir, json } => assert_eq!((run_dir, json), (None, false)),
            other => panic!("{other:?}"),
        }
        match parse_ok(&["report", "/r"]).0 {
            Command::Report { run_dir, json } => {
                assert_eq!((run_dir, json), (Some(os("/r")), false))
            }
            other => panic!("{other:?}"),
        }
        match parse_ok(&["start", "deck.md"]).0 {
            Command::Start { repo, new, .. } => assert_eq!((repo, new), (os("."), false)),
            other => panic!("{other:?}"),
        }
        match parse_ok(&["run", "/r"]).0 {
            Command::Run {
                plugin_dir,
                print_mode,
                ..
            } => assert_eq!((plugin_dir, print_mode), (None, false)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn python_forms_parse() {
        let dispatch = [
            "dispatch",
            "--run",
            "/r",
            "--dispatch-id",
            "c1-a1-implement-1",
            "--card",
            "1",
            "--attempt",
            "1",
            "--role",
            "implementer",
            "--model",
            "gpt-5",
            "--effort",
            "high",
            "--brief",
            "/w/.leadhand/b.md",
            "--report",
            "/w/.leadhand/r.md",
            "--cwd",
            "/w",
            "--resume",
            "t1",
            "--kind",
            "implement",
            "--branch",
            "b--card-1",
        ];
        match parse_ok(&dispatch) {
            (Command::Dispatch(d), name) => {
                assert_eq!(name, "dispatch");
                assert_eq!(
                    (d.run, d.dispatch_id, d.card, d.attempt),
                    (os("/r"), os("c1-a1-implement-1"), Some(1), 1)
                );
                assert_eq!(
                    (d.resume, d.kind, d.branch),
                    (Some(os("t1")), Some(os("implement")), Some(os("b--card-1")))
                );
            }
            other => panic!("{other:?}"),
        }
        // A run-level dispatch has no card.
        let card = dispatch.iter().position(|a| *a == "--card").unwrap();
        let run_level = [&dispatch[..card], &dispatch[card + 2..]].concat();
        match parse_ok(&run_level) {
            (Command::Dispatch(d), _) => assert_eq!((d.card, d.attempt), (None, 1)),
            other => panic!("{other:?}"),
        }
        for (args, want) in [
            (
                &["stop-dispatch", "/r", "--dispatch", "d1"][..],
                "stop-dispatch",
            ),
            (&["dispatch-runner", "/r/workers/d1"], "dispatch-runner"),
            (&["next", "/r"], "next"),
            (&["timer", "/r", "--dispatch", "d1"], "timer"),
            (&["pause", "/r"], "pause"),
            (&["cancel", "/r"], "cancel"),
            (&["report", "/r"], "report"),
            (
                &["launch-line", "/r/deck.md", "--plugin-dir", "/p"],
                "launch-line",
            ),
            (&["run", "/r", "--print"], "run"),
            (&["unpark", "/r", "--card", "2", "--note", "-"], "unpark"),
            (
                &[
                    "record",
                    "/r",
                    "result",
                    "--dispatch",
                    "d1",
                    "--worker",
                    "a1",
                    "--reply-file",
                    "/f",
                    "--stopped",
                ],
                "record",
            ),
            (
                &["record", "/r", "ruling", "--card", "1", "--text", "-"],
                "record",
            ),
            (
                &[
                    "route",
                    "deck.md",
                    "1",
                    "reviewer",
                    "--last-vendor",
                    "codex",
                    "--installed",
                    "claude",
                ],
                "route",
            ),
            (&["start", "deck.md", "--new", "--repo=/repo"], "start"),
            (&["version"], "version"),
        ] {
            assert_eq!(parse_ok(args).1, want, "{args:?}");
        }
    }

    #[test]
    fn argparse_forms_parse_the_same() {
        // Negative numbers are values, as argparse reads them.
        match parse_ok(&["unpark", "/r", "--card", "-1", "--note", "n"]).0 {
            Command::Unpark { card, .. } => assert_eq!(card, -1),
            other => panic!("{other:?}"),
        }
        match parse_ok(&["route", "deck.md", "-2", "implementer"]).0 {
            Command::Route { card, .. } => assert_eq!(card, -2),
            other => panic!("{other:?}"),
        }
        // Long flags abbreviate to an unambiguous prefix; an exact name wins over a longer one.
        match parse_ok(&["check", "deck.md", "--rep", "/repo"]).0 {
            Command::Check { repo, .. } => assert_eq!(repo, os("/repo")),
            other => panic!("{other:?}"),
        }
        match parse_ok(&["record", "/r", "result", "--reply", "done"]).0 {
            Command::Record(r) => assert_eq!((r.reply, r.reply_file), (Some(os("done")), None)),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            usage_error(&["record", "/r", "result", "--repl", "x"]).exit_code(),
            2
        );
        // Empty values are values.
        match parse_ok(&["check", "", "--repo", ""]).0 {
            Command::Check { deck, repo, .. } => assert_eq!((deck, repo), (os(""), os(""))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_errors_exit_2() {
        for args in [
            &[][..],
            &["statu"],
            &["help"],
            &["--version"],
            &["no-such-command", "--help"],
            &["dispatch", "--run", "/r"],
            &[
                "record",
                "/r",
                "result",
                "--reply",
                "x",
                "--reply-file",
                "/f",
            ],
            &["record", "/r", "finished"],
            &["record", "/r", "diff-check", "--ok", "maybe"],
            &["route", "deck.md", "one", "implementer"],
            &[
                "route",
                "deck.md",
                "1",
                "implementer",
                "--last-vendor",
                "gemini",
            ],
            &["unpark", "/r", "--card", "1"],
            &["check", "deck.md", "--rpeo", "."],
            &["version", "extra"],
        ] {
            assert_eq!(usage_error(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn help_exits_0() {
        for args in [
            &["--help"][..],
            &["-h"],
            &["dispatch-runner", "--help"],
            &["record", "-h"],
        ] {
            assert_eq!(usage_error(args).exit_code(), 0, "{args:?}");
        }
    }

    #[test]
    fn hidden_surface_parses_but_stays_out_of_help() {
        let mut cli = Cli::command();
        let top = cli.render_help().to_string();
        assert!(!top.contains("dispatch-runner"), "{top}");
        assert!(top.contains("stop-dispatch"), "{top}");
        assert!(!help("route").contains("--installed"));
        assert!(!help("run").contains("--print"));
    }

    #[test]
    fn help_names_every_flag_and_record_kind() {
        let record = help("record");
        for word in [
            "--dispatch",
            "--worker",
            "--reply",
            "--reply-file",
            "--stopped",
            "--card",
            "--text",
            "--reason",
            "--ok",
            "--note",
            "started",
            "result",
            "ruling",
            "park",
            "diff-check",
        ] {
            assert!(record.contains(word), "{word} not in\n{record}");
        }
    }

    #[test]
    fn every_human_command_has_a_purpose_and_an_example() {
        for name in HUMAN_COMMANDS {
            let text = help(name);
            let lines: Vec<&str> = text.lines().collect();
            assert!(
                !lines[0].is_empty() && !lines[0].starts_with("Usage:"),
                "{text}"
            );
            let at = lines
                .iter()
                .position(|l| *l == "Example:")
                .unwrap_or_else(|| panic!("no example:\n{text}"));
            assert!(
                lines[at + 1].starts_with(&format!("  leadhand {name}")),
                "{text}"
            );
            // The example itself parses.
            let words = shell_words(lines[at + 1].trim());
            let args: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
            assert_eq!(parse_ok(&args).1, name, "{text}");
        }
    }

    /// Splits a line into words at spaces, keeping single-quoted text whole.
    fn shell_words(line: &str) -> Vec<String> {
        let (mut words, mut word, mut quoted) = (Vec::new(), String::new(), false);
        for c in line.chars() {
            match c {
                '\'' => quoted = !quoted,
                ' ' if !quoted => {
                    words.extend((!word.is_empty()).then(|| std::mem::take(&mut word)))
                }
                c => word.push(c),
            }
        }
        words.extend((!word.is_empty()).then_some(word));
        words
    }

    #[test]
    fn unported_commands_say_so_and_exit_1() {
        // `pause` is ported last (Task 17).
        let (command, name) = parse_ok(&["pause", "/r"]);
        let e = run(
            command,
            &name,
            &Ok(Caller {
                run_dir: None,
                run_id: None,
            }),
        )
        .unwrap_err();
        assert_eq!(e, Error::msg("not ported yet: pause"));
        assert_eq!(e.exit, 1);
    }

    #[test]
    fn version_is_the_python_version() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.0");
    }

    fn caller(vars: &[(&str, &str)]) -> Result<Caller> {
        let env: HashMap<String, OsString> =
            vars.iter().map(|(k, v)| (k.to_string(), os(v))).collect();
        resolve_caller_in(&move |name| env.get(name).cloned())
    }

    #[test]
    fn caller_from_the_run_anchor() {
        assert_eq!(
            caller(&[]),
            Ok(Caller {
                run_dir: None,
                run_id: None
            })
        );
        assert_eq!(
            caller(&[
                ("LEADHAND_RUN_DIR", "/runs/a run"),
                ("LEADHAND_RUN_ID", "2026-11-10-x-1a2b")
            ]),
            Ok(Caller {
                run_dir: Some(PathBuf::from("/runs/a run")),
                run_id: Some("2026-11-10-x-1a2b".into())
            })
        );
        // An empty run ID counts as unset, as in the Python.
        assert_eq!(
            caller(&[("LEADHAND_RUN_ID", "")]),
            Ok(Caller {
                run_dir: None,
                run_id: None
            })
        );
        // Kept byte for byte, unvalidated: bytes that aren't UTF-8 included.
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"run-\xff\xfe".to_vec());
        let got = resolve_caller_in(&|name| (name == "LEADHAND_RUN_ID").then(|| raw.clone()));
        assert_eq!(got.unwrap().run_id, Some(raw.clone()));
    }

    #[test]
    fn set_but_empty_run_dir_is_refused() {
        for value in ["", " ", "\t\n"] {
            let e = caller(&[("LEADHAND_RUN_DIR", value), ("LEADHAND_RUN_ID", "x")]).unwrap_err();
            assert_eq!(
                e,
                Error::msg("refused: $LEADHAND_RUN_DIR is set but empty"),
                "{value:?}"
            );
        }
    }

    #[test]
    fn check_json_keeps_path_bytes_as_python_writes_them() {
        use std::os::unix::ffi::OsStrExt;
        let top = OsStr::from_bytes(b"/runs/odd-\xff");
        let problem = deck::Problem {
            level: deck::Level::Error,
            card: None,
            message: PyStr::from_os_parts(&[OsStr::new("base commit abc1234 not found in "), top]),
            fix: Some(vec!["git".into(), "-C".into(), top.into()]),
        };
        let report = check_report(Ok((1, vec![problem])));
        assert_eq!(
            crate::wire::to_output(&report).unwrap(),
            "{\"ok\": false, \"cards\": 1, \"problems\": [{\"level\": \"error\", \"card\": null, \
             \"message\": \"base commit abc1234 not found in /runs/odd-\\udcff\"}], \
             \"fix\": \"git -C '/runs/odd-\\udcff'\"}"
        );
        // The text lines show the path as before: lossily.
        assert_eq!(
            check_text(&report),
            "error: base commit abc1234 not found in /runs/odd-\u{fffd}\n"
        );
        // A load failure's JSON is the failure's own lossless text, never an Error's.
        let failure = Failure::io(
            Path::new(OsStr::from_bytes(b"/decks/d\xff.md")),
            std::io::Error::from_raw_os_error(2),
        );
        let report = check_report(Err(failure));
        let json = crate::wire::to_output(&report).unwrap();
        assert!(
            json.contains(
                "\"message\": \"[Errno 2] No such file or directory: '/decks/d\\udcff.md'\""
            ),
            "{json}"
        );
        assert!(json.contains("\"cards\": null"), "{json}");
        assert_eq!(
            check_text(&report),
            "error: [Errno 2] No such file or directory: '/decks/d\u{fffd}.md'\n"
        );
        // And Python reads the JSON back to the same path bytes.
        let script = "import json,os,sys; r = json.loads(sys.stdin.read()); \
                      sys.stdout.buffer.write(os.fsencode(r['problems'][0]['message']))";
        let mut child = std::process::Command::new("python3")
            .args(["-B", "-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(json.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            out.stdout,
            b"[Errno 2] No such file or directory: '/decks/d\xff.md'"
        );
    }

    #[test]
    fn errors_print_as_error_lines_and_exit_with_their_code() {
        assert_eq!(report(&Error::msg("x")), ExitCode::from(1));
        assert_eq!(report(&Error::refused("x")), ExitCode::from(3));
        assert_eq!(
            report(&Error::still_running("still running: x")),
            ExitCode::from(75)
        );
    }
}
