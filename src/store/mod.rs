//! The run store: the append, read and atomic-write primitives behind every run file, under
//! the same `flock(2)` on `<run>/state.lock` that the Python `state` module takes, so a Rust and a
//! Python process acting on one run exclude each other (spec, contract item 12; Review Focus 1).
//!
//! - `lock`: `state_lock`, re-entrant within the process (`state.state_lock`);
//! - `jsonl`: append one line and read whole lines (`state._append`, `state._read_jsonl`);
//! - `atomic`: replace a JSON file (`state.write_json_atomic`), or any file, atomically.
//!
//! Records are written through `crate::wire`, as ASCII JSON.

use std::ffi::{OsStr, OsString};
use std::os::fd::AsFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{Mode, OFlags};

use crate::error::{Error, Result};

mod atomic;
mod jsonl;
mod lock;

pub use atomic::{write_file_atomic, write_file_atomic_mode, write_json_atomic};
pub use jsonl::{append_jsonl, append_jsonl_with, read_jsonl};
pub use lock::{StateLock, state_lock, try_state_lock};

/// `fsync(2)` (not macOS's `F_FULLFSYNC`), as Python's `os.fsync`; `path` names the file in
/// an error.
pub(crate) fn fsync(fd: impl AsFd, path: &Path) -> Result<()> {
    #[cfg(test)]
    FSYNCS.set(FSYNCS.get() + 1);
    rustix::fs::fsync(fd).map_err(|e| Error::io(path, e.into()))
}

/// `state._fsync_dir`: makes a new or replaced directory entry durable.
pub(crate) fn fsync_dir(dir: &Path) -> Result<()> {
    let fd = rustix::fs::open(dir, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| Error::io(dir, e.into()))?;
    fsync(fd, dir)
}

/// A fresh temporary name beside `name`: `.<name>.<pid>.<hex>.tmp`. Callers create it with
/// `O_EXCL` and take another on a clash.
pub(crate) fn temp_name(name: &OsStr) -> OsString {
    #[cfg(test)]
    if let Some(n) = FORCED_TEMP.with_borrow_mut(|q| q.pop_front()) {
        return n;
    }
    static TAKEN: AtomicU64 = AtomicU64::new(0);
    let n = TAKEN.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut out = OsString::from(".");
    out.push(name);
    out.push(format!(".{}.{nanos:08x}{n:x}.tmp", std::process::id()));
    out
}

/// How many fresh temporary names a writer tries before giving up.
pub(crate) const TEMP_TRIES: u32 = 100;

/// The directory holding `file`: `.` for a bare name, as Python's `Path.parent`.
pub(crate) fn parent(file: &Path) -> &Path {
    match file.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

#[cfg(test)]
thread_local! {
    /// The `fsync` calls this thread made, for the tests that count them.
    pub(crate) static FSYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Names `temp_name` hands out, in order, before making its own (tests only).
    pub(crate) static FORCED_TEMP: std::cell::RefCell<std::collections::VecDeque<OsString>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}
