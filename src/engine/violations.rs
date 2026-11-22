//! Violations: what the shell does for `decide` when a worker changed what its dispatch guards,
//! or an implementer left something in the workspace (`engine.py`: `_find_violation`,
//! `_engine_writes`, `_violation_paths`, `_void`, `_restore_violation`, `_ensure_clean`,
//! `_finish_leftovers`; R14', R30, R33).
//!
//! - A check (review, rereview, verify) must leave the card tip, a clean workspace and every
//!   ref of the run as it found them; an implementer-side worker the run branch. A change is
//!   recorded `violation{stage: found}` before anything moves, so a crash part way still voids
//!   the result; it is then kept under `refs/leadhand/<run id>/violations/<n>` (with its
//!   siblings, and each moved ref's new tip on a ref of its own), put back, and recorded
//!   `restored`.
//! - Before a check is dispatched and before the engine switches branches or rebases, whatever
//!   an implementer left (a dirty tree or index, HEAD off the branch tip, an operation in
//!   progress, untracked files it may sweep) is kept under
//!   `refs/leadhand/<run id>/leftovers/<n>` and cleared, as `violation{by: implementer}`. It
//!   voids nothing.
//!
//! - After an evidence result, the evidence base worktree must still be at the base commit with
//!   no tracked change; otherwise the result is void too (`violation{base_worktree}`), its moved
//!   HEAD kept on `refs/leadhand/<run id>/violations/<n>-base` and the base worktree put back.
//!
//! Every Git call runs in the run's worktree (`Run::git`), or in the run's evidence base
//! worktree, never in the operator's checkout. Deciding what follows a void (a re-run, the card
//! parked at the cap, or the run ended blocked at a run-level cap) is `next`'s.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use super::cards::{Dispatch, Log, Run, by, heads, os};
use crate::dispatch::untracked_to_take;
use crate::error::{Error, Result};
use crate::git::cards::{branch_ref, card_branch_name, operation, ref_snapshot};
use crate::git::preserve::{
    checkout_top, detect_violation, lexists, preserve_and_restore, violation_paths,
};
use crate::git::workspace::head;
use crate::git::{GIT_TIMEOUT, Git, untracked_files};
use crate::pytext::path_text;
use crate::state::events::{self, RefMove, given};
use crate::state::{Event, append_event};
use crate::wire::{PyMap, PyStr};

/// The run's ID, which every leadhand ref of the run is named under (`ctx.meta["run_id"]`).
pub(super) fn run_id(run: &Run) -> Result<&str> {
    run.run_id
        .as_deref()
        .ok_or_else(|| Error::msg("KeyError: 'run_id'"))
}

/// The Python's `KeyError` on a field a record lacks, as an error line (S2-R43).
fn key_error(field: &str) -> Error {
    Error::msg(format!("KeyError: '{field}'"))
}

// --------------------------------------------------------------------------
// A check's violation

