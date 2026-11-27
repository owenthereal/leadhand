//! Who owns a dispatch (`dispatch._owner_lock`, `_runner_locked`, `_live`, `_starting_runner`,
//! `_runner_alive`, `runner_alive`, `_runner_target`, `_beat`, `_owner_state`, `runner_state`,
//! `_supersede`, `_spawn_runner`).
//!
//! Every ownership decision (look at the folder, then cancel, supersede or spawn) and every
//! cancel runs under the worker folder's `owner.lock`, held only for that step, never while
//! waiting for a runner to finish. A runner holds `runner.lock` for its whole life: a held lock
//! means a live runner, whatever its recorded pid says. A runner is judged gone only from its
//! lock and the start times recorded for it; a process that cannot be looked up counts as gone
//! there, as in the Python, but never while it holds `runner.lock`.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use rustix::fs::FlockOperation;
use serde::Serialize;
use serde_json::value::RawValue;

use super::caller::py_name;
use super::runner::POLL;
use super::tracker::Tracker;
use super::{
    CANCEL, HEARTBEAT, LAST, PROCS, RAW, RESULT, RUNNER, RUNNER_LOCK, STDERR, STOPPING, STREAM,
    SUPERSEDED, StoppedProc, field, load_result, open_lock_file, proc_record, proc_record_in,
    read_json_object, wall_now,
};
use crate::error::{Error, Result};
use crate::procs::ProcId;
use crate::store::write_json_atomic;
use crate::sys::detach_session;

/// A live runner whose heartbeat is older than this many seconds is looked at again before
/// it counts as hung.
pub const HEARTBEAT_STALE: f64 = 60.0;
/// How long a stale heartbeat must stay unchanged before the runner counts as hung.
pub const HUNG_RECHECK: Duration = Duration::from_secs(5);
/// A spawned runner that has not written `runner.json` yet counts as starting this long.
pub const SPAWN_GRACE: f64 = 60.0;
/// How often a held `owner.lock` is tried again.
const OWNER_LOCK_RETRY: Duration = Duration::from_millis(50);

/// The pid and start time of the runner `dispatch` spawned, and when.
pub const SPAWN: &str = "spawn.json";
/// Held around each ownership decision and each cancel.
pub const OWNER_LOCK: &str = "owner.lock";
/// The runner's own output.
pub const RUNNER_OUT: &str = "runner.out";
pub const RUNNER_ERR: &str = "runner.err";
/// The runner's commit intent (R39).
pub const COMMIT_INTENT: &str = "commit.json";
/// The files of one generation, moved to `superseded/<n>/` when it is replaced.
pub const GENERATION_FILES: [&str; 14] = [
    SPAWN,
    RUNNER,
    HEARTBEAT,
    PROCS,
    STREAM,
    STDERR,
    LAST,
    RAW,
    CANCEL,
    STOPPING,
    RESULT,
    COMMIT_INTENT,
    RUNNER_OUT,
    RUNNER_ERR,
];

/// `spawn.json`, in the Python's key order.
#[derive(Serialize)]
struct SpawnRecord {
    pid: i32,
    start: Option<String>,
    at: f64,
}

// --------------------------------------------------------------------------
// The locks

/// `owner.lock`, held while this lives.
pub struct OwnerLock {
    _file: File,
}

/// `_owner_lock`: takes the worker folder's `owner.lock`, waiting for it; with a deadline, an
/// error once it passes.
pub fn owner_lock(wd: &Path, deadline: Option<Instant>) -> Result<OwnerLock> {
    let path = wd.join(OWNER_LOCK);
    let file = open_lock_file(&path)?;
    loop {
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(OwnerLock { _file: file }),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(Error::msg(format!(
                        "another leadhand process is handling {}",
                        py_name(wd).to_string_lossy()
                    )));
                }
                thread::sleep(OWNER_LOCK_RETRY);
            }
            Err(e) => return Err(Error::io(&path, e.into())),
        }
    }
}

/// `_runner_locked`: whether a runner holds `runner.lock` (a probe holds it for an instant).
fn runner_locked(wd: &Path) -> Result<bool> {
    let path = wd.join(RUNNER_LOCK);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::io(&path, e)),
    };
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            rustix::fs::flock(&file, FlockOperation::Unlock)
                .map_err(|e| Error::io(&path, e.into()))?;
            Ok(false)
        }
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Ok(true),
        Err(e) => Err(Error::io(&path, e.into())),
    }
}

