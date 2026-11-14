//! The dispatch commands (`dispatch._cmd_dispatch`, `_cmd_stop`, `_cmd_runner`), and what
//! `dispatch` does: build the request from the run's configuration (`request_for`), write it
//! once, then start or attach to the dispatch's runner and wait for its result (`dispatch`).
//!
//! `dispatch` waits in the foreground up to `testing::foreground_wait()` (540 s: the proxy's
//! Bash call is capped at 10 minutes), then exits 75 with the `still running` line; the runner
//! carries on, and the same command again attaches to it.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::value::RawValue;

use super::cancel::{CANCEL_TIMEOUT, cancel, cancel_locked, new_token};
use super::commit::RunnerCommits;
use super::finalize::finalize;
use super::own::{
    HUNG_RECHECK, OwnerState, RUNNER_ERR, beat, owner_lock, owner_state, spawn_runner, supersede,
};
use super::runner::{Finish, POLL, StopReason};
use super::shell::run_runner_with;
use super::tracker::Tracker;
use super::{
    CANCEL, DispatchRequest, DispatchResult, ExitReason, REQUEST, RESULT, RUNNER, anchored_worker,
    check_caller, check_request, load_result, read_json_object, runner_worker, truthy, wall_now,
};
use crate::cli::{Caller, DispatchArgs};
use crate::config::load_run_config;
use crate::error::{Error, Result};
use crate::pytext::{expand_user, py_isspace, py_repr, py_splitlines, py_strip};
use crate::store::write_json_atomic;
use crate::wire::{PyMap, PyStr};

/// Roles that share another role's `[limits]` entry (`LIMITS_ROLE`).
const LIMITS_ROLE: [(&str, &str); 3] = [
    ("implementer_hard", "implementer"),
    ("reviewer_hard", "reviewer"),
    ("final_reviewer", "reviewer"),
];

// --------------------------------------------------------------------------
// The request

/// What a dispatch's request is made of besides the run's configuration.
pub struct Worker {
    pub card: Option<i64>,
    pub attempt: i64,
    pub role: String,
    pub model: PyStr,
    pub effort: PyStr,
    pub brief: PathBuf,
    pub report: PathBuf,
    pub cwd: PathBuf,
    pub resume: Option<PyStr>,
    pub kind: Option<String>,
    pub branch: Option<PyStr>,
}

/// `Path(p).absolute()`: `p` when absolute, else joined to the current directory.
fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| Error::msg(e.to_string()))?;
    Ok(cwd.join(p))
}

/// `request_for`: the request for a dispatch, with its limits, network and writable folders
/// from the run's configuration snapshot (a worker editing `.leadhand.toml` mid-run changes
/// nothing). `dispatch` and the engine build requests only here, so the `request.json` both
/// write is the same.
pub fn request_for(run_dir: &Path, dispatch_id: &str, w: Worker) -> Result<DispatchRequest> {
    let run_dir = absolute(run_dir)?;
    let cwd = absolute(&w.cwd)?;
    let meta = read_json_object(&crate::state::run_paths(&run_dir).run_json)?.unwrap_or_default();
    let repo = meta
        .get(&PyStr::from("repo"))
        .and_then(|raw| serde_json::from_str::<PyStr>(raw.get()).ok())
        .filter(|r| !r.is_empty());
    let repo = match repo {
        Some(repo) => PathBuf::from(repo.to_os()?),
        None => cwd.clone(),
    };
    let config = load_run_config(&run_dir, &repo)?;
    let key = LIMITS_ROLE
        .iter()
        .find(|(role, _)| *role == w.role)
        .map_or(w.role.as_str(), |(_, key)| key);
    let Some(limits) = config.limits_for(key) else {
        return Err(Error::msg(format!(
            "limits.{key}: not configured (role {})",
            w.role
        )));
    };
    let writable_dirs = config
        .codex
        .writable_dirs
        .iter()
        .map(|d| {
            let p = PathBuf::from(expand_user(OsStr::new(d)));
            if p.is_absolute() { p } else { cwd.join(p) }
        })
        .collect();
    Ok(DispatchRequest {
        model: w.model,
        effort: w.effort,
        role: w.role,
        brief: absolute(&w.brief)?,
        report: absolute(&w.report)?,
        cwd,
        run_dir,
        card: w.card,
        attempt: w.attempt,
        dispatch_id: dispatch_id.to_string(),
        steps: limits.steps,
        minutes: limits.minutes,
        network: config.codex.network,
        writable_dirs,
        resume: w.resume,
        kind: w.kind,
        branch: w.branch,
    })
}

