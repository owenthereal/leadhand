//! Codex dispatch (`dispatch.py`): what starts a Codex worker, and what keeps a forged
//! dispatch out.
//!
//! - `request`: the typed `request.json` and `result.json` records (contract item 5);
//! - `argv`: the exact `codex exec` command line (contract item 10);
//! - `env`: the worker's environment, which seals Git push credentials and carries the
//!   dispatch tag (contract item 9);
//! - `caller`: the run anchor `dispatch`, `stop-dispatch` and `dispatch-runner` check
//!   (security fixes H1, H2 and M1, `docs/compatibility.md`);
//! - `runner`: the detached runner's pure core, a state machine (spec, decision 6);
//! - `shell`: the runner's life, and the threads and I/O around the core;
//! - `tracker`: Codex and its recorded processes, which the stop ladder signals;
//! - `finalize`: classifying, committing, charging, the attempt and `result.json`;
//! - `commit`: the runner's commit of a Codex worker's work (ruling R39);
//! - `own`: who owns a dispatch (`owner.lock`, `runner.lock`, `runner.json`), and replacing
//!   or spawning its runner;
//! - `cancel`: stopping a dispatch and confirming everything is gone;
//! - `cmd`: the `dispatch`, `stop-dispatch` and `dispatch-runner` commands.

pub mod argv;
pub mod caller;
pub mod cancel;
pub mod cmd;
pub mod commit;
pub mod env;
pub mod finalize;
pub mod own;
pub mod request;
pub mod runner;
pub mod shell;
pub mod tracker;

use std::fs::{File, FileTimes, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::IgnoredAny;
use serde_json::value::RawValue;

use crate::error::{Error, Result};
use crate::wire::{PyMap, PyStr};

pub use argv::{codex_argv, codex_argv_parts, toml_string};
pub use caller::{
    Anchor, anchored_run, anchored_worker, check_caller, check_dispatch_id, check_request,
    evidence_dirs, runner_worker, worker_dir,
};
pub use cancel::cancel;
pub use cmd::{dispatch, request_for};
pub use commit::{RunnerCommits, commit_work, untracked_to_take};
pub use env::{
    ASKPASS_RESET, BLOCKING_ENV, CREDENTIAL_HELPER_RESET, CREDENTIAL_VARS, Env, TAG_VAR,
    blocking_env, codex_overrides, worker_env,
};
pub use own::{RunnerState, runner_alive, runner_state, stop_strays};
pub use request::{
    BlockedBy, DispatchRequest, DispatchResult, ExitReason, Status, StoppedProc, load_result,
    read_request, thread_id,
};

/// The stream items that count as a worker step: an `item.completed` event whose item type
/// is one of these (probes.md, Tasks 3 and 4).
pub const STEP_ITEM_TYPES: [&str; 2] = ["command_execution", "file_change"];

/// The dispatch kinds whose work the runner commits once Codex is confirmed stopped (ruling
/// R39: Codex's sandbox keeps the Git directory read-only).
pub const COMMIT_KINDS: [&str; 5] = ["implement", "fix", "continue", "answer", "final_fix"];

/// The worker folder's request, written once by `dispatch`.
pub const REQUEST: &str = "request.json";
/// The worker folder's final result, written last by the runner.
pub const RESULT: &str = "result.json";
/// The runner's own identity, the launch intent, then Codex's (contract item 5).
pub const RUNNER: &str = "runner.json";
/// Touched by the runner every `HEARTBEAT_EVERY` wall-clock seconds.
pub const HEARTBEAT: &str = "heartbeat";
/// Codex's live descendants, refreshed while it runs.
pub const PROCS: &str = "procs.json";
/// Codex's `--json` stdout, which Codex writes itself.
pub const STREAM: &str = "stream.jsonl";
/// Codex's stderr.
pub const STDERR: &str = "codex.stderr";
/// Codex's `-o` file: the reply.
pub const LAST: &str = "last-message.md";
/// The reply, kept when it has no parseable status line.
pub const RAW: &str = "reply.raw.md";
/// A cancel request; its existence is the request.
pub const CANCEL: &str = "cancel";
/// The runner's acknowledgement that it is stopping.
pub const STOPPING: &str = "stopping.json";
/// Held while a dispatch is finalized.
pub const FINALIZE_LOCK: &str = "finalize.lock";
/// Held by a runner for its whole life: held means a runner is alive.
pub const RUNNER_LOCK: &str = "runner.lock";
/// The empty `GH_CONFIG_DIR` the worker gets.
pub const GH_DIR: &str = "gh-config";
/// The files of generations that were cancelled and replaced.
pub const SUPERSEDED: &str = "superseded";
/// The workspace's untracked files when the first runner launched Codex (I3).
pub const UNTRACKED: &str = "untracked.json";

/// A record's field `key` read leniently, as the Python's `d.get(key)` followed by a type
/// check: `None` when it is missing or not a `T`.
pub(crate) fn field<T: serde::de::DeserializeOwned>(
    map: &PyMap<Box<RawValue>>,
    key: &str,
) -> Option<T> {
    map.get(&PyStr::from(key))
        .and_then(|raw| serde_json::from_str::<T>(raw.get()).ok())
}

/// A process record (`{pid, pgid, start}`, also `spawn.json`'s `{pid, start, at}`) read
/// leniently, as the Python reads it: `None` unless `pid` is an integer; a `pgid` that is not
/// an integer is none, and so is a start time that is not a string. Liveness is judged from
/// the pid and start time alone (`StoppedProc::alive`), as `procs.same_process` judges it.
pub(crate) fn proc_record_in(map: &PyMap<Box<RawValue>>) -> Option<StoppedProc> {
    Some(StoppedProc {
        pid: field(map, "pid")?,
        pgid: field(map, "pgid"),
        start: field(map, "start"),
    })
}

/// `proc_record_in` of a JSON value: `None` unless it is an object.
pub(crate) fn proc_record(raw: &RawValue) -> Option<StoppedProc> {
    proc_record_in(&serde_json::from_str(raw.get()).ok()?)
}

/// A JSON object file as `dispatch._read_json` reads it: `None` when the file is missing,
/// not UTF-8, not JSON, or not an object; its entries otherwise (a repeated key keeps its
/// last value, as `json.loads` does). Any other I/O error is an error.
pub fn read_json_object(path: &Path) -> Result<Option<PyMap<Box<RawValue>>>> {
    Ok(read_json_text(path)?.and_then(|text| serde_json::from_str(&text).ok()))
}

// --------------------------------------------------------------------------
// The worker folder's files, as the Python's runner reads and writes them

/// `_touch`: creates the file if needed, and sets its times to now.
pub(crate) fn touch(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    let now = SystemTime::now();
    file.set_times(FileTimes::new().set_accessed(now).set_modified(now))
        .map_err(|e| Error::io(path, e))
}

/// `time.time()`.
pub(crate) fn wall_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// A lock file, opened as the Python opens it (`O_RDWR | O_CREAT | O_CLOEXEC`, `0644`).
pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(path)
        .map_err(|e| Error::io(path, e))
}

