//! The dispatch runner's shell (`dispatch.run_runner` and what it calls): the runner's life
//! in its worker folder, the threads that feed the pure core (`runner.rs`), the process
//! tracker that carries out its snapshots and signals, and finalizing.
//!
//! Codex's stdout is `stream.jsonl` itself, as the Python's `Popen(stdout=file)` makes it, so
//! everything Codex writes is on disk even while the runner is stopped, and after it dies;
//! the stream thread follows the file. Three threads feed the core one channel: the stream's
//! lines, Codex's exit (SIGCHLD) and the runner's signals (SIGTERM, SIGHUP); a receive with a
//! timeout supplies the ticks. The shell owns Codex's `Child` and polls it before every event
//! (`try_wait`, the Python's `proc.poll()`), so an exit that came first is seen first, ahead
//! of a cancel or a limit seen in the same look.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::FlockOperation;
use rustix::process::{getpgrp, getpid};
use serde::de::IgnoredAny;
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGTERM};
use signal_hook::iterator::Signals;

use super::caller::py_name;
use super::request::StoppedProc;
use super::runner::{
    Action, CANCEL_FINISH_MARGIN, Event, Finish, HEARTBEAT_EVERY, Ladder, Liveness, POLL,
    RUNNER_LOCK_RETRY, Runner, Sig, StopReason, StreamSummary, StreamView, classify,
    possibly_incomplete, return_code,
};
use super::{
    BlockedBy, CANCEL, COMMIT_KINDS, DispatchRequest, DispatchResult, Env, ExitReason,
    FINALIZE_LOCK, GH_DIR, HEARTBEAT, LAST, PROCS, RAW, RESULT, RUNNER, RUNNER_LOCK, STDERR,
    STOPPING, STREAM, SUPERSEDED, Status, TAG_VAR, UNTRACKED, codex_argv_parts, codex_overrides,
    evidence_dirs, load_result, read_json_object, read_request, worker_env,
};
use crate::briefs::worker_prompt;
use crate::error::{Error, Failure, Result};
use crate::git::{Git, git_common_dir, split_nul};
use crate::procs::{self, ProcId, ProcInfo};
use crate::pytext::py_strip;
use crate::state::{AttemptRecord, CardRef, append_attempt, read_attempts, run_paths, utc_now};
use crate::store::{state_lock, write_json_atomic};
use crate::sys::detach_session;
use crate::testing::crash_point;
use crate::usage::{
    CODEX_STREAM_SOURCE, ChargeSource, Usage, charge, default_sessions_dir, estimate_cost,
};
use crate::wire::{PyMap, PyStr};

// --------------------------------------------------------------------------
// The records the runner writes (contract item 5)

/// `runner.json`, in the Python's key order: the runner's identity, then the launch intent,
/// then Codex's.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunnerRecord {
    pub runner: StoppedProc,
    pub codex: Option<StoppedProc>,
    pub generation: u32,
    pub tag: String,
    pub started_wall: f64,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launching: Option<Launching>,
}

/// The launch intent: when, and whether the launch was abandoned (a cancel came first, or
/// Codex could not be started).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Launching {
    pub at: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub aborted: bool,
}

/// `procs.json`: the recorded processes still alive, by pid, and the last tag sweep's
/// unreadable environments when there were any.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct ProcsFile {
    descendants: Vec<StoppedProc>,
    #[serde(skip_serializing_if = "Unreadable::is_empty")]
    tag_unreadable: Unreadable,
}

/// The pids whose environment the last tag sweep could not read, or `unsupported` where the
/// process table could not be read at all.
#[derive(Clone, Debug, PartialEq)]
enum Unreadable {
    Pids(Vec<i32>),
    Unsupported,
}

impl Unreadable {
    fn is_empty(&self) -> bool {
        matches!(self, Unreadable::Pids(pids) if pids.is_empty())
    }
}

impl Serialize for Unreadable {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Unreadable::Pids(pids) => pids.serialize(s),
            Unreadable::Unsupported => s.serialize_str("unsupported"),
        }
    }
}

/// `stopping.json`: the runner's acknowledgement.
#[derive(Serialize)]
struct Stopping<'a> {
    reason: &'a str,
    at: String,
}

/// `untracked.json`: the workspace's untracked files when the first runner launched Codex,
/// or why they could not be listed (I3).
#[derive(Serialize)]
struct UntrackedRecord {
    at: String,
    paths: Option<Vec<PyStr>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<PyStr>,
}

// --------------------------------------------------------------------------
// Committing the worker's work

/// What committing the worker's work came to (`_commit_work`'s outcome).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitOutcome {
    /// The commit made for the worker.
    pub committed: Option<String>,
    /// Whether the worker left changes.
    pub changes: bool,
    pub commit_skipped: Option<PyStr>,
    pub commit_error: Option<PyStr>,
    pub gitlinks_skipped: Option<Vec<PyStr>>,
}

/// The runner's commit of a commit kind's work (R39), supplied by the caller: called only for
/// a launched dispatch whose kind is one of `COMMIT_KINDS` with a branch. `deadline` bounds
/// every Git call, for a cancel.
pub trait Commits {
    fn commit_work(
        &self,
        wd: &Path,
        req: &DispatchRequest,
        deadline: Option<Instant>,
    ) -> CommitOutcome;
}

/// No commit: a commit kind's work is reported as uncommitted changes, so its result can never
/// pass for finished work.
pub struct NoCommit;

impl Commits for NoCommit {
    fn commit_work(&self, _: &Path, _: &DispatchRequest, _: Option<Instant>) -> CommitOutcome {
        CommitOutcome {
            changes: true,
            commit_error: Some("the runner's commits are not available in this build".into()),
            ..CommitOutcome::default()
        }
    }
}

/// `commit_failed`: why a dispatch's changes were left uncommitted, or `None`. A result whose
/// worker left changes the runner could not commit must not pass for finished work.
pub fn commit_failed(commit: &CommitOutcome) -> Option<PyStr> {
    if commit.committed.as_deref().is_some_and(|c| !c.is_empty()) || !commit.changes {
        return None;
    }
    let given = |text: &Option<PyStr>| text.clone().filter(|t| !t.is_empty());
    given(&commit.commit_error).or_else(|| given(&commit.commit_skipped))
}

/// `_commit_work`'s first rule, then the caller's commit.
fn commit_work(
    wd: &Path,
    req: &DispatchRequest,
    deadline: Option<Instant>,
    commits: &dyn Commits,
) -> CommitOutcome {
    let kind = req.kind.as_deref().filter(|k| !k.is_empty());
    let commits_kind = kind.is_some_and(|k| COMMIT_KINDS.contains(&k));
    if !commits_kind || req.branch.as_ref().is_none_or(PyStr::is_empty) {
        return CommitOutcome {
            commit_skipped: Some(
                format!("{} dispatch does not commit", kind.unwrap_or("this")).into(),
            ),
            ..CommitOutcome::default()
        };
    }
    commits.commit_work(wd, req, deadline)
}

// --------------------------------------------------------------------------
// The runner

/// The detached runner's body (`run_runner`): launch Codex, watch it, stop it at a limit or a
/// cancel, then finalize. 0 once `result.json` is written; 3 when another runner owns the
/// dispatch (it then touches nothing).
pub fn run_runner(worker_dir: &Path) -> Result<u8> {
    run_runner_with(worker_dir, &NoCommit)
}

