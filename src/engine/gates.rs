//! The end-of-run gates' I/O (`engine.py`: `_request_run`, `_start_wave`'s cut, `_recut_final`,
//! `_after_run_result`'s gate record and fix acceptance, `_base_worktree`): what the shell
//! carries out for `decide`, which decides which gate, wave or re-run is due (`next::gates`,
//! `next::run_level`).
//!
//! Every Git call runs in the run's worktree (`Run::git`), or in the repository for the
//! evidence base worktree's preparation (`git::base_worktree`). The writes are: the checkout
//! of the run branch or the final branch in the run's worktree; the final branch
//! `<run branch>--card-final`, cut and re-cut with a compare-and-swap `update-ref` (created
//! only when missing); the run-scoped refs `refs/leadhand/<run id>/final/<wave>` and
//! `…/violations/<n>-final` that keep a fix wave's unaccepted commits (plain `update-ref`,
//! S2-R47); the run branch's fast-forward to a clean fix; and the evidence base worktree.
//!
//! Crash points sit at the Python's positions (S2-R34): `before_gate_record` before a gate's
//! result is recorded; `before_fast_forward` and `after_fast_forward` around a fix's
//! acceptance.

use std::ffi::OsStr;
use std::path::PathBuf;

use super::cards::{Dispatch, Log, Run, final_branch, heads, os};
use super::next::{RunRequest, RunState};
use super::protocol::{FINAL, RUN_CHECK_KINDS};
use super::shell::{Guard, NewDispatch, append_request, check_out};
use super::violations::{ensure_clean, run_id};
use crate::briefs_write::{BriefSpec, write_brief};
use crate::dispatch::base_worktree_path as dispatch_base_path;
use crate::error::{Error, Result};
use crate::git::Git;
use crate::git::base_worktree::{BaseWorktree, Record, prepare};
use crate::git::cards::{branch_ref, fast_forward, ref_snapshot};
use crate::git::untracked_files;
use crate::git::workspace::head;
use crate::pytext::{path_text, pure_path};
use crate::state::events::{self, given};
use crate::state::{CardRef, Event, append_event, two_phase, two_phase_resume};
use crate::testing::crash_point;
use crate::wire::{PyMap, PyStr};

fn final_card() -> CardRef {
    CardRef::Text(FINAL.into())
}

// --------------------------------------------------------------------------
// Requests

