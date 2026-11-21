//! Process identity, over the process table in `crate::sys` (`procs.py`).
//!
//! A process's identity is (pid, start time): a pid alone can be reused, so code that
//! signals a recorded process first checks that the pid still has the start time it was
//! recorded with. The start time is opaque, only ever compared for equality, and
//! byte-identical to `procs.py`'s, so a Rust process and a Python process can verify each
//! other's recorded identities. Zombies (exited, not yet reaped) count as gone: they cannot
//! run or write anything.

use std::collections::{HashMap, HashSet};

use rustix::process::{Pid, Signal, getpgrp, getpid, getuid, kill_process_group};
use serde::{Deserialize, Serialize};

use crate::sys;
pub use crate::sys::ProcInfo;

/// A process's identity: its pid and the start time that pid had when it was recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcId {
    pub pid: i32,
    pub start: String,
}

/// The live process's identity; `None` if it is gone or a zombie (`procs.process_start_time`).
pub fn identify(pid: i32) -> Option<ProcId> {
    let info = sys::proc_info(pid).filter(|i| !i.zombie)?;
    Some(ProcId {
        pid: info.pid,
        start: info.start,
    })
}

/// True when `pid` is alive (not a zombie) and still has start time `start`
/// (`procs.same_process`: a missing or zero pid, or a missing or empty start, is false).
pub fn same_process(pid: Option<i32>, start: Option<&str>) -> bool {
    match (pid, start) {
        (Some(pid), Some(start)) if pid != 0 && !start.is_empty() => {
            identify(pid).is_some_and(|id| id.start == start)
        }
        _ => false,
    }
}

/// Every live process below `root` in the ppid tree, not `root` itself (`procs.descendants`).
/// Zombies are left out, though the walk still goes below them.
pub fn descendants(root: i32) -> Vec<ProcInfo> {
    let mut children: HashMap<i32, Vec<ProcInfo>> = HashMap::new();
    for p in sys::all_procs().unwrap_or_default() {
        if p.pid != p.ppid {
            children.entry(p.ppid).or_default().push(p);
        }
    }
    let (mut out, mut todo, mut seen) = (Vec::new(), vec![root], HashSet::from([root]));
    while let Some(parent) = todo.pop() {
        for child in children.remove(&parent).unwrap_or_default() {
            if seen.insert(child.pid) {
                todo.push(child.pid);
                if !child.zombie {
                    out.push(child);
                }
            }
        }
    }
    out
}

/// This user's live processes whose initial environment holds `entry` (`NAME=value`), and
/// the pids of this user's live processes whose environment could not be read; never the
/// calling process (`procs.tagged_processes`). `None` when the process table cannot be read.
pub fn tagged(entry: &str) -> Option<(Vec<ProcInfo>, Vec<i32>)> {
    let (me, uid) = (getpid().as_raw_pid(), getuid().as_raw());
    let (mut found, mut unreadable) = (Vec::new(), Vec::new());
    for info in sys::all_procs()? {
        if info.zombie || info.pid == 0 || info.pid == me || info.uid != uid {
            continue;
        }
        match sys::env_has(info.pid, entry.as_bytes()) {
            Some(true) => found.push(info),
            Some(false) => {}
            // Still there with the same start time: alive, and unreadable. Otherwise gone.
            None => {
                if same_process(Some(info.pid), Some(&info.start)) {
                    unreadable.push(info.pid);
                }
            }
        }
    }
    Some((found, unreadable))
}

/// Sends `sig` to process group `pgid` only while `owner`, its recorded leader, is alive with
/// its recorded start time and still in that group: while a live member holds it, the group
/// id cannot have been reused. Never this process's own group, nor 0 or 1. True when the
/// signal was sent.
pub fn signal_group_if_same(owner: &ProcId, pgid: i32, sig: Signal) -> bool {
    if pgid <= 1 || pgid == getpgrp().as_raw_pid() {
        return false;
    }
    let Some(info) = sys::proc_info(owner.pid) else {
        return false;
    };
    if info.zombie || info.start != owner.start || info.pgid != pgid {
        return false;
    }
    Pid::from_raw(pgid).is_some_and(|group| kill_process_group(group, sig).is_ok())
}

