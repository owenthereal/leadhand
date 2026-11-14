//! Finalizing a dispatch (`dispatch._finalize`, `_finalize_body`, `_charge_usage`,
//! `_commit_work`'s first rule, `commit_failed`): classify it, commit a commit kind's work,
//! charge the usage, record the attempt and write `result.json`, last.
//!
//! The runner finalizes its own dispatch; a cancel or a `dispatch` that finds the runner dead
//! finalizes it for it. Everything is read back from the worker folder, as the Python reads
//! it, so whoever finalizes reaches the same result.

use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use rustix::fs::FlockOperation;
use serde_json::value::RawValue;

use super::request::StoppedProc;
use super::runner::{Finish, HEARTBEAT_EVERY, StreamSummary, classify, possibly_incomplete};
use super::{
    BlockedBy, CANCEL, COMMIT_KINDS, DispatchRequest, DispatchResult, ExitReason, FINALIZE_LOCK,
    LAST, RAW, RESULT, RUNNER, STDERR, STREAM, Status, load_result, open_lock_file, read_from,
    read_json_object, read_text, run_id_of, touch, truthy, wall_now, write_text,
};
use crate::error::{Error, Result};
use crate::pytext::py_strip;
use crate::state::{AttemptRecord, CardRef, append_attempt, read_attempts, run_paths, utc_now};
use crate::store::{state_lock, write_json_atomic};
use crate::testing::crash_point;
use crate::usage::{
    CODEX_STREAM_SOURCE, ChargeSource, Usage, charge, default_sessions_dir, estimate_cost,
};
use crate::wire::{PyMap, PyStr};

// --------------------------------------------------------------------------
// Committing the worker's work

/// What committing the worker's work came to (`_commit_work`'s outcome).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitOutcome {
    /// The commit made for the worker.
    pub committed: Option<String>,
    /// Whether the worker left changes.
    pub changes: bool,
    pub commit_skipped: Option<PyStr>,
    pub commit_error: Option<PyStr>,
    pub gitlinks_skipped: Option<Vec<PyStr>>,
}

/// The runner's commit of a commit kind's work (R39), supplied by the caller: called only for
/// a launched dispatch whose kind is one of `COMMIT_KINDS` with a branch. `deadline` bounds
/// every Git call, for a cancel.
pub trait Commits {
    fn commit_work(
        &self,
        wd: &Path,
        req: &DispatchRequest,
        deadline: Option<Instant>,
    ) -> CommitOutcome;
}

/// No commit: a commit kind's work is reported as uncommitted changes, so its result can never
/// pass for finished work.
pub struct NoCommit;

impl Commits for NoCommit {
    fn commit_work(&self, _: &Path, _: &DispatchRequest, _: Option<Instant>) -> CommitOutcome {
        CommitOutcome {
            changes: true,
            commit_error: Some("the runner's commits are not available in this build".into()),
            ..CommitOutcome::default()
        }
    }
}

/// `commit_failed`: why a dispatch's changes were left uncommitted, or `None`. A result whose
/// worker left changes the runner could not commit must not pass for finished work.
pub fn commit_failed(commit: &CommitOutcome) -> Option<PyStr> {
    if commit.committed.as_deref().is_some_and(|c| !c.is_empty()) || !commit.changes {
        return None;
    }
    let given = |text: &Option<PyStr>| text.clone().filter(|t| !t.is_empty());
    given(&commit.commit_error).or_else(|| given(&commit.commit_skipped))
}

/// `_commit_work`'s first rule, then the caller's commit.
fn commit_work(
    wd: &Path,
    req: &DispatchRequest,
    deadline: Option<Instant>,
    commits: &dyn Commits,
) -> CommitOutcome {
    let kind = req.kind.as_deref().filter(|k| !k.is_empty());
    let commits_kind = kind.is_some_and(|k| COMMIT_KINDS.contains(&k));
    if !commits_kind || req.branch.as_ref().is_none_or(PyStr::is_empty) {
        return CommitOutcome {
            commit_skipped: Some(
                format!("{} dispatch does not commit", kind.unwrap_or("this")).into(),
            ),
            ..CommitOutcome::default()
        };
    }
    commits.commit_work(wd, req, deadline)
}

// --------------------------------------------------------------------------
// Finalizing: classify, commit, charge, record the attempt, write result.json

/// The heartbeat, touched every second while a block runs (`_beating`): committing and
/// charging can take a while, and a stale heartbeat reads as hung.
struct Beating {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Beating {
    fn start(beat: &Path) -> Beating {
        let _ = touch(beat);
        let (tx, rx) = mpsc::channel::<()>();
        let beat = beat.to_path_buf();
        let thread = thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(HEARTBEAT_EVERY) {
                let _ = touch(&beat);
            }
        });
        Beating {
            stop: Some(tx),
            thread: Some(thread),
        }
    }
}

