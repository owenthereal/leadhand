//! What a worker left in its workspace, and staging it for the runner's own commit (ruling
//! R39; `gitops.top_and_prefix`, `untracked_files`, `status_entries`, `hidden_index_entries`,
//! `unhide_index_entries`, `is_sparse`, `stage_work`).
//!
//! Git names paths from the checkout's top (`--full-name`, status and diff output), and a
//! workspace can be a subdirectory of it (an in-place run started with `--repo
//! <checkout>/<dir>`): such paths are used from the top, and only those under the workspace's
//! prefix belong to it. Paths are text as the Python holds them: Git's bytes decoded with
//! `surrogateescape` (`PyStr::from_os`), so they compare with, and are stored as, the Python's.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{GIT_TIMEOUT, Git, split_nul};
use crate::error::{Error, Failure, Result};
use crate::wire::PyStr;

fn os(s: &str) -> &OsStr {
    OsStr::new(s)
}

fn text(bytes: &[u8]) -> PyStr {
    PyStr::from_os(OsStr::from_bytes(bytes))
}

/// Git's output without its trailing newlines (`gitops.git`'s `rstrip("\n")`).
fn strip_newlines(out: &[u8]) -> &[u8] {
    let end = out.iter().rposition(|b| *b != b'\n').map_or(0, |i| i + 1);
    &out[..end]
}

/// `top_and_prefix`: the top of the checkout `git`'s directory lies in, and that directory's
/// place below it as Git names paths (empty at the top, else `<dir>/`). An error rather than a
/// guess when Git cannot say.
pub fn top_and_prefix(git: &Git) -> Result<(PathBuf, PyStr)> {
    let out = git.run(&[os("rev-parse"), os("--show-toplevel"), os("--show-prefix")])?;
    let out = strip_newlines(&out);
    let (top, prefix) = match out.iter().position(|b| *b == b'\n') {
        Some(at) => (&out[..at], &out[at + 1..]),
        None => (out, &b""[..]),
    };
    Ok((PathBuf::from(OsStr::from_bytes(top)), text(prefix)))
}

/// `untracked_files`: the untracked paths Git does not ignore, named from the checkout's top;
/// a nested repository is its folder with a trailing slash (Git never looks inside).
pub fn untracked_files(ws: &Path) -> std::result::Result<Vec<PyStr>, Failure> {
    let git = Git::try_at(ws)?;
    let args = [
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
        "--full-name",
        "--",
        ":/",
    ]
    .map(OsStr::new);
    let out = git.try_output(&args)?;
    if !out.status.success() {
        return Err(git.failure_text(&args, &out));
    }
    Ok(split_nul(&out.stdout).iter().map(|p| text(p)).collect())
}

/// `status_entries`: whether any tracked file changed, and the untracked paths Git does not
/// ignore.
pub fn status_entries(git: &Git) -> Result<(bool, Vec<PyStr>)> {
    let out = git.run(&[
        os("status"),
        os("--porcelain=v1"),
        os("-z"),
        os("--untracked-files=all"),
        os("--no-renames"),
    ])?;
    let (mut tracked, mut untracked) = (false, Vec::new());
    for entry in split_nul(strip_newlines(&out)) {
        match entry.strip_prefix(b"?? ") {
            Some(path) => untracked.push(text(path)),
            None => tracked = true,
        }
    }
    Ok((tracked, untracked))
}

/// `hidden_index_entries`: the tracked paths, in the whole checkout and named from its top,
/// whose index entry hides an edit from `status` and `diff`: assume-unchanged (a lowercase
/// `git ls-files -v` tag) or skip-worktree (`S`).
fn hidden_index_entries(git: &Git) -> Result<Vec<Vec<u8>>> {
    let out = git.run(&[
        os("ls-files"),
        os("-v"),
        os("-z"),
        os("--full-name"),
        os("--"),
        os(":/"),
    ])?;
    Ok(split_nul(strip_newlines(&out))
        .into_iter()
        .filter(|entry| {
            entry.first().is_some_and(u8::is_ascii_lowercase) || entry.starts_with(b"S ")
        })
        .map(|entry| entry.get(2..).unwrap_or_default().to_vec())
        .collect())
}

