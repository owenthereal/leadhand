//! A worker's changes to the run's worktree: found, kept and undone (`gitops`:
//! `hidden_index_entries`, `unhide_index_entries`, `is_sparse`, `Violation`, `detect_violation`,
//! `preserve_and_restore` with `_snapshot_untracked`, `_snapshot_conflicted`, `_take_autostash`,
//! `_clear_operations`, `_set_sibling` and `_remove_untracked`, and `checkout_top`; the engine's
//! `_violation_paths`).
//!
//! Every call here runs in the run's own worktree, never in the operator's checkout (bug class
//! 1), and writes only:
//! - the index flags that hide an edit (assume-unchanged, skip-worktree), cleared, and a sparse
//!   checkout a worker turned on there, turned off again (R64);
//! - the ref a violation or a leftover is kept on (`refs/leadhand/<run id>/…`) and its siblings
//!   `<ref>-branch` and `<ref>-autostash`; never `refs/stash`;
//! - a scratch index beside the worktree's own, removed again;
//! - restoring: the worktree's checkout, index and tracked files, a Git operation in progress
//!   cleared, and the untracked files a leftover kept, removed.

use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::cards::{
    NO_SIGN, branch_ref, check_branch_name, checked_out_branch, git_path, is_ancestor, os,
    rebase_in_progress, text, trim_newlines, yes_or_no,
};
use super::stage::pathspecs;
use super::workspace::{head, ref_exists};
use super::{GIT_TIMEOUT, Git, split_nul, top_and_prefix};
use crate::error::{Error, Result};
use crate::pytext::{py_splitlines_bytes, real_path};
use crate::wire::PyStr;

// --------------------------------------------------------------------------
// Edits hidden behind an index flag (Codex round 10)

/// `hidden_index_entries`: the tracked paths, in the whole checkout and named from its top,
/// whose index entry hides an edit from `status` and `diff`: assume-unchanged (a lowercase
/// `git ls-files -v` tag) or skip-worktree (`S`).
pub fn hidden_index_entries(git: &Git) -> Result<Vec<Vec<u8>>> {
    let out = git.run(&[
        os("ls-files"),
        os("-v"),
        os("-z"),
        os("--full-name"),
        os("--"),
        os(":/"),
    ])?;
    Ok(split_nul(trim_newlines(&out))
        .into_iter()
        .filter(|entry| {
            entry.first().is_some_and(u8::is_ascii_lowercase) || entry.starts_with(b"S ")
        })
        .map(|entry| entry.get(2..).unwrap_or_default().to_vec())
        .collect())
}

/// `unhide_index_entries`: clears assume-unchanged and skip-worktree wherever
/// `hidden_index_entries` finds them, so an edit behind them shows in `status` and `diff` again
/// (a worker can set them after it is dispatched). Every check and every leadhand commit reads
/// the workspace after this. A sparse checkout a worker turned on is turned off again. The
/// paths cleared.
pub fn unhide_index_entries(git: &Git) -> Result<Vec<PyStr>> {
    let hidden = hidden_index_entries(git)?;
    if !hidden.is_empty() {
        let (top, _) = top_and_prefix(git)?;
        let top = Git::at(&top)?.with_deadline(git.deadline());
        let mut paths = Vec::new();
        for p in &hidden {
            paths.extend_from_slice(p);
            paths.push(0);
        }
        // One mode per call.
        for flag in ["--no-assume-unchanged", "--no-skip-worktree"] {
            top.run_input(
                &[os("update-index"), os(flag), os("-z"), os("--stdin")],
                &paths,
            )?;
        }
        if is_sparse(&top) {
            // With the flags already cleared it changes no file, so the edits stay visible.
            top.run(&[os("sparse-checkout"), os("disable")])?;
        }
    }
    Ok(hidden.iter().map(|p| text(p)).collect())
}

/// `is_sparse`: whether the checkout is sparse (`core.sparseCheckout`, as that worktree sees
/// it). False when Git cannot be run.
fn is_sparse(git: &Git) -> bool {
    let args = [
        os("config"),
        os("--bool"),
        os("--get"),
        os("core.sparseCheckout"),
    ];
    let Ok(out) = git.with_deadline(GIT_TIMEOUT).output(&args) else {
        return false;
    };
    out.status.success() && out.stdout.trim_ascii() == b"true"
}

// --------------------------------------------------------------------------
// Finding a change

/// How the workspace differs from a branch tip it must be on (`gitops.Violation`). Refs a
/// worker moved are the engine's to compare (`engine::violations`), against what its events
/// record moving itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// HEAD is not `expected_head` on `expected_branch`.
    pub head_moved: bool,
    /// The index differs from HEAD.
    pub index_dirty: bool,
    /// Tracked files whose working copy differs from the index.
    pub tracked_changes: Vec<PyStr>,
    /// Entries found assume-unchanged or skip-worktree (now cleared).
    pub hidden: Vec<PyStr>,
}

