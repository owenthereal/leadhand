//! The launcher (`launch.py`): `leadhand run` owns a run and starts its Claude Code
//! supervisor; `leadhand launch-line` prints the command that does.
//!
//! Ownership before mutation. `run` takes the run lock first (`lock`: the flock on
//! `run.lock`, and `holder.json`) and changes nothing in the run directory until it has it.
//! A run whose latest outcome is complete, incomplete (cancelled included) or failed is
//! refused: only a blocked or unfinished run continues. Only once it owns the run does it
//! append `resumed`, and only when the run has events beyond `run_started`.
//!
//! The supervisor, `claude` (`argv`: contract item 11), runs in the run's workspace with
//! `LEADHAND_RUN_ID`, `LEADHAND_RUN_DIR`, `LEADHAND_CLI` and `LEADHAND_LOCK_FD` set, and is
//! the only process that inherits the lock's descriptor (spec, bug class 6): the flag is
//! cleared in its pre-exec step alone, and, as Python's `Popen(close_fds=True)`, nothing else
//! the launcher inherited reaches it or the keep-awake process beside it (`awake`). SIGINT
//! and SIGTERM are forwarded to it while it runs; the lock is released once it has exited.
//!
//! In-place runs are refused (deliberate change 1): the checkout claim the Python takes for
//! one is gone, so a run whose workspace is the repository's checkout does not launch.
//!
//! `run` and `launch-line` are commands a person types: the run is found as
//! `runsel::select_run` finds it, and `launch-line` names the binary bare (`leadhand`) when
//! the `leadhand` on PATH is this same binary (spec, "Calls made in planning").

pub mod argv;
pub mod awake;
pub mod lock;

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};
use serde::Serialize;
use serde_json::value::RawValue;

use crate::cli::human;
use crate::dispatch::caller::py_str_of;
use crate::dispatch::{field, truthy};
use crate::error::{EXIT_REFUSED, Error, Failure, Result};
use crate::pytext::{abspath, pure_path, py_isspace, real_path, which};
use crate::runsel::{self, RunArg};
use crate::state::events::Resumed;
use crate::state::{Event, RunPaths, append_event, read_events, run_paths};
use crate::sys::{self, Forwarding};
use crate::wire::{self, PyMap, PyStr};
use crate::{config, git, testing};

pub use argv::{
    LaunchArgs, PRINT_ARGS, SUPERVISOR_AGENT, cli_settings, first_message, launch_argv,
};
pub use lock::RunLock;

/// The supervisor's program, found on PATH.
const CLAUDE_BIN: &str = "claude";
/// The signals forwarded to the supervisor.
const FORWARDED_SIGNALS: [Signal; 2] = [Signal::INT, Signal::TERM];
/// The Python's in-place checkout claim (`launch.CHECKOUT_FD_ENV`), which no Rust launch
/// passes: every Python command drops an inherited one, so `claude` never sees it.
const CHECKOUT_FD_ENV: &str = "LEADHAND_CHECKOUT_FD";

const ATTENDED_WARNING: [&str; 2] = [
    "leadhand: attended use only. Until the Claude-side run guard lands, keep this session in \
     view and run it only on fixture and trial repositories.\nleadhand: the supervisor runs \
     with --permission-mode auto. Its --settings overlay holds one sandbox.excludedCommands \
     entry, the leadhand CLI (",
    " *); whether it merges with or replaces your own excludedCommands is not established.\n",
];
const SPACES_WARNING: [&str; 2] = [
    "leadhand: warning: the leadhand CLI path contains whitespace (",
    "); the sandbox exclusion may not match; install leadhand under a path without spaces.\n",
];

// --------------------------------------------------------------------------
// The commands

/// `leadhand run [RUN] [--plugin-dir DIR]`: the run found as a command a person types
/// finds it, then `run`. The exit code is the supervisor's.
pub fn run_command(
    arg: Option<&OsStr>,
    plugin_dir: Option<&OsStr>,
    print_mode: bool,
) -> Result<ExitCode> {
    let target = run_target("run", arg)?;
    let plugin = plugin_dir.filter(|p| !p.is_empty()).map(Path::new);
    Ok(ExitCode::from(run(&target, plugin, print_mode)?))
}