/// Compares the workspace and the run's refs with what dispatch `d` recorded
/// (`_find_violation`): the `found` record when its worker changed what the dispatch guards,
/// else `None`. Moves the engine recorded after the dispatch are its own, not the worker's.
/// Nothing is recorded or moved here; the one write is `detect_violation`'s, which clears an
/// index flag that hid an edit, as every check does.
pub(super) fn find_violation(
    run: &Run,
    log: &Log,
    d: &Dispatch,
) -> Result<Option<events::Violation>> {
    let req = d.req;
    let git = run.git()?;
    // `if req.get("expected_head")`: an empty head is none, as the Python's truthiness reads it.
    let v = match given(&req.expected_head).filter(|h| !h.is_empty()) {
        Some(expected) => {
            let branch = given(&req.expected_branch).cloned().unwrap_or_default();
            detect_violation(&git, expected, &branch)?
        }
        None => None,
    };
    let before = given(&req.refs).cloned().unwrap_or_default();
    let now = ref_snapshot(&git, &run.branch, run_id(run)?)?;
    let (latest, owned) = engine_writes(run, log, req.seq);
    let run_ref = run.run_ref();
    // An acceptance whose fast-forward happened but whose `accepted` is not recorded yet moved
    // the run branch to its target: the engine's move too.
    let targets: HashSet<Option<&str>> = log
        .events
        .iter()
        .filter_map(|e| match e {
            Event::AcceptStarted(a) if a.seq > req.seq => Some(a.target.as_deref()),
            _ => None,
        })
        .collect();
    // `sorted(names - owned)`: WTF-8 order is code point order.
    let names: BTreeSet<PyStr> = match given(&req.ref_scope) {
        Some(scope) if !scope.is_empty() => scope.iter().cloned().collect(),
        _ => before
            .0
            .iter()
            .map(|(k, _)| k.clone())
            .chain(now.0.iter().map(|(k, _)| k.clone()))
            .collect(),
    };
    let mut moved: PyMap<RefMove> = PyMap::default();
    for name in names.iter().filter(|name| !owned.contains(*name)) {
        let expected: Option<String> = match latest.get(name) {
            Some(value) => value.clone(),
            None => before.get(name).cloned().flatten(),
        };
        let current = now.get(name).cloned();
        if *name == run_ref && targets.contains(&current.as_deref()) {
            continue;
        }
        if current != expected {
            moved.0.push((
                name.clone(),
                RefMove {
                    old: expected,
                    new: current,
                },
            ));
        }
    }
    if v.is_none() && moved.is_empty() {
        return Ok(None);
    }
    let n = 1 + log
        .check_violations()
        .filter(|e| e.stage.as_deref() == Some("found"))
        .count();
    let saved_ref = PyStr::from(format!("refs/leadhand/{}/violations/{n}", run_id(run)?));
    let mut preserved: PyMap<PyStr> = PyMap::default();
    for (name, m) in &moved.0 {
        if m.new.is_none() {
            continue; // deleted: restoring it brings it back
        }
        let mut keep = saved_ref.clone();
        keep.push_str(&format!("-{}", ref_slug(name)));
        let (stem, mut k) = (keep.clone(), 1);
        while preserved.values().any(|kept| *kept == keep) {
            k += 1;
            keep = stem.clone();
            keep.push_str(&format!("-{k}"));
        }
        preserved.0.push((name.clone(), keep));
    }
    let hidden = v.as_ref().map(|v| v.hidden.clone()).unwrap_or_default();
    Ok(Some(events::Violation {
        stage: Some("found".into()),
        by: Some("check".into()),
        dispatch_id: Some(Some(d.id().to_string())),
        card: Some(req.card.clone()),
        dispatch_kind: Some(Some(d.kind().to_string())),
        n: Some(i64::try_from(n).unwrap_or(i64::MAX)),
        r#ref: v.is_some().then_some(saved_ref),
        paths: Some(Some(violation_paths(&git, v.as_ref())?)),
        head_moved: Some(Some(v.as_ref().is_some_and(|v| v.head_moved))),
        index_dirty: Some(Some(v.as_ref().is_some_and(|v| v.index_dirty))),
        expected_head: Some(given(&req.expected_head).cloned()),
        expected_branch: Some(given(&req.expected_branch).cloned()),
        refs: Some(Some(moved)),
        preserved: Some(Some(preserved)),
        hidden: (!hidden.is_empty()).then_some(Some(hidden)),
        ..events::Violation::default()
    }))
}

