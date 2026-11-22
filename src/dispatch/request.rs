//! The worker folder's `request.json` and `result.json` (`dispatch.DispatchRequest`,
//! `request_to_json`, `request_from_json`; `_finalize_body`'s result and `_result_from`).
//!
//! Both are read and written by the Python as well (mixed mode), so the fields, their order
//! and their JSON types are the Python's. Paths are held as `pathlib` holds them (`PurePath`
//! spelling, `pytext::pure_path`) and written as `str(path)`, surrogate-escaped
//! (`wire::PyStr`); worker and Git text is `PyStr`; the closed sets are enums.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};
use crate::pytext::{path_text, pure_path};
use crate::usage::Usage;
use crate::usage::charges::null_as_default;
use crate::wire::PyStr;

/// A dispatch's `request.json` (`dispatch.DispatchRequest`), in the Python's field order.
/// `writable_dirs`, `resume`, `kind` and `branch` may be missing (or `null`), as
/// `request_from_json` allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchRequest {
    pub model: PyStr,
    pub effort: PyStr,
    pub role: String,
    #[serde(with = "path")]
    pub brief: PathBuf,
    #[serde(with = "path")]
    pub report: PathBuf,
    #[serde(with = "path")]
    pub cwd: PathBuf,
    #[serde(with = "path")]
    pub run_dir: PathBuf,
    pub card: Option<i64>,
    pub attempt: i64,
    pub dispatch_id: String,
    pub steps: i64,
    pub minutes: i64,
    pub network: bool,
    #[serde(default, with = "paths")]
    pub writable_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub resume: Option<PyStr>,
    /// The dispatch kind; `COMMIT_KINDS` get their work committed.
    #[serde(default)]
    pub kind: Option<String>,
    /// The branch the work must be committed on.
    #[serde(default)]
    pub branch: Option<PyStr>,
}

/// A worker's status, as the result records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    #[serde(rename = "READY")]
    Ready,
    #[serde(rename = "QUESTION")]
    Question,
    #[serde(rename = "BLOCKED")]
    Blocked,
    #[serde(rename = "SPENT")]
    Spent,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ready => "READY",
            Status::Question => "QUESTION",
            Status::Blocked => "BLOCKED",
            Status::Spent => "SPENT",
        }
    }
}

/// What blocked a worker (`briefs.BLOCKED_REASONS`), or nothing (`""`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockedBy {
    #[default]
    #[serde(rename = "")]
    Nothing,
    #[serde(rename = "environment")]
    Environment,
    #[serde(rename = "usage limit")]
    UsageLimit,
}

impl BlockedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedBy::Nothing => "",
            BlockedBy::Environment => "environment",
            BlockedBy::UsageLimit => "usage limit",
        }
    }
}

/// Why the worker stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Completed,
    StepLimit,
    TimeLimit,
    UsageLimit,
    Cancelled,
    Error,
    Interrupted,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Completed => "completed",
            ExitReason::StepLimit => "step_limit",
            ExitReason::TimeLimit => "time_limit",
            ExitReason::UsageLimit => "usage_limit",
            ExitReason::Cancelled => "cancelled",
            ExitReason::Error => "error",
            ExitReason::Interrupted => "interrupted",
        }
    }
}

/// A process as the worker folder records it (`runner.json`, `procs.json`) and `result.json`
/// lists it. Codex's own record may have no start time (`null`), when it could not be read at
/// launch. Everything leadhand records has a group; a record read without one (a hand-written
/// or foreign file) is still judged by its pid and start time, and its group is never signalled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedProc {
    pub pid: i32,
    #[serde(default)]
    pub pgid: Option<i32>,
    pub start: Option<String>,
}

impl StoppedProc {
    /// The identity it is checked under: its pid and start time, when it has one.
    pub fn id(&self) -> Option<crate::procs::ProcId> {
        self.start.clone().map(|start| crate::procs::ProcId {
            pid: self.pid,
            start,
        })
    }

    /// Whether the recorded process is alive with its recorded start time
    /// (`procs.same_process`).
    pub fn alive(&self) -> bool {
        crate::procs::same_process(Some(self.pid), self.start.as_deref())
    }
}