// --------------------------------------------------------------------------
// Who is alive

/// `_live`: the record's process (`proc_record`), while its pid still has the recorded start
/// time.
fn live(rec: Option<StoppedProc>) -> Option<ProcId> {
    rec.filter(StoppedProc::alive).and_then(|rec| rec.id())
}

/// `_starting_runner`: a spawned runner that has not written `runner.json` yet, alive, and
/// when it was spawned (`spawn.json`'s `at`, 0 when it names none).
fn starting_runner(wd: &Path) -> Result<Option<(ProcId, f64)>> {
    if wd.join(RUNNER).exists() {
        return Ok(None);
    }
    let Some(spawn) = read_json_object(&wd.join(SPAWN))? else {
        return Ok(None);
    };
    let Some(id) = live(proc_record_in(&spawn)) else {
        return Ok(None);
    };
    let at = field::<f64>(&spawn, "at").unwrap_or(0.0);
    Ok(Some((id, at)))
}

/// `_runner_alive`: a runner holds `runner.lock`, or was just spawned.
pub(crate) fn runner_alive_in(wd: &Path) -> Result<bool> {
    Ok(runner_locked(wd)? || starting_runner(wd)?.is_some())
}

/// `runner_alive`: whether a runner owns the dispatch now (it holds `runner.lock`, or was just
/// spawned).
pub fn runner_alive(worker_dir: &Path) -> Result<bool> {
    Ok(worker_dir.is_dir() && runner_alive_in(worker_dir)?)
}

/// `_runner_target`: the runner as a signal target, only while its pid and start time match:
/// `runner.json`'s record when there is one, else `spawn.json`'s.
pub(crate) fn runner_target(wd: &Path) -> Result<Option<ProcId>> {
    if let Some(runner) = read_json_object(&wd.join(RUNNER))? {
        let rec = field::<Box<RawValue>>(&runner, "runner").and_then(|raw| proc_record(&raw));
        return Ok(live(rec));
    }
    Ok(live(
        read_json_object(&wd.join(SPAWN))?.and_then(|spawn| proc_record_in(&spawn)),
    ))
}

fn mtime(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    })
}

/// `_beat`: the heartbeat's time, else `runner.json`'s, else `spawn.json`'s, else 0.
pub(crate) fn beat(wd: &Path) -> f64 {
    [HEARTBEAT, RUNNER, SPAWN]
        .iter()
        .filter_map(|name| mtime(&wd.join(name)))
        .find(|t| *t != 0.0)
        .unwrap_or(0.0)
}

/// Who owns a dispatch (`_owner_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerState {
    Running,
    /// A live runner whose heartbeat is old: `dispatch` looks again before calling it hung.
    Stale,
    /// The runner is gone and a recorded process is alive.
    Orphaned,
    /// The runner and every recorded process are gone.
    Dead,
    /// No runner was ever recorded.
    None,
}

/// `_owner_state`.
pub fn owner_state(wd: &Path) -> Result<OwnerState> {
    let now = wall_now();
    if runner_locked(wd)? {
        return Ok(if now - beat(wd) > HEARTBEAT_STALE {
            OwnerState::Stale
        } else {
            OwnerState::Running
        });
    }
    if let Some((_, at)) = starting_runner(wd)? {
        return Ok(if now - at < SPAWN_GRACE {
            OwnerState::Running
        } else {
            OwnerState::Stale
        });
    }
    if !wd.join(RUNNER).exists() {
        return Ok(OwnerState::None);
    }
    let mut tracker = Tracker::load(wd)?;
    tracker.find_tagged();
    Ok(if tracker.alive().is_empty() {
        OwnerState::Dead
    } else {
        OwnerState::Orphaned
    })
}

/// A dispatch's runner, for recovery after a relaunch (`runner_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerState {
    /// `result.json` is there.
    Finished,
    Running,
    /// Alive, with a heartbeat that stayed stale.
    Hung,
    Orphaned,
    Dead,
    /// No runner was ever recorded.
    None,
}

impl RunnerState {
    /// The Python's name for the state, as `reconciled{runner}` records it.
    pub fn as_str(self) -> &'static str {
        match self {
            RunnerState::Finished => "finished",
            RunnerState::Running => "running",
            RunnerState::Hung => "hung",
            RunnerState::Orphaned => "orphaned",
            RunnerState::Dead => "dead",
            RunnerState::None => "none",
        }
    }
}