/// A ref's name as a keep ref's suffix: `refs/` dropped, every run of characters but ASCII
/// letters, digits and `_` one `-`, and `-` trimmed at either end; `ref` when nothing is left
/// (`re.sub(r"[^A-Za-z0-9_]+", "-", name.removeprefix("refs/")).strip("-") or "ref"`). A
/// character outside ASCII is all bytes of 0x80 and above in WTF-8, so a byte-wise run is a
/// run of characters.
fn ref_slug(name: &PyStr) -> String {
    let bytes = name.as_wtf8();
    let rest = bytes.strip_prefix(b"refs/").unwrap_or(bytes);
    let mut out = String::new();
    let mut in_run = false;
    for &b in rest {
        if b.is_ascii_alphanumeric() || b == b'_' {
            out.push(char::from(b));
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    match out.trim_matches('-') {
        "" => "ref".to_string(),
        slug => slug.to_string(),
    }
}

/// What the engine itself recorded moving after event `after` (`_engine_writes`): branch →
/// the value it last set, and the leadhand refs it wrote (owned outright).
fn engine_writes(
    run: &Run,
    log: &Log,
    after: u64,
) -> (HashMap<PyStr, Option<String>>, HashSet<PyStr>) {
    let mut latest: HashMap<PyStr, Option<String>> = HashMap::new();
    let mut owned: HashSet<PyStr> = HashSet::new();
    for e in log.events.iter().filter(|e| e.seq() > after) {
        match e {
            Event::Accepted(a) => {
                latest.insert(run.run_ref(), a.sha.clone());
            }
            Event::CardCut(c) => {
                if let Some(branch) = c.branch.as_ref().filter(|b| !b.is_empty()) {
                    latest.insert(heads(branch), c.base.clone());
                }
            }
            Event::Rebased(r) => {
                if let Some(card) = &r.card {
                    let branch = card_branch_name(&run.branch, &card.text().lossy());
                    latest.insert(heads(&branch), r.tip.clone());
                }
            }
            Event::IntegrationStarted(i) => {
                if let Some(keep) = i.keep.as_ref().filter(|k| !k.is_empty()) {
                    owned.insert(keep.clone());
                }
            }
            Event::Violation(v) => {
                if let Some(r) = v.r#ref.as_ref().filter(|r| !r.is_empty()) {
                    for suffix in ["", "-branch", "-autostash"] {
                        let mut name = r.clone();
                        name.push_str(suffix);
                        owned.insert(name);
                    }
                }
                if let Some(preserved) = given(&v.preserved) {
                    owned.extend(preserved.values().cloned());
                }
                if v.stage.as_deref() == Some("found") && by(v) == "implementer" {
                    let branch = given(&v.expected_branch).cloned().unwrap_or_default();
                    latest.insert(heads(&branch), given(&v.expected_head).cloned());
                } else if v.stage.as_deref() == Some("found") {
                    for (name, m) in given(&v.refs).map(|r| r.0.as_slice()).unwrap_or_default() {
                        latest.insert(name.clone(), m.old.clone());
                    }
                }
            }
            _ => {}
        }
    }
    (latest, owned)
}

/// `_void`'s I/O for a check dispatch whose worker changed what it guards: records the `found`
/// violation `decide` found (when it is new), then, unless one is recorded, keeps what the
/// worker changed and puts it back (`_restore_violation`) and records `restored`. Repeat-safe
/// after a crash between the two phases.
pub(super) fn void(run: &Run, dispatch_id: &str, found: Option<events::Violation>) -> Result<()> {
    if let Some(found) = found {
        append_event(&run.paths, Event::Violation(found))?;
    }
    let log = Log::read(&run.paths)?;
    let d = log
        .dispatch(Some(dispatch_id))
        .ok_or_else(|| Error::msg(format!("no dispatch {dispatch_id} was requested")))?;
    let mine: Vec<&events::Violation> = log
        .check_violations()
        .filter(|v| given(&v.dispatch_id).map(String::as_str) == Some(dispatch_id))
        .collect();
    let Some(found) = mine.iter().find(|v| v.stage.as_deref() == Some("found")) else {
        return Err(Error::msg("StopIteration"));
    };
    if mine.iter().any(|v| v.stage.as_deref() == Some("restored")) {
        return Ok(());
    }
    if found.base_worktree.is_some() {
        // An evidence worker that changed the base worktree.
        return restore_base(run, found);
    }
    let preserved = given(&found.preserved)
        .cloned()
        .ok_or_else(|| key_error("preserved"))?;
    let (saved, siblings) = restore_violation(run, found, &preserved)?;
    append_event(
        &run.paths,
        Event::Violation(events::Violation {
            stage: Some("restored".into()),
            by: Some("check".into()),
            dispatch_id: Some(Some(dispatch_id.to_string())),
            card: Some(d.req.card.clone()),
            n: found.n,
            r#ref: found.r#ref.clone(),
            saved: Some(saved),
            siblings: Some(Some(siblings)),
            preserved: Some(Some(preserved)),
            ..events::Violation::default()
        }),
    )?;
    Ok(())
}

/// Keeps every moved ref's new tip and the workspace on side refs, then puts the workspace and
/// the refs back (`_restore_violation`). Repeat-safe after a crash at any point. The saved
/// commit and the siblings it set.
fn restore_violation(
    run: &Run,
    found: &events::Violation,
    preserved: &PyMap<PyStr>,
) -> Result<(Option<String>, Vec<PyStr>)> {
    let git = run.git()?;
    let refs = given(&found.refs).ok_or_else(|| key_error("refs"))?;
    for (name, keep) in &preserved.0 {
        let new = refs
            .get(name)
            .ok_or_else(|| Error::msg(format!("KeyError: {}", name.repr())))?
            .new
            .clone()
            .unwrap_or_default();
        git.run(&[
            os("update-ref"),
            os("-m"),
            os("leadhand: preserve a ref a worker moved"),
            &keep.to_os()?,
            os(&new),
        ])?;
    }
    let (mut saved, mut siblings) = (None, Vec::new());
    let id = run_id(run)?;
    if let Some(saved_ref) = found.r#ref.as_ref().filter(|r| !r.is_empty()) {
        let expected_head = given(&found.expected_head).cloned().unwrap_or_default();
        let expected_branch = given(&found.expected_branch).cloned().unwrap_or_default();
        if detect_violation(&git, &expected_head, &expected_branch)?.is_some() {
            saved = Some(preserve_and_restore(
                &git,
                saved_ref,
                &expected_head,
                &expected_branch,
                &[],
            )?);
        }
        let now = ref_snapshot(&git, &run.branch, id)?;
        saved = saved.or_else(|| now.get(saved_ref).cloned());
        siblings = present_siblings(saved_ref, &now);
    }
    let now = ref_snapshot(&git, &run.branch, id)?;
    for (name, m) in &refs.0 {
        let current = now.get(name);
        if current == m.old.as_ref() {
            continue;
        }
        if current != m.new.as_ref() {
            let shown = |s: Option<&String>| s.map_or_else(|| "None".to_string(), String::clone);
            return Err(Error::msg(format!(
                "{} changed again after the violation by {} (recorded {}, now {}); restore it \
                 by hand",
                name.shown(),
                given(&found.dispatch_id).map_or("None", String::as_str),
                shown(m.new.as_ref()),
                shown(current)
            )));
        }
        let name_os = name.to_os()?;
        let current = current.cloned().unwrap_or_default();
        match &m.old {
            None => git.run(&[
                os("update-ref"),
                os("-m"),
                os("leadhand: remove a ref a worker created"),
                os("-d"),
                &name_os,
                os(&current),
            ])?,
            // The moved value is the expected current value: a ref changed meanwhile is not
            // overwritten.
            Some(old) => git.run(&[
                os("update-ref"),
                os("-m"),
                os("leadhand: restore a ref a worker moved"),
                &name_os,
                os(old),
                os(&current),
            ])?,
        };
    }
    Ok((saved, siblings))
}

