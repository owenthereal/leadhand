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
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
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
use crate::error::{Error, Result};
use crate::git::{git_common_dir, untracked_files};
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
            pgid: Some(me.pgid),
            start: Some(me.start),
        },
        None => StoppedProc {
            pid,
            pgid: Some(getpgrp().as_raw_pid()),
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
        pgid: Some(info.as_ref().map_or(pid, |i| i.pgid)),
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
    use std::ffi::OsStr;
    use std::os::fd::BorrowedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::process::Output;

    use serde::Deserialize;

    use super::*;
    use crate::dispatch::runner::Limits;
    use crate::dispatch::{BlockedBy, DispatchResult, ExitReason, RESULT, Status, load_result};
    use crate::git::fixture::Fixture;
    use crate::state::{AttemptRecord, read_attempts, run_paths};
    use crate::usage::{Charge, read_charges};
    #[cfg(feature = "testing")]
    use crate::wire::tests::fake_bin;
    use crate::wire::tests::{reference_fixture, reference_json};

    #[cfg(feature = "testing")]
    const DEFAULT_THREAD: &str = "0f0f0f0f-0000-4000-8000-00000000c0de";

    fn manifest() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The Rust fake Codex, `leadhand-fake-codex`, from the profile folder a `--features
    /// testing` build puts it in (`fake_bin`). A `--lib` run does not rebuild it: after editing
    /// `tests/fakes/codex.rs`, run the full `cargo test --features testing` once, or these
    /// tests replay the stale fake.
    #[cfg(feature = "testing")]
    fn fake_codex() -> PathBuf {
        fake_bin("leadhand-fake-codex")
    }

    /// A run with one worker folder, `d1`, whose workspace is a Git repository; and a `PATH`
    /// for the runner children that holds `git` and never a real `codex`. Each world's run ID
    /// is its own (the pid and a counter), so no two tests' dispatch tags match: a runner's
    /// tag sweep stops every process carrying its tag.
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
            let git = crate::pytext::which("git").expect("git is installed");
            std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
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

        /// The runner children's environment: the fake Codex with `scenario` (a file of
        /// `tests/fixtures/scenarios/`), and nothing of this process's.
        fn env(&self, scenario: &str) -> Vec<(OsString, OsString)> {
            self.env_with(
                &manifest()
                    .join("tests/fixtures/scenarios")
                    .join(format!("{scenario}.jsonl")),
            )
        }

        /// `env`, with the frozen copy of `scenario` the reference runner ran
        /// (`tests/fixtures/reference/dispatch/shell/scenarios/`).
        fn frozen_env(&self, scenario: &str) -> Vec<(OsString, OsString)> {
            self.env_with(&reference_fixture(&format!(
                "dispatch/shell/scenarios/{scenario}.jsonl"
            )))
        }

        /// The runner children's environment, the fake Codex replaying `scenario`. Only a
        /// `testing` build takes `LEADHAND_TEST_CODEX_BIN` (the Rust fake); any other runs
        /// `codex` from `PATH`, where there is none.
        fn env_with(&self, scenario: &Path) -> Vec<(OsString, OsString)> {
            let mut path = self.root.join("bin").into_os_string();
            path.push(":/usr/bin:/bin");
            let mut vars: Vec<(&str, OsString)> = vec![
                ("PATH", path),
                ("HOME", self.root.join("home").into()),
                ("TMPDIR", std::env::temp_dir().into()),
                ("LEADHAND_TESTING", "1".into()),
            ];
            #[cfg(feature = "testing")]
            vars.push(("LEADHAND_TEST_CODEX_BIN", fake_codex().into()));
            vars.extend([
                ("FAKE_CODEX_SCENARIO", scenario.into()),
                ("FAKE_CODEX_LOG", self.log().into()),
                ("CODEX_HOME", self.root.join("codex home").into()),
                ("GIT_CONFIG_NOSYSTEM", "1".into()),
                ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ]);
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

    // CATALOG: test_dispatch::FixRoundTests::test_launch_rechecks_cancel_immediately_before_starting
    #[test]
    fn launch_rechecks_cancel_immediately_before_starting() {
        let w = World::new();
        let flag = w.root.join("started");
        let mut record = RunnerRecord {
            runner: own_record(),
            codex: None,
            generation: 1,
            tag: format!("{}:d1:1", w.run_id),
            started_wall: wall_now(),
            started_at: utc_now(),
            launching: None,
        };
        // A program that leaves a mark if it ever starts.
        let prepared = Prepared {
            argv: Ok(["/bin/sh", "-c", "echo ran > \"$0\""]
                .into_iter()
                .map(OsString::from)
                .chain([flag.clone().into_os_string()])
                .collect()),
            env: std::env::vars_os().collect(),
            prompt: Vec::new(),
        };
        let launched = launch(&w.wd, &mut record, &prepared, &w.ws, &|| true).unwrap();
        assert!(matches!(launched, Launch::Cancelled));
        assert!(!flag.exists(), "the program started");
        assert!(!w.wd.join(STREAM).exists());
        let saved: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&fs::read(w.wd.join(RUNNER)).unwrap()).unwrap();
        assert_eq!(saved["codex"], serde_json::Value::Null);
        let launching = saved["launching"].as_object().expect("the launch intent");
        assert_eq!(launching["aborted"], serde_json::Value::Bool(true));
        assert!(launching["at"].is_string());
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

    /// What the Python reference runner did on the same scenario, in a world of its own
    /// (`dispatch/shell/<name>.json`): its exit code and what the test compares, steadied as
    /// `steady`, `steady_attempts` and `steady_charges` steady them, its root written `{root}`.
    struct Reference {
        recorded: serde_json::Value,
        root: String,
    }

    /// The reference's run of scenario `name`, its root read as `w`'s, once it is checked to
    /// have run on what the Rust runner ran on in `w` with `env`: the same request, the same
    /// environment, and the same files the test wrote for the case (a scenario, a probe).
    #[track_caller]
    fn reference(name: &str, w: &World, env: &[(OsString, OsString)]) -> Reference {
        let recorded = reference_json(&format!("dispatch/shell/{name}.json"));
        let root = w.root.display().to_string();
        let theirs = Reference { recorded, root };
        let req = crate::dispatch::read_request(&w.wd).unwrap().unwrap();
        let recorded_req: DispatchRequest = theirs.parsed("request");
        assert_eq!(req, recorded_req, "{name}: recorded on another request");
        let mut recorded_env: Vec<(String, String)> = theirs.value("env");
        // Only a `testing` build gives the runner a fake Codex (`env_with`).
        if cfg!(not(feature = "testing")) {
            recorded_env.retain(|(k, _)| k != "LEADHAND_TEST_CODEX_BIN");
        }
        assert_eq!(
            shown_env(env, w),
            recorded_env,
            "{name}: recorded on another environment"
        );
        for (key, file) in [("crafted", "crafted.jsonl"), ("probe_codex", "probe codex")] {
            let path = w.root.join(file);
            assert_eq!(
                path.exists(),
                !theirs.recorded[key].is_null(),
                "{name}: {file}"
            );
            if path.exists() {
                let text = fs::read_to_string(&path).unwrap();
                assert_eq!(text, theirs.text(key), "{name}: recorded on another {file}");
            }
        }
        theirs
    }

    /// `env` as the shell fixtures give a runner's environment: ordered by name, what varies
    /// by host written `{tmpdir}` (TMPDIR), `{fd}` (the probe's descriptor), `{fake_codex}`
    /// (the fake Codex), `{frozen}` (the frozen scenarios' folder) and `{root}`.
    fn shown_env(env: &[(OsString, OsString)], w: &World) -> Vec<(String, String)> {
        let frozen = reference_fixture("dispatch/shell/scenarios");
        let frozen = frozen.display().to_string();
        let root = w.root.display().to_string();
        let mut shown: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| {
                let k = k.to_str().unwrap().to_string();
                let v = match k.as_str() {
                    "TMPDIR" => "{tmpdir}".to_string(),
                    "PROBE_FD" => "{fd}".to_string(),
                    #[cfg(feature = "testing")]
                    "LEADHAND_TEST_CODEX_BIN" if Path::new(v) == fake_codex() => {
                        "{fake_codex}".to_string()
                    }
                    _ => v.to_str().unwrap().to_string(),
                };
                (k, v.replace(&frozen, "{frozen}").replace(&root, "{root}"))
            })
            .collect();
        shown.sort();
        shown
    }

    impl Reference {
        fn exit(&self) -> i64 {
            self.recorded["exit"].as_i64().expect("the exit code")
        }

        /// A recorded text, the root filled in.
        #[track_caller]
        fn text(&self, key: &str) -> String {
            let text = self.recorded[key].as_str();
            text.unwrap_or_else(|| panic!("no text {key:?}"))
                .replace("{root}", &self.root)
        }

        #[track_caller]
        fn value<T: serde::de::DeserializeOwned>(&self, key: &str) -> T {
            serde_json::from_value(self.recorded[key].clone()).unwrap()
        }

        #[track_caller]
        fn parsed<T: serde::de::DeserializeOwned>(&self, key: &str) -> T {
            serde_json::from_str(&self.text(key)).unwrap()
        }

        /// The recorded lines of `key`, each read as a `T`.
        fn lines<T: serde::de::DeserializeOwned>(&self, key: &str) -> Vec<T> {
            let lines: Vec<String> = self.value(key);
            lines
                .iter()
                .map(|l| serde_json::from_str(&l.replace("{root}", &self.root)).unwrap())
                .collect()
        }

        /// `result.json`, steadied; `None` when there was none.
        fn result(&self) -> Option<DispatchResult> {
            if self.recorded["result"].is_null() {
                return None;
            }
            Some(self.parsed("result"))
        }

        fn attempts(&self) -> Vec<AttemptRecord> {
            self.lines("attempts")
        }

        fn charges(&self) -> Vec<Charge> {
            self.lines("charges")
        }
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
            pgid: Some(0),
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

    /// A world with the test's request: the Rust runner runs in it (the Python reference ran
    /// in a world of its own, the generator's, on the same request).
    fn world_with(change: impl Fn(&World, &mut DispatchRequest)) -> World {
        let w = World::new();
        let mut req = w.request();
        change(&w, &mut req);
        w.write_request(&req);
        w
    }

    /// What both runners left the same way: the result, the attempts and the charges.
    fn assert_same_records(ours: &World, theirs: &Reference) {
        assert_eq!(ours.result().map(steady), theirs.result());
        assert_eq!(steady_attempts(ours), theirs.attempts());
        assert_eq!(steady_charges(ours.charges()), theirs.charges());
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
        let ours = world_with(|_, _| {});
        let env = ours.frozen_env("ready");
        let out = rust_runner(&ours.wd, &env);
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

        // The Python reference runner, on the same request, left the same records.
        let theirs = reference("ready", &ours, &env);
        assert_eq!(theirs.exit(), 0);
        assert_same_records(&ours, &theirs);
        assert_eq!(
            fs::read(ours.wd.join(STREAM)).unwrap(),
            theirs.text("stream").into_bytes()
        );
        assert_eq!(untracked(&ours), theirs.parsed("untracked"));
        assert!(untracked(&ours).is_some());
    }

    #[cfg(feature = "testing")]
    #[test]
    fn step_limit_scenario_stops_codex_with_sigint() {
        let ours = world_with(|_, req| req.steps = 3);
        let started = Instant::now();
        let env = ours.frozen_env("many-steps");
        let out = rust_runner(&ours.wd, &env);
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
        // The reference's exit code and its result's status, exit reason and Codex exit.
        let theirs = reference("step-limit", &ours, &env);
        assert_eq!(theirs.exit(), 0);
        let theirs_r: (Status, ExitReason, Option<i32>) = theirs.value("stop");
        assert_eq!(theirs_r, (r.status, r.exit_reason, r.codex_exit));
    }

    #[cfg(feature = "testing")]
    #[test]
    fn crafted_report_head_kills_the_runner_before_charging() {
        // S2-R32: a head the Python's `parse_report_head` raises KeyError on escapes
        // `_classify` before anything is charged or recorded; the dispatch is left to
        // dead-runner reconciliation.
        let ours = world_with(|_, _| {});
        let scenario = ours.root.join("crafted.jsonl");
        fs::write(
            &scenario,
            "{\"type\": \"thread.started\", \"thread_id\": \"{thread}\"}\n\
             {\"action\": \"last_message\", \"text\": \"re\\u017fult: pass\\n\"}\n",
        )
        .unwrap();
        let with_scenario = |w: &World| -> Vec<(OsString, OsString)> {
            w.frozen_env("ready")
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
        let env = with_scenario(&ours);
        let out = rust_runner(&ours.wd, &env);
        assert_eq!(out.status.code(), Some(1), "{}", shown(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("KeyError: 'Re\u{17f}ult'"),
            "{}",
            shown(&out)
        );
        assert!(!ours.wd.join(RESULT).exists());
        assert_eq!(ours.attempts(), []);
        assert_eq!(ours.charges(), []);
        assert!(
            ours.wd.join(STOPPING).exists(),
            "it stopped before finalizing"
        );
        // The reference, on the same scenario: the same.
        let theirs = reference("crafted-head", &ours, &env);
        assert_eq!(theirs.exit(), 1);
        assert!(!theirs.value::<bool>("result_exists"));
        assert_eq!(theirs.attempts(), []);
        assert_eq!(theirs.charges(), []);
        assert!(
            theirs.value::<bool>("stopping_exists"),
            "it stopped before finalizing"
        );
    }

    /// The text of `codex.stderr`.
    fn stderr_text(w: &World) -> String {
        String::from_utf8(fs::read(w.wd.join(STDERR)).unwrap()).unwrap()
    }

    /// `<first>-<last>`: the positions, in Python's characters (a surrogate escape for each
    /// byte that is not UTF-8), of the first run of such bytes in `text`.
    fn surrogates_in(text: &OsStr) -> String {
        let (mut at, mut first, mut last) = (0, None, 0);
        for chunk in text.as_bytes().utf8_chunks() {
            if first.is_some() && !chunk.valid().is_empty() {
                break;
            }
            at += chunk.valid().chars().count();
            if !chunk.invalid().is_empty() {
                first.get_or_insert(at);
                at += chunk.invalid().len();
                last = at - 1;
            }
        }
        format!("{}-{last}", first.expect("a byte that is not UTF-8"))
    }

    #[test]
    fn prompt_that_is_not_utf8_is_blocked_as_python_blocks_it() {
        // S2-R33: Python sends `worker_prompt(...).encode("utf-8")`, strictly; a brief path
        // with an undecodable byte makes it raise, and the worker ends BLOCKED, never started.
        let ours = world_with(|w, req| {
            let mut name = OsString::from("bri");
            name.push(OsStr::from_bytes(b"\xff\xfe"));
            name.push("ef.md");
            req.brief = w.ws.join(".leadhand/1/d1").join(name);
        });
        let env = ours.frozen_env("ready");
        let out = rust_runner(&ours.wd, &env);
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        let theirs = reference("prompt-not-utf8", &ours, &env);
        assert_eq!(theirs.exit(), 0);
        let text = stderr_text(&ours);
        assert!(
            text.starts_with(
                "leadhand: cannot prepare the Codex worker: 'utf-8' codec can't encode \
                 characters in position "
            ) && text.ends_with(": surrogates not allowed\n"),
            "{text}"
        );
        // The reference's text, its encoder's position (which moves with the root and the
        // agent file) written `{surrogates}`: here, where the prompt holds the escapes.
        let req = crate::dispatch::read_request(&ours.wd).unwrap().unwrap();
        let prompt = crate::briefs::worker_prompt(&req.role, &req.brief, &req.report).unwrap();
        let theirs_text = theirs
            .text("stderr")
            .replace("{surrogates}", &surrogates_in(&prompt));
        assert_eq!(text, theirs_text);
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
        let ours = world_with(|_, req| {
            req.model = serde_json::from_str(r#""gpt\ud800""#).unwrap();
        });
        let env = ours.frozen_env("ready");
        let out = rust_runner(&ours.wd, &env);
        assert_eq!(out.status.code(), Some(1), "{}", shown(&out));
        let theirs = reference("unencodable-model", &ours, &env);
        assert_eq!(theirs.exit(), 1);
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
        let runner: Launch =
            serde_json::from_str(&fs::read_to_string(ours.wd.join(RUNNER)).unwrap()).unwrap();
        assert_eq!((runner.codex, runner.launching.aborted), (None, None));
        assert_eq!(fs::read(ours.wd.join(STREAM)).unwrap(), b"");
        assert_eq!(fs::read(ours.wd.join(STDERR)).unwrap(), b"");
        assert!(!ours.wd.join(RESULT).exists());
        assert!(!ours.wd.join(STOPPING).exists());
        assert_eq!(ours.attempts(), []);
        assert!(ours.records().is_empty());
        // The reference: its `runner.json`'s Codex and launch intent, and the rest.
        let codex: Option<StoppedProc> = theirs.value("codex");
        let aborted: Option<bool> = theirs.value("aborted");
        assert_eq!((codex, aborted), (None, None));
        assert_eq!(theirs.text("stream").as_bytes(), b"");
        assert_eq!(theirs.text("stderr").as_bytes(), b"");
        assert!(!theirs.value::<bool>("result_exists"));
        assert!(!theirs.value::<bool>("stopping_exists"));
        assert_eq!(theirs.attempts(), []);
        assert!(
            theirs.value::<usize>("records") == 0,
            "the fake's log lines"
        );
    }

    #[test]
    fn cancel_before_launch_finalizes_cancelled() {
        let ours = world_with(|_, _| {});
        fs::write(
            ours.wd.join("cancel"),
            "{\"requested_at\": \"earlier\", \"token\": \"tok\"}",
        )
        .unwrap();
        let env = ours.frozen_env("ready");
        let out = rust_runner(&ours.wd, &env);
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        let theirs = reference("cancel-before-launch", &ours, &env);
        assert_eq!(theirs.exit(), 0);
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

        let ours = world_with(|_, _| {});
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
        let out_file = ours.root.join("probe.out");
        let env: Vec<(OsString, OsString)> = ours
            .frozen_env("ready")
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
        let out = rust_runner_with(&ours.wd, &env, Some(fd.as_fd()));
        assert_eq!(out.status.code(), Some(0), "{}", shown(&out));
        assert_eq!(
            fs::read_to_string(&out_file).unwrap(),
            "fd closed\nsurvived SIGHUP\n",
            "the Rust runner"
        );
        // The Python reference runner, given the same probe and a descriptor of its own the
        // same way: what the probe reported.
        let theirs = reference("probe", &ours, &env);
        assert_eq!(theirs.exit(), 0);
        assert_eq!(
            theirs.text("probe"),
            "fd closed\nsurvived SIGHUP\n",
            "the Python reference runner"
        );
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