/// `run_runner`, committing a commit kind's work with `commits`.
pub fn run_runner_with(worker_dir: &Path, commits: &dyn Commits) -> Result<u8> {
    let wd = worker_dir;
    // First of all, as the Python: SIGTERM is a cancel request, and SIGHUP is caught and does
    // nothing (it is not `SIG_IGN`, so Codex does not inherit it ignored).
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&term)).map_err(signal_error)?;
    let signals = Signals::new([SIGTERM, SIGHUP]).map_err(signal_error)?;
    let exits = Signals::new([SIGCHLD]).map_err(signal_error)?;
    let Some(lock) = take_runner_lock(wd)? else {
        eprintln!("leadhand: another runner owns {}", wd.display());
        return Ok(3);
    };
    // Held until this process exits, as the Python keeps its descriptor.
    std::mem::forget(lock);
    if load_result(wd)?.is_some() {
        return Ok(0);
    }
    let req = read_request(wd)?
        .ok_or_else(|| Error::msg(format!("no request.json in {}", wd.display())))?;
    let started_at = utc_now();
    let generation = generation_of(wd)?;
    let run_id = run_id_of(&req.run_dir)?;
    let tag = format!("{run_id}:{}:{generation}", req.dispatch_id);
    let mut record = RunnerRecord {
        runner: own_record(),
        codex: None,
        generation,
        tag: tag.clone(),
        started_wall: wall_now(),
        started_at: started_at.clone(),
        launching: None,
    };
    write_json_atomic(&wd.join(RUNNER), &record)?;
    touch(&wd.join(HEARTBEAT))?;

    let cancelled = || term.load(Ordering::SeqCst) || wd.join(CANCEL).exists();
    let without_codex = |stopped_for| {
        let finish = Finish {
            stopped_for,
            codex_exit: None,
        };
        finalize(
            wd,
            &req,
            finish,
            &[],
            Some(&started_at),
            None,
            None,
            commits,
        )
        .map(|_| 0)
    };
    if cancelled() {
        return without_codex(Some(StopReason::Cancelled));
    }
    let prepared = match prepare(wd, &req, &run_id, &tag) {
        Ok(prepared) => prepared,
        Err(e) => {
            let text = format!("leadhand: cannot prepare the Codex worker: {e}\n");
            write_text(&wd.join(STDERR), &PyStr::from(text))?;
            return without_codex(None);
        }
    };
    record_untracked(wd, &req)?;
    let mut child = match launch(wd, &mut record, &prepared, &req.cwd, &cancelled)? {
        Launch::Started(child) => child,
        Launch::Cancelled => return without_codex(Some(StopReason::Cancelled)),
        Launch::CannotStart(program, e) => {
            let text = format!("leadhand: cannot start {}: {e}\n", program.display());
            write_text(&wd.join(STDERR), &PyStr::from(text))?;
            return without_codex(None);
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prepared.prompt;
        // `_feed`: the prompt, then end of file; a worker that exits before reading it all
        // breaks the pipe, which ends the write.
        thread::spawn(move || {
            let _ = stdin.write_all(&prompt);
        });
    }
    let mut tracker = Tracker::new(wd, record.codex.clone(), Some(child), Some(tag));
    let (finish, fault) = watch(wd, &req, &mut tracker, signals, exits)?;
    // `proc.wait()`: Codex is reaped once everything is gone.
    let codex_exit = finish
        .codex_exit
        .or_else(|| tracker.wait().map(return_code));
    let deadline = match finish.stopped_for {
        Some(StopReason::Cancelled) => cancel_deadline(wd)?,
        _ => None,
    };
    let finish = Finish {
        codex_exit,
        ..finish
    };
    finalize(
        wd,
        &req,
        finish,
        &tracker.stopped(),
        Some(&started_at),
        deadline,
        Some(&wd.join(HEARTBEAT)),
        commits,
    )?;
    // The Python's runner re-raises what stopped it, once the result is written.
    fault.map_or(Ok(0), Err)
}

fn signal_error(e: std::io::Error) -> Error {
    Error::msg(format!("cannot handle signals: {e}"))
}

/// `_take_runner_lock`: the runner's lifetime lock. A probe by another process holds it for
/// an instant, so a starting runner retries briefly before concluding another runner owns it.
fn take_runner_lock(wd: &Path) -> Result<Option<File>> {
    let path = wd.join(RUNNER_LOCK);
    let file = open_lock_file(&path)?;
    let until = Instant::now() + RUNNER_LOCK_RETRY;
    loop {
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Some(file)),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                if Instant::now() >= until {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(Error::io(&path, e.into())),
        }
    }
}

/// A lock file, opened as the Python opens it (`O_RDWR | O_CREAT | O_CLOEXEC`, `0644`).
fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(path)
        .map_err(|e| Error::io(path, e))
}

/// This process's record: pid, group and start time (the start `None` when the process table
/// cannot be read).
fn own_record() -> StoppedProc {
    let pid = getpid().as_raw_pid();
    match crate::sys::proc_info(pid) {
        Some(me) => StoppedProc {
            pid: me.pid,
            pgid: me.pgid,
            start: Some(me.start),
        },
        None => StoppedProc {
            pid,
            pgid: getpgrp().as_raw_pid(),
            start: None,
        },
    }
}

/// `time.time()`.
fn wall_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// This runner's generation: one more than the generations already superseded.
fn generation_of(wd: &Path) -> Result<u32> {
    let dir = wd.join(SUPERSEDED);
    if !dir.is_dir() {
        return Ok(1);
    }
    let entries = fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?.count();
    Ok(u32::try_from(entries).map_or(u32::MAX, |n| n.saturating_add(1)))
}

/// `_run_id`: `run.json`'s `run_id`, else the run directory's name. A run ID that is not a
/// string, or not text, is an error (an anchored run's is always a non-empty string).
fn run_id_of(run_dir: &Path) -> Result<String> {
    let meta = read_json_object(&run_paths(run_dir).run_json)?.unwrap_or_default();
    match meta.get(&PyStr::from("run_id")).filter(|raw| truthy(raw)) {
        Some(raw) => {
            let text = serde_json::from_str::<PyStr>(raw.get()).map_err(|_| {
                Error::msg(format!("run.json's run_id {} is not a string", raw.get()))
            })?;
            String::from_utf8(text.as_wtf8().to_vec())
                .map_err(|_| Error::msg(format!("run.json's run_id {} is not text", text.repr())))
        }
        None => py_name(run_dir).into_string().map_err(|name| {
            Error::msg(format!(
                "the run directory's name {} is not text",
                PyStr::from_os(&name).repr()
            ))
        }),
    }
}

/// Whether a JSON value is true as Python's `bool` reads the value `json.loads` gives.
fn truthy(raw: &RawValue) -> bool {
    let text = raw.get().trim();
    match text.as_bytes().first() {
        Some(b'n' | b'f') | None => false,
        Some(b't') => true,
        Some(b'"') => text != "\"\"",
        Some(b'[') => serde_json::from_str::<Vec<IgnoredAny>>(text).is_ok_and(|v| !v.is_empty()),
        Some(b'{') => serde_json::from_str::<PyMap<IgnoredAny>>(text).is_ok_and(|m| !m.is_empty()),
        Some(_) => serde_json::from_str::<f64>(text).is_ok_and(|n| n != 0.0),
    }
}

/// `_touch`: creates the file if needed, and sets its times to now.
fn touch(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    let now = SystemTime::now();
    file.set_times(FileTimes::new().set_accessed(now).set_modified(now))
        .map_err(|e| Error::io(path, e))
}

/// `Path.write_text(text, encoding="utf-8")`: the file is replaced first, then written; text
/// holding a lone surrogate fails there, leaving it empty, as Python's strict encoder does.
fn write_text(path: &Path, text: &PyStr) -> Result<()> {
    let mut file = File::create(path).map_err(|e| Error::io(path, e))?;
    file.write_all(text.encode_utf8()?.as_bytes())
        .map_err(|e| Error::io(path, e))
}

/// `Path.read_text(encoding="utf-8", errors="replace")`: undecodable bytes replaced, line ends
/// translated to `\n`; `None` when the file is missing.
fn read_text(path: &Path) -> Result<Option<PyStr>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(path, e)),
    };
    let text = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    Ok(Some(PyStr::from(text)))
}

/// What launching Codex needs, prepared before the launch.
struct Prepared {
    /// The command line, or why the OS cannot be given it (found at the launch, as `Popen`
    /// finds it).
    argv: Result<Vec<OsString>>,
    env: Env,
    /// The prompt, UTF-8.
    prompt: Vec<u8>,
}

/// The Python's `try` block before the launch: the worker's `GH_CONFIG_DIR`, environment,
/// command line and prompt. An error is the worker's "cannot prepare".
fn prepare(wd: &Path, req: &DispatchRequest, run_id: &str, tag: &str) -> Result<Prepared> {
    let gh = wd.join(GH_DIR);
    if let Err(e) = fs::create_dir(&gh)
        && !(e.kind() == ErrorKind::AlreadyExists && gh.is_dir())
    {
        return Err(Error::io(&gh, e));
    }
    let parent: Env = std::env::vars_os().collect();
    let env = worker_env(&parent, run_id, &gh, tag);
    let overrides = codex_overrides(&parent, run_id, &gh, tag);
    let git_common = git_common_dir(&req.cwd)?;
    let extra = evidence_dirs(req)?;
    let argv = codex_argv_parts(req, &wd.join(LAST), &git_common, &overrides, &extra)?;
    // S2-R33: sent as `worker_prompt(...).encode("utf-8")`, strictly.
    let prompt = worker_prompt(&req.role, &req.brief, &req.report)?;
    let prompt = PyStr::from_os(&prompt).encode_utf8()?.as_bytes().to_vec();
    Ok(Prepared { argv, env, prompt })
}

/// `_record_untracked`: before the first launch, the untracked files already in the workspace
/// (I3), kept across generations. If they cannot be listed, the commit later takes only
/// tracked changes.
fn record_untracked(wd: &Path, req: &DispatchRequest) -> Result<()> {
    let path = wd.join(UNTRACKED);
    if path.exists() {
        return Ok(());
    }
    let (paths, error) = match untracked_files(&req.cwd) {
        Ok(paths) => (Some(paths), None),
        Err(failure) => (None, Some(failure.text)),
    };
    let record = UntrackedRecord {
        at: utc_now(),
        paths,
        error,
    };
    write_json_atomic(&path, &record)
}