/// A dispatch's `result.json`, in the field order `_finalize_body` writes, with the runner's
/// commit outcome (`_commit_work`) last. `status` and `exit_reason` are required, as
/// `_result_from` requires them; every other field reads as its default when missing, and
/// `blocked_by`, `reply` and `steps` also when `null` (`d.get(k) or default`). The commit
/// fields that `_commit_work` writes only sometimes are left out when `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DispatchResult {
    #[serde(default)]
    pub dispatch_id: String,
    pub status: Status,
    #[serde(default, deserialize_with = "null_as_default")]
    pub blocked_by: BlockedBy,
    /// The Codex thread ID (`thread.started.thread_id`), format-checked by `thread_id`.
    #[serde(default, deserialize_with = "session")]
    pub session_id: Option<String>,
    /// The worker's reply.
    #[serde(default, deserialize_with = "null_as_default")]
    pub reply: PyStr,
    #[serde(default, deserialize_with = "null_as_default")]
    pub steps: u32,
    /// `null` when the usage is not known.
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub usage_known: bool,
    /// `none`, `unknown`, `stream` or `session`.
    #[serde(default)]
    pub usage_source: String,
    #[serde(default)]
    pub possibly_incomplete: bool,
    pub exit_reason: ExitReason,
    #[serde(default)]
    pub codex_exit: Option<i32>,
    #[serde(default)]
    pub charge_ids: Vec<PyStr>,
    #[serde(default)]
    pub generation: u32,
    #[serde(default)]
    pub stopped: Vec<StoppedProc>,
    #[serde(default)]
    pub confirmed_at: f64,
    #[serde(default)]
    pub cancel_token: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    /// The commit the runner made for the worker, if any.
    #[serde(default)]
    pub committed: Option<String>,
    #[serde(default)]
    pub changes: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_skipped: Option<PyStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_error: Option<PyStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitlinks_skipped: Option<Vec<PyStr>>,
    /// The worker's own status and exit reason, when a failed commit replaced them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_exit_reason: Option<ExitReason>,
}

/// A Codex thread ID, kept as `String`: any text the stream names, as the Python accepts
/// any `str` there. `None` for a string `String` cannot hold (one with a lone surrogate),
/// which no Codex thread ID is.
pub fn thread_id(text: &PyStr) -> Option<String> {
    String::from_utf8(text.as_wtf8().to_vec()).ok()
}

/// `session_id`: `null`, or a thread ID that passes `thread_id`.
fn session<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    match Option::<PyStr>::deserialize(d)? {
        None => Ok(None),
        Some(text) => thread_id(&text).map(Some).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "session_id {} is not a Codex thread ID",
                text.repr()
            ))
        }),
    }
}

/// The worker folder's `request.json`: `None` where `dispatch._read_json` gives `None`
/// (missing, not UTF-8, not a JSON object); an error when its fields do not fit.
pub fn read_request(worker_dir: &Path) -> Result<Option<DispatchRequest>> {
    read_typed(&worker_dir.join(super::REQUEST))
}

/// The worker folder's `result.json` (`dispatch.load_result`), `None` the same way, and also
/// for an empty object (`_result_from(d) if d else None`).
pub fn load_result(worker_dir: &Path) -> Result<Option<DispatchResult>> {
    let file = worker_dir.join(super::RESULT);
    let Some(text) = super::read_json_text(&file)? else {
        return Ok(None);
    };
    let empty = serde_json::from_str::<crate::wire::PyMap<serde::de::IgnoredAny>>(&text)
        .is_ok_and(|map| map.is_empty());
    if empty {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| Error::msg(format!("{}: {e}", file.display())))
}

fn read_typed<T: serde::de::DeserializeOwned>(file: &Path) -> Result<Option<T>> {
    let Some(text) = super::read_json_text(file)? else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| Error::msg(format!("{}: {e}", file.display())))
}

/// `Path(text)`: the text's OS bytes (surrogate escapes become their bytes), as `pathlib`
/// spells them.
fn path_from<E: serde::de::Error>(text: &PyStr) -> std::result::Result<PathBuf, E> {
    let os = text.to_os().map_err(|e| E::custom(e.message))?;
    Ok(pure_path(&os))
}

/// A path field: written as `str(path)`, read as `Path(text)`.
mod path {
    use super::*;

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> std::result::Result<S::Ok, S::Error> {
        path_text(p).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<PathBuf, D::Error> {
        path_from(&PyStr::deserialize(d)?)
    }
}

/// A list of paths, read as `tuple(Path(p) for p in d.get(key) or ())`.
mod paths {
    use super::*;