/// `None` when the workspace is on `expected_branch` at `expected_head` with a clean index and
/// clean tracked files (`gitops.detect_violation`). Untracked files (build output) are not a
/// violation. An entry marked assume-unchanged or skip-worktree is: the mark is cleared first,
/// so an edit behind it counts as the tracked change it is.
pub fn detect_violation(
    git: &Git,
    expected_head: &str,
    expected_branch: &PyStr,
) -> Result<Option<Violation>> {
    let hidden = unhide_index_entries(git)?;
    let head_moved = checked_out_branch(git)?.as_ref() != Some(expected_branch)
        || head(git, os("HEAD"))? != expected_head;
    let index_dirty = !yes_or_no(
        git,
        &[
            os("diff"),
            os("--cached"),
            os("--quiet"),
            os("--no-ext-diff"),
            os("HEAD"),
            os("--"),
        ],
    )?;
    // --name-only rather than --quiet: it ignores stat-only changes.
    let out = git.run(&[
        os("diff"),
        os("--name-only"),
        os("-z"),
        os("--no-renames"),
        os("--no-ext-diff"),
        os("--"),
    ])?;
    let tracked_changes: Vec<PyStr> = split_nul(trim_newlines(&out))
        .iter()
        .map(|p| text(p))
        .collect();
    if head_moved || index_dirty || !tracked_changes.is_empty() || !hidden.is_empty() {
        return Ok(Some(Violation {
            head_moved,
            index_dirty,
            tracked_changes,
            hidden,
        }));
    }
    Ok(None)
}

/// The paths a violation changed, sorted: its tracked changes and, with a dirty index, what is
/// staged (`engine._violation_paths`). None without a violation.
pub fn violation_paths(git: &Git, v: Option<&Violation>) -> Result<Vec<PyStr>> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let mut paths = v.tracked_changes.clone();
    if v.index_dirty {
        let staged = git.run(&[
            os("diff"),
            os("--cached"),
            os("--name-only"),
            os("-z"),
            os("--no-renames"),
            os("--no-ext-diff"),
            os("HEAD"),
            os("--"),
        ])?;
        paths.extend(split_nul(trim_newlines(&staged)).iter().map(|p| text(p)));
    }
    // `sorted(set(...))`: WTF-8 order is code point order.
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The top of the checkout `path` lies in; `path` itself when it is not in one
/// (`gitops.checkout_top`).
pub fn checkout_top(path: &Path) -> PathBuf {
    let Ok(git) = Git::at(path) else {
        return path.to_path_buf();
    };
    let Ok(out) = git.run(&[
        os("rev-parse"),
        os("--show-toplevel"),
        os("--absolute-git-dir"),
    ]) else {
        return path.to_path_buf();
    };
    // `top, git_dir = ....splitlines()`: two lines, or it is not a checkout.
    match py_splitlines_bytes(trim_newlines(&out))[..] {
        [top, _] => PathBuf::from(OsStr::from_bytes(top)),
        _ => path.to_path_buf(),
    }
}

// --------------------------------------------------------------------------
// Keeping and undoing it