// --------------------------------------------------------------------------
// The evidence base worktree

/// After an evidence result, whether the base worktree is still at the base commit with no
/// tracked change (`_base_violation`'s check): the `found` record to void the result with when
/// it is not, else `None`. Nothing is recorded here; an edit hidden behind an index flag shows
/// again (the flag is cleared), as every check does. A moved HEAD is to be kept on
/// `refs/leadhand/<run id>/violations/<n>-base`.
pub(super) fn base_violation(
    run: &Run,
    log: &Log,
    d: &Dispatch,
) -> Result<Option<events::Violation>> {
    let req = d.req;
    let path = super::gates::base_worktree_at(run, d);
    let base = &run.base_sha;
    // A worktree Git cannot read counts as moved, with nothing to keep.
    let (hidden, now, dirty) = base_state(&path).unwrap_or_default();
    if now.as_deref() == Some(base.as_str()) && dirty.is_empty() && hidden.is_empty() {
        return Ok(None);
    }
    let n = 1 + log
        .check_violations()
        .filter(|e| e.stage.as_deref() == Some("found"))
        .count();
    let keep = PyStr::from(format!(
        "refs/leadhand/{}/violations/{n}-base",
        run_id(run)?
    ));
    let moved = now.as_deref() != Some(base.as_str());
    let mut preserved = PyMap::default();
    if now.is_some() && moved {
        preserved.0.push((PyStr::from("base"), keep));
    }
    Ok(Some(events::Violation {
        stage: Some("found".into()),
        by: Some("check".into()),
        dispatch_id: Some(Some(d.id().to_string())),
        card: Some(None),
        dispatch_kind: Some(Some(d.kind().to_string())),
        n: Some(i64::try_from(n).unwrap_or(i64::MAX)),
        r#ref: None,
        base_worktree: Some(Some(path_text(&path))),
        base_head: Some(now),
        paths: Some(Some(dirty)),
        head_moved: Some(Some(moved)),
        index_dirty: Some(Some(false)),
        expected_head: Some(given(&req.expected_head).cloned()),
        expected_branch: Some(given(&req.expected_branch).cloned()),
        refs: Some(Some(PyMap::default())),
        preserved: Some(Some(preserved)),
        ..events::Violation::default()
    }))
}

