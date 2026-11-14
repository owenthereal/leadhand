//! The run's ownership (`launch.RunLock`): an exclusive `flock(2)` on `run.lock`, the
//! long-held run lock, and `holder.json`, which names the launcher and the supervisor.
//!
//! A launch is refused while another process holds the flock, and while `holder.json` names
//! a supervisor whose pid still has its recorded start time, even if that supervisor no
//! longer holds the flock. `holder.json` is checked before `run.lock` is opened (so a refusal
//! creates nothing) and again once the flock is held (the authoritative check). The flock is
//! the Python's, on the same file, so a Rust and a Python launcher exclude each other.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::dispatch::caller::py_str_of;
use crate::dispatch::{field, open_lock_file, read_json_object, truthy};
use crate::error::{Error, Result};
use crate::procs;
use crate::state::{RunPaths, utc_now};
use crate::store::write_json_atomic;
use crate::wire::{PyMap, PyStr};

/// `holder.json` as read: its entries, or none when it is missing or not a JSON object.
pub type Holder = PyMap<Box<RawValue>>;

/// The run lock, held: its open `run.lock` (close-on-exec, as every file Rust opens).
#[derive(Debug)]
pub struct RunLock {
    file: File,
    paths: RunPaths,
    run_id: PyStr,
}

/// `holder.json`'s entries (`RunLock._holder`): none when it is missing, not UTF-8, not JSON
/// or not an object; any other I/O error is an error.
pub fn read_holder(paths: &RunPaths) -> Result<Holder> {
    Ok(read_json_object(&paths.holder)?.unwrap_or_default())
}

/// Whether `holder` names a supervisor that is still alive (`_supervisor_alive`): an integer
/// `supervisor_pid` (not a boolean) and a string `supervisor_start` that the pid still has.
pub fn supervisor_alive(holder: &Holder) -> bool {
    let pid = field::<i64>(holder, "supervisor_pid").and_then(|p| i32::try_from(p).ok());
    let start = field::<String>(holder, "supervisor_start");
    pid.is_some() && start.is_some() && procs::same_process(pid, start.as_deref())
}

/// The refusal (`OwnershipError`): who holds the run, as `holder.json` names it.
pub fn ownership_message(holder: &Holder, run_id: &PyStr, supervisor_alive: bool) -> String {
    let get = |key: &str| holder.get(&PyStr::from(key)).filter(|raw| truthy(raw));
    let run = get("run_id").map_or_else(|| run_id.lossy().into_owned(), |raw| py_str_of(raw));
    let pid = get("supervisor_pid").map(|raw| py_str_of(raw));
    if supervisor_alive {
        let pid = pid
            .or_else(|| get("launcher_pid").map(|raw| py_str_of(raw)))
            .unwrap_or_else(|| "unknown".into());
        let since = get("started_at").map_or_else(|| "unknown".into(), |raw| py_str_of(raw));
        format!("Run already in progress: supervisor pid {pid} since {since} (run {run})")
    } else if let Some(pid) = pid {
        format!(
            "Run already in progress: lock held by another process (supervisor pid {pid} \
             recorded, not running) (run {run})"
        )
    } else {
        format!(
            "Run already in progress: lock held by another process (no supervisor recorded) \
             (run {run})"
        )
    }
}

/// `holder.json` as the launcher writes it, in the Python's key order.
#[derive(Serialize)]
struct HolderRecord<'a> {
    run_id: &'a PyStr,
    launcher_pid: i32,
    launcher_start: Option<String>,
    supervisor_pid: i32,
    supervisor_start: Option<String>,
    started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin_dir: Option<PyStr>,
}