/// `unhide_index_entries`: clears assume-unchanged and skip-worktree wherever
/// `hidden_index_entries` finds them, so an edit behind them shows in `status` and `diff` again
/// (a worker can set them after it is dispatched). A sparse checkout a worker turned on is
/// turned off again. The paths cleared.
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

/// The deadline of each Git call of a staging bounded by `deadline` (`stage_work`'s
/// `timeout()`).
fn call_deadline(deadline: Option<Instant>) -> Result<Duration> {
    let Some(deadline) = deadline else {
        return Ok(GIT_TIMEOUT);
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Error::msg("no time left before the deadline"));
    }
    Ok(left.min(GIT_TIMEOUT))
}

/// The paths as a NUL-separated pathspec list, for `--pathspec-from-file=- --pathspec-file-nul`
/// (the argument vector stays short).
fn pathspecs(paths: &[PyStr]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for p in paths {
        out.extend_from_slice(p.to_os()?.as_bytes());
        out.push(0);
    }
    Ok(out)
}

/// `stage_work`: stages a worker's changes for leadhand's own commit: every tracked change
/// (`add -u`) and the untracked files in `new_paths` (those that appeared during the dispatch),
/// never any other untracked file, never a gitlink (a nested repository or submodule pointer:
/// one that got staged is unstaged again, R45), and never a new file outside the workspace
/// when it is a subdirectory of its checkout. `deadline` bounds every Git call but the last,
/// as in the Python. Whether anything is staged, and the gitlinks left out (sorted).
pub fn stage_work(
    ws: &Path,
    new_paths: &[PyStr],
    deadline: Option<Instant>,
) -> Result<(bool, Vec<PyStr>)> {
    let at = |git: &Git| -> Result<Git> { Ok(git.with_deadline(call_deadline(deadline)?)) };
    let ws = Git::at(ws)?;
    // `new_paths` and the gitlinks below are named from the top; only the workspace's are taken.
    let (top, prefix) = top_and_prefix(&at(&ws)?)?;
    let top = Git::at(&top)?;
    at(&top)?.run(&[os("add"), os("-u"), os("--"), os(":/")])?;
    let files: Vec<PyStr> = new_paths
        .iter()
        .filter(|p| !p.as_wtf8().ends_with(b"/") && p.as_wtf8().starts_with(prefix.as_wtf8()))
        .cloned()
        .collect();
    if !files.is_empty() {
        at(&top)?.run_input(
            &[
                os("--literal-pathspecs"),
                os("add"),
                os("--pathspec-from-file=-"),
                os("--pathspec-file-nul"),
            ],
            &pathspecs(&files)?,
        )?;
    }
    let raw = at(&top)?.run(&[
        os("diff"),
        os("--cached"),
        os("--raw"),
        os("-z"),
        os("--no-renames"),
        os("--no-ext-diff"),
        os("--ignore-submodules=none"),
        os("HEAD"),
        os("--"),
    ])?;
    let raw = split_nul(strip_newlines(&raw));
    let mut gitlinks: Vec<PyStr> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|pair| {
            let meta = String::from_utf8_lossy(&pair[0]);
            let modes: Vec<&str> = meta.trim_start_matches(':').split_whitespace().collect();
            modes.iter().take(2).any(|m| *m == "160000")
        })
        .map(|pair| text(&pair[1]))
        .collect();
    gitlinks.sort_by(|a, b| a.as_wtf8().cmp(b.as_wtf8()));
    gitlinks.dedup();
    if !gitlinks.is_empty() {
        at(&top)?.run_input(
            &[
                os("--literal-pathspecs"),
                os("reset"),
                os("--quiet"),
                os("--pathspec-from-file=-"),
                os("--pathspec-file-nul"),
                os("HEAD"),
            ],
            &pathspecs(&gitlinks)?,
        )?;
    }
    // `_test`, with the default deadline: exit 1 is "something is staged".
    let args = [
        os("diff"),
        os("--cached"),
        os("--quiet"),
        os("--no-ext-diff"),
        os("--ignore-submodules=none"),
        os("HEAD"),
        os("--"),
    ];
    let out = top.output(&args)?;
    let staged = match out.status.code() {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(top.failure(&args, &out)),
    };
    Ok((staged, gitlinks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;

    fn texts(items: &[&str]) -> Vec<PyStr> {
        items.iter().map(|s| PyStr::from(*s)).collect()
    }

    #[test]
    fn top_and_prefix_names_a_subdirectory_from_the_top() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        std::fs::create_dir_all(repo.join("src/pkg")).unwrap();
        let (top, prefix) = top_and_prefix(&Git::at(&repo).unwrap()).unwrap();
        assert_eq!((top, prefix), (repo.clone(), PyStr::from("")));
        let (top, prefix) = top_and_prefix(&Git::at(&repo.join("src/pkg")).unwrap()).unwrap();
        assert_eq!((top, prefix), (repo.clone(), PyStr::from("src/pkg/")));
        let outside = f.root().join("not a checkout");
        std::fs::create_dir(&outside).unwrap();
        assert!(top_and_prefix(&Git::at(&outside).unwrap()).is_err());
    }

    #[test]
    fn status_entries_tell_tracked_changes_from_untracked_files() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let git = Git::at(&repo).unwrap();
        assert_eq!(status_entries(&git).unwrap(), (false, vec![]));
        f.write(&repo, &[("new file.txt", "x\n"), ("dir/deep.txt", "y\n")]);
        assert_eq!(
            status_entries(&git).unwrap(),
            (false, texts(&["dir/deep.txt", "new file.txt"]))
        );
        f.write(&repo, &[("README.md", "changed\n")]);
        assert!(status_entries(&git).unwrap().0);
        // Named from the top, whatever the directory.
        let sub = Git::at(&repo.join("dir")).unwrap();
        assert!(
            status_entries(&sub)
                .unwrap()
                .1
                .contains(&PyStr::from("new file.txt"))
        );
        assert_eq!(
            untracked_files(&repo).unwrap(),
            texts(&["dir/deep.txt", "new file.txt"])
        );
    }

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
    fn stage_work_takes_tracked_changes_and_only_the_new_paths_given() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.write(
            &repo,
            &[
                ("README.md", "changed\n"),
                ("new.py", "NEW = 1\n"),
                ("operator's.txt", "not the worker's\n"),
                ("src/inner.py", "x\n"),
            ],
        );
        let (staged, gitlinks) = stage_work(&repo, &texts(&["new.py", "src/"]), None).unwrap();
        assert!(staged);
        assert_eq!(gitlinks, Vec::<PyStr>::new());
        assert_eq!(
            f.git(&repo, &["diff", "--cached", "--name-only"]),
            "README.md\nnew.py"
        );
    }

    #[test]
    fn stage_work_in_a_subdirectory_leaves_new_files_outside_it() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.write(&repo, &[("src/new.py", "x\n"), ("other/notes.txt", "y\n")]);
        let paths = texts(&["src/new.py", "other/notes.txt"]);
        let (staged, _) = stage_work(&repo.join("src"), &paths, None).unwrap();
        assert!(staged);
        assert_eq!(
            f.git(&repo, &["diff", "--cached", "--name-only"]),
            "src/new.py"
        );
    }

    #[test]
    fn stage_work_never_stages_a_gitlink() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        let nested = f.make_repo_in(&repo, "nested");
        f.commit(&nested, &[("n.txt", "n\n")], "n");
        // `git add -A` would stage it as a gitlink: the worker's own `add`.
        f.git(&repo, &["add", "nested"]);
        let (staged, gitlinks) = stage_work(&repo, &[], None).unwrap();
        assert!(!staged);
        assert_eq!(gitlinks, texts(&["nested"]));
        assert_eq!(f.git(&repo, &["diff", "--cached", "--name-only"]), "");
    }

    #[test]
    fn stage_work_with_no_time_left_runs_nothing() {
        let f = Fixture::new();
        let repo = f.make_repo("my repo");
        f.write(&repo, &[("README.md", "changed\n")]);
        let past = Instant::now() - Duration::from_secs(1);
        let e = stage_work(&repo, &[], Some(past)).unwrap_err();
        assert_eq!(e.message, "no time left before the deadline");
        assert_eq!(f.git(&repo, &["diff", "--cached", "--name-only"]), "");
    }
}
