//! `state_lock`: an exclusive `flock(2)` on `<run>/state.lock`, re-entrant within the process
//! (`state.state_lock`).
//!
//! The lock is short-held, around every JSONL repair and append and every
//! read-compute-append sequence, so a caller that must read and then append (the event `seq`)
//! takes it first and the append inside re-enters it. Within this process a per-path depth
//! count makes a nested lock neither re-lock nor unlock early; other threads wait for the
//! holder, other processes (Rust or Python) wait on the `flock`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::marker::PhantomData;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, ThreadId};

use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;

use crate::error::{Error, Failure, Result};

/// The file name of the state lock inside a run directory.
pub(crate) const STATE_LOCK: &str = "state.lock";

/// This process's hold on one state lock: which thread has it, how deeply, and the file
/// whose `flock` it is.
#[derive(Default)]
struct Held {
    holder: Mutex<Holder>,
    released: Condvar,
}

#[derive(Default)]
struct Holder {
    thread: Option<ThreadId>,
    depth: usize,
    file: Option<File>,
}

/// The hold for each state lock this process has taken, by its real path (Python keys by
/// pid as well, for a forked child; Rust never forks without `exec`).
fn held_for(key: PathBuf) -> Arc<Held> {
    static HELD: OnceLock<Mutex<HashMap<PathBuf, Arc<Held>>>> = OnceLock::new();
    let map = HELD.get_or_init(Mutex::default);
    Arc::clone(locked(map).entry(key).or_default())
}

/// The mutex's contents; nothing panics while holding one, so a poisoned one is still sound.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The held state lock; dropping it releases one level (RAII). It belongs to the thread that
/// took it.
pub struct StateLock {
    held: Arc<Held>,
    /// Not `Send`: the hold is the taking thread's.
    _thread: PhantomData<*const ()>,
}

/// Takes the state lock of `run_dir`, waiting for any other holder.
pub fn state_lock(run_dir: &Path) -> Result<StateLock> {
    try_state_lock(run_dir).map_err(Error::from)
}

/// As `state_lock`, with the failure's text kept lossless, for a reader that reports it as
/// data (`status`'s attention lines).
pub fn try_state_lock(run_dir: &Path) -> std::result::Result<StateLock, Failure> {
    let path = run_dir.join(STATE_LOCK);
    let key = fs::canonicalize(run_dir)
        .map_err(|e| Failure::io(&path, e))?
        .join(STATE_LOCK);
    let held = held_for(key);
    let me = thread::current().id();
    {
        let mut holder = locked(&held.holder);
        while holder.thread.is_some_and(|t| t != me) {
            holder = held
                .released
                .wait(holder)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if holder.thread == Some(me) {
            holder.depth += 1;
            return Ok(StateLock {
                held: Arc::clone(&held),
                _thread: PhantomData,
            });
        }
        // Reserved: other threads wait while this one waits for the flock.
        holder.thread = Some(me);
    }
    match take_flock(&path) {
        Ok(file) => {
            let mut holder = locked(&held.holder);
            holder.file = Some(file);
            holder.depth = 1;
        }
        Err(e) => {
            locked(&held.holder).thread = None;
            held.released.notify_one();
            return Err(e);
        }
    }
    Ok(StateLock {
        held,
        _thread: PhantomData,
    })
}

/// Opens `path` (created `0644`) and takes an exclusive `flock` on it, waiting.
fn take_flock(path: &Path) -> std::result::Result<File, Failure> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(path)
        .map_err(|e| Failure::io(path, e))?;
    loop {
        match flock(&file, FlockOperation::LockExclusive) {
            Ok(()) => return Ok(file),
            Err(Errno::INTR) => continue,
            Err(e) => return Err(Failure::io(path, e.into())),
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let mut holder = locked(&self.held.holder);
        holder.depth -= 1;
        if holder.depth == 0 {
            if let Some(file) = holder.file.take() {
                // Closing the file releases it as well.
                let _ = flock(&file, FlockOperation::Unlock);
            }
            holder.thread = None;
            drop(holder);
            self.held.released.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::path::Path;

    use rustix::fs::{FlockOperation, flock};

    use super::*;

    /// Whether another open file description could take the lock now.
    fn try_flock(run: &Path) -> bool {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(run.join(STATE_LOCK))
            .unwrap();
        let free = flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok();
        if free {
            flock(&file, FlockOperation::Unlock).unwrap();
        }
        free
    }

    fn run_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("run with spaces")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn state_lock_is_reentrant_within_a_process() {
        let tmp = run_dir();
        let run = tmp.path();
        {
            let _outer = state_lock(run).unwrap();
            {
                let _inner = state_lock(run).unwrap();
                assert!(!try_flock(run));
            }
            // Leaving the inner level must not release the lock.
            assert!(!try_flock(run));
        }
        assert!(try_flock(run));
    }

    #[test]
    fn state_lock_released_after_exception() {
        let tmp = run_dir();
        let run = tmp.path();
        let panicked = std::panic::catch_unwind(|| {
            let _held = state_lock(run).unwrap();
            panic!("boom");
        });
        assert!(panicked.is_err());
        assert!(try_flock(run));

        // An early error return releases it too.
        let failing = || -> Result<()> {
            let _held = state_lock(run)?;
            Err(crate::Error::msg("boom"))
        };
        assert!(failing().is_err());
        assert!(try_flock(run));
    }

    #[test]
    fn another_thread_waits_for_the_holder() {
        let tmp = run_dir();
        let run = tmp.path().to_path_buf();
        let held = state_lock(&run).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _lock = state_lock(&run).unwrap();
            tx.send(()).unwrap();
        });
        let early = rx.recv_timeout(std::time::Duration::from_millis(200));
        assert!(early.is_err(), "the other thread took a held lock");
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn a_missing_run_dir_names_the_lock_file() {
        let tmp = run_dir();
        let gone = tmp.path().join("nope");
        let err = state_lock(&gone).err().unwrap();
        assert_eq!(
            err.message,
            format!(
                "[Errno 2] No such file or directory: '{}'",
                gone.join(STATE_LOCK).display()
            )
        );
    }
}