/// Saves the workspace under `ref_name`, then puts it back on `expected_branch` at
/// `expected_head` with a clean index and tracked files; the saved commit
/// (`gitops.preserve_and_restore`).
///
/// The saved commit is `git stash create`'s: its first parent is HEAD (so moved commits stay
/// reachable), its second holds the index, its tree the tracked working tree. With nothing to
/// stash it is HEAD itself. With an unmerged index (a conflicted merge, rebase or cherry-pick)
/// it holds the tracked working tree, conflict markers included, with HEAD and, during a merge,
/// `MERGE_HEAD` as parents.
///
/// When `expected_branch` itself moved to a commit the saved one does not contain (HEAD was
/// elsewhere), that tip is kept on the sibling `<ref>-branch`. A deleted `expected_branch` is
/// recreated. The autostash of a conflicted merge or rebase is kept on `<ref>-autostash`
/// rather than landing on the shared stash. A sibling this call does not need is deleted, so
/// the ref and its siblings always describe one violation. A rebase, am, cherry-pick or merge
/// in progress is cleared; `refs/stash` is never touched. Untracked files are left alone, but
/// for those named in `untracked` (named from the checkout's top; a nested repository's
/// folder, and anything outside a workspace that is a subdirectory of its checkout, is
/// skipped): they are kept in the saved commit, a child of the saved state whose tree adds
/// them, and removed from the worktree.
pub fn preserve_and_restore(
    git: &Git,
    ref_name: &PyStr,
    expected_head: &str,
    expected_branch: &PyStr,
    untracked: &[PyStr],
) -> Result<String> {
    let refused = || {
        Error::msg(format!(
            "not a valid ref to preserve under: {}",
            ref_name.repr()
        ))
    };
    let Ok(ref_os) = ref_name.to_os() else {
        return Err(refused());
    };
    if !ref_name.starts_with("refs/")
        || !git
            .output(&[os("check-ref-format"), &ref_os])?
            .status
            .success()
    {
        return Err(refused());
    }
    check_branch_name(git, expected_branch)?;
    let mut stash: Vec<&OsStr> = NO_SIGN.iter().map(|a| os(a)).collect();
    stash.extend([
        os("stash"),
        os("create"),
        os("leadhand: preserved workspace"),
    ]);
    let out = git.output(&stash)?;
    let mut saved = if out.status.success() {
        match out.stdout.trim_ascii() {
            b"" => head(git, os("HEAD"))?,
            sha => String::from_utf8_lossy(sha).into_owned(),
        }
    } else if !split_nul(&git.run(&[os("ls-files"), os("--unmerged"), os("-z")])?).is_empty() {
        snapshot_conflicted(git)?
    } else {
        return Err(git.failure(&stash, &out));
    };
    // `untracked` is named from the top; only the workspace's are taken.
    let (top, prefix) = top_and_prefix(git)?;
    let files: Vec<PyStr> = untracked
        .iter()
        .filter(|p| {
            !p.as_wtf8().ends_with(b"/")
                && p.as_wtf8().starts_with(prefix.as_wtf8())
                && p.to_os().is_ok_and(|rel| lexists(&top.join(rel)))
        })
        .cloned()
        .collect();
    if !files.is_empty() {
        let top_git = Git::at(&top)?.with_deadline(git.deadline());
        saved = snapshot_untracked(&top_git, &saved, &files)?;
    }
    git.run(&[
        os("update-ref"),
        os("-m"),
        os("leadhand: preserve workspace"),
        &ref_os,
        os(&saved),
    ])?;
    let expected_ref = branch_ref(expected_branch)?;
    let mut moved_tip = None;
    if ref_exists(git, &expected_ref)? {
        let tip = head(git, &expected_ref)?;
        if !(is_ancestor(git, os(&tip), os(&saved))?
            || is_ancestor(git, os(&tip), os(expected_head))?)
        {
            moved_tip = Some(tip);
        }
    }
    set_sibling(
        git,
        &sibling(ref_name, "-branch")?,
        moved_tip.as_deref(),
        "leadhand: preserve moved branch",
    )?;
    // Take any autostash out of the operation first: quitting it or resetting would push it
    // onto the shared refs/stash.
    let autostash = take_autostash(git)?;
    set_sibling(
        git,
        &sibling(ref_name, "-autostash")?,
        autostash.as_deref(),
        "leadhand: preserve autostash",
    )?;
    clear_operations(git)?;
    git.run(&[
        os("checkout"),
        os("--quiet"),
        os("--force"),
        os("-B"),
        &expected_branch.to_os()?,
        os(expected_head),
        os("--"),
    ])?;
    git.run(&[os("reset"), os("--quiet"), os("--hard"), os(expected_head)])?;
    for p in &files {
        remove_untracked(&top, p, git.cwd())?;
    }
    Ok(saved)
}

/// `<ref><suffix>`, as Git takes it.
fn sibling(ref_name: &PyStr, suffix: &str) -> Result<OsString> {
    let mut name = ref_name.to_os()?;
    name.push(suffix);
    Ok(name)
}

/// Points `name` at `sha`, or deletes it when `None`, so a sibling set by an earlier call never
/// lingers beside a later violation (`gitops._set_sibling`).
fn set_sibling(git: &Git, name: &OsStr, sha: Option<&str>, message: &str) -> Result<()> {
    match sha {
        Some(sha) => {
            git.run(&[os("update-ref"), os("-m"), os(message), name, os(sha)])?;
        }
        None if ref_exists(git, name)? => {
            git.run(&[os("update-ref"), os("-d"), name])?;
        }
        None => {}
    }
    Ok(())
}

/// Detaches the autostash of a conflicted merge or rebase and returns its commit
/// (`gitops._take_autostash`). Only one is expected; any further one is left for Git to stash.
fn take_autostash(git: &Git) -> Result<Option<String>> {
    let out = git.output(&[
        os("rev-parse"),
        os("--verify"),
        os("--quiet"),
        os("MERGE_AUTOSTASH"),
    ])?;
    let sha = out.stdout.trim_ascii();
    if out.status.code() == Some(0) && !sha.is_empty() {
        git.run(&[os("update-ref"), os("-d"), os("MERGE_AUTOSTASH")])?;
        return Ok(Some(String::from_utf8_lossy(sha).into_owned()));
    }
    for state in ["rebase-merge", "rebase-apply"] {
        let marker = git_path(git, state)?.join("autostash");
        if marker.exists() {
            let named = std::fs::read_to_string(&marker).map_err(|e| Error::io(&marker, e))?;
            let sha = head(git, os(named.trim()))?;
            std::fs::remove_file(&marker).map_err(|e| Error::io(&marker, e))?;
            return Ok(Some(sha));
        }
    }
    Ok(None)
}

/// Forgets a rebase, am, cherry-pick or revert in progress (`gitops._clear_operations`). HEAD,
/// the index and files are untouched; merge and pick heads go with the hard reset after it.
fn clear_operations(git: &Git) -> Result<()> {
    if git_path(git, "rebase-apply")?.join("applying").exists() {
        git.run(&[os("am"), os("--quit")])?;
    } else if rebase_in_progress(git)? {
        git.run(&[os("rebase"), os("--quit")])?;
    }
    if git_path(git, "sequencer")?.exists() {
        git.run(&[os("cherry-pick"), os("--quit")])?;
    }
    Ok(())
}