/// The base worktree's hidden entries (now cleared), HEAD and changed tracked paths, as
/// `_base_violation` reads them (`status --porcelain` lines from their fourth character on,
/// sorted). An error, with nothing read or unhidden, unless the folder is its own Git top
/// (S2-R54): one whose `.git` is gone lies inside the operator's checkout, whose index
/// entries Git would unhide.
fn base_state(path: &Path) -> Result<(Vec<PyStr>, Option<String>, Vec<PyStr>)> {
    let git = Git::at(path)?;
    if !crate::git::workspace::is_own_top(path) {
        return Err(Error::msg(format!(
            "the evidence base worktree {} is not a worktree top",
            path.display()
        )));
    }
    let hidden = crate::git::unhide_index_entries(&git)?;
    let (now, dirty) = crate::git::base_worktree::head_and_status(&git)?;
    Ok((hidden, Some(now), dirty))
}

/// Keeps the base HEAD an evidence worker moved on its ref, puts the base worktree back and
/// records `restored` (`_restore_base`). Repeat-safe: after a crash in between, `void` calls it
/// again.
fn restore_base(run: &Run, found: &events::Violation) -> Result<()> {
    let preserved = given(&found.preserved).cloned().unwrap_or_default();
    let base_head = given(&found.base_head).cloned();
    if let (Some(keep), Some(head)) = (
        preserved
            .get(&PyStr::from("base"))
            .filter(|k| !k.is_empty()),
        base_head.as_ref().filter(|h| !h.is_empty()),
    ) {
        run.git()?.run(&[
            os("update-ref"),
            os("-m"),
            os("leadhand: keep a base worktree a verifier moved"),
            &keep.to_os()?,
            os(head),
        ])?;
    }
    super::gates::base_worktree(run)?;
    let preserved = given(&found.preserved)
        .cloned()
        .ok_or_else(|| key_error("preserved"))?;
    append_event(
        &run.paths,
        Event::Violation(events::Violation {
            stage: Some("restored".into()),
            by: Some("check".into()),
            dispatch_id: Some(given(&found.dispatch_id).cloned()),
            card: Some(None),
            n: found.n,
            r#ref: None,
            saved: Some(base_head),
            siblings: Some(Some(Vec::new())),
            preserved: Some(Some(preserved)),
            ..events::Violation::default()
        }),
    )?;
    Ok(())
}