/// `gitops.untracked_files`: the untracked paths Git does not ignore, named from the
/// worktree's top; a nested repository is its folder with a trailing slash.
fn untracked_files(ws: &Path) -> std::result::Result<Vec<PyStr>, Failure> {
    let git = Git::try_at(ws)?;
    let args = [
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
        "--full-name",
        "--",
        ":/",
    ]
    .map(OsStr::new);
    let out = git.try_output(&args)?;
    if !out.status.success() {
        return Err(git.failure_text(&args, &out));
    }
    Ok(split_nul(&out.stdout)
        .iter()
        .map(|p| PyStr::from_os(OsStr::from_bytes(p)))
        .collect())
}

/// How a launch went.
enum Launch {
    Started(Child),
    /// A cancel arrived first; nothing was started.
    Cancelled,
    /// The program could not be started (or its output files opened): what, and why.
    CannotStart(PathBuf, Error),
}

/// `_launch`, two-phase: record the intent, re-check for a cancel, start Codex in a new
/// session (its own process group), record it. An argument the OS cannot be given is an error
/// after the intent, which is not marked aborted, as the Python's `Popen` raises there.
fn launch(
    wd: &Path,
    record: &mut RunnerRecord,
    prepared: &Prepared,
    cwd: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<Launch> {
    let runner_json = wd.join(RUNNER);
    record.launching = Some(Launching {
        at: utc_now(),
        aborted: false,
    });
    write_json_atomic(&runner_json, record)?;
    crash_point("before_worker_launch");
    let abort = |record: &mut RunnerRecord| {
        if let Some(launching) = &mut record.launching {
            launching.aborted = true;
        }
        write_json_atomic(&runner_json, record)
    };
    if cancelled() {
        abort(record)?;
        return Ok(Launch::Cancelled);
    }
    let program = match &prepared.argv {
        Ok(argv) => PathBuf::from(&argv[0]),
        Err(_) => PathBuf::new(),
    };
    let (stream, stderr) = (wd.join(STREAM), wd.join(STDERR));
    let files = File::create(&stream)
        .map_err(|e| Error::io(&stream, e))
        .and_then(|out| {
            File::create(&stderr)
                .map(|err| (out, err))
                .map_err(|e| Error::io(&stderr, e))
        });
    let (out, err) = match files {
        Ok(files) => files,
        Err(e) => {
            abort(record)?;
            return Ok(Launch::CannotStart(program, e));
        }
    };
    let argv = prepared.argv.as_ref().map_err(Clone::clone)?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(&prepared.env)
        .stdin(Stdio::piped())
        .stdout(out)
        .stderr(err);
    detach_session(&mut cmd);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            abort(record)?;
            let e = Error::io(&program, e);
            return Ok(Launch::CannotStart(program, e));
        }
    };
    let pid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    let info = crate::sys::proc_info(pid);
    record.codex = Some(StoppedProc {
        pid,
        pgid: info.as_ref().map_or(pid, |i| i.pgid),
        start: info.map(|i| i.start),
    });
    write_json_atomic(&runner_json, record)?;
    crash_point("after_worker_launch");
    Ok(Launch::Started(child))
}

/// `_cancel_deadline`: the canceller's deadline, from the cancel request, less
/// `CANCEL_FINISH_MARGIN`; `None` when it names none.
fn cancel_deadline(wd: &Path) -> Result<Option<Instant>> {
    let cancel = read_json_object(&wd.join(CANCEL))?.unwrap_or_default();
    let Some(raw) = cancel.get(&PyStr::from("deadline_wall")) else {
        return Ok(None);
    };
    // `isinstance(wall, (int, float))`, a bool counting as an int.
    let wall = match raw.get().trim() {
        "true" => 1.0,
        "false" => 0.0,
        text => match serde_json::from_str::<f64>(text) {
            Ok(wall) => wall,
            Err(_) => return Ok(None),
        },
    };
    let left = wall - wall_now() - CANCEL_FINISH_MARGIN.as_secs_f64();
    let now = Instant::now();
    Ok(if left >= 0.0 {
        Duration::try_from_secs_f64(left)
            .ok()
            .and_then(|d| now.checked_add(d))
    } else {
        Some(
            Duration::try_from_secs_f64(-left)
                .ok()
                .and_then(|d| now.checked_sub(d))
                .unwrap_or(now),
        )
    })
}

// --------------------------------------------------------------------------
// Watching: the threads, the channel and the core

/// What the threads send.
enum Input {
    Line(StreamView),
    Signal(i32),
    /// SIGCHLD: Codex may have exited.
    Child,
    /// The stream could not be read.
    Fault(Error),
}

/// Watches Codex with the core until everything recorded is gone: how it ended, and the
/// fault that ended it, if one did (the runner fails with it once the result is written).
fn watch(
    wd: &Path,
    req: &DispatchRequest,
    tracker: &mut Tracker,
    signals: Signals,
    exits: Signals,
) -> Result<(Finish, Option<Error>)> {
    let (tx, rx) = mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let handles = [signals.handle(), exits.handle()];
    let threads = [
        follow_stream(wd.join(STREAM), tx.clone(), Arc::clone(&done)),
        forward_signals(exits, tx.clone()),
        forward_signals(signals, tx),
    ];
    let mut shell = Shell {
        wd,
        tracker,
        fault: None,
        finish: None,
        exit_seen: false,
    };
    let watched = shell.run(Runner::new(req, Instant::now()), &rx);
    done.store(true, Ordering::SeqCst);
    for handle in handles {
        handle.close();
    }
    for thread in threads {
        let _ = thread.join();
    }
    watched
}

/// The stream thread: Codex's complete lines, as it writes them to `stream.jsonl`
/// (`_Stream.poll`: the file read from where the last look ended, every `POLL`).
fn follow_stream(path: PathBuf, tx: Sender<Input>, done: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let (mut offset, mut pending) = (0u64, Vec::new());
        while !done.load(Ordering::SeqCst) {
            let data = match read_from(&path, offset) {
                Ok(data) => data,
                Err(e) => {
                    let _ = tx.send(Input::Fault(e));
                    return;
                }
            };
            offset += data.len() as u64;
            pending.extend_from_slice(&data);
            while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                if let Some(view) = StreamView::parse(&line[..end])
                    && tx.send(Input::Line(view)).is_err()
                {
                    return;
                }
            }
            if data.is_empty() {
                thread::sleep(POLL);
            }
        }
    })
}

/// The bytes of `path` from `offset` on; none when the file is missing.
fn read_from(path: &Path, offset: u64) -> Result<Vec<u8>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(path, e)),
    };
    let mut data = Vec::new();
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_to_end(&mut data))
        .map_err(|e| Error::io(path, e))?;
    Ok(data)
}

/// A signal thread: each signal caught, until its handle is closed.
fn forward_signals(mut signals: Signals, tx: Sender<Input>) -> JoinHandle<()> {
    thread::spawn(move || {
        for sig in signals.forever() {
            let input = if sig == SIGCHLD {
                Input::Child
            } else {
                Input::Signal(sig)
            };
            if tx.send(input).is_err() {
                break;
            }
        }
    })
}

/// The shell's side of the watch: it carries out the core's actions.
struct Shell<'a> {
    wd: &'a Path,
    tracker: &'a mut Tracker,
    fault: Option<Error>,
    finish: Option<Finish>,
    exit_seen: bool,
}

