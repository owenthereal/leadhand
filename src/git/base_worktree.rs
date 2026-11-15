//! The run's evidence base worktree, `<repo>/.worktrees/<workspace name>--base` (`engine.py`:
//! `_base_worktree`, R60): detached at the run's base commit, so an evidence worker can check
//! that what the run's tests prove did not hold before the run.
//!
//! Before every evidence request it is put back exactly: stale worktree records pruned
//! (`git worktree prune`, as the Python prunes: the branch release, which never prunes, is
//! another matter), created when missing, then detached at the base, hard-reset and cleaned of
//! every untracked and ignored file. This is the one place leadhand resets and cleans, and it
//! does so only in a worktree this run added (bug class 4): `base_worktree_added` is recorded
//! before `git worktree add`; after it, the run's ID is written to the worktree's admin folder
//! (`leadhand-run`, which `git worktree remove` deletes, so a worktree that later replaces it
//! lacks the marker), the run directory beside it (`leadhand-run-dir`, the run record a person's
//! commands look up), and `base_worktree_ready` is recorded. A worktree at the path is the run's
//! only when its marker names the run; in the crash window (an add recorded without its
//! `base_worktree_ready`), one detached at the base commit with nothing in `status --ignored`
//! and no assume-unchanged or skip-worktree entry is adopted, as resetting it loses nothing.
//! Anything else is refused, and so is a path that is a symlink or lies behind one (security
//! review M3). The operator's checkout is never reset: its only writes here are the prune, the
//! add and the `.worktrees/` exclude (bug class 1).
//!
//! The engine records the two events; this module tells it when (`Record`).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::cards::{os, text, trim_newlines};
use super::preserve::{hidden_index_entries, lexists, unhide_index_entries};
use super::workspace::{ensure_excluded, resolves_in_place, worktrees, write_run_record};
use super::{Git, git_common_dir};
use crate::error::{Error, Result};
use crate::pytext::{py_splitlines_bytes, py_strip, real_path};
use crate::wire::PyStr;

/// The marker in the base worktree's admin folder: the ID of the run that added it (R60).
pub const BASE_MARKER: &str = "leadhand-run";

/// A step of the preparation the engine records, when it is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// `base_worktree_added`: before `git worktree add`.
    Added,
    /// `base_worktree_ready`: after the marker is written.
    Ready,
}

/// What the preparation is given: the run's repository, the base worktree's path, the base
/// commit, the run's ID and directory, and whether the run is in the crash window (an add
/// recorded without its `base_worktree_ready`).
#[derive(Clone, Copy, Debug)]
pub struct BaseWorktree<'a> {
    pub repo: &'a Path,
    pub path: &'a Path,
    pub base: &'a str,
    pub run_id: &'a str,
    pub run_dir: &'a Path,
    pub crash_window: bool,
}

/// Puts the base worktree back exactly, creating it when missing (`_base_worktree`): see the
/// module's comment. Idempotent. `record` is called for each event as it is due. Returns the
/// worktree's path.
pub fn prepare(b: &BaseWorktree, record: &mut dyn FnMut(Record) -> Result<()>) -> Result<PathBuf> {
    let path = b.path;
    in_place(b)?;
    let repo = Git::at(b.repo)?;
    repo.run(&[os("worktree"), os("prune")])?;
    let registered: HashSet<PathBuf> = worktrees(&repo)?
        .iter()
        .filter_map(|w| w.get(&b"worktree"[..]))
        .map(|p| real_path(Path::new(OsStr::from_bytes(p))))
        .collect();
    if lexists(path) && !registered.contains(&real_path(path)) {
        return Err(Error::msg(format!(
            "{} exists but is not a worktree of {}; move it aside",
            path.display(),
            b.repo.display()
        )));
    }
    if lexists(path) && !mine(b) {
        if !(b.crash_window && holds_nothing(b)) {
            return Err(Error::msg(format!(
                "{} is a worktree this run did not add; it is never reset for the evidence gate. \
                 Move it aside (git worktree move or remove), then run next again",
                path.display()
            )));
        }
        claim(b, record)?;
    }
    if !lexists(path) {
        let common = git_common_dir(b.repo)?;
        ensure_excluded(&common, ".worktrees/")?;
        record(Record::Added)?;
        repo.run(&[
            os("worktree"),
            os("add"),
            os("--quiet"),
            os("--detach"),
            path.as_os_str(),
            os(b.base),
        ])?;
        claim(b, record)?;
    }
    in_place(b)?;
    let git = Git::at(path)?;
    // Or a skip-worktree edit would survive the reset.
    unhide_index_entries(&git)?;
    git.run(&[
        os("checkout"),
        os("--quiet"),
        os("--force"),
        os("--detach"),
        os(b.base),
        os("--"),
    ])?;
    git.run(&[os("reset"), os("--quiet"), os("--hard"), os(b.base)])?;
    git.run(&[os("clean"), os("-q"), os("-f"), os("-d"), os("-x")])?;
    Ok(path.to_path_buf())
}