impl Drop for Beating {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What `runner.json` says about the generation, for finalizing.
struct RunnerFacts {
    generation: u32,
    /// Two-phase launch: an intent without a recorded Codex may still have started one.
    launched: bool,
}

/// `runner.json` as `_finalize_body` reads it: `generation` (1 when missing or false),
/// `codex`, and `launching` with its `aborted`, each as Python's truth tests read them.
fn runner_facts(wd: &Path) -> Result<RunnerFacts> {
    let runner = read_json_object(&wd.join(RUNNER))?.unwrap_or_default();
    let field = |key: &str| runner.get(&PyStr::from(key)).filter(|raw| truthy(raw));
    let generation = match field("generation") {
        None => 1,
        Some(raw) => match raw.get().trim() {
            "true" => 1,
            text => serde_json::from_str::<u32>(text).map_err(|_| {
                Error::msg(format!(
                    "runner.json: generation {text} is not a generation"
                ))
            })?,
        },
    };
    let launched = field("codex").is_some()
        || match field("launching") {
            None => false,
            Some(raw) => {
                let launching: PyMap<Box<RawValue>> = serde_json::from_str(raw.get())
                    .map_err(|_| Error::msg("runner.json: launching is not an object"))?;
                !launching
                    .get(&PyStr::from("aborted"))
                    .is_some_and(|a| truthy(a))
            }
        };
    Ok(RunnerFacts {
        generation,
        launched,
    })
}

/// The cancel request's token, when it is a string.
fn cancel_token(wd: &Path) -> Result<Option<String>> {
    let cancel = read_json_object(&wd.join(CANCEL))?.unwrap_or_default();
    Ok(cancel
        .get(&PyStr::from("token"))
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok()))
}

/// `_charge_usage`: (usage, charge IDs, source). The session file first, then the stream,
/// else unknown; nothing when Codex never ran.
fn charge_usage(
    wd: &Path,
    req: &DispatchRequest,
    thread: Option<&str>,
    launched: bool,
    generation: u32,
    incomplete: bool,
) -> Result<(Usage, Vec<PyStr>, &'static str)> {
    if !launched {
        return Ok((Usage::default(), Vec::new(), "none"));
    }
    let paths = run_paths(&req.run_dir);
    let suffix = if generation > 1 {
        format!(":g{generation}")
    } else {
        String::new()
    };
    let did = &req.dispatch_id;
    let (charge_id, key) = match thread.filter(|t| !t.is_empty()) {
        Some(t) => (format!("{did}:codex:{t}{suffix}"), t.to_string()),
        None => (
            format!("{did}:codex:unknown{suffix}"),
            format!("dispatch:{did}"),
        ),
    };
    let charge_id = PyStr::from(charge_id);
    let thread = thread.map(PyStr::from);
    let source = ChargeSource::Codex {
        thread: thread.as_ref(),
        sessions_dir: &default_sessions_dir(),
        stream: Some(&wd.join(STREAM)),
    };
    let c = charge(
        &paths,
        &charge_id,
        &PyStr::from(key),
        did,
        &req.role,
        source,
        incomplete,
    )?;
    if !c.usage.known {
        return Ok((Usage::unknown(), vec![charge_id], "unknown"));
    }
    let source = if c.source == CODEX_STREAM_SOURCE {
        "stream"
    } else {
        "session"
    };
    Ok((c.usage, vec![charge_id], source))
}

