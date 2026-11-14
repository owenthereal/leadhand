//! The dispatch runner's shell (`dispatch.run_runner` and what it calls): the runner's life
//! in its worker folder, and the threads and I/O that feed the pure core (`runner.rs`). The
//! processes are the tracker's (`tracker.rs`), the result `finalize.rs`'s.
//!
//! Codex's stdout is `stream.jsonl` itself, as the Python's `Popen(stdout=file)` makes it, so
//! everything Codex writes is on disk even while the runner is stopped, and after it dies;
//! the stream thread follows the file. Three threads feed the core one channel: the stream's
//! lines, Codex's exit (SIGCHLD) and the runner's SIGTERM; a receive with a timeout supplies
//! the ticks. Before every event the shell looks as the Python's loop looks, in its order:
//! Codex's exit (`try_wait`, the Python's `proc.poll()`), then a cancel (SIGTERM or the
//! `cancel` file, `cancelled()`); so an exit that came first is seen first.
//!
//! At its start the runner sets SIGHUP to ignored and keeps every inherited descriptor from
//! its children, as the Python's `SIG_IGN` and `Popen(close_fds=True)` do.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::fs::FlockOperation;
use rustix::process::{getpgrp, getpid};
use serde::Serialize;
use signal_hook::consts::{SIGCHLD, SIGTERM};
use signal_hook::iterator::Signals;

use super::finalize::{Commits, NoCommit, finalize};
use super::request::StoppedProc;
use super::runner::{
    Action, CANCEL_FINISH_MARGIN, Event, Finish, POLL, RUNNER_LOCK_RETRY, Runner, StopReason,
    StreamView, return_code,
};
use super::tracker::{Tracker, carry_out};
use super::{
    CANCEL, DispatchRequest, Env, GH_DIR, HEARTBEAT, LAST, RUNNER, RUNNER_LOCK, STDERR, STOPPING,
    STREAM, SUPERSEDED, UNTRACKED, codex_argv_parts, codex_overrides, evidence_dirs, load_result,
    open_lock_file, read_from, read_json_object, read_request, run_id_of, touch, wall_now,
    worker_env, write_text,
};
use crate::briefs::worker_prompt;
use crate::error::{Error, Failure, Result};
use crate::git::{Git, git_common_dir, split_nul};
use crate::state::utc_now;
use crate::store::write_json_atomic;
use crate::sys::detach_session;
use crate::testing::crash_point;
use crate::wire::PyStr;

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
    // First of all, as the Python: SIGHUP is ignored, so Codex and its tools inherit it
    // ignored (S2-R37); nothing this process inherited reaches them (`close_fds=True`,
    // S2-R38); SIGTERM is a cancel request.
    crate::sys::ignore_sighup();
    crate::sys::keep_inherited_fds_from_children()
        .map_err(|e| Error::msg(format!("cannot list this process's descriptors: {e}")))?;
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&term)).map_err(signal_error)?;
    let signals = Signals::new([SIGTERM]).map_err(signal_error)?;
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
    let (finish, fault) = watch(wd, &req, &mut tracker, &term, signals, exits)?;
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