/// `launch-line`'s `--json` object: the same line.
#[derive(Serialize)]
struct LaunchLineJson {
    command: PyStr,
}

/// `leadhand launch-line [RUN] [--plugin-dir DIR] [--json]`: the line alone, or as one JSON
/// object.
pub fn launch_line_command(
    arg: Option<&OsStr>,
    plugin_dir: Option<&OsStr>,
    json: bool,
) -> Result<ExitCode> {
    let target = run_target("launch-line", arg)?;
    let plugin = plugin_dir.filter(|p| !p.is_empty()).map(Path::new);
    let line = launch_line(&target, plugin)?;
    let mut out = std::io::stdout().lock();
    if json {
        let value = LaunchLineJson {
            command: PyStr::from_os(&line),
        };
        writeln!(out, "{}", wire::to_output(&value)?)?;
    } else {
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// What the run argument names: a path form as given (refused later, as the Python refuses
/// it, when it is not a run), else the run `runsel` finds by ID, name or slug, or from the
/// current directory.
fn run_target(command: &str, arg: Option<&OsStr>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().map_err(|e| Error::io(Path::new("."), e))?;
    match arg {
        Some(arg) if matches!(RunArg::parse(arg, &cwd), RunArg::Dir(_) | RunArg::Deck(_)) => {
            Ok(PathBuf::from(arg))
        }
        _ => runsel::select_run(command, arg, &cwd),
    }
}

/// A run directory, or a run's `deck.md` (whose folder has `run.json`), as an absolute path
/// (`resolve_run_dir`).
pub fn resolve_run_dir(path: &Path) -> Result<PathBuf> {
    let p = abspath(path).map_err(|e| Error::io(path, e))?;
    if p.is_dir() && p.join("run.json").is_file() {
        return Ok(p);
    }
    if p.file_name() == Some(OsStr::new("deck.md")) && p.is_file() {
        let parent = p.parent().unwrap_or(Path::new("/"));
        if parent.join("run.json").is_file() {
            return Ok(parent.to_path_buf());
        }
    }
    Err(Error::msg(format!(
        "not a run directory or a run's deck.md: {}",
        p.display()
    )))
}

// --------------------------------------------------------------------------
// launch-line

/// The exact command that launches (or resumes) the run, every word quoted (`launch_line`).
/// Without `plugin_dir`, the plugin directory the last launch recorded in `holder.json` is
/// kept, so a relaunch loads the plugin the same way.
pub fn launch_line(run_dir: &Path, plugin_dir: Option<&Path>) -> Result<OsString> {
    let root = resolve_run_dir(run_dir)?;
    let plugin = match plugin_dir {
        Some(dir) => Some(dir.to_path_buf()),
        None => recorded_plugin_dir(&run_paths(&root))?,
    };
    let mut argv = vec![typed_cli(), "run".into(), root.join("deck.md").into()];
    if let Some(dir) = plugin {
        argv.push("--plugin-dir".into());
        argv.push(abspath(&dir).map_err(|e| Error::io(&dir, e))?.into());
    }
    Ok(human::command_line(&argv))
}

/// The plugin directory `holder.json` records, when it names one.
fn recorded_plugin_dir(paths: &RunPaths) -> Result<Option<PathBuf>> {
    let holder = lock::read_holder(paths)?;
    match field::<PyStr>(&holder, "plugin_dir").filter(|dir| !dir.is_empty()) {
        Some(dir) => Ok(Some(PathBuf::from(dir.to_os()?))),
        None => Ok(None),
    }
}

/// The CLI as a person types it: bare `leadhand` when the `leadhand` on PATH is this same
/// binary, else its absolute path (`testing::self_path`).
fn typed_cli() -> OsString {
    let me = testing::self_path();
    match which("leadhand") {
        Some(found) if real_path(&found) == real_path(&me) => "leadhand".into(),
        _ => me.into_os_string(),
    }
}

// --------------------------------------------------------------------------
// run.json

/// `run.json`, as the launcher reads it: its entries, and its run ID, a non-empty string.
struct Meta {
    map: PyMap<Box<RawValue>>,
    run_id: PyStr,
}

impl Meta {
    /// `_read_meta`.
    fn read(paths: &RunPaths) -> Result<Meta> {
        let path = &paths.run_json;
        let cannot = |why: String| Error::msg(format!("cannot read {}: {why}", path.display()));
        let bytes = std::fs::read(path).map_err(|e| cannot(Failure::io(path, e).to_string()))?;
        let text = String::from_utf8(bytes).map_err(|e| cannot(e.to_string()))?;
        let value: &RawValue = serde_json::from_str(&text).map_err(|e| cannot(e.to_string()))?;
        let map: PyMap<Box<RawValue>> = serde_json::from_str(value.get()).unwrap_or_default();
        let no_run_id = || Error::msg(format!("{}: no run_id", path.display()));
        let run_id = field::<PyStr>(&map, "run_id")
            .filter(|id| !id.is_empty())
            .ok_or_else(no_run_id)?;
        Ok(Meta { map, run_id })
    }

    fn raw(&self, key: &str) -> Option<&RawValue> {
        self.map.get(&PyStr::from(key)).map(|raw| &**raw)
    }

    /// A non-empty string entry, as a path.
    fn path(&self, key: &str) -> Option<PathBuf> {
        let text = field::<PyStr>(&self.map, key).filter(|t| !t.is_empty())?;
        Some(PathBuf::from(text.to_os().ok()?))
    }

    /// `str(meta.get(key))` of a true entry, its bytes kept; `None` for a false one.
    fn word(&self, key: &str) -> Option<OsString> {
        let raw = self.raw(key).filter(|raw| truthy(raw))?;
        match serde_json::from_str::<PyStr>(raw.get()) {
            Ok(text) => Some(text.to_os().unwrap_or_else(|_| text.to_os_lossy())),
            Err(_) => Some(py_str_of(raw).into()),
        }
    }

    /// `_workspace`: an existing absolute directory.
    fn workspace(&self) -> Result<PathBuf> {
        match self.path("workspace") {
            Some(ws) if ws.is_absolute() && ws.is_dir() => Ok(pure_path(ws.as_os_str())),
            _ => {
                let shown = match self.raw("workspace") {
                    None => "None".to_string(),
                    Some(raw) => match serde_json::from_str::<PyStr>(raw.get()) {
                        Ok(text) => text.repr(),
                        Err(_) => py_str_of(raw),
                    },
                };
                Err(Error::msg(format!(
                    "run.json: workspace is not an existing absolute directory: {shown}"
                )))
            }
        }
    }

    /// `run.json`'s `repo`, a non-empty string.
    fn repo(&self) -> Result<PathBuf> {
        self.path("repo")
            .ok_or_else(|| Error::msg("run.json: no repo"))
    }

    /// `_in_place`: the workspace is the repository's checkout itself.
    fn in_place(&self) -> bool {
        match (self.path("workspace"), self.path("repo")) {
            (Some(ws), Some(repo)) => real_path(&ws) == real_path(&repo),
            _ => false,
        }
    }
}

/// The operator's command to start a new run of this run's deck (`_start_command`), every
/// word quoted.
fn start_command(meta: &Meta, cli: &Path) -> OsString {
    let mut argv = vec![
        cli.as_os_str().to_os_string(),
        "start".into(),
        meta.word("deck_source").unwrap_or_else(|| "<deck>".into()),
    ];
    if let Some(repo) = meta.word("repo") {
        argv.push("--repo".into());
        argv.push(repo);
    }
    human::command_line(&argv)
}

// --------------------------------------------------------------------------
// The checks before a launch

/// The outcome event that ended the run, unless a later `resumed` reopened it (`_ended`).
fn ended(events: &[(Event, wire::RawLine)]) -> Option<&crate::state::events::Outcome> {
    let mut last = None;
    for (e, _) in events {
        match e {
            Event::Outcome(o) => last = Some(o),
            Event::Resumed(_) => last = None,
            _ => {}
        }
    }
    last
}

/// Why a run in `workspace` must not launch, or `None` (`tools_inside`, security review L6):
/// the leadhand CLI (which runs outside the sandbox) or the plugin directory the supervisor
/// loads lies inside the workspace, where workers can edit it.
fn tools_inside(workspace: &Path, cli: &Path, plugin_dir: Option<&Path>) -> Option<String> {
    let ws = real_path(workspace);
    let ws = ws.as_os_str().as_bytes();
    let mut under = ws.strip_suffix(b"/").unwrap_or(ws).to_vec();
    under.push(b'/');
    let tools = [
        ("the leadhand CLI", Some(cli)),
        ("--plugin-dir", plugin_dir),
    ];
    for (what, path) in tools {
        let Some(path) = path else { continue };
        let real = real_path(path);
        let real = real.as_os_str().as_bytes();
        if real == ws || real.starts_with(&under) {
            return Some(format!(
                "{what} ({}) lies inside the run's workspace {}, where workers can edit it; \
                 install leadhand outside that repository, or run in a worktree",
                pure_path(path.as_os_str()).display(),
                workspace.display()
            ));
        }
    }
    None
}

/// The supervisor route's model and effort, from the run's configuration snapshot
/// (`_supervisor_route`).
fn supervisor_route(root: &Path, repo: &Path) -> Result<(Option<String>, Option<String>)> {
    let config = config::load_run_config(root, repo)?;
    let route = config
        .role("supervisor")
        .and_then(|role| role.route("claude"));
    Ok(route.map_or((None, None), |r| {
        (Some(r.model.clone()), Some(r.effort.clone()))
    }))
}

// --------------------------------------------------------------------------
// run

/// Owns the run and starts its supervisor (`launch.run`); returns the supervisor's exit code
/// (128 + the signal for one killed by a signal), 1 when the run cannot be launched, 3 when
/// someone else owns it. The ownership refusal and an ended run's refusal are printed here;
/// any other failure is the error returned.
pub fn run(run_dir: &Path, plugin_dir: Option<&Path>, print_mode: bool) -> Result<u8> {
    let root = resolve_run_dir(run_dir)?;
    let paths = run_paths(&root);
    let meta = Meta::read(&paths)?;
    let lock = match RunLock::acquire(&paths, &meta.run_id) {
        Ok(lock) => lock,
        Err(e) if e.exit == EXIT_REFUSED => {
            let mut err = std::io::stderr().lock();
            writeln!(err, "{}", e.message)?;
            err.flush()?;
            return Ok(EXIT_REFUSED);
        }
        Err(e) => return Err(e),
    };
    let mut supervisor_alive = false;
    let launched = launch_owned(
        &root,
        &paths,
        &meta,
        &lock,
        plugin_dir,
        print_mode,
        &mut supervisor_alive,
    );
    if supervisor_alive {
        // Never unlock under a live supervisor: it keeps its inherited copy.
        lock.detach();
    } else {
        lock.release();
    }
    launched
}

/// `run`, from the checks on: the caller holds the run lock, and releases it afterwards
/// unless `supervisor_alive` is left true (an error while the supervisor still runs).
fn launch_owned(
    root: &Path,
    paths: &RunPaths,
    meta: &Meta,
    lock: &RunLock,
    plugin_dir: Option<&Path>,
    print_mode: bool,
    supervisor_alive: &mut bool,
) -> Result<u8> {
    let cli = testing::self_path();
    if let Some(o) = ended(&read_events(paths)?)
        && o.outcome.as_deref() != Some("blocked")
    {
        // Only a blocked or unfinished run continues: a relaunch must never reopen a run that
        // completed, failed or was cancelled.
        return refuse(&ended_refusal(o, meta, &cli));
    }
    let workspace = meta.workspace()?;
    if let Some(problem) = tools_inside(&workspace, &cli, plugin_dir) {
        return Err(Error::msg(problem));
    }
    let repo = meta.repo()?;
    if let Some(text) = in_place_refusal(meta, &cli) {
        return refuse(&text);
    }
    // As at start: so a sparse checkout found later is a worker's (R63). A checkout that does
    // not read is left to `next`, which refuses the run anyway.
    if let Ok(Some(problem)) = git::sparse_problem(&repo) {
        return Err(Error::msg(problem.to_string_lossy()));
    }
    let (model, effort) = supervisor_route(root, &repo)?;
    if which(CLAUDE_BIN).is_none() {
        return Err(Error::msg(format!("{CLAUDE_BIN} not found on PATH")));
    }

    if read_events(paths)?
        .iter()
        .any(|(e, _)| e.kind() != "run_started")
    {
        let run_id = meta.run_id.encode_utf8()?.to_string();
        append_event(
            paths,
            Event::Resumed(Resumed {
                run_id: Some(run_id),
                ..Resumed::default()
            }),
        )?;
    }

    let plugin = match plugin_dir {
        Some(dir) => Some(abspath(dir).map_err(|e| Error::io(dir, e))?),
        None => None,
    };
    let message = first_message(root, &cli);
    let settings = cli_settings(&cli)?;
    let argv = launch_argv(&LaunchArgs {
        claude_bin: OsStr::new(CLAUDE_BIN),
        agent: SUPERVISOR_AGENT,
        first_message: &message,
        plugin_dir: plugin.as_deref(),
        settings: Some(&settings),
        model: model.as_deref(),
        effort: effort.as_deref(),
        print_mode,
    });
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&workspace)
        .env("LEADHAND_RUN_ID", meta.run_id.to_os()?)
        .env("LEADHAND_RUN_DIR", root)
        .env("LEADHAND_CLI", &cli)
        .env(sys::LOCK_FD_VAR, lock.fd().as_raw_fd().to_string())
        .env_remove(CHECKOUT_FD_ENV);
    sys::pass_fd_to_child(&mut cmd, lock.fd());

    warn(&cli)?;
    std::io::stdout().flush()?;
    supervise(lock, cmd, plugin.as_deref(), supervisor_alive)
}

/// A refusal whose one fixing command a person copies (spec, "Commands you type"): the
/// `error: <head>` line, then the command alone on the last line, its bytes kept.
fn fix_refusal(head: &[u8], fix: &OsStr) -> Vec<u8> {
    let mut text = b"error: ".to_vec();
    text.extend_from_slice(head);
    text.push(b'\n');
    text.extend_from_slice(fix.as_bytes());
    text.push(b'\n');
    text
}

/// Prints a refusal `fix_refusal` made, and gives exit 1.
fn refuse(text: &[u8]) -> Result<u8> {
    let mut err = std::io::stderr().lock();
    err.write_all(text)?;
    err.flush()?;
    Ok(1)
}

/// The refusal of a run that has ended (`run`'s terminal-outcome check), with the command
/// that starts a new run of its deck.
fn ended_refusal(o: &crate::state::events::Outcome, meta: &Meta, cli: &Path) -> Vec<u8> {
    let outcome = o.outcome.as_deref().unwrap_or("None");
    let mut head = format!("this run has ended: {outcome}");
    if let Some(q) = o.qualifier.as_deref().filter(|q| !q.is_empty()) {
        head += &format!(" ({q})");
    }
    head += "; only a blocked or unfinished run continues. Start a new run from the deck with:";
    fix_refusal(head.as_bytes(), &start_command(meta, cli))
}

/// The refusal of an in-place run (deliberate change 1: the Python's checkout claim is gone),
/// with the command that starts a new run of its deck in a worktree; `None` for a worktree
/// run.
fn in_place_refusal(meta: &Meta, cli: &Path) -> Option<Vec<u8>> {
    let repo = meta.path("repo").filter(|_| meta.in_place())?;
    let mut head = b"not supported in this version: run.worktree = false (this run works in the \
                     checkout "
        .to_vec();
    head.extend_from_slice(repo.as_os_str().as_bytes());
    head.extend_from_slice(
        b" itself); start a new run of the deck, which works in a worktree, with:",
    );
    Some(fix_refusal(&head, &start_command(meta, cli)))
}

/// The attended-use warning, and R28's when the CLI's path holds whitespace.
fn warn(cli: &Path) -> Result<()> {
    let cli = cli.as_os_str().as_bytes();
    let mut text = Vec::new();
    for (i, part) in ATTENDED_WARNING.iter().enumerate() {
        if i > 0 {
            text.extend_from_slice(cli);
        }
        text.extend_from_slice(part.as_bytes());
    }
    if String::from_utf8_lossy(cli).chars().any(py_isspace) {
        text.extend_from_slice(SPACES_WARNING[0].as_bytes());
        text.extend_from_slice(cli);
        text.extend_from_slice(SPACES_WARNING[1].as_bytes());
    }
    let mut err = std::io::stderr().lock();
    err.write_all(&text)?;
    err.flush()?;
    Ok(())
}

/// Starts the supervisor holding the lock's descriptor, forwards SIGINT and SIGTERM to it,
/// and waits for it (`_supervise`). Returns only once it has exited, with its exit code;
/// `supervisor_alive` is true while it runs, so an error then leaves the lock held.
fn supervise(
    lock: &RunLock,
    cmd: Command,
    plugin_dir: Option<&Path>,
    supervisor_alive: &mut bool,
) -> Result<u8> {
    supervise_with(lock, cmd, plugin_dir, supervisor_alive, &|_| {})
}

/// `supervise`, calling `after_exit` once the supervisor has exited and been reaped, while
/// the forwarding is still installed (where the Python's test runs its handler).
fn supervise_with(
    lock: &RunLock,
    mut cmd: Command,
    plugin_dir: Option<&Path>,
    supervisor_alive: &mut bool,
    after_exit: &dyn Fn(&Forwarding),
) -> Result<u8> {
    let fwd = sys::forward_signals(&FORWARDED_SIGNALS)
        .map_err(|e| Error::msg(format!("cannot forward signals: {e}")))?;
    if let Some(sig) = fwd.pending().first() {
        return Ok(signal_code(sig.as_raw()));
    }
    // `close_fds=True`: nothing this process inherited reaches the supervisor or keep-awake;
    // the lock's descriptor is passed to the supervisor alone.
    sys::keep_inherited_fds_from_children()
        .map_err(|e| Error::msg(format!("cannot list this process's descriptors: {e}")))?;
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let program = Path::new(cmd.get_program());
            return Err(Error::msg(format!(
                "cannot start {}: {}",
                program.display(),
                Failure::io(program, e)
            )));
        }
    };
    *supervisor_alive = true;
    let pid = Pid::from_child(&child);
    for sig in fwd.forward_to(pid.as_raw_pid()) {
        let _ = kill_process(pid, sig);
    }
    if let Err(e) = lock.record_supervisor(pid.as_raw_pid(), plugin_dir) {
        eprintln!(
            "warning: could not write {}: {e}",
            lock.holder_path().display()
        );
    }
    let keep_awake = awake::start(pid.as_raw_pid());
    let status = wait_forwarding(&fwd, &mut child, after_exit);
    if status.is_ok() {
        *supervisor_alive = false;
    }
    drop(fwd);
    awake::stop(keep_awake);
    let status = status.map_err(|e| Error::msg(format!("cannot wait for the supervisor: {e}")))?;
    Ok(exit_code(status))
}