    pub fn serialize<S: Serializer>(ps: &[PathBuf], s: S) -> std::result::Result<S::Ok, S::Error> {
        let texts: Vec<PyStr> = ps.iter().map(|p| path_text(p)).collect();
        texts.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Vec<PathBuf>, D::Error> {
        Option::<Vec<PyStr>>::deserialize(d)?
            .unwrap_or_default()
            .iter()
            .map(path_from)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;
    use crate::wire::tests::{reference_json, reference_outputs};
    use crate::wire::{to_line, to_output, to_pretty};

    /// The reference fixture of the tests here: the documents the reference wrote, and what it
    /// read.
    const FIXTURE: &str = "dispatch/request.json";

    /// The documents of `section`, as the reference wrote them.
    fn reference_texts(section: &str) -> Vec<String> {
        reference_json(FIXTURE)[section]
            .as_array()
            .unwrap_or_else(|| panic!("{FIXTURE}: no section {section:?}"))
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn request_json_round_trips_with_python() {
        // Python's `request_to_json` of requests it builds, as `write_json_atomic` writes them.
        let texts = reference_texts("requests");
        assert_eq!(texts.len(), 2);
        let mut lines = Vec::new();
        for text in &texts {
            let req: DispatchRequest = serde_json::from_str(text).unwrap();
            // Written back exactly as the Python writes it.
            assert_eq!(&to_pretty(&req).unwrap(), text);
            let ours = to_line(&req).unwrap();
            lines.push(serde_json::json!([ours, text]));
        }
        // And the Python reads what Rust writes as the request it wrote: the reference compared
        // its reading of these very lines (the recorded inputs are checked to be them) with its
        // reading of its own text.
        for same in reference_outputs(FIXTURE, "request_read_back", &lines) {
            assert_eq!(same, "True\n");
        }
        let second: DispatchRequest = serde_json::from_str(&texts[1]).unwrap();
        assert_eq!(second.cwd.as_os_str().as_bytes(), b"/w s/\xff");
        assert_eq!(second.brief.as_os_str().as_bytes(), b"/w s/\xff/brief.md");
        assert_eq!(second.run_dir, Path::new("//runs/r 2"));
        assert_eq!(
            second.branch.unwrap().to_os().unwrap().as_bytes(),
            b"lh/r\xff"
        );
        assert_eq!(second.card, None);
    }

    #[test]
    fn request_reads_paths_and_optional_fields_as_python() {
        // Hand-written requests: unnormalized paths, the optional fields missing or null.
        let docs = [
            r#"{"model": "m", "effort": "e", "role": "reviewer", "brief": "/w//.leadhand/./b.md",
               "report": "rel/r.md/", "cwd": "/w/", "run_dir": "", "card": 2, "attempt": 1,
               "dispatch_id": "c2-a1-review", "steps": 1, "minutes": 2, "network": false}"#,
            r#"{"model": "m", "effort": "e", "role": "verifier", "brief": "/b", "report": "/r",
               "cwd": "/w", "run_dir": "/r", "card": null, "attempt": 1, "dispatch_id": "v",
               "steps": 1, "minutes": 2, "network": true, "writable_dirs": null,
               "resume": null, "kind": null, "branch": null, "unknown": [1]}"#,
        ];
        // Python's `request_to_json(request_from_json(doc))`, as `write_json_atomic` writes it.
        let inputs: Vec<serde_json::Value> = docs.iter().map(|d| (*d).into()).collect();
        let theirs = reference_outputs(FIXTURE, "request_reads", &inputs);
        for (doc, want) in docs.iter().zip(theirs) {
            let req: DispatchRequest = serde_json::from_str(doc).unwrap();
            assert_eq!(to_pretty(&req).unwrap(), want, "{doc}");
        }
    }

    /// `result.json` as `_finalize_body` writes it, in its key order, for each shape of the
    /// commit outcome `_commit_work` returns: the reference's documents.
    #[test]
    fn result_json_round_trips_with_python() {
        let texts = reference_texts("results");
        assert_eq!(texts.len(), 6);
        for text in &texts {
            let result: DispatchResult = serde_json::from_str(text).unwrap();
            assert_eq!(&to_pretty(&result).unwrap(), text);
        }
        let blocked: DispatchResult = serde_json::from_str(&texts[4]).unwrap();
        assert_eq!(blocked.status, Status::Blocked);
        assert_eq!(blocked.blocked_by, BlockedBy::Environment);
        assert_eq!(blocked.worker_status, Some(Status::Question));
        assert_eq!(blocked.exit_reason, ExitReason::Error);
        assert!(blocked.reply.as_wtf8().ends_with("\u{1f600}".as_bytes()));
        let spent: DispatchResult = serde_json::from_str(&texts[2]).unwrap();
        assert_eq!(
            (spent.session_id, spent.usage, spent.codex_exit),
            (None, None, Some(-15))
        );
        assert_eq!(spent.stopped[0].start, None);
        let limited: DispatchResult = serde_json::from_str(&texts[5]).unwrap();
        assert_eq!(limited.blocked_by, BlockedBy::UsageLimit);
        assert_eq!(limited.exit_reason, ExitReason::UsageLimit);
    }

    #[test]
    fn result_reads_as_python_result_from_reads() {
        // `_result_from`'s defaults: `blocked_by`, `reply` and `steps` null or missing.
        let docs = [
            r#"{"status": "QUESTION", "exit_reason": "completed"}"#,
            r#"{"status": "READY", "exit_reason": "completed", "blocked_by": null,
               "reply": null, "steps": null, "usage": null, "session_id": null}"#,
            r#"{"status": "SPENT", "exit_reason": "time_limit", "steps": 12, "reply": "r\udcff",
               "session_id": "t-1", "usage": {"input": 1, "known": true}}"#,
        ];
        // The reference's `_result_from` of each: `[status, blocked_by, session_id, reply,
        // steps, usage_dict(usage), exit_reason]`, as JSON.
        let inputs: Vec<serde_json::Value> = docs.iter().map(|d| (*d).into()).collect();
        let out = reference_outputs(FIXTURE, "result_reads", &inputs);
        let theirs: Vec<&str> = out.iter().map(|o| o.as_str().unwrap()).collect();
        assert_eq!(theirs.len(), docs.len());
        for (doc, want) in docs.iter().zip(theirs) {
            let r: DispatchResult = serde_json::from_str(doc).unwrap();
            let ours = (
                r.status,
                r.blocked_by,
                &r.session_id,
                &r.reply,
                r.steps,
                &r.usage,
                r.exit_reason,
            );
            assert_eq!(to_output(&ours).unwrap(), want, "{doc}");
        }
    }