/// The siblings of `saved_ref` the snapshot lists: `<ref>-branch`, `<ref>-autostash`.
fn present_siblings(saved_ref: &PyStr, now: &PyMap<String>) -> Vec<PyStr> {
    ["-branch", "-autostash"]
        .iter()
        .map(|suffix| {
            let mut name = saved_ref.clone();
            name.push_str(suffix);
            name
        })
        .filter(|name| now.get(name).is_some())
        .collect()
}

// --------------------------------------------------------------------------
// An implementer's leftovers (R30)

/// R30, before a check is dispatched and before the engine rebases or switches branches
/// (`_ensure_clean`): anything an implementer left in the workspace (a dirty tree or index,
/// HEAD off `branch`'s tip, an operation in progress) is kept under
/// `refs/leadhand/<run id>/leftovers/<n>` with `preserve_and_restore`'s siblings and cleared,
/// putting the workspace on `branch` at its tip. Recorded as `violation{by: implementer}`; it
/// voids nothing. With `operations_only`, only an operation in progress counts. With
/// `untracked` (before the engine switches cards or rebases), the untracked files Git does not
/// ignore count too, but for a nested repository's and those the operator had at start (R48'):
/// they are kept in the leftovers commit and removed, so no later card's commit sweeps them in
/// (I3). Whether anything was found.
pub(super) fn ensure_clean(
    run: &Run,
    branch: &PyStr,
    why: &PyStr,
    after_dispatch: Option<&str>,
    operations_only: bool,
    untracked: bool,
) -> Result<bool> {
    let git = run.git()?;
    let op = operation(&git)?;
    if operations_only && op.is_none() {
        return Ok(false);
    }
    let tip = head(&git, &branch_ref(branch)?)?;
    let v = detect_violation(&git, &tip, branch)?;
    let loose = if untracked {
        let paths = untracked_files(&run.ws).map_err(Error::from)?;
        untracked_to_take(&run.root, &run.ws, &paths, GIT_TIMEOUT)?
    } else {
        Vec::new()
    };
    if op.is_none() && v.is_none() && loose.is_empty() {
        return Ok(false);
    }
    let n = 1 + Log::read(&run.paths)?
        .violations()
        .filter(|e| by(e) == "implementer" && e.stage.as_deref() == Some("found"))
        .count();
    let hidden = v.as_ref().map(|v| v.hidden.clone()).unwrap_or_default();
    let found = events::Violation {
        stage: Some("found".into()),
        by: Some("implementer".into()),
        n: Some(i64::try_from(n).unwrap_or(i64::MAX)),
        r#ref: Some(format!("refs/leadhand/{}/leftovers/{n}", run_id(run)?).into()),
        paths: Some(Some(violation_paths(&git, v.as_ref())?)),
        operation: Some(op.map(str::to_string)),
        head_moved: Some(Some(v.as_ref().is_some_and(|v| v.head_moved))),
        index_dirty: Some(Some(v.as_ref().is_some_and(|v| v.index_dirty))),
        expected_head: Some(Some(tip)),
        expected_branch: Some(Some(branch.clone())),
        after_dispatch: Some(after_dispatch.map(str::to_string)),
        why: Some(Some(why.clone())),
        untracked: (!loose.is_empty()).then_some(Some(loose)),
        hidden: (!hidden.is_empty()).then_some(Some(hidden)),
        ..events::Violation::default()
    };
    let (found, _) = append_event(&run.paths, Event::Violation(found))?;
    let Event::Violation(found) = found else {
        unreachable!("append_event returns the event it appended");
    };
    finish_leftovers(run, &[&found])?;
    Ok(true)
}