/// Waits for `child` to exit, then tells the forwarding it has, before reaping it: the
/// exit is seen with `WNOWAIT`, so until the forwarding drops signals the pid is still the
/// child's (a zombie), never another process's. `after_exit` runs once it is reaped.
fn wait_forwarding(
    fwd: &Forwarding,
    child: &mut Child,
    after_exit: &dyn Fn(&Forwarding),
) -> std::io::Result<ExitStatus> {
    let pid = Pid::from_child(child);
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Ok(_) => break,
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    fwd.child_exited();
    let status = child.wait();
    after_exit(fwd);
    status
}

/// The launcher's exit code for the supervisor's status: its own code, or 128 + the signal
/// that killed it.
fn exit_code(status: ExitStatus) -> u8 {
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
        (None, Some(sig)) => signal_code(sig),
        (None, None) => 1,
    }
}

fn signal_code(sig: i32) -> u8 {
    u8::try_from(128 + sig).unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use super::*;

    const LAUNCH_CHILD: &str = "LEADHAND_TEST_LAUNCH_CHILD";

    /// `supervise` in this test binary run again as a child (a signal goes to the whole
    /// process, so not to the multi-threaded test run): a `true` supervisor, then, once it has
    /// exited and been reaped, a SIGTERM to the launcher. It prints `verdict: <exit code>
    /// <supervisor alive> <(handled, forwarded) by the handler for that SIGTERM>
    /// <holder.json names the supervisor>` and exits. PATH is empty, so no keep-awake runs.
    #[test]
    fn supervise_child() {
        if std::env::var_os(LAUNCH_CHILD).is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = run_paths(dir.path());
        let lock = RunLock::acquire(&paths, &PyStr::from("abc123")).unwrap();
        let mut cmd = Command::new("/usr/bin/true");
        cmd.stdin(Stdio::null());
        let deltas = std::cell::Cell::new(None);
        let after_exit = |fwd: &Forwarding| {
            let (handled, forwarded) = fwd.counts();
            kill_process(rustix::process::getpid(), Signal::TERM).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while fwd.counts().0 == handled && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let (h, f) = fwd.counts();
            deltas.set(Some((h - handled, f - forwarded)));
        };
        let mut alive = true;
        let code = supervise_with(&lock, cmd, None, &mut alive, &after_exit).unwrap();
        let holder = lock::read_holder(&paths).unwrap();
        let recorded = field::<i64>(&holder, "supervisor_pid").is_some();
        lock.release();
        println!("verdict: {code} {alive} {:?} {recorded}", deltas.get());
        std::process::exit(0);
    }

    #[test]
    fn signal_after_child_exit_is_not_forwarded() {
        // The catalogue row (FollowUpTests): a SIGTERM handled once the supervisor has exited
        // (here: once it has been reaped) is not forwarded ("kills: []"), and the launch
        // returns 0. A `supervise` that waited for the child any other way would never reach
        // the point where the signal is sent, and the verdict would say `None`.
        let empty = tempfile::tempdir().unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "launch::tests::supervise_child",
                "--exact",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(LAUNCH_CHILD, "1")
            .env("PATH", empty.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let verdict = text
            .lines()
            .find_map(|l| l.split_once("verdict: ").map(|(_, v)| v))
            .unwrap_or_else(|| {
                panic!("no verdict: {text}{}", String::from_utf8_lossy(&out.stderr))
            });
        assert_eq!(verdict, "0 false Some((1, 0)) true");
    }

    #[test]
    fn an_in_place_run_is_refused_with_the_start_command_alone_last() {
        let dir = tempfile::Builder::new()
            .prefix("in place ")
            .tempdir()
            .unwrap();
        let repo = dir.path().join("my repo");
        fs::create_dir_all(repo.join(".worktrees/run")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let meta = |workspace: &Path| {
            let json = format!(
                r#"{{"workspace": {}, "repo": {}, "deck_source": "/d/my deck.md"}}"#,
                serde_json::to_string(workspace.to_str().unwrap()).unwrap(),
                serde_json::to_string(repo.to_str().unwrap()).unwrap()
            );
            Meta {
                map: serde_json::from_str(&json).unwrap(),
                run_id: PyStr::from("abc123"),
            }
        };
        let cli = Path::new("/opt/lead hand/leadhand");
        // The checkout itself, by any spelling: in place.
        for ws in [repo.clone(), link.clone()] {
            let m = meta(&ws);
            assert!(m.in_place(), "{ws:?}");
            let text = String::from_utf8(in_place_refusal(&m, cli).unwrap()).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(
                lines,
                [
                    format!(
                        "error: not supported in this version: run.worktree = false (this run \
                         works in the checkout {} itself); start a new run of the deck, which \
                         works in a worktree, with:",
                        repo.display()
                    )
                    .as_str(),
                    &format!(
                        "'/opt/lead hand/leadhand' start '/d/my deck.md' --repo '{}'",
                        repo.display()
                    ),
                ]
            );
            assert!(text.ends_with('\n'));
        }
        // A worktree run: not in place, no refusal.
        let m = meta(&repo.join(".worktrees/run"));
        assert!(!m.in_place());
        assert_eq!(in_place_refusal(&m, cli), None);
        // Without a repo or a workspace: not in place.
        let none = Meta {
            map: serde_json::from_str("{}").unwrap(),
            run_id: PyStr::from("abc123"),
        };
        assert!(!none.in_place());
    }

    #[test]
    fn an_ended_run_is_refused_with_the_start_command_alone_last() {
        let meta = Meta {
            map: serde_json::from_str(r#"{"deck_source": "/d/my deck.md", "repo": "/r"}"#).unwrap(),
            run_id: PyStr::from("abc123"),
        };
        let o: crate::state::events::Outcome = serde_json::from_str(
            r#"{"kind": "outcome", "outcome": "incomplete", "qualifier": "cancelled"}"#,
        )
        .unwrap();
        let text = ended_refusal(&o, &meta, Path::new("/bin/leadhand"));
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "error: this run has ended: incomplete (cancelled); only a blocked or unfinished run \
             continues. Start a new run from the deck with:\n\
             /bin/leadhand start '/d/my deck.md' --repo /r\n"
        );
    }

    #[test]
    fn exit_codes_are_the_childs_or_128_plus_the_signal() {
        let status = |code: &str| {
            Command::new("sh")
                .args(["-c", code])
                .stdin(Stdio::null())
                .status()
                .unwrap()
        };
        assert_eq!(exit_code(status("exit 7")), 7);
        assert_eq!(exit_code(status("kill -TERM $$")), 128 + 15);
        assert_eq!(exit_code(status("kill -INT $$")), 128 + 2);
    }

    #[test]
    fn a_run_is_its_directory_or_its_deck() {
        let dir = tempfile::Builder::new()
            .prefix("run dir ")
            .tempdir()
            .unwrap();
        let root = dir.path().join("r");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("deck.md"), "# Deck\n").unwrap();
        let not_a_run = |p: &Path| {
            resolve_run_dir(p).unwrap_err().message
                == format!("not a run directory or a run's deck.md: {}", p.display())
        };
        assert!(not_a_run(&root));
        assert!(not_a_run(&root.join("deck.md")));
        fs::write(root.join("run.json"), "{}").unwrap();
        assert_eq!(resolve_run_dir(&root).unwrap(), root);
        assert_eq!(resolve_run_dir(&root.join("deck.md")).unwrap(), root);
        assert_eq!(resolve_run_dir(&root.join("x/../deck.md")).unwrap(), root);
        assert!(not_a_run(&root.join("run.json")));
    }

    #[test]
    fn tools_inside_the_workspace_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        fs::create_dir_all(ws.join("bin")).unwrap();
        let outside = dir.path().join("ws2/leadhand");
        assert_eq!(tools_inside(&ws, &outside, None), None);
        let inside = ws.join("bin/leadhand");
        let problem = tools_inside(&ws, &inside, None).unwrap();
        assert!(problem.starts_with("the leadhand CLI ("), "{problem}");
        assert!(
            problem.ends_with("install leadhand outside that repository, or run in a worktree")
        );
        let problem = tools_inside(&ws, &outside, Some(&ws)).unwrap();
        assert!(problem.starts_with("--plugin-dir ("), "{problem}");
    }

    #[test]
    fn the_start_command_quotes_every_word() {
        let meta = |json: &str| Meta {
            map: serde_json::from_str(json).unwrap(),
            run_id: PyStr::from("abc123"),
        };
        let cli = Path::new("/opt/lead hand/leadhand");
        assert_eq!(
            start_command(
                &meta(r#"{"deck_source": "/d/my deck.md", "repo": "/r"}"#),
                cli
            ),
            "'/opt/lead hand/leadhand' start '/d/my deck.md' --repo /r"
        );
        assert_eq!(
            start_command(&meta(r#"{"deck_source": "", "repo": null}"#), cli),
            "'/opt/lead hand/leadhand' start '<deck>'"
        );
    }
}
