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
//! - `shell`: the runner's threads and I/O around it, its process tracker, and finalizing.

pub mod argv;
pub mod caller;
pub mod env;
pub mod request;
pub mod runner;
pub mod shell;

use std::path::Path;

use serde_json::value::RawValue;

use crate::error::{Error, Result};
use crate::wire::PyMap;

pub use argv::{codex_argv, codex_argv_parts, toml_string};
pub use caller::{
    Anchor, anchored_run, anchored_worker, base_worktree_path, check_caller, check_dispatch_id,
    check_request, evidence_dirs, runner_worker, worker_dir,
};
pub use env::{
    ASKPASS_RESET, BLOCKING_ENV, CREDENTIAL_HELPER_RESET, CREDENTIAL_VARS, Env, TAG_VAR,
    blocking_env, codex_overrides, worker_env,
};
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

/// A JSON object file as `dispatch._read_json` reads it: `None` when the file is missing,
/// not UTF-8, not JSON, or not an object; its entries otherwise (a repeated key keeps its
/// last value, as `json.loads` does). Any other I/O error is an error.
pub fn read_json_object(path: &Path) -> Result<Option<PyMap<Box<RawValue>>>> {
    Ok(read_json_text(path)?.and_then(|text| serde_json::from_str(&text).ok()))
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