/// A JSON value as Python compares the values `json.loads` gives (`True == 1 == 1.0`; objects
/// by their keys), strings kept lossless.
#[derive(Debug)]
enum Json {
    Null,
    Num(f64),
    Str(PyStr),
    List(Vec<Json>),
    Map(PyMap<Json>),
    /// Anything that does not read (never from a parsed file).
    Other(String),
}

impl Json {
    fn of(raw: &RawValue) -> Json {
        let text = raw.get().trim();
        let other = || Json::Other(text.to_string());
        match text.as_bytes().first() {
            Some(b'n') => Json::Null,
            Some(b't') => Json::Num(1.0),
            Some(b'f') => Json::Num(0.0),
            Some(b'"') => serde_json::from_str(text).map_or_else(|_| other(), Json::Str),
            Some(b'[') => serde_json::from_str::<Vec<Box<RawValue>>>(text).map_or_else(
                |_| other(),
                |items| Json::List(items.iter().map(|i| Json::of(i)).collect()),
            ),
            Some(b'{') => serde_json::from_str::<PyMap<Box<RawValue>>>(text).map_or_else(
                |_| other(),
                |map| {
                    Json::Map(PyMap(
                        map.0
                            .iter()
                            .map(|(k, v)| (k.clone(), Json::of(v)))
                            .collect(),
                    ))
                },
            ),
            _ => serde_json::from_str::<f64>(text).map_or_else(|_| other(), Json::Num),
        }
    }
}

impl PartialEq for Json {
    fn eq(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Null, Json::Null) => true,
            (Json::Num(a), Json::Num(b)) => a == b,
            (Json::Str(a), Json::Str(b)) => a == b,
            (Json::List(a), Json::List(b)) => a == b,
            (Json::Map(a), Json::Map(b)) => {
                a.len() == b.len() && a.0.iter().all(|(k, v)| b.get(k) == Some(v))
            }
            (Json::Other(a), Json::Other(b)) => a == b,
            _ => false,
        }
    }
}

/// `_write_request`: writes `request.json` unless it is there; an error naming the fields
/// that differ when the one there is another request.
fn write_request(wd: &Path, req: &DispatchRequest) -> Result<()> {
    let path = wd.join(REQUEST);
    let Some(existing) = read_json_object(&path)? else {
        return write_json_atomic(&path, req);
    };
    let ours: PyMap<Box<RawValue>> = serde_json::from_str(&crate::wire::to_line(req)?)
        .map_err(|e| Error::msg(format!("request: {e}")))?;
    let value = |map: &PyMap<Box<RawValue>>, k: &PyStr| map.get(k).map(|raw| Json::of(raw));
    let mut keys: Vec<&PyStr> = existing.0.iter().chain(&ours.0).map(|(k, _)| k).collect();
    keys.sort_by(|a, b| a.as_wtf8().cmp(b.as_wtf8()));
    keys.dedup();
    let changed: Vec<String> = keys
        .into_iter()
        .filter(|k| value(&existing, k) != value(&ours, k))
        .map(|k| k.lossy().into_owned())
        .collect();
    if changed.is_empty() {
        return Ok(());
    }
    Err(Error::msg(format!(
        "dispatch {} was requested with different {}; use a new dispatch id",
        req.dispatch_id,
        changed.join(", ")
    )))
}

// --------------------------------------------------------------------------
// Starting or attaching

/// The line `dispatch` prints, alone on stdout, when it exits 75.
fn still_running(dispatch_id: &str) -> Error {
    Error::still_running(format!(
        "still running: run the same command again (dispatch {dispatch_id})"
    ))
}

/// What `dispatch` makes of the owner's state, after its second look at a stale heartbeat.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Running,
    Hung,
    Orphaned,
    Dead,
    None,
}