/// Writes a run-level brief and appends its `dispatch_requested`, card `None`
/// (`_request_run`). A gate runs on the run branch, a wave's implementer and review on the
/// final branch. Checks are guarded like card checks (R14'); a wave's implementer like an
/// integrate worker (it commits on the final branch, but must leave the run branch alone); an
/// evidence dispatch, first run or re-run, starts from a freshly reset base worktree; a gate
/// also records `gate_started` with the commit it examines. Deterministic, as a card's request.
pub(super) fn request_run(st: &RunState, r: &RunRequest) -> Result<()> {
    let run = st.run;
    let branch = if r.wave.is_none() {
        run.branch.clone()
    } else {
        final_branch(run)
    };
    check_out(st, None, &branch)?;
    let mut guard = Guard::default();
    if RUN_CHECK_KINDS.contains(&r.kind.as_str()) {
        let why = PyStr::from(format!("before {} {}", r.kind, r.dispatch_id));
        ensure_clean(run, &branch, &why, None, false, false)?;
        let git = run.git()?;
        let expected_head = head(&git, &branch_ref(&branch)?)?;
        let snap = ref_snapshot(&git, &run.branch, run_id(run)?)?;
        guard.expected_branch = Some(Some(branch.clone()));
        guard.expected_head = Some(Some(expected_head));
        guard.refs = Some(Some(PyMap(
            snap.0.into_iter().map(|(k, v)| (k, Some(v))).collect(),
        )));
    } else if r.kind == "final_fix" {
        let git = run.git()?;
        let run_ref = run.run_ref();
        let run_tip = head(&git, &run_ref.to_os()?)?;
        guard.refs = Some(Some(PyMap(vec![(run_ref.clone(), Some(run_tip))])));
        guard.ref_scope = Some(Some(vec![run_ref]));
        guard.start_tip = Some(Some(head(&git, &branch_ref(&branch)?)?));
        guard.untracked = Some(Some(untracked_files(&run.ws).map_err(Error::from)?));
    }
    let mut extra = r.extra.clone();
    if r.kind == "evidence" {
        let path = base_worktree(run)?;
        *extra.entry(&PyStr::from("base_worktree"), || None) = Some(path_text(&path));
    }
    let cards: Vec<String> = r.cards.iter().map(u32::to_string).collect();
    let mut full = PyMap(vec![
        (
            PyStr::from("base"),
            Some(PyStr::from(run.base_sha.as_str())),
        ),
        (PyStr::from("branch"), Some(run.branch.clone())),
        (
            PyStr::from("test_first"),
            Some(PyStr::from(run.config.run.test_first.as_str())),
        ),
        (PyStr::from("cards"), Some(PyStr::from(cards.join(",")))),
    ]);
    for (k, v) in &extra.0 {
        *full.entry(k, || None) = v.clone();
    }
    if let Some(gate) = &r.gate {
        let started = st.log.events.iter().any(|e| {
            matches!(e, Event::GateStarted(g) if g.dispatch_id.as_deref() == Some(&r.dispatch_id))
        });
        if !started {
            let Some(examined) = given(&guard.expected_head).cloned() else {
                return Err(Error::msg("KeyError: 'expected_head'"));
            };
            append_event(
                &run.paths,
                Event::GateStarted(events::GateStarted {
                    gate: Some(gate.clone()),
                    examined_sha: Some(examined),
                    dispatch_id: Some(r.dispatch_id.clone()),
                    ..events::GateStarted::default()
                }),
            )?;
        }
    }
    let (brief, report) = write_brief(&BriefSpec {
        kind: &r.kind,
        ws: &run.ws,
        deck: &run.deck,
        card: None,
        attempt: r.attempt,
        dispatch_id: &r.dispatch_id,
        extra: &full,
        notes: &r.notes,
    })?;
    let new = NewDispatch {
        dispatch_id: &r.dispatch_id,
        card: None,
        attempt: r.attempt,
        kind: &r.kind,
        role: &r.role,
        route: &r.route,
        extra: &extra,
        notes: &r.notes,
        resume: None,
        gate: Some(r.gate.clone()),
        wave: Some(r.wave),
    };
    append_request(st, &new, &brief, &report, guard)
}

// --------------------------------------------------------------------------
// The final branch

/// A ref's commit, `None` when there is none (`_ref_value`: `for-each-ref
/// --format=%(objectname) <ref>`).
fn ref_value(git: &Git, name: &PyStr) -> Result<Option<String>> {
    let out = git.run(&[
        os("for-each-ref"),
        os("--format=%(objectname)"),
        &name.to_os()?,
    ])?;
    let sha = String::from_utf8_lossy(out.trim_ascii()).into_owned();
    Ok((!sha.is_empty()).then_some(sha))
}

/// Moves the final branch from `old` to `to` (compare-and-swap; created only when it is
/// missing), after keeping `old` on `keep`.
fn move_final(
    git: &Git,
    name: &PyStr,
    old: Option<&str>,
    to: &str,
    keep: (&str, String),
    message: &str,
) -> Result<()> {
    if let Some(old) = old {
        git.run(&[os("update-ref"), os("-m"), os(keep.0), os(&keep.1), os(old)])?;
    }
    git.run(&[
        os("update-ref"),
        os("-m"),
        os(message),
        &name.to_os()?,
        os(to),
        os(old.unwrap_or_default()),
    ])?;
    Ok(())
}