/// Keeps and clears every recorded leftovers violation not restored yet (`_finish_leftovers`).
/// Repeat-safe: a workspace already clean is not kept again, so the saved state is never
/// replaced.
pub(super) fn finish_leftovers(run: &Run, pending: &[&events::Violation]) -> Result<()> {
    let git = run.git()?;
    // Git names untracked paths from the checkout's top.
    let top = checkout_top(&run.ws);
    let id = run_id(run)?;
    for f in pending {
        let loose: Vec<PyStr> = given(&f.untracked)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|p| p.to_os().is_ok_and(|rel| lexists(&top.join(rel))))
            .cloned()
            .collect();
        let saved_ref = f.r#ref.clone().ok_or_else(|| key_error("ref"))?;
        let expected_head = given(&f.expected_head).cloned().unwrap_or_default();
        let expected_branch = given(&f.expected_branch).cloned().unwrap_or_default();
        let mut saved = None;
        if operation(&git)?.is_some()
            || detect_violation(&git, &expected_head, &expected_branch)?.is_some()
            || !loose.is_empty()
        {
            saved = Some(preserve_and_restore(
                &git,
                &saved_ref,
                &expected_head,
                &expected_branch,
                &loose,
            )?);
        }
        let now = ref_snapshot(&git, &run.branch, id)?;
        append_event(
            &run.paths,
            Event::Violation(events::Violation {
                stage: Some("restored".into()),
                by: Some("implementer".into()),
                n: f.n,
                r#ref: Some(saved_ref.clone()),
                saved: Some(saved.or_else(|| now.get(&saved_ref).cloned())),
                after_dispatch: Some(given(&f.after_dispatch).cloned()),
                siblings: Some(Some(present_siblings(&saved_ref, &now))),
                ..events::Violation::default()
            }),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // S2-R54, bug class 1: an evidence base worktree folder whose `.git` file is gone lies
    // inside the operator's checkout, where unhiding index entries would clear the operator's
    // own flags. `base_state` reads nothing there unless Git's top is the folder itself.
    #[test]
    fn base_state_reads_only_a_folder_that_is_its_own_git_top() {
        let f = crate::git::fixture::Fixture::new();
        let repo = f.make_repo("repo");
        std::fs::write(repo.join(".git/info/exclude"), ".worktrees/\n").unwrap();
        let base = repo.join(".worktrees").join("demo--base");
        let sha = f.git(&repo, &["rev-parse", "HEAD"]);
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                base.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert_eq!(
            base_state(&base),
            Ok((Vec::new(), Some(sha.clone()), Vec::new()))
        );
        // A genuine one reached through a symlink reads too.
        let link = f.root().join("base link");
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert_eq!(base_state(&link), Ok((Vec::new(), Some(sha), Vec::new())));
        std::fs::remove_file(base.join(".git")).unwrap();
        // The operator hides an edit of their own behind assume-unchanged.
        f.write(&repo, &[("README.md", "the operator's edit\n")]);
        f.git(&repo, &["update-index", "--assume-unchanged", "README.md"]);
        let before = f.checkout_state(&repo);
        let e = base_state(&base).unwrap_err();
        assert!(e.message.contains("is not a worktree top"), "{e}");
        assert_eq!(f.checkout_state(&repo), before);
    }

    #[test]
    fn keep_ref_suffixes_are_named_as_the_python_names_them() {
        let names = [
            "refs/heads/leadhand/demo",
            "refs/heads/leadhand/demo--card-1",
            "refs/leadhand/abc/elsewhere/note",
            "refs/heads/caf\u{e9}/x",
            "refs/---",
            "refs/",
            "heads/no-prefix",
            "refs/heads/a_b.c",
        ];
        // `engine.json`: the reference's naming of each,
        // `re.sub(r'[^A-Za-z0-9_]+', '-', n.removeprefix('refs/')).strip('-') or 'ref'`.
        let inputs: Vec<serde_json::Value> = names.iter().map(|n| serde_json::json!(n)).collect();
        let theirs: Vec<String> =
            crate::wire::tests::reference_outputs("engine.json", "keep_ref_suffix", &inputs)
                .iter()
                .map(|out| out.as_str().unwrap().to_string())
                .collect();
        let ours: Vec<String> = names.iter().map(|n| ref_slug(&(*n).into())).collect();
        assert_eq!(ours, theirs);
        // An undecodable byte is a character outside the class too.
        let odd = PyStr::from_os(std::os::unix::ffi::OsStrExt::from_bytes(
            b"refs/heads/a\xffb",
        ));
        assert_eq!(ref_slug(&odd), "heads-a-b");
    }
}