/// A commit whose parent is `saved` and whose tree is `saved`'s plus the untracked `paths`,
/// built in a scratch index so the real one is untouched (`gitops._snapshot_untracked`).
fn snapshot_untracked(top: &Git, saved: &str, paths: &[PyStr]) -> Result<String> {
    let index = git_path(top, "index")?;
    let scratch = ScratchIndex::beside(&index)?;
    let env = [("GIT_INDEX_FILE", scratch.0.as_os_str())];
    let mut tree_of = OsString::from(saved);
    tree_of.push("^{tree}");
    top.run_env(&[os("read-tree"), &tree_of], &env)?;
    top.run_env_input(
        &[
            os("--literal-pathspecs"),
            os("add"),
            os("--force"),
            os("--pathspec-from-file=-"),
            os("--pathspec-file-nul"),
        ],
        &env,
        &pathspecs(paths)?,
    )?;
    let tree = sha_of(&top.run_env(&[os("write-tree")], &env)?);
    drop(scratch);
    let mut args: Vec<&OsStr> = NO_SIGN.iter().map(|a| os(a)).collect();
    args.extend([
        os("commit-tree"),
        os(&tree),
        os("-p"),
        os(saved),
        os("-m"),
        os("leadhand: preserved untracked files"),
    ]);
    Ok(sha_of(&top.run(&args)?))
}

/// Commits the tracked working tree of a workspace whose index has conflicts, through a copy
/// of the index so the real one is untouched (`gitops._snapshot_conflicted`).
fn snapshot_conflicted(git: &Git) -> Result<String> {
    let index = git_path(git, "index")?;
    let scratch = ScratchIndex::beside(&index)?;
    let data = std::fs::read(&index).map_err(|e| Error::io(&index, e))?;
    std::fs::write(&scratch.0, &data).map_err(|e| Error::io(&scratch.0, e))?;
    let env = [("GIT_INDEX_FILE", scratch.0.as_os_str())];
    git.run_env(&[os("add"), os("--update")], &env)?;
    let tree = sha_of(&git.run_env(&[os("write-tree")], &env)?);
    drop(scratch);
    let first = head(git, os("HEAD"))?;
    let merge = git.output(&[
        os("rev-parse"),
        os("--verify"),
        os("--quiet"),
        os("MERGE_HEAD^{commit}"),
    ])?;
    let merge_head = String::from_utf8_lossy(merge.stdout.trim_ascii()).into_owned();
    let mut args: Vec<&OsStr> = NO_SIGN.iter().map(|a| os(a)).collect();
    args.extend([os("commit-tree"), os(&tree), os("-p"), os(&first)]);
    if merge.status.success() {
        args.extend([os("-p"), os(&merge_head)]);
    }
    args.extend([os("-m"), os("leadhand: preserved conflicted workspace")]);
    Ok(sha_of(&git.run(&args)?))
}

/// Removes one untracked file (`rel`, named from the checkout's top; a symlink itself, never
/// its target), then the folders it leaves empty, up to the checkout's top but never the
/// workspace folder `ws` itself (`gitops._remove_untracked`).
fn remove_untracked(top: &Path, rel: &PyStr, ws: &Path) -> Result<()> {
    let path = top.join(rel.to_os()?);
    if path.is_dir() && !path.is_symlink() {
        return Ok(());
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(&path, e)),
    }
    let (root, keep) = (real_path(top), real_path(ws));
    let mut parent = path.parent().map(Path::to_path_buf);
    while let Some(dir) = parent {
        let resolved = real_path(&dir);
        // `root in parent.resolve().parents`: strictly below the top.
        if resolved == root || resolved == keep || !resolved.starts_with(&root) {
            break;
        }
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        parent = dir.parent().map(Path::to_path_buf);
    }
    Ok(())
}

/// Git's one-line answer (a SHA), without its newline.
fn sha_of(out: &[u8]) -> String {
    String::from_utf8_lossy(trim_newlines(out)).into_owned()
}

/// `os.path.lexists`: whether anything, a dangling symlink included, is at `path`.
pub fn lexists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A scratch index file beside the real one, made empty and exclusive as `tempfile.mkstemp`
/// makes it (`leadhand-index.<unique>`, mode 0600), and removed when dropped.
struct ScratchIndex(PathBuf);