/// Cuts the final branch for fix wave `wave` at the run head `head` (`_start_wave`'s cut),
/// keeping an earlier wave's unaccepted commits on `refs/leadhand/<run id>/final/<wave - 1>`;
/// records `card_cut{card: "final", wave}`. The workspace leaves the final branch first.
pub(super) fn cut_final(st: &RunState, wave: i64, head_sha: &str) -> Result<()> {
    let run = st.run;
    let branch = final_branch(run);
    check_out(st, None, &run.branch)?;
    let git = run.git()?;
    let name = heads(&branch);
    let old = ref_value(&git, &name)?;
    if old.as_deref() != Some(head_sha) {
        let keep = format!("refs/leadhand/{}/final/{}", run_id(run)?, wave - 1);
        move_final(
            &git,
            &name,
            old.as_deref(),
            head_sha,
            ("leadhand: keep an unaccepted fix wave", keep),
            "leadhand: cut the final fix branch",
        )?;
    }
    append_event(
        &run.paths,
        Event::CardCut(events::CardCut {
            card: Some(final_card()),
            branch: Some(branch),
            base: Some(head_sha.to_string()),
            wave: Some(Some(wave)),
            ..events::CardCut::default()
        }),
    )?;
    Ok(())
}

/// R36: the fresh re-run of a voided fix wave implementer starts from the run head
/// (`_recut_final`). The final branch is moved back there (compare-and-swap), and the voided
/// worker's commits are kept only on `refs/leadhand/<run id>/violations/<n>-final`, so the fix
/// review never covers them. Recorded as `card_cut{card: "final", recut_after}`; repeat-safe.
pub(super) fn recut_final(st: &RunState, dispatch_id: &str) -> Result<()> {
    let run = st.run;
    let done = st.log.events.iter().any(|e| {
        matches!(e, Event::CardCut(c) if given(&c.recut_after).map(String::as_str) == Some(dispatch_id))
    });
    if done {
        return Ok(());
    }
    let d = dispatch(&st.log, dispatch_id)?;
    let found = st
        .log
        .violations()
        .find(|v| {
            given(&v.dispatch_id).map(String::as_str) == Some(dispatch_id)
                && v.stage.as_deref() == Some("found")
        })
        .ok_or_else(|| Error::msg("StopIteration"))?;
    let git = run.git()?;
    let branch = final_branch(run);
    let head_sha = head(&git, &run.run_ref().to_os()?)?;
    let name = heads(&branch);
    let old = ref_value(&git, &name)?;
    if old.as_deref() != Some(head_sha.as_str()) {
        check_out(st, None, &run.branch)?;
        let n = found
            .n
            .map_or_else(|| "None".to_string(), |n| n.to_string());
        let keep = format!("refs/leadhand/{}/violations/{n}-final", run_id(run)?);
        move_final(
            &git,
            &name,
            old.as_deref(),
            &head_sha,
            ("leadhand: keep a voided final fix", keep),
            "leadhand: re-cut the final fix branch",
        )?;
    }
    append_event(
        &run.paths,
        Event::CardCut(events::CardCut {
            card: Some(final_card()),
            branch: Some(branch),
            base: Some(head_sha),
            wave: Some(given(&d.req.wave).copied()),
            recut_after: Some(Some(dispatch_id.to_string())),
            ..events::CardCut::default()
        }),
    )?;
    Ok(())
}

fn dispatch<'l>(log: &'l Log, id: &str) -> Result<Dispatch<'l>> {
    log.dispatch(Some(id))
        .ok_or_else(|| Error::msg(format!("no dispatch {id} was requested")))
}

// --------------------------------------------------------------------------
// Results

/// Records a gate's result, past the crash point before it (`_after_run_result`).
pub(super) fn record_gate(run: &Run, event: Event) -> Result<()> {
    crash_point("before_gate_record");
    append_event(&run.paths, event)?;
    Ok(())
}