/// `stop_strays`: stops the recorded Codex processes of a dispatch whose runner is gone (a
/// SIGKILLed runner leaves Codex's tool and helper processes under init, P14). What is still
/// alive after `timeout`, empty when everything is stopped. A live runner is left alone: it
/// stops its own.
pub fn stop_strays(worker_dir: &Path, timeout: Duration) -> Result<Vec<StoppedProc>> {
    let wd = worker_dir;
    if !wd.join(RUNNER).exists() || runner_alive_in(wd)? {
        return Ok(Vec::new());
    }
    let mut tracker = Tracker::load(wd)?;
    tracker.find_tagged();
    if tracker.alive().is_empty() {
        return Ok(Vec::new());
    }
    let deadline = Instant::now() + timeout;
    Ok(super::tracker::stop(&mut tracker, Some(deadline)))
}

/// `runner_state`: a live runner whose heartbeat is stale is looked at again for `recheck`
/// (default `HUNG_RECHECK`) before it counts as hung: after a laptop sleep the runner
/// refreshes it as soon as it runs again.
pub fn runner_state(worker_dir: &Path, recheck: Option<Duration>) -> Result<RunnerState> {
    let wd = worker_dir;
    if load_result(wd)?.is_some() {
        return Ok(RunnerState::Finished);
    }
    if !wd.is_dir() {
        return Ok(RunnerState::None);
    }
    let of = |state| match state {
        OwnerState::Running => RunnerState::Running,
        OwnerState::Stale => RunnerState::Hung,
        OwnerState::Orphaned => RunnerState::Orphaned,
        OwnerState::Dead => RunnerState::Dead,
        OwnerState::None => RunnerState::None,
    };
    let mut state = of(owner_state(wd)?);
    if state == RunnerState::Hung {
        let (seen, until) = (beat(wd), Instant::now() + recheck.unwrap_or(HUNG_RECHECK));
        while Instant::now() < until {
            if load_result(wd)?.is_some() {
                return Ok(RunnerState::Finished);
            }
            if beat(wd) != seen {
                return Ok(RunnerState::Running);
            }
            thread::sleep(POLL);
        }
        state = of(owner_state(wd)?);
    }
    if load_result(wd)?.is_some() {
        return Ok(RunnerState::Finished);
    }
    Ok(state)
}

// --------------------------------------------------------------------------
// Replacing a runner, and spawning one

/// `_supersede`: moves the generation's files to `superseded/<n>/`, `n` its generation (one
/// more than the generations already there when it names none), the next free number if that
/// one is taken.
pub fn supersede(wd: &Path) -> Result<()> {
    let runner = read_json_object(&wd.join(RUNNER))?.unwrap_or_default();
    let superseded = wd.join(SUPERSEDED);
    if let Err(e) = fs::create_dir(&superseded)
        && !(e.kind() == ErrorKind::AlreadyExists && superseded.is_dir())
    {
        return Err(Error::io(&superseded, e));
    }
    let named = field::<i64>(&runner, "generation").filter(|n| *n != 0);
    let mut n = match named {
        Some(n) => n,
        None => {
            let entries = fs::read_dir(&superseded).map_err(|e| Error::io(&superseded, e))?;
            1 + i64::try_from(entries.count()).unwrap_or(i64::MAX - 1)
        }
    };
    let mut dest = superseded.join(n.to_string());
    while dest.exists() {
        n += 1;
        dest = superseded.join(n.to_string());
    }
    fs::create_dir(&dest).map_err(|e| Error::io(&dest, e))?;
    for name in GENERATION_FILES {
        match fs::rename(wd.join(name), dest.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&wd.join(name), e)),
        }
    }
    Ok(())
}

/// `_spawn_runner`: `dispatch-runner <wd>` (this CLI, as `testing::self_path` names it), in a
/// new session, in the worker folder, with stdin from `/dev/null` and its output appended to
/// `runner.out` and `runner.err`; and `spawn.json`. Nothing else this process holds reaches it
/// (`Popen(close_fds=True)`): the spawn itself first marks every inherited descriptor
/// close-on-exec, whoever calls it.
pub fn spawn_runner(wd: &Path) -> Result<Child> {
    let program = crate::testing::self_path();
    let mut cmd = Command::new(&program);
    cmd.arg("dispatch-runner").arg(wd);
    spawn_detached(wd, cmd)
}