impl RunLock {
    /// Locks `run.lock` (`RunLock.acquire`), or refuses with exit 3 (`Error::refused`) when
    /// someone else owns the run; the refusal's message is printed as it is, with no
    /// `error:` in front.
    pub fn acquire(paths: &RunPaths, run_id: &PyStr) -> Result<RunLock> {
        let refuse =
            |holder: &Holder, alive: bool| Error::refused(ownership_message(holder, run_id, alive));
        let holder = read_holder(paths)?;
        if supervisor_alive(&holder) {
            return Err(refuse(&holder, true));
        }
        let file = open_lock_file(&paths.lock)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            // EAGAIN is EWOULDBLOCK on both platforms.
            Err(Errno::WOULDBLOCK | Errno::ACCESS) => {
                drop(file);
                let holder = read_holder(paths)?;
                return Err(refuse(&holder, supervisor_alive(&holder)));
            }
            Err(e) => return Err(Error::io(&paths.lock, e.into())),
        }
        let lock = RunLock {
            file,
            paths: paths.clone(),
            run_id: run_id.clone(),
        };
        let holder = read_holder(paths)?;
        if supervisor_alive(&holder) {
            lock.release();
            return Err(refuse(&holder, true));
        }
        Ok(lock)
    }

    /// The locked descriptor, which `claude` alone inherits.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Writes `holder.json` (`record_supervisor`): the launcher's pid and start, the
    /// supervisor's, the run ID, when, and the plugin directory when the session loads one.
    pub fn record_supervisor(&self, pid: i32, plugin_dir: Option<&Path>) -> Result<()> {
        let me = rustix::process::getpid().as_raw_pid();
        let holder = HolderRecord {
            run_id: &self.run_id,
            launcher_pid: me,
            launcher_start: procs::identify(me).map(|id| id.start),
            supervisor_pid: pid,
            supervisor_start: procs::identify(pid).map(|id| id.start),
            started_at: utc_now(),
            plugin_dir: plugin_dir.map(|dir| PyStr::from_os(dir.as_os_str())),
        };
        write_json_atomic(&self.paths.holder, &holder)
    }

    /// `holder.json`'s path.
    pub fn holder_path(&self) -> &Path {
        &self.paths.holder
    }

    /// Closes this process's descriptor without unlocking (`detach`): the supervisor's
    /// inherited copy keeps the lock held until it exits.
    pub fn detach(self) {
        drop(self.file);
    }

    /// Unlocks and closes (`release`). Unlocking drops the lock for every holder of the open
    /// file description, so this is called only once the supervisor has exited, or when none
    /// was started.
    pub fn release(self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::run_paths;

    fn holder(json: &str) -> Holder {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn ownership_messages_name_who_holds_the_run() {
        let id = PyStr::from("abc123");
        let h = holder(
            r#"{"run_id": "r1", "supervisor_pid": 42, "started_at": "2026-11-10T00:00:00.000Z"}"#,
        );
        assert_eq!(
            ownership_message(&h, &id, true),
            "Run already in progress: supervisor pid 42 since 2026-11-10T00:00:00.000Z (run r1)"
        );
        assert_eq!(
            ownership_message(&h, &id, false),
            "Run already in progress: lock held by another process (supervisor pid 42 recorded, \
             not running) (run r1)"
        );
        let empty = Holder::default();
        assert_eq!(
            ownership_message(&empty, &id, false),
            "Run already in progress: lock held by another process (no supervisor recorded) \
             (run abc123)"
        );
        assert_eq!(
            ownership_message(&holder(r#"{"launcher_pid": 7}"#), &id, true),
            "Run already in progress: supervisor pid 7 since unknown (run abc123)"
        );
    }

    #[test]
    fn a_recorded_supervisor_is_alive_only_with_its_start_time() {
        let me = rustix::process::getpid().as_raw_pid();
        let start = procs::identify(me).unwrap().start;
        let alive = |json: String| supervisor_alive(&holder(&json));
        assert!(alive(format!(
            r#"{{"supervisor_pid": {me}, "supervisor_start": "{start}"}}"#
        )));
        assert!(!alive(format!(
            r#"{{"supervisor_pid": {me}, "supervisor_start": "other"}}"#
        )));
        // Not an integer, or no start string: not alive (a bool is not a pid).
        assert!(!alive(format!(
            r#"{{"supervisor_pid": "{me}", "supervisor_start": "{start}"}}"#
        )));
        assert!(!alive(format!(
            r#"{{"supervisor_pid": true, "supervisor_start": "{start}"}}"#
        )));
        assert!(!alive(format!(r#"{{"supervisor_pid": {me}}}"#)));
    }

    #[test]
    fn the_lock_excludes_a_second_owner_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let paths = run_paths(dir.path());
        let id = PyStr::from("abc123");
        let lock = RunLock::acquire(&paths, &id).unwrap();
        let busy = RunLock::acquire(&paths, &id).unwrap_err();
        assert_eq!(busy.exit, 3);
        assert!(busy.message.contains("no supervisor recorded"), "{busy}");
        lock.record_supervisor(rustix::process::getpid().as_raw_pid(), None)
            .unwrap();
        // holder.json now names a live supervisor (this process): refused even once the
        // flock is free.
        lock.release();
        let refused = RunLock::acquire(&paths, &id).unwrap_err();
        assert!(refused.message.contains(" since "), "{refused}");
        std::fs::remove_file(&paths.holder).unwrap();
        RunLock::acquire(&paths, &id).unwrap().release();
    }
}