/// Sends `sig` to the process `id` names only while it is alive with its recorded start time
/// (`_Tracker.signal`'s per-process step, for a recorded process whose group was not
/// signalled). Never this process. True when the signal was sent.
pub fn signal_if_same(id: &ProcId, sig: Signal) -> bool {
    if id.pid == getpid().as_raw_pid() || !same_process(Some(id.pid), Some(&id.start)) {
        return false;
    }
    Pid::from_raw(id.pid).is_some_and(|pid| rustix::process::kill_process(pid, sig).is_ok())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{ChildStdin, Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use rustix::process::getppid;

    use super::*;
    use crate::sys::tests::{Kid, sleeper};
    use crate::sys::{self, detach_session};

    fn me() -> i32 {
        getpid().as_raw_pid()
    }

    fn pids(procs: &[ProcInfo]) -> Vec<i32> {
        let mut out: Vec<i32> = procs.iter().map(|p| p.pid).collect();
        out.sort_unstable();
        out
    }

    /// Polls `cond` for up to five seconds.
    fn eventually(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// A unique `LEADHAND_TEST_TAG=…` entry: the pid, a process-wide counter (two tests in
    /// parallel can read the same clock time) and the time.
    fn unique_tag() -> String {
        static TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TAKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("LEADHAND_TEST_TAG=t-{}-{n}-{nanos}", me())
    }

    #[test]
    fn unique_tags_never_repeat() {
        let tags: Vec<String> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..4)
                .map(|_| s.spawn(|| (0..2500).map(|_| unique_tag()).collect::<Vec<_>>()))
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        });
        let distinct: std::collections::HashSet<&String> = tags.iter().collect();
        assert_eq!(distinct.len(), tags.len());
    }

    /// A Python child in a new session that starts `sleep 30` in another new session, prints
    /// its pid, and kills it when its stdin closes; with `tag` in its environment, if given.
    /// Returns the child, its stdin, and the grandchild's pid.
    fn parent_of_detached_sleeper(tag: Option<&str>) -> (Kid, ChildStdin, i32) {
        let mut cmd = Command::new("python3");
        cmd.args([
            "-c",
            "import subprocess, sys\n\
             p = subprocess.Popen(['sleep', '30'], start_new_session=True, stdin=subprocess.DEVNULL)\n\
             print(p.pid, flush=True)\n\
             sys.stdin.read()\n\
             p.kill()\n\
             p.wait()\n",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
        if let Some((name, value)) = tag.and_then(|t| t.split_once('=')) {
            cmd.env(name, value);
        }
        detach_session(&mut cmd);
        let mut kid = Kid(cmd.spawn().expect("spawn python3"));
        let stdin = kid.0.stdin.take().unwrap();
        let mut line = String::new();
        BufReader::new(kid.0.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild = line.trim().parse().expect("the grandchild's pid");
        (kid, stdin, grandchild)
    }

    // ----------------------------------------------------------------------
    // The catalogue's `procs` rows (test_procs.py::ProcsTests)

    #[test]
    fn own_process_info() {
        let info = sys::proc_info(me()).expect("our own process is in the table");
        assert_eq!(info.pid, me());
        assert_eq!(info.ppid, Pid::as_raw(getppid()));
        assert_eq!(info.pgid, getpgrp().as_raw_pid());
        assert!(!info.zombie);
        assert_eq!(info.uid, getuid().as_raw());
    }

    #[test]
    fn start_time_is_stable_and_differs_between_processes() {
        let (a, b) = (sleeper(false), sleeper(false));
        let sa = identify(a.pid()).expect("a is alive").start;
        assert!(!sa.is_empty());
        assert_eq!(identify(a.pid()).unwrap().start, sa);
        assert_ne!(identify(me()).unwrap().start, sa);
        assert!(identify(b.pid()).is_some_and(|p| !p.start.is_empty()));
    }

    #[test]
    fn child_in_new_session_has_own_group_and_is_a_descendant() {
        let child = sleeper(true);
        let info = sys::proc_info(child.pid()).unwrap();
        assert_eq!((info.ppid, info.pgid), (me(), child.pid()));
        assert!(pids(&descendants(me())).contains(&child.pid()));
        assert!(pids(&sys::all_procs().unwrap()).contains(&child.pid()));
    }

    #[test]
    fn same_process_checks_start_time_and_zombies() {
        let mut child = sleeper(false);
        let pid = child.pid();
        let start = identify(pid).unwrap().start;
        assert!(same_process(Some(pid), Some(&start)));
        assert!(!same_process(Some(pid), Some(&format!("{start}x"))));
        assert!(!same_process(Some(pid), None));
        assert!(!same_process(None, Some(&start)));
        assert!(!same_process(Some(pid), Some("")));
        child.0.kill().unwrap();
        // Killed but not yet reaped: a zombie counts as gone.
        assert!(eventually(|| !same_process(Some(pid), Some(&start))));
        child.0.wait().unwrap();
        assert_eq!(identify(pid), None);
        assert!(!pids(&descendants(me())).contains(&pid));
    }

    // test_ps_fallback_parses_lstart_with_spaces is not ported (S2-R19): it tests the
    // Python's `ps` fallback, and Rust never runs `ps`.

    #[test]
    fn tagged_processes_finds_a_process_by_an_environment_entry() {
        let tag = unique_tag();
        let (name, value) = tag.split_once('=').unwrap();
        let mut cmd = Command::new("python3");
        cmd.args([
            "-c",
            "import time; time.sleep(30)",
            &format!("{name}={value}-argv"),
        ])
        .env(name, value)
        .stdin(Stdio::null());
        detach_session(&mut cmd);
        let p = Kid(cmd.spawn().expect("spawn python3"));
        let other = sleeper(true);
        std::thread::sleep(Duration::from_millis(200));
        let (found, _) = tagged(&tag).expect("environments are readable on Linux and macOS");
        assert_eq!(pids(&found), vec![p.pid()]);
        assert_eq!(
            Some(found[0].start.clone()),
            identify(p.pid()).map(|i| i.start)
        );
        // An argument is not the environment.
        assert_eq!(tagged(&format!("{tag}-argv")).unwrap().0, vec![]);
        assert!(!pids(&tagged(&tag).unwrap().0).contains(&other.pid()));
    }

    #[test]
    fn missing_pid() {
        assert_eq!(sys::proc_info(0), None);
        assert_eq!(sys::proc_info((1 << 22) + 12345), None);
    }

    // ----------------------------------------------------------------------
    // The brief's own tests

    /// A start time Rust reads is byte-identical to the one `procs.py` reads for the same
    /// process, so either implementation can verify the other's recorded identities.
    #[test]
    fn start_time_string_matches_python() {
        let kid = sleeper(false);
        let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/lib");
        let out = Command::new("python3")
            .args([
                "-B",
                "-c",
                "import sys\n\
                 from leadhand import procs\n\
                 print(procs.process_start_time(int(sys.argv[1])))\n",
                &kid.pid().to_string(),
            ])
            .env("PYTHONPATH", &lib)
            .stdin(Stdio::null())
            .output()
            .expect("run python3");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let python = stdout.trim_end_matches('\n');
        let rust = identify(kid.pid()).expect("the child is alive").start;
        assert_ne!(python, "None", "Python could not read the child");
        assert_eq!(python, rust);
    }

    #[test]
    fn zombie_counts_as_gone() {
        let mut child = sleeper(false);
        let pid = child.pid();
        let start = identify(pid).unwrap().start;
        child.0.kill().unwrap();
        assert!(
            eventually(|| sys::proc_info(pid).is_some_and(|i| i.zombie)),
            "killed and not reaped: in the table as a zombie"
        );
        assert_eq!(identify(pid), None);
        assert!(!same_process(Some(pid), Some(&start)));
        assert!(!pids(&descendants(me())).contains(&pid));
        let owner = ProcId { pid, start };
        assert!(!signal_group_if_same(&owner, pid, Signal::TERM));
        child.0.wait().unwrap();
        assert_eq!(sys::proc_info(pid), None);
    }

    #[test]
    fn descendants_found_across_groups() {
        let (child, stdin, grandchild) = parent_of_detached_sleeper(None);
        let infos = descendants(me());
        let child_info = infos
            .iter()
            .find(|p| p.pid == child.pid())
            .expect("the child");
        let grand_info = infos
            .iter()
            .find(|p| p.pid == grandchild)
            .expect("the grandchild");
        assert_eq!(child_info.pgid, child.pid());
        assert_eq!(
            (grand_info.ppid, grand_info.pgid),
            (child.pid(), grandchild)
        );
        assert_eq!(pids(&descendants(child.pid())), vec![grandchild]);
        assert_eq!(descendants(grandchild), vec![]);
        drop(stdin);
        let mut child = child;
        assert!(child.0.wait().unwrap().success());
        assert!(!pids(&descendants(me())).contains(&grandchild));
    }

    #[test]
    fn tagged_finds_env_entry() {
        // The dispatch case: a descendant in another session inherits the tag.
        let tag = unique_tag();
        let (child, stdin, grandchild) = parent_of_detached_sleeper(Some(&tag));
        let untagged = sleeper(true);
        let (found, _) = tagged(&tag).unwrap();
        let mut want = vec![child.pid(), grandchild];
        want.sort_unstable();
        assert_eq!(pids(&found), want);
        // Only the whole entry matches.
        assert_eq!(tagged(&tag[..tag.len() - 1]).unwrap().0, vec![]);
        assert_eq!(tagged("LEADHAND_TEST_TAG").unwrap().0, vec![]);
        assert!(!pids(&found).contains(&untagged.pid()));
        // Never this process, even when it carries the entry.
        let path = std::env::var("PATH").unwrap();
        assert!(!pids(&tagged(&format!("PATH={path}")).unwrap().0).contains(&me()));
        drop(stdin);
        let mut child = child;
        child.0.wait().unwrap();
        assert!(eventually(|| tagged(&tag).unwrap().0.is_empty()));
    }

    #[test]
    fn signal_group_if_same_needs_a_live_matching_leader() {
        let mut kid = sleeper(true);
        let pid = kid.pid();
        let owner = identify(pid).unwrap();
        let other = sleeper(true);
        let stale = ProcId {
            pid,
            start: format!("{}x", owner.start),
        };
        assert!(
            !signal_group_if_same(&stale, pid, Signal::TERM),
            "a stale identity"
        );
        assert!(
            !signal_group_if_same(&owner, other.pid(), Signal::TERM),
            "a group the owner is not in"
        );
        // Never our own group, nor 0 or 1. SIGCHLD, ignored by default, in case it were sent.
        let mine = identify(me()).unwrap();
        assert!(!signal_group_if_same(
            &mine,
            getpgrp().as_raw_pid(),
            Signal::CHILD
        ));
        assert!(!signal_group_if_same(&mine, 0, Signal::CHILD));
        assert!(!signal_group_if_same(&mine, 1, Signal::CHILD));
        assert_eq!(kid.0.try_wait().unwrap(), None, "nothing was signalled");

        assert!(signal_group_if_same(&owner, pid, Signal::TERM));
        assert_eq!(kid.0.wait().unwrap().signal(), Some(Signal::TERM.as_raw()));
        assert!(!signal_group_if_same(&owner, pid, Signal::TERM), "gone");
        let mut other = other;
        assert_eq!(other.0.try_wait().unwrap(), None);
    }

    #[test]
    fn signal_if_same_needs_the_recorded_start_time() {
        let mut kid = sleeper(false);
        let pid = kid.pid();
        let id = identify(pid).unwrap();
        let stale = ProcId {
            pid,
            start: format!("{}x", id.start),
        };
        assert!(!signal_if_same(&stale, Signal::TERM), "a stale identity");
        // Never this process. SIGCHLD, ignored by default, in case it were sent.
        assert!(!signal_if_same(&identify(me()).unwrap(), Signal::CHILD));
        assert_eq!(kid.0.try_wait().unwrap(), None, "nothing was signalled");
        assert!(signal_if_same(&id, Signal::TERM));
        assert_eq!(kid.0.wait().unwrap().signal(), Some(Signal::TERM.as_raw()));
        assert!(!signal_if_same(&id, Signal::TERM), "gone");
    }
}