/// This runner's generation: one more than the generations already superseded.
fn generation_of(wd: &Path) -> Result<u32> {
    let dir = wd.join(SUPERSEDED);
    if !dir.is_dir() {
        return Ok(1);
    }
    let entries = fs::read_dir(&dir).map_err(|e| Error::io(&dir, e))?.count();
    Ok(u32::try_from(entries).map_or(u32::MAX, |n| n.saturating_add(1)))
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
    term: &AtomicBool,
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
        term,
        fault: None,
        finish: None,
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
    /// SIGTERM's flag (`term.is_set()`).
    term: &'a AtomicBool,
    fault: Option<Error>,
    finish: Option<Finish>,
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
                Ok(input) => self.event_of(&core, input),
                Err(RecvTimeoutError::Timeout) => Event::Tick,
                Err(RecvTimeoutError::Disconnected) => {
                    thread::sleep(wait.min(POLL));
                    Event::Tick
                }
            };
        }
    }

    /// The event a thread's input is. A stream that cannot be read stops Codex ("error"), and
    /// fails the runner once the result is written, only while Codex is watched: the Python
    /// reads the stream only in its watch loop, never while stopping.
    fn event_of(&mut self, core: &Runner, input: Input) -> Event {
        match input {
            Input::Line(view) => Event::Line(view),
            Input::Signal(sig) => Event::Signal(sig),
            Input::Child => Event::Tick,
            Input::Fault(e) if core.watching() => {
                self.fault.get_or_insert(e);
                Event::Fault
            }
            Input::Fault(_) => Event::Tick,
        }
    }

    /// One event, after the Python's loop's first two looks, in its order: `proc.poll()`
    /// (Codex's exit), then `cancelled()` (SIGTERM's flag, or the `cancel` file). Then every
    /// action it leads to, carried out with the tracker.
    fn feed(&mut self, core: &mut Runner, ev: Event) -> Result<()> {
        let mut events: VecDeque<Event> = self
            .tracker
            .newly_exited()
            .map(Event::Exited)
            .into_iter()
            .collect();
        if core.watching() && (self.term.load(Ordering::SeqCst) || self.wd.join(CANCEL).exists()) {
            events.push_back(Event::CancelRequested);
        }
        events.push_back(ev);
        while let Some(ev) = events.pop_front() {
            let actions = core.step(ev, Instant::now());
            let (wd, finish) = (self.wd, &mut self.finish);
            let mut beat_failed = None;
            carry_out(&mut *self.tracker, core, actions, &mut |action| {
                match action {
                    Action::TouchHeartbeat => {
                        if let Err(e) = touch(&wd.join(HEARTBEAT)) {
                            beat_failed.get_or_insert(e);
                        }
                    }
                    Action::Acknowledge(reason) => {
                        let ack = Stopping {
                            reason: reason.map_or("finished", StopReason::as_str),
                            at: utc_now(),
                        };
                        write_json_atomic(&wd.join(STOPPING), &ack)?;
                    }
                    Action::ChargeAndFinish(done) => *finish = Some(done),
                    // The tracker's own actions.
                    Action::Snapshot { .. } | Action::CheckAlive(_) | Action::SignalGroups(_) => {}
                }
                Ok(())
            })?;
            // Watching, a heartbeat that cannot be touched stops Codex ("error"); stopping, it
            // is let be, as the Python's `wait_gone` lets it be.
            if let Some(e) = beat_failed
                && core.watching()
            {
                self.fault.get_or_insert(e);
                events.push_back(Event::Fault);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::BorrowedFd;
    use std::process::Output;

    use serde::Deserialize;

    use super::*;
    use crate::dispatch::runner::Limits;
    use crate::dispatch::{BlockedBy, DispatchResult, ExitReason, RESULT, Status, load_result};
    use crate::git::fixture::Fixture;
    use crate::state::{AttemptRecord, read_attempts, run_paths};
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
        #[serde(default)]
        escaped_pid: Option<i32>,
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
        rust_runner_with(wd, env, None)
    }

    /// `rust_runner`, the child also inheriting `fd` without close-on-exec, as a runner can
    /// inherit a descriptor from whoever started it.
    fn rust_runner_with(
        wd: &Path,
        env: &[(OsString, OsString)],
        fd: Option<BorrowedFd<'_>>,
    ) -> Output {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "dispatch::shell::tests::runner_child",
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ])
        .env_clear()
        .envs(env.iter().cloned())
        .env(CHILD_WD, wd)
        .stdin(Stdio::null());
        if let Some(fd) = fd {
            crate::sys::pass_fd_to_child(&mut cmd, fd);
        }
        cmd.output().unwrap()
    }

    /// The Python reference runner on `wd`, with `env` only.
    fn python_runner(wd: &Path, env: &[(OsString, OsString)]) -> Output {
        python_runner_with(wd, env, None)
    }

    /// `python_runner`, the child also inheriting `fd` without close-on-exec.
    fn python_runner_with(
        wd: &Path,
        env: &[(OsString, OsString)],
        fd: Option<BorrowedFd<'_>>,
    ) -> Output {
        let code = "import sys\nfrom pathlib import Path\nfrom leadhand import dispatch\n\
                    sys.exit(dispatch.run_runner(Path(sys.argv[1])))\n";
        let mut cmd = Command::new(crate::pytext::which("python3").unwrap());
        cmd.args([
            OsStr::new("-B"),
            OsStr::new("-c"),
            OsStr::new(code),
            wd.as_os_str(),
        ])
        .env_clear()
        .envs(env.iter().cloned())
        .env("PYTHONPATH", manifest().join("scripts/lib"))
        .stdin(Stdio::null());
        if let Some(fd) = fd {
            crate::sys::pass_fd_to_child(&mut cmd, fd);
        }
        cmd.output().unwrap()
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

    /// A process recorded by identity: killed when the test ends if it is still that process.
    #[cfg(feature = "testing")]
    struct KillOnDrop(crate::procs::ProcId);

    #[cfg(feature = "testing")]
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            crate::procs::signal_if_same(&self.0, rustix::process::Signal::KILL);
        }
    }

    #[cfg(feature = "testing")]
    #[test]
    fn a_process_escaping_codexs_tree_is_stopped_before_finalizing() {
        // M4: the fake's escaped writer double-forks into a new session, so it leaves Codex's
        // tree, and writes on after Codex exits. Only the tag sweep after the exit finds it;
        // it is stopped, and recorded, before the result is written.
        let w = World::new();
        w.write_request(&w.request());
        let out = rust_runner(&w.wd, &w.env("ready-escaped-writer"));
        let escaped = w
            .records()
            .into_iter()
            .find(|r| r.event == "escaped")
            .and_then(|r| r.escaped_pid);
        let _guard = escaped.and_then(crate::procs::identify).map(KillOnDrop);
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        let r = w.result().unwrap();
        assert_eq!(r.status, Status::Ready);
        let pid = escaped.expect("the fake logged its escaped writer");
        assert!(r.stopped.iter().any(|p| p.pid == pid), "{:?}", r.stopped);
        assert_eq!(
            crate::procs::identify(pid),
            None,
            "the escaped writer outlived the dispatch"
        );
    }

    /// A stand-in for Codex that reports what it inherited into `$PROBE_OUT`: whether
    /// descriptor `$PROBE_FD` is open, then whether it survives a SIGHUP it sends itself.
    #[cfg(feature = "testing")]
    const PROBE: &str = "#!/bin/sh\n\
        if [ -e \"/dev/fd/$PROBE_FD\" ]; then echo 'fd open' > \"$PROBE_OUT\"; \
        else echo 'fd closed' > \"$PROBE_OUT\"; fi\n\
        kill -HUP $$\n\
        echo 'survived SIGHUP' >> \"$PROBE_OUT\"\n";

    #[cfg(feature = "testing")]
    #[test]
    fn codex_gets_sighup_ignored_and_no_inherited_descriptor() {
        // S2-R37 and S2-R38, as the Python's runner gives them: SIGHUP's `SIG_IGN` is kept
        // across exec, and `close_fds=True` keeps a descriptor the runner inherited out of
        // Codex.
        use std::os::fd::{AsFd, AsRawFd};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::ExitStatusExt;

        let (ours, theirs) = twins(|_, _| {});
        let probe = ours.root.join("probe codex");
        fs::write(&probe, PROBE).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        let held = File::create(ours.root.join("held")).unwrap();
        let fd = rustix::io::fcntl_dupfd_cloexec(&held, 60).unwrap();
        let number = fd.as_raw_fd().to_string();
        // The probe sees both when nothing hides them: run from here with the descriptor
        // passed, and SIGHUP's default action, it reports the descriptor and dies of SIGHUP.
        let control = ours.root.join("control.out");
        let mut cmd = Command::new(&probe);
        cmd.env("PROBE_FD", &number)
            .env("PROBE_OUT", &control)
            .stdin(Stdio::null());
        crate::sys::pass_fd_to_child(&mut cmd, fd.as_fd());
        let status = cmd.status().unwrap();
        assert_eq!(status.signal(), Some(rustix::process::Signal::HUP.as_raw()));
        assert_eq!(fs::read_to_string(&control).unwrap(), "fd open\n");
        // Through either runner, given the same descriptor without close-on-exec: Codex has
        // neither.
        for (w, python) in [(&ours, false), (&theirs, true)] {
            let out_file = w.root.join("probe.out");
            let env: Vec<(OsString, OsString)> = w
                .env("ready")
                .into_iter()
                .map(|(k, v)| {
                    if k == "LEADHAND_TEST_CODEX_BIN" {
                        (k, probe.clone().into())
                    } else {
                        (k, v)
                    }
                })
                .chain([
                    ("PROBE_FD".into(), number.clone().into()),
                    ("PROBE_OUT".into(), out_file.clone().into()),
                ])
                .collect();
            let out = if python {
                python_runner_with(&w.wd, &env, Some(fd.as_fd()))
            } else {
                rust_runner_with(&w.wd, &env, Some(fd.as_fd()))
            };
            assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
            assert_eq!(
                fs::read_to_string(&out_file).unwrap(),
                "fd closed\nsurvived SIGHUP\n",
                "python: {python}"
            );
        }
    }

    /// A shell watching no Codex, in a folder of its own: for the shell's own rules.
    fn bare_shell<'a>(wd: &'a Path, tracker: &'a mut Tracker, term: &'a AtomicBool) -> Shell<'a> {
        Shell {
            wd,
            tracker,
            term,
            fault: None,
            finish: None,
        }
    }

    #[test]
    fn a_stream_fault_counts_only_while_codex_is_watched() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tracker = Tracker::new(tmp.path(), None, None, None);
        let term = AtomicBool::new(false);
        let mut shell = bare_shell(tmp.path(), &mut tracker, &term);
        let t0 = Instant::now();
        let limits = Limits {
            steps: 50,
            time: Duration::MAX,
        };
        let fault = || Input::Fault(Error::msg("the stream cannot be read"));
        let watching = Runner::with_limits(limits, t0);
        assert_eq!(shell.event_of(&watching, fault()), Event::Fault);
        assert!(shell.fault.take().is_some());
        // Stopping, the Python reads the stream no more: nothing to fail the runner with.
        let mut stopping = Runner::with_limits(limits, t0);
        let _ = stopping.step(Event::CancelRequested, t0);
        assert!(!stopping.watching());
        assert_eq!(shell.event_of(&stopping, fault()), Event::Tick);
        assert!(shell.fault.is_none());
    }

    #[test]
    fn sigterm_comes_before_a_step_limit_seen_in_the_same_look() {
        // The Python's loop checks `cancelled()` (SIGTERM's flag, or the cancel file) before
        // the steps: a step line read in the same look as a SIGTERM still stops for the
        // cancel.
        for (term, want) in [
            (true, StopReason::Cancelled),
            (false, StopReason::StepLimit),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut tracker = Tracker::new(tmp.path(), None, None, None);
            let term = AtomicBool::new(term);
            let mut shell = bare_shell(tmp.path(), &mut tracker, &term);
            let limits = Limits {
                steps: 1,
                time: Duration::MAX,
            };
            let mut core = Runner::with_limits(limits, Instant::now());
            let step = StreamView::parse(
                br#"{"type": "item.completed", "item": {"type": "command_execution"}}"#,
            )
            .unwrap();
            shell.feed(&mut core, Event::Line(step)).unwrap();
            assert_eq!(shell.finish.map(|f| f.stopped_for), Some(Some(want)));
            let ack = fs::read_to_string(tmp.path().join(STOPPING)).unwrap();
            assert!(
                ack.contains(&format!("\"reason\": \"{}\"", want.as_str())),
                "{ack}"
            );
        }
    }
}