/// `dispatch`: start or attach to the dispatch's runner and wait for its result. After `wait`
/// the error is exit 75's `still running` line; the runner carries on, and calling again
/// attaches to it.
pub fn dispatch(req: &DispatchRequest, wait: Duration) -> Result<DispatchResult> {
    let id = &req.dispatch_id;
    if id.is_empty() || id.contains('/') || id == "." || id == ".." {
        return Err(Error::msg(format!("bad dispatch id {}", py_repr(id))));
    }
    let wd = req.run_dir.join("workers").join(id);
    fs::create_dir_all(&wd).map_err(|e| Error::io(&wd, e))?;
    {
        let _owner = owner_lock(&wd, None)?;
        write_request(&wd, req)?;
    }
    let deadline = Instant::now().checked_add(wait);
    let mut mine: Option<Child> = None;
    // (heartbeat time, when first seen stale)
    let mut suspect: Option<(f64, f64)> = None;
    let exited = || {
        Error::msg(format!(
            "the runner exited without a result; see {}",
            wd.join(RUNNER_ERR).display()
        ))
    };
    loop {
        reap(&mut mine);
        if let Some(done) = load_result(&wd)? {
            return Ok(done);
        }
        {
            let _owner = owner_lock(&wd, None)?;
            if let Some(done) = load_result(&wd)? {
                return Ok(done);
            }
            let state = owner_state(&wd)?;
            let gone = matches!(
                state,
                OwnerState::Orphaned | OwnerState::Dead | OwnerState::None
            );
            if gone && wd.join(RESULT).exists() {
                // The runner finished and exited between the checks.
                continue;
            }
            let mut seen = match state {
                OwnerState::Stale => {
                    // Hung only if the heartbeat stays the same across a second look: after a
                    // laptop sleep the runner refreshes it as soon as it runs again.
                    let (beat, now) = (beat(&wd), wall_now());
                    let first = match suspect {
                        Some((b, at)) if b == beat => at,
                        _ => {
                            suspect = Some((beat, now));
                            now
                        }
                    };
                    if now - first >= HUNG_RECHECK.as_secs_f64() {
                        Seen::Hung
                    } else {
                        Seen::Running
                    }
                }
                other => {
                    suspect = None;
                    match other {
                        OwnerState::Running | OwnerState::Stale => Seen::Running,
                        OwnerState::Orphaned => Seen::Orphaned,
                        OwnerState::Dead => Seen::Dead,
                        OwnerState::None => Seen::None,
                    }
                }
            };
            if seen != Seen::Running && wd.join(CANCEL).exists() {
                // A stop was requested (stop-dispatch, run cancel): finish it, never replace.
                return cancel_locked(&wd, Instant::now() + CANCEL_TIMEOUT, None);
            }
            match seen {
                Seen::Hung | Seen::Orphaned => {
                    let token = new_token()?;
                    let result =
                        cancel_locked(&wd, Instant::now() + CANCEL_TIMEOUT, Some(token.clone()))?;
                    if result.exit_reason != ExitReason::Cancelled
                        || result.cancel_token.as_deref() != Some(token.as_str())
                    {
                        // It finished on its own meanwhile: keep that.
                        return Ok(result);
                    }
                    supersede(&wd)?;
                    // The replaced runner was not a failure.
                    reap(&mut mine);
                    (suspect, seen, mine) = (None, Seen::None, None);
                }
                Seen::Dead => {
                    if mine.is_some() {
                        return Err(exited());
                    }
                    finalize_interrupted(&wd, req)?;
                    supersede(&wd)?;
                    seen = Seen::None;
                }
                Seen::Running | Seen::None => {}
            }
            if seen == Seen::None {
                if mine.is_some() {
                    // Ours died before even writing runner.json.
                    return Err(exited());
                }
                mine = Some(spawn_runner(&wd)?);
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(still_running(id));
        }
        thread::sleep(POLL);
    }
}

/// A runner that died mid-attempt: what it used is recorded before it is replaced.
fn finalize_interrupted(wd: &Path, req: &DispatchRequest) -> Result<()> {
    let runner = read_json_object(&wd.join(RUNNER))?.unwrap_or_default();
    let field = |key: &str| runner.get(&PyStr::from(key)).filter(|raw| truthy(raw));
    let aborted = |raw: &RawValue| {
        serde_json::from_str::<PyMap<Box<RawValue>>>(raw.get())
            .ok()
            .and_then(|launch| launch.get(&PyStr::from("aborted")).map(|a| truthy(a)))
            .unwrap_or(false)
    };
    let launched = field("codex").is_some() || field("launching").is_some_and(|l| !aborted(l));
    if !launched {
        return Ok(());
    }
    let started_at = runner
        .get(&PyStr::from("started_at"))
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok());
    let tracker = Tracker::load(wd)?;
    let finish = Finish {
        stopped_for: Some(StopReason::Interrupted),
        codex_exit: None,
    };
    finalize(
        wd,
        req,
        finish,
        &tracker.stopped(),
        started_at.as_deref(),
        None,
        None,
        &RunnerCommits,
    )?;
    Ok(())
}