/// The worktree's HEAD and the tracked paths `git status --porcelain --untracked-files=no`
/// lists, each line from its fourth character on, sorted and without repeats
/// (`_base_violation`'s reads).
pub fn head_and_status(git: &Git) -> Result<(String, Vec<PyStr>)> {
    let now = git.run(&[os("rev-parse"), os("--verify"), os("HEAD")])?;
    let now = String::from_utf8_lossy(trim_newlines(&now)).into_owned();
    let dirty = git.run(&[os("status"), os("--porcelain"), os("--untracked-files=no")])?;
    let mut paths: Vec<PyStr> = py_splitlines_bytes(trim_newlines(&dirty))
        .into_iter()
        .filter(|ln| ln.len() > 3)
        .map(|ln| text(&ln[3..]))
        .collect();
    // `sorted({...})`: WTF-8 order is code point order.
    paths.sort();
    paths.dedup();
    Ok((now, paths))
}

/// Refuses a base worktree path that is a symlink or lies behind one below the repository: a
/// symlink there would aim the reset at another worktree (security review M3).
fn in_place(b: &BaseWorktree) -> Result<()> {
    if resolves_in_place(b.repo, b.path) {
        return Ok(());
    }
    Err(Error::msg(format!(
        "{} is a symlink or lies behind one; refusing to reset it. Move it aside",
        b.path.display()
    )))
}

/// The marker's path in the worktree's admin folder.
fn marker(path: &Path) -> Result<PathBuf> {
    let out = Git::at(path)?.run(&[os("rev-parse"), os("--absolute-git-dir")])?;
    Ok(Path::new(OsStr::from_bytes(trim_newlines(&out))).join(BASE_MARKER))
}

/// Whether the worktree's marker names this run.
fn mine(b: &BaseWorktree) -> bool {
    let Ok(marker) = marker(b.path) else {
        return false;
    };
    std::fs::read(marker)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|t| py_strip(&t) == b.run_id)
}

/// Whether resetting the worktree loses nothing: detached at the base commit, nothing in
/// `status --ignored`, and no entry hiding an edit behind assume-unchanged (the reset would
/// overwrite it) or skip-worktree (the reset would leave the base off its commit).
fn holds_nothing(b: &BaseWorktree) -> bool {
    let Ok(git) = Git::at(b.path) else {
        return false;
    };
    let head = git.run(&[os("rev-parse"), os("--verify"), os("HEAD")]);
    if !head.is_ok_and(|h| trim_newlines(&h) == b.base.as_bytes()) {
        return false;
    }
    let status = git.run(&[
        os("status"),
        os("--porcelain"),
        os("--ignored"),
        os("--untracked-files=all"),
    ]);
    if !status.is_ok_and(|s| trim_newlines(&s).is_empty()) {
        return false;
    }
    if !hidden_index_entries(&git).is_ok_and(|h| h.is_empty()) {
        return false;
    }
    // Detached; on a branch it is someone's.
    !git.ok(&[os("symbolic-ref"), os("--quiet"), os("HEAD")])
}