/// Accepts a clean fix wave: fast-forwards the run branch to the reviewed fix tip `target`,
/// as intent, move and completion; with the intent already recorded (a crash came after it),
/// the move and the completion only (`_after_run_result`'s acceptance). Idempotent: a run
/// branch that already contains the target is left where it is.
pub(super) fn accept_fix(
    run: &Run,
    dispatch_id: &str,
    wave: Option<i64>,
    target: &str,
    started: bool,
) -> Result<()> {
    let git = run.git()?;
    let act = || fast_forward(&git, &run.branch, target);
    let done = |_: &String| {
        Event::Accepted(events::Accepted {
            card: Some(final_card()),
            wave: Some(wave),
            sha: Some(target.to_string()),
            dispatch_id: Some(Some(dispatch_id.to_string())),
            ..events::Accepted::default()
        })
    };
    if started {
        two_phase_resume(&run.paths, act, "after_fast_forward", done)?;
    } else {
        let intent = Event::AcceptStarted(events::AcceptStarted {
            card: Some(final_card()),
            wave: Some(wave),
            target: Some(target.to_string()),
            dispatch_id: Some(Some(dispatch_id.to_string())),
            ..events::AcceptStarted::default()
        });
        two_phase(
            &run.paths,
            intent,
            "before_fast_forward",
            act,
            "after_fast_forward",
            done,
        )?;
    }
    Ok(())
}

// --------------------------------------------------------------------------
// The evidence base worktree

/// The run's evidence base worktree, `<repo>/.worktrees/<workspace name>--base`
/// (`_base_worktree_path`).
pub(crate) fn base_worktree_path(run: &Run) -> PathBuf {
    let name = run.root.file_name().unwrap_or_default();
    dispatch_base_path(&run.repo, &run.ws, name)
}

/// The base worktree an evidence dispatch recorded (its `extra`'s `base_worktree`), else the
/// run's.
pub(super) fn base_worktree_at(run: &Run, d: &Dispatch) -> PathBuf {
    match d
        .extra()
        .get(&PyStr::from("base_worktree"))
        .cloned()
        .flatten()
        .filter(|p| !p.is_empty())
    {
        Some(p) => pure_path(OsStr::new(&p.to_os_lossy())),
        None => base_worktree_path(run),
    }
}

/// Puts the evidence base worktree back exactly, creating it when missing (`_base_worktree`,
/// R60): `git::base_worktree::prepare`, with this run's crash window read from its events
/// and `base_worktree_added` and `base_worktree_ready` recorded when due.
pub(super) fn base_worktree(run: &Run) -> Result<PathBuf> {
    let path = base_worktree_path(run);
    let shown = path_text(&path);
    let log = Log::read(&run.paths)?;
    let (mut added, mut ready) = (-1_i64, -1_i64);
    for e in &log.events {
        match e {
            Event::BaseWorktreeAdded(a) if a.path.as_ref() == Some(&shown) => added = seq(e),
            Event::BaseWorktreeReady(r) if r.path.as_ref() == Some(&shown) => ready = seq(e),
            _ => {}
        }
    }
    let b = BaseWorktree {
        repo: &run.repo,
        path: &path,
        base: &run.base_sha,
        run_id: run_id(run)?,
        run_dir: &run.root,
        crash_window: added > ready,
    };
    prepare(&b, &mut |step| {
        let event = match step {
            Record::Added => Event::BaseWorktreeAdded(events::BaseWorktreeAdded {
                path: Some(shown.clone()),
                ..events::BaseWorktreeAdded::default()
            }),
            Record::Ready => Event::BaseWorktreeReady(events::BaseWorktreeReady {
                path: Some(shown.clone()),
                ..events::BaseWorktreeReady::default()
            }),
        };
        append_event(&run.paths, event).map(drop)
    })
}

fn seq(e: &Event) -> i64 {
    i64::try_from(e.seq()).unwrap_or(i64::MAX)
}