/// Reaps the runner this process spawned once it has exited, so it leaves no zombie.
fn reap(mine: &mut Option<Child>) {
    if let Some(child) = mine {
        let _ = child.try_wait();
    }
}

// --------------------------------------------------------------------------
// The commands

/// What `dispatch` and `stop-dispatch` print: the reply's first 15 lines (or its status, what
/// blocked it and why it stopped, when it has none), then `session: <thread ID or none>`.
pub fn reply_lines(result: &DispatchResult) -> Vec<String> {
    let reply = result.reply.lossy();
    let mut lines: Vec<String> = py_splitlines(py_strip(&reply))
        .into_iter()
        .map(|l| l.trim_end_matches(py_isspace).to_string())
        .collect();
    if lines.is_empty() {
        lines.push(format!("Status: {}", result.status.as_str()));
        if result.blocked_by.as_str() != "" {
            lines.push(format!("Blocked by: {}", result.blocked_by.as_str()));
        }
        lines.push(format!("Stopped: {}", result.exit_reason.as_str()));
    }
    lines.truncate(15);
    lines
}

fn print_result(result: &DispatchResult) -> Result<ExitCode> {
    let mut out = std::io::stdout().lock();
    let session = result.session_id.as_deref().filter(|s| !s.is_empty());
    let mut text = String::new();
    for line in reply_lines(result) {
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str(&format!("session: {}\n", session.unwrap_or("none")));
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| Error::msg(e.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

/// Nothing this process inherited reaches what it starts (the runner, Git), as the Python's
/// `Popen(close_fds=True)` keeps them out: a runner holding the caller's stdout or stderr
/// would keep a supervisor's Bash call waiting.
fn keep_inherited_fds() -> Result<()> {
    crate::sys::keep_inherited_fds_from_children()
        .map_err(|e| Error::msg(format!("cannot list this process's descriptors: {e}")))
}

/// A dispatch ID given on the command line, which must be text.
fn dispatch_id_of(raw: &OsStr) -> Result<&str> {
    raw.to_str().ok_or_else(|| {
        Error::msg(format!(
            "refused: bad dispatch id {}",
            PyStr::from_os(raw).repr()
        ))
    })
}

/// `_request_from_args`: the checked request, only for the anchored run, in its workspace (H2).
fn request_from_args(args: &DispatchArgs, caller: &Result<Caller>) -> Result<DispatchRequest> {
    let dispatch_id = dispatch_id_of(&args.dispatch_id)?;
    let run = Path::new(&args.run);
    let anchor = check_caller(caller, run, dispatch_id)?;
    let role = args.role.to_str().ok_or_else(|| {
        let role = args.role.to_string_lossy();
        Error::msg(format!("limits.{role}: not configured (role {role})"))
    })?;
    let kind = match &args.kind {
        None => None,
        Some(kind) => Some(
            kind.to_str()
                .ok_or_else(|| {
                    Error::msg(format!(
                        "--kind {} is not text",
                        PyStr::from_os(kind).repr()
                    ))
                })?
                .to_string(),
        ),
    };
    let worker = Worker {
        card: args.card,
        attempt: args.attempt,
        role: role.to_string(),
        model: PyStr::from_os(&args.model),
        effort: PyStr::from_os(&args.effort),
        brief: PathBuf::from(&args.brief),
        report: PathBuf::from(&args.report),
        cwd: PathBuf::from(&args.cwd),
        resume: args.resume.as_deref().map(PyStr::from_os),
        kind,
        branch: args.branch.as_deref().map(PyStr::from_os),
    };
    let req = request_for(run, dispatch_id, worker)?;
    check_request(&req, &anchor)?;
    Ok(req)
}

/// `leadhand dispatch …`: the reply and `session: …`; exit 75 with the `still running` line
/// after the foreground wait.
pub fn dispatch_command(args: &DispatchArgs, caller: &Result<Caller>) -> Result<ExitCode> {
    keep_inherited_fds()?;
    let req = request_from_args(args, caller)?;
    let result = dispatch(&req, crate::testing::foreground_wait())?;
    print_result(&result)
}

/// `leadhand stop-dispatch RUN --dispatch ID`: only a worker folder of the anchored run, whose
/// request stays in it (M1): its `runner.json` and `procs.json` are the only processes ever
/// signalled.
pub fn stop_command(
    run_dir: &OsStr,
    dispatch_id: &OsStr,
    caller: &Result<Caller>,
) -> Result<ExitCode> {
    keep_inherited_fds()?;
    let dispatch_id = dispatch_id_of(dispatch_id)?;
    let (wd, req) = anchored_worker(caller, Path::new(run_dir), dispatch_id)?;
    let result = match load_result(&wd)? {
        Some(result) => result,
        None => {
            if req.is_none() {
                return Err(Error::msg(format!("no request.json in {}", wd.display())));
            }
            cancel(&wd, CANCEL_TIMEOUT)?
        }
    };
    print_result(&result)
}

/// `leadhand dispatch-runner WORKER_DIR` (hidden): the detached runner, for a worker folder of
/// an anchored run with a checked request. A refusal is `leadhand: …` on stderr and exit 1.
pub fn runner_command(worker_dir: &OsStr, caller: &Result<Caller>) -> Result<ExitCode> {
    let wd = match runner_worker(caller, Path::new(worker_dir)) {
        Ok((wd, _)) => wd,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "leadhand: {}", e.message);
            return Ok(ExitCode::from(1));
        }
    };
    run_runner_with(&wd, &RunnerCommits).map(ExitCode::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{BlockedBy, Status};

    // CATALOG: test_test_hooks.py::ForegroundWait::test_default_is_540_seconds
    #[test]
    fn foreground_wait_default_is_540_seconds() {
        assert_eq!(crate::testing::FOREGROUND_WAIT, Duration::from_secs(540));
        // Without the hook set, `dispatch_command` waits the default.
        if std::env::var_os("LEADHAND_TEST_FOREGROUND_WAIT").is_none() {
            assert_eq!(crate::testing::foreground_wait(), Duration::from_secs(540));
        }
    }

    fn result(reply: &str) -> DispatchResult {
        serde_json::from_str(&format!(
            "{{\"status\": \"BLOCKED\", \"blocked_by\": \"environment\", \"exit_reason\": \
             \"error\", \"reply\": {}}}",
            serde_json::to_string(reply).unwrap()
        ))
        .unwrap()
    }

    #[test]
    fn reply_lines_are_the_reply_or_its_status() {
        let r = result("\n  Status: READY  \nline two\t\n\n");
        assert_eq!(reply_lines(&r), ["Status: READY", "line two"]);
        let r = result(" \n");
        assert_eq!(
            reply_lines(&r),
            [
                "Status: BLOCKED",
                "Blocked by: environment",
                "Stopped: error"
            ]
        );
        let mut r = result("");
        (r.status, r.blocked_by) = (Status::Spent, BlockedBy::Nothing);
        assert_eq!(reply_lines(&r), ["Status: SPENT", "Stopped: error"]);
        let long: String = (0..20).map(|i| format!("l{i}\n")).collect();
        assert_eq!(reply_lines(&result(&long)).len(), 15);
    }

    #[test]
    fn json_compares_as_python_compares_loaded_values() {
        let of = |text: &str| Json::of(&serde_json::from_str::<Box<RawValue>>(text).unwrap());
        assert_eq!(of("1"), of("1.0"));
        assert_eq!(of("true"), of("1"));
        assert_ne!(of("\"1\""), of("1"));
        assert_eq!(
            of("{\"a\": [1, null], \"b\": \"x\"}"),
            of("{\"b\": \"x\", \"a\": [1.0, null]}")
        );
        assert_ne!(of("{\"a\": 1}"), of("{\"a\": 1, \"b\": 2}"));
        assert_ne!(of("\"\\udcff\""), of("\"\\udcfe\""));
        assert_eq!(of("\"\\udcff\""), of("\"\\udcff\""));
    }
}