/// Marks the worktree as this run's (`leadhand-run`), records the run directory beside the
/// marker (`leadhand-run-dir`), then has `base_worktree_ready` recorded.
fn claim(b: &BaseWorktree, record: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
    let marker = marker(b.path)?;
    std::fs::write(&marker, format!("{}\n", b.run_id)).map_err(|e| Error::io(&marker, e))?;
    write_run_record(b.path, b.run_dir)?;
    record(Record::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use crate::runsel::RUN_RECORD;

    const RUN_ID: &str = "abc123abc123";

    fn prepared(f: &Fixture, repo: &Path, crash_window: bool) -> (Result<PathBuf>, Vec<Record>) {
        let base = f.git(repo, &["rev-parse", "HEAD"]);
        let path = repo.join(".worktrees").join("ws--base");
        let run_dir = f.root().join("run dir");
        let mut seen = Vec::new();
        let out = prepare(
            &BaseWorktree {
                repo,
                path: &path,
                base: &base,
                run_id: RUN_ID,
                run_dir: &run_dir,
                crash_window,
            },
            &mut |r| {
                seen.push(r);
                Ok(())
            },
        );
        (out, seen)
    }

    #[test]
    fn a_missing_base_worktree_is_added_marked_and_recorded() {
        let f = Fixture::new();
        let repo = f.make_repo("repo");
        let base = f.git(&repo, &["rev-parse", "HEAD"]);
        let (path, seen) = prepared(&f, &repo, false);
        let path = path.unwrap();
        assert_eq!(seen, [Record::Added, Record::Ready]);
        assert_eq!(f.git(&path, &["rev-parse", "HEAD"]), base);
        let admin = PathBuf::from(f.git(&path, &["rev-parse", "--absolute-git-dir"]));
        assert_eq!(
            std::fs::read_to_string(admin.join(BASE_MARKER)).unwrap(),
            format!("{RUN_ID}\n")
        );
        let record = std::fs::read(admin.join(RUN_RECORD)).unwrap();
        assert_eq!(
            record,
            format!("{}\n", f.root().join("run dir").display()).into_bytes()
        );
        // Prepared again, it is reset and cleaned, and nothing more is recorded.
        f.write(&path, &[("README.md", "edited\n"), ("build/out.o", "x\n")]);
        let (again, seen) = prepared(&f, &repo, false);
        assert_eq!(again.unwrap(), path);
        assert!(seen.is_empty());
        assert_eq!(f.git(&path, &["status", "--porcelain", "--ignored"]), "");
        assert_eq!(
            std::fs::read_to_string(path.join("README.md")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn a_worktree_this_run_did_not_add_is_never_reset() {
        let f = Fixture::new();
        let repo = f.make_repo("repo");
        let path = repo.join(".worktrees").join("ws--base");
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "notes",
                path.to_str().unwrap(),
            ],
        );
        f.write(&path, &[("valuable.txt", "keep\n")]);
        for crash_window in [false, true] {
            let (out, seen) = prepared(&f, &repo, crash_window);
            let e = out.unwrap_err();
            assert!(
                e.message.contains("is a worktree this run did not add"),
                "{}",
                e.message
            );
            assert!(seen.is_empty());
            assert_eq!(
                std::fs::read_to_string(path.join("valuable.txt")).unwrap(),
                "keep\n"
            );
        }
    }

    #[test]
    fn a_crash_window_worktree_that_holds_nothing_is_adopted() {
        let f = Fixture::new();
        let repo = f.make_repo("repo");
        let base = f.git(&repo, &["rev-parse", "HEAD"]);
        let path = repo.join(".worktrees").join("ws--base");
        f.git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                path.to_str().unwrap(),
                &base,
            ],
        );
        let (out, _) = prepared(&f, &repo, false);
        assert!(out.is_err(), "outside the crash window it is not the run's");
        let (out, seen) = prepared(&f, &repo, true);
        assert_eq!(out.unwrap(), path);
        assert_eq!(seen, [Record::Ready]);
    }

    #[test]
    fn a_symlinked_base_worktree_path_is_refused() {
        let f = Fixture::new();
        let repo = f.make_repo("repo");
        let other = f.root().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(repo.join(".worktrees")).unwrap();
        std::os::unix::fs::symlink(&other, repo.join(".worktrees").join("ws--base")).unwrap();
        let (out, seen) = prepared(&f, &repo, true);
        assert!(
            out.unwrap_err()
                .message
                .contains("is a symlink or lies behind one")
        );
        assert!(seen.is_empty());
    }

    #[test]
    fn status_paths_are_read_from_the_fourth_character() {
        let f = Fixture::new();
        let repo = f.make_repo("repo");
        f.commit(&repo, &[("b c.txt", "1\n"), ("a.txt", "1\n")], "two");
        f.write(&repo, &[("b c.txt", "2\n"), ("a.txt", "2\n")]);
        let git = Git::at(&repo).unwrap();
        let (now, paths) = head_and_status(&git).unwrap();
        assert_eq!(now, f.git(&repo, &["rev-parse", "HEAD"]));
        let shown: Vec<String> = paths.iter().map(|p| p.lossy().into_owned()).collect();
        assert_eq!(shown, ["\"b c.txt\"", "a.txt"]);
    }
}