/// Finalize a dispatch (`_finalize`): classify it, commit a commit kind's work, charge the
/// usage, record the attempt and write `result.json`, under `finalize.lock`. A result already
/// there is returned as it is. With `beat`, the heartbeat is touched every second meanwhile.
///
/// S2-R32: a reply head the Python's `parse_report_head` raises `KeyError` on is an error
/// here, before anything is written, charged or recorded, as the Python's exception escapes.
#[allow(clippy::too_many_arguments)]
pub fn finalize(
    wd: &Path,
    req: &DispatchRequest,
    finish: Finish,
    stopped: &[StoppedProc],
    started_at: Option<&str>,
    deadline: Option<Instant>,
    beat: Option<&Path>,
    commits: &dyn Commits,
) -> Result<DispatchResult> {
    let _beating = beat.map(Beating::start);
    let lock_path = wd.join(FINALIZE_LOCK);
    let lock = open_lock_file(&lock_path)?;
    rustix::fs::flock(&lock, FlockOperation::LockExclusive)
        .map_err(|e| Error::io(&lock_path, e.into()))?;
    if let Some(existing) = load_result(wd)? {
        return Ok(existing);
    }
    let run_id = run_id_of(&req.run_dir)?;
    let runner = runner_facts(wd)?;
    let stream = StreamSummary::of(&read_from(&wd.join(STREAM), 0)?);
    let mut reply = read_text(&wd.join(LAST))?.unwrap_or_default();
    if py_strip(&reply.lossy()).is_empty() {
        reply = stream.last_agent_message.clone().unwrap_or_default();
    }
    let stderr = read_text(&wd.join(STDERR))?.unwrap_or_default();
    let classified = classify(
        finish.stopped_for,
        finish.codex_exit,
        &reply.lossy(),
        &stream.errors,
        &stderr.lossy(),
    )?;
    let (mut status, mut blocked_by, mut exit_reason) = (
        classified.status,
        classified.blocked_by,
        classified.exit_reason,
    );
    if !runner.launched && finish.stopped_for.is_none() {
        // Codex could not be started at all.
        blocked_by = BlockedBy::Environment;
    }
    if classified.keep_raw {
        write_text(&wd.join(RAW), &reply)?;
    }
    let incomplete = possibly_incomplete(
        runner.launched,
        finish.stopped_for,
        finish.codex_exit,
        stream.turn_open,
    );
    let mut commit = if runner.launched {
        commit_work(wd, req, deadline, commits)
    } else {
        CommitOutcome {
            commit_skipped: Some("Codex never ran".into()),
            ..CommitOutcome::default()
        }
    };
    let mut worker = None;
    if let Some(failed) = commit_failed(&commit) {
        // The work is still in the tree but not on the branch: an environment problem.
        let mut text = PyStr::from(
            "Status: BLOCKED\nBlocked by: environment\nleadhand could not commit the worker's \
             changes on ",
        );
        text.push(&req.branch.clone().unwrap_or_else(|| PyStr::from("None")));
        text.push_str(": ");
        text.push(&failed);
        if !py_strip(&reply.lossy()).is_empty() {
            text.push_str(&format!("\n\nWorker reply ({}):\n", exit_reason.as_str()));
            text.push(&reply);
        }
        reply = text;
        worker = Some((status, exit_reason));
        (status, blocked_by, exit_reason) =
            (Status::Blocked, BlockedBy::Environment, ExitReason::Error);
    }
    let (usage, charge_ids, source) = charge_usage(
        wd,
        req,
        stream.thread_id.as_deref(),
        runner.launched,
        runner.generation,
        incomplete,
    )?;
    crash_point("after_charge_before_attempt");
    let ended_at = utc_now();
    // Codex models are not priced: an empty table suffices.
    let cost = if usage.known {
        estimate_cost(&usage, Some(&req.model), &[])
    } else {
        None
    };
    let known = usage.known.then(|| usage.clone());
    let attempt = AttemptRecord {
        dispatch: Some(req.dispatch_id.clone()),
        run: Some(run_id),
        card: req.card.map(CardRef::Number),
        attempt: Some(req.attempt),
        role: Some(req.role.clone()),
        vendor: Some("codex".into()),
        model: Some(req.model.clone()),
        effort: Some(req.effort.clone()),
        agent: None,
        worker_id: stream.thread_id.as_deref().map(PyStr::from),
        started_at: Some(started_at.unwrap_or(&ended_at).to_string()),
        ended_at: Some(ended_at.clone()),
        steps: Some(i64::from(stream.steps)),
        charge_ids: charge_ids.clone(),
        usage: known.clone(),
        usage_known: Some(usage.known),
        usage_source: Some(source.to_string()),
        cost_estimate: cost,
        exit_reason: Some(exit_reason.as_str().to_string()),
        status: Some(status.as_str().to_string()),
        blocked_by: Some(blocked_by.as_str().to_string()),
        recorded_by: None,
        generation: Some(i64::from(runner.generation)),
        possibly_incomplete: Some(incomplete),
    };
    let paths = run_paths(&req.run_dir);
    {
        let _state = state_lock(&paths.root)?;
        let generation = i64::from(runner.generation);
        let recorded = read_attempts(&paths)?.iter().any(|a| {
            a.dispatch.as_deref() == Some(&req.dispatch_id)
                && a.generation.unwrap_or(1) == generation
        });
        if !recorded {
            append_attempt(&paths, &attempt)?;
        }
    }
    let mut stopped = stopped.to_vec();
    stopped.sort_by_key(|p| p.pid);
    let cancel_token = match exit_reason {
        ExitReason::Cancelled => cancel_token(wd)?,
        _ => None,
    };
    let result = DispatchResult {
        dispatch_id: req.dispatch_id.clone(),
        status,
        blocked_by,
        session_id: stream.thread_id.clone(),
        reply,
        steps: stream.steps,
        usage: known,
        usage_known: usage.known,
        usage_source: source.to_string(),
        possibly_incomplete: incomplete,
        exit_reason,
        codex_exit: finish.codex_exit,
        charge_ids,
        generation: runner.generation,
        stopped,
        confirmed_at: wall_now(),
        cancel_token,
        started_at: started_at.map(str::to_string),
        ended_at: Some(ended_at),
        committed: commit.committed.take(),
        changes: commit.changes,
        commit_skipped: commit.commit_skipped.take(),
        commit_error: commit.commit_error.take(),
        gitlinks_skipped: commit.gitlinks_skipped.take(),
        worker_status: worker.map(|(s, _)| s),
        worker_exit_reason: worker.map(|(_, e)| e),
    };
    write_json_atomic(&wd.join(RESULT), &result)?;
    Ok(result)
}
