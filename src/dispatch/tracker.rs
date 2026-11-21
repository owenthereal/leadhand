//! Codex and its recorded processes (`dispatch._Tracker`): recording them, signalling them by
//! identity, and confirming they are gone; and the loop that carries out a state machine's
//! process actions (`carry_out`), shared by the runner's watch and a cancel's stop ladder
//! (`stop`).
//!
//! Each process is recorded as (pid, pgid, start time), and counts as the one recorded only
//! while its pid still has that start time; a zombie counts as gone. With a dispatch tag, a
//! snapshot also sweeps up every process of this user carrying it, so a process that left
//! Codex's tree before a snapshot saw it is stopped too (M4).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::thread;
use std::time::Instant;

use serde::{Serialize, Serializer};
use serde_json::value::RawValue;

use super::request::StoppedProc;
use super::runner::{Action, Event, Ladder, Liveness, Runner, Sig};
use super::{PROCS, RUNNER, TAG_VAR, field, proc_record, read_json_object};
use crate::error::Result;
use crate::procs::{self, ProcId, ProcInfo};
use crate::store::write_json_atomic;

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

/// Codex and its recorded descendants (`_Tracker`).
pub struct Tracker {
    wd: PathBuf,
    codex: Option<StoppedProc>,
    child: Option<Child>,
    exit: Option<ExitStatus>,
    /// Whether `newly_exited` has given Codex's exit already.
    exit_seen: bool,
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
            exit_seen: false,
            tag,
            live: Vec::new(),
            seen: Vec::new(),
            saved: None,
            unreadable: Unreadable::Pids(Vec::new()),
        }
    }

    /// The tracker of a dispatch whose runner is gone (`_Tracker.load`): Codex as `runner.json`
    /// records it, the descendants `procs.json` last recorded (those with a pid and a start
    /// time), and the dispatch tag. A record is judged by its pid and start time alone, as the
    /// Python judges it (`proc_record`); one with no integer pid is no process.
    pub fn load(wd: &Path) -> Result<Tracker> {
        let runner = read_json_object(&wd.join(RUNNER))?.unwrap_or_default();
        let saved = read_json_object(&wd.join(PROCS))?.unwrap_or_default();
        let codex = field::<Box<RawValue>>(&runner, "codex").and_then(|raw| proc_record(&raw));
        let tag = field::<String>(&runner, "tag").filter(|tag| !tag.is_empty());
        let mut tracker = Tracker::new(wd, codex, None, tag);
        let descendants: Vec<Box<RawValue>> = field(&saved, "descendants").unwrap_or_default();
        for raw in &descendants {
            if let Some(rec) = proc_record(raw)
                && rec.pid != 0
                && rec.start.as_deref().is_some_and(|s| !s.is_empty())
            {
                tracker.add(rec);
            }
        }
        Ok(tracker)
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

    /// Codex's exit status the first time it is seen here, `None` before and after.
    pub fn newly_exited(&mut self) -> Option<ExitStatus> {
        let status = self.exited()?;
        (!std::mem::replace(&mut self.exit_seen, true)).then_some(status)
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
        self.codex.as_ref().is_some_and(StoppedProc::alive)
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
        target.id()
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
    pub fn find_tagged(&mut self) {
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
        self.live.retain(StoppedProc::alive);
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
        let any = codex || self.live.iter().any(StoppedProc::alive);
        Liveness { codex, any }
    }

    /// What is still alive: Codex, then the recorded processes.
    pub fn alive(&mut self) -> Vec<StoppedProc> {
        let mut out: Vec<StoppedProc> = Vec::new();
        if self.codex_alive() {
            out.extend(self.codex.clone());
        }
        out.extend(self.live.iter().filter(|r| r.alive()).cloned());
        out
    }

    /// One rung's signal (`_Tracker.signal`): first to each group whose leader is a target
    /// still with its identity, then to each other target, itself, with its identity. Codex
    /// alone for SIGINT; Codex and every recorded process otherwise. A record without a group
    /// is never signalled as a group, only itself.
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
            if let Some(pgid) = target.pgid
                && target.pid == pgid
                && !groups.contains(&pgid)
                && let Some(id) = self.identity(target, *is_codex)
                && procs::signal_group_if_same(&id, pgid, signal)
            {
                groups.push(pgid);
            }
        }
        for (target, is_codex) in &targets {
            if !target.pgid.is_some_and(|pgid| groups.contains(&pgid))
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
        pgid: Some(p.pgid),
        start: Some(p.start.clone()),
    }
}