impl ScratchIndex {
    fn beside(index: &Path) -> Result<ScratchIndex> {
        static TAKEN: AtomicU64 = AtomicU64::new(0);
        let dir = crate::store::parent(index);
        let mut last = None;
        for _ in 0..crate::store::TEMP_TRIES {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let n = TAKEN.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(
                "leadhand-index.{}{nanos:08x}{n:x}",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(_) => return Ok(ScratchIndex(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(path),
                Err(e) => return Err(Error::io(&path, e)),
            }
        }
        let path = last.unwrap_or_else(|| dir.to_path_buf());
        Err(Error::msg(format!(
            "no free scratch index name beside {}",
            path.display()
        )))
    }
}

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cards::{card_branch_name, cut_card_branch};
    use crate::git::fixture::Fixture;
    use crate::git::{prepare_worktree, repo_identity, status_entries};

    const RUN: &str = "leadhand/run-1";

    /// A repository and its run worktree on `RUN` at the base (`GitopsTests.workspace`).
    fn workspace(f: &Fixture) -> (PathBuf, PathBuf, String) {
        let repo = f.make_repo("my repo");
        let base = f.git(&repo, &["rev-parse", "HEAD"]);
        let id = repo_identity(&repo).unwrap();
        let ws = prepare_worktree(&id, RUN, &base, "run-1", &f.root().join("run dir")).unwrap();
        (repo, ws, base)
    }

    /// Cuts card `n` and commits `files` on it (`GitopsTests.card`): its branch, base and tip.
    fn card(f: &Fixture, ws: &Path, n: u32, files: &[(&str, &str)]) -> (PyStr, String, String) {
        let git = Git::at(ws).unwrap();
        let (branch, base) = cut_card_branch(&git, &RUN.into(), &n.to_string(), true).unwrap();
        let tip = f.commit(ws, files, &format!("card {n}"));
        (branch, base, tip)
    }

    fn ref_exists(f: &Fixture, cwd: &Path, name: &str) -> bool {
        let refs = f.git(cwd, &["for-each-ref", "--format=%(refname)"]);
        refs.lines().any(|r| r == name)
    }

    fn texts(items: &[&str]) -> Vec<PyStr> {
        items.iter().map(|s| PyStr::from(*s)).collect()
    }

    const SAVED: &str = "refs/leadhand/saved/1";

    // CATALOG: test_gitops.py::GitopsTests::test_preserve_refuses_a_ref_outside_refs
    #[test]
    fn preserve_refuses_a_ref_outside_refs() {
        let f = Fixture::new();
        let (_, ws, _) = workspace(&f);
        let (branch, _, h) = card(&f, &ws, 1, &[("f.txt", "orig\n")]);
        f.write(&ws, &[("f.txt", "edited\n")]);
        let git = Git::at(&ws).unwrap();
        let e = preserve_and_restore(&git, &"SAVED".into(), &h, &branch, &[]).unwrap_err();
        assert_eq!(e.message, "not a valid ref to preserve under: 'SAVED'");
        assert_eq!(
            std::fs::read_to_string(ws.join("f.txt")).unwrap(),
            "edited\n"
        );
        // A name Git refuses under refs/ is refused the same way, before anything is saved.
        let e = preserve_and_restore(&git, &"refs/a..b".into(), &h, &branch, &[]).unwrap_err();
        assert_eq!(e.message, "not a valid ref to preserve under: 'refs/a..b'");
        assert_eq!(
            std::fs::read_to_string(ws.join("f.txt")).unwrap(),
            "edited\n"
        );
    }

    // CATALOG: test_gitops.py::GitopsTests::test_preserve_drops_a_stale_branch_sibling
    #[test]
    fn preserve_drops_a_stale_branch_sibling() {
        let f = Fixture::new();
        let (_, ws, _) = workspace(&f);
        let (branch, _, h) = card(&f, &ws, 1, &[("f.txt", "orig\n")]);
        f.commit(
            &ws,
            &[("f.txt", "stray\n")],
            "reviewer commit on the card branch",
        );
        f.git(&ws, &["checkout", "--quiet", "--detach", &h]);
        let git = Git::at(&ws).unwrap();
        preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert!(ref_exists(&f, &ws, "refs/leadhand/saved/1-branch"));
        f.write(&ws, &[("f.txt", "edited\n")]);
        preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert!(!ref_exists(&f, &ws, "refs/leadhand/saved/1-branch"));
    }

    #[test]
    fn detect_violation_names_what_changed() {
        let f = Fixture::new();
        let (_, ws, base) = workspace(&f);
        let (branch, _, tip) = card(&f, &ws, 1, &[("a.txt", "a\n")]);
        let git = Git::at(&ws).unwrap();
        assert_eq!(detect_violation(&git, &tip, &branch).unwrap(), None);
        // Untracked build output and a stat-only touch are not a violation.
        f.write(&ws, &[("build/out.o", "bin\n"), ("a.txt", "a\n")]);
        assert_eq!(detect_violation(&git, &tip, &branch).unwrap(), None);
        // A tracked edit and a deletion.
        f.write(&ws, &[("a.txt", "changed\n")]);
        std::fs::remove_file(ws.join("README.md")).unwrap();
        let v = detect_violation(&git, &tip, &branch).unwrap().unwrap();
        assert_eq!(
            (v.head_moved, v.index_dirty, &v.tracked_changes[..]),
            (false, false, &texts(&["README.md", "a.txt"])[..])
        );
        assert_eq!(
            violation_paths(&git, Some(&v)).unwrap(),
            texts(&["README.md", "a.txt"])
        );
        f.git(&ws, &["checkout", "--quiet", "--", "a.txt", "README.md"]);
        // A staged change whose working copy was put back.
        f.write(&ws, &[("a.txt", "staged\n")]);
        f.git(&ws, &["add", "--", "a.txt"]);
        f.write(&ws, &[("a.txt", "a\n")]);
        let v = detect_violation(&git, &tip, &branch).unwrap().unwrap();
        assert!(v.index_dirty && !v.head_moved, "{v:?}");
        assert_eq!(violation_paths(&git, Some(&v)).unwrap(), texts(&["a.txt"]));
        f.git(&ws, &["reset", "--quiet", "--hard"]);
        // HEAD moved, or on another branch.
        assert!(
            detect_violation(&git, &base, &branch)
                .unwrap()
                .is_some_and(|v| v.head_moved)
        );
        f.git(&ws, &["checkout", "--quiet", RUN]);
        assert!(
            detect_violation(&git, &tip, &branch)
                .unwrap()
                .is_some_and(|v| v.head_moved)
        );
        assert_eq!(violation_paths(&git, None).unwrap(), Vec::<PyStr>::new());
    }

    // Moved with `unhide_index_entries` from `stage.rs`, unchanged.
    #[test]
    fn unhide_clears_assume_unchanged_and_skip_worktree() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.commit(&repo, &[("b.txt", "b\n")], "b");
        f.git(&repo, &["update-index", "--assume-unchanged", "README.md"]);
        f.git(&repo, &["update-index", "--skip-worktree", "b.txt"]);
        f.write(
            &repo,
            &[("README.md", "hidden\n"), ("b.txt", "hidden too\n")],
        );
        let git = Git::at(&repo).unwrap();
        assert!(!status_entries(&git).unwrap().0);
        let cleared = unhide_index_entries(&git).unwrap();
        assert_eq!(cleared, texts(&["README.md", "b.txt"]));
        assert_eq!(f.git(&repo, &["ls-files", "-v"]), "H README.md\nH b.txt");
        assert!(status_entries(&git).unwrap().0);
        assert_eq!(unhide_index_entries(&git).unwrap(), Vec::<PyStr>::new());
    }