impl Shell<'_> {
    fn run(&mut self, mut core: Runner, rx: &Receiver<Input>) -> Result<(Finish, Option<Error>)> {
        let mut next = Event::Tick;
        loop {
            self.feed(&mut core, next)?;
            if let Some(finish) = self.finish {
                return Ok((finish, self.fault.take()));
            }
            let wait = core.next_wakeup(Instant::now());
            next = match rx.recv_timeout(wait) {
                Ok(Input::Line(view)) => Event::Line(view),
                Ok(Input::Signal(sig)) => Event::Signal(sig),
                Ok(Input::Child) | Err(RecvTimeoutError::Timeout) => Event::Tick,
                Ok(Input::Fault(e)) => {
                    self.fault.get_or_insert(e);
                    Event::Fault
                }
                Err(RecvTimeoutError::Disconnected) => {
                    thread::sleep(wait.min(POLL));
                    Event::Tick
                }
            };
        }
    }

    /// One event, after the Python's `proc.poll()` and its look for a cancel request; then
    /// every action it leads to, the answers to snapshots and checks fed straight back.
    fn feed(&mut self, core: &mut Runner, ev: Event) -> Result<()> {
        let mut events: VecDeque<Event> = self.exit().into_iter().collect();
        if core.watching() && self.wd.join(CANCEL).exists() {
            events.push_back(Event::CancelRequested);
        }
        events.push_back(ev);
        while let Some(ev) = events.pop_front() {
            let mut actions: VecDeque<Action> = core.step(ev, Instant::now()).into();
            while let Some(action) = actions.pop_front() {
                match action {
                    Action::TouchHeartbeat => {
                        // Watching, a heartbeat that cannot be touched stops Codex ("error");
                        // stopping, it is let be, as the Python's `wait_gone` lets it be.
                        if let Err(e) = touch(&self.wd.join(HEARTBEAT))
                            && core.watching()
                        {
                            self.fault.get_or_insert(e);
                            events.push_back(Event::Fault);
                        }
                    }
                    Action::Snapshot { tagged } => {
                        self.tracker.snapshot(tagged);
                        for answer in self.answer() {
                            actions.extend(core.step(answer, Instant::now()));
                        }
                    }
                    Action::CheckAlive => {
                        for answer in self.answer() {
                            actions.extend(core.step(answer, Instant::now()));
                        }
                    }
                    Action::SignalGroups(sig) => self.tracker.signal(sig),
                    Action::Acknowledge(reason) => {
                        let ack = Stopping {
                            reason: reason.map_or("finished", StopReason::as_str),
                            at: utc_now(),
                        };
                        write_json_atomic(&self.wd.join(STOPPING), &ack)?;
                    }
                    Action::ChargeAndFinish(finish) => self.finish = Some(finish),
                }
            }
        }
        Ok(())
    }

    /// Codex's exit, the first time it is seen.
    fn exit(&mut self) -> Option<Event> {
        let status = self.tracker.exited()?;
        if std::mem::replace(&mut self.exit_seen, true) {
            return None;
        }
        Some(Event::Exited(status))
    }

    /// The answer to a snapshot or a check: Codex's exit first, when this is where it is
    /// first seen.
    fn answer(&mut self) -> Vec<Event> {
        let procs = self.tracker.liveness();
        let mut out: Vec<Event> = self.exit().into_iter().collect();
        out.push(Event::Procs(procs));
        out
    }
}

// --------------------------------------------------------------------------
// Processes: recording, signalling, confirming (`_Tracker`)

/// Codex and its recorded descendants (`_Tracker`): each recorded as (pid, pgid, start), a
/// process counting as the one recorded only while its pid still has that start time. With a
/// dispatch tag, a snapshot can also sweep up every process of this user carrying it, so a
/// process that left Codex's tree before a snapshot saw it is stopped too (M4).
pub struct Tracker {
    wd: PathBuf,
    codex: Option<StoppedProc>,
    child: Option<Child>,
    exit: Option<ExitStatus>,
    tag: Option<String>,
    /// The recorded processes not yet seen gone, in the order first recorded.
    live: Vec<StoppedProc>,
    /// Every process ever recorded, in that order.
    seen: Vec<StoppedProc>,
    saved: Option<ProcsFile>,
    unreadable: Unreadable,
}

impl Tracker {
    /// A tracker for Codex (`codex`, our own unreaped child when `child` is given), with the
    /// dispatch tag the sweep looks for.
    pub fn new(
        wd: &Path,
        codex: Option<StoppedProc>,
        child: Option<Child>,
        tag: Option<String>,
    ) -> Tracker {
        Tracker {
            wd: wd.to_path_buf(),
            codex,
            child,
            exit: None,
            tag,
            live: Vec::new(),
            seen: Vec::new(),
            saved: None,
            unreadable: Unreadable::Pids(Vec::new()),
        }
    }

    /// Codex's exit status, once our child has exited (it is reaped then).
    pub fn exited(&mut self) -> Option<ExitStatus> {
        if self.exit.is_none()
            && let Some(child) = &mut self.child
        {
            // An error polling our own child leaves it counted as running: never as gone.
            self.exit = child.try_wait().ok().flatten();
        }
        self.exit
    }

    /// Codex's exit status, waiting for it (`proc.wait()`).
    pub fn wait(&mut self) -> Option<ExitStatus> {
        if self.exit.is_none()
            && let Some(child) = &mut self.child
        {
            self.exit = child.wait().ok();
        }
        self.exit
    }

    pub fn codex_alive(&mut self) -> bool {
        if self.child.is_some() {
            // Our own unreaped child: its pid (and group) cannot be reused until we reap it,
            // so it is identified even when its start time could not be read.
            return self.exited().is_none();
        }
        self.codex
            .as_ref()
            .is_some_and(|c| procs::same_process(Some(c.pid), c.start.as_deref()))
    }

    /// The identity a target is signalled under: a recorded process's own record; Codex,
    /// while it is our unreaped child, as the process table shows its pid now.
    fn identity(&mut self, target: &StoppedProc, is_codex: bool) -> Option<ProcId> {
        if is_codex && self.child.is_some() {
            return if self.codex_alive() {
                procs::identify(target.pid)
            } else {
                None
            };
        }
        target.start.clone().map(|start| ProcId {
            pid: target.pid,
            start,
        })
    }

    fn add(&mut self, rec: StoppedProc) {
        let same = |p: &StoppedProc| p.pid == rec.pid && p.start == rec.start;
        if !self.live.iter().any(same) {
            self.live.push(rec.clone());
        }
        if !self.seen.iter().any(same) {
            self.seen.push(rec);
        }
    }

    /// `find_tagged`: every live process carrying the dispatch tag. Processes whose
    /// environment cannot be read (or a platform where none can) are noted in `procs.json`.
    fn find_tagged(&mut self) {
        let Some(tag) = &self.tag else {
            return;
        };
        match procs::tagged(&format!("{TAG_VAR}={tag}")) {
            None => self.unreadable = Unreadable::Unsupported,
            Some((found, unreadable)) => {
                self.unreadable = Unreadable::Pids(unreadable);
                for p in &found {
                    self.add(record_of(p));
                }
            }
        }
    }

    /// Records Codex's live descendants, and with `tagged` every live process carrying the
    /// tag; drops the recorded processes that are gone; writes `procs.json` when it changed.
    pub fn snapshot(&mut self, tagged: bool) {
        if self.codex_alive()
            && let Some(pid) = self.codex.as_ref().map(|c| c.pid)
        {
            for p in procs::descendants(pid) {
                self.add(record_of(&p));
            }
        }
        if tagged {
            self.find_tagged();
        }
        self.live
            .retain(|r| procs::same_process(Some(r.pid), r.start.as_deref()));
        let mut descendants = self.live.clone();
        descendants.sort_by_key(|p| p.pid);
        let current = ProcsFile {
            descendants,
            tag_unreadable: self.unreadable.clone(),
        };
        if self.saved.as_ref() != Some(&current) {
            // As the Python: a snapshot that cannot be written is not a failure.
            let _ = write_json_atomic(&self.wd.join(PROCS), &current);
            self.saved = Some(current);
        }
    }

    pub fn liveness(&mut self) -> Liveness {
        let codex = self.codex_alive();
        let any = codex
            || self
                .live
                .iter()
                .any(|r| procs::same_process(Some(r.pid), r.start.as_deref()));
        Liveness { codex, any }
    }

    /// What is still alive: Codex, then the recorded processes.
    pub fn alive(&mut self) -> Vec<StoppedProc> {
        let mut out: Vec<StoppedProc> = Vec::new();
        if self.codex_alive() {
            out.extend(self.codex.clone());
        }
        out.extend(
            self.live
                .iter()
                .filter(|r| procs::same_process(Some(r.pid), r.start.as_deref()))
                .cloned(),
        );
        out
    }

    /// One rung's signal (`_Tracker.signal`): first to each group whose leader is a target
    /// still with its identity, then to each other target, itself, with its identity. Codex
    /// alone for SIGINT; Codex and every recorded process otherwise.
    pub fn signal(&mut self, sig: Sig) {
        let mut targets: Vec<(StoppedProc, bool)> = Vec::new();
        if self.codex_alive()
            && let Some(codex) = &self.codex
        {
            targets.push((codex.clone(), true));
        }
        if !sig.codex_only() {
            targets.extend(self.live.iter().cloned().map(|r| (r, false)));
        }
        let signal = sig.signal();
        let mut groups: Vec<i32> = Vec::new();
        for (target, is_codex) in &targets {
            if target.pid == target.pgid
                && !groups.contains(&target.pgid)
                && let Some(id) = self.identity(target, *is_codex)
                && procs::signal_group_if_same(&id, target.pgid, signal)
            {
                groups.push(target.pgid);
            }
        }
        for (target, is_codex) in &targets {
            if !groups.contains(&target.pgid)
                && let Some(id) = self.identity(target, *is_codex)
            {
                procs::signal_if_same(&id, signal);
            }
        }
    }

    /// Codex and every process ever recorded, by pid: `result.json`'s `stopped`.
    pub fn stopped(&self) -> Vec<StoppedProc> {
        let mut out: Vec<StoppedProc> = self.codex.iter().chain(&self.seen).cloned().collect();
        out.sort_by_key(|p| p.pid);
        out
    }
}

fn record_of(p: &ProcInfo) -> StoppedProc {
    StoppedProc {
        pid: p.pid,
        pgid: p.pgid,
        start: Some(p.start.clone()),
    }
}