// --------------------------------------------------------------------------
// Carrying out the actions

/// A state machine whose process actions the tracker carries out: the runner, or a cancel's
/// ladder.
pub(crate) trait Machine {
    fn step(&mut self, ev: Event, now: Instant) -> Vec<Action>;
}

impl Machine for Runner {
    fn step(&mut self, ev: Event, now: Instant) -> Vec<Action> {
        Runner::step(self, ev, now)
    }
}

impl Machine for Ladder {
    fn step(&mut self, ev: Event, now: Instant) -> Vec<Action> {
        Ladder::step(self, &ev, now)
    }
}

/// Carries out `actions` in order, and every action they lead to. The tracker's own here
/// (snapshots, checks and signals): each request is answered at once, under its own token,
/// and Codex's exit is fed first when that is where it is first seen. Every other action goes
/// to `other`, whose error ends it.
pub(crate) fn carry_out(
    tracker: &mut Tracker,
    machine: &mut dyn Machine,
    actions: Vec<Action>,
    other: &mut dyn FnMut(Action) -> Result<()>,
) -> Result<()> {
    let mut todo: VecDeque<Action> = actions.into();
    while let Some(action) = todo.pop_front() {
        let ask = match action {
            Action::Snapshot { tagged, ask } => {
                tracker.snapshot(tagged);
                ask
            }
            Action::CheckAlive(ask) => Some(ask),
            Action::SignalGroups(sig) => {
                tracker.signal(sig);
                None
            }
            action => {
                other(action)?;
                None
            }
        };
        let Some(ask) = ask else {
            continue;
        };
        let procs = tracker.liveness();
        if let Some(status) = tracker.newly_exited() {
            todo.extend(machine.step(Event::Exited(status), Instant::now()));
        }
        todo.extend(machine.step(Event::Procs(ask, procs), Instant::now()));
    }
    Ok(())
}