    #[test]
    fn a_hidden_edit_is_a_violation_once_unhidden() {
        let f = Fixture::new();
        let (_, ws, _) = workspace(&f);
        let (branch, _, tip) = card(&f, &ws, 1, &[("b.txt", "b\n")]);
        f.git(&ws, &["update-index", "--assume-unchanged", "README.md"]);
        f.git(&ws, &["update-index", "--skip-worktree", "b.txt"]);
        f.write(&ws, &[("README.md", "hidden\n")]);
        std::fs::remove_file(ws.join("b.txt")).unwrap();
        assert_eq!(f.git(&ws, &["status", "--porcelain"]), "");
        let git = Git::at(&ws).unwrap();
        let v = detect_violation(&git, &tip, &branch).unwrap().unwrap();
        assert_eq!(v.hidden, texts(&["README.md", "b.txt"]));
        assert_eq!(v.tracked_changes, texts(&["README.md", "b.txt"]));
        assert_eq!(f.git(&ws, &["ls-files", "-v"]), "H README.md\nH b.txt");
        // Cleared: a second look finds the same edit, now plainly visible, and nothing hidden.
        assert_eq!(unhide_index_entries(&git).unwrap(), Vec::<PyStr>::new());
        f.git(&ws, &["checkout", "--quiet", "--", "."]);
        assert_eq!(detect_violation(&git, &tip, &branch).unwrap(), None);
    }