/// The stop ladder, run to its end (`_Tracker.stop`): what is still alive, empty once
/// everything is confirmed gone. With a deadline it gives up there; without one it sends
/// SIGKILL again until everything is gone.
pub fn stop(tracker: &mut Tracker, deadline: Option<Instant>) -> Vec<StoppedProc> {
    let mut ladder = Ladder::new(deadline);
    let mut actions: VecDeque<Action> = ladder.start(Instant::now()).into();
    loop {
        while let Some(action) = actions.pop_front() {
            match action {
                Action::Snapshot { tagged } => {
                    tracker.snapshot(tagged);
                    let procs = Event::Procs(tracker.liveness());
                    actions.extend(ladder.step(&procs, Instant::now()));
                }
                Action::CheckAlive => {
                    let procs = Event::Procs(tracker.liveness());
                    actions.extend(ladder.step(&procs, Instant::now()));
                }
                Action::SignalGroups(sig) => tracker.signal(sig),
                _ => {}
            }
        }
        match ladder.done() {
            Some(true) => return Vec::new(),
            Some(false) => return tracker.alive(),
            None => {}
        }
        thread::sleep(ladder.next_wakeup(Instant::now()));
        actions.extend(ladder.step(&Event::Tick, Instant::now()));
    }
}

// --------------------------------------------------------------------------
// Finalizing: classify, commit, charge, record the attempt, write result.json

/// The heartbeat, touched every second while a block runs (`_beating`): committing and
/// charging can take a while, and a stale heartbeat reads as hung.
struct Beating {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Beating {
    fn start(beat: &Path) -> Beating {
        let _ = touch(beat);
        let (tx, rx) = mpsc::channel::<()>();
        let beat = beat.to_path_buf();
        let thread = thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(HEARTBEAT_EVERY) {
                let _ = touch(&beat);
            }
        });
        Beating {
            stop: Some(tx),
            thread: Some(thread),
        }
    }
}

impl Drop for Beating {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What `runner.json` says about the generation, for finalizing.
struct RunnerFacts {
    generation: u32,
    /// Two-phase launch: an intent without a recorded Codex may still have started one.
    launched: bool,
}

/// `runner.json` as `_finalize_body` reads it: `generation` (1 when missing or false),
/// `codex`, and `launching` with its `aborted`, each as Python's truth tests read them.
fn runner_facts(wd: &Path) -> Result<RunnerFacts> {
    let runner = read_json_object(&wd.join(RUNNER))?.unwrap_or_default();
    let field = |key: &str| runner.get(&PyStr::from(key)).filter(|raw| truthy(raw));
    let generation = match field("generation") {
        None => 1,
        Some(raw) => match raw.get().trim() {
            "true" => 1,
            text => serde_json::from_str::<u32>(text).map_err(|_| {
                Error::msg(format!(
                    "runner.json: generation {text} is not a generation"
                ))
            })?,
        },
    };
    let launched = field("codex").is_some()
        || match field("launching") {
            None => false,
            Some(raw) => {
                let launching: PyMap<Box<RawValue>> = serde_json::from_str(raw.get())
                    .map_err(|_| Error::msg("runner.json: launching is not an object"))?;
                !launching
                    .get(&PyStr::from("aborted"))
                    .is_some_and(|a| truthy(a))
            }
        };
    Ok(RunnerFacts {
        generation,
        launched,
    })
}

/// The cancel request's token, when it is a string.
fn cancel_token(wd: &Path) -> Result<Option<String>> {
    let cancel = read_json_object(&wd.join(CANCEL))?.unwrap_or_default();
    Ok(cancel
        .get(&PyStr::from("token"))
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok()))
}

/// `_charge_usage`: (usage, charge IDs, source). The session file first, then the stream,
/// else unknown; nothing when Codex never ran.
fn charge_usage(
    wd: &Path,
    req: &DispatchRequest,
    thread: Option<&str>,
    launched: bool,
    generation: u32,
    incomplete: bool,
) -> Result<(Usage, Vec<PyStr>, &'static str)> {
    if !launched {
        return Ok((Usage::default(), Vec::new(), "none"));
    }
    let paths = run_paths(&req.run_dir);
    let suffix = if generation > 1 {
        format!(":g{generation}")
    } else {
        String::new()
    };
    let did = &req.dispatch_id;
    let (charge_id, key) = match thread.filter(|t| !t.is_empty()) {
        Some(t) => (format!("{did}:codex:{t}{suffix}"), t.to_string()),
        None => (
            format!("{did}:codex:unknown{suffix}"),
            format!("dispatch:{did}"),
        ),
    };
    let charge_id = PyStr::from(charge_id);
    let thread = thread.map(PyStr::from);
    let source = ChargeSource::Codex {
        thread: thread.as_ref(),
        sessions_dir: &default_sessions_dir(),
        stream: Some(&wd.join(STREAM)),
    };
    let c = charge(
        &paths,
        &charge_id,
        &PyStr::from(key),
        did,
        &req.role,
        source,
        incomplete,
    )?;
    if !c.usage.known {
        return Ok((Usage::unknown(), vec![charge_id], "unknown"));
    }
    let source = if c.source == CODEX_STREAM_SOURCE {
        "stream"
    } else {
        "session"
    };
    Ok((c.usage, vec![charge_id], source))
}