/// `cmd` started as the runner is, and recorded in `spawn.json`.
fn spawn_detached(wd: &Path, mut cmd: Command) -> Result<Child> {
    crate::sys::keep_inherited_fds_from_children()
        .map_err(|e| Error::msg(format!("cannot list this process's descriptors: {e}")))?;
    let append = |name: &str| -> Result<File> {
        let path = wd.join(name);
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))
    };
    let (out, err) = (append(RUNNER_OUT)?, append(RUNNER_ERR)?);
    cmd.current_dir(wd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    detach_session(&mut cmd);
    let program = PathBuf::from(cmd.get_program());
    let child = cmd.spawn().map_err(|e| Error::io(&program, e))?;
    let pid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    let record = SpawnRecord {
        pid,
        start: crate::sys::proc_info(pid).map(|info| info.start),
        at: wall_now(),
    };
    write_json_atomic(&wd.join(SPAWN), &record)?;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rustix::process::Signal;

    use super::*;
    use crate::procs;

    /// A process this test started, in its own session: killed (its whole group) and reaped
    /// when the test ends, pass or fail.
    struct Kid(Child);

    impl Drop for Kid {
        fn drop(&mut self) {
            if let Ok(None) = self.0.try_wait()
                && let Some(id) = procs::identify(self.pid())
            {
                procs::signal_group_if_same(&id, self.pid(), Signal::KILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Kid {
        fn pid(&self) -> i32 {
            i32::try_from(self.0.id()).unwrap()
        }

        /// `{pid, pgid, start}`, as the process table shows it.
        fn record(&self) -> String {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(info) = crate::sys::proc_info(self.pid()) {
                    return format!(
                        "{{\"pid\": {}, \"pgid\": {}, \"start\": \"{}\"}}",
                        info.pid, info.pgid, info.start
                    );
                }
                assert!(Instant::now() < deadline, "no process record");
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn kill(mut self) {
            if let Some(id) = procs::identify(self.pid()) {
                procs::signal_group_if_same(&id, self.pid(), Signal::KILL);
            }
            let status = self.0.wait().unwrap();
            assert!(status.signal().is_some() || status.code().is_some());
        }
    }

    /// `cmd` started in a session of its own, its stdout a pipe.
    fn spawn_kid(mut cmd: Command) -> Kid {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        detach_session(&mut cmd);
        Kid(cmd.spawn().unwrap())
    }

    /// `sleep 120`, in a session of its own.
    fn sleeper() -> Kid {
        let mut cmd = Command::new("sleep");
        cmd.arg("120");
        spawn_kid(cmd)
    }

    /// A stray carrying the dispatch tag `tag`: this test binary as `procs::tests::
    /// waiting_child` (the process table shows its environment on macOS too, unlike a platform
    /// binary's such as `/bin/sleep`), in a session of its own, waiting for its stdin, held open
    /// here, to close. Returned once it has printed: it runs then, past its `exec`, so a sweep
    /// sees its environment.
    fn tagged_kid(tag: &str) -> Kid {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["procs::tests::waiting_child", "--exact", "--test-threads=1"])
            .env(procs::tests::WAITING, "1")
            .env(super::super::TAG_VAR, tag)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        detach_session(&mut cmd);
        let mut kid = Kid(cmd.spawn().unwrap());
        let mut line = String::new();
        let read = BufReader::new(kid.0.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(read > 0, "the tagged child ended before it ran");
        kid
    }

    /// The lock file `lock_holder_child` takes; unset in an ordinary run.
    const LOCK_HOLDER: &str = "LEADHAND_OWN_TEST_LOCK_HOLDER";

    /// Not a test of its own: the process `holding_runner_lock` starts (this test binary,
    /// running this test alone). It opens the file it was given (created if missing), takes an
    /// exclusive `flock` on it, says so and sleeps. Does nothing in an ordinary run.
    #[test]
    fn lock_holder_child() {
        let Some(lock) = std::env::var_os(LOCK_HOLDER) else {
            return;
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock)
            .unwrap();
        rustix::fs::flock(&file, FlockOperation::LockExclusive).unwrap();
        // On a line of its own: libtest has printed `test <name> ... ` without a newline.
        println!("\nlocked");
        thread::sleep(Duration::from_secs(120));
    }

    /// A process holding `runner.lock`, once it holds it: this test binary again, in a
    /// session of its own.
    fn holding_runner_lock(wd: &Path) -> Kid {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "dispatch::own::tests::lock_holder_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(LOCK_HOLDER, wd.join(RUNNER_LOCK));
        let mut kid = spawn_kid(cmd);
        let mut stdout = BufReader::new(kid.0.stdout.as_mut().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            if stdout.read_line(&mut line).unwrap() == 0 || line.trim() == "locked" {
                break;
            }
        }
        assert_eq!(line.trim(), "locked");
        kid
    }

    fn worker_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("lh runner ")
            .tempdir()
            .unwrap();
        let wd = tmp.path().join("worker dir");
        fs::create_dir(&wd).unwrap();
        (tmp, wd)
    }

    fn age(path: &Path, seconds: u64) {
        let old = std::time::SystemTime::now() - Duration::from_secs(seconds);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    // CATALOG: test_recovery::RunnerHelpers::test_runner_state
    #[test]
    fn runner_state() {
        let (tmp, wd) = worker_dir();
        let short = Some(Duration::from_millis(200));
        assert_eq!(
            super::runner_state(&tmp.path().join("missing"), None).unwrap(),
            RunnerState::None
        );
        assert_eq!(super::runner_state(&wd, None).unwrap(), RunnerState::None);
        let runner = holding_runner_lock(&wd);
        let record = format!("{{\"runner\": {}, \"codex\": null}}", runner.record());
        fs::write(wd.join(RUNNER), record).unwrap();
        File::create(wd.join(HEARTBEAT)).unwrap();
        assert_eq!(
            super::runner_state(&wd, short).unwrap(),
            RunnerState::Running
        );
        age(&wd.join(HEARTBEAT), 600);
        assert_eq!(super::runner_state(&wd, short).unwrap(), RunnerState::Hung);
        runner.kill();
        let codex = sleeper();
        let record = format!(
            "{{\"runner\": {}, \"codex\": {}}}",
            codex.record(),
            codex.record()
        );
        fs::write(wd.join(RUNNER), record).unwrap();
        assert_eq!(
            super::runner_state(&wd, None).unwrap(),
            RunnerState::Orphaned
        );
        codex.kill();
        assert_eq!(super::runner_state(&wd, None).unwrap(), RunnerState::Dead);
        fs::write(
            wd.join(RESULT),
            "{\"status\": \"READY\", \"exit_reason\": \"completed\"}",
        )
        .unwrap();
        assert_eq!(
            super::runner_state(&wd, None).unwrap(),
            RunnerState::Finished
        );
    }

    // test_recovery::RunnerHelpers::test_stop_strays_stops_what_a_dead_runner_left_and_
    // leaves_a_live_runner_alone (P14), against real processes.
    #[test]
    fn stop_strays_stops_what_a_dead_runner_left_and_leaves_a_live_runner_alone() {
        let (_tmp, wd) = worker_dir();
        let timeout = Duration::from_secs(15);
        assert!(super::stop_strays(&wd, timeout).unwrap().is_empty());
        let mut tool = sleeper();
        let mut codex = sleeper();
        let record = format!(
            "{{\"runner\": {{\"pid\": 1, \"pgid\": 1, \"start\": \"gone\"}}, \"codex\": {}}}",
            codex.record()
        );
        fs::write(wd.join(RUNNER), record).unwrap();
        let procs = format!("{{\"descendants\": [{}]}}", tool.record());
        fs::write(wd.join(PROCS), procs).unwrap();
        let runner = holding_runner_lock(&wd);
        // A live runner stops its own.
        assert!(super::stop_strays(&wd, timeout).unwrap().is_empty());
        assert!(codex.0.try_wait().unwrap().is_none());
        assert!(tool.0.try_wait().unwrap().is_none());
        runner.kill();
        assert!(super::stop_strays(&wd, timeout).unwrap().is_empty());
        for kid in [&mut codex, &mut tool] {
            let deadline = Instant::now() + Duration::from_secs(10);
            while kid.0.try_wait().unwrap().is_none() {
                assert!(Instant::now() < deadline, "a stray outlived stop_strays");
                thread::sleep(Duration::from_millis(20));
            }
        }
        // Nothing left: a no-op.
        assert!(super::stop_strays(&wd, timeout).unwrap().is_empty());
    }

    #[test]
    fn stop_strays_stops_only_what_carries_the_dispatch_tag() {
        // A dead runner's tag sweep stops the process whose environment holds the tag's exact
        // entry, and nothing else: not one with a longer value of the same variable, nor one
        // without the variable.
        static TAGS: AtomicU64 = AtomicU64::new(0);
        let n = TAGS.fetch_add(1, Ordering::Relaxed);
        // A prefix of its own: the tracker's and the shell worlds' tags are `r<pid>-<n>:d1:1`,
        // and two tests sharing a tag would stop each other's processes.
        let tag = format!("own-strays-{}-{n}:d1:1", std::process::id());
        let (_tmp, wd) = worker_dir();
        let mut stray = tagged_kid(&tag);
        let mut longer = tagged_kid(&format!("{tag}0"));
        let mut untagged = sleeper();
        let record = format!(
            "{{\"runner\": {{\"pid\": 1, \"pgid\": 1, \"start\": \"gone\"}}, \"codex\": null, \
             \"tag\": \"{tag}\"}}"
        );
        fs::write(wd.join(RUNNER), record).unwrap();
        assert_eq!(
            super::stop_strays(&wd, Duration::from_secs(15)).unwrap(),
            []
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while stray.0.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "the tagged stray outlived stop_strays"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            longer.0.try_wait().unwrap().is_none(),
            "a process with another tag was stopped"
        );
        assert!(
            untagged.0.try_wait().unwrap().is_none(),
            "an untagged process was stopped"
        );
        longer.kill();
        untagged.kill();
    }

    #[test]
    fn a_runner_holding_its_lock_is_never_gone_whatever_its_record_says() {
        // "Gone" (a record whose start time does not match, or none at all) never makes a
        // runner holding runner.lock dead or orphaned.
        let (_tmp, wd) = worker_dir();
        let runner = holding_runner_lock(&wd);
        for record in [
            "{\"runner\": {\"pid\": 1, \"pgid\": 1, \"start\": \"gone\"}, \"codex\": null}",
            "{\"runner\": {\"pid\": 1, \"pgid\": 1, \"start\": null}, \"codex\": null}",
        ] {
            fs::write(wd.join(RUNNER), record).unwrap();
            File::create(wd.join(HEARTBEAT)).unwrap();
            assert_eq!(owner_state(&wd).unwrap(), OwnerState::Running);
            assert!(runner_alive(&wd).unwrap());
            assert_eq!(runner_target(&wd).unwrap(), None);
            age(&wd.join(HEARTBEAT), 600);
            assert_eq!(owner_state(&wd).unwrap(), OwnerState::Stale);
        }
        runner.kill();
        assert_eq!(owner_state(&wd).unwrap(), OwnerState::Dead);
        assert!(!runner_alive(&wd).unwrap());
    }

    #[test]
    fn a_spawned_runner_counts_as_starting_until_runner_json() {
        let (_tmp, wd) = worker_dir();
        assert_eq!(owner_state(&wd).unwrap(), OwnerState::None);
        let kid = sleeper();
        let record = kid.record();
        let with_at = |at: f64| format!("{}, \"at\": {at}}}", record.trim_end_matches('}'));
        fs::write(wd.join(SPAWN), with_at(wall_now())).unwrap();
        assert_eq!(owner_state(&wd).unwrap(), OwnerState::Running);
        assert_eq!(
            runner_target(&wd).unwrap().map(|id| id.pid),
            Some(kid.pid())
        );
        fs::write(wd.join(SPAWN), with_at(wall_now() - 120.0)).unwrap();
        assert_eq!(owner_state(&wd).unwrap(), OwnerState::Stale);
        // Once runner.json is written, spawn.json no longer speaks for the runner.
        fs::write(wd.join(RUNNER), "{\"runner\": null, \"codex\": null}").unwrap();
        assert_eq!(owner_state(&wd).unwrap(), OwnerState::Dead);
        assert_eq!(runner_target(&wd).unwrap(), None);
        kid.kill();
    }

    #[test]
    fn supersede_moves_the_generation_to_its_number() {
        let (_tmp, wd) = worker_dir();
        for name in [RUNNER, RESULT, HEARTBEAT, COMMIT_INTENT, RUNNER_ERR] {
            fs::write(wd.join(name), "{\"generation\": 2}").unwrap();
        }
        fs::write(wd.join(super::super::REQUEST), "{}").unwrap();
        fs::write(wd.join(super::super::UNTRACKED), "{}").unwrap();
        supersede(&wd).unwrap();
        let two = wd.join(SUPERSEDED).join("2");
        for name in [RUNNER, RESULT, HEARTBEAT, COMMIT_INTENT, RUNNER_ERR] {
            assert!(two.join(name).exists(), "{name}");
            assert!(!wd.join(name).exists(), "{name}");
        }
        // The request and the untracked files at the first launch stay.
        assert!(wd.join(super::super::REQUEST).exists());
        assert!(wd.join(super::super::UNTRACKED).exists());
        // Generation 2 again (or none): the next free number.
        fs::write(wd.join(RUNNER), "{\"generation\": 2}").unwrap();
        supersede(&wd).unwrap();
        assert!(wd.join(SUPERSEDED).join("3").join(RUNNER).exists());
        supersede(&wd).unwrap();
        assert!(wd.join(SUPERSEDED).join("4").is_dir());
    }

    #[test]
    fn owner_lock_waits_and_gives_up_at_its_deadline() {
        let (_tmp, wd) = worker_dir();
        let held = owner_lock(&wd, None).unwrap();
        let started = Instant::now();
        let e = owner_lock(&wd, Some(Instant::now() + Duration::from_millis(200)))
            .err()
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(e.message, "another leadhand process is handling worker dir");
        drop(held);
        // Free again (a test elsewhere forking at that moment can hold a copy of the closed
        // descriptor until its child execs, so not necessarily at the first try).
        assert!(owner_lock(&wd, Some(Instant::now() + Duration::from_secs(5))).is_ok());
    }

    /// The children of the descriptor tests: this test binary run again with a pipe on its
    /// stdout and stderr and another inherited without close-on-exec, as a supervisor's Bash
    /// tool starts `dispatch`. Neither marks any descriptor itself first: the spawn must.
    const SPAWN_WD: &str = "LEADHAND_TEST_SPAWN_WD";

    /// `spawn_detached` of `sleep 30`.
    #[test]
    fn spawn_child() {
        let Some(wd) = std::env::var_os(SPAWN_WD) else {
            return;
        };
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let child = spawn_detached(Path::new(&wd), cmd).unwrap();
        println!("spawned {}", child.id());
        std::process::exit(0);
    }

    /// The library's `dispatch` (no command wrapper), on a run in `SPAWN_WD`, waiting 1.5 s.
    /// The runner it spawns is `LEADHAND_TEST_CLI`, a stand-in that sleeps.
    #[cfg(feature = "testing")]
    #[test]
    fn dispatch_child() {
        let Some(run) = std::env::var_os(SPAWN_WD) else {
            return;
        };
        let run = PathBuf::from(run);
        let req = crate::dispatch::DispatchRequest {
            model: "gpt-6-luna".into(),
            effort: "low".into(),
            role: "implementer".into(),
            brief: run.join("brief.md"),
            report: run.join("report.md"),
            cwd: run.clone(),
            run_dir: run.clone(),
            card: Some(1),
            attempt: 1,
            dispatch_id: "d1".into(),
            steps: 50,
            minutes: 5,
            network: false,
            writable_dirs: vec![],
            resume: None,
            kind: None,
            branch: None,
        };
        let e = crate::dispatch::dispatch(&req, Duration::from_millis(1500)).unwrap_err();
        println!("{}", e.message);
        std::process::exit(0);
    }

    /// Runs the test `child` as the caller, with `env`; returns its stdout once every pipe it
    /// held (stdout, stderr and the inherited one) has reached its end, which fails the test if
    /// whatever it spawned still holds one.
    fn caller_with_pipes(child: &str, env: &[(&str, &OsStr)]) -> String {
        let (reader, writer) = std::io::pipe().unwrap();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([child, "--exact", "--test-threads=1", "--nocapture"])
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        {
            use std::os::fd::AsFd;
            crate::sys::pass_fd_to_child(&mut cmd, writer.as_fd());
        }
        let mut caller = cmd.spawn().unwrap();
        drop(writer);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut pipes: Vec<Box<dyn Read + Send>> = vec![
            Box::new(caller.stdout.take().unwrap()),
            Box::new(caller.stderr.take().unwrap()),
            Box::new(reader),
        ];
        for (i, mut pipe) in pipes.drain(..).enumerate() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send((i, buf));
            });
        }
        let mut out = Vec::new();
        for _ in 0..3 {
            let (i, buf) = rx
                .recv_timeout(Duration::from_secs(20))
                .expect("a pipe stayed open: the spawned process holds it");
            if i == 0 {
                out = buf;
            }
        }
        assert!(caller.wait().unwrap().success());
        String::from_utf8(out).unwrap()
    }

    /// The process `spawn.json` names: still alive, in its own session's group; then killed.
    fn spawned_is_alive_then_killed(wd: &Path) -> ProcId {
        let spawn = read_json_object(&wd.join(SPAWN)).unwrap().unwrap();
        let id = live(proc_record_in(&spawn)).expect("the spawned process is still alive");
        let info = crate::sys::proc_info(id.pid);
        assert!(info.is_none_or(|i| i.pgid == id.pid || i.zombie));
        assert!(crate::procs::signal_if_same(&id, Signal::KILL));
        id
    }

    #[test]
    fn spawned_process_holds_none_of_the_callers_descriptors() {
        let (_tmp, wd) = worker_dir();
        let out = caller_with_pipes(
            "dispatch::own::tests::spawn_child",
            &[(SPAWN_WD, wd.as_os_str())],
        );
        let id = spawned_is_alive_then_killed(&wd);
        assert!(out.contains(&format!("spawned {}", id.pid)), "{out}");
    }

    #[cfg(feature = "testing")]
    #[test]
    fn the_library_dispatch_spawns_its_runner_without_the_callers_descriptors() {
        let (tmp, run) = worker_dir();
        let stand_in = tmp.path().join("runner stand-in");
        crate::test_executable::write_executable(&stand_in, "#!/bin/sh\nexec sleep 30\n");
        let out = caller_with_pipes(
            "dispatch::own::tests::dispatch_child",
            &[
                (SPAWN_WD, run.as_os_str()),
                ("LEADHAND_TESTING", OsStr::new("1")),
                ("LEADHAND_TEST_CLI", stand_in.as_os_str()),
            ],
        );
        assert!(
            out.contains("still running: run the same command again (dispatch d1)"),
            "{out}"
        );
        spawned_is_alive_then_killed(&run.join("workers").join("d1"));
    }

    #[test]
    fn a_live_process_recorded_without_a_group_is_alive_not_dead() {
        // Liveness is the pid and start time alone, as the Python's `alive()` judges it: a
        // record without `pgid` still names a live Codex, so the runner is orphaned (stopped,
        // never replaced alongside it), not dead.
        let (_tmp, wd) = worker_dir();
        let codex = sleeper();
        let tool = sleeper();
        let without_group = |kid: &Kid| {
            let info = crate::sys::proc_info(kid.pid()).unwrap();
            format!("{{\"pid\": {}, \"start\": \"{}\"}}", info.pid, info.start)
        };
        let record = format!("{{\"runner\": null, \"codex\": {}}}", without_group(&codex));
        fs::write(wd.join(RUNNER), record).unwrap();
        let procs = format!("{{\"descendants\": [{}]}}", without_group(&tool));
        fs::write(wd.join(PROCS), procs).unwrap();
        assert_eq!(owner_state(&wd).unwrap(), OwnerState::Orphaned);
        assert_eq!(
            super::runner_state(&wd, None).unwrap(),
            RunnerState::Orphaned
        );
        let mut tracker = Tracker::load(&wd).unwrap();
        let alive: Vec<i32> = tracker.alive().iter().map(|p| p.pid).collect();
        assert_eq!(alive, [codex.pid(), tool.pid()]);
        // Stopped by its own pid (never as a group), and listed without one.
        assert_eq!(
            crate::dispatch::tracker::stop(
                &mut tracker,
                Some(Instant::now() + Duration::from_secs(15))
            ),
            []
        );
        assert!(tracker.stopped().iter().all(|p| p.pgid.is_none()));
        assert_eq!(owner_state(&wd).unwrap(), OwnerState::Dead);
        assert!(!wd.join(SUPERSEDED).exists());
        codex.kill();
        tool.kill();
    }
}