    #[test]
    fn a_sparse_checkout_a_worker_turned_on_is_turned_off_again() {
        let f = Fixture::new();
        let (repo, ws, _) = workspace(&f);
        let (branch, _, tip) = card(&f, &ws, 1, &[("src/a.txt", "a\n")]);
        f.git(&ws, &["sparse-checkout", "set", "--no-cone", "/src/"]);
        assert!(!ws.join("README.md").exists());
        let git = Git::at(&ws).unwrap();
        let v = detect_violation(&git, &tip, &branch).unwrap().unwrap();
        assert_eq!(v.hidden, texts(&["README.md"]));
        assert_eq!(v.tracked_changes, texts(&["README.md"]));
        let sparse = |cwd: &Path| {
            Git::at(cwd)
                .unwrap()
                .output(&[
                    os("config"),
                    os("--bool"),
                    os("--get"),
                    os("core.sparseCheckout"),
                ])
                .unwrap()
                .stdout
        };
        assert_ne!(sparse(&ws).trim_ascii(), b"true");
        assert_ne!(sparse(&repo).trim_ascii(), b"true");
        // The deletion stayed visible: restoring brings the file back.
        preserve_and_restore(&git, &SAVED.into(), &tip, &branch, &[]).unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("README.md")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            f.git(&repo, &["ls-tree", "-r", "--name-only", SAVED]),
            "src/a.txt"
        );
    }

    #[test]
    fn preserve_keeps_staged_and_unstaged_versions_and_restores() {
        let f = Fixture::new();
        let (_, ws, _) = workspace(&f);
        let (branch, _, h) = card(&f, &ws, 1, &[("f.txt", "orig\n")]);
        f.write(&ws, &[("f.txt", "staged\n")]);
        f.git(&ws, &["add", "--", "f.txt"]);
        f.write(&ws, &[("f.txt", "unstaged\n"), ("out.log", "build\n")]);
        let git = Git::at(&ws).unwrap();
        let saved = preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert_eq!(f.git(&ws, &["rev-parse", SAVED]), saved);
        assert_eq!(f.git(&ws, &["rev-parse", &format!("{SAVED}^1")]), h);
        assert_eq!(f.git(&ws, &["show", &format!("{SAVED}:f.txt")]), "unstaged");
        assert_eq!(f.git(&ws, &["show", &format!("{SAVED}^2:f.txt")]), "staged");
        assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "orig\n");
        // Untracked files not named are left alone; no sibling was needed; no stash entry.
        assert_eq!(
            std::fs::read_to_string(ws.join("out.log")).unwrap(),
            "build\n"
        );
        assert!(!ref_exists(&f, &ws, &format!("{SAVED}-branch")));
        assert!(!ref_exists(&f, &ws, &format!("{SAVED}-autostash")));
        assert!(!ref_exists(&f, &ws, "refs/stash"));
        assert_eq!(detect_violation(&git, &h, &branch).unwrap(), None);
        // A clean workspace saves HEAD itself.
        assert_eq!(
            preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap(),
            h
        );
    }

    #[test]
    fn preserve_keeps_a_moved_tip_and_recreates_a_deleted_branch() {
        let f = Fixture::new();
        let (_, ws, base) = workspace(&f);
        let (branch, _, h) = card(&f, &ws, 1, &[("f.txt", "orig\n")]);
        let git = Git::at(&ws).unwrap();
        // The card branch moved while HEAD went elsewhere: the tip is kept beside the saved one.
        let stray = f.commit(&ws, &[("f.txt", "stray\n")], "stray");
        f.git(&ws, &["checkout", "--quiet", RUN]);
        let saved = preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert_eq!(saved, base);
        assert_eq!(
            f.git(&ws, &["rev-parse", &format!("{SAVED}-branch")]),
            stray
        );
        assert_eq!(
            f.git(&ws, &["symbolic-ref", "--short", "HEAD"]),
            branch.lossy()
        );
        assert_eq!(f.git(&ws, &["rev-parse", "HEAD"]), h);
        // A tip already in the saved commit is not kept twice.
        let moved = f.commit(&ws, &[("f.txt", "committed\n")], "committed");
        f.write(&ws, &[("f.txt", "dirty\n")]);
        preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert_eq!(f.git(&ws, &["rev-parse", &format!("{SAVED}^1")]), moved);
        assert!(!ref_exists(&f, &ws, &format!("{SAVED}-branch")));
        // A deleted branch is recreated at the expected head.
        f.git(&ws, &["checkout", "--quiet", RUN]);
        f.git(&ws, &["branch", "--quiet", "-D", &branch.lossy()]);
        preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert_eq!(
            f.git(
                &ws,
                &["rev-parse", &format!("refs/heads/{}", branch.lossy())]
            ),
            h
        );
        assert_eq!(detect_violation(&git, &h, &branch).unwrap(), None);
    }

    /// Branch `name` off `main` in the operator's checkout, one commit per entry, then back.
    fn side_branch(f: &Fixture, repo: &Path, name: &str, files: &[&[(&str, &str)]]) -> Vec<String> {
        f.git(repo, &["checkout", "--quiet", "-b", name]);
        let shas = files
            .iter()
            .enumerate()
            .map(|(i, files)| f.commit(repo, files, &format!("{name} {i}")))
            .collect();
        f.git(repo, &["checkout", "--quiet", "main"]);
        shas
    }

    /// Runs a Git command the way a worker would, whatever its exit.
    fn worker_git(f: &Fixture, ws: &Path, args: &[&str]) {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["-c", "user.name=W", "-c", "user.email=w@example.invalid"])
            .args(args)
            .current_dir(f.confined(ws))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        for (k, v) in f.hermetic_env() {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        cmd.status().expect("git runs");
    }

    #[test]
    fn preserve_saves_and_clears_a_conflicted_merge_unsigned() {
        let f = Fixture::new();
        let (repo, ws, _) = workspace(&f);
        let side = side_branch(&f, &repo, "side", &[&[("README.md", "side\n")]]);
        let (branch, _, h) = card(&f, &ws, 1, &[("README.md", "card\n")]);
        // Every commit made with the repository's own settings tries to sign, and fails.
        f.git(&repo, &["config", "commit.gpgsign", "true"]);
        f.git(&repo, &["config", "gpg.program", "false"]);
        worker_git(&f, &ws, &["merge", "--no-edit", "side"]);
        assert_ne!(f.git(&ws, &["ls-files", "-u"]), "");
        let git = Git::at(&ws).unwrap();
        preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert_eq!(f.git(&ws, &["rev-parse", &format!("{SAVED}^1")]), h);
        assert_eq!(f.git(&ws, &["rev-parse", &format!("{SAVED}^2")]), side[0]);
        assert!(
            f.git(&ws, &["show", &format!("{SAVED}:README.md")])
                .contains("<<<<<<<")
        );
        assert!(!git_path(&git, "MERGE_HEAD").unwrap().exists());
        assert_eq!(
            std::fs::read_to_string(ws.join("README.md")).unwrap(),
            "card\n"
        );
        assert_eq!(detect_violation(&git, &h, &branch).unwrap(), None);
        // No scratch index is left beside the real one.
        let index = git_path(&git, "index").unwrap();
        let left: Vec<_> = std::fs::read_dir(index.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("leadhand-index.")
            })
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn preserve_keeps_a_rebase_autostash_and_clears_the_rebase() {
        let f = Fixture::new();
        let (repo, ws, _) = workspace(&f);
        side_branch(&f, &repo, "side", &[&[("README.md", "side\n")]]);
        let (branch, _, h) = card(
            &f,
            &ws,
            1,
            &[("README.md", "card\n"), ("c.txt", "card file\n")],
        );
        f.write(&ws, &[("c.txt", "dirty\n")]);
        worker_git(&f, &ws, &["rebase", "--autostash", "side"]);
        let git = Git::at(&ws).unwrap();
        assert!(rebase_in_progress(&git).unwrap());
        preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert_eq!(
            f.git(&ws, &["show", &format!("{SAVED}-autostash:c.txt")]),
            "dirty"
        );
        assert!(!rebase_in_progress(&git).unwrap());
        assert_eq!(f.git(&ws, &["stash", "list"]), "");
        assert_eq!(
            std::fs::read_to_string(ws.join("c.txt")).unwrap(),
            "card file\n"
        );
        assert_eq!(detect_violation(&git, &h, &branch).unwrap(), None);
    }

    #[test]
    fn preserve_keeps_named_untracked_files_and_removes_them() {
        let f = Fixture::new();
        let (_, ws, _) = workspace(&f);
        let (branch, _, h) = card(&f, &ws, 1, &[("f.txt", "orig\n")]);
        f.write(
            &ws,
            &[
                ("loose/deep/new.txt", "the worker's\n"),
                ("kept.txt", "not named\n"),
                ("spaced name.txt", "x\n"),
            ],
        );
        let git = Git::at(&ws).unwrap();
        let named = texts(&[
            "loose/deep/new.txt",
            "spaced name.txt",
            "missing.txt",
            "nested/",
        ]);
        let saved = preserve_and_restore(&git, &SAVED.into(), &h, &branch, &named).unwrap();
        assert_eq!(f.git(&ws, &["rev-parse", &format!("{saved}^1")]), h);
        assert_eq!(
            f.git(&ws, &["show", &format!("{SAVED}:loose/deep/new.txt")]),
            "the worker's"
        );
        assert_eq!(
            f.git(&ws, &["show", &format!("{SAVED}:spaced name.txt")]),
            "x"
        );
        // Removed, with the folders they leave empty; the one not named stays.
        assert!(!ws.join("loose").exists());
        assert!(!ws.join("spaced name.txt").exists());
        assert!(ws.join("kept.txt").exists());
        assert!(ws.is_dir());
        // The real index never saw them.
        assert_eq!(f.git(&ws, &["ls-files"]), "README.md\nf.txt");
    }

    #[test]
    fn checkout_top_names_the_top_or_the_path_itself() {
        let f = Fixture::new();
        let (_, ws, _) = workspace(&f);
        std::fs::create_dir_all(ws.join("src/pkg")).unwrap();
        assert_eq!(
            checkout_top(&ws.join("src/pkg")),
            ws.canonicalize().unwrap()
        );
        let outside = f.root().join("not a checkout");
        std::fs::create_dir(&outside).unwrap();
        assert_eq!(checkout_top(&outside), outside);
        assert_eq!(
            checkout_top(Path::new("relative")),
            PathBuf::from("relative")
        );
    }

    #[test]
    fn a_non_ascii_branch_name_reaches_git_as_its_bytes() {
        // (A name that is not UTF-8 cannot be a loose ref file on every file system, APFS
        // among them; its bytes take the same path through `to_os`.)
        let f = Fixture::new();
        let (_, ws, _) = workspace(&f);
        let git = Git::at(&ws).unwrap();
        let run = PyStr::from("leadhand/r\u{e9}");
        f.git(&ws, &["branch", "leadhand/r\u{e9}"]);
        let (branch, _) = cut_card_branch(&git, &run, "1", true).unwrap();
        assert_eq!(branch, card_branch_name(&run, "1"));
        let h = f.commit(&ws, &[("f.txt", "orig\n")], "card 1");
        f.write(&ws, &[("f.txt", "edited\n")]);
        assert!(detect_violation(&git, &h, &branch).unwrap().is_some());
        preserve_and_restore(&git, &SAVED.into(), &h, &branch, &[]).unwrap();
        assert_eq!(detect_violation(&git, &h, &branch).unwrap(), None);
        let card_ref = branch_ref(&branch).unwrap();
        assert_eq!(
            card_ref.as_bytes(),
            "refs/heads/leadhand/r\u{e9}--card-1".as_bytes()
        );
        assert_eq!(head(&git, &card_ref).unwrap(), h);
    }
}