/// Finalize a dispatch (`_finalize`): classify it, commit a commit kind's work, charge the
/// usage, record the attempt and write `result.json`, under `finalize.lock`. A result already
/// there is returned as it is. With `beat`, the heartbeat is touched every second meanwhile.
///
/// S2-R32: a reply head the Python's `parse_report_head` raises `KeyError` on is an error
/// here, before anything is written, charged or recorded, as the Python's exception escapes.
#[allow(clippy::too_many_arguments)]
pub fn finalize(
    wd: &Path,
    req: &DispatchRequest,
    finish: Finish,
    stopped: &[StoppedProc],
    started_at: Option<&str>,
    deadline: Option<Instant>,
    beat: Option<&Path>,
    commits: &dyn Commits,
) -> Result<DispatchResult> {
    let _beating = beat.map(Beating::start);
    let lock_path = wd.join(FINALIZE_LOCK);
    let lock = open_lock_file(&lock_path)?;
    rustix::fs::flock(&lock, FlockOperation::LockExclusive)
        .map_err(|e| Error::io(&lock_path, e.into()))?;
    if let Some(existing) = load_result(wd)? {
        return Ok(existing);
    }
    let run_id = run_id_of(&req.run_dir)?;
    let runner = runner_facts(wd)?;
    let stream = StreamSummary::of(&read_from(&wd.join(STREAM), 0)?);
    let mut reply = read_text(&wd.join(LAST))?.unwrap_or_default();
    if py_strip(&reply.lossy()).is_empty() {
        reply = stream.last_agent_message.clone().unwrap_or_default();
    }
    let stderr = read_text(&wd.join(STDERR))?.unwrap_or_default();
    let classified = classify(
        finish.stopped_for,
        finish.codex_exit,
        &reply.lossy(),
        &stream.errors,
        &stderr.lossy(),
    )?;
    let (mut status, mut blocked_by, mut exit_reason) = (
        classified.status,
        classified.blocked_by,
        classified.exit_reason,
    );
    if !runner.launched && finish.stopped_for.is_none() {
        // Codex could not be started at all.
        blocked_by = BlockedBy::Environment;
    }
    if classified.keep_raw {
        write_text(&wd.join(RAW), &reply)?;
    }
    let incomplete = possibly_incomplete(
        runner.launched,
        finish.stopped_for,
        finish.codex_exit,
        stream.turn_open,
    );
    let mut commit = if runner.launched {
        commit_work(wd, req, deadline, commits)
    } else {
        CommitOutcome {
            commit_skipped: Some("Codex never ran".into()),
            ..CommitOutcome::default()
        }
    };
    let mut worker = None;
    if let Some(failed) = commit_failed(&commit) {
        // The work is still in the tree but not on the branch: an environment problem.
        let mut text = PyStr::from(
            "Status: BLOCKED\nBlocked by: environment\nleadhand could not commit the worker's \
             changes on ",
        );
        text.push(&req.branch.clone().unwrap_or_else(|| PyStr::from("None")));
        text.push_str(": ");
        text.push(&failed);
        if !py_strip(&reply.lossy()).is_empty() {
            text.push_str(&format!("\n\nWorker reply ({}):\n", exit_reason.as_str()));
            text.push(&reply);
        }
        reply = text;
        worker = Some((status, exit_reason));
        (status, blocked_by, exit_reason) =
            (Status::Blocked, BlockedBy::Environment, ExitReason::Error);
    }
    let (usage, charge_ids, source) = charge_usage(
        wd,
        req,
        stream.thread_id.as_deref(),
        runner.launched,
        runner.generation,
        incomplete,
    )?;
    crash_point("after_charge_before_attempt");
    let ended_at = utc_now();
    // Codex models are not priced: an empty table suffices.
    let cost = if usage.known {
        estimate_cost(&usage, Some(&req.model), &[])
    } else {
        None
    };
    let known = usage.known.then(|| usage.clone());
    let attempt = AttemptRecord {
        dispatch: Some(req.dispatch_id.clone()),
        run: Some(run_id),
        card: req.card.map(CardRef::Number),
        attempt: Some(req.attempt),
        role: Some(req.role.clone()),
        vendor: Some("codex".into()),
        model: Some(req.model.clone()),
        effort: Some(req.effort.clone()),
        agent: None,
        worker_id: stream.thread_id.as_deref().map(PyStr::from),
        started_at: Some(started_at.unwrap_or(&ended_at).to_string()),
        ended_at: Some(ended_at.clone()),
        steps: Some(i64::from(stream.steps)),
        charge_ids: charge_ids.clone(),
        usage: known.clone(),
        usage_known: Some(usage.known),
        usage_source: Some(source.to_string()),
        cost_estimate: cost,
        exit_reason: Some(exit_reason.as_str().to_string()),
        status: Some(status.as_str().to_string()),
        blocked_by: Some(blocked_by.as_str().to_string()),
        recorded_by: None,
        generation: Some(i64::from(runner.generation)),
        possibly_incomplete: Some(incomplete),
    };
    let paths = run_paths(&req.run_dir);
    {
        let _state = state_lock(&paths.root)?;
        let generation = i64::from(runner.generation);
        let recorded = read_attempts(&paths)?.iter().any(|a| {
            a.dispatch.as_deref() == Some(&req.dispatch_id)
                && a.generation.unwrap_or(1) == generation
        });
        if !recorded {
            append_attempt(&paths, &attempt)?;
        }
    }
    let mut stopped = stopped.to_vec();
    stopped.sort_by_key(|p| p.pid);
    let cancel_token = match exit_reason {
        ExitReason::Cancelled => cancel_token(wd)?,
        _ => None,
    };
    let result = DispatchResult {
        dispatch_id: req.dispatch_id.clone(),
        status,
        blocked_by,
        session_id: stream.thread_id.clone(),
        reply,
        steps: stream.steps,
        usage: known,
        usage_known: usage.known,
        usage_source: source.to_string(),
        possibly_incomplete: incomplete,
        exit_reason,
        codex_exit: finish.codex_exit,
        charge_ids,
        generation: runner.generation,
        stopped,
        confirmed_at: wall_now(),
        cancel_token,
        started_at: started_at.map(str::to_string),
        ended_at: Some(ended_at),
        committed: commit.committed.take(),
        changes: commit.changes,
        commit_skipped: commit.commit_skipped.take(),
        commit_error: commit.commit_error.take(),
        gitlinks_skipped: commit.gitlinks_skipped.take(),
        worker_status: worker.map(|(s, _)| s),
        worker_exit_reason: worker.map(|(_, e)| e),
    };
    write_json_atomic(&wd.join(RESULT), &result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::process::{Command, Output, Stdio};
    use std::time::Duration;

    use serde::Deserialize;

    use super::*;
    use crate::dispatch::{
        BlockedBy, ExitReason, RESULT, RUNNER, RUNNER_LOCK, STDERR, STOPPING, STREAM, Status,
        load_result,
    };
    use crate::git::fixture::Fixture;
    use crate::state::{AttemptRecord, read_attempts, run_paths};
    use crate::sys::detach_session;
    use crate::usage::{Charge, read_charges};

    #[cfg(feature = "testing")]
    const DEFAULT_THREAD: &str = "0f0f0f0f-0000-4000-8000-00000000c0de";

    fn manifest() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// A run with one worker folder, `d1`, whose workspace is a Git repository; and a `PATH`
    /// for the runner children that holds `python3` and `git` and never a real `codex`. Each
    /// world's run ID is its own (the pid and a counter), so no two tests' dispatch tags
    /// match: a runner's tag sweep stops every process carrying its tag.
    struct World {
        _fixture: Fixture,
        root: PathBuf,
        ws: PathBuf,
        run: PathBuf,
        wd: PathBuf,
        run_id: String,
    }

    impl World {
        fn new() -> World {
            static WORLDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = WORLDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let run_id = format!("r{}-{n}", std::process::id());
            let fixture = Fixture::new();
            let root = fixture.root().to_path_buf();
            let ws = fixture.make_repo("work space");
            let run = root.join("run dir");
            let wd = run.join("workers").join("d1");
            fs::create_dir_all(&wd).unwrap();
            let meta = format!(
                "{{\"run_id\": \"{run_id}\", \"workspace\": {}, \"repo\": {}}}\n",
                json_path(&ws),
                json_path(&ws)
            );
            fs::write(run.join("run.json"), meta).unwrap();
            let scratch = ws.join(".leadhand/1/d1");
            fs::create_dir_all(&scratch).unwrap();
            fs::write(scratch.join("brief.md"), "# Brief\n").unwrap();
            let bin = root.join("bin");
            fs::create_dir(&bin).unwrap();
            for tool in ["python3", "git"] {
                let real = crate::pytext::which(tool).expect("the tool is installed");
                std::os::unix::fs::symlink(real, bin.join(tool)).unwrap();
            }
            fs::create_dir(root.join("home")).unwrap();
            World {
                _fixture: fixture,
                root,
                ws,
                run,
                wd,
                run_id,
            }
        }

        fn request(&self) -> DispatchRequest {
            let scratch = self.ws.join(".leadhand/1/d1");
            DispatchRequest {
                model: "gpt-6-luna".into(),
                effort: "low".into(),
                role: "implementer".into(),
                brief: scratch.join("brief.md"),
                report: scratch.join("report.md"),
                cwd: self.ws.clone(),
                run_dir: self.run.clone(),
                card: Some(1),
                attempt: 1,
                dispatch_id: "d1".into(),
                steps: 50,
                minutes: 5,
                network: false,
                writable_dirs: Vec::new(),
                resume: None,
                kind: None,
                branch: None,
            }
        }

        fn write_request(&self, req: &DispatchRequest) {
            crate::store::write_json_atomic(&self.wd.join("request.json"), req).unwrap();
        }

        fn log(&self) -> PathBuf {
            self.root.join("codex log.jsonl")
        }

        /// The runner children's environment: the fake Codex with `scenario`, and nothing of
        /// this process's.
        fn env(&self, scenario: &str) -> Vec<(OsString, OsString)> {
            let mut path = self.root.join("bin").into_os_string();
            path.push(":/usr/bin:/bin");
            let vars: Vec<(&str, OsString)> = vec![
                ("PATH", path),
                ("HOME", self.root.join("home").into()),
                ("TMPDIR", std::env::temp_dir().into()),
                ("LEADHAND_TESTING", "1".into()),
                (
                    "LEADHAND_TEST_CODEX_BIN",
                    manifest().join("tests/fakes/codex").into(),
                ),
                (
                    "FAKE_CODEX_SCENARIO",
                    manifest()
                        .join("tests/fixtures/scenarios")
                        .join(format!("{scenario}.jsonl"))
                        .into(),
                ),
                ("FAKE_CODEX_LOG", self.log().into()),
                ("CODEX_HOME", self.root.join("codex home").into()),
                ("GIT_CONFIG_NOSYSTEM", "1".into()),
                ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ];
            let env: Vec<(OsString, OsString)> =
                vars.into_iter().map(|(k, v)| (k.into(), v)).collect();
            no_real_codex(&env);
            env
        }

        fn result(&self) -> Option<DispatchResult> {
            load_result(&self.wd).unwrap()
        }

        fn attempts(&self) -> Vec<AttemptRecord> {
            read_attempts(&run_paths(&self.run)).unwrap()
        }

        fn charges(&self) -> Vec<Charge> {
            read_charges(&run_paths(&self.run)).unwrap()
        }

        /// The fake Codex's log lines.
        fn records(&self) -> Vec<CodexLog> {
            match fs::read_to_string(self.log()) {
                Ok(text) => text
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect(),
                Err(_) => Vec::new(),
            }
        }
    }

    /// A line of the fake Codex's log: only the fields the tests read (the tests that run
    /// Codex need the `testing` build; the others only count the lines).
    #[derive(Deserialize)]
    #[cfg_attr(not(feature = "testing"), allow(dead_code))]
    struct CodexLog {
        event: String,
        #[serde(default)]
        prompt: Option<PyStr>,
        #[serde(default)]
        how: Option<String>,
    }

    fn json_path(p: &Path) -> String {
        crate::wire::ascii_json(&PyStr::from_os(p.as_os_str()))
    }

    /// Nothing on the children's `PATH` may be a real Codex.
    fn no_real_codex(env: &[(OsString, OsString)]) {
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v)
            .unwrap();
        for dir in path.as_bytes().split(|b| *b == b':') {
            let candidate = Path::new(OsStr::from_bytes(dir)).join("codex");
            assert!(
                !candidate.exists(),
                "{} would be found",
                candidate.display()
            );
        }
    }

    /// The worker folder `runner_child` runs the runner in.
    const CHILD_WD: &str = "LEADHAND_TEST_RUNNER_WD";

    /// Runs the runner in the worker folder `CHILD_WD` names, when it names one: this test
    /// binary run again as a child, so the runner's signal handlers and environment are its
    /// own. Exits with the runner's code, or 1 with `error: …` when it fails.
    #[test]
    fn runner_child() {
        let Some(wd) = std::env::var_os(CHILD_WD) else {
            return;
        };
        let code = match run_runner(Path::new(&wd)) {
            Ok(code) => i32::from(code),
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        };
        std::process::exit(code);
    }

    /// The Rust runner on `wd`, as a child with `env` only.
    fn rust_runner(wd: &Path, env: &[(OsString, OsString)]) -> Output {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "dispatch::shell::tests::runner_child",
                "--exact",
                "--test-threads=1",
                "--nocapture",
            ])
            .env_clear()
            .envs(env.iter().cloned())
            .env(CHILD_WD, wd)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The Python reference runner on `wd`, with `env` only.
    fn python_runner(wd: &Path, env: &[(OsString, OsString)]) -> Output {
        let code = "import sys\nfrom pathlib import Path\nfrom leadhand import dispatch\n\
                    sys.exit(dispatch.run_runner(Path(sys.argv[1])))\n";
        Command::new(crate::pytext::which("python3").unwrap())
            .args([
                OsStr::new("-B"),
                OsStr::new("-c"),
                OsStr::new(code),
                wd.as_os_str(),
            ])
            .env_clear()
            .envs(env.iter().cloned())
            .env("PYTHONPATH", manifest().join("scripts/lib"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn shown(out: &Output) -> String {
        format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    /// A result with what differs between two runs cleared: times and process identities.
    fn steady(mut r: DispatchResult) -> DispatchResult {
        r.confirmed_at = 0.0;
        r.started_at = None;
        r.ended_at = None;
        r.stopped.fill(StoppedProc {
            pid: 0,
            pgid: 0,
            start: None,
        });
        r
    }

    /// Attempts with their times cleared, and the run's own ID in place of `run`.
    fn steady_attempts(w: &World) -> Vec<AttemptRecord> {
        w.attempts()
            .into_iter()
            .map(|a| {
                assert_eq!(a.run.as_deref(), Some(w.run_id.as_str()));
                AttemptRecord {
                    run: None,
                    started_at: None,
                    ended_at: None,
                    ..a
                }
            })
            .collect()
    }

    fn steady_charges(charges: Vec<Charge>) -> Vec<Charge> {
        charges
            .into_iter()
            .map(|c| Charge {
                ts: String::new(),
                ..c
            })
            .collect()
    }

    /// Two worlds, the same request in each: the Rust runner runs in the first, the Python
    /// reference in the second.
    fn twins(change: impl Fn(&World, &mut DispatchRequest)) -> (World, World) {
        let (ours, theirs) = (World::new(), World::new());
        for w in [&ours, &theirs] {
            let mut req = w.request();
            change(w, &mut req);
            w.write_request(&req);
        }
        (ours, theirs)
    }

    /// What both runners left the same way: the result, the attempts and the charges.
    fn assert_same_records(ours: &World, theirs: &World) {
        assert_eq!(ours.result().map(steady), theirs.result().map(steady));
        assert_eq!(steady_attempts(ours), steady_attempts(theirs));
        assert_eq!(
            steady_charges(ours.charges()),
            steady_charges(theirs.charges())
        );
    }

    /// `untracked.json`'s paths.
    #[cfg(feature = "testing")]
    fn untracked(w: &World) -> Option<Vec<PyStr>> {
        #[derive(Deserialize)]
        struct Untracked {
            paths: Option<Vec<PyStr>>,
        }
        let text = fs::read_to_string(w.wd.join(crate::dispatch::UNTRACKED)).ok()?;
        serde_json::from_str::<Untracked>(&text).unwrap().paths
    }

    #[cfg(feature = "testing")]
    #[test]
    fn fake_codex_ready_scenario_writes_result_and_attempt() {
        let (ours, theirs) = twins(|_, _| {});
        let out = rust_runner(&ours.wd, &ours.env("ready"));
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        let r = ours.result().expect("result.json");
        assert_eq!(
            (r.status, r.blocked_by, r.exit_reason),
            (Status::Ready, BlockedBy::Nothing, ExitReason::Completed)
        );
        assert_eq!(r.session_id.as_deref(), Some(DEFAULT_THREAD));
        assert_eq!(r.steps, 2);
        assert!(r.reply.starts_with("Status: READY"));
        assert_eq!((r.codex_exit, r.generation), (Some(0), 1));
        assert!(r.usage_known && !r.possibly_incomplete);
        assert_eq!(r.usage.as_ref().map(|u| u.tokens.output), Some(50));
        assert_eq!(r.usage_source, "stream");
        let charge_id = PyStr::from(format!("d1:codex:{DEFAULT_THREAD}"));
        assert_eq!(r.charge_ids, std::slice::from_ref(&charge_id));
        assert_eq!(
            (r.committed.as_deref(), r.changes, r.commit_skipped.clone()),
            (
                None,
                false,
                Some(PyStr::from("this dispatch does not commit"))
            )
        );
        assert_eq!(r.stopped.len(), 1, "Codex alone: {:?}", r.stopped);
        let [attempt] = &ours.attempts()[..] else {
            panic!("{:?}", ours.attempts())
        };
        assert_eq!(
            (
                attempt.dispatch.as_deref(),
                attempt.run.as_deref(),
                attempt.vendor.as_deref()
            ),
            (Some("d1"), Some(ours.run_id.as_str()), Some("codex"))
        );
        assert_eq!(attempt.worker_id, Some(PyStr::from(DEFAULT_THREAD)));
        assert_eq!(
            (
                attempt.status.as_deref(),
                attempt.exit_reason.as_deref(),
                attempt.steps
            ),
            (Some("READY"), Some("completed"), Some(2))
        );
        assert_eq!(attempt.charge_ids, [charge_id]);
        assert_eq!(attempt.cost_estimate, None);
        let [charge] = &ours.charges()[..] else {
            panic!("{:?}", ours.charges())
        };
        assert_eq!(charge.source, "codex-stream");
        // Codex got the prompt on stdin, and its stream is on disk as it wrote it.
        let req = ours.request();
        let prompt = crate::briefs::worker_prompt(&req.role, &req.brief, &req.report).unwrap();
        let start = ours
            .records()
            .into_iter()
            .find(|r| r.event == "start")
            .unwrap();
        assert_eq!(start.prompt, Some(PyStr::from_os(&prompt)));
        let stream = fs::read_to_string(ours.wd.join(STREAM)).unwrap();
        assert!(
            stream.starts_with("{\"type\": \"thread.started\""),
            "{stream}"
        );
        assert!(ours.wd.join("heartbeat").is_file());
        let stopping = fs::read_to_string(ours.wd.join(STOPPING)).unwrap();
        assert!(stopping.contains("\"reason\": \"finished\""), "{stopping}");
        let runner = fs::read_to_string(ours.wd.join(RUNNER)).unwrap();
        let tag = format!("\"tag\": \"{}:d1:1\"", ours.run_id);
        assert!(runner.contains(&tag), "{runner}");
        assert!(ours.wd.join(crate::dispatch::LAST).is_file());

        // The Python reference runner, on the same request, leaves the same records.
        let py = python_runner(&theirs.wd, &theirs.env("ready"));
        assert_eq!(py.status.code(), Some(0), "{}", shown(&py));
        assert_same_records(&ours, &theirs);
        assert_eq!(
            fs::read(ours.wd.join(STREAM)).unwrap(),
            fs::read(theirs.wd.join(STREAM)).unwrap()
        );
        assert_eq!(untracked(&ours), untracked(&theirs));
        assert!(untracked(&ours).is_some());
    }

    #[cfg(feature = "testing")]
    #[test]
    fn step_limit_scenario_stops_codex_with_sigint() {
        let (ours, theirs) = twins(|_, req| req.steps = 3);
        let started = Instant::now();
        let out = rust_runner(&ours.wd, &ours.env("many-steps"));
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        assert!(started.elapsed() < Duration::from_secs(20));
        let r = ours.result().unwrap();
        assert_eq!(
            (r.status, r.exit_reason),
            (Status::Spent, ExitReason::StepLimit)
        );
        assert!((3..9).contains(&r.steps), "{}", r.steps);
        assert!(r.possibly_incomplete);
        let exits: Vec<Option<String>> = ours
            .records()
            .into_iter()
            .filter(|r| r.event == "exit")
            .map(|r| r.how)
            .collect();
        assert_eq!(exits, [Some("sigint".to_string())]);
        let stopping = fs::read_to_string(ours.wd.join(STOPPING)).unwrap();
        assert!(
            stopping.contains("\"reason\": \"step_limit\""),
            "{stopping}"
        );
        let py = python_runner(&theirs.wd, &theirs.env("many-steps"));
        assert_eq!(py.status.code(), Some(0), "{}", shown(&py));
        let theirs_r = theirs.result().unwrap();
        assert_eq!(
            (theirs_r.status, theirs_r.exit_reason, theirs_r.codex_exit),
            (r.status, r.exit_reason, r.codex_exit)
        );
    }

    #[cfg(feature = "testing")]
    #[test]
    fn crafted_report_head_kills_the_runner_before_charging() {
        // S2-R32: a head the Python's `parse_report_head` raises KeyError on escapes
        // `_classify` before anything is charged or recorded; the dispatch is left to
        // dead-runner reconciliation.
        let (ours, theirs) = twins(|_, _| {});
        let scenario = ours.root.join("crafted.jsonl");
        fs::write(
            &scenario,
            "{\"type\": \"thread.started\", \"thread_id\": \"{thread}\"}\n\
             {\"action\": \"last_message\", \"text\": \"re\\u017fult: pass\\n\"}\n",
        )
        .unwrap();
        let with_scenario = |w: &World| -> Vec<(OsString, OsString)> {
            w.env("ready")
                .into_iter()
                .map(|(k, v)| {
                    if k == "FAKE_CODEX_SCENARIO" {
                        (k, scenario.clone().into())
                    } else {
                        (k, v)
                    }
                })
                .collect()
        };
        let out = rust_runner(&ours.wd, &with_scenario(&ours));
        assert_eq!(out.status.code(), Some(1), "{}", shown(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("KeyError: 'Re\u{17f}ult'"),
            "{}",
            shown(&out)
        );
        let py = python_runner(&theirs.wd, &with_scenario(&theirs));
        assert_eq!(py.status.code(), Some(1), "{}", shown(&py));
        for w in [&ours, &theirs] {
            assert!(!w.wd.join(RESULT).exists());
            assert_eq!(w.attempts(), []);
            assert_eq!(w.charges(), []);
            assert!(w.wd.join(STOPPING).exists(), "it stopped before finalizing");
        }
    }

    /// The text of `codex.stderr`.
    fn stderr_text(w: &World) -> String {
        String::from_utf8(fs::read(w.wd.join(STDERR)).unwrap()).unwrap()
    }

    #[test]
    fn prompt_that_is_not_utf8_is_blocked_as_python_blocks_it() {
        // S2-R33: Python sends `worker_prompt(...).encode("utf-8")`, strictly; a brief path
        // with an undecodable byte makes it raise, and the worker ends BLOCKED, never started.
        let (ours, theirs) = twins(|w, req| {
            let mut name = OsString::from("bri");
            name.push(OsStr::from_bytes(b"\xff\xfe"));
            name.push("ef.md");
            req.brief = w.ws.join(".leadhand/1/d1").join(name);
        });
        let out = rust_runner(&ours.wd, &ours.env("ready"));
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        let py = python_runner(&theirs.wd, &theirs.env("ready"));
        assert_eq!(py.status.code(), Some(0), "{}", shown(&py));
        let text = stderr_text(&ours);
        assert!(
            text.starts_with(
                "leadhand: cannot prepare the Codex worker: 'utf-8' codec can't encode \
                 characters in position "
            ) && text.ends_with(": surrogates not allowed\n"),
            "{text}"
        );
        assert_eq!(text, stderr_text(&theirs));
        let r = ours.result().unwrap();
        assert_eq!(
            (r.status, r.blocked_by, r.exit_reason),
            (Status::Blocked, BlockedBy::Environment, ExitReason::Error)
        );
        assert_eq!(r.usage_source, "none");
        assert_same_records(&ours, &theirs);
        assert!(ours.records().is_empty(), "Codex never started");
    }

    #[test]
    fn argument_the_os_cannot_take_kills_the_runner_at_launch() {
        // A model with a lone surrogate that is not a surrogate escape. The Python builds the
        // command line and fails at `Popen`, after the launch intent, with an exception that
        // is not an OSError: the runner dies, nothing is started or finalized, and the intent
        // is not marked aborted.
        let (ours, theirs) = twins(|_, req| {
            req.model = serde_json::from_str(r#""gpt\ud800""#).unwrap();
        });
        let out = rust_runner(&ours.wd, &ours.env("ready"));
        assert_eq!(out.status.code(), Some(1), "{}", shown(&out));
        let py = python_runner(&theirs.wd, &theirs.env("ready"));
        assert_eq!(py.status.code(), Some(1), "{}", shown(&py));
        #[derive(Deserialize)]
        struct Launch {
            codex: Option<StoppedProc>,
            launching: Launching,
        }
        #[derive(Deserialize, Debug, PartialEq)]
        struct Launching {
            #[serde(default)]
            aborted: Option<bool>,
        }
        for w in [&ours, &theirs] {
            let runner: Launch =
                serde_json::from_str(&fs::read_to_string(w.wd.join(RUNNER)).unwrap()).unwrap();
            assert_eq!((runner.codex, runner.launching.aborted), (None, None));
            assert_eq!(fs::read(w.wd.join(STREAM)).unwrap(), b"");
            assert_eq!(fs::read(w.wd.join(STDERR)).unwrap(), b"");
            assert!(!w.wd.join(RESULT).exists());
            assert!(!w.wd.join(STOPPING).exists());
            assert_eq!(w.attempts(), []);
            assert!(w.records().is_empty());
        }
    }

    #[test]
    fn cancel_before_launch_finalizes_cancelled() {
        let (ours, theirs) = twins(|_, _| {});
        for w in [&ours, &theirs] {
            fs::write(
                w.wd.join("cancel"),
                "{\"requested_at\": \"earlier\", \"token\": \"tok\"}",
            )
            .unwrap();
        }
        let out = rust_runner(&ours.wd, &ours.env("ready"));
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        let py = python_runner(&theirs.wd, &theirs.env("ready"));
        assert_eq!(py.status.code(), Some(0), "{}", shown(&py));
        let r = ours.result().unwrap();
        assert_eq!(
            (r.status, r.exit_reason),
            (Status::Spent, ExitReason::Cancelled)
        );
        assert_eq!(r.cancel_token.as_deref(), Some("tok"));
        assert_same_records(&ours, &theirs);
        assert!(ours.records().is_empty());
    }

    #[test]
    fn second_runner_exits_3_and_touches_nothing() {
        let w = World::new();
        w.write_request(&w.request());
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(w.wd.join(RUNNER_LOCK))
            .unwrap();
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
        let out = rust_runner(&w.wd, &w.env("ready"));
        assert_eq!(out.status.code(), Some(3), "{}", shown(&out));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&format!("leadhand: another runner owns {}", w.wd.display())),
            "{err}"
        );
        assert!(!w.wd.join(RUNNER).exists());
        assert!(w.records().is_empty());
    }

    // CATALOG: test_dispatch.py::FixRoundTests::test_unknown_codex_start_time_still_enforced
    #[test]
    fn unknown_codex_start_time_still_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdin(Stdio::null());
        detach_session(&mut cmd);
        let child = cmd.spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let record = StoppedProc {
            pid,
            pgid: pid,
            start: None,
        };
        let mut tracker = Tracker::new(tmp.path(), Some(record), Some(child), None);
        assert!(tracker.codex_alive());
        let left = stop(&mut tracker, Some(Instant::now() + Duration::from_secs(15)));
        assert_eq!(left, []);
        assert!(tracker.exited().is_some());
    }

    #[test]
    fn ladder_stops_a_tool_child_codex_left_in_its_own_group() {
        // Codex starts a tool command in a process group of its own and leaves it behind at
        // SIGINT: the snapshot recorded it, so SIGTERM to its group stops it.
        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("python3");
        cmd.args([
            "-c",
            "import subprocess, sys\n\
             p = subprocess.Popen(['sleep', '30'], start_new_session=True, stdin=subprocess.DEVNULL)\n\
             print(p.pid, flush=True)\n\
             sys.stdin.read()\n",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        detach_session(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let _stdin = child.stdin.take();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let tool: i32 = line.trim().parse().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let start = crate::procs::identify(pid).map(|id| id.start);
        let record = StoppedProc {
            pid,
            pgid: pid,
            start,
        };
        let mut tracker = Tracker::new(tmp.path(), Some(record), Some(child), None);
        tracker.snapshot(false);
        let procs = fs::read_to_string(tmp.path().join("procs.json")).unwrap();
        assert!(procs.contains(&format!("\"pid\": {tool}")), "{procs}");
        let started = Instant::now();
        assert_eq!(stop(&mut tracker, None), []);
        assert!(started.elapsed() < Duration::from_secs(12));
        assert_eq!(crate::procs::identify(tool), None);
        let stopped: Vec<i32> = tracker.stopped().iter().map(|p| p.pid).collect();
        assert!(
            stopped.contains(&pid) && stopped.contains(&tool),
            "{stopped:?}"
        );
    }
}