/// The stop ladder, run to its end (`_Tracker.stop`): what is still alive, empty once
/// everything is confirmed gone. With a deadline it gives up there; without one it sends
/// SIGKILL again until everything is gone.
pub fn stop(tracker: &mut Tracker, deadline: Option<Instant>) -> Vec<StoppedProc> {
    let mut ladder = Ladder::new(deadline);
    let mut next = ladder.start(Instant::now());
    loop {
        // A ladder asks only for snapshots, checks and signals: nothing else to carry out,
        // so nothing can fail.
        let _ = carry_out(tracker, &mut ladder, next, &mut |_| Ok(()));
        match ladder.done() {
            Some(true) => return Vec::new(),
            Some(false) => return tracker.alive(),
            None => {}
        }
        thread::sleep(ladder.next_wakeup(Instant::now()));
        next = ladder.step(&Event::Tick, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use rustix::process::Signal;

    use super::*;
    use crate::sys::detach_session;

    /// A tracker whose Codex is killed and reaped when the test ends, pass or fail, as the
    /// Python's `addCleanup(p.kill, p.wait)`.
    struct Reaped(Tracker);

    impl Drop for Reaped {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0.child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// A process this test recorded by identity: killed when the test ends if it is still
    /// that process.
    struct Stray(ProcId);

    impl Drop for Stray {
        fn drop(&mut self) {
            procs::signal_if_same(&self.0, Signal::KILL);
        }
    }

    /// `sleep 30` in a new session.
    fn sleeper(tag: Option<&str>) -> Child {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdin(Stdio::null());
        if let Some(tag) = tag {
            cmd.env(TAG_VAR, tag);
        }
        detach_session(&mut cmd);
        cmd.spawn().unwrap()
    }

    fn pid_of(child: &Child) -> i32 {
        i32::try_from(child.id()).unwrap()
    }

    // CATALOG: test_dispatch.py::FixRoundTests::test_unknown_codex_start_time_still_enforced
    #[test]
    fn unknown_codex_start_time_still_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let child = sleeper(None);
        let pid = pid_of(&child);
        let record = StoppedProc {
            pid,
            pgid: Some(pid),
            start: None,
        };
        let mut tracker = Reaped(Tracker::new(tmp.path(), Some(record), Some(child), None));
        assert!(tracker.0.codex_alive());
        let left = stop(
            &mut tracker.0,
            Some(Instant::now() + Duration::from_secs(15)),
        );
        assert_eq!(left, []);
        assert!(tracker.0.exited().is_some());
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
        let pid = pid_of(&child);
        let tool: i32 = line.trim().parse().unwrap();
        let _stray = procs::identify(tool).map(Stray);
        let start = procs::identify(pid).map(|id| id.start);
        let record = StoppedProc {
            pid,
            pgid: Some(pid),
            start,
        };
        let mut tracker = Reaped(Tracker::new(tmp.path(), Some(record), Some(child), None));
        tracker.0.snapshot(false);
        let procs_json = fs::read_to_string(tmp.path().join("procs.json")).unwrap();
        assert!(
            procs_json.contains(&format!("\"pid\": {tool}")),
            "{procs_json}"
        );
        let started = Instant::now();
        assert_eq!(stop(&mut tracker.0, None), []);
        assert!(started.elapsed() < Duration::from_secs(12));
        assert_eq!(procs::identify(tool), None);
        let stopped: Vec<i32> = tracker.0.stopped().iter().map(|p| p.pid).collect();
        assert!(
            stopped.contains(&pid) && stopped.contains(&tool),
            "{stopped:?}"
        );
    }

    #[test]
    fn tagged_process_outside_codexs_tree_is_stopped_after_codex_exits() {
        // A process carrying the dispatch tag that is no descendant of Codex (here the test's
        // own child, in a session of its own): the tag sweep after Codex's exit finds it, and
        // it gets SIGTERM, Codex being gone already.
        static TAGS: AtomicU64 = AtomicU64::new(0);
        let n = TAGS.fetch_add(1, Ordering::Relaxed);
        let tag = format!("r{}-{n}:d1:1", std::process::id());
        let tmp = tempfile::tempdir().unwrap();
        let mut codex = Command::new("true");
        detach_session(&mut codex);
        let mut codex = codex.spawn().unwrap();
        let codex_pid = pid_of(&codex);
        let record = StoppedProc {
            pid: codex_pid,
            pgid: Some(codex_pid),
            start: procs::identify(codex_pid).map(|id| id.start),
        };
        let mut stray = sleeper(Some(&tag));
        let stray_pid = pid_of(&stray);
        let _guard = procs::identify(stray_pid).map(Stray);
        // Codex is gone first.
        let status = codex.wait().unwrap();
        let mut tracker = Tracker::new(tmp.path(), Some(record), None, Some(tag.clone()));
        assert!(status.success() && !tracker.codex_alive());
        // Its exit leaves nothing in its tree to snapshot: only the sweep can find the stray.
        tracker.snapshot(false);
        assert!(tracker.alive().is_empty());
        assert_eq!(stop(&mut tracker, None), []);
        assert_eq!(stray.wait().unwrap().signal(), Some(Signal::TERM.as_raw()));
        let stopped: Vec<i32> = tracker.stopped().iter().map(|p| p.pid).collect();
        assert!(stopped.contains(&stray_pid), "{stopped:?}");
        let procs_json = fs::read_to_string(tmp.path().join("procs.json")).unwrap();
        assert!(procs_json.contains("\"descendants\": []"), "{procs_json}");
    }
}