/// `Path.write_text(text, encoding="utf-8")`: the file is replaced first, then written; text
/// holding a lone surrogate fails there, leaving it empty, as Python's strict encoder does.
pub(crate) fn write_text(path: &Path, text: &PyStr) -> Result<()> {
    let mut file = File::create(path).map_err(|e| Error::io(path, e))?;
    file.write_all(text.encode_utf8()?.as_bytes())
        .map_err(|e| Error::io(path, e))
}

/// `Path.read_text(encoding="utf-8", errors="replace")`: undecodable bytes replaced, line ends
/// translated to `\n`; `None` when the file is missing.
pub(crate) fn read_text(path: &Path) -> Result<Option<PyStr>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(path, e)),
    };
    let text = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    Ok(Some(PyStr::from(text)))
}

/// The bytes of `path` from `offset` on; none when the file is missing.
pub(crate) fn read_from(path: &Path, offset: u64) -> Result<Vec<u8>> {
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

/// Whether a JSON value is true as Python's `bool` reads the value `json.loads` gives.
pub(crate) fn truthy(raw: &RawValue) -> bool {
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

/// `_run_id`: `run.json`'s `run_id`, else the run directory's name. A run ID that is not a
/// string, or not text, is an error (an anchored run's is always a non-empty string).
pub(crate) fn run_id_of(run_dir: &Path) -> Result<String> {
    let meta = read_json_object(&crate::state::run_paths(run_dir).run_json)?.unwrap_or_default();
    match meta.get(&PyStr::from("run_id")).filter(|raw| truthy(raw)) {
        Some(raw) => {
            let text = serde_json::from_str::<PyStr>(raw.get()).map_err(|_| {
                Error::msg(format!("run.json's run_id {} is not a string", raw.get()))
            })?;
            String::from_utf8(text.as_wtf8().to_vec())
                .map_err(|_| Error::msg(format!("run.json's run_id {} is not text", text.repr())))
        }
        None => caller::py_name(run_dir).into_string().map_err(|name| {
            Error::msg(format!(
                "the run directory's name {} is not text",
                PyStr::from_os(&name).repr()
            ))
        }),
    }
}

/// The text of a JSON object file, `None` where `read_json_object` is `None`: for a typed
/// read of the whole object.
pub(crate) fn read_json_text(path: &Path) -> Result<Option<String>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(path, e)),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    let is_object = serde_json::from_str::<&RawValue>(&text)
        .is_ok_and(|raw| raw.get().trim_start().starts_with('{'));
    Ok(is_object.then_some(text))
}