    #[test]
    fn result_without_status_or_exit_reason_is_an_error() {
        // `_result_from` raises KeyError for both.
        for doc in [r#"{"exit_reason": "completed"}"#, r#"{"status": "READY"}"#] {
            assert!(
                serde_json::from_str::<DispatchResult>(doc).is_err(),
                "{doc}"
            );
        }
    }

    #[test]
    fn thread_id_keeps_any_text_a_string_can_hold() {
        assert_eq!(
            thread_id(&PyStr::from("0f0f0f0f-0000-4000-8000-00000000c0de")).as_deref(),
            Some("0f0f0f0f-0000-4000-8000-00000000c0de")
        );
        assert_eq!(
            thread_id(&PyStr::from("caf\u{e9} -x")).as_deref(),
            Some("caf\u{e9} -x")
        );
        let lone: PyStr = serde_json::from_str(r#""t\ud800""#).unwrap();
        assert_eq!(thread_id(&lone), None);
        // A result naming such a session is not read.
        let doc = r#"{"status": "READY", "exit_reason": "completed", "session_id": "t\udcff"}"#;
        assert!(serde_json::from_str::<DispatchResult>(doc).is_err());
    }

    #[test]
    fn worker_files_read_as_read_json_reads_them() {
        let tmp = tempfile::tempdir().unwrap();
        let wd = tmp.path();
        assert_eq!(read_request(wd).unwrap(), None);
        assert_eq!(load_result(wd).unwrap(), None);
        for bad in [
            &b"{"[..],
            b"[1, 2]",
            b"\"text\"",
            b"{\"status\": \"READY\xff\"}",
        ] {
            std::fs::write(wd.join("result.json"), bad).unwrap();
            assert_eq!(load_result(wd).unwrap(), None, "{bad:?}");
            std::fs::write(wd.join("request.json"), bad).unwrap();
            assert_eq!(read_request(wd).unwrap(), None, "{bad:?}");
        }
        std::fs::write(
            wd.join("result.json"),
            b"  {\"status\": \"READY\", \"exit_reason\": \"completed\", \"steps\": 2}\n",
        )
        .unwrap();
        assert_eq!(load_result(wd).unwrap().unwrap().steps, 2);
        // An empty object is no result (`_result_from(d) if d else None`); an object without
        // a status is still an error, as `_result_from`'s KeyError.
        for empty in [&b"{}"[..], b" { }\n"] {
            std::fs::write(wd.join("result.json"), empty).unwrap();
            assert_eq!(load_result(wd).unwrap(), None, "{empty:?}");
        }
        std::fs::write(wd.join("result.json"), b"{\"steps\": 1}").unwrap();
        assert!(load_result(wd).is_err());
        // An object that does not fit is an error, as `request_from_json` raises.
        std::fs::write(wd.join("request.json"), b"{\"model\": \"m\"}").unwrap();
        assert!(read_request(wd).is_err());
        // A directory where the file should be: Python's IsADirectoryError.
        std::fs::remove_file(wd.join("result.json")).unwrap();
        std::fs::create_dir(wd.join("result.json")).unwrap();
        assert!(load_result(wd).is_err());
    }
}
